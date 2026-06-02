use crate::clob::{BookCache, ClobClient, Market};
use crate::config::Config;
use crate::executor::OrderExecutor;
use crate::feeds::{BinanceFeed, ChainlinkFeed};
use crate::position::{full_cost_per_share, taker_fee, MarketPosition, Phase, TradeRecord};
use crate::state::SmartStateStore;
use crate::ws::MarketWs;
use crate::zscore::ZScoreModel;
use anyhow::Result;
use tracing::{debug, info, warn};
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
/// 结算检查的最小间隔(秒)。结算结果有链上延迟、本就不需逐 tick 查;
/// 节流到此间隔后,结算用的网络请求不再卡在每个决策 tick 前面拖慢入场。
const SETTLEMENT_CHECK_INTERVAL: i64 = 10;

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
    /// 已采集训练样本的盘口(每盘只记一次特征快照,防重复)。LightGBM训练数据。
    pub sampled_slugs: std::collections::HashSet<String>,
    /// 上次结算检查的 unix 秒。决策主循环据此节流结算网络请求(见 SETTLEMENT_CHECK_INTERVAL),
    /// 让结算查询不再每 tick 阻塞、拖慢下一盘入场。
    pub last_settlement_check: i64,
    /// LightGBM 影子模型(model/ 目录就绪时加载)。每盘记一条"模型预测 vs z vs 结果",
    /// 不参与下单;待影子胜率稳超 z 再接管。None=模型未训出,影子静默跳过。
    pub shadow: Option<crate::model::LgbModel>,
    /// 影子模型目录,及已加载 model.txt 的 mtime(0=未加载)。用于热重载:
    /// 训练 timer 训出新模型后,bot 在下个新盘自动加载,无需重启(见 maybe_reload_shadow)。
    pub model_dir: PathBuf,
    pub shadow_mtime: i64,
}

