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
/// P5 便宜保险触发价格上限
const CHEAP_THRESHOLD: f64 = 0.35;
/// P3/P5 微批份额 = order_shares / 4
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
            cached_market: None, executor,
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

        // ── P2：锁利（worst_pnl_after_lock >= LOCK_MIN_PROFIT）────────────
        // 等额买入 opp 边，确保 worst_pnl 锁定为正
        let p2_worst = pos.worst_pnl_if_add(opp_dir, opp_ask, main_shares);
        if p2_worst >= LOCK_MIN_PROFIT {
            info!(
                "[SMART LOCK_PROFIT {mode}] {} 买{opp_dir}@{opp_ask:.3} ×{main_shares:.0}份  锁定worst_pnl={p2_worst:+.2}  T-{seconds_left}s",
                market.title
            );
            self.do_lock(&market, &pos, opp_dir, opp_ask, main_shares, p2_worst, "lock_profit").await?;
            return Ok(());
        }

        // ── P3：补弱边（改善 worst_pnl，减少方向风险）────────────────────
        let weak_dir = opp_dir;  // 弱边就是 opp（主边份额更多）
        let weak_ask = opp_ask;
        let p3_worst = pos.worst_pnl_if_add(weak_dir, weak_ask, micro);
        if p3_worst > cur_worst && seconds_left > ENTRY_MIN_SECONDS_LEFT {
            info!(
                "[SMART REBAL {mode}] {} 补弱边{weak_dir}@{weak_ask:.3} ×{micro:.0}份  worst改善 {cur_worst:+.2}→{p3_worst:+.2}  T-{seconds_left}s",
                market.title
            );
            self.do_buy(&market, weak_dir, weak_ask, micro, "rebalance", pos.price_to_beat).await?;
            return Ok(());
        }

        // ── P4：趋势追单（顺势加仓，不能让 worst_pnl 低于下限）────────────
        if seconds_left > ENTRY_MIN_SECONDS_LEFT {
            if let Some(sig) = self.model.compute(pos.price_to_beat, seconds_left) {
                if sig.direction() == Some(main_dir) {
                    // 统计主边历史交易，确定上次追单价
                    let trend_trades: Vec<_> = pos.trades.iter()
                        .filter(|t| t.side == main_dir && !t.phase.starts_with("lock") && !t.phase.starts_with("arb"))
                        .collect();
                    let trade_count  = trend_trades.len();
                    let last_price   = trend_trades.last().map(|t| t.price).unwrap_or(0.0);

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
                        } else {
                            info!(
                                "[SMART] {} 趋势追单会使worst_pnl={p4_worst:+.2} < 下限{TREND_WORST_PNL_FLOOR}，跳过",
                                market.title
                            );
                        }
                    }
                }
            }
        }

        // ── P5：便宜保险（opp 价格 ≤ CHEAP_THRESHOLD 且改善 worst_pnl）────
        if opp_ask <= CHEAP_THRESHOLD && seconds_left > ENTRY_MIN_SECONDS_LEFT {
            let p5_worst = pos.worst_pnl_if_add(opp_dir, opp_ask, micro);
            if p5_worst > cur_worst {
                info!(
                    "[SMART INSURE {mode}] {} {opp_dir}@{opp_ask:.3} 便宜保险 ×{micro:.0}份  worst改善 {cur_worst:+.2}→{p5_worst:+.2}  T-{seconds_left}s",
                    market.title
                );
                self.do_buy(&market, opp_dir, opp_ask, micro, "insurance", pos.price_to_beat).await?;
                return Ok(());
            }
        }

        // ── 强制锁仓（最后 60s）──────────────────────────────────────────
        if seconds_left <= ENTRY_MIN_SECONDS_LEFT {
            let proj = pos.worst_pnl_if_add(opp_dir, opp_ask, main_shares);
            let label = if proj >= 0.0 { "lock_profit" } else { "lock_loss" };
            info!(
                "[SMART FORCE {mode}] {} 强制锁仓 {opp_dir}@{opp_ask:.3} ×{main_shares:.0}份  worst={proj:+.2}  T-{seconds_left}s",
                market.title
            );
            self.do_lock(&market, &pos, opp_dir, opp_ask, main_shares, proj, label).await?;
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

    /// 下单（真实或模拟）。返回 true 表示可以记账，false 表示下单失败应跳过本次记账。
    async fn place_order(
        &self,
        market: &Market,
        dir: &str,
        price: f64,
        shares: f64,
        phase_label: &str,
    ) -> bool {
        let Some(token) = market.token_for(dir) else {
            warn!("[SMART] {} 找不到 {dir} 的 token_id，跳过下单", market.title);
            return false;
        };
        match self.executor.buy(token, price, shares).await {
            Ok(fill) => {
                if !fill.simulated {
                    info!("[SMART ORDER] {} {dir} {phase_label} id={} status={} ok={}",
                        market.title, fill.order_id, fill.status, fill.success);
                }
                fill.success
            }
            Err(e) => {
                warn!("[SMART ORDER ERR] {} {dir} {phase_label}: {e}", market.title);
                false
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
    ) -> Result<()> {
        // 真实/模拟下单（DRY_RUN 模拟立即成交；LIVE 真实提交，失败则不记账、下轮重试）
        if !self.place_order(market, dir, price, shares, phase_label).await {
            return Ok(());
        }

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
        self.write_signal(&serde_json::json!({
            "phase": phase_label, "market": market.slug,
            "direction": dir, "price": price, "shares": shares,
            "full_cost": full_c,
            "dry_run": self.config.dry_run, "ts": trade.ts,
        })).await?;
        let pos = self.state.get_or_create(&market.slug, market.end_ts);
        pos.add_trade(trade);
        if matches!(pos.phase, Phase::Waiting) {
            pos.price_to_beat = price_to_beat;
            pos.phase = Phase::Holding;
        }
        self.state.save().await?;
        Ok(())
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

        info!(
            "[SMART LOCK {mode} {}] {} {dir}@{price:.3} ×{shares:.0}份  worst_pnl≈${projected_pnl:+.2}  T-{secs}s",
            phase_label.to_uppercase(), market.title
        );

        // 真实/模拟下单
        if !self.place_order(market, dir, price, shares, phase_label).await {
            return Ok(());
        }

        let fee    = taker_fee(price);
        let full_c = full_cost_per_share(price);
        let trade = TradeRecord {
            side: dir.to_string(), shares, price,
            fee_per_share: fee, full_cost_per_share: full_c,
            total_cost: full_c * shares, phase: phase_label.to_string(),
            ts: chrono::Utc::now().timestamp(), time_bj: beijing_now(),
        };
        self.write_signal(&serde_json::json!({
            "phase": phase_label, "market": market.slug,
            "direction": dir, "price": price, "shares": shares,
            "projected_pnl": projected_pnl, "seconds_left": secs,
            "dry_run": self.config.dry_run, "ts": trade.ts,
        })).await?;
        let pos = self.state.get_or_create(&market.slug, market.end_ts);
        pos.add_trade(trade);
        pos.phase = Phase::Locked;
        self.state.save().await?;
        Ok(())
    }

    // ── 结算 ──────────────────────────────────────────────────────────────

    async fn check_settlements(&mut self) -> Result<()> {
        let pending = self.state.pending_settlement();
        if pending.is_empty() { return Ok(()); }

        let mut changed = false;
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

fn beijing_time(ts: i64) -> String {
    let dt = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
    dt.format("%H:%M:%S+08:00").to_string()
}

fn beijing_now() -> String { beijing_time(chrono::Utc::now().timestamp()) }
