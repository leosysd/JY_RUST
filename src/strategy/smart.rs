use crate::clob::{BookCache, ClobClient, Market};
use crate::config::Config;
use crate::executor::OrderExecutor;
use crate::feeds::{BinanceFeed, ChainlinkFeed};
use crate::position::{full_cost_per_share, taker_fee, MarketPosition, Phase, TradeRecord};
use crate::state::SmartStateStore;
use crate::ws::MarketWs;
use crate::zscore::ZScoreModel;
use anyhow::Result;
use tracing::{info, warn};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

// ── 策略参数 ──────────────────────────────────────────────────────────────
/// P1 纯套利门槛：full_cost(up)+full_cost(dn) < 此值时同时买两边
const ARB_THRESHOLD: f64 = 0.995;
/// P2 锁利门槛系数现由 config.lock_min_profit_factor 控制（门槛 = order_shares × factor）。
/// P4 趋势入场价格范围
const TREND_ENTRY_MIN: f64 = 0.48;
const TREND_ENTRY_MAX: f64 = 0.65;
/// P4 趋势追单步长（价格涨 0.05 才追下一笔）
const TREND_STEP: f64 = 0.05;
/// P4 最多追多少笔
const MAX_TREND_TRADES: usize = 5;
/// P4 追单允许 worst_pnl 最多恶化多少（超过就不追）
const TREND_WORST_PNL_FLOOR: f64 = -30.0;
/// P3 减险触发：单次买入须把 worst_pnl 改善至少这么多（避免每秒刷单）
const REBALANCE_MIN_IMPROVE: f64 = 1.0;
/// 冷门彩票：临近结束时便宜边 ask ≤ 此价才买（下行受限于极低价）
const LOTTERY_MAX_PRICE: f64 = 0.10;
/// P3 微批份额 = order_shares / 4
const MICRO_DIVISOR: f64 = 4.0;
/// 最后多少秒不开新首单
const ENTRY_MIN_SECONDS_LEFT: i64 = 60;

pub struct SmartStrategy {
    pub config: Config,
    pub state: SmartStateStore,
    pub client: ClobClient,
    pub cache: BookCache,
    pub model: ZScoreModel,
    pub signal_file: PathBuf,
    pub first_allowed_start: i64,
    pub ws: MarketWs,
    pub cached_market: Option<Market>,
    pub executor: Arc<OrderExecutor>,
    /// 双轨制影子账：实盘时记录"假设按 ask 全额成交"的理想账，与真实账对比。
    /// 模拟(DRY_RUN)时不使用（主账本身即理想账）。
    pub ideal_state: SmartStateStore,
    /// 逐秒盘口快照日志路径（用于离线回放对冲/锁仓策略）。
    pub book_log_file: PathBuf,
    /// 盘口日志节流：上次写入的 unix 秒（POLL_MS<1000 时避免重复写同一秒）。
    pub last_book_log_ts: i64,
}

/// 由主状态文件路径派生影子账路径：quant_state.json → quant_state_ideal.json
fn ideal_path(p: &std::path::Path) -> PathBuf {
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("quant_state");
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("json");
    let parent = p.parent().unwrap_or_else(|| std::path::Path::new("."));
    parent.join(format!("{stem}_ideal.{ext}"))
}

impl SmartStrategy {
    pub async fn new(
        config: Config,
        cache: BookCache,
        chainlink: ChainlinkFeed,
        binance: BinanceFeed,
        ws: MarketWs,
        executor: Arc<OrderExecutor>,
    ) -> Result<Self> {
        let state = SmartStateStore::load(config.state_file.clone()).await?;
        let ideal_state = SmartStateStore::load(ideal_path(&config.state_file)).await?;
        let client = ClobClient::new(
            &config.clob_api_url, &config.gamma_api_url, &config.market_slug_prefix,
        );
        let model = ZScoreModel::new(chainlink, binance);
        let signal_file = config.signal_file.clone();
        // 盘口快照日志与 signal 同目录：quant_signals.jsonl → quant_book.jsonl
        let book_log_file = signal_file
            .parent().unwrap_or_else(|| std::path::Path::new("."))
            .join("quant_book.jsonl");
        let now = chrono::Utc::now().timestamp();
        let first_allowed_start = ((now / 300) + 1) * 300;
        Ok(Self {
            config, state, client, cache, model,
            signal_file, first_allowed_start, ws,
            cached_market: None, executor, ideal_state,
            book_log_file, last_book_log_ts: 0,
        })
    }

