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
        clob_v2_api_url: env("CLOB_V2_API_URL", "https://clob-v2.polymarket.com"),
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
        poll_ms: env_u64("POLL_MS", 1000),
        state_file: base.join(env("QUANT_STATE_FILE", "quant_state.json")),
        signal_file: base.join(env("QUANT_SIGNAL_FILE", "data/quant_signals.jsonl")),
        log_file: base.join(env("LOG_FILE", "logs/jy-bot.log")),
    })
}
