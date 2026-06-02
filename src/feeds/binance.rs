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
    /// 成交量(BTC 数量),aggTrade 的 q 字段。旧逻辑只用 price,此字段为量价信号新增。
    pub qty: f64,
    /// 是否主动卖单:aggTrade 的 m(买方是否为 maker)。true=主动卖(卖压),false=主动买(买压)。
    pub sell: bool,
}

/// 一段时间窗内的量价信号(买卖压力)。用于"前段量价分析"判断方向。
#[derive(Clone, Debug, Default)]
pub struct FlowSignal {
    pub buy_vol: f64,   // 主动买入量
    pub sell_vol: f64,  // 主动卖出量
    pub trades: usize,  // 成交笔数
    /// 买卖不平衡度 ∈[-1,1]:(买-卖)/(买+卖)。>0 买压占优(看涨),<0 卖压占优(看跌)。
    pub imbalance: f64,
}

#[derive(Clone)]
pub struct BinanceFeed {
    pub history: Arc<Mutex<VecDeque<TradePoint>>>,
    /// 新成交到达信号:每收到一笔 aggTrade 即 notify,供主循环事件驱动决策
    /// (z 信号由 Binance/Chainlink 驱动,只盯 Polymarket 盘口会漏掉 alpha 源的变动)。
    updated: Arc<tokio::sync::Notify>,
}

impl BinanceFeed {
    pub fn new() -> Self {
        Self {
            history: Arc::new(Mutex::new(VecDeque::new())),
            updated: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// 取得"新成交"通知句柄,主循环 await 它即可在 Binance 价更新时被唤醒。
    pub fn updated_handle(&self) -> Arc<tokio::sync::Notify> {
        self.updated.clone()
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

    /// 计算最近 window_sec 秒的量价买卖压力信号。用于量价方向判断。
    pub fn flow(&self, now: i64, window_sec: i64) -> FlowSignal {
        let cutoff = now - window_sec;
        let h = self.history.lock().unwrap();
        let mut s = FlowSignal::default();
        for p in h.iter().filter(|p| p.ts >= cutoff) {
            s.trades += 1;
            if p.sell { s.sell_vol += p.qty } else { s.buy_vol += p.qty }
        }
        let tot = s.buy_vol + s.sell_vol;
        s.imbalance = if tot > 0.0 { (s.buy_vol - s.sell_vol) / tot } else { 0.0 };
        s
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
        // 量价信号新增:成交量 q、买卖方向 m(true=买方maker=主动卖)。缺失则默认 0/买。
        let qty = payload.get("q").and_then(|x| x.as_str())
            .and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let sell = payload.get("m").and_then(|x| x.as_bool()).unwrap_or(false);

        let now = chrono::Utc::now().timestamp();
        let cutoff = now - HISTORY_SEC;

        {
            let mut h = self.history.lock().unwrap();
            h.push_back(TradePoint { ts, price, qty, sell });
            while h.front().map(|p| p.ts < cutoff).unwrap_or(false) {
                h.pop_front();
            }
        }
        // 唤醒主循环:Binance 价是 z 信号主要驱动源,价一动就让决策有机会立刻重算。
        // 高频 aggTrade 由主循环 min_gap(50ms) 节流兜底,Notify 只存一个 permit,多余通知自动合并。
        self.updated.notify_one();
    }
}
