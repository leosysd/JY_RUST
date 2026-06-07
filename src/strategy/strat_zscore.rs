use super::smart::{
    SmartStrategy, ARB_THRESHOLD, ENTRY_MIN_SECONDS_LEFT, LOTTERY_MAX_PRICE, MAX_TREND_TRADES,
    MICRO_DIVISOR, REBALANCE_MIN_IMPROVE_FACTOR, TREND_ENTRY_MAX, TREND_ENTRY_MIN, TREND_STEP,
    TREND_WORST_PNL_FLOOR_FACTOR,
};
use crate::clob::Market;
use crate::position::{full_cost_per_share, MarketPosition, Phase};
use anyhow::Result;
use tracing::{debug, info};

impl SmartStrategy {
    // ── Waiting：无仓位时的决策 ───────────────────────────────────────────
    //
    // P1. 纯套利：两边全成本之和 < ARB_THRESHOLD → 同时买两边
    // P4. 趋势入场：z-score 方向明确，价格在 [0.48, 0.70] → 买一手

    pub(crate) async fn decide_waiting(
        &mut self,
        market: &Market,
        _pos: MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        if seconds_left < ENTRY_MIN_SECONDS_LEFT { return Ok(()); }

        // 必须用真开盘价(start_ts处的chainlink),取不到就跳过——不退回"最新价"凑数,
        // 否则 price_to_beat 失真→z方向变形(原 .or_else(latest) 是隐患,已去掉)。
        let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
        if price_to_beat < 1000.0 {
            debug!("[SMART] {} 无真开盘价(chainlink过期/未就绪)，跳过", market.title);
            return Ok(());
        }

        let shares = self.order_shares();

        // ── P1：纯套利 ────────────────────────────────────────────────────
        let arb_cost = full_cost_per_share(up_ask) + full_cost_per_share(dn_ask);
        if arb_cost < ARB_THRESHOLD {
            let proj = shares * (1.0 - arb_cost);
            let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
            info!(
                "[SMART ARB {mode}] {} Up@{up_ask:.3}+Down@{dn_ask:.3} 全成本={arb_cost:.4} 套利+${proj:.2}  T-{seconds_left}s",
                market.title
            );
            // 买 Up 建仓，再买 Down 锁定
            self.do_buy(&market, "Up", up_ask, shares, "arb_entry", price_to_beat).await?;
            let pos = self.state.get_or_create(&market.slug, market.end_ts).clone();
            let locked_pnl = pos.worst_pnl_if_add("Down", dn_ask, shares);
            self.do_lock(&market, &pos, "Down", dn_ask, shares, locked_pnl, "arb_lock").await?;
            return Ok(());
        }

        // ── P4：趋势入场 ──────────────────────────────────────────────────
        let Some(sig) = self.model.compute(price_to_beat, seconds_left) else {
            debug!("[SMART] {} 价格数据不足，跳过入场", market.title);
            return Ok(());
        };
        let Some(dir) = sig.direction() else {
            debug!("[SMART] {} z={:.3} 信号不足，不入场", market.title, sig.z);
            return Ok(());
        };

        let entry_ask = if dir == "Up" { up_ask } else { dn_ask };
        if entry_ask < TREND_ENTRY_MIN || entry_ask > TREND_ENTRY_MAX {
            debug!(
                "[SMART] {} {dir}@{entry_ask:.3} 不在追单区间[{TREND_ENTRY_MIN},{TREND_ENTRY_MAX}]，跳过",
                market.title
            );
            return Ok(());
        }

        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        info!(
            "[SMART ENTRY {mode}] {} {dir}@{entry_ask:.3} ×{shares:.0}份  z={:.3}  T-{seconds_left}s",
            market.title, sig.z
        );
        // 入场信号快照:丰富特征(为 LightGBM 铺路),结算后 join winner 作训练标签。
        let mut feat = self.build_features(&sig, dir, entry_ask, up_ask, dn_ask, seconds_left);
        self.add_book_depth(&mut feat, market).await;
        feat["phase"] = serde_json::json!("entry_signal");
        feat["market"] = serde_json::json!(market.slug);
        feat["strategy"] = serde_json::json!("zscore");
        self.write_signal(&feat).await?;
        self.do_buy(&market, dir, entry_ask, shares, "entry", price_to_beat).await?;
        Ok(())
    }

