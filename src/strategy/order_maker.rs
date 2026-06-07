use super::smart::{beijing_now, record_trade, strategy_order_shares, SmartStrategy};
use crate::clob::Market;
use crate::position::TradeRecord;
use anyhow::Result;
use tracing::{debug, info, warn};

impl SmartStrategy {
    // ── maker 路线:挂 GTC post_only 单(省 taker 费)+ 收割闭环 ───────────────

    /// maker 入场买单:在对侧 ask 下 1 tick 挂 GTC post_only 单。
    /// DryRun 立即全成交直接记账;LIVE 挂单成功则进 open_orders 等 harvest_makers 收割。
    pub(crate) async fn do_buy_maker(
        &mut self,
        market: &Market,
        dir: &str,
        price: f64,
        shares: f64,
        phase_label: &str,
        price_to_beat: f64,
    ) -> Result<bool> {
        let Some(shares) = strategy_order_shares(shares) else {
            warn!("[MAKER] {} {dir} {phase_label} 下单份额非法: {shares}", market.title);
            return Ok(false);
        };
        let Some(token) = market.token_for(dir) else {
            warn!("[MAKER] {} 找不到 {dir} 的 token_id,跳过挂单", market.title);
            return Ok(false);
        };

        // 去重:该盘已有同方向 maker 挂单则不重复挂
        if let Some(pos) = self.state.get(&market.slug) {
            if pos.open_orders.iter().any(|o| o.side == dir) {
                debug!("[MAKER] {} {dir} 已有未结挂单,跳过", market.title);
                return Ok(false);
            }
        }

        // 挂价:比对侧 ask 低 1 tick,post_only 保证是 maker
        let maker_px = (price - 0.01).clamp(0.01, 0.99);

        let fill = match self.executor.place_maker(token, maker_px, shares).await {
            Ok(f) => f,
            Err(e) => { warn!("[MAKER ORDER ERR] {} {dir} {phase_label}: {e:#}", market.title); return Ok(false); }
        };
        if !fill.success { return Ok(false); }

        // DryRun 特判:模拟立即全成交,直接记账,不进挂单追踪
        if fill.simulated && fill.filled_shares > 0.0 {
            record_trade(&mut self.state, market, dir, maker_px, fill.filled_shares, phase_label, price_to_beat, false);
            self.state.save().await?;
            return Ok(true);
        }

        // LIVE 挂单成功:push 进 open_orders,等 harvest_makers 收割
        let now = chrono::Utc::now().timestamp();
        let order = crate::position::OpenOrder {
            order_id: fill.order_id.clone(),
            side: dir.to_string(),
            price: maker_px,
            size: shares,
            matched_recorded: 0.0,
            placed_ts: now,
            phase: phase_label.to_string(),
            seen_live: false,
        };
        let pos = self.state.get_or_create(&market.slug, market.end_ts);
        pos.open_orders.push(order);
        self.state.save().await?;
        info!("[MAKER {} ] {} {dir} {phase_label} 挂单 id={} @{maker_px:.3} ×{shares:.0}份",
            if self.config.dry_run { "DRY_RUN" } else { "LIVE" }, market.title, fill.order_id);
        Ok(true)
    }

    /// maker 收割闭环:轮询所有未结挂单,增量记成交、过期/全成交则撤单移除。
    /// market 模式下直接返回。borrow 安全:先 clone 快照→await 查询→再 get_or_create 改 pos。
    pub(crate) async fn harvest_makers(&mut self) -> Result<()> {
        if self.config.order_mode != "maker" { return Ok(()); }
        let now = chrono::Utc::now().timestamp();
        let targets = self.state.open_order_slugs();
        for (slug, end_ts) in targets {
            // 该盘 open_orders 快照(query/cancel 是 await,期间不持有 state 可变借用)
            let orders = self.state.get(&slug).map(|p| p.open_orders.clone()).unwrap_or_default();
            for o in orders {
                let st = match self.executor.query_order(&o.order_id).await {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let matched = st.size_matched;

                // 增量成交:补记 maker 成交(maker 无 taker fee)
                let new_fill = matched - o.matched_recorded;
                if new_fill > 0.001 {
                    let trade = TradeRecord {
                        side: o.side.clone(),
                        shares: new_fill,
                        price: o.price,
                        fee_per_share: 0.0,
                        full_cost_per_share: o.price,
                        total_cost: o.price * new_fill,
                        phase: o.phase.clone(),
                        ts: now,
                        time_bj: beijing_now(),
                    };
                    {
                        let pos = self.state.get_or_create(&slug, end_ts);
                        pos.add_trade(trade);
                        // 更新该挂单的 matched_recorded
                        if let Some(oo) = pos.open_orders.iter_mut().find(|x| x.order_id == o.order_id) {
                            oo.matched_recorded = matched;
                        }
                    }
                    let _ = self.write_signal(&serde_json::json!({
                        "phase": "maker_fill", "market": slug,
                        "direction": o.side, "price": o.price, "shares": new_fill,
                        "full_cost": o.price,
                        "dry_run": self.config.dry_run, "ts": now,
                    })).await;
                    info!("[MAKER FILL] {} {} 成交{new_fill:.1}份@{:.3}", slug, o.side, o.price);
                }

                // 撤单/移除判定
                let expired = now >= end_ts - self.config.force_lock_seconds_left
                    || (now - o.placed_ts) > 90;
                let full = matched >= o.size - 0.001;
                if full {
                    let pos = self.state.get_or_create(&slug, end_ts);
                    pos.open_orders.retain(|x| x.order_id != o.order_id);
                } else if expired {
                    let _ = self.executor.cancel(&o.order_id).await;
                    let pos = self.state.get_or_create(&slug, end_ts);
                    pos.open_orders.retain(|x| x.order_id != o.order_id);
                    info!("[MAKER CANCEL] {} {} 挂单过期撤单 id={}", slug, o.side, o.order_id);
                }
                // 否则:保留,继续等
            }
        }
        self.state.save().await?;
        Ok(())
    }
}
