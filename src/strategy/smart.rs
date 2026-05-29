use crate::clob::{BookCache, ClobClient, Market};
use crate::config::Config;
use crate::feeds::{BinanceFeed, ChainlinkFeed};
use crate::position::{full_cost_per_share, taker_fee, MarketPosition, Phase, TradeRecord};
use crate::state::SmartStateStore;
use crate::zscore::ZScoreModel;
use anyhow::Result;
use tracing::{info, warn};
use std::path::PathBuf;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

const TARGET_PROFIT_PER_SHARE: f64 = 0.02;   // 锁利润门槛 2¢/份
const MAX_LOSS_PER_MARKET: f64 = 15.0;        // 锁亏损：最大亏损 $15
const ENTRY_MAX_ASK: f64 = 0.53;              // 入场最高价（不入贵的）
const ENTRY_MIN_SECONDS_LEFT: i64 = 60;       // 最后60秒不做新首单

pub struct SmartStrategy {
    pub config: Config,
    pub state: SmartStateStore,
    pub client: ClobClient,
    pub cache: BookCache,
    pub model: ZScoreModel,
    pub signal_file: PathBuf,
    pub first_allowed_start: i64,
}

impl SmartStrategy {
    pub async fn new(
        config: Config,
        cache: BookCache,
        chainlink: ChainlinkFeed,
        binance: BinanceFeed,
    ) -> Result<Self> {
        let state = SmartStateStore::load(config.state_file.clone()).await?;
        let client = ClobClient::new(&config.clob_api_url, &config.gamma_api_url, &config.market_slug_prefix);
        let model = ZScoreModel::new(chainlink, binance);
        let signal_file = config.signal_file.clone();
        let now = chrono::Utc::now().timestamp();
        let first_allowed_start = ((now / 300) + 1) * 300;

        Ok(Self { config, state, client, cache, model, signal_file, first_allowed_start })
    }

    pub async fn run_once(&mut self) -> Result<()> {
        // 先检查已结束盘口的结算
        self.check_settlements().await?;

        let Some(market) = self.client.find_current_market().await else {
            return Ok(());
        };

        if market.start_ts < self.first_allowed_start {
            info!("[SMART] 等待新盘口，最早北京时间 {}",
                beijing_time(self.first_allowed_start));
            return Ok(());
        }

        let seconds_left = market.seconds_left();
        if seconds_left < 5 { return Ok(()); }

        // 获取盘口价格
        let up_idx = market.outcomes.iter().position(|o| o == "Up").unwrap_or(0);
        let dn_idx = market.outcomes.iter().position(|o| o == "Down").unwrap_or(1);
        let up_token = &market.token_ids[up_idx];
        let dn_token = &market.token_ids[dn_idx];

        let up_book = match self.client.fetch_book(up_token).await {
            Ok(b) => b, Err(e) => { warn!("[SMART] book Up: {e}"); return Ok(()); }
        };
        let dn_book = match self.client.fetch_book(dn_token).await {
            Ok(b) => b, Err(e) => { warn!("[SMART] book Down: {e}"); return Ok(()); }
        };

        let Some(up_ask) = up_book.best_ask() else { return Ok(()); };
        let Some(dn_ask) = dn_book.best_ask() else { return Ok(()); };
        let up_ask = f64::from(up_ask.try_into().unwrap_or(0.5f32));
        let dn_ask = f64::from(dn_ask.try_into().unwrap_or(0.5f32));

        // 获取或初始化仓位（clone 避免借用冲突）
        let pos = self.state.get_or_create(&market.slug, market.end_ts).clone();

        match pos.phase {
            Phase::Waiting => {
                self.try_entry(&market, pos, up_ask, dn_ask, seconds_left).await?;
            }
            Phase::Holding => {
                self.try_chase_or_lock(&market, pos, up_ask, dn_ask, seconds_left).await?;
            }
            Phase::Locked | Phase::Settled => {}
        }

        Ok(())
    }

    // ── 阶段1：首单入场 ────────────────────────────────────────────────────