/// 读 model.txt 的修改时间(unix 秒);不存在则 0。用于热重载判新。
fn model_mtime(dir: &std::path::Path) -> i64 {
    std::fs::metadata(dir.join("model.txt"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
        // 启动时一次性建好 signal 目录,write_signal 热路径上不再每条 create_dir_all。
        if let Some(p) = signal_file.parent() {
            let _ = fs::create_dir_all(p).await;
        }
        // 影子模型:数据目录下的 model/(train.py 输出处)。未训出则 None,影子静默跳过。
        let model_dir = config
            .state_file
            .parent()
            .map(|p| p.join("model"))
            .unwrap_or_else(|| PathBuf::from("model"));
        let shadow_mtime = model_mtime(&model_dir);
        let shadow = crate::model::LgbModel::load(&model_dir);
        match &shadow {
            Some(_) => info!("[SHADOW] LightGBM 模型已加载({}),影子预测开启", model_dir.display()),
            None => info!("[SHADOW] 暂无模型({} 未就绪),影子跳过(训出后自动热加载)", model_dir.display()),
        }
        Ok(Self {
            config, state, client, cache, model,
            signal_file, first_allowed_start, ws,
            cached_market: None, executor, ideal_state,
            book_log_file, last_book_log_ts: 0,
            sampled_slugs: std::collections::HashSet::new(),
            last_settlement_check: 0,
            shadow, model_dir, shadow_mtime,
        })
    }

    pub async fn run_once(&mut self) -> Result<()> {
        // 结算检查节流:绝大多数 tick 直接跳过,避免逐 tick 的结算网络请求拖慢决策/入场。
        let now = chrono::Utc::now().timestamp();
        if now - self.last_settlement_check >= SETTLEMENT_CHECK_INTERVAL {
            self.last_settlement_check = now;
            self.check_settlements().await?;
        }

        let Some(market) = self.get_or_fetch_market().await else { return Ok(()); };

        if market.start_ts < self.first_allowed_start {
            debug!("[SMART] 等待新盘口，最早北京时间 {}", beijing_time(self.first_allowed_start));
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
                debug!("[SMART] {} WS盘口未就绪...", market.title);
                return Ok(());
            };
            let Some(da) = cache.get(&dn_token).and_then(|b| b.best_ask()) else {
                debug!("[SMART] {} WS盘口未就绪...", market.title);
                return Ok(());
            };
            (f64::from(ua.try_into().unwrap_or(0.5f32)),
             f64::from(da.try_into().unwrap_or(0.5f32)))
        };

        let pos = self.state.get_or_create(&market.slug, market.end_ts).clone();

        // ── 每盘记一条训练样本(特征快照,不管下不下单;LightGBM训练数据)──────────
        // 在固定窗口[240,290]记,每盘只记一次(sampled_slugs去重)。标签由Python训练时
        // 用slug join settlement(quant_signals.jsonl)拿。3-4天可攒1000+,无选择偏差。
        if seconds_left >= 240 && seconds_left <= 290 && !self.sampled_slugs.contains(&market.slug) {
            self.record_train_sample(&market, up_ask, dn_ask, seconds_left).await;
            self.sampled_slugs.insert(market.slug.clone());
        }

        // 路线四：ev_solo 纯单边裸持（数学上唯一正期望路径）。
        if self.config.entry_strategy == "ev_solo" {
            self.decide_ev_solo(&market, pos, up_ask, dn_ask, seconds_left).await?;
            return Ok(());
        }

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
            // 写 token→slug+outcome+end_ts 映射(复盘用,一劳永逸:book只有token,靠此join赢家)
            self.write_token_map(&market).await;
            // 新盘时检查训练 timer 是否训出/更新了模型,变了就热加载(免重启依赖)。
            self.maybe_reload_shadow();
        }
        self.cached_market = Some(market.clone());
        Some(market)
    }

    /// 热重载影子模型:model.txt 的 mtime 变化(训练 timer 训出新版)时重新加载。
    /// 每新盘调一次(5min/次),开销可忽略;让 06-06 训出的模型无需重启 bot 即生效。
    fn maybe_reload_shadow(&mut self) {
        let mt = model_mtime(&self.model_dir);
        if mt == self.shadow_mtime {
            return;
        }
        self.shadow_mtime = mt;
        self.shadow = crate::model::LgbModel::load(&self.model_dir);
        if self.shadow.is_some() {
            info!("[SHADOW] 检测到模型更新(mtime={mt}),已热加载,影子预测开启");
        }
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

        // 必须用真开盘价(start_ts处的chainlink),取不到就跳过——不退回"最新价"凑数,
        // 否则 price_to_beat 失真→z方向变形(原 .or_else(latest) 是隐患,已去掉)。
        let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
        if price_to_beat < 1000.0 {
            debug!("[SMART] {} 无真开盘价(chainlink过期/未就绪)，跳过", market.title);
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
            debug!("[SMART] {} 价格数据不足，跳过入场", market.title);
            return Ok(());
        };
        let Some(dir) = sig.direction() else {
            debug!("[SMART] {} z={:.3} 信号不足，不入场", market.title, sig.z);
            return Ok(());
        };

        let entry_ask = if dir == "Up" { up_ask } else { dn_ask };
        if entry_ask < TREND_ENTRY_MIN || entry_ask > TREND_ENTRY_MAX {
            debug!(
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
        // 入场信号快照:丰富特征(为 LightGBM 铺路),结算后 join winner 作训练标签。
        let mut feat = self.build_features(&sig, dir, entry_ask, up_ask, dn_ask, seconds_left);
        self.add_book_depth(&mut feat, market).await;
        feat["phase"] = serde_json::json!("entry_signal");
        feat["market"] = serde_json::json!(market.slug);
        feat["strategy"] = serde_json::json!("zscore");
        self.write_signal(&feat).await?;
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

        // ── P2.5：早止损（方向押错时，不等 T-60 强制线提前锁亏）──────────────
        // 数据：旧实盘29场锁亏100%卡T-60被迫天价锁(中位0.66,最贵0.99)，单次均-1.85，
        // 是最大失血点。这里在方向刚转坏、亏损还小时提前补对面锁平，把大亏压成可控小亏。
        //
        // 触发信号 = "未实现方向亏损" = 主边份额 ×(主边现价 − 主边入场均价)。
        // 关键:不能用 worst_pnl —— 单边裸仓的 worst_pnl 恒为 ≈-满仓成本(假设对面赢),
        // 入场第一秒就 <止损线 会导致开盘秒锁。用现价跌幅才不误触发。
        //
        // 时间门槛(stop_loss_max_seconds_left):前段(剩余>此值)绝不止损,给5分钟行情时间,
        // 避免开盘段被正常波动晃出(之前剩280s就止损=刚入场就投降的问题)。
        // 只在盘后半段、方向确实没回来时才认小亏,而不是死扛到T-60天价锁。
        // naked 模式下完全关闭早止损(去掉锁亏=方向错就裸持,不提前补对面)。
        if self.config.stop_loss_factor > 0.0
            && self.config.force_loss_mode != "naked"
            && lock_qty > 0.0
            && seconds_left > self.config.force_lock_seconds_left
            && seconds_left <= self.config.stop_loss_max_seconds_left
        {
            let main_avg = if main_dir == "Up" { pos.up_avg_full() } else { pos.down_avg_full() };
            // 主边现价用 main_ask 近似(买一侧);跌破入场价即方向走坏
            let unrealized = main_shares * (main_ask - main_avg);
            let stop_line = -(shares * self.config.stop_loss_factor);
            if unrealized <= stop_line && opp_ask <= self.config.stop_loss_max_opp {
                let proj = pos.worst_pnl_if_add(opp_dir, opp_ask, lock_qty);
                info!(
                    "[SMART STOPLOSS {mode}] {} 方向亏{unrealized:+.2}≤{stop_line:.2} 止损买{opp_dir}@{opp_ask:.3} ×{lock_qty:.0}份  锁定{proj:+.2}  T-{seconds_left}s",
                    market.title
                );
                self.do_lock(&market, &pos, opp_dir, opp_ask, lock_qty, proj, "lock_loss").await?;
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
                debug!("[SMART] {} 趋势追单会使worst={p4_worst:+.2} < 下限{TREND_WORST_PNL_FLOOR}，跳过",
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
                } else if self.config.force_loss_mode == "naked" {
                    // 去掉锁亏:方向押错不补对面、不花钱对锁,裸持到结算认那一边成本。
                    // 赌"行情常回来";最坏=入场那笔成本归零,但不再追加确定支出去锁亏。
                    info!(
                        "[SMART FORCE NAKED {mode}] {} 不锁亏裸持到结算 主{main_dir}{main_shares:.0}份 worst={proj:+.2}  T-{seconds_left}s",
                        market.title
                    );
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

        // 等待(每 tick 状态,降到 debug 以免逐 tick 同步写日志拖慢主循环)
        debug!(
            "[SMART] {} {main_dir}{main_shares:.0}份@{:.3} opp@{opp_ask:.3}  worst_pnl={cur_worst:+.2}  T-{seconds_left}s",
            market.title,
            if main_dir == "Up" { pos.up_avg_full() } else { pos.down_avg_full() }
        );
        Ok(())
    }

    // ── 路线四：ev_solo 纯单边裸持（数学上唯一正期望路径）──────────────────────
    //
    // z-score 定方向 → 只买该边、不对冲、不锁利、不止损 → 裸持到结算。
    // 依据: 154场实测 z-score 方向胜率 57.8%(>50%有edge)。
    // 数学: 对冲腿在7%费下每份边际EV必<0(已证明),故彻底单边。
    // EV/份 = 胜率×(1-fc) - (1-胜率)×fc。仅在 ev_solo_min_ask≤ask≤max_ask 时入场。
    // 纯记录 entry_signal(含z/价/方向),结算后 join winner 验证胜率是否稳。
    async fn decide_ev_solo(
        &mut self,
        market: &Market,
        pos: MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        // 只在 Waiting 时入场;入场后裸持(Holding/Locked 不做任何动作)
        if !matches!(pos.phase, Phase::Waiting) { return Ok(()); }
        if seconds_left < ENTRY_MIN_SECONDS_LEFT { return Ok(()); }

        // 真开盘价取不到则跳过(不退回最新价,见 decide_waiting 注释)。
        let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
        if price_to_beat < 1000.0 { return Ok(()); }

        let Some(sig) = self.model.compute(price_to_beat, seconds_left) else { return Ok(()); };
        let Some(dir) = sig.direction() else {
            debug!("[EV_SOLO] {} z={:.3} 信号不足,不入场", market.title, sig.z);
            return Ok(());
        };
        let ask = if dir == "Up" { up_ask } else { dn_ask };
        // 价位过滤:只在 [min,max] 入场(避开贵价负EV区 + 过低赔率差区)
        if ask < self.config.ev_solo_min_ask || ask > self.config.ev_solo_max_ask {
            debug!("[EV_SOLO] {} {dir}@{ask:.3} 不在入场区[{:.2},{:.2}],跳过 z={:.3}",
                market.title, self.config.ev_solo_min_ask, self.config.ev_solo_max_ask, sig.z);
            return Ok(());
        }

        let qty = self.config.ev_solo_qty;
        let fc = full_cost_per_share(ask);
        let ev_per = 0.578 * (1.0 - fc) - 0.422 * fc; // 用实测胜率估每份EV(仅日志参考)
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        info!("[EV_SOLO {mode}] {} 单边买{dir}@{ask:.3}×{qty:.0} z={:.3} 估EV{ev_per:+.3}/份 T-{seconds_left}s",
            market.title, sig.z);

        // 记录入场信号(丰富特征,为 LightGBM 铺路;结算后 join winner 作训练标签)
        let mut feat = self.build_features(&sig, dir, ask, up_ask, dn_ask, seconds_left);
        self.add_book_depth(&mut feat, market).await;
        feat["phase"] = serde_json::json!("entry_signal");
        feat["market"] = serde_json::json!(market.slug);
        feat["strategy"] = serde_json::json!("ev_solo");
        self.write_signal(&feat).await?;

        // 买单边,然后标 Locked 裸持到结算(不进任何后续决策)
        if self.do_buy(&market, dir, ask, qty, "ev_solo", price_to_beat).await? {
            let p = self.state.get_or_create(&market.slug, market.end_ts);
            p.phase = Phase::Locked;
            self.state.save().await?;
        }
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
                    // 实盘发单但未成交=扑空(FAK taking=0)。记一条 phase:"miss" 到 signals,
                    // 供 stats 算"下单未成交率"。纯增量记录,不改任何成交/决策逻辑;
                    // train.py 只读 phase=settlement/kind=train_sample,不读 miss,训练不受影响。
                    if !fill.success {
                        let _ = self.write_signal(&serde_json::json!({
                            "phase": "miss", "market": market.slug,
                            "direction": dir, "price": price, "shares": shares,
                            "label": phase_label,
                            "dry_run": self.config.dry_run,
                            "ts": chrono::Utc::now().timestamp(),
                        })).await;
                    }
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
        // 目录已在 new() 建好,此处不再 create_dir_all。
        let mut f = OpenOptions::new().create(true).append(true).open(&self.signal_file).await?;
        f.write_all((serde_json::to_string(v)? + "\n").as_bytes()).await?;
        Ok(())
    }

    /// 每盘记一条训练样本(特征快照)到 book 目录的 train_samples.jsonl。
    /// 纯记录、不影响交易。z信号缺失时跳过(无特征)。标签由训练脚本join settlement。
    async fn record_train_sample(&self, market: &Market, up_ask: f64, dn_ask: f64, seconds_left: i64) {
        // 训练样本也要真开盘价(否则特征里的 ct-b/z 失真,污染训练集)。
        let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
        if price_to_beat < 1000.0 { return; }
        let Some(sig) = self.model.compute(price_to_beat, seconds_left) else { return; };
        // 方向取 z 倾向(>0看Up),仅作记录;入场价取该方向 ask
        let dir = if sig.z >= 0.0 { "Up" } else { "Down" };
        let entry_ask = if dir == "Up" { up_ask } else { dn_ask };
        let mut feat = self.build_features(&sig, dir, entry_ask, up_ask, dn_ask, seconds_left);
        self.add_book_depth(&mut feat, market).await;
        // 影子预测:模型就绪时记一条"模型置信 vs z vs(待结算)结果"到 signal 文件,不参与下单。
        // 结算后用 market join settlement(winner)即可评估模型挑盘能力,达标再接管。
        if let Some(m) = &self.shadow {
            if let Some(p) = m.predict_proba(&feat) {
                let rec = serde_json::json!({
                    "phase": "shadow",
                    "market": market.slug,
                    "ts": chrono::Utc::now().timestamp(),
                    "z": sig.z,
                    "z_dir": dir,                 // 模型对 z 方向打置信分,方向同 z
                    "model_p": p,                 // 校准后 P(z 方向正确)
                    "model_bet": p >= m.threshold, // 是否达下注阈值
                    "thr": m.threshold,
                });
                let _ = self.write_signal(&rec).await;
            }
        }
        feat["slug"] = serde_json::json!(market.slug);
        feat["end_ts"] = serde_json::json!(market.end_ts);
        feat["kind"] = serde_json::json!("train_sample");
        let path = self.config.book_record_dir.join("train_samples.jsonl");
        if let Some(p) = path.parent() { let _ = fs::create_dir_all(p).await; }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path).await {
            let _ = f.write_all((feat.to_string() + "\n").as_bytes()).await;
        }
    }

    /// 构造入场时刻的丰富特征集(为 LightGBM 铺路)。纯记录,无副作用。
    /// 含: z全套、多窗口量价、多窗口动量、盘口衍生、时间特征。
    fn build_features(
        &self, sig: &crate::zscore::ZSignal, dir: &str,
        entry_ask: f64, up_ask: f64, dn_ask: f64, seconds_left: i64,
    ) -> serde_json::Value {
        let now = chrono::Utc::now().timestamp();
        // 多窗口量价
        let f30 = self.model.binance_flow(now, 30);
        let f60 = self.model.binance_flow(now, 60);
        let f120 = self.model.binance_flow(now, 120);
        // 多窗口动量
        let m10 = self.model.binance_momentum(now, 10).unwrap_or(0.0);
        let m30 = self.model.binance_momentum(now, 30).unwrap_or(0.0);
        let m60 = self.model.binance_momentum(now, 60).unwrap_or(0.0);
        let m120 = self.model.binance_momentum(now, 120).unwrap_or(0.0);
        // 时间特征(北京小时)
        let bj_hour = ((now + 8*3600) / 3600) % 24;
        serde_json::json!({
            "ts": now, "direction": dir, "entry_ask": entry_ask,
            // 盘口
            "up_ask": up_ask, "dn_ask": dn_ask, "ask_sum": up_ask + dn_ask,
            // z全套
            "z": sig.z, "p_up": sig.p_up, "p_down": sig.p_down,
            "e": sig.e, "v": sig.v, "ct": sig.ct, "xt": sig.xt, "b": sig.b,
            "sigma120": sig.sigma120, "basis60": sig.basis60,
            // 衍生:价差
            "ct_minus_b": sig.ct - sig.b,        // chainlink相对开盘
            "xt_minus_ct": sig.xt - sig.ct,      // binance-chainlink basis
            // 多窗口量价不平衡
            "flow_imb_30": f30.imbalance, "flow_imb_60": f60.imbalance, "flow_imb_120": f120.imbalance,
            "flow_buy_60": f60.buy_vol, "flow_sell_60": f60.sell_vol, "flow_trades_60": f60.trades,
            // 30/120 窗口绝对量(只记不启用,补齐量特征;之前只有60窗口有绝对量)
            "flow_buy_30": f30.buy_vol, "flow_sell_30": f30.sell_vol, "flow_trades_30": f30.trades,
            "flow_buy_120": f120.buy_vol, "flow_sell_120": f120.sell_vol, "flow_trades_120": f120.trades,
            // 多窗口动量
            "mom_10": m10, "mom_30": m30, "mom_60": m60, "mom_120": m120,
            // 时间
            "seconds_left": seconds_left, "bj_hour": bj_hour,
        })
    }

    /// 往特征 json 补 Polymarket 盘口深度:Up/Down 两个 token 各自的 bid/ask 挂单总量。
    /// **只记不启用**(train.py FEATURES 未纳入),为将来"盘口深度"特征铺路;纯记录、零风险。
    async fn add_book_depth(&self, feat: &mut serde_json::Value, market: &Market) {
        let cache = self.cache.read().await;
        let depth = |tok: Option<&str>, ask_side: bool| -> f64 {
            tok.and_then(|t| cache.get(t))
                .map(|b| {
                    let lv = if ask_side { &b.asks } else { &b.bids };
                    lv.iter()
                        .map(|(_, s)| s.to_string().parse::<f64>().unwrap_or(0.0))
                        .sum()
                })
                .unwrap_or(0.0)
        };
        let up = market.token_for("Up");
        let dn = market.token_for("Down");
        feat["up_bid_depth"] = serde_json::json!(depth(up, false));
        feat["up_ask_depth"] = serde_json::json!(depth(up, true));
        feat["dn_bid_depth"] = serde_json::json!(depth(dn, false));
        feat["dn_ask_depth"] = serde_json::json!(depth(dn, true));
    }

    /// 写 token→slug/outcome/end_ts 映射到 book 目录的 token_map.jsonl(复盘join赢家用)。
    /// book 录的只有 token,靠此映射把逐tick价格关联到盘口+方向,再join结算赢家。
    async fn write_token_map(&self, market: &Market) {
        let dir = &self.config.book_record_dir;
        let _ = fs::create_dir_all(dir).await;
        let path = dir.join("token_map.jsonl");
        let mut lines = String::new();
        for (i, tok) in market.token_ids.iter().enumerate() {
            let outcome = market.outcomes.get(i).cloned().unwrap_or_default();
            let rec = serde_json::json!({
                "token": tok, "slug": market.slug, "outcome": outcome,
                "end_ts": market.end_ts, "ts": chrono::Utc::now().timestamp(),
            });
            lines.push_str(&rec.to_string());
            lines.push('\n');
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path).await {
            let _ = f.write_all(lines.as_bytes()).await;
        }
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
