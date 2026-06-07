use super::smart::{strategy_order_shares, SmartStrategy};
use crate::clob::Market;
use tracing::{info, warn};

impl SmartStrategy {
    // ── 通用：买入（不切换 Locked）────────────────────────────────────────

    /// 下单（真实或模拟）。返回成交结果；None 表示无法下单（找不到 token 或网络错误）。
    pub(crate) async fn place_order(
        &self,
        market: &Market,
        dir: &str,
        price: f64,
        shares: f64,
        phase_label: &str,
        limit_price: Option<f64>,
    ) -> Option<crate::executor::Fill> {
        let shares = match strategy_order_shares(shares) {
            Some(v) => v,
            None => {
                warn!("[SMART] {} {dir} {phase_label} 下单份额非法: {shares}", market.title);
                return None;
            }
        };
        let token = match market.token_for(dir) {
            Some(t) => t,
            None => {
                warn!("[SMART] {} 找不到 {dir} 的 token_id，跳过下单", market.title);
                return None;
            }
        };
        match self.executor.buy(token, price, shares, limit_price).await {
            Ok(fill) => {
                if !fill.simulated {
                    info!("[SMART ORDER] {} {dir} {phase_label} id={} status={} ok={} 成交{:.1}份@{:.3}",
                        market.title, fill.order_id, fill.status, fill.success,
                        fill.filled_shares, fill.filled_price);
                    // 实盘发单但未成交=扑空。记一条 phase:"miss" 到 signals,
                    // 供 stats 算"下单未成交率"。纯增量记录,不改任何成交/决策逻辑;
                    // train.py 只读 phase=settlement/kind=train_sample,不读 miss,训练不受影响。
                    if !fill.success {
                        let _ = self.write_signal(&serde_json::json!({
                            "phase": "miss", "market": market.slug,
                            "direction": dir, "price": price, "shares": shares,
                            "label": phase_label,
                            "dry_run": self.config.dry_run,
                            "ts": chrono::Utc::now().timestamp(),
                        })).await;
                    }
                }
                Some(fill)
            }
            Err(e) => {
                warn!("[SMART ORDER ERR] {} {dir} {phase_label}: {e:#}", market.title);
                None
            }
        }
    }
}
