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
    pub bot_mode: String,

    // copy 模式
    pub target_wallet: String,
    pub copy_ratio: Decimal,
    pub price_mode: String,
    pub max_slippage: Decimal,

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

    // 系统
    pub poll_ms: u64,
    pub state_file: PathBuf,
    pub signal_file: PathBuf,
    pub log_file: PathBuf,
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
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
    if let Some(p) = env_path {
        dotenvy::from_path(p).ok();
    } else {
        dotenvy::dotenv().ok();
    }

    let private_key = match std::env::var("PRIVATE_KEY") {
        Ok(k) if !k.trim().is_empty() => Some(k.trim().to_string()),
        _ => None,
    };
    let deposit_wallet = match std::env::var("DEPOSIT_WALLET_ADDRESS") {
        Ok(w) if !w.trim().is_empty() => Some(w.trim().to_string()),
        _ => None,
    };
    let dry_run = env_bool("DRY_RUN", true);
    // 模式：copy=跟单，其余=量化(smart)
    let bot_mode = match env("BOT_MODE", &env("QUANT_STRATEGY", "quant")).to_lowercase().as_str() {
        "copy" => "copy".to_string(),
        _ => "quant".to_string(),
    };
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
        env_path
            .and_then(|p| std::path::Path::new(p).parent().map(|p| p.to_str().unwrap_or(".").to_string()))
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
        bot_mode,
        target_wallet: env("TARGET_WALLET", "0xe0229e10a858860218b6132f4234602c47bd6603"),
        copy_ratio: env_decimal("COPY_RATIO", "1.0"),
        price_mode: env("PRICE_MODE", "safe"),
        max_slippage: env_decimal("MAX_SLIPPAGE", "0.02"),
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
        poll_ms: env_u64("POLL_MS", 1000),
        state_file: base.join(env("QUANT_STATE_FILE", "quant_state.json")),
        signal_file: base.join(env("QUANT_SIGNAL_FILE", "data/quant_signals.jsonl")),
        log_file: base.join(env("LOG_FILE", "logs/jy-bot.log")),
    })
}
