use anyhow::{bail, Result};
use rust_decimal::Decimal;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct Config {
    // 链/API
    pub clob_api_url: String,
    pub clob_v2_api_url: String,
    pub gamma_api_url: String,
    pub chain_id: u64,
    pub signature_type: u8,
    pub private_key: Option<String>,
    pub deposit_wallet: Option<String>,

    // 运行模式
    pub dry_run: bool,

    // 市场
    pub market_slug_prefix: String,
    pub market_ws_url: String,

    // 策略参数
    pub order_shares: Decimal,
    pub min_entry_price: Decimal,
    pub max_entry_price: Decimal,
    pub min_lock_profit: Decimal,
    pub fee_rate: Decimal,
    pub max_entry_delay_sec: u64,
    pub min_seconds_left: u64,

    // 锁仓/追单可调参数
    /// 锁利门槛系数：实际门槛 = order_shares × 此值（设5×0.2=$1 才锁利）
    pub lock_min_profit_factor: f64,
    /// 追单价格上限：main_ask 高于此值不再追单
    pub trend_chase_max_price: f64,
    /// 强制锁线（秒）：剩余 ≤ 此值触发强制处理
    pub force_lock_seconds_left: i64,
    /// 强制亏损模式："smooth"=按趋势顺势加注锁利；"lock"=旧行为等额对锁(锁亏)
    pub force_loss_mode: String,
    /// 磨平预算倍数：smooth 模式顺势加注最多再花 entry成本 × 此值
    pub smooth_budget_mult: f64,
    /// 早止损系数：worst_pnl ≤ -(order_shares×此值) 时不等T-60,立即补对面锁平认小亏。
    /// 0=关(旧行为,死扛到强制线)。数据显示锁亏100%卡T-60天价锁,早止损可把单次亏从-1.85压小。
    pub stop_loss_factor: f64,
    /// 早止损价格上限：对面 ask > 此值(天价)则不锁、裸持到结算,避免高价接盘放大亏损。
    pub stop_loss_max_opp: f64,
    /// 早止损最早触发时点：剩余秒数 > 此值时绝不止损(给行情时间,避免开盘段被正常波动晃出)。
    /// 默认120 → 只在盘后半段(已过≥180s)才允许止损。300秒盘:配合force_lock=60,止损窗口为剩余[60,120]。
    pub stop_loss_max_seconds_left: i64,

    // ── 入场策略选择 ──────────────────────────────────────────────────────
    /// "zscore"=z信号+锁利/追单/减险(baseline);"ev_solo"=z定方向+纯单边裸持(正期望主策略)
    pub entry_strategy: String,

    // ── 路线四:ev_solo 纯单边裸持(数学上唯一正期望路径)──────────────────────
    // z-score定方向→只买该边、不对冲、裸持到结算。靠 z-score 方向胜率(实测154场57.8%)
    // 在合适价位形成正EV。对冲腿在7%费下每份必亏(已数学证明),故彻底不对冲。
    /// 入场价上限:仅当看好侧 ask ≤ 此值才入场(避开贵价负EV区,实测0.52-0.58区负EV)。
    pub ev_solo_max_ask: f64,
    /// 入场价下限:太便宜(<此值)往往是方向已成定局、赔率差,跳过。
    pub ev_solo_min_ask: f64,
    /// 单边份额。
    pub ev_solo_qty: f64,
    /// ev_solo 入场最早时点(剩余秒数 ≥ 此值才入场)。默认240=只打开盘头60秒,
    /// 砍掉晚入场尾巴(实测 T<240 胜率塌方到 25~50%)。
    pub ev_solo_min_seconds_left: i64,
    /// flow_imb_30 同向闸:开盘前30秒资金流与 z 方向明确相悖则跳过入场。
    /// 依据实测 flow_imb_30 一致59%/矛盾51%(目前最强方向单信号,AUC 0.528)。默认开。
    pub ev_solo_flow_gate: bool,

    // 系统
    pub poll_ms: u64,
    pub state_file: PathBuf,
    pub signal_file: PathBuf,
    pub log_file: PathBuf,

    // ── 全盘口数据采集（与交易无关，常驻 WS 录制；按天分 JSONL）──────────────
    /// 是否开启 tick 级盘口录制（默认开）。安装后一启动即采集，不受 DRY_RUN 影响。
    pub book_record_enabled: bool,
    /// 录制输出目录：每天一个 quant_book-YYYYMMDD.jsonl
    pub book_record_dir: PathBuf,
    /// 旧版单文件逐秒盘口日志（quant_book.jsonl，仅当前盘）。默认关：已被 tick 采集器取代。
    pub book_legacy_log_enabled: bool,
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// 解析 .env 路径：优先环境变量 JY_ENV_PATH，其次按常见安装布局探测第一个存在的，
/// 都没有则回退旧默认 /opt/polymarket-copy/.env。bot 与 CLI 共用此函数，保证两边一致，
/// 不再写死单一目录（其他 VPS 用 /opt/jy-data 时也能对上）。
pub fn default_env_path() -> String {
    if let Ok(p) = std::env::var("JY_ENV_PATH") {
        if !p.trim().is_empty() {
            return p.trim().to_string();
        }
    }
    for cand in [
        "/opt/polymarket-copy/.env",
        "/opt/jy-data/.env",
        "/opt/jy-rust/.env",
        ".env",
    ] {
        if std::path::Path::new(cand).exists() {
            return cand.to_string();
        }
    }
    "/opt/polymarket-copy/.env".to_string()
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key).as_deref() {
        Ok("1") | Ok("true") | Ok("yes") => true,
        Ok("0") | Ok("false") | Ok("no") => false,
        _ => default,
    }
}

