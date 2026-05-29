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

// ── 策略参数 ──────────────────────────────────────────────────────────────
//
// 策略核心：趋势追单 + 对边便宜时锁利
//
// 逻辑：
//  1. z-score 确认方向（如 BTC 上涨 → 追 Up）
//  2. 当趋势方向价格在 [0.48, 0.70] 区间内，首单买入 order_shares 份
//  3. 每当价格再上涨 0.05，追加一笔（最多 5 笔 = 最大 100 份）
//  4. 对边价格跌到可以锁利（a*+d* < 0.95）时，等额买入对边 → 锁定利润
//  5. 最后 60s 仍未锁利 → 强制等额锁仓（可能锁亏）
//
// 核算示例（Up 连追5笔，涨到 0.75 锁利）：
//  追单总成本 ≈ $61.69（100份 Up，均价 0.617）
//  锁利成本   ≈ $26.31（100份 Down@0.25）
//  锁利后保底 ≈ +$12.00（无论谁赢，ROI 13.6%）

const TARGET_PROFIT_PER_SHARE: f64 = 0.05; // 锁利门槛 5¢/份（a*+d* < 0.95）
const TREND_ENTRY_MIN: f64 = 0.48;         // 首单入场价下限
const TREND_ENTRY_MAX: f64 = 0.70;         // 追单价格上限（>0.70 不追）
const TREND_STEP: f64 = 0.05;              // 价格每涨 0.05 追一笔
const MAX_TREND_TRADES: usize = 5;         // 最多追 5 笔
const ENTRY_MIN_SECONDS_LEFT: i64 = 60;    // 最后 60s 不开新首单

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
        let client = ClobClient::new(
            &config.clob_api_url,
            &config.gamma_api_url,
            &config.market_slug_prefix,
        );
        let model = ZScoreModel::new(chainlink, binance);
        let signal_file = config.signal_file.clone();
        let now = chrono::Utc::now().timestamp();
        let first_allowed_start = ((now / 300) + 1) * 300;

        Ok(Self {
            config, state, client, cache, model,
            signal_file, first_allowed_start, ws,
            cached_market: None,
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
            Phase::Waiting  => self.try_entry(&market, pos, up_ask, dn_ask, seconds_left).await?,
            Phase::Holding  => self.try_manage(&market, pos, up_ask, dn_ask, seconds_left).await?,
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
        let is_new = self.cached_market.as_ref()
            .map(|m| m.slug != market.slug)
            .unwrap_or(true);
        if is_new {
            self.ws.ensure_subscribed(&market.token_ids).await;
            info!("[SMART] 新盘口 {} 已订阅WS", market.slug);
        }
        self.cached_market = Some(market.clone());
        Some(market)
    }

    // ── 阶段1：首单入场 ────────────────────────────────────────────────────
    //
    // z-score 确认方向，趋势方向价格在 [TREND_ENTRY_MIN, TREND_ENTRY_MAX] 时入场。
    // 入场份额 = order_shares（CLI 可配置，默认 20 份）。

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
            info!("[SMART] {} Chainlink未就绪，跳过入场", market.title);
            return Ok(());
        }

        let Some(sig) = self.model.compute(price_to_beat, seconds_left) else {
            info!("[SMART] {} 价格数据不足，跳过入场", market.title);
            return Ok(());
        };

        let Some(dir) = sig.direction() else {
            info!("[SMART] {} z={:.3} 信号不足，不入场", market.title, sig.z);
            return Ok(());
        };

        let entry_ask = if dir == "Up" { up_ask } else { dn_ask };

        if entry_ask < TREND_ENTRY_MIN {
            info!(
                "[SMART] {} {dir}@{entry_ask:.3} < {TREND_ENTRY_MIN}，市场已极端偏向，不追",
                market.title
            );
            return Ok(());
        }
        if entry_ask > TREND_ENTRY_MAX {
            info!(
                "[SMART] {} {dir}@{entry_ask:.3} > {TREND_ENTRY_MAX}，价格过高不追",
                market.title
            );
            return Ok(());
        }

        let shares = self.order_shares();
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        info!(
            "[SMART ENTRY {mode}] {} {dir}@{entry_ask:.3} ×{shares:.0}份  z={:.3}  T-{seconds_left}s",
            market.title, sig.z
        );
        self.do_buy(&market, dir, entry_ask, shares, "entry", price_to_beat).await?;
        Ok(())
    }

    // ── 阶段2：趋势追单 + 锁利管理 ─────────────────────────────────────────
    //
    // 优先级：
    //  1. 锁利：a* + d* < 0.95 → 等额买入对边，锁定利润
    //  2. 趋势追单：趋势方向价格比上笔又涨了 TREND_STEP(0.05)，且未超过 TREND_ENTRY_MAX
    //     → 再买 order_shares 份（最多追 MAX_TREND_TRADES 笔）
    //  3. 60s 强制锁仓（可能锁亏，对边等额买入）

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

        // 双边都有 → 已完成锁仓，打印状态等结算
        if has_up && has_down {
            let up_wins   = pos.pnl_if_up_wins();
            let down_wins = pos.pnl_if_down_wins();
            info!(
                "[SMART] {} 已锁仓 Up={:.0}@{:.3} Down={:.0}@{:.3} Up赢={:+.2} Down赢={:+.2} T-{seconds_left}s",
                market.title,
                pos.up_shares, pos.up_avg_full(),
                pos.down_shares, pos.down_avg_full(),
                up_wins, down_wins
            );
            let p = self.state.get_or_create(&market.slug, market.end_ts);
            p.phase = Phase::Locked;
            self.state.save().await?;
            return Ok(());
        }

        let (trend_dir, opp_dir, trend_ask, opp_ask) = if has_up {
            ("Up",   "Down", up_ask, dn_ask)
        } else {
            ("Down", "Up",   dn_ask, up_ask)
        };

        let trend_shares = if has_up { pos.up_shares } else { pos.down_shares };
        let shares = self.order_shares();

        // ── 1. 锁利（最优先）─────────────────────────────────────────────
        if pos.can_lock_profit(opp_ask, TARGET_PROFIT_PER_SHARE) {
            let proj = pos.projected_locked_pnl(opp_ask);
            let a = pos.up_avg_full().max(pos.down_avg_full());
            info!(
                "[SMART] {} 触发锁利！{trend_dir}{trend_shares:.0}份 a*={:.3} opp@{opp_ask:.3} d*={:.3} a+d={:.3} 预计+${proj:.2} T-{seconds_left}s",
                market.title, a,
                full_cost_per_share(opp_ask),
                a + full_cost_per_share(opp_ask)
            );
            self.do_lock(&market, &pos, opp_dir, opp_ask, trend_shares, proj, "lock_profit").await?;
            return Ok(());
        }

        // ── 2. 趋势追单 ───────────────────────────────────────────────────
        // 当趋势方向价格比上笔买入再涨了 TREND_STEP，且总笔数未超上限
        let trend_trades: Vec<_> = pos.trades.iter()
            .filter(|t| t.side == trend_dir && !t.phase.contains("lock"))
            .collect();
        let trade_count = trend_trades.len();
        let last_trend_price = trend_trades.last().map(|t| t.price).unwrap_or(0.0);

        if trade_count < MAX_TREND_TRADES
            && trend_ask >= last_trend_price + TREND_STEP
            && trend_ask <= TREND_ENTRY_MAX
            && seconds_left > ENTRY_MIN_SECONDS_LEFT
        {
            let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
            info!(
                "[SMART TREND {mode}] {} 追单 {trend_dir}@{trend_ask:.3}（上笔{last_trend_price:.3}+{TREND_STEP}）×{shares:.0}份  第{}/{}笔  T-{seconds_left}s",
                market.title,
                trade_count + 1, MAX_TREND_TRADES
            );
            self.do_buy(&market, trend_dir, trend_ask, shares, "trend_chase", pos.price_to_beat).await?;
            return Ok(());
        }

        // ── 3. 60s 强制锁仓 ───────────────────────────────────────────────
        if seconds_left <= 60 {
            let proj = pos.projected_locked_pnl(opp_ask);
            let label = if proj >= 0.0 { "lock_profit" } else { "lock_loss" };
            self.do_lock(&market, &pos, opp_dir, opp_ask, trend_shares, proj, label).await?;
            return Ok(());
        }

        // 等待
        let a = pos.up_avg_full().max(pos.down_avg_full());
        let d = full_cost_per_share(opp_ask);
        info!(
            "[SMART] {} {trend_dir}{trend_shares:.0}份@{a:.3} opp@{opp_ask:.3}  a+d={:.3}（需<{:.2}才锁利）  第{}/{}笔  T-{seconds_left}s",
            market.title,
            a + d, 1.0 - TARGET_PROFIT_PER_SHARE,
            trade_count, MAX_TREND_TRADES
        );
        Ok(())
    }

    // ── 通用：买入 ────────────────────────────────────────────────────────

    async fn do_buy(
        &mut self,
        market: &Market,
        dir: &str,
        price: f64,
        shares: f64,
        phase_label: &str,
        price_to_beat: f64,
    ) -> Result<()> {
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
            "full_cost": full_c, "total_cost": full_c * shares,
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

    // ── 锁仓 ──────────────────────────────────────────────────────────────

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
        let fee    = taker_fee(opp_ask);
        let full_c = full_cost_per_share(opp_ask);
        let mode   = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        let secs   = (pos.end_ts - chrono::Utc::now().timestamp()).max(0);

        info!(
            "[SMART LOCK {mode} {}] {} {opp_dir}@{opp_ask:.3} ×{shares:.0}份  预计PNL≈${projected_pnl:+.2}  T-{secs}s",
            phase_label.to_uppercase(), market.title
        );

        let trade = TradeRecord {
            side: opp_dir.to_string(), shares, price: opp_ask,
            fee_per_share: fee, full_cost_per_share: full_c,
            total_cost: full_c * shares, phase: phase_label.to_string(),
            ts: chrono::Utc::now().timestamp(), time_bj: beijing_now(),
        };
        self.write_signal(&serde_json::json!({
            "phase": phase_label, "market": market.slug,
            "direction": opp_dir, "price": opp_ask, "shares": shares,
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

            let pnl = if winner == "Up" {
                pos.pnl_if_up_wins()
            } else {
                pos.pnl_if_down_wins()
            };
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
                "phase": "settlement", "slug": slug,
                "winner": winner, "pnl": pnl,
                "ts": chrono::Utc::now().timestamp()
            })).await?;
            changed = true;
        }

        if changed {
            self.state.save().await?;
            let s = self.state.summary();
            info!(
                "[SMART STATS] 共{}盘 锁{} 赢{} 输{}  净PNL ${:.2}",
                s.total, s.locked, s.win, s.lose, s.total_pnl
            );
        }
        Ok(())
    }

    fn order_shares(&self) -> f64 {
        self.config.order_shares.to_string().parse::<f64>().unwrap_or(20.0)
    }

    async fn write_signal(&self, v: &serde_json::Value) -> Result<()> {
        if let Some(p) = self.signal_file.parent() {
            fs::create_dir_all(p).await?;
        }
        let mut f = OpenOptions::new()
            .create(true).append(true)
            .open(&self.signal_file).await?;
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
