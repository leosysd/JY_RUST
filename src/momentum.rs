//! 价格动量（独立模块）。动量 = 现价 − window 秒前价（取时间上最近的成交点）。
//!
//! 拆成独立模块的目的：让 z 信号、ML 特征、各策略都复用同一份动量定义，改这里即全体生效，
//! 不再像以前那样「compute() 里内联一份、binance_momentum() 里又一份」各算各的。
//!
//! 延时设计（见 feed 实现）：核心是纯函数 [`momentum_on`]，只吃一个「已取好的快照切片」
//! `&[TradePoint]`，自身不加锁、不 snapshot（整段克隆）。因此放进 `ZScoreModel::compute()`
//! （它本就持有一份快照）是**零额外开销**。需要独立调用的策略走
//! `ZScoreModel::binance_momentum()`，内部用 `latest()+at_ts()`（两次廉价锁、不克隆），
//! 与本函数同口径（都取时间最近点）。两条路径同一公式。

use crate::feeds::binance::TradePoint;

/// 找时间上离 `target_ts` 最近的成交点价格（与 `BinanceFeed::at_ts` 同口径：`min_by_key |Δts|`）。
fn price_nearest(bn: &[TradePoint], target_ts: i64) -> Option<f64> {
    bn.iter()
        .min_by_key(|p| (p.ts - target_ts).abs())
        .map(|p| p.price)
}

/// 基于已取好的快照切片算动量：现价 − (now − window_sec) 时刻的最近价。
///
/// 切片为空（取不到现价或历史价）→ `None`。**不自行加锁/克隆**，调用方传入快照即可，
/// 因此在已持有快照的热路径（如 `compute()`）里调用零额外成本。
pub fn momentum_on(bn: &[TradePoint], now: i64, window_sec: i64) -> Option<f64> {
    let cur = bn.last()?.price;
    let past = price_nearest(bn, now - window_sec)?;
    Some(cur - past)
}