    async fn try_entry(
        &mut self,
        market: &Market,
        pos: MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        if seconds_left < ENTRY_MIN_SECONDS_LEFT {
            return Ok(());
        }

        // 只在价格接近 0.50（ask ≤ 0.53）时入场
        if up_ask > ENTRY_MAX_ASK && dn_ask > ENTRY_MAX_ASK {
            return Ok(());
        }

        // Price to Beat = 开盘时 Chainlink BTC/USD 价格
        // 优先用开盘时刻的缓存价；若没有则用当前最新价（开盘即为当前价）
        let price_to_beat = self.model.chainlink_at(market.start_ts)
            .or_else(|| self.model.chainlink_latest())
            .unwrap_or(0.0);

        if price_to_beat < 1000.0 {
            info!("[SMART] {} Chainlink 数据未就绪（B={:.0}），跳过入场", market.title, price_to_beat);
            return Ok(());
        }

        // 计算 z-score（需要至少 10 秒的 Binance 数据）
        let Some(sig) = self.model.compute(price_to_beat, seconds_left) else {
            info!("[SMART] {} 价格数据不足（需等待约10s），跳过入场", market.title);
            return Ok(());
        };

        let Some(dir) = sig.direction() else {
            info!("[SMART] {} z={:.3} 信号不足（|z|<0.15），不入场", market.title, sig.z);
            return Ok(());
        };

        // 检查对应方向的价格
        let (entry_ask, opp_ask) = if dir == "Up" { (up_ask, dn_ask) } else { (dn_ask, up_ask) };
        if entry_ask > ENTRY_MAX_ASK {
            info!("[SMART] {} {}@{:.3} > {:.3}，价格偏贵，等待", market.title, dir, entry_ask, ENTRY_MAX_ASK);
            return Ok(());
        }

        // 检查手续费后胜率是否足够（>55%）
        let required_p = full_cost_per_share(entry_ask) + 0.01; // 至少1%缓冲
        let entry_p = if dir == "Up" { sig.p_up } else { sig.p_down };
        if entry_p < required_p.max(0.55) {
            info!("[SMART] {} {}@{:.3} p={:.3} < 需要{:.3}，跳过", market.title, dir, entry_ask, entry_p, required_p);
            return Ok(());
        }

        let shares = self.config.order_shares.to_string().parse::<f64>().unwrap_or(20.0);
        let fee = taker_fee(entry_ask);
        let full_c = full_cost_per_share(entry_ask);

        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        info!(
            "[SMART ENTRY {mode}] {} {dir}@{entry_ask:.3} ×{shares:.0}份  z={:.3} p={entry_p:.3}  T-{seconds_left}s",
            market.title, sig.z
        );

        let trade = TradeRecord {
            side: dir.to_string(),
            shares,
            price: entry_ask,
            fee_per_share: fee,
            full_cost_per_share: full_c,
            total_cost: full_c * shares,
            phase: "entry".to_string(),
            ts: chrono::Utc::now().timestamp(),
            time_bj: beijing_now(),
        };

        self.write_signal(&serde_json::json!({
            "phase": "entry", "market": market.slug,
            "direction": dir, "price": entry_ask, "shares": shares,
            "z": sig.z, "p_up": sig.p_up, "fee": fee,
            "seconds_left": seconds_left, "dry_run": self.config.dry_run,
            "ts": trade.ts,
        })).await?;

        let pos = self.state.get_or_create(&market.slug, market.end_ts);
        pos.price_to_beat = price_to_beat;
        pos.add_trade(trade);
        pos.phase = Phase::Holding;
        self.state.save().await?;
        Ok(())
    }

    // ── 阶段2：追仓或锁仓 ─────────────────────────────────────────────────

    async fn try_chase_or_lock(
        &mut self,
        market: &Market,
        pos: MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        let has_up   = pos.up_shares > 0.0;
        let has_down = pos.down_shares > 0.0;

        // 决定入场方向和对立方向
        let (main_dir, opp_dir, main_ask, opp_ask) = if has_up && !has_down {
            ("Up", "Down", up_ask, dn_ask)
        } else if has_down && !has_up {
            ("Down", "Up", dn_ask, up_ask)
        } else {
            // 双边都有，已接近锁仓状态
            return Ok(());
        };

        // ── 优先检查锁利润 ────────────────────────────────────────────────
        if pos.can_lock_profit(opp_ask, TARGET_PROFIT_PER_SHARE) {
            let proj = pos.projected_locked_pnl(opp_ask);
            let shares = if main_dir == "Up" { pos.up_shares } else { pos.down_shares };
            self.do_lock(&market, &pos, opp_dir, opp_ask, shares, proj, "lock_profit").await?;
            return Ok(());
        }

        // ── 若有明显信号，考虑追仓 ────────────────────────────────────────
        if seconds_left > 30 {
            let price_to_beat = pos.price_to_beat;
            if let Some(sig) = self.model.compute(price_to_beat, seconds_left) {
                let chase_dir = sig.chase_direction();
                if chase_dir == Some(main_dir) {
                    let chase_shares = if main_dir == "Up" { pos.up_shares } else { pos.down_shares };
                    // 检查追仓后还能锁利润
                    if pos.can_chase_then_lock(main_dir, chase_shares, main_ask, opp_ask, TARGET_PROFIT_PER_SHARE) {
                        self.do_chase(&market, &pos, main_dir, main_ask, chase_shares, &sig).await?;
                        return Ok(());
                    }
                }
            }
        }

        // ── 若剩余时间 ≤ 30s 且方向不对，考虑锁亏损 ──────────────────────
        if seconds_left <= 60 {
            let loss_shares = pos.loss_lock_shares(opp_ask, MAX_LOSS_PER_MARKET);
            if loss_shares >= 1.0 {
                let proj = pos.pnl_if_up_wins().min(pos.pnl_if_down_wins());
                self.do_lock(&market, &pos, opp_dir, opp_ask, loss_shares, proj, "lock_loss").await?;
                return Ok(());
            }
        }

        // 继续等待
        let a = pos.up_avg_full().max(pos.down_avg_full());
        let d_full = full_cost_per_share(opp_ask);
        info!(
            "[SMART] {} {}仓 a*={:.3} opp@{:.3}(d*={:.3}) a+d={:.3} 等待锁仓  T-{seconds_left}s",
            market.title, main_dir, a, opp_ask, d_full, a + d_full
        );

        Ok(())
    }

