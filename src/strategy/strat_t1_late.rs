use super::smart::{record_trade, strategy_order_shares, SmartStrategy, T1LateState};
use crate::clob::Market;
use crate::position::full_cost_per_share;
use anyhow::Result;
use tracing::{info, warn};

fn strong_side(up_ask: f64, dn_ask: f64) -> (&'static str, f64, &'static str, f64) {
    if up_ask >= dn_ask {
        ("Up", up_ask, "Down", dn_ask)
    } else {
        ("Down", dn_ask, "Up", up_ask)
    }
}

fn dec_to_f64<T: ToString>(v: T) -> f64 {
    v.to_string().parse::<f64>().unwrap_or(0.0)
}

impl SmartStrategy {
    /// 路线八：T-10/T-8 同向确认, T-1 最后一秒 FAK 买强势边。
    ///
    /// 这是 Telonex 2026-05-22..2026-06-22 一个月数据里当前最强的 taker-only 候选:
    /// c10_8 confirm >=0.98, e1 entry >=0.75, opp<=0.30。固定 300u cap 日均约
    /// +281.80u; 300u 起步 100% dry equity 滚仓历史日收益均 >300u。
    pub(crate) async fn decide_t1_late(
        &mut self,
        market: &Market,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        if now < market.start_ts || seconds_left <= 0 {
            return Ok(());
        }

        if self
            .state
            .get(&market.slug)
            .map(|p| p.trades.iter().any(|t| t.phase.starts_with("t1_late")))
            .unwrap_or(false)
        {
            self.t1_late.entry(market.slug.clone()).or_default().locked = true;
            return Ok(());
        }

        let confirm1_secs = self.config.t1_late_confirm1_secs;
        let confirm2_secs = self.config.t1_late_confirm2_secs;
        let entry_secs = self.config.t1_late_entry_secs;
        let confirm_min = self.config.t1_late_confirm_min_ask;
        let (side, main_ask, _, _) = strong_side(up_ask, dn_ask);

        let mut signal: Option<serde_json::Value> = None;
        {
            let st = self
                .t1_late
                .entry(market.slug.clone())
                .or_insert_with(T1LateState::default);
            if st.locked {
                return Ok(());
            }

            if st.confirm1_side.is_empty()
                && seconds_left <= confirm1_secs
                && seconds_left > confirm2_secs
            {
                if main_ask < confirm_min {
                    st.locked = true;
                    signal = Some(serde_json::json!({
                        "phase": "t1_late_block",
                        "market": market.slug,
                        "reason": "confirm1_below_min",
                        "seconds_left": seconds_left,
                        "side": side,
                        "main_ask": main_ask,
                        "confirm_min": confirm_min,
                        "up_ask": up_ask,
                        "dn_ask": dn_ask,
                        "ts": now,
                    }));
                } else {
                    st.confirm1_side = side.to_string();
                    st.confirm1_ask = main_ask;
                    st.confirm1_seen_at = seconds_left;
                    signal = Some(serde_json::json!({
                        "phase": "t1_late_confirm1",
                        "market": market.slug,
                        "seconds_left": seconds_left,
                        "side": side,
                        "main_ask": main_ask,
                        "up_ask": up_ask,
                        "dn_ask": dn_ask,
                        "ts": now,
                    }));
                }
            }
        }
        if let Some(v) = signal.take() {
            self.write_signal(&v).await?;
        }

        let mut signal: Option<serde_json::Value> = None;
        {
            let st = self
                .t1_late
                .entry(market.slug.clone())
                .or_insert_with(T1LateState::default);
            if st.locked {
                return Ok(());
            }
            if st.confirm2_side.is_empty()
                && seconds_left <= confirm2_secs
                && seconds_left > entry_secs
            {
                if st.confirm1_side.is_empty() {
                    st.locked = true;
                    signal = Some(serde_json::json!({
                        "phase": "t1_late_block",
                        "market": market.slug,
                        "reason": "missing_confirm1",
                        "seconds_left": seconds_left,
                        "up_ask": up_ask,
                        "dn_ask": dn_ask,
                        "ts": now,
                    }));
                } else if side != st.confirm1_side {
                    st.locked = true;
                    signal = Some(serde_json::json!({
                        "phase": "t1_late_block",
                        "market": market.slug,
                        "reason": "confirm2_side_changed",
                        "confirm1_side": st.confirm1_side,
                        "confirm2_side": side,
                        "seconds_left": seconds_left,
                        "up_ask": up_ask,
                        "dn_ask": dn_ask,
                        "ts": now,
                    }));
                } else if main_ask < confirm_min {
                    st.locked = true;
                    signal = Some(serde_json::json!({
                        "phase": "t1_late_block",
                        "market": market.slug,
                        "reason": "confirm2_below_min",
                        "side": side,
                        "main_ask": main_ask,
                        "confirm_min": confirm_min,
                        "seconds_left": seconds_left,
                        "up_ask": up_ask,
                        "dn_ask": dn_ask,
                        "ts": now,
                    }));
                } else {
                    st.confirm2_side = side.to_string();
                    st.confirm2_ask = main_ask;
                    st.confirm2_seen_at = seconds_left;
                    signal = Some(serde_json::json!({
                        "phase": "t1_late_confirm2",
                        "market": market.slug,
                        "seconds_left": seconds_left,
                        "side": side,
                        "main_ask": main_ask,
                        "confirm1_side": st.confirm1_side,
                        "confirm1_ask": st.confirm1_ask,
                        "up_ask": up_ask,
                        "dn_ask": dn_ask,
                        "ts": now,
                    }));
                }
            }
        }
        if let Some(v) = signal.take() {
            self.write_signal(&v).await?;
        }

        if seconds_left > entry_secs {
            return Ok(());
        }

        let st = self.t1_late.get(&market.slug).cloned().unwrap_or_default();
        if st.locked {
            return Ok(());
        }
        if st.confirm1_side.is_empty() || st.confirm2_side.is_empty() {
            self.t1_late.entry(market.slug.clone()).or_default().locked = true;
            self.write_signal(&serde_json::json!({
                "phase": "t1_late_block",
                "market": market.slug,
                "reason": "missing_confirm_before_entry",
                "seconds_left": seconds_left,
                "up_ask": up_ask,
                "dn_ask": dn_ask,
                "ts": now,
            }))
            .await?;
            return Ok(());
        }

        let (entry_side, entry_ask, opp_side, opp_ask) = strong_side(up_ask, dn_ask);
        if entry_side != st.confirm1_side || entry_side != st.confirm2_side {
            self.t1_late.entry(market.slug.clone()).or_default().locked = true;
            self.write_signal(&serde_json::json!({
                "phase": "t1_late_block",
                "market": market.slug,
                "reason": "entry_side_changed",
                "entry_side": entry_side,
                "confirm1_side": st.confirm1_side,
                "confirm2_side": st.confirm2_side,
                "seconds_left": seconds_left,
                "up_ask": up_ask,
                "dn_ask": dn_ask,
                "ts": now,
            }))
            .await?;
            return Ok(());
        }
        if entry_ask < self.config.t1_late_entry_min_ask
            || opp_ask > self.config.t1_late_opp_max_ask
        {
            self.t1_late.entry(market.slug.clone()).or_default().locked = true;
            self.write_signal(&serde_json::json!({
                "phase": "t1_late_block",
                "market": market.slug,
                "reason": "entry_filter",
                "entry_side": entry_side,
                "entry_ask": entry_ask,
                "opp_side": opp_side,
                "opp_ask": opp_ask,
                "entry_min": self.config.t1_late_entry_min_ask,
                "opp_max": self.config.t1_late_opp_max_ask,
                "seconds_left": seconds_left,
                "ts": now,
            }))
            .await?;
            return Ok(());
        }

        let ask_size = self.t1_late_top_ask_size(market, entry_side).await;
        let equity = (self.config.t1_late_start_equity
            + self.state.realized_pnl_for_phase_prefix("t1_late"))
        .max(0.0);
        let mut max_deploy = f64::INFINITY;
        if self.config.t1_late_risk_fraction > 0.0 {
            max_deploy = max_deploy.min(equity * self.config.t1_late_risk_fraction);
        }
        if self.config.t1_late_max_deploy_usdc > 0.0 {
            max_deploy = max_deploy.min(self.config.t1_late_max_deploy_usdc);
        }
        if !max_deploy.is_finite() {
            max_deploy = self.config.t1_late_target_qty * full_cost_per_share(entry_ask);
        }

        let cost_per_share = full_cost_per_share(entry_ask);
        let qty_by_cost = (max_deploy / cost_per_share).floor();
        let planned = strategy_order_shares(
            self.config
                .t1_late_target_qty
                .min(ask_size.floor())
                .min(qty_by_cost),
        )
        .unwrap_or(0.0);
        let planned_cost = planned * cost_per_share;
        if planned < 1.0 || planned_cost < 1.0 {
            self.t1_late.entry(market.slug.clone()).or_default().locked = true;
            self.write_signal(&serde_json::json!({
                "phase": "t1_late_block",
                "market": market.slug,
                "reason": "planned_order_too_small",
                "entry_side": entry_side,
                "entry_ask": entry_ask,
                "ask_size": ask_size,
                "equity": equity,
                "max_deploy": max_deploy,
                "planned_shares": planned,
                "planned_cost": planned_cost,
                "seconds_left": seconds_left,
                "ts": now,
            }))
            .await?;
            return Ok(());
        }

        self.t1_late.entry(market.slug.clone()).or_default().locked = true;
        let price_to_beat = self
            .model
            .chainlink_at(market.start_ts)
            .or_else(|| self.model.chainlink_latest())
            .unwrap_or(0.0);
        let mode = if self.config.dry_run {
            "DRY_RUN"
        } else {
            "LIVE"
        };
        info!(
            "[T1_LATE {mode}] {} {entry_side}@{entry_ask:.3}×{planned:.0} ask_size={ask_size:.0} equity={equity:.2} deploy≈{planned_cost:.2} T-{seconds_left}s",
            market.title
        );
        self.write_signal(&serde_json::json!({
            "phase": "intent",
            "label": "t1_late_entry",
            "market": market.slug,
            "direction": entry_side,
            "price": entry_ask,
            "shares": planned,
            "ask_size": ask_size,
            "equity": equity,
            "max_deploy": max_deploy,
            "planned_cost": planned_cost,
            "confirm1_seen_at": st.confirm1_seen_at,
            "confirm1_ask": st.confirm1_ask,
            "confirm2_seen_at": st.confirm2_seen_at,
            "confirm2_ask": st.confirm2_ask,
            "seconds_left": seconds_left,
            "mode": "fak",
            "ts": now,
        }))
        .await?;

        let Some(token) = market.token_for(entry_side) else {
            warn!(
                "[T1_LATE] {} 找不到 {entry_side} token_id,跳过",
                market.title
            );
            return Ok(());
        };
        let fill = match self
            .executor
            .buy_fak(token, entry_ask, planned, Some(entry_ask))
            .await
        {
            Ok(f) => f,
            Err(e) => {
                warn!("[T1_LATE ORDER ERR] {} {entry_side}: {e:#}", market.title);
                return Ok(());
            }
        };
        self.write_signal(&serde_json::json!({
            "phase": "submit",
            "label": "t1_late_entry",
            "order_id": fill.order_id,
            "success": fill.success,
            "filled_shares": fill.filled_shares,
            "filled_price": fill.filled_price,
            "market": market.slug,
            "direction": entry_side,
            "ts": chrono::Utc::now().timestamp(),
        }))
        .await?;

        if !fill.success || fill.filled_shares <= 0.0 {
            return Ok(());
        }
        let rp = fill.filled_price;
        let rs = fill.filled_shares;
        self.write_signal(&serde_json::json!({
            "phase": "t1_late_entry",
            "market": market.slug,
            "direction": entry_side,
            "price": rp,
            "shares": rs,
            "full_cost": full_cost_per_share(rp),
            "ask_size": ask_size,
            "dry_run": self.config.dry_run,
            "ts": chrono::Utc::now().timestamp(),
        }))
        .await?;
        if !self.config.dry_run {
            record_trade(
                &mut self.ideal_state,
                market,
                entry_side,
                entry_ask,
                planned,
                "t1_late_entry",
                price_to_beat,
                false,
            );
            self.ideal_state.save().await?;
        }
        record_trade(
            &mut self.state,
            market,
            entry_side,
            rp,
            rs,
            "t1_late_entry",
            price_to_beat,
            false,
        );
        self.state.save().await?;
        Ok(())
    }

    async fn t1_late_top_ask_size(&self, market: &Market, side: &str) -> f64 {
        let Some(token) = market.token_for(side) else {
            return 0.0;
        };
        let cache = self.cache.read().await;
        cache
            .get(token)
            .and_then(|b| b.asks.first().map(|(_, s)| dec_to_f64(*s)))
            .unwrap_or(0.0)
    }
}
