use crate::clob::{ClobClient, OrderBook};
use crate::config::Config;
use crate::executor::OrderExecutor;
use anyhow::Result;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{info, warn};

const DATA_API: &str = "https://data-api.polymarket.com";

#[derive(Debug, Deserialize)]
struct Activity {
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,
    #[serde(rename = "transactionHash")]
    tx_hash: Option<String>,
    #[serde(rename = "type")]
    activity_type: Option<String>,
    size: Option<serde_json::Value>,
    #[serde(rename = "usdcSize")]
    usdc_size: Option<f64>,
    asset: Option<String>,
    side: Option<String>,
    price: Option<serde_json::Value>,
    outcome: Option<String>,
    title: Option<String>,
}

pub struct CopyStrategy {
    config: Config,
    client: ClobClient,
    seen: HashSet<String>,
    executor: Arc<OrderExecutor>,
}

impl CopyStrategy {
    pub fn new(config: Config, executor: Arc<OrderExecutor>) -> Self {
        let client = ClobClient::new(
            &config.clob_api_url,
            &config.gamma_api_url,
            &config.market_slug_prefix,
        );
        Self { config, client, seen: HashSet::new(), executor }
    }

    pub async fn bootstrap(&mut self) -> Result<()> {
        let activities = self.fetch_activity(200).await?;
        for a in &activities {
            if let Some(hash) = &a.tx_hash {
                self.seen.insert(hash.clone());
            }
        }
        info!("[COPY] 启动已忽略 {} 笔历史交易", self.seen.len());
        Ok(())
    }

    pub async fn run_once(&mut self) -> Result<()> {
        let activities = self.fetch_activity(50).await?;
        let mut new_trades: Vec<&Activity> = vec![];

        for a in &activities {
            let hash = match &a.tx_hash {
                Some(h) => h.clone(),
                None => continue,
            };
            if self.seen.contains(&hash) {
                continue;
            }
            if a.activity_type.as_deref() != Some("TRADE") {
                self.seen.insert(hash);
                continue;
            }
            new_trades.push(a);
        }

        if new_trades.is_empty() {
            return Ok(());
        }

        // 按时间正序处理（API 返回最新在前，倒序遍历）
        new_trades.reverse();

        for trade in new_trades {
            if let Err(e) = self.process_trade(trade).await {
                warn!("[COPY] 处理交易失败: {e}");
            }
            if let Some(hash) = &trade.tx_hash {
                self.seen.insert(hash.clone());
            }
        }

        Ok(())
    }

    async fn process_trade(&self, trade: &Activity) -> Result<()> {
        let token_id = match &trade.asset {
            Some(id) => id.clone(),
            None => return Ok(()),
        };
        let side = trade.side.as_deref().unwrap_or("BUY").to_uppercase();
        let target_price = parse_decimal(&trade.price).unwrap_or_default();
        let target_size = parse_decimal(&trade.size).unwrap_or_default();
        let title = trade.title.as_deref().unwrap_or("?");
        let outcome = trade.outcome.as_deref().unwrap_or("?");

        if target_size.is_zero() || target_price.is_zero() {
            return Ok(());
        }
        // 目前只镜像 BUY（建仓）；SELL（平仓）暂不跟，避免误用 buy 执行器
        if side != "BUY" {
            info!("[COPY SKIP] {title} | {outcome} | {side}（暂不跟卖出）");
            return Ok(());
        }

        let copy_size = (target_size * self.config.copy_ratio)
            .round_dp(4);

        if copy_size < Decimal::new(1, 0) {
            info!("[COPY SKIP] size 太小: copy_size={copy_size}");
            return Ok(());
        }

        // 获取盘口确认流动性
        let book = match self.client.fetch_book(&token_id).await {
            Ok(b) => b,
            Err(e) => {
                warn!("[COPY] 获取盘口失败 {token_id}: {e}");
                return Ok(());
            }
        };

        let copy_price = self.choose_price(&book, &side, target_price)?;
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };

        info!(
            "[COPY {mode}] {title} | {outcome} | {side} {copy_size}份 @ {copy_price} \
(原始: {target_size}份 @ {target_price} ratio={})",
            self.config.copy_ratio
        );

        let price_f64 = copy_price.to_string().parse::<f64>().unwrap_or(0.0);
        let shares_f64 = copy_size.to_string().parse::<f64>().unwrap_or(0.0);
        match self.executor.buy(&token_id, price_f64, shares_f64).await {
            Ok(fill) if fill.simulated => {} // DRY_RUN：已在上方日志体现
            Ok(fill) => info!(
                "[COPY ORDER] {title} id={} status={} ok={}",
                fill.order_id, fill.status, fill.success
            ),
            Err(e) => warn!("[COPY ORDER ERR] {title}: {e}"),
        }

        Ok(())
    }

    fn choose_price(
        &self,
        book: &OrderBook,
        side: &str,
        target_price: Decimal,
    ) -> Result<Decimal> {
        if self.config.price_mode == "aggressive" {
            return Ok(if side == "BUY" {
                Decimal::new(99, 2)
            } else {
                Decimal::new(1, 2)
            });
        }
        // safe 模式：用市场最优价 + 滑点容忍
        let market_price = if side == "BUY" {
            book.best_ask().unwrap_or(target_price)
        } else {
            book.bids.first().map(|(p, _)| *p).unwrap_or(target_price)
        };
        let max_price = (market_price + self.config.max_slippage)
            .min(Decimal::new(99, 2));
        Ok(max_price)
    }

    async fn fetch_activity(&self, limit: usize) -> Result<Vec<Activity>> {
        let url = format!(
            "{}/activity?user={}&limit={}",
            DATA_API, self.config.target_wallet, limit
        );
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let resp: Vec<Activity> = http
            .get(&url)
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }
}

fn parse_decimal(v: &Option<serde_json::Value>) -> Option<Decimal> {
    match v {
        Some(serde_json::Value::Number(n)) => {
            Decimal::from_str(&n.to_string()).ok()
        }
        Some(serde_json::Value::String(s)) => Decimal::from_str(s).ok(),
        _ => None,
    }
}