    // ── Holding：有仓位时的 5 优先级决策 ─────────────────────────────────

    pub(crate) async fn decide_holding(
        &mut self,
        market: &Market,
        pos: MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        let shares  = self.order_shares();
        let micro   = (shares / MICRO_DIVISOR).max(1.0);
        let trend_floor   = -(shares * TREND_WORST_PNL_FLOOR_FACTOR);   // 追单天花板,随份额缩放
        let rebalance_min = shares * REBALANCE_MIN_IMPROVE_FACTOR;      // 减险最小改善,随份额缩放
        let cur_worst = pos.worst_pnl();
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };

        // 找出主边（份额更多的一边）和弱边
        let (main_dir, opp_dir, main_ask, opp_ask) = if pos.up_shares >= pos.down_shares {
            ("Up", "Down", up_ask, dn_ask)
        } else {
            ("Down", "Up", dn_ask, up_ask)
        };
        let main_shares = if main_dir == "Up" { pos.up_shares } else { pos.down_shares };
        let opp_shares  = if opp_dir  == "Up" { pos.up_shares } else { pos.down_shares };
        // 锁仓只需补足"差额"使两边相等；对边已有的份额不能重复买，否则会超买成单边赌注
        let lock_qty = (main_shares - opp_shares).max(0.0);

        // ── P2：锁利（补差额至两边相等后 worst_pnl >= 份额×系数）────
        // 门槛随下单份额缩放：lock_min_profit_factor=0.2 时，设5需$1、设20需$4 才锁。
        let lock_min_profit = shares * self.config.lock_min_profit_factor;
        if lock_qty > 0.0 {
            let p2_worst = pos.worst_pnl_if_add(opp_dir, opp_ask, lock_qty);
            if p2_worst >= lock_min_profit {
                info!(
                    "[SMART LOCK_PROFIT {mode}] {} 买{opp_dir}@{opp_ask:.3} ×{lock_qty:.0}份  锁定worst_pnl={p2_worst:+.2}≥{lock_min_profit:.2}  T-{seconds_left}s",
                    market.title
                );
                self.do_lock(&market, &pos, opp_dir, opp_ask, lock_qty, p2_worst, "lock_profit").await?;
                return Ok(());
            }
        }

        // ── P2.5：早止损（方向押错时，不等 T-60 强制线提前锁亏）──────────────
        // 数据：旧实盘29场锁亏100%卡T-60被迫天价锁(中位0.66,最贵0.99)，单次均-1.85，
        // 是最大失血点。这里在方向刚转坏、亏损还小时提前补对面锁平，把大亏压成可控小亏。
        //
        // 触发信号 = "未实现方向亏损" = 主边份额 ×(主边现价 − 主边入场均价)。
        // 关键:不能用 worst_pnl —— 单边裸仓的 worst_pnl 恒为 ≈-满仓成本(假设对面赢),
        // 入场第一秒就 <止损线 会导致开盘秒锁。用现价跌幅才不误触发。
        //
        // 时间门槛(stop_loss_max_seconds_left):前段(剩余>此值)绝不止损,给5分钟行情时间,
        // 避免开盘段被正常波动晃出(之前剩280s就止损=刚入场就投降的问题)。
        // 只在盘后半段、方向确实没回来时才认小亏,而不是死扛到T-60天价锁。
        // naked 模式下完全关闭早止损(去掉锁亏=方向错就裸持,不提前补对面)。
        if self.config.stop_loss_factor > 0.0
            && self.config.force_loss_mode != "naked"
            && lock_qty > 0.0
            && seconds_left > self.config.force_lock_seconds_left
            && seconds_left <= self.config.stop_loss_max_seconds_left
        {
            let main_avg = if main_dir == "Up" { pos.up_avg_full() } else { pos.down_avg_full() };
            // 主边现价用 main_ask 近似(买一侧);跌破入场价即方向走坏
            let unrealized = main_shares * (main_ask - main_avg);
            let stop_line = -(shares * self.config.stop_loss_factor);
            if unrealized <= stop_line && opp_ask <= self.config.stop_loss_max_opp {
                let proj = pos.worst_pnl_if_add(opp_dir, opp_ask, lock_qty);
                info!(
                    "[SMART STOPLOSS {mode}] {} 方向亏{unrealized:+.2}≤{stop_line:.2} 止损买{opp_dir}@{opp_ask:.3} ×{lock_qty:.0}份  锁定{proj:+.2}  T-{seconds_left}s",
                    market.title
                );
                self.do_lock(&market, &pos, opp_dir, opp_ask, lock_qty, proj, "lock_loss").await?;
                return Ok(());
            }
        }