    pub async fn run_once(&mut self) -> Result<()> {
        self.check_settlements().await?;

        let Some(market) = self.get_or_fetch_market().await else { return Ok(()); };

        if market.start_ts < self.first_allowed_start {
            info!("[SMART] 等待新盘口，最早北京时间 {}", beijing_time(self.first_allowed_start));
            return Ok(());
        }

        let seconds_left = market.seconds_left();
        if seconds_left < 5 { return Ok(()); }

        let up_idx = market.outcomes.iter().position(|o| o == "Up").unwrap_or(0);
        let dn_idx = market.outcomes.iter().position(|o| o == "Down").unwrap_or(1);
        let up_token = market.token_ids[up_idx].clone();
        let dn_token = market.token_ids[dn_idx].clone();

        let (up_ask, dn_ask, up_bid, dn_bid) = {
            let cache = self.cache.read().await;
            let Some(ua) = cache.get(&up_token).and_then(|b| b.best_ask()) else {
                info!("[SMART] {} WS盘口未就绪...", market.title);
                return Ok(());
            };
            let Some(da) = cache.get(&dn_token).and_then(|b| b.best_ask()) else {
                info!("[SMART] {} WS盘口未就绪...", market.title);
                return Ok(());
            };
            let ub = cache.get(&up_token).and_then(|b| b.best_bid())
                .map(|d| f64::from(d.try_into().unwrap_or(0.0f32))).unwrap_or(0.0);
            let db = cache.get(&dn_token).and_then(|b| b.best_bid())
                .map(|d| f64::from(d.try_into().unwrap_or(0.0f32))).unwrap_or(0.0);
            (f64::from(ua.try_into().unwrap_or(0.5f32)),
             f64::from(da.try_into().unwrap_or(0.5f32)), ub, db)
        };

        let pos = self.state.get_or_create(&market.slug, market.end_ts).clone();

        // 旧版逐秒盘口快照（默认关，已被 recorder tick 采集器取代）。
        if self.config.book_legacy_log_enabled {
            self.log_book(&market, &pos, up_ask, dn_ask, seconds_left).await;
        }

        // 路线二：maker scale-in 策略（与旧 z-score 单发并存，ENTRY_STRATEGY 切换）。
        if self.config.entry_strategy == "maker_scalein" {
            self.decide_maker(&market, pos, up_ask, dn_ask, up_bid, dn_bid, seconds_left).await?;
            return Ok(());
        }

        match pos.phase {
            Phase::Waiting  => self.decide_waiting(&market, pos, up_ask, dn_ask, seconds_left).await?,
            Phase::Holding  => self.decide_holding(&market, pos, up_ask, dn_ask, seconds_left).await?,
            Phase::Locked | Phase::Settled => {}
        }
        Ok(())
    }

    /// 写一条逐秒盘口快照到 quant_book.jsonl（节流：同一 unix 秒只写一条）。
    /// 含当前持仓与 worst_pnl，回放时无需再 join 状态文件即可重建对冲决策。
    async fn log_book(
        &mut self,
        market: &Market,
        pos: &MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) {
        let now = chrono::Utc::now().timestamp();
        if now == self.last_book_log_ts { return; }
        self.last_book_log_ts = now;
        let rec = serde_json::json!({
            "event": "book",
            "ts": now,
            "time_bj": beijing_now(),
            "slug": market.slug,
            "end_ts": market.end_ts,
            "seconds_left": seconds_left,
            "up_ask": up_ask,
            "down_ask": dn_ask,
            "up_shares": pos.up_shares,
            "down_shares": pos.down_shares,
            "up_cost_total": pos.up_cost_total,
            "down_cost_total": pos.down_cost_total,
            "worst_pnl": pos.worst_pnl(),
            "price_to_beat": pos.price_to_beat,
        });
        if let Some(p) = self.book_log_file.parent() {
            let _ = fs::create_dir_all(p).await;
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true)
            .open(&self.book_log_file).await
        {
            let line = rec.to_string() + "\n";
            let _ = f.write_all(line.as_bytes()).await;
        }
    }

    async fn get_or_fetch_market(&mut self) -> Option<Market> {
        let now = chrono::Utc::now().timestamp();
        if let Some(m) = &self.cached_market {
            if now < m.end_ts { return Some(m.clone()); }
        }
        let market = self.client.find_current_market().await?;
        let is_new = self.cached_market.as_ref().map(|m| m.slug != market.slug).unwrap_or(true);
        if is_new {
            self.ws.ensure_subscribed(&market.token_ids).await;
            info!("[SMART] 新盘口 {} 已订阅WS", market.slug);
        }
        self.cached_market = Some(market.clone());
        Some(market)
    }

    // ── Waiting：无仓位时的决策 ───────────────────────────────────────────
    //
    // P1. 纯套利：两边全成本之和 < ARB_THRESHOLD → 同时买两边
    // P4. 趋势入场：z-score 方向明确，价格在 [0.48, 0.70] → 买一手