fn env_decimal(key: &str, default: &str) -> Decimal {
    let s = env(key, default);
    Decimal::from_str(&s).unwrap_or_else(|_| Decimal::from_str(default).unwrap())
}

fn env_u64(key: &str, default: u64) -> u64 {
    env(key, &default.to_string())
        .parse()
        .unwrap_or(default)
}

fn env_i64(key: &str, default: i64) -> i64 {
    env(key, &default.to_string())
        .parse()
        .unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    env(key, &default.to_string())
        .parse()
        .unwrap_or(default)
}

pub fn load(env_path: Option<&str>) -> Result<Config> {
    // env_path 为 None 时默认用规范路径，与 CLI(cli/main.rs ENV_PATH) 完全一致。
    // 否则 bot 不带 .env 参数启动会退回 CWD 解析(dotenv 搜 CWD + base="."),
    // 把 quant_state.json 写进工作目录,而 CLI 仍读 /opt/polymarket-copy/quant_state.json
    // → stats 永远"暂无数据文件"。见其他 VPS 模拟无统计表问题。
    let env_path: String = env_path.map(|s| s.to_string()).unwrap_or_else(default_env_path);
    dotenvy::from_path(&env_path).ok();

    let private_key = match std::env::var("PRIVATE_KEY") {
        Ok(k) if !k.trim().is_empty() => Some(k.trim().to_string()),
        _ => None,
    };
    let deposit_wallet = match std::env::var("DEPOSIT_WALLET_ADDRESS") {
        Ok(w) if !w.trim().is_empty() => Some(w.trim().to_string()),
        _ => None,
    };
    let dry_run = env_bool("DRY_RUN", true);
    let signature_type: u8 = env("SIGNATURE_TYPE", "3").parse().unwrap_or(3);

    // 实盘前置校验：DRY_RUN=0 必须有 PRIVATE_KEY；用代理钱包(sig_type≠0)还必须有 DEPOSIT_WALLET_ADDRESS。
    // API creds 由官方 SDK 在启动时用私钥自动派生/校验（见 executor::OrderExecutor::new）。
    if !dry_run {
        if private_key.is_none() {
            bail!("DRY_RUN=0 时必须设置 PRIVATE_KEY");
        }
        if signature_type != 0 && deposit_wallet.is_none() {
            bail!("DRY_RUN=0 且 SIGNATURE_TYPE={signature_type}（代理钱包）时必须设置 DEPOSIT_WALLET_ADDRESS");
        }
    }

    let base = PathBuf::from(
        std::path::Path::new(&env_path)
            .parent()
            .map(|p| p.to_str().unwrap_or(".").to_string())
            .unwrap_or_else(|| ".".to_string()),
    );

    Ok(Config {
        clob_api_url: env("CLOB_API_URL", "https://clob.polymarket.com"),
        // 注意：clob-v2.polymarket.com 现已 301 重定向到主域名，SDK 不跟随重定向，
        // 故默认直接用主域名（实测可正常认证/签名/成交）。
        clob_v2_api_url: env("CLOB_V2_API_URL", "https://clob.polymarket.com"),
        gamma_api_url: env("GAMMA_API_URL", "https://gamma-api.polymarket.com"),
        chain_id: env_u64("CHAIN_ID", 137),
        signature_type,
        private_key,
        deposit_wallet,
        dry_run,
        market_slug_prefix: env("QUANT_MARKET_SLUG_PREFIX", "btc-updown-5m"),
        market_ws_url: env(
            "MARKET_WS_URL",
            "wss://ws-subscriptions-clob.polymarket.com/ws/market",
        ),
        order_shares: env_decimal("QUANT_ORDER_SHARES", "20"),
        min_entry_price: env_decimal("JF_MIN_ENTRY_PRICE", "0.44"),
        max_entry_price: env_decimal("JF_MAX_ENTRY_PRICE", "0.65"),
        min_lock_profit: env_decimal("QUANT_ARBITRAGE_MIN_PROFIT", "0.15"),
        fee_rate: env_decimal("TAKER_FEE_RATE", "0.07"),
        max_entry_delay_sec: env_u64("JF_MAX_ENTRY_DELAY_SEC", 30),
        min_seconds_left: env_u64("QUANT_MIN_SECONDS_LEFT", 5),
        lock_min_profit_factor: env_f64("LOCK_MIN_PROFIT_FACTOR", 0.2),
        trend_chase_max_price: env_f64("TREND_CHASE_MAX_PRICE", 0.60),
        force_lock_seconds_left: env_i64("FORCE_LOCK_SECONDS_LEFT", 60),
        force_loss_mode: env("FORCE_LOSS_MODE", "smooth").to_lowercase(),
        smooth_budget_mult: env_f64("SMOOTH_BUDGET_MULT", 0.5),
        stop_loss_factor: env_f64("STOP_LOSS_FACTOR", 0.0),
        stop_loss_max_opp: env_f64("STOP_LOSS_MAX_OPP", 0.75),
        stop_loss_max_seconds_left: env_i64("STOP_LOSS_MAX_SECONDS_LEFT", 120),
        entry_strategy: env("ENTRY_STRATEGY", "ev_solo").to_lowercase(),
        ev_solo_max_ask: env_f64("EV_SOLO_MAX_ASK", 0.52),
        ev_solo_min_ask: env_f64("EV_SOLO_MIN_ASK", 0.44),
        ev_solo_qty: env_f64("EV_SOLO_QTY", 20.0),
        ev_solo_min_seconds_left: env_i64("EV_SOLO_MIN_SECONDS_LEFT", 240),
        ev_solo_flow_gate: env_bool("EV_SOLO_FLOW_GATE", true),
        poll_ms: env_u64("POLL_MS", 200),
        state_file: base.join(env("QUANT_STATE_FILE", "quant_state.json")),
        signal_file: base.join(env("QUANT_SIGNAL_FILE", "data/quant_signals.jsonl")),
        log_file: base.join(env("LOG_FILE", "logs/jy-bot.log")),
        book_record_enabled: env_bool("BOOK_RECORD_ENABLED", true),
        book_record_dir: base.join(env("BOOK_RECORD_DIR", "data/books")),
        book_legacy_log_enabled: env_bool("BOOK_LEGACY_LOG_ENABLED", false),
    })
}
