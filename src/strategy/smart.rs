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
pub(crate) const ARB_THRESHOLD: f64 = 0.995;
/// P2 锁利门槛系数现由 config.lock_min_profit_factor 控制（门槛 = order_shares × factor）。
/// 注:原 P4 趋势入场价带常量 TREND_ENTRY_MIN/MAX 已废弃——入场价带改用
/// config.ev_solo_min_ask / ev_solo_max_ask（zscore 入场已对齐 ev_solo）。
/// P4 趋势追单步长（价格涨 0.05 才追下一笔）
pub(crate) const TREND_STEP: f64 = 0.05;
/// P4 最多追多少笔
pub(crate) const MAX_TREND_TRADES: usize = 5;
/// P4 追单允许 worst_pnl 最多恶化的下限 = −(order_shares × 此系数)。
/// 必须随份额缩放:原设计份额5对应−30(系数6);若写死−30,份额改20后入场就−10、追1笔即撞墙,
/// 追单瘫痪→利润跑不起来(份额20回测胜率从98%崩到43%、1s延迟转负)。
pub(crate) const TREND_WORST_PNL_FLOOR_FACTOR: f64 = 6.0;
/// P3 减险触发:单次买入须把 worst_pnl 改善 ≥ order_shares × 此系数(避免每秒刷单)。
/// 同样随份额缩放:原设计份额5对应1.0(系数0.2)。
pub(crate) const REBALANCE_MIN_IMPROVE_FACTOR: f64 = 0.2;
/// 冷门彩票：临近结束时便宜边 ask ≤ 此价才买（下行受限于极低价）
pub(crate) const LOTTERY_MAX_PRICE: f64 = 0.10;
/// P3 微批份额 = order_shares / 4
pub(crate) const MICRO_DIVISOR: f64 = 4.0;
/// 最后多少秒不开新首单
pub(crate) const ENTRY_MIN_SECONDS_LEFT: i64 = 60;
/// 结算检查的最小间隔(秒)。结算结果有链上延迟、本就不需逐 tick 查;
/// 节流到此间隔后,结算用的网络请求不再卡在每个决策 tick 前面拖慢入场。
const SETTLEMENT_CHECK_INTERVAL: i64 = 10;

pub(crate) fn strategy_order_shares(shares: f64) -> Option<f64> {
    if !shares.is_finite() || shares <= 0.0 {
        return None;
    }
    Some(shares.round().max(1.0))
}

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
    /// 已狙击的盘(sniper 策略每盘只狙一次,防重复下单)。
    pub sniped_slugs: std::collections::HashSet<String>,
    /// 已预热缓存的盘(sniper 每盘开始预热 tick/neg-risk/fee,防重复预热)。
    pub primed_slugs: std::collections::HashSet<String>,
    /// 上次结算检查的 unix 秒。决策主循环据此节流结算网络请求(见 SETTLEMENT_CHECK_INTERVAL),
    /// 让结算查询不再每 tick 阻塞、拖慢下一盘入场。
    pub last_settlement_check: i64,
    /// 盘内 z 采集上次写入的 unix 秒(每秒最多记一条 z 快照,节流)。
    pub z_last_ts: i64,
    /// LightGBM 影子模型(model/ 目录就绪时加载)。每盘记一条"模型预测 vs z vs 结果",
    /// 不参与下单;待影子胜率稳超 z 再接管。None=模型未训出,影子静默跳过。
    pub shadow: Option<crate::model::LgbModel>,
    /// 影子模型目录,及已加载 model.txt 的 mtime(0=未加载)。用于热重载:
    /// 训练 timer 训出新模型后,bot 在下个新盘自动加载,无需重启(见 maybe_reload_shadow)。
    pub model_dir: PathBuf,
    pub shadow_mtime: i64,
    /// accum 路线每盘状态(slug→方向锚点+追涨/补仓进度+锁定)。
    pub accum: std::collections::HashMap<String, AccumLeg>,
    /// 是否已执行启动对账(reconcile_on_startup)。run_once 首次跑时置 true 并对账一次。
    pub reconciled: bool,
    /// maker 挂单尝试节流:key=`slug|dir`,value=上次尝试挂单的时间戳(秒)。
    /// 覆盖两种重复挂单场景:DryRun 撤单后重挂、LIVE 挂单失败(余额不足等)重试。
    /// 距上次尝试不足 maker_quote_ttl_secs 则跳过,避免每 tick 重复挂单刷屏。
    pub maker_attempt: std::collections::HashMap<String, i64>,
    /// zquote 每盘开局锁定的 z 方向(slug→"Up"/"Down")。开局定一次,之后不再随 z 变。
    pub zquote_dir: std::collections::HashMap<String, String>,
}

