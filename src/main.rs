mod clob;
mod config;
mod executor;
mod feeds;
mod position;
mod recorder;
mod state;
mod strategy;
mod ws;
mod zscore;

use anyhow::Result;
use clob::new_book_cache;
use executor::OrderExecutor;
use feeds::{BinanceFeed, ChainlinkFeed};
use std::sync::Arc;
use strategy::copy::CopyStrategy;
use strategy::smart::SmartStrategy;
use tracing::{error, info};
use ws::MarketWs;

#[tokio::main]
async fn main() -> Result<()> {
    // 依赖树同时含 ring 与 aws-lc-rs，显式选定 ring 作为 rustls 后端，否则 TLS 握手会 panic
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    let env_path = std::env::args().nth(1);
    let config = config::load(env_path.as_deref())?;

    info!(
        "[JY-BOT] mode={} dry_run={} shares={}",
        config.bot_mode, config.dry_run, config.order_shares
    );
    if config.dry_run {
        info!("[MODE] DRY_RUN=1，模拟，不真实下单。");
    } else {
        info!("[MODE] DRY_RUN=0，真实下单！");
    }

    // 统一下单执行器（LIVE 模式在此完成认证/派生 API creds，失败即退出）
    let exec = Arc::new(OrderExecutor::new(&config).await?);

    let poll = tokio::time::Duration::from_millis(config.poll_ms);
    info!("[JY-BOT] 开始轮询，间隔 {}ms", config.poll_ms);

    match config.bot_mode.as_str() {
        "copy" => run_copy(config, exec, poll).await,
        _      => run_quant(config, exec, poll).await,
    }
}

/// 量化模式：BTC 5m 趋势追单 + 锁利
async fn run_quant(
    config: config::Config,
    exec: Arc<OrderExecutor>,
    poll: tokio::time::Duration,
) -> Result<()> {
    let cache = new_book_cache();

    let chainlink = ChainlinkFeed::new();
    let binance = BinanceFeed::new();
    let _cl = chainlink.clone().run();
    let _bn = binance.clone().run();

    // 全盘口 tick 采集器：与交易无关，一启动即录制每个订阅盘口（含不交易时段）。
    let recorder = if config.book_record_enabled {
        Some(recorder::Recorder::spawn(config.book_record_dir.clone()))
    } else {
        None
    };
    let ws = MarketWs::new(&config.market_ws_url, cache.clone(), recorder);
    let _ws = ws.run();

    let mut strategy =
        SmartStrategy::new(config, cache, chainlink, binance, ws, exec).await?;

    loop {
        if let Err(e) = strategy.run_once().await {
            error!("[JY-BOT ERROR] {e}");
        }
        tokio::time::sleep(poll).await;
    }
}

/// 跟单模式：镜像目标钱包的成交
async fn run_copy(
    config: config::Config,
    exec: Arc<OrderExecutor>,
    poll: tokio::time::Duration,
) -> Result<()> {
    let mut strategy = CopyStrategy::new(config, exec);
    strategy.bootstrap().await?;

    loop {
        if let Err(e) = strategy.run_once().await {
            error!("[JY-BOT ERROR] {e}");
        }
        tokio::time::sleep(poll).await;
    }
}