    async fn decide_waiting(
        &mut self,
        market: &Market,
        _pos: MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        if seconds_left < ENTRY_MIN_SECONDS_LEFT { return Ok(()); }

        let price_to_beat = self.model.chainlink_at(market.start_ts)
            .or_else(|| self.model.chainlink_latest())
            .unwrap_or(0.0);
        if price_to_beat < 1000.0 {
            info!("[SMART] {} Chainlink未就绪，跳过", market.title);
            return Ok(());
        }

        let shares = self.order_shares();

        // ── P1：纯套利 ────────────────────────────────────────────────────
        let arb_cost = full_cost_per_share(up_ask) + full_cost_per_share(dn_ask);
        if arb_cost < ARB_THRESHOLD {
            let proj = shares * (1.0 - arb_cost);
            let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
            info!(
                "[SMART ARB {mode}] {} Up@{up_ask:.3}+Down@{dn_ask:.3} 全成本={arb_cost:.4} 套利+${proj:.2}  T-{seconds_left}s",
                market.title
            );
            // 买 Up 建仓，再买 Down 锁定
            self.do_buy(&market, "Up", up_ask, shares, "arb_entry", price_to_beat).await?;
            let pos = self.state.get_or_create(&market.slug, market.end_ts).clone();
            let locked_pnl = pos.worst_pnl_if_add("Down", dn_ask, shares);
            self.do_lock(&market, &pos, "Down", dn_ask, shares, locked_pnl, "arb_lock").await?;
            return Ok(());
        }

        // ── P4：趋势入场 ──────────────────────────────────────────────────
        let Some(sig) = self.model.compute(price_to_beat, seconds_left) else {
            info!("[SMART] {} 价格数据不足，跳过入场", market.title);
            return Ok(());
        };
        let Some(dir) = sig.direction() else {
            info!("[SMART] {} z={:.3} 信号不足，不入场", market.title, sig.z);
            return Ok(());
        };

        let entry_ask = if dir == "Up" { up_ask } else { dn_ask };
        if entry_ask < TREND_ENTRY_MIN || entry_ask > TREND_ENTRY_MAX {
            info!(
                "[SMART] {} {dir}@{entry_ask:.3} 不在追单区间[{TREND_ENTRY_MIN},{TREND_ENTRY_MAX}]，跳过",
                market.title
            );
            return Ok(());
        }

        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        info!(
            "[SMART ENTRY {mode}] {} {dir}@{entry_ask:.3} ×{shares:.0}份  z={:.3}  T-{seconds_left}s",
            market.title, sig.z
        );
        // 入场信号快照：记录 z-score 全套字段，用于离线分析"信号强度 vs 方向准确率"。
        // 纯记录，不影响下单。结算后可 join winner 验证。
        self.write_signal(&serde_json::json!({
            "phase": "entry_signal",
            "market": market.slug,
            "direction": dir,
            "entry_ask": entry_ask,
            "z": sig.z,
            "p_up": sig.p_up,
            "p_down": sig.p_down,
            "e": sig.e,
            "v": sig.v,
            "ct": sig.ct,
            "xt": sig.xt,
            "b": sig.b,
            "sigma120": sig.sigma120,
            "basis60": sig.basis60,
            "seconds_left": seconds_left,
            "ts": chrono::Utc::now().timestamp(),
        })).await?;
        self.do_buy(&market, dir, entry_ask, shares, "entry", price_to_beat).await?;
        Ok(())
    }

    // ── Holding：有仓位时的 5 优先级决策 ─────────────────────────────────

    async fn decide_holding(
        &mut self,
        market: &Market,
        pos: MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        let shares  = self.order_shares();
        let micro   = (shares / MICRO_DIVISOR).max(1.0);
        let cur_worst = pos.worst_pnl();
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };

        // 找出主边（份额更多的一边）和弱边
        let (main_dir, opp_dir, main_ask, opp_ask) = if pos.up_shares >= pos.down_shares {
            ("Up", "Down", up_ask, dn_ask)
        } else {
            ("Down", "Up", dn_ask, up_ask)
        };
        let main_shares = if main_dir == "Up" { pos.up_shares } else { pos.down_shares };
        let opp_shares  = if opp_dir  == "Up" { pos.up_shares } else { pos.down_shares };
        // 锁仓只需补足"差额"使两边相等；对边已有的份额不能重复买，否则会超买成单边赌注
        let lock_qty = (main_shares - opp_shares).max(0.0);

        // ── P2：锁利（补差额至两边相等后 worst_pnl >= 份额×系数）────
        // 门槛随下单份额缩放：lock_min_profit_factor=0.2 时，设5需$1、设20需$4 才锁。
        let lock_min_profit = shares * self.config.lock_min_profit_factor;
        if lock_qty > 0.0 {
            let p2_worst = pos.worst_pnl_if_add(opp_dir, opp_ask, lock_qty);
            if p2_worst >= lock_min_profit {
                info!(
                    "[SMART LOCK_PROFIT {mode}] {} 买{opp_dir}@{opp_ask:.3} ×{lock_qty:.0}份  锁定worst_pnl={p2_worst:+.2}≥{lock_min_profit:.2}  T-{seconds_left}s",
                    market.title
                );
                self.do_lock(&market, &pos, opp_dir, opp_ask, lock_qty, p2_worst, "lock_profit").await?;
                return Ok(());
            }
        }

        // 当前趋势信号（用于决定"追单"还是"减险"）
        let trend_dir = self.model.compute(pos.price_to_beat, seconds_left)
            .and_then(|s| s.direction());

