use crate::feeds::{BinanceFeed, ChainlinkFeed};
use crate::feeds::binance::TradePoint;
use crate::feeds::chainlink::PricePoint;
use crate::momentum;

/// z 信号动量项：短/长窗口（秒）与权重。原内联写死的 2s/5s + 0.6/0.3，现抽成具名常量。
/// 窗口/权重将来要可调的话，把这四个常量改成读 config 即可（动量计算本身已在 momentum 模块）。
const MOM_WIN_SHORT: i64 = 2;
const MOM_WIN_LONG: i64 = 5;
const MOM_W_SHORT: f64 = 0.60;
const MOM_W_LONG: f64 = 0.30;

/// z-score 方向信号
#[derive(Debug, Clone)]
pub struct ZSignal {
    pub z: f64,
    pub p_up: f64,
    pub p_down: f64,
    pub e: f64,          // 预期偏移
    pub v: f64,          // 噪声尺度
    pub ct: f64,         // 当前 Chainlink
    pub xt: f64,         // 当前 Binance
    pub b: f64,          // Price to Beat (开盘 Chainlink)
    pub sigma120: f64,
    pub basis60: f64,
    pub seconds_left: i64,
}

impl ZSignal {
    /// z ≥ 0.15 买 Up，z ≤ -0.15 买 Down，|z| < 0.15 不入场
    pub fn direction(&self) -> Option<&'static str> {
        if self.z >= 0.15 { Some("Up") }
        else if self.z <= -0.15 { Some("Down") }
        else { None }
    }
}

pub struct ZScoreModel {
    chainlink: ChainlinkFeed,
    binance: BinanceFeed,
}

impl ZScoreModel {
    pub fn new(chainlink: ChainlinkFeed, binance: BinanceFeed) -> Self {
        Self { chainlink, binance }
    }

    /// 获取指定时间戳附近的 Chainlink 价格
    pub fn chainlink_at(&self, ts: i64) -> Option<f64> {
        self.chainlink.at_ts(ts)
    }

    /// 获取最新 Chainlink 价格
    pub fn chainlink_latest(&self) -> Option<f64> {
        self.chainlink.latest().map(|p| p.price)
    }

    /// 获取 Binance 在指定时间戳附近的价格(狙击用:取本盘开盘价)。
    pub fn binance_at(&self, ts: i64) -> Option<f64> {
        self.binance.at_ts(ts)
    }

    /// 获取 Binance 最新成交价(狙击用:当前价)。
    pub fn binance_latest(&self) -> Option<f64> {
        self.binance.latest().map(|p| p.price)
    }

    /// 暴露 Binance 量价买卖压力信号(用于量价方向验证)。
    pub fn binance_flow(&self, now: i64, window_sec: i64) -> crate::feeds::FlowSignal {
        self.binance.flow(now, window_sec)
    }

    /// Binance 价格动量 = 现价 − window_sec 秒前价。>0 涨势。各策略复用的动量入口。
    /// 与 `momentum::momentum_on` 同口径（取时间最近点）；这里走 latest()+at_ts() 直接访问
    /// feed（两次廉价锁、**不整段克隆**），适合策略独立调用——无需先 snapshot。
    pub fn binance_momentum(&self, now: i64, window_sec: i64) -> Option<f64> {
        let cur = self.binance.latest()?.price;
        let past = self.binance.at_ts(now - window_sec)?;
        Some(cur - past)
    }