        // 当前趋势信号（用于决定"追单"还是"减险"）
        let trend_dir = self.model.compute(pos.price_to_beat, seconds_left)
            .and_then(|s| s.direction());

        // ── P4：趋势追单（趋势仍支持主边时，优先顺势加仓）────────────────
        // 注意：必须在 P3 减险之前，否则单边持仓时减险会每 tick 抢先触发，追单永不执行。
        if seconds_left > ENTRY_MIN_SECONDS_LEFT && trend_dir == Some(main_dir) {
            let trend_trades: Vec<_> = pos.trades.iter()
                .filter(|t| t.side == main_dir && !t.phase.starts_with("lock") && !t.phase.starts_with("arb"))
                .collect();
            let trade_count = trend_trades.len();
            let last_price  = trend_trades.last().map(|t| t.price).unwrap_or(0.0);

            // 追单价格上限：超过 trend_chase_max_price 不再追（避免追高被迫天价锁）
            if trade_count < MAX_TREND_TRADES
                && main_ask >= last_price + TREND_STEP
                && main_ask <= self.config.trend_chase_max_price
            {
                let p4_worst = pos.worst_pnl_if_add(main_dir, main_ask, shares);
                if p4_worst >= trend_floor {
                    info!(
                        "[SMART TREND {mode}] {} 追{main_dir}@{main_ask:.3} ×{shares:.0}份（第{}/{}笔）worst={p4_worst:+.2}  T-{seconds_left}s",
                        market.title, trade_count + 1, MAX_TREND_TRADES
                    );
                    self.do_buy(&market, main_dir, main_ask, shares, "trend_chase", pos.price_to_beat).await?;
                    return Ok(());
                }
                debug!("[SMART] {} 趋势追单会使worst={p4_worst:+.2} < 下限{trend_floor:.1}，跳过",
                    market.title);
            }
        }

        // ── P3：减险/对冲（仅当趋势不再支持主边时；改善需达阈值，避免每秒刷单）──
        // 合并了原"便宜保险"：对边便宜本就让 worst 改善更多，统一走这里。
        if seconds_left > ENTRY_MIN_SECONDS_LEFT && trend_dir != Some(main_dir) {
            let p3_worst = pos.worst_pnl_if_add(opp_dir, opp_ask, micro);
            if p3_worst - cur_worst >= rebalance_min {
                info!(
                    "[SMART HEDGE {mode}] {} 趋势转向，减险买{opp_dir}@{opp_ask:.3} ×{micro:.0}份  worst {cur_worst:+.2}→{p3_worst:+.2}  T-{seconds_left}s",
                    market.title
                );
                self.do_buy(&market, opp_dir, opp_ask, micro, "hedge", pos.price_to_beat).await?;
                return Ok(());
            }
        }