        // ── P4：趋势追单（趋势仍支持主边时，优先顺势加仓）────────────────
        // 注意：必须在 P3 减险之前，否则单边持仓时减险会每 tick 抢先触发，追单永不执行。
        if seconds_left > ENTRY_MIN_SECONDS_LEFT && trend_dir == Some(main_dir) {
            let trend_trades: Vec<_> = pos.trades.iter()
                .filter(|t| t.side == main_dir && !t.phase.starts_with("lock") && !t.phase.starts_with("arb"))
                .collect();
            let trade_count = trend_trades.len();
            let last_price  = trend_trades.last().map(|t| t.price).unwrap_or(0.0);

            // 追单价格上限：超过 trend_chase_max_price 不再追（避免追高被迫天价锁）
            if trade_count < MAX_TREND_TRADES
                && main_ask >= last_price + TREND_STEP
                && main_ask <= self.config.trend_chase_max_price
            {
                let p4_worst = pos.worst_pnl_if_add(main_dir, main_ask, shares);
                if p4_worst >= TREND_WORST_PNL_FLOOR {
                    info!(
                        "[SMART TREND {mode}] {} 追{main_dir}@{main_ask:.3} ×{shares:.0}份（第{}/{}笔）worst={p4_worst:+.2}  T-{seconds_left}s",
                        market.title, trade_count + 1, MAX_TREND_TRADES
                    );
                    self.do_buy(&market, main_dir, main_ask, shares, "trend_chase", pos.price_to_beat).await?;
                    return Ok(());
                }
                info!("[SMART] {} 趋势追单会使worst={p4_worst:+.2} < 下限{TREND_WORST_PNL_FLOOR}，跳过",
                    market.title);
            }
        }

        // ── P3：减险/对冲（仅当趋势不再支持主边时；改善需达阈值，避免每秒刷单）──
        // 合并了原"便宜保险"：对边便宜本就让 worst 改善更多，统一走这里。
        if seconds_left > ENTRY_MIN_SECONDS_LEFT && trend_dir != Some(main_dir) {
            let p3_worst = pos.worst_pnl_if_add(opp_dir, opp_ask, micro);
            if p3_worst - cur_worst >= REBALANCE_MIN_IMPROVE {
                info!(
                    "[SMART HEDGE {mode}] {} 趋势转向，减险买{opp_dir}@{opp_ask:.3} ×{micro:.0}份  worst {cur_worst:+.2}→{p3_worst:+.2}  T-{seconds_left}s",
                    market.title
                );
                self.do_buy(&market, opp_dir, opp_ask, micro, "hedge", pos.price_to_beat).await?;
                return Ok(());
            }
        }

        // ── 强制处理（最后 force_lock_seconds_left 秒）─────────────────────
        if seconds_left <= self.config.force_lock_seconds_left {
            // 1) 补差额：若能锁平为盈利则照旧对锁；若会锁亏，按 force_loss_mode 处理。
            if lock_qty > 0.0 {
                let proj = pos.worst_pnl_if_add(opp_dir, opp_ask, lock_qty);
                if proj >= 0.0 {
                    // 对锁后仍保底盈利 → 照旧补对面锁平
                    info!(
                        "[SMART FORCE {mode}] {} 锁平 {opp_dir}@{opp_ask:.3} ×{lock_qty:.0}份  worst={proj:+.2}  T-{seconds_left}s",
                        market.title
                    );
                    if !self.do_buy(&market, opp_dir, opp_ask, lock_qty, "lock_profit", pos.price_to_beat).await? {
                        return Ok(());
                    }
                } else if self.config.force_loss_mode == "smooth" {
                    // 锁亏改"按趋势锁利"：不补对面认亏，而是顺当前领先方向加注，争取赢回。
                    // 领先方向 = ask 更高(更被市场看好)的一边。
                    let (lead_dir, lead_ask) = if up_ask >= dn_ask { ("Up", up_ask) } else { ("Down", dn_ask) };
                    let lead_capped = lead_ask.min(0.99);
                    let have_lead = if lead_dir == "Up" { pos.up_shares } else { pos.down_shares };
                    let cost_total = pos.up_cost_total + pos.down_cost_total;
                    // 回本所需该边总份额: X*(1-ask) >= cost - have*ask  =>  X >= (cost-have*ask)/(1-ask)
                    let need_total = if lead_capped < 0.999 {
                        (cost_total - have_lead * lead_capped) / (1.0 - lead_capped)
                    } else { have_lead };
                    let want = (need_total - have_lead).max(0.0);
                    // 预算封顶: 顺势加注最多再花 entry成本 × smooth_budget_mult
                    let budget = cost_total * self.config.smooth_budget_mult;
                    let max_by_budget = if lead_capped > 0.0 { budget / lead_capped } else { 0.0 };
                    let buy = want.min(max_by_budget);
                    if buy >= 1.0 {
                        info!(
                            "[SMART FORCE SMOOTH {mode}] {} 锁亏转顺势 买{lead_dir}@{lead_capped:.3} ×{buy:.0}份  原worst={proj:+.2}  T-{seconds_left}s",
                            market.title
                        );
                        let _ = self.do_buy(&market, lead_dir, lead_capped, buy, "smooth", pos.price_to_beat).await?;
                    } else {
                        info!("[SMART FORCE SMOOTH {mode}] {} 锁亏但加注<1份，裸持到结算  worst={proj:+.2}  T-{seconds_left}s",
                            market.title);
                    }
                } else {
                    // 旧行为：等额对锁(锁亏)
                    info!(
                        "[SMART FORCE {mode}] {} 锁亏 {opp_dir}@{opp_ask:.3} ×{lock_qty:.0}份  worst={proj:+.2}  T-{seconds_left}s",
                        market.title
                    );
                    if !self.do_buy(&market, opp_dir, opp_ask, lock_qty, "lock_loss", pos.price_to_beat).await? {
                        return Ok(());
                    }
                }
            }

            // 2) 可控冷门彩票：便宜边 ask ≤ LOTTERY_MAX_PRICE 时固定份额博一把。
            //    下行被极低价锁死（最多亏掉这笔成本），不参与对冲，纯方向小注。
            let (cheap_dir, cheap_ask) = if up_ask <= dn_ask { ("Up", up_ask) } else { ("Down", dn_ask) };
            if cheap_ask > 0.0 && cheap_ask <= LOTTERY_MAX_PRICE {
                let lot = self.order_shares();
                info!(
                    "[SMART LOTTERY {mode}] {} 冷门彩票 {cheap_dir}@{cheap_ask:.3} ×{lot:.0}份  成本≈${:.2}  T-{seconds_left}s",
                    market.title, cheap_ask * lot
                );
                let _ = self.do_buy(&market, cheap_dir, cheap_ask, lot, "lottery", pos.price_to_beat).await?;
            }

            // 3) 标记锁定
            let p = self.state.get_or_create(&market.slug, market.end_ts);
            p.phase = Phase::Locked;
            self.state.save().await?;
            return Ok(());
        }

