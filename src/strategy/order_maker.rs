use super::smart::{beijing_now, strategy_order_shares, SmartStrategy};
use crate::clob::Market;
use crate::position::TradeRecord;
use anyhow::Result;
use tracing::{debug, info, warn};

impl SmartStrategy {
    // ── maker 路线:挂 GTC post_only 单(省 taker 费)+ 收割闭环 ───────────────

    /// maker 入场买单:在对侧 ask 下 1 tick 挂 GTC post_only 单。
    /// DryRun 立即全成交直接记账;LIVE 挂单成功则进 open_orders 等 harvest_makers 收割。
    ///
    /// 仅薄封装:算出"跟随盘口"挂价(price-1tick)后委托 do_buy_maker_at。
    /// market maker 模式行为与拆分前完全一致。
    pub(crate) async fn do_buy_maker(
        &mut self,
        market: &Market,
        dir: &str,
        price: f64,
        shares: f64,
        phase_label: &str,
        price_to_beat: f64,
    ) -> Result<bool> {
        // 挂价:比对侧 ask 低 1 tick,post_only 保证是 maker
        let maker_px = (price - 0.01).clamp(0.01, 0.99);
        self.do_buy_maker_at(market, dir, maker_px, shares, phase_label, price_to_beat).await
    }

    /// maker 入场买单(精确价):直接用传入的 maker_px 挂 GTC post_only 单。
    /// 去重/place_maker/DryRun记账/记OpenOrder/token_id 逻辑与 do_buy_maker 完全一致,
    /// 唯一区别是挂价由调用方给定(zquote 双边固定报价用此)。
    pub(crate) async fn do_buy_maker_at(
        &mut self,
        market: &Market,
        dir: &str,
        maker_px: f64,
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

        // 尝试节流:覆盖去重失效的两种重挂场景——
        //   1. DryRun:挂单后又被 TTL 撤掉,open_orders 已空但不应立刻重挂;
        //   2. LIVE:挂单失败(余额不足等)直接 return,open_orders 也空。
        // 距上次「尝试挂单」不足 maker_quote_ttl_secs 则跳过,避免每 tick 重复挂单刷屏。
        let now = chrono::Utc::now().timestamp();
        let attempt_key = format!("{}|{}", market.slug, dir);
        if now - *self.maker_attempt.get(&attempt_key).unwrap_or(&0)
            < self.config.maker_quote_ttl_secs
        {
            debug!("[MAKER] {} {dir} 近期已尝试挂单,节流跳过", market.title);
            return Ok(false);
        }
        self.maker_attempt.insert(attempt_key, now);

        // audit:决策要挂单、真正发单前记 intent。
        self.write_signal(&serde_json::json!({
            "phase": "intent", "market": market.slug,
            "direction": dir, "shares": shares, "price": maker_px,
            "label": phase_label, "mode": self.config.order_mode,
            "ts": chrono::Utc::now().timestamp(),
        })).await?;

        let fill = match self.executor.place_maker(token, maker_px, shares).await {
            Ok(f) => f,
            Err(e) => { warn!("[MAKER ORDER ERR] {} {dir} {phase_label}: {e:#}", market.title); return Ok(false); }
        };
        // audit:executor 返回后记 submit。
        self.write_signal(&serde_json::json!({
            "phase": "submit", "order_id": fill.order_id, "success": fill.success,
            "filled_shares": fill.filled_shares, "filled_price": fill.filled_price,
            "market": market.slug, "direction": dir,
            "ts": chrono::Utc::now().timestamp(),
        })).await?;
        if !fill.success { return Ok(false); }

        // 挂单成功(DryRun 模拟「已挂、未成交」/ LIVE 真挂):push 进 open_orders,
        // 等 harvest_makers 收割。DryRun 下 query_order 返回 size_matched=0(不成交),
        // 到 TTL 由 harvest 撤单移除,下次 decide 节流也到期才重挂,形成挂→等→撤→重挂闭环。
        let order = crate::position::OpenOrder {
            order_id: fill.order_id.clone(),
            side: dir.to_string(),
            price: maker_px,
            size: shares,
            matched_recorded: 0.0,
            placed_ts: now,
            phase: phase_label.to_string(),
            seen_live: false,
            token_id: token.to_string(),
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

                // ── 撤单/移除判定（完整 lifecycle）──────────────────────────
                // 撤掉后不在此重挂:下个 tick 策略循环会按新盘口自然挂新价(cancel/replace)。
                let full = matched >= o.size - 0.001;
                if full {
                    let pos = self.state.get_or_create(&slug, end_ts);
                    pos.open_orders.retain(|x| x.order_id != o.order_id);
                    continue;
                }

                // 条件 1:TTL 过期(挂单存活上限)。
                let ttl_expired = now - o.placed_ts >= self.config.maker_quote_ttl_secs;
                // 条件 2:盘末(force_lock 窗口内不再挂新单,已有挂单一律撤)。
                let near_end = now >= end_ts - self.config.force_lock_seconds_left;

                // 条件 3:盘口移动(仅"跟随盘口"的挂单;zquote 固定报价跳过)。
                // 取价用本地 BookCache(self.cache,按 token_id 索引),零网络请求。
                // "现在该挂的价" = 同向 best_ask - 0.01(与 do_buy_maker 的挂价口径一致);
                // 与挂单价偏离 > maker_replace_ticks 个 0.01 tick 即撤,让策略按新盘口重挂。
                let mut quote_moved = false;
                if !o.phase.starts_with("zquote") && !o.token_id.is_empty() {
                    let best_ask = {
                        let cache = self.cache.read().await;
                        cache.get(&o.token_id)
                            .and_then(|b| b.best_ask())
                            .and_then(|d| d.try_into().ok())
                            .map(|a: f32| f64::from(a))
                    };
                    if let Some(best_ask) = best_ask {
                        let target_px = (best_ask - 0.01).clamp(0.01, 0.99);
                        let drift = (target_px - o.price).abs();
                        let tol = self.config.maker_replace_ticks as f64 * 0.01;
                        // 浮点容差:用 1e-9 抵消 0.01 二进制误差,避免恰好等于阈值时误撤。
                        if drift > tol + 1e-9 {
                            quote_moved = true;
                        }
                    }
                }

                if ttl_expired || near_end || quote_moved {
                    let _ = self.executor.cancel(&o.order_id).await;
                    let pos = self.state.get_or_create(&slug, end_ts);
                    pos.open_orders.retain(|x| x.order_id != o.order_id);
                    let reason = if near_end { "盘末" }
                        else if ttl_expired { "TTL过期" }
                        else { "盘口移动" };
                    // audit:撤单结构化记录(reason 区分三种触发条件)。
                    let reason_tag = if near_end { "near_end" }
                        else if ttl_expired { "ttl" }
                        else { "quote_moved" };
                    let _ = self.write_signal(&serde_json::json!({
                        "phase": "cancel", "order_id": o.order_id, "reason": reason_tag,
                        "market": slug, "direction": o.side,
                        "ts": now,
                    })).await;
                    info!("[MAKER CANCEL] {} {} {reason}撤单 id={} @{:.3}", slug, o.side, o.order_id, o.price);
                }
                // 否则:保留,继续等
            }
        }
        self.state.save().await?;
        Ok(())
    }
}
