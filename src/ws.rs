use crate::clob::{parse_book, BookCache, OrderBook};
use crate::recorder::Recorder;
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
    /// 盘口更新信号:每次收到 book/price_change 后 notify,主循环据此事件驱动决策(替代轮询)。
    book_updated: tokio::sync::Notify,
    /// 可选 tick 级盘口录制器（None=不采集）。
    recorder: Option<Recorder>,
}

/// WebSocket 盘口缓存，Arc 包装可随意 clone。
#[derive(Clone)]
pub struct MarketWs(Arc<Inner>);

impl MarketWs {
    pub fn new(url: &str, cache: BookCache, recorder: Option<Recorder>) -> Self {
        Self(Arc::new(Inner {
            url: url.to_string(),
            subscribed: Mutex::new(HashSet::new()),
            cache,
            reconnect: tokio::sync::Notify::new(),
            book_updated: tokio::sync::Notify::new(),
            recorder,
        }))
    }

    /// 等待下一次盘口更新(事件驱动)。主循环 await 此方法,盘口一变即被唤醒决策。
    pub async fn wait_book_update(&self) {
        self.0.book_updated.notified().await;
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

        // 收集本批更新到的 asset，写完 cache 后统一生成 tick 快照投递给采集器。
        let mut touched: Vec<String> = Vec::new();
        {
            let mut cache = self.0.cache.write().await;
            for event in &events {
                let ev_type = event.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
                if ev_type == "book" {
                    if let Some(asset_id) = event.get("asset_id").and_then(|v| v.as_str()) {
                        cache.insert(asset_id.to_string(), parse_book(event));
                        touched.push(asset_id.to_string());
                    }
                } else if ev_type == "price_change" {
                    // 增量更新：用当前 asks/bids 字段覆盖缓存中对应层级
                    if let Some(asset_id) = event.get("asset_id").and_then(|v| v.as_str()) {
                        if let Some(book) = cache.get_mut(asset_id) {
                            apply_price_change(book, event);
                            touched.push(asset_id.to_string());
                        }
                    }
                }
            }
            // tick 采集：在持锁期间从最新 book 生成快照行（避免再加锁）。
            if let Some(rec) = &self.0.recorder {
                let ts_ms = chrono::Utc::now().timestamp_millis();
                for asset_id in &touched {
                    if let Some(book) = cache.get(asset_id) {
                        rec.record(book_snapshot_line(asset_id, ts_ms, book));
                    }
                }
            }
        }
        // 事件驱动:本批有盘口更新 → 唤醒主循环立即决策(替代轮询滞后)。
        if !touched.is_empty() {
            self.0.book_updated.notify_one();
        }
    }
}

/// 把一档盘口压成一行 JSON：top-of-book 价/量 + 两侧深度汇总（档数与总量）。
/// 足以离线重建顶档与流动性，体积可控。
fn book_snapshot_line(asset_id: &str, ts_ms: i64, book: &OrderBook) -> String {
    let to_f = |d: rust_decimal::Decimal| d.to_string().parse::<f64>().unwrap_or(0.0);
    let bb = book.bids.first().map(|(p, _)| to_f(*p)).unwrap_or(0.0);
    let bb_sz = book.bids.first().map(|(_, s)| to_f(*s)).unwrap_or(0.0);
    let ba = book.asks.first().map(|(p, _)| to_f(*p)).unwrap_or(0.0);
    let ba_sz = book.asks.first().map(|(_, s)| to_f(*s)).unwrap_or(0.0);
    let bid_depth: f64 = book.bids.iter().map(|(_, s)| to_f(*s)).sum();
    let ask_depth: f64 = book.asks.iter().map(|(_, s)| to_f(*s)).sum();
    json!({
        "ts_ms": ts_ms,
        "asset": asset_id,
        "bb": bb, "bb_sz": bb_sz,
        "ba": ba, "ba_sz": ba_sz,
        "bids_n": book.bids.len(), "asks_n": book.asks.len(),
        "bid_depth": bid_depth, "ask_depth": ask_depth,
    }).to_string()
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
