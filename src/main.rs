mod clob;
mod config;
mod feeds;
mod position;
mod state;
mod strategy;
mod ws;
mod zscore;

use anyhow::Result;
use clob::new_book_cache;
use feeds::{BinanceFeed, ChainlinkFeed};
use strategy::smart::SmartStrategy;
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

    info!(
        "[JY-BOT] strategy=smart dry_run={} shares={}",
        config.dry_run, config.order_shares
    );
    if config.dry_run {
        info!("[MODE] DRY_RUN=1，只打印，不真实下单。");
    } else {
        info!("[MODE] DRY_RUN=0，真实下单！");
    }

    let cache = new_book_cache();

    // 价格数据源
    let chainlink = ChainlinkFeed::new();
    let binance   = BinanceFeed::new();

    // 启动后台数据流
    let _cl_handle = chainlink.clone().run();
    let _bn_handle = binance.clone().run();

    // WebSocket 盘口缓存
    let ws = MarketWs::new(&config.market_ws_url, cache.clone());
    let _ws_handle = ws.run();

    // 初始化策略
    let mut strategy = SmartStrategy::new(
        config.clone(), cache, chainlink, binance, ws.clone()
    ).await?;

    let poll = tokio::time::Duration::from_millis(config.poll_ms);
    info!("[JY-BOT] 开始轮询，间隔 {}ms", config.poll_ms);

    loop {
        if let Err(e) = strategy.run_once().await {
            error!("[JY-BOT ERROR] {e}");
        }
        tokio::time::sleep(poll).await;
    }
}
