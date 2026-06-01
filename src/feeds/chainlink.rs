use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

const RTDS_URL: &str = "wss://ws-live-data.polymarket.com";
/// 历史保留时长。必须 > 盘口时长(300s)+富余,否则盘后半段取不到真开盘价。
/// 原 180s 是 bug:过 180s 后 chainlink_at(start_ts) 找不到开盘价会取最旧旧价→z方向变形。
const HISTORY_SEC: i64 = 420;
/// at_ts 容差:目标时间最近的样本若偏离超过此秒数,视为"没有该时刻价格",返回 None。
/// 防止拿一个时间差很大的旧价格凑数(那会让 price_to_beat 失真)。
const AT_TS_TOLERANCE_SEC: i64 = 5;

#[derive(Clone, Debug)]
pub struct PricePoint {
    pub ts: i64,
    pub price: f64,
}

#[derive(Clone)]
pub struct ChainlinkFeed {
    pub history: Arc<Mutex<VecDeque<PricePoint>>>,
}

impl ChainlinkFeed {
    pub fn new() -> Self {
        Self { history: Arc::new(Mutex::new(VecDeque::new())) }
    }

    /// 获取最新价格（当前 Chainlink BTC/USD）
    pub fn latest(&self) -> Option<PricePoint> {
        self.history.lock().unwrap().back().cloned()
    }

    /// 获取指定时间戳附近的价格。最近样本偏离超过 AT_TS_TOLERANCE_SEC 则返回 None
    /// (宁可不入场,也不拿错误的旧价当开盘价 → 避免 z-score 方向变形)。
    pub fn at_ts(&self, target_ts: i64) -> Option<f64> {
        let h = self.history.lock().unwrap();
        let p = h.iter().min_by_key(|p| (p.ts - target_ts).abs())?;
        if (p.ts - target_ts).abs() <= AT_TS_TOLERANCE_SEC {
            Some(p.price)
        } else {
            None
        }
    }

    /// 获取全部历史（用于 σ 计算）
    pub fn snapshot(&self) -> Vec<PricePoint> {
        self.history.lock().unwrap().iter().cloned().collect()
    }

    pub fn run(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.connect_once().await {
                    Ok(()) => info!("[Chainlink] connection closed, reconnecting..."),
                    Err(e) => warn!("[Chainlink] error: {e}, reconnecting in 3s..."),
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        })
    }

    async fn connect_once(&self) -> anyhow::Result<()> {
        let (ws, _) = connect_async(RTDS_URL).await?;
        info!("[Chainlink] connected to RTDS");
        let (mut write, mut read) = ws.split();

        // 订阅 Chainlink btc/usd
        let sub = json!({
            "action": "subscribe",
            "subscriptions": [{
                "topic": "crypto_prices_chainlink",
                "type": "*",
                "filters": "{\"symbol\":\"btc/usd\"}"
            }]
        });
        write.send(Message::Text(sub.to_string().into())).await?;

        // 主循环 + Ping 心跳
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(t))) => self.handle(&t),
                        Some(Ok(Message::Ping(d))) => { write.send(Message::Pong(d)).await.ok(); }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(e)) => return Err(e.into()),
                        _ => {}
                    }
                }
                _ = interval.tick() => {
                    write.send(Message::Text("PING".into())).await.ok();
                }
            }
        }
        Ok(())
    }

    fn handle(&self, text: &str) {
        if text == "PONG" { return; }
        let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(text) else { return };

        let Some(payload) = v.get("payload").and_then(|p| p.as_object()) else { return };
        let now = chrono::Utc::now().timestamp();
        let cutoff = now - HISTORY_SEC;

        let rows_val: Vec<serde_json::Value> = payload.get("data")
            .and_then(|d| d.as_array().cloned())
            .unwrap_or_else(|| vec![serde_json::Value::Object(payload.clone())]);

        let mut h = self.history.lock().unwrap();
        for row in &rows_val {
            let ts_raw = match row.get("timestamp").or_else(|| row.get("ts"))
                .and_then(|v| v.as_f64()) {
                Some(t) => t, None => continue,
            };
            let price = match row.get("value").or_else(|| row.get("price"))
                .and_then(|v| v.as_f64()) {
                Some(p) => p, None => continue,
            };
            let ts = if ts_raw > 1e10 { (ts_raw / 1000.0) as i64 } else { ts_raw as i64 };
            h.push_back(PricePoint { ts, price });
        }

        while h.front().map(|p| p.ts < cutoff).unwrap_or(false) { h.pop_front(); }
        let mut v: Vec<_> = h.drain(..).collect();
        v.sort_by_key(|p| p.ts);
        v.dedup_by_key(|p| p.ts);
        h.extend(v);
    }
}

