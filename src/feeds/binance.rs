use futures_util::StreamExt;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

const BINANCE_WS: &str = "wss://data-stream.binance.vision/ws/btcusdt@aggTrade";
const HISTORY_SEC: i64 = 180;

#[derive(Clone, Debug)]
pub struct TradePoint {
    pub ts: i64,
    pub price: f64,
}

#[derive(Clone)]
pub struct BinanceFeed {
    pub history: Arc<Mutex<VecDeque<TradePoint>>>,
}

impl BinanceFeed {
    pub fn new() -> Self {
        Self { history: Arc::new(Mutex::new(VecDeque::new())) }
    }

    pub fn latest(&self) -> Option<TradePoint> {
        self.history.lock().unwrap().back().cloned()
    }

    /// 获取指定时间戳前后最近价格
    pub fn at_ts(&self, target_ts: i64) -> Option<f64> {
        let h = self.history.lock().unwrap();
        h.iter()
            .min_by_key(|p| (p.ts - target_ts).abs())
            .map(|p| p.price)
    }

    /// 获取全部历史
    pub fn snapshot(&self) -> Vec<TradePoint> {
        self.history.lock().unwrap().iter().cloned().collect()
    }

    pub fn run(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.connect_once().await {
                    Ok(()) => info!("[Binance] connection closed, reconnecting..."),
                    Err(e) => warn!("[Binance] error: {e}, reconnecting in 3s..."),
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        })
    }

    async fn connect_once(&self) -> anyhow::Result<()> {
        let (ws, _) = connect_async(BINANCE_WS).await?;
        info!("[Binance] connected aggTrade stream");
        let (_, mut read) = ws.split();

        while let Some(msg) = read.next().await {
            match msg? {
                Message::Text(t) => self.handle(&t),
                Message::Close(_) => break,
                _ => {}
            }
        }
        Ok(())
    }

    fn handle(&self, text: &str) {
        let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(text) else { return };
        let payload = v.get("data").unwrap_or(&v);
        let Some(price) = payload.get("p").and_then(|x| x.as_str())
            .and_then(|s| s.parse::<f64>().ok()) else { return };
        let Some(ts_ms) = payload.get("T").and_then(|x| x.as_f64()) else { return };
        let ts = (ts_ms / 1000.0) as i64;

        let now = chrono::Utc::now().timestamp();
        let cutoff = now - HISTORY_SEC;

        let mut h = self.history.lock().unwrap();
        h.push_back(TradePoint { ts, price });
        while h.front().map(|p| p.ts < cutoff).unwrap_or(false) {
            h.pop_front();
        }
    }
}