        // ── 强制处理（最后 force_lock_seconds_left 秒）─────────────────────
        if seconds_left <= self.config.force_lock_seconds_left {
            // 1) 补差额：若能锁平为盈利则照旧对锁；若会锁亏，按 force_loss_mode 处理。
            if lock_qty > 0.0 {
                let proj = pos.worst_pnl_if_add(opp_dir, opp_ask, lock_qty);
                if proj >= 0.0 {
                    // 对锁后仍保底盈利 → 照旧补对面锁平
                    info!(
                        "[SMART FORCE {mode}] {} 锁平 {opp_dir}@{opp_ask:.3} ×{lock_qty:.0}份  worst={proj:+.2}  T-{seconds_left}s",
                        market.title
                    );
                    if !self.do_buy(&market, opp_dir, opp_ask, lock_qty, "lock_profit", pos.price_to_beat).await? {
                        return Ok(());
                    }
                } else if self.config.force_loss_mode == "naked" {
                    // 去掉锁亏:方向押错不补对面、不花钱对锁,裸持到结算认那一边成本。
                    // 赌"行情常回来";最坏=入场那笔成本归零,但不再追加确定支出去锁亏。
                    info!(
                        "[SMART FORCE NAKED {mode}] {} 不锁亏裸持到结算 主{main_dir}{main_shares:.0}份 worst={proj:+.2}  T-{seconds_left}s",
                        market.title
                    );
                } else if self.config.force_loss_mode == "smooth" {
                    // 锁亏改"按趋势锁利"：不补对面认亏，而是顺当前领先方向加注，争取赢回。
                    // 领先方向 = ask 更高(更被市场看好)的一边。
                    let (lead_dir, lead_ask) = if up_ask >= dn_ask { ("Up", up_ask) } else { ("Down", dn_ask) };
                    let lead_capped = lead_ask.min(0.99);
                    let have_lead = if lead_dir == "Up" { pos.up_shares } else { pos.down_shares };
                    let cost_total = pos.up_cost_total + pos.down_cost_total;
                    // 回本所需该边总份额: X*(1-ask) >= cost - have*ask  =>  X >= (cost-have*ask)/(1-ask)
                    let need_total = if lead_capped < 0.999 {
                        (cost_total - have_lead * lead_capped) / (1.0 - lead_capped)
                    } else { have_lead };
                    let want = (need_total - have_lead).max(0.0);
                    // 预算封顶: 顺势加注最多再花 entry成本 × smooth_budget_mult
                    let budget = cost_total * self.config.smooth_budget_mult;
                    let max_by_budget = if lead_capped > 0.0 { budget / lead_capped } else { 0.0 };
                    let buy = want.min(max_by_budget);
                    if buy >= 1.0 {
                        info!(
                            "[SMART FORCE SMOOTH {mode}] {} 锁亏转顺势 买{lead_dir}@{lead_capped:.3} ×{buy:.0}份  原worst={proj:+.2}  T-{seconds_left}s",
                            market.title
                        );
                        let _ = self.do_buy(&market, lead_dir, lead_capped, buy, "smooth", pos.price_to_beat).await?;
                    } else {
                        info!("[SMART FORCE SMOOTH {mode}] {} 锁亏但加注<1份，裸持到结算  worst={proj:+.2}  T-{seconds_left}s",
                            market.title);
                    }
                } else {
                    // 旧行为：等额对锁(锁亏)
                    info!(
                        "[SMART FORCE {mode}] {} 锁亏 {opp_dir}@{opp_ask:.3} ×{lock_qty:.0}份  worst={proj:+.2}  T-{seconds_left}s",
                        market.title
                    );
                    if !self.do_buy(&market, opp_dir, opp_ask, lock_qty, "lock_loss", pos.price_to_beat).await? {
                        return Ok(());
                    }
                }
            }

            // 2) 可控冷门彩票：便宜边 ask ≤ LOTTERY_MAX_PRICE 时固定份额博一把。
            //    下行被极低价锁死（最多亏掉这笔成本），不参与对冲，纯方向小注。
            let (cheap_dir, cheap_ask) = if up_ask <= dn_ask { ("Up", up_ask) } else { ("Down", dn_ask) };
            if cheap_ask > 0.0 && cheap_ask <= LOTTERY_MAX_PRICE {
                let lot = self.order_shares();
                info!(
                    "[SMART LOTTERY {mode}] {} 冷门彩票 {cheap_dir}@{cheap_ask:.3} ×{lot:.0}份  成本≈${:.2}  T-{seconds_left}s",
                    market.title, cheap_ask * lot
                );
                let _ = self.do_buy(&market, cheap_dir, cheap_ask, lot, "lottery", pos.price_to_beat).await?;
            }

            // 3) 标记锁定
            let p = self.state.get_or_create(&market.slug, market.end_ts);
            p.phase = Phase::Locked;
            self.state.save().await?;
            return Ok(());
        }

        // 等待(每 tick 状态,降到 debug 以免逐 tick 同步写日志拖慢主循环)
        debug!(
            "[SMART] {} {main_dir}{main_shares:.0}份@{:.3} opp@{opp_ask:.3}  worst_pnl={cur_worst:+.2}  T-{seconds_left}s",
            market.title,
            if main_dir == "Up" { pos.up_avg_full() } else { pos.down_avg_full() }
        );
        Ok(())
    }
}