/// accum 路线每盘状态。首笔 z 定主腿方向(盈亏锚点),之后谁涨追谁/谁跌补谁,
/// 计算模块把结算锁到「主腿赢≥target、主腿输≥−maxloss」,达标即停止下单裸持。
#[derive(Clone)]
pub struct AccumLeg {
    /// z 主腿方向("Up"/"Down"),盈亏锚点,整盘不换。
    pub main_dir: String,
    /// Up / Down 两边各自已追涨的档位下标(谁涨追谁,两边都可能追)。
    pub up_chase: Vec<usize>,
    pub dn_chase: Vec<usize>,
    /// Up / Down 两边各自已补仓的档位下标(谁跌补谁)。
    pub up_dip: Vec<usize>,
    pub dn_dip: Vec<usize>,
    /// 盈亏已锁住(两个结算情景都达标)→ 停止一切下单。
    pub locked: bool,
    /// 晚场顺势补救已触发过(每盘只补救一次)。
    pub rescued: bool,
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
            sniped_slugs: std::collections::HashSet::new(),
            primed_slugs: std::collections::HashSet::new(),
            last_settlement_check: 0,
            z_last_ts: 0,
            shadow, model_dir, shadow_mtime,
            accum: std::collections::HashMap::new(),
            reconciled: false,
            maker_attempt: std::collections::HashMap::new(),
            zquote_dir: std::collections::HashMap::new(),
        })
    }

    pub async fn run_once(&mut self) -> Result<()> {
        // 启动对账(只跑一次):把本地 state open_orders 与链上挂单对齐,清幻影、告警孤儿。
        // DryRun 下 list_open_orders 返回空、usdc_balance 返回 0,基本空跑,安全。
        if !self.reconciled {
            self.reconciled = true;
            if let Err(e) = self.reconcile_on_startup().await { warn!("[RECONCILE] {e:#}"); }
        }

        // 结算检查节流:绝大多数 tick 直接跳过,避免逐 tick 的结算网络请求拖慢决策/入场。
        let now = chrono::Utc::now().timestamp();
        if now - self.last_settlement_check >= SETTLEMENT_CHECK_INTERVAL {
            self.last_settlement_check = now;
            self.check_settlements().await?;
        }

        if let Err(e) = self.harvest_makers().await { warn!("[MAKER HARVEST] {e:#}"); }

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

        // sniper 开盘预热(每盘一次):填好 Up/Down 的 tick/neg-risk/fee 缓存 + 打热一条连接。
        // 实盘统计:96% 下单在开盘后≤30s、全部≤48s,远早于 CF 切空闲连接(~100s),
        // 故开盘 prewarm 一次即覆盖全部下单窗口,无需整盘每 50s ping(84% 时间根本不下单)。
        // (连接握手实测仅 ~45ms;prewarm 省的是这 45ms,暴雷在平台撮合、与连接无关。)
        if matches!(self.config.entry_strategy.as_str(), "sniper" | "accum") && !self.primed_slugs.contains(&market.slug) {
            self.executor.prime_token(&up_token).await;
            self.executor.prime_token(&dn_token).await;
            let exec = self.executor.clone();
            tokio::spawn(async move { exec.prewarm().await; }); // 开盘打热连接,不阻塞决策
            self.primed_slugs.insert(market.slug.clone());
        }

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

        // ── 盘内 z 采集(常驻,独立于策略;accum/sniper 在跑也照采)──────────────
        // 每秒记一条真 z 快照(z/p_up/e/v/σ120 + 两边 ask),为"盘内便宜边+真z"回测攒数据。
        // 现状只有开盘单点(entry_signal),盘内逐秒缺;这条补上,几天后才能验"盘内赢多亏少"。
        if self.config.z_record_enabled && now > self.z_last_ts {
            let pb = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
            if pb >= 1000.0 && now >= market.start_ts {
                if let Some(sig) = self.model.compute(pb, seconds_left) {
                    self.z_last_ts = now;
                    let rec = serde_json::json!({
                        "phase":"z_tick","market":market.slug,"ts":now,
                        "seconds_left":seconds_left,
                        "z":sig.z,"p_up":sig.p_up,"e":sig.e,"v":sig.v,"sigma120":sig.sigma120,
                        "up_ask":up_ask,"dn_ask":dn_ask,
                    });
                    self.write_signal(&rec).await?;
                }
            }
        }

        // 路线四：ev_solo 纯单边裸持（数学上唯一正期望路径）。
        if self.config.entry_strategy == "ev_solo" {
            self.decide_ev_solo(&market, pos, up_ask, dn_ask, seconds_left).await?;
            return Ok(());
        }

        // 路线五：sniper 延迟套利狙击(binance 突破 → FOK 限价 → 裸持)。
        if self.config.entry_strategy == "sniper" {
            self.decide_sniper(&market, up_ask, dn_ask, seconds_left).await?;
            return Ok(());
        }

        // 路线六：accum 双边追涨补仓 + 计算模块(谁涨追谁/谁跌补谁,赢≥12/亏≤7,锁住即停)。
        if self.config.entry_strategy == "accum" {
            self.decide_accum(&market, up_ask, dn_ask, seconds_left).await?;
            return Ok(());
        }

        // 路线七:zquote z定方向 + 双边固定价 maker 挂单等成交。
        if self.config.entry_strategy == "zquote" {
            self.decide_zquote(&market, up_ask, dn_ask, seconds_left).await?;
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

    pub(crate) async fn do_buy(
        &mut self,
        market: &Market,
        dir: &str,
        price: f64,
        shares: f64,
        phase_label: &str,
        price_to_beat: f64,
    ) -> Result<bool> {
        let Some(shares) = strategy_order_shares(shares) else {
            warn!("[SMART] {} {dir} {phase_label} 下单份额非法: {shares}", market.title);
            return Ok(false);
        };
        // maker 模式:入场类买单改挂 GTC maker 单(省 taker 费)。market 模式原样不动。
        if self.config.order_mode == "maker" {
            return self.do_buy_maker(market, dir, price, shares, phase_label, price_to_beat).await;
        }
        // audit:决策要下单、真正发单前记 intent。
        self.write_signal(&serde_json::json!({
            "phase": "intent", "market": market.slug,
            "direction": dir, "shares": shares, "price": price,
            "label": phase_label, "mode": self.config.order_mode,
            "ts": chrono::Utc::now().timestamp(),
        })).await?;

        // 默认不设价格帽:由 SDK market order 按 shares 扫订单簿算 cutoff 价格。
        let limit_override: Option<f64> = None;
        let fill = self.place_order(market, dir, price, shares, phase_label, limit_override).await;

        // audit:executor 返回后记 submit(无论成交与否,fill 为 None 表示发单失败)。
        if let Some(f) = &fill {
            self.write_signal(&serde_json::json!({
                "phase": "submit", "order_id": f.order_id, "success": f.success,
                "filled_shares": f.filled_shares, "filled_price": f.filled_price,
                "market": market.slug, "direction": dir,
                "ts": chrono::Utc::now().timestamp(),
            })).await?;
        }

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
        // audit:market 成交结构化 fill 记录(便于全生命周期 join intent/submit/fill)。
        self.write_signal(&serde_json::json!({
            "phase": "fill", "order_id": fill.order_id, "market": market.slug,
            "direction": dir, "price": rp, "shares": rs,
            "full_cost": full_cost_per_share(rp), "label": phase_label,
            "dry_run": self.config.dry_run, "ts": chrono::Utc::now().timestamp(),
        })).await?;
        record_trade(&mut self.state, market, dir, rp, rs, phase_label, price_to_beat, false);
        self.state.save().await?;
        Ok(true)
    }

    // ── 锁仓（切换 Phase::Locked）─────────────────────────────────────────

    pub(crate) async fn do_lock(
        &mut self,
        market: &Market,
        pos: &MarketPosition,
        dir: &str,
        price: f64,
        shares: f64,
        projected_pnl: f64,
        phase_label: &str,
    ) -> Result<()> {
        let Some(shares) = strategy_order_shares(shares) else {
            warn!("[SMART LOCK] {} {dir} {phase_label} 下单份额非法: {shares}", market.title);
            return Ok(());
        };
        // 纯按开关:maker 模式下锁仓/对冲也挂 maker 单(不立即切 Locked,成交由 harvest 记账)
        if self.config.order_mode == "maker" {
            return self.do_buy_maker(market, dir, price, shares, phase_label, pos.price_to_beat).await.map(|_| ());
        }
        let mode   = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        let secs   = (pos.end_ts - chrono::Utc::now().timestamp()).max(0);

        let fill = self.place_order(market, dir, price, shares, phase_label, None).await;

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

    pub(crate) fn order_shares(&self) -> f64 {
        let shares = self.config.order_shares.to_string().parse::<f64>().unwrap_or(20.0);
        strategy_order_shares(shares).unwrap_or(20.0)
    }

    pub(crate) async fn write_signal(&self, v: &serde_json::Value) -> Result<()> {
        // 目录已在 new() 建好,此处不再 create_dir_all。
        let mut f = OpenOptions::new().create(true).append(true).open(&self.signal_file).await?;
        f.write_all((serde_json::to_string(v)? + "\n").as_bytes()).await?;
        Ok(())
    }

}

/// 把一笔成交记入指定状态库（主账或影子账通用）。
pub(crate) fn record_trade(
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

pub(crate) fn beijing_now() -> String { beijing_time(chrono::Utc::now().timestamp()) }
