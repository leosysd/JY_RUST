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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

// ── 策略参数 ──────────────────────────────────────────────────────────────
/// P1 纯套利门槛：full_cost(up)+full_cost(dn) < 此值时同时买两边
const ARB_THRESHOLD: f64 = 0.995;
/// P2 锁利门槛：等额锁定后 worst_pnl >= 此值才执行
const LOCK_MIN_PROFIT: f64 = 0.20;
/// P4 趋势入场价格范围
const TREND_ENTRY_MIN: f64 = 0.48;
const TREND_ENTRY_MAX: f64 = 0.70;
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
}

/// 由主状态文件路径派生影子账路径：quant_state.json → quant_state_ideal.json
fn ideal_path(p: &Path) -> PathBuf {
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("quant_state");
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("json");
    let parent = p.parent().unwrap_or_else(|| Path::new("."));
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
        let now = chrono::Utc::now().timestamp();
        let first_allowed_start = ((now / 300) + 1) * 300;
        Ok(Self {
            config, state, client, cache, model,
            signal_file, first_allowed_start, ws,
            cached_market: None, executor, ideal_state,
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

        let (up_ask, dn_ask) = {
            let cache = self.cache.read().await;
            let Some(ua) = cache.get(&up_token).and_then(|b| b.best_ask()) else {
                info!("[SMART] {} WS盘口未就绪...", market.title);
                return Ok(());
            };
            let Some(da) = cache.get(&dn_token).and_then(|b| b.best_ask()) else {
                info!("[SMART] {} WS盘口未就绪...", market.title);
                return Ok(());
            };
            (f64::from(ua.try_into().unwrap_or(0.5f32)),
             f64::from(da.try_into().unwrap_or(0.5f32)))
        };

        let pos = self.state.get_or_create(&market.slug, market.end_ts).clone();

        match pos.phase {
            Phase::Waiting  => self.decide_waiting(&market, pos, up_ask, dn_ask, seconds_left).await?,
            Phase::Holding  => self.decide_holding(&market, pos, up_ask, dn_ask, seconds_left).await?,
            Phase::Locked | Phase::Settled => {}
        }
        Ok(())
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

        // ── P2：锁利（补差额至两边相等后 worst_pnl >= LOCK_MIN_PROFIT）────
        if lock_qty > 0.0 {
            let p2_worst = pos.worst_pnl_if_add(opp_dir, opp_ask, lock_qty);
            if p2_worst >= LOCK_MIN_PROFIT {
                info!(
                    "[SMART LOCK_PROFIT {mode}] {} 买{opp_dir}@{opp_ask:.3} ×{lock_qty:.0}份  锁定worst_pnl={p2_worst:+.2}  T-{seconds_left}s",
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

            if trade_count < MAX_TREND_TRADES
                && main_ask >= last_price + TREND_STEP
                && main_ask <= TREND_ENTRY_MAX
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

        // ── 强制锁仓（最后 60s）：先补差额锁平，再可选冷门彩票 ─────────────
        if seconds_left <= ENTRY_MIN_SECONDS_LEFT {
            // 1) 补差额锁平核心仓位（只买差额，避免超买成单边赌注）
            if lock_qty > 0.0 {
                let proj = pos.worst_pnl_if_add(opp_dir, opp_ask, lock_qty);
                let label = if proj >= 0.0 { "lock_profit" } else { "lock_loss" };
                info!(
                    "[SMART FORCE {mode}] {} 锁平 {opp_dir}@{opp_ask:.3} ×{lock_qty:.0}份  worst={proj:+.2}  T-{seconds_left}s",
                    market.title
                );
                if !self.do_buy(&market, opp_dir, opp_ask, lock_qty, label, pos.price_to_beat).await? {
                    return Ok(()); // 锁平下单失败，本轮不锁定，下轮重试
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
                warn!("[SMART ORDER ERR] {} {dir} {phase_label}: {e}", market.title);
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
