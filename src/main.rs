mod clob;
mod config;
mod signing;
mod state;
mod strategy;
mod ws;

use anyhow::Result;
use clob::new_book_cache;
use strategy::{copy::CopyStrategy, jetfadil::JetFadilStrategy};
use tracing::{error, info};
use ws::MarketWs;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    let env_path = std::env::args().nth(1);
    let config = config::load(env_path.as_deref())?;
    let mode = &config.bot_mode;

    info!(
        "[JY-BOT] mode={mode} dry_run={} strategy={}",
        config.dry_run, config.bot_mode
    );
    if config.dry_run {
        info!("[MODE] DRY_RUN=1，只打印，不真实下单。");
    } else {
        info!("[MODE] DRY_RUN=0，真实下单！");
    }

    let cache = new_book_cache();
    let ws = MarketWs::new(&config.market_ws_url, cache.clone());
    let _ws_handle = ws.run();

    let poll = tokio::time::Duration::from_millis(config.poll_ms);

    match mode.as_str() {
        "copy" => {
            info!("[JY-BOT] 跟单模式: target={}", config.target_wallet);
            let mut strategy = CopyStrategy::new(config, cache);
            strategy.bootstrap().await?;
            info!("[JY-BOT] 开始轮询，间隔 {}ms", poll.as_millis());
            loop {
                if let Err(e) = strategy.run_once().await {
                    error!("[COPY ERROR] {e}");
                }
                tokio::time::sleep(poll).await;
            }
        }
        _ => {
            // jetfadil / arb / combo - 默认 JetFadil
            info!(
                "[JY-BOT] JetFadil 策略: shares={} min_lock_profit={} max_entry_delay={}s",
                config.order_shares, config.min_lock_profit, config.max_entry_delay_sec
            );
            let mut strategy = JetFadilStrategy::new(config.clone(), cache).await?;
            info!("[JY-BOT] 开始轮询，间隔 {}ms", poll.as_millis());
            loop {
                if let Err(e) = strategy.run_once().await {
                    error!("[JY-BOT ERROR] {e}");
                }
                tokio::time::sleep(poll).await;
            }
        }
    }
}
