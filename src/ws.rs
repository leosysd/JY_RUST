use crate::clob::{parse_book, BookCache};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

struct Inner {
    url: String,
    subscribed: Mutex<HashSet<String>>,
    cache: BookCache,
    reconnect: tokio::sync::Notify,
}

/// WebSocket 盘口缓存，Arc 包装可随意 clone。
#[derive(Clone)]
pub struct MarketWs(Arc<Inner>);

impl MarketWs {
    pub fn new(url: &str, cache: BookCache) -> Self {
        Self(Arc::new(Inner {
            url: url.to_string(),
            subscribed: Mutex::new(HashSet::new()),
            cache,
            reconnect: tokio::sync::Notify::new(),
        }))
    }

    /// 订阅新 token，若有新增则触发重连（重连后服务端会推完整快照）。
    pub async fn ensure_subscribed(&self, token_ids: &[String]) {
        let mut sub = self.0.subscribed.lock().await;
        let before = sub.len();
        for id in token_ids {
            sub.insert(id.clone());
        }
        if sub.len() > before {
            drop(sub);
            self.0.reconnect.notify_one();
        }
    }

    pub fn run(&self) -> tokio::task::JoinHandle<()> {
        let ws = self.clone();
        tokio::spawn(async move {
            loop {
                match ws.connect_once().await {
                    Ok(true)  => info!("[WS] 新token触发重连"),
                    Ok(false) => {
                        info!("[WS] 连接关闭，3s后重连");
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                    }
                    Err(e) => {
                        warn!("[WS] 错误: {e}，3s后重连");
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                    }
                }
            }
        })
    }

    /// 返回 true = 主动重连（新token），false = 对端关闭。
    async fn connect_once(&self) -> anyhow::Result<bool> {
        let (ws_stream, _) = connect_async(&self.0.url).await?;
        info!("[WS] connected to {}", self.0.url);
        let (mut write, mut read) = ws_stream.split();

        let assets: Vec<String> = self.0.subscribed.lock().await.iter().cloned().collect();
        if !assets.is_empty() {
            self.send_subscribe(&mut write, &assets).await?;
        }

        loop {
            tokio::select! {
                _ = self.0.reconnect.notified() => {
                    return Ok(true);
                }
                msg_opt = read.next() => {
                    let Some(msg) = msg_opt else { return Ok(false); };
                    match msg? {
                        Message::Text(text) => self.handle_message(&text).await,
                        Message::Ping(data) => { write.send(Message::Pong(data)).await.ok(); }
                        Message::Close(_)   => return Ok(false),
                        _ => {}
                    }
                }
            }
        }
    }

    async fn send_subscribe(
        &self,
        write: &mut (impl SinkExt<Message, Error = impl std::fmt::Debug> + Unpin),
        assets: &[String],
    ) -> anyhow::Result<()> {
        let msg = json!({ "assets_ids": assets, "type": "market" });
        write.send(Message::Text(msg.to_string().into()))
            .await
            .map_err(|e| anyhow::anyhow!("ws send: {:?}", e))?;
        info!("[WS] subscribed {} tokens", assets.len());
        Ok(())
    }

    async fn handle_message(&self, text: &str) {
        let Ok(data): Result<serde_json::Value, _> = serde_json::from_str(text) else { return };

        let events = match &data {
            serde_json::Value::Array(arr) => arr.clone(),
            obj @ serde_json::Value::Object(_) => vec![obj.clone()],
            _ => return,
        };

        let mut cache = self.0.cache.write().await;
        for event in &events {
            let ev_type = event.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
            if ev_type == "book" {
                if let Some(asset_id) = event.get("asset_id").and_then(|v| v.as_str()) {
                    cache.insert(asset_id.to_string(), parse_book(event));
                }
            } else if ev_type == "price_change" {
                // 增量更新：用当前 asks/bids 字段覆盖缓存中对应层级
                if let Some(asset_id) = event.get("asset_id").and_then(|v| v.as_str()) {
                    if let Some(book) = cache.get_mut(asset_id) {
                        apply_price_change(book, event);
                    }
                }
            }
        }
    }
}

fn apply_price_change(book: &mut crate::clob::OrderBook, event: &serde_json::Value) {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let parse_levels = |arr: &serde_json::Value| -> Vec<(Decimal, Decimal)> {
        arr.as_array().map(|a| {
            a.iter().filter_map(|l| {
                let p = Decimal::from_str(l.get("price")?.as_str()?).ok()?;
                let s = Decimal::from_str(l.get("size")?.as_str()?).ok()?;
                Some((p, s))
            }).collect()
        }).unwrap_or_default()
    };

    if let Some(asks) = event.get("asks") {
        let updates = parse_levels(asks);
        for (price, size) in updates {
            book.asks.retain(|(p, _)| *p != price);
            if size > Decimal::ZERO {
                book.asks.push((price, size));
            }
        }
        book.asks.sort_by(|a, b| a.0.cmp(&b.0));
    }

    if let Some(bids) = event.get("bids") {
        let updates = parse_levels(bids);
        for (price, size) in updates {
            book.bids.retain(|(p, _)| *p != price);
            if size > Decimal::ZERO {
                book.bids.push((price, size));
            }
        }
        book.bids.sort_by(|a, b| b.0.cmp(&a.0));
    }
}