    async fn do_chase(
        &mut self,
        market: &Market,
        pos: &MarketPosition,
        dir: &str,
        price: f64,
        shares: f64,
        sig: &crate::zscore::ZSignal,
    ) -> Result<()> {
        let fee = taker_fee(price);
        let full_c = full_cost_per_share(price);
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };

        info!(
            "[SMART CHASE {mode}] {} {dir}@{price:.3} ×{shares:.0}份  z={:.3}  新均摊成本≈{:.3}",
            market.title, sig.z,
            if dir == "Up" {
                (pos.up_cost_total + shares*full_c)/(pos.up_shares+shares)
            } else {
                (pos.down_cost_total + shares*full_c)/(pos.down_shares+shares)
            }
        );

        let trade = TradeRecord {
            side: dir.to_string(), shares, price,
            fee_per_share: fee, full_cost_per_share: full_c,
            total_cost: full_c * shares, phase: "chase".to_string(),
            ts: chrono::Utc::now().timestamp(), time_bj: beijing_now(),
        };
        self.write_signal(&serde_json::json!({
            "phase":"chase","market":market.slug,"direction":dir,"price":price,"shares":shares,
            "z":sig.z,"seconds_left":sig.seconds_left,"dry_run":self.config.dry_run,"ts":trade.ts,
        })).await?;

        let pos = self.state.get_or_create(&market.slug, market.end_ts);
        pos.add_trade(trade);
        self.state.save().await?;
        Ok(())
    }

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
        let fee = taker_fee(opp_ask);
        let full_c = full_cost_per_share(opp_ask);
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
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
            "phase":phase_label,"market":market.slug,"direction":opp_dir,"price":opp_ask,
            "shares":shares,"projected_pnl":projected_pnl,"seconds_left":seconds_left,
            "dry_run":self.config.dry_run,"ts":trade.ts,
        })).await?;

        let pos = self.state.get_or_create(&market.slug, market.end_ts);
        pos.add_trade(trade);
        pos.phase = Phase::Locked;
        self.state.save().await?;
        Ok(())
    }

    // ── 结算查询 ──────────────────────────────────────────────────────────

    async fn check_settlements(&mut self) -> Result<()> {
        let pending = self.state.pending_settlement();
        if pending.is_empty() { return Ok(()); }

        let mut changed = false;
        for (slug, pos) in pending {
            let Some(winner) = self.client.fetch_winning_outcome(&slug).await else { continue };

            let pnl = if winner == "Up" {
                pos.pnl_if_up_wins()
            } else {
                pos.pnl_if_down_wins()
            };

            let emoji = if pnl >= 0.0 { "✅" } else { "❌" };
            info!(
                "[SMART SETTLE] {} | 赢家={} | Up仓={:.0}@{:.3} Down仓={:.0}@{:.3} | PNL={:+.2} {}",
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
                "phase":"settlement","slug":slug,"winner":winner,"pnl":pnl,"ts":chrono::Utc::now().timestamp()
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
        if let Some(p) = self.signal_file.parent() {
            fs::create_dir_all(p).await?;
        }
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

fn beijing_now() -> String {
    beijing_time(chrono::Utc::now().timestamp())
}
