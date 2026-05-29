use crate::clob::{BookCache, ClobClient, Market};
use crate::config::Config;
use crate::feeds::{BinanceFeed, ChainlinkFeed};
use crate::position::{full_cost_per_share, taker_fee, MarketPosition, Phase, TradeRecord};
use crate::state::SmartStateStore;
use crate::ws::MarketWs;
use crate::zscore::ZScoreModel;
use anyhow::Result;
use tracing::info;
use std::path::PathBuf;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

const TARGET_PROFIT_PER_SHARE: f64 = 0.02;   // 锁利润门槛 2¢/份
const ENTRY_MAX_ASK: f64 = 0.53;              // 标准入场最高价（接近50/50）
const EXTREME_ENTRY_ASK: f64 = 0.22;         // 极端价格入场（均值回归）
const ENTRY_MIN_SECONDS_LEFT: i64 = 60;       // 最后60秒不做首单
const MAX_SHARES_PER_SIDE: f64 = 40.0;        // 单边最大累积份额
const MICRO_LOT_DIVISOR: f64 = 4.0;           // 微批 = order_shares / 4

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
}

impl SmartStrategy {
    pub async fn new(
        config: Config,
        cache: BookCache,
        chainlink: ChainlinkFeed,
        binance: BinanceFeed,
        ws: MarketWs,
    ) -> Result<Self> {
        let state = SmartStateStore::load(config.state_file.clone()).await?;
        let client = ClobClient::new(&config.clob_api_url, &config.gamma_api_url, &config.market_slug_prefix);
        let model = ZScoreModel::new(chainlink, binance);
        let signal_file = config.signal_file.clone();
        let now = chrono::Utc::now().timestamp();
        let first_allowed_start = ((now / 300) + 1) * 300;

        Ok(Self { config, state, client, cache, model, signal_file, first_allowed_start, ws, cached_market: None })
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
                info!("[SMART] {} WS盘口未就绪，等待推送...", market.title);
                return Ok(());
            };
            let Some(da) = cache.get(&dn_token).and_then(|b| b.best_ask()) else {
                info!("[SMART] {} WS盘口未就绪，等待推送...", market.title);
                return Ok(());
            };
            (f64::from(ua.try_into().unwrap_or(0.5f32)), f64::from(da.try_into().unwrap_or(0.5f32)))
        };

        let pos = self.state.get_or_create(&market.slug, market.end_ts).clone();

        match pos.phase {
            Phase::Waiting => {
                self.try_entry(&market, pos, up_ask, dn_ask, seconds_left).await?;
            }
            Phase::Holding => {
                self.try_manage(&market, pos, up_ask, dn_ask, seconds_left).await?;
            }
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

    // ── 阶段1：入场决策 ────────────────────────────────────────────────────

    async fn try_entry(
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

        let sig = self.model.compute(price_to_beat, seconds_left);
        let order_shares = self.config.order_shares.to_string().parse::<f64>().unwrap_or(20.0);
        let micro_lot = (order_shares / MICRO_LOT_DIVISOR).max(1.0);

        // ── 路径A：极端价格入场（均值回归）─────────────────────────────────
        // 当一边价格 ≤ 0.22，该边被市场严重低估，逆势买入便宜份额
        let extreme = if up_ask <= EXTREME_ENTRY_ASK && up_ask <= dn_ask {
            Some(("Up", up_ask))
        } else if dn_ask <= EXTREME_ENTRY_ASK {
            Some(("Down", dn_ask))
        } else {
            None
        };

        if let Some((dir, cheap_ask)) = extreme {
            // z-score 过滤：不要对抗确认趋势（|z| > 0.35 且方向相反时跳过）
            let trend_ok = sig.as_ref().map(|s| match dir {
                "Up"   => s.z > -0.35,
                _      => s.z <  0.35,
            }).unwrap_or(true);

            if trend_ok {
                let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
                let z_str = sig.as_ref().map(|s| format!("{:.3}", s.z)).unwrap_or("-".into());
                info!(
                    "[SMART EXTREME {mode}] {} {dir}@{cheap_ask:.3} ×{micro_lot:.0}份  z={z_str}  T-{seconds_left}s  (均值回归入场)",
                    market.title
                );
                self.do_buy(&market, dir, cheap_ask, micro_lot, "entry").await?;
                return Ok(());
            }
            info!("[SMART] {} 极端价格但趋势过强（z={:.3}），跳过极端入场",
                market.title,
                sig.as_ref().map(|s| s.z).unwrap_or(0.0));
        }

        // ── 路径B：标准 z-score 入场（接近50/50，方向明确）──────────────────
        if up_ask > ENTRY_MAX_ASK && dn_ask > ENTRY_MAX_ASK { return Ok(()); }

        let Some(sig) = sig else {
            info!("[SMART] {} 价格数据不足，跳过入场", market.title);
            return Ok(());
        };
        let Some(dir) = sig.direction() else {
            info!("[SMART] {} z={:.3} 信号不足，不入场", market.title, sig.z);
            return Ok(());
        };

        let (entry_ask, _) = if dir == "Up" { (up_ask, dn_ask) } else { (dn_ask, up_ask) };
        if entry_ask > ENTRY_MAX_ASK {
            info!("[SMART] {} {}@{:.3} 偏贵，等待", market.title, dir, entry_ask);
            return Ok(());
        }

        let required_p = full_cost_per_share(entry_ask) + 0.01;
        let entry_p = if dir == "Up" { sig.p_up } else { sig.p_down };
        if entry_p < required_p.max(0.55) {
            info!("[SMART] {} {}@{:.3} p={:.3}<{:.3}，跳过", market.title, dir, entry_ask, entry_p, required_p);
            return Ok(());
        }

        // 标准入场用 order_shares/2（小化单次风险）
        let std_lot = (order_shares / 2.0).max(1.0);
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        info!(
            "[SMART ENTRY {mode}] {} {dir}@{entry_ask:.3} ×{std_lot:.0}份  z={:.3} p={entry_p:.3}  T-{seconds_left}s",
            market.title, sig.z
        );
        self.do_buy(&market, dir, entry_ask, std_lot, "entry").await?;
        Ok(())
    }

    // ── 阶段2：持仓管理（锁利 / 累积 / 锁亏）──────────────────────────────

    async fn try_manage(
        &mut self,
        market: &Market,
        pos: MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        let has_up   = pos.up_shares > 0.0;
        let has_down = pos.down_shares > 0.0;

        // 双边都有 → 检查是否可以锁利/锁亏后结束
        if has_up && has_down {
            // 计算当前双边 PnL
            let proj_up_wins   = pos.pnl_if_up_wins();
            let proj_down_wins = pos.pnl_if_down_wins();
            let worst = proj_up_wins.min(proj_down_wins);

            // 已经锁定（两边都有且都不能追加了）
            info!(
                "[SMART] {} 双边持仓  Up={:.0}@{:.3} Down={:.0}@{:.3}  Up赢={:+.2} Down赢={:+.2}  T-{seconds_left}s",
                market.title,
                pos.up_shares, pos.up_avg_full(),
                pos.down_shares, pos.down_avg_full(),
                proj_up_wins, proj_down_wins
            );

            // 如果两边都有 且 已达到锁定条件（最差情形也正赢或接受），转 Locked
            if seconds_left <= 60 || worst >= -0.5 {
                let p = self.state.get_or_create(&market.slug, market.end_ts);
                p.phase = Phase::Locked;
                self.state.save().await?;
            }
            return Ok(());
        }

        let (main_dir, opp_dir, main_ask, opp_ask) = if has_up {
            ("Up", "Down", up_ask, dn_ask)
        } else {
            ("Down", "Up", dn_ask, up_ask)
        };

        let order_shares = self.config.order_shares.to_string().parse::<f64>().unwrap_or(20.0);
        let micro_lot = (order_shares / MICRO_LOT_DIVISOR).max(1.0);
        let main_shares = if has_up { pos.up_shares } else { pos.down_shares };

        // ── 1. 优先：锁利润 ────────────────────────────────────────────────
        if pos.can_lock_profit(opp_ask, TARGET_PROFIT_PER_SHARE) {
            let proj = pos.projected_locked_pnl(opp_ask);
            self.do_lock(&market, &pos, opp_dir, opp_ask, main_shares, proj, "lock_profit").await?;
            return Ok(());
        }

        // ── 2. 主边继续便宜 → 微量累积 ────────────────────────────────────
        if main_ask <= EXTREME_ENTRY_ASK && main_shares < MAX_SHARES_PER_SIDE && seconds_left > 90 {
            let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
            info!(
                "[SMART ACCUM {mode}] {} 继续累积 {main_dir}@{main_ask:.3} ×{micro_lot:.0}份  已有{main_shares:.0}份  T-{seconds_left}s",
                market.title
            );
            self.do_buy(&market, main_dir, main_ask, micro_lot, "accumulate").await?;
            return Ok(());
        }

        // ── 3. 对边也变便宜 → 便宜对冲，降低方向风险 ──────────────────────
        if opp_ask <= EXTREME_ENTRY_ASK && seconds_left > 90 {
            let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
            let proj = pos.projected_locked_pnl(opp_ask);
            info!(
                "[SMART HEDGE {mode}] {} 对边也便宜！买 {opp_dir}@{opp_ask:.3} ×{micro_lot:.0}份  预计最差PnL≈{proj:+.2}  T-{seconds_left}s",
                market.title
            );
            self.do_buy(&market, opp_dir, opp_ask, micro_lot, "cheap_hedge").await?;
            return Ok(());
        }

        // ── 4. 标准追仓（z-score 强信号 + 仍可锁利）──────────────────────
        if seconds_left > 30 {
            if let Some(sig) = self.model.compute(pos.price_to_beat, seconds_left) {
                if sig.chase_direction() == Some(main_dir) {
                    let chase_lot = micro_lot;
                    if pos.can_chase_then_lock(main_dir, chase_lot, main_ask, opp_ask, TARGET_PROFIT_PER_SHARE)
                       && main_shares < MAX_SHARES_PER_SIDE {
                        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
                        info!(
                            "[SMART CHASE {mode}] {} {main_dir}@{main_ask:.3} ×{chase_lot:.0}份  z={:.3}  T-{seconds_left}s",
                            market.title, sig.z
                        );
                        self.do_buy(&market, main_dir, main_ask, chase_lot, "chase").await?;
                        return Ok(());
                    }
                }
            }
        }

        // ── 5. 60s 到期强制锁仓 ────────────────────────────────────────────
        if seconds_left <= 60 {
            let proj = pos.projected_locked_pnl(opp_ask);
            self.do_lock(&market, &pos, opp_dir, opp_ask, main_shares, proj, "lock_loss").await?;
            return Ok(());
        }

        // 继续等待
        let a = pos.up_avg_full().max(pos.down_avg_full());
        info!(
            "[SMART] {} {main_dir}仓 {main_shares:.0}份@{a:.3}  opp@{opp_ask:.3}  a+d={:.3}  T-{seconds_left}s",
            market.title, a + full_cost_per_share(opp_ask)
        );
        Ok(())
    }

    // ── 通用：买入（不切换phase） ──────────────────────────────────────────

    async fn do_buy(
        &mut self,
        market: &Market,
        dir: &str,
        price: f64,
        shares: f64,
        phase_label: &str,
    ) -> Result<()> {
        let fee   = taker_fee(price);
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
            "dry_run": self.config.dry_run, "ts": trade.ts,
        })).await?;

        let pos = self.state.get_or_create(&market.slug, market.end_ts);
        pos.add_trade(trade);
        if matches!(pos.phase, Phase::Waiting) {
            pos.price_to_beat = self.model.chainlink_at(market.start_ts)
                .or_else(|| self.model.chainlink_latest())
                .unwrap_or(0.0);
            pos.phase = Phase::Holding;
        }
        self.state.save().await?;
        Ok(())
    }

    // ── 锁仓（切换 Phase::Locked） ─────────────────────────────────────────

    async fn do_lock(
        &mut self,
        market: &Market,
        pos: &MarketPosition,
        opp_dir: &str,
        opp_ask: f64,
        shares: f64,
        projected_pnl: f64,
        phase_label: &str,
    ) -> Result<()> {
        let fee   = taker_fee(opp_ask);
        let full_c = full_cost_per_share(opp_ask);
        let mode  = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        let seconds_left = (pos.end_ts - chrono::Utc::now().timestamp()).max(0);

        info!(
            "[SMART LOCK {mode} {}] {} {opp_dir}@{opp_ask:.3} ×{shares:.0}份  预计PNL≈${projected_pnl:+.2}  T-{seconds_left}s",
            phase_label.to_uppercase(), market.title
        );

        let trade = TradeRecord {
            side: opp_dir.to_string(), shares, price: opp_ask,
            fee_per_share: fee, full_cost_per_share: full_c,
            total_cost: full_c * shares, phase: phase_label.to_string(),
            ts: chrono::Utc::now().timestamp(), time_bj: beijing_now(),
        };
        self.write_signal(&serde_json::json!({
            "phase": phase_label, "market": market.slug, "direction": opp_dir,
            "price": opp_ask, "shares": shares, "projected_pnl": projected_pnl,
            "seconds_left": seconds_left, "dry_run": self.config.dry_run, "ts": trade.ts,
        })).await?;

        let pos = self.state.get_or_create(&market.slug, market.end_ts);
        pos.add_trade(trade);
        pos.phase = Phase::Locked;
        self.state.save().await?;
        Ok(())
    }

    // ── 结算 ───────────────────────────────────────────────────────────────

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