    /// 计算当前 z-score 信号
    /// price_to_beat: 本轮开盘时的 Chainlink 价格 (B)
    /// seconds_left: 距结算剩余秒数
    pub fn compute(&self, price_to_beat: f64, seconds_left: i64) -> Option<ZSignal> {
        let now = chrono::Utc::now().timestamp();

        // 一次性取两路历史快照,后续所有派生量(latest/at_ts/basis60/sigma120)都复用这两个本地数组,
        // 避免对同一历史在一次决策里反复加锁 + 整段 clone(原实现 clone 多达 3 次)。
        let cl_snap = self.chainlink.snapshot();
        let bn_snap = self.binance.snapshot();

        let cl_latest = cl_snap.last()?;
        let bn_latest = bn_snap.last()?;

        // 检查数据是否足够新鲜（最多5秒延迟）
        if (now - cl_latest.ts).abs() > 5 || (now - bn_latest.ts).abs() > 5 {
            return None;
        }

        let ct = cl_latest.price;
        let xt = bn_latest.price;

        // basis60：最近60秒 Binance - Chainlink 平均差（用于把币安价换算到 Chainlink 口径）
        let basis60 = compute_basis60(now, &cl_snap, &bn_snap);

        // σ120：最近120秒每秒价格变化的标准差
        let sigma120 = compute_sigma120(now, &bn_snap);

        // 动量项：调用独立的 momentum 模块，吃本函数已取好的 bn_snap 快照
        // → 零额外加锁/克隆。取不到（切片太短）回退 0，等价于原来的 xt−xt=0。
        let mom_short = momentum::momentum_on(&bn_snap, now, MOM_WIN_SHORT).unwrap_or(0.0);
        let mom_long  = momentum::momentum_on(&bn_snap, now, MOM_WIN_LONG).unwrap_or(0.0);

        // 预期偏移 E —— 方向信号全部改由「币安」驱动(tick级、实时,比 Chainlink 灵敏)。
        // 开盘基准价 B(price_to_beat)仍用 Chainlink,保证与市场结算口径对齐。
        // xt_adj = 币安现价经基差校正到 Chainlink 口径(basis60≈xt−ct),
        // 这样"相对开盘价的漂移"用币安现价算,又不会和 Chainlink 开盘价串了口径。
        // 原 Chainlink 动量项(ct−ct_2)与单独的基差回归项已并入此处,不再单列;
        // 动量已抽到 momentum 模块,各策略可经 binance_momentum() 复用同一定义。
        let xt_adj = xt - basis60;
        let e = (xt_adj - price_to_beat)        // 币安现价(基差校正) − Chainlink 开盘价
            + MOM_W_SHORT * mom_short           // 币安短窗动量(默认 2s)
            + MOM_W_LONG  * mom_long;           // 币安长窗动量(默认 5s)

        // 噪声尺度 V = σ120 × √T
        let t = seconds_left.max(1) as f64;
        let v = sigma120 * t.sqrt();

        if v < 1e-8 { return None; }

        let z = e / v;
        let p_up = normal_cdf(z);
        let p_down = 1.0 - p_up;

        Some(ZSignal {
            z, p_up, p_down, e, v,
            ct, xt, b: price_to_beat,
            sigma120, basis60,
            seconds_left,
        })
    }

}

/// 最近60秒 Binance−Chainlink 平均基差。基于已取好的快照切片,不再自行 snapshot。
/// 语义与原实现等价:对每个60秒内的 Chainlink 点找最近的 Binance 点,|Δts|≤2 才计入。
fn compute_basis60(now: i64, cl: &[PricePoint], bn: &[TradePoint]) -> f64 {
    let cutoff = now - 60;
    let mut sum = 0.0;
    let mut n = 0usize;
    for cp in cl.iter().filter(|p| p.ts >= cutoff) {
        if let Some(bp) = bn.iter().min_by_key(|p| (p.ts - cp.ts).abs()) {
            if (bp.ts - cp.ts).abs() <= 2 {
                sum += bp.price - cp.price;
                n += 1;
            }
        }
    }
    if n == 0 { 0.0 } else { sum / n as f64 }
}

/// 最近120秒每秒价格变化的标准差。基于已取好的 Binance 快照切片,不再自行 snapshot。
fn compute_sigma120(now: i64, bn: &[TradePoint]) -> f64 {
    let cutoff = now - 120;
    let prices: Vec<f64> = bn.iter()
        .filter(|p| p.ts >= cutoff)
        .map(|p| p.price)
        .collect();

    if prices.len() < 3 { return 50.0; } // 默认50美元波动

    let changes: Vec<f64> = prices.windows(2)
        .map(|w| w[1] - w[0])
        .collect();

    let mean = changes.iter().sum::<f64>() / changes.len() as f64;
    let variance = changes.iter()
        .map(|x| (x - mean).powi(2))
        .sum::<f64>() / changes.len() as f64;
    variance.sqrt().max(1.0)
}

/// 标准正态分布 CDF（Abramowitz & Stegun 近似）
pub fn normal_cdf(z: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.2316419 * z.abs());
    let poly = t * (0.319381530
        + t * (-0.356563782
        + t * (1.781477937
        + t * (-1.821255978
        + t * 1.330274429))));
    let pdf = (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let cdf = 1.0 - pdf * poly;
    if z >= 0.0 { cdf } else { 1.0 - cdf }
}