        // 等待
        info!(
            "[SMART] {} {main_dir}{main_shares:.0}份@{:.3} opp@{opp_ask:.3}  worst_pnl={cur_worst:+.2}  T-{seconds_left}s",
            market.title,
            if main_dir == "Up" { pos.up_avg_full() } else { pos.down_avg_full() }
        );
        Ok(())
    }

    // ── 路线二：scale-in 策略（ENTRY_STRATEGY=maker_scalein）──────────────────
    //
    // JetFadil 式：5 分钟窗口内每隔 N 秒按当前领先侧顺势加仓。
    // 已硬切 taker：scalein_place 走 buy()(FAK 立即吃单)，保证成交、付 taker 费；
    // 不再挂 maker 单等收割（harvest/open_orders 路径对 taker 为空操作，保留兼容旧 state）。
    // force 线撤残留挂单裸持到结算。与旧 z 策略完全隔离，旧逻辑一行不动。
    // （配置键仍叫 maker_scalein 以兼容 CLI 开关，语义已是 taker scale-in。）
    async fn decide_maker(
        &mut self,
        market: &Market,
        pos: MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        up_bid: f64,
        dn_bid: f64,
        seconds_left: i64,
    ) -> Result<()> {
        if matches!(pos.phase, Phase::Settled) { return Ok(()); }

        let price_to_beat = self.model.chainlink_at(market.start_ts)
            .or_else(|| self.model.chainlink_latest())
            .unwrap_or(0.0);

        // 1) 收割已挂 maker 单的成交（更新 open_orders）
        self.harvest_open_orders(market).await?;

        // 2) force 边界：撤所有挂单，裸持到结算（maker 顺势策略不对锁，符合 JetFadil 模式）
        if seconds_left <= self.config.force_lock_seconds_left {
            self.cancel_all_open(market).await?;
            let p = self.state.get_or_create(&market.slug, market.end_ts);
            if !matches!(p.phase, Phase::Settled) { p.phase = Phase::Locked; }
            self.state.save().await?;
            return Ok(());
        }

        // 3) scale-in 窗口：(stop, start] 内每隔 step 秒加仓当前领先侧
        if seconds_left <= self.config.scalein_start_secs
            && seconds_left > self.config.scalein_stop_secs
        {
            let pos = self.state.get_or_create(&market.slug, market.end_ts).clone();
            let now = chrono::Utc::now().timestamp();
            let last = pos.open_orders.iter().map(|o| o.placed_ts)
                .chain(pos.trades.iter().map(|t| t.ts))
                .max().unwrap_or(0);
            if now - last >= self.config.scalein_step_sec {
                let (dir, ask, _bid) = if up_ask >= dn_ask { ("Up", up_ask, up_bid) } else { ("Down", dn_ask, dn_bid) };
                // 单边累计份额（已成交 + 残留挂单）风控
                let held = if dir == "Up" { pos.up_shares } else { pos.down_shares };
                let pending: f64 = pos.open_orders.iter()
                    .filter(|o| o.side == dir)
                    .map(|o| (o.size - o.matched_recorded).max(0.0))
                    .sum();
                if held + pending < self.config.scalein_max_shares && ask > 0.02 {
                    // 硬切 taker：直接传 ask，buy() 内部加 +0.02 缓冲穿透盘口立即吃单(FAK)。
                    self.scalein_place(market, dir, ask, self.config.scalein_qty, price_to_beat).await?;
                }
            }
        }
        Ok(())
    }

    /// 收割 open_orders 成交：用 `orders()` 列表对账查增量记账。
    ///
    /// Bug2 修复（orders 版）：`maker_fills()` 返回**仍挂在簿上**的单的 size_matched。
    /// 因为全成交的单会从 orders 列表消失，故用 `seen_live` 区分两种"消失"：
    ///   - 在列表里：确认 seen_live=true，按 size_matched 记增量；超时则撤单（已成部分已记）。
    ///   - 不在列表里 + seen_live=true：判定全成交 → 补记剩余 (size - matched_recorded)。
    ///   - 不在列表里 + seen_live=false + 已过 ttl：可能挂单失败/被拒，保守不补记直接移除。
    ///   - 不在列表里 + seen_live=false + 未过 ttl：刚挂未被索引，保留等待。
    /// 拉表失败则本 tick 全部保留、不误判。
    async fn harvest_open_orders(&mut self, market: &Market) -> Result<()> {
        let pos = self.state.get_or_create(&market.slug, market.end_ts).clone();
        if pos.open_orders.is_empty() { return Ok(()); }
        let ex = self.executor.clone();
        let now = chrono::Utc::now().timestamp();

        let live = match ex.maker_fills().await {
            Ok(f) => f,
            Err(e) => { warn!("[MAKER] 拉挂单失败，本tick保留全部挂单: {e:#}"); return Ok(()); }
        };

        let mut still_open: Vec<crate::position::OpenOrder> = vec![];
        let mut newly: Vec<(String, f64, f64, String)> = vec![]; // (dir, price, shares, phase)

        for oo in &pos.open_orders {
            let timed_out = now - oo.placed_ts >= self.config.maker_ttl_sec;
            match live.get(&oo.order_id).copied() {
                // ── 仍挂在簿上：按 size_matched 记增量 ──
                Some((matched, fp)) => {
                    let inc = matched - oo.matched_recorded;
                    if inc > 0.0 {
                        let fill_price = if fp > 0.0 { fp } else { oo.price };
                        newly.push((oo.side.clone(), fill_price, inc, oo.phase.clone()));
                    }
                    if matched >= oo.size - 0.001 {
                        // 全成 → 不保留
                    } else if timed_out {
                        let _ = ex.cancel(&oo.order_id).await; // 撤超时单，已成部分已记，剩余作罢
                    } else {
                        let mut k = oo.clone();
                        k.matched_recorded = matched;
                        k.seen_live = true;
                        still_open.push(k);
                    }
                }
                // ── 不在簿上 ──
                None => {
                    if oo.seen_live {
                        // 曾确认挂上、现消失 → 判定全成交，补记剩余份额
                        let inc = oo.size - oo.matched_recorded;
                        if inc > 0.0 {
                            newly.push((oo.side.clone(), oo.price, inc, oo.phase.clone()));
                        }
                        // 不保留
                    } else if timed_out {
                        // 从没见过且已超时：挂单大概率失败/被拒，保守不补记，移除
                    } else {
                        // 刚挂未被索引，保留等待
                        still_open.push(oo.clone());
                    }
                }
            }
        }
        // 注：maker 成交暂仍按 record_trade 的 taker 口径记 7% 费（保守，偏高估）。
        //     实测确认 maker 费率后再校准记账。
        for (dir, price, shares, phase) in &newly {
            record_trade(&mut self.state, market, dir, *price, *shares, phase, 0.0, false);
        }
        let p = self.state.get_or_create(&market.slug, market.end_ts);
        p.open_orders = still_open;
        self.state.save().await?;
        Ok(())
    }

    /// 下一笔 scale-in taker 单（FAK 立即吃单）：成交即按真实成交价/份额记账；
    /// 不成交则作罢（FAK 自动撤剩余，不挂簿、无 open_orders）。
    async fn scalein_place(
        &mut self, market: &Market, dir: &str, price: f64, qty: f64, price_to_beat: f64,
    ) -> Result<()> {
        let Some(token) = market.token_for(dir) else { return Ok(()); };
        let fill = match self.executor.buy(token, price, qty).await {
            Ok(f) => f,
            Err(e) => { warn!("[SCALEIN] 下单失败 {dir} {e:#}"); return Ok(()); }
        };
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        if fill.filled_shares > 0.0 {
            info!("[SMART SCALEIN {mode}] {} {dir}@{:.3} ×{:.0}份 taker成交",
                market.title, fill.filled_price, fill.filled_shares);
            // record_trade 内部把 Waiting→Holding 并写 price_to_beat
            record_trade(&mut self.state, market, dir, fill.filled_price, fill.filled_shares, "scalein", price_to_beat, false);
            self.state.save().await?;
        } else {
            info!("[SMART SCALEIN {mode}] {} {dir}@{price:.3} ×{qty:.0}份 未成交(FAK无对手，作罢)",
                market.title);
        }
        Ok(())
    }

    /// 撤掉某市场所有挂单（force 边界清场）。
    /// 用 orders() 语义必须**先对账再撤单**：撤单后单子从 orders 列表消失，无法再查 size_matched。
    /// 故先拉一次 live 收割已成部分(含 seen_live 消失=全成)，再撤剩余，最后 clear。
    async fn cancel_all_open(&mut self, market: &Market) -> Result<()> {
        let pos = self.state.get_or_create(&market.slug, market.end_ts).clone();
        if pos.open_orders.is_empty() { return Ok(()); }
        let ex = self.executor.clone();

        // 撤单前先对账：拿仍挂着的 size_matched，消失且 seen_live 的判全成。
        let live = ex.maker_fills().await.unwrap_or_default();
        let mut newly: Vec<(String, f64, f64, String)> = vec![];
        for oo in &pos.open_orders {
            match live.get(&oo.order_id).copied() {
                Some((matched, fp)) => {
                    let inc = matched - oo.matched_recorded;
                    if inc > 0.0 {
                        let fill_price = if fp > 0.0 { fp } else { oo.price };
                        newly.push((oo.side.clone(), fill_price, inc, oo.phase.clone()));
                    }
                }
                None if oo.seen_live => {
                    let inc = oo.size - oo.matched_recorded;
                    if inc > 0.0 {
                        newly.push((oo.side.clone(), oo.price, inc, oo.phase.clone()));
                    }
                }
                None => {}
            }
        }
        for (dir, price, shares, phase) in &newly {
            record_trade(&mut self.state, market, dir, *price, *shares, phase, 0.0, false);
        }
        // 收割完再撤剩余挂单
        for oo in &pos.open_orders {
            let _ = ex.cancel(&oo.order_id).await;
        }
        let p = self.state.get_or_create(&market.slug, market.end_ts);
        p.open_orders.clear();
        self.state.save().await?;
        Ok(())
    }

    // ── 通用：买入（不切换 Locked）────────────────────────────────────────

    /// 下单（真实或模拟）。返回成交结果；None 表示无法下单（找不到 token 或网络错误）。
    async fn place_order(
        &self,
        market: &Market,
        dir: &str,
        price: f64,
        shares: f64,
        phase_label: &str,
    ) -> Option<crate::executor::Fill> {
        let token = match market.token_for(dir) {
            Some(t) => t,
            None => {
                warn!("[SMART] {} 找不到 {dir} 的 token_id，跳过下单", market.title);
                return None;
            }
        };
        match self.executor.buy(token, price, shares).await {
            Ok(fill) => {
                if !fill.simulated {
                    info!("[SMART ORDER] {} {dir} {phase_label} id={} status={} ok={} 成交{:.1}份@{:.3}",
                        market.title, fill.order_id, fill.status, fill.success,
                        fill.filled_shares, fill.filled_price);
                }
                Some(fill)
            }
            Err(e) => {
                warn!("[SMART ORDER ERR] {} {dir} {phase_label}: {e:#}", market.title);
                None
            }
        }
    }

    async fn do_buy(
        &mut self,
        market: &Market,
        dir: &str,
        price: f64,
        shares: f64,
        phase_label: &str,
        price_to_beat: f64,
    ) -> Result<bool> {
        let fill = self.place_order(market, dir, price, shares, phase_label).await;

        // A轨 影子账（仅实盘）：假设按 ask 全额成交，与真实账对比滑点/未成交代价。
        // 模拟模式下主账本身即理想账，无需重复。
        if !self.config.dry_run {
            record_trade(&mut self.ideal_state, market, dir, price, shares, phase_label, price_to_beat, false);
            self.ideal_state.save().await?;
        }

        // B轨 真实账：只有真正成交才记账，用真实成交价/份额
        let Some(fill) = fill else { return Ok(false); };
        if !fill.success { return Ok(false); }
        let (rp, rs) = (fill.filled_price, fill.filled_shares);

        self.write_signal(&serde_json::json!({
            "phase": phase_label, "market": market.slug,
            "direction": dir, "price": rp, "shares": rs,
            "full_cost": full_cost_per_share(rp),
            "dry_run": self.config.dry_run, "ts": chrono::Utc::now().timestamp(),
        })).await?;
        record_trade(&mut self.state, market, dir, rp, rs, phase_label, price_to_beat, false);
        self.state.save().await?;
        Ok(true)
    }

    // ── 锁仓（切换 Phase::Locked）─────────────────────────────────────────

    async fn do_lock(
        &mut self,
        market: &Market,
        pos: &MarketPosition,
        dir: &str,
        price: f64,
        shares: f64,
        projected_pnl: f64,
        phase_label: &str,
    ) -> Result<()> {
        let mode   = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        let secs   = (pos.end_ts - chrono::Utc::now().timestamp()).max(0);

        let fill = self.place_order(market, dir, price, shares, phase_label).await;

        // A轨 影子账（仅实盘）：假设按 ask 全额成交并锁定
        if !self.config.dry_run {
            record_trade(&mut self.ideal_state, market, dir, price, shares, phase_label, pos.price_to_beat, true);
            self.ideal_state.save().await?;
        }

        // B轨 真实账：失败则不记账、不打印锁仓成功日志、下轮重试
        let Some(fill) = fill else { return Ok(()); };
        if !fill.success { return Ok(()); }
        let (rp, rs) = (fill.filled_price, fill.filled_shares);

        info!(
            "[SMART LOCK {mode} {}] {} {dir}@{rp:.3} ×{rs:.0}份  worst_pnl≈${projected_pnl:+.2}  T-{secs}s",
            phase_label.to_uppercase(), market.title
        );
        self.write_signal(&serde_json::json!({
            "phase": phase_label, "market": market.slug,
            "direction": dir, "price": rp, "shares": rs,
            "projected_pnl": projected_pnl, "seconds_left": secs,
            "dry_run": self.config.dry_run, "ts": chrono::Utc::now().timestamp(),
        })).await?;
        record_trade(&mut self.state, market, dir, rp, rs, phase_label, pos.price_to_beat, true);
        self.state.save().await?;
        Ok(())
    }

    // ── 结算 ──────────────────────────────────────────────────────────────

    async fn check_settlements(&mut self) -> Result<()> {
        let pending = self.state.pending_settlement();
        if pending.is_empty() { return Ok(()); }

        let mut changed = false;
        let mut ideal_changed = false;
        for (slug, pos) in pending {
            let Some(winner) = self.client.fetch_winning_outcome(&slug).await else { continue };
            let pnl = if winner == "Up" { pos.pnl_if_up_wins() } else { pos.pnl_if_down_wins() };
            let emoji = if pnl >= 0.0 { "✅" } else { "❌" };
            info!(
                "[SMART SETTLE] {} | 赢={} | Up={:.0}@{:.3} Down={:.0}@{:.3} | PNL={:+.2} {}",
                slug, winner,
                pos.up_shares, pos.up_avg_full(),
                pos.down_shares, pos.down_avg_full(),
                pnl, emoji
            );
            let p = self.state.get_or_create(&slug, pos.end_ts);
            p.phase = Phase::Settled;
            p.winner = Some(winner.clone());
            p.realized_pnl = Some(pnl);
            // 路线二：结算时清理残留挂单引用（防 maker open_orders 跨盘口泄漏到 state）
            p.open_orders.clear();

            // 同步结算影子账（实盘双轨；模拟时影子账为空，跳过）
            if let Some(ipos) = self.ideal_state.get(&slug).cloned() {
                if !matches!(ipos.phase, Phase::Settled) && !ipos.trades.is_empty() {
                    let ipnl = if winner == "Up" { ipos.pnl_if_up_wins() } else { ipos.pnl_if_down_wins() };
                    let ip = self.ideal_state.get_or_create(&slug, ipos.end_ts);
                    ip.phase = Phase::Settled;
                    ip.winner = Some(winner.clone());
                    ip.realized_pnl = Some(ipnl);
                    ideal_changed = true;
                }
            }

            self.write_signal(&serde_json::json!({
                "phase":"settlement","slug":slug,"winner":winner,"pnl":pnl,
                "ts":chrono::Utc::now().timestamp()
            })).await?;
            changed = true;
        }

        if changed {
            self.state.save().await?;
            let s = self.state.summary();
            info!("[SMART STATS] 共{}盘 锁{} 赢{} 输{}  净PNL ${:.2}",
                s.total, s.locked, s.win, s.lose, s.total_pnl);
        }
        if ideal_changed {
            self.ideal_state.save().await?;
        }
        Ok(())
    }

    fn order_shares(&self) -> f64 {
        self.config.order_shares.to_string().parse::<f64>().unwrap_or(20.0)
    }

    async fn write_signal(&self, v: &serde_json::Value) -> Result<()> {
        if let Some(p) = self.signal_file.parent() { fs::create_dir_all(p).await?; }
        let mut f = OpenOptions::new().create(true).append(true).open(&self.signal_file).await?;
        f.write_all((serde_json::to_string(v)? + "\n").as_bytes()).await?;
        Ok(())
    }
}

/// 把一笔成交记入指定状态库（主账或影子账通用）。
fn record_trade(
    store: &mut SmartStateStore,
    market: &Market,
    dir: &str,
    price: f64,
    shares: f64,
    phase_label: &str,
    price_to_beat: f64,
    is_lock: bool,
) {
    let fee    = taker_fee(price);
    let full_c = full_cost_per_share(price);
    let trade = TradeRecord {
        side: dir.to_string(), shares, price,
        fee_per_share: fee, full_cost_per_share: full_c,
        total_cost: full_c * shares,
        phase: phase_label.to_string(),
        ts: chrono::Utc::now().timestamp(),
        time_bj: beijing_now(),
    };
    let pos = store.get_or_create(&market.slug, market.end_ts);
    pos.add_trade(trade);
    if matches!(pos.phase, Phase::Waiting) {
        pos.price_to_beat = price_to_beat;
        pos.phase = Phase::Holding;
    }
    if is_lock {
        pos.phase = Phase::Locked;
    }
}

fn beijing_time(ts: i64) -> String {
    let dt = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
    dt.format("%H:%M:%S+08:00").to_string()
}

fn beijing_now() -> String { beijing_time(chrono::Utc::now().timestamp()) }
