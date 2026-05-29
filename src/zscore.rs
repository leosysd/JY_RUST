use crate::feeds::{BinanceFeed, ChainlinkFeed};

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

    /// 计算当前 z-score 信号
    /// price_to_beat: 本轮开盘时的 Chainlink 价格 (B)
    /// seconds_left: 距结算剩余秒数
    pub fn compute(&self, price_to_beat: f64, seconds_left: i64) -> Option<ZSignal> {
        let now = chrono::Utc::now().timestamp();

        let cl_latest = self.chainlink.latest()?;
        let bn_latest = self.binance.latest()?;

        // 检查数据是否足够新鲜（最多5秒延迟）
        if (now - cl_latest.ts).abs() > 5 || (now - bn_latest.ts).abs() > 5 {
            return None;
        }

        let ct = cl_latest.price;
        let xt = bn_latest.price;

        // 2秒前和5秒前的 Binance 价格
        let xt_2 = self.binance.at_ts(now - 2).unwrap_or(xt);
        let xt_5 = self.binance.at_ts(now - 5).unwrap_or(xt);

        // 2秒前 Chainlink
        let ct_2 = self.chainlink.at_ts(now - 2).unwrap_or(ct);

        // basis60：最近60秒 Binance - Chainlink 平均差
        let basis60 = self.compute_basis60(now);

        // σ120：最近120秒每秒价格变化的标准差
        let sigma120 = self.compute_sigma120(now);

        // 预期偏移 E
        let e = (ct - price_to_beat)
            + 0.60 * (xt - xt_2)
            + 0.30 * (xt - xt_5)
            + 0.20 * (ct - ct_2)
            + 0.20 * ((xt - ct) - basis60);

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

    fn compute_basis60(&self, now: i64) -> f64 {
        let cl = self.chainlink.snapshot();
        let bn = self.binance.snapshot();
        let cutoff = now - 60;

        let mut diffs = vec![];
        for cp in cl.iter().filter(|p| p.ts >= cutoff) {
            if let Some(bp) = bn.iter().min_by_key(|p| (p.ts - cp.ts).abs()) {
                if (bp.ts - cp.ts).abs() <= 2 {
                    diffs.push(bp.price - cp.price);
                }
            }
        }
        if diffs.is_empty() { return 0.0; }
        diffs.iter().sum::<f64>() / diffs.len() as f64
    }

    fn compute_sigma120(&self, now: i64) -> f64 {
        let cutoff = now - 120;
        let bn = self.binance.snapshot();
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
