use crate::clob::{parse_book, BookCache};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

pub struct MarketWs {
    url: String,
    subscribed: Arc<Mutex<HashSet<String>>>,
    cache: BookCache,
}

impl MarketWs {
    pub fn new(url: &str, cache: BookCache) -> Self {
        Self {
            url: url.to_string(),
            subscribed: Arc::new(Mutex::new(HashSet::new())),
            cache,
        }
    }

    pub async fn subscribe(&self, token_ids: &[String]) {
        let mut sub = self.subscribed.lock().await;
        for id in token_ids {
            sub.insert(id.clone());
        }
    }

    pub fn run(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.connect_once().await {
                    Ok(()) => info!("[WS] connection closed, reconnecting..."),
                    Err(e) => warn!("[WS] error: {e}, reconnecting in 3s..."),
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        })
    }

    async fn connect_once(&self) -> anyhow::Result<()> {
        let (ws_stream, _) = connect_async(&self.url).await?;
        info!("[WS] connected to {}", self.url);
        let (mut write, mut read) = ws_stream.split();

        // subscribe to currently known assets
        let assets: Vec<String> = self.subscribed.lock().await.iter().cloned().collect();
        if !assets.is_empty() {
            self.send_subscribe(&mut write, &assets).await?;
        }

        while let Some(msg) = read.next().await {
            let msg = msg?;
            match msg {
                Message::Text(text) => {
                    self.handle_message(&text, &mut write).await;
                }
                Message::Ping(data) => {
                    write.send(Message::Pong(data)).await.ok();
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        Ok(())
    }

    async fn send_subscribe(
        &self,
        write: &mut (impl SinkExt<Message, Error = impl std::fmt::Debug> + Unpin),
        assets: &[String],
    ) -> anyhow::Result<()> {
        let msg = json!({
            "assets_ids": assets,
            "type": "market"
        });
        write
            .send(Message::Text(msg.to_string().into()))
            .await
            .map_err(|e| anyhow::anyhow!("ws send error: {:?}", e))?;
        info!("[WS] subscribed {} assets", assets.len());
        Ok(())
    }

    async fn handle_message(
        &self,
        text: &str,
        _write: &mut (impl SinkExt<Message, Error = impl std::fmt::Debug> + Unpin),
    ) {
        let Ok(data): Result<serde_json::Value, _> = serde_json::from_str(text) else {
            return;
        };

        // Polymarket WS sends array of events
        let events = match &data {
            serde_json::Value::Array(arr) => arr.clone(),
            obj @ serde_json::Value::Object(_) => vec![obj.clone()],
            _ => return,
        };

        let mut cache = self.cache.write().await;
        for event in &events {
            let event_type = event.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
            if event_type == "book" {
                if let Some(asset_id) = event.get("asset_id").and_then(|v| v.as_str()) {
                    let book = parse_book(event);
                    cache.insert(asset_id.to_string(), book);
                }
            } else if event_type == "price_change" {
                // partial update - refresh from HTTP later
            }
        }
    }

    pub async fn ensure_subscribed(&self, token_ids: &[String]) {
        self.subscribe(token_ids).await;
    }
}
