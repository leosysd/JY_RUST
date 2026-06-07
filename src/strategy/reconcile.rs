use super::smart::SmartStrategy;
use anyhow::Result;
use std::collections::HashSet;
use tracing::{info, warn};

impl SmartStrategy {
    /// 启动对账:把本地 state 的 open_orders 与链上真实挂单对齐,消除
    /// "下了单但状态没记住 / 重启后本地 state 与链上不一致"的风险。
    ///
    /// - 幻影单(本地有、链上无 = 已成交或已撤):纯本地纠偏,从 state 移除(安全)。
    /// - 孤儿单(链上有、本地无 = 上次崩溃残留):只告警 + 记录,不自动撤
    ///   (自动撤有风险:若 state 文件丢失会误撤一切)。
    ///
    /// DryRun 下 list_open_orders 返回空、usdc_balance 返回 0,基本空跑,安全。
    pub(crate) async fn reconcile_on_startup(&mut self) -> Result<()> {
        // 1. 链上当前挂单
        let chain = self.executor.list_open_orders().await?;
        // 2. 余额(失败不致命,记 -1.0 表示未取到)
        let bal = self.executor.usdc_balance().await.unwrap_or(-1.0);

        // 3. 链上 order_id 集合
        let chain_ids: HashSet<String> = chain.iter().map(|o| o.order_id.clone()).collect();

        // 本地 state 所有 open_orders 的 order_id 集合(用于孤儿单判定)
        let mut local_ids: HashSet<String> = HashSet::new();
        for (slug, _) in self.state.open_order_slugs() {
            if let Some(pos) = self.state.get(&slug) {
                for o in &pos.open_orders {
                    local_ids.insert(o.order_id.clone());
                }
            }
        }
        let state_orders = local_ids.len();

        // 4. 幻影单:本地有、链上无 → 从对应 pos 的 open_orders 移除(纯本地纠偏)
        let mut phantom = 0usize;
        let targets = self.state.open_order_slugs();
        for (slug, end_ts) in targets {
            // 该盘需要清掉的幻影单 id(链上不存在的)
            let phantom_ids: Vec<String> = self
                .state
                .get(&slug)
                .map(|p| {
                    p.open_orders
                        .iter()
                        .filter(|o| !chain_ids.contains(&o.order_id))
                        .map(|o| o.order_id.clone())
                        .collect()
                })
                .unwrap_or_default();
            if phantom_ids.is_empty() {
                continue;
            }
            {
                let pos = self.state.get_or_create(&slug, end_ts);
                pos.open_orders.retain(|o| !phantom_ids.contains(&o.order_id));
            }
            for id in phantom_ids {
                phantom += 1;
                self.write_signal(&serde_json::json!({
                    "phase": "reconcile",
                    "kind": "phantom_cleared",
                    "order_id": id,
                    "market": slug,
                    "ts": chrono::Utc::now().timestamp(),
                }))
                .await?;
            }
        }
        // 幻影单已被移除,落盘
        if phantom > 0 {
            self.state.save().await?;
        }

        // 5. 孤儿单:链上有、本地无 → 只告警 + 记录,不自动撤
        let mut orphan = 0usize;
        for o in &chain {
            if local_ids.contains(&o.order_id) {
                continue;
            }
            orphan += 1;
            warn!(
                "[RECONCILE] 孤儿单(链上有、本地无): id={} token={} price={:.3} (不自动撤,需人工核查)",
                o.order_id, o.token_id, o.price
            );
            self.write_signal(&serde_json::json!({
                "phase": "reconcile",
                "kind": "orphan",
                "order_id": o.order_id,
                "token_id": o.token_id,
                "price": o.price,
                "side": o.side,
                "ts": chrono::Utc::now().timestamp(),
            }))
            .await?;
        }

        // 6. 汇总
        let chain_orders = chain.len();
        self.write_signal(&serde_json::json!({
            "phase": "reconcile",
            "kind": "summary",
            "balance": bal,
            "chain_orders": chain_orders,
            "state_orders": state_orders,
            "phantom": phantom,
            "orphan": orphan,
            "ts": chrono::Utc::now().timestamp(),
        }))
        .await?;
        info!(
            "[RECONCILE] 启动对账完成: 余额=${:.2} 链上挂单={} 本地挂单={} 幻影清理={} 孤儿告警={}",
            bal, chain_orders, state_orders, phantom, orphan
        );
        Ok(())
    }
}
