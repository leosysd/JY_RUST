use super::smart::{record_trade, strategy_order_shares, AccumLeg, SmartStrategy};
use crate::clob::Market;
use crate::position::full_cost_per_share;
use anyhow::Result;
use tracing::{info, warn};

impl SmartStrategy {
    // ── 路线六：accum 双边追涨补仓 + 计算模块 ─────────────────────────────
    //
    // 首笔 z 定主腿方向(盈亏锚点,整盘不换)。之后每 tick 对 Up/Down 两边:
    //  ① 谁涨追谁:某边 ask≥追涨档[0.62,0.65,0.68,0.70]且未追过 → 追买那边 QTY 份。
    //  ② 谁跌补谁(计算模块):某边 ask≤补档[0.28,0.25,0.20]且未补过 → 算份额补那边——
    //     补主腿边→把"主腿赢"补到 target(12);补对侧→把"主腿输"补到 −maxloss(−7)。
    //  ③ 锁住即停:一旦"主腿赢≥target 且 主腿输≥−maxloss"(两结算情景都达标),停止下单裸持。
    // 计算模块公式:补 side 边 q 份后该边结算指标 =(side份额−总成本)+q·(1−fc(价))。
    //   令其=目标 → q=(目标−当前指标)/(1−fc(价))。补不齐(对侧没跌够/主腿没涨够)则尽力而为。
    pub(crate) async fn decide_accum(
        &mut self,
        market: &Market,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        if now < market.start_ts { return Ok(()); }                          // 还没开盘
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        if self.config.accum_late_cascade_enabled {
            return self.decide_accum_late_cascade(market, up_ask, dn_ask, seconds_left, mode).await;
        }
        if seconds_left <= self.config.accum_force_seconds { return Ok(()); } // 临近结算停建
        let qty = strategy_order_shares(self.config.accum_qty).unwrap_or(20.0);
        let target = self.config.accum_target_win;
        let maxloss = self.config.accum_max_loss;

        // ── 首笔:z 定主腿方向,只 BUY ──
        if !self.accum.contains_key(&market.slug) {
            let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
            if price_to_beat < 1000.0 { return Ok(()); }       // 开盘 Chainlink 价未就绪
            let Some(sig) = self.model.compute(price_to_beat, seconds_left, crate::zscore::DirSource::Chainlink) else { return Ok(()); };
            let z = self.config.accum_entry_z;
            let dir = if sig.z >= z { "Up" } else if sig.z <= -z { "Down" } else { return Ok(()); };
            let ask = if dir == "Up" { up_ask } else { dn_ask };
            info!("[ACCUM {mode}] {} 首笔 z={:.3}→{dir} 主腿@{ask:.3}×{qty:.0} T-{seconds_left}s",
                market.title, sig.z);
            self.accum_buy(market, dir, ask, qty, "accum_first", price_to_beat).await?;
            self.accum.insert(market.slug.clone(), AccumLeg {
                main_dir: dir.to_string(),
                up_chase: Vec::new(), dn_chase: Vec::new(),
                up_dip: Vec::new(), dn_dip: Vec::new(), locked: false, rescued: false,
            });
            return Ok(());
        }

        let leg = self.accum.get(&market.slug).unwrap().clone();
        if leg.locked { return Ok(()); }                       // 已锁住,不再下单
        let main_dir = leg.main_dir.clone();
        let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
        let chase = self.config.accum_chase_levels.clone();
        let dip = self.config.accum_dip_levels.clone();
        let (up_chase, dn_chase) = (leg.up_chase.clone(), leg.dn_chase.clone());
        let (up_dip, dn_dip) = (leg.up_dip.clone(), leg.dn_dip.clone());

        // 进 tick 先判锁住(可能上 tick 刚好达标)
        let (wm, wo) = self.accum_pnl(&market.slug, market.end_ts, &main_dir);
        if wm >= target && wo >= -maxloss {
            if let Some(l) = self.accum.get_mut(&market.slug) { l.locked = true; }
            info!("[ACCUM {mode}] {} 盈亏锁住(主腿赢{wm:+.1}≥{target:.0} 主腿输{wo:+.1}≥{:.0}),停止下单裸持 T-{seconds_left}s",
                market.title, -maxloss);
            return Ok(());
        }

        // ① 谁涨追谁:Up/Down 两边,ask≥追涨档且未追过 → 追买 qty 份
        for side in ["Up", "Down"] {
            let side_ask = if side == "Up" { up_ask } else { dn_ask };
            let chased = if side == "Up" { &up_chase } else { &dn_chase };
            for (k, &lv) in chase.iter().enumerate() {
                if chased.contains(&k) || side_ask < lv { continue; }
                info!("[ACCUM {mode}] {} 追涨{side}#{k}(ask{side_ask:.3}≥{lv:.2})×{qty:.0} T-{seconds_left}s",
                    market.title);
                self.accum_buy(market, side, side_ask, qty, "accum_chase", price_to_beat).await?;
                if let Some(l) = self.accum.get_mut(&market.slug) {
                    if side == "Up" { l.up_chase.push(k); } else { l.dn_chase.push(k); }
                }
                let (wm, wo) = self.accum_pnl(&market.slug, market.end_ts, &main_dir);
                if wm >= target && wo >= -maxloss {
                    if let Some(l) = self.accum.get_mut(&market.slug) { l.locked = true; }
                    info!("[ACCUM {mode}] {} 盈亏锁住,停止下单裸持 T-{seconds_left}s", market.title);
                    return Ok(());
                }
            }
        }

        // ② 晚场顺势补救(临结算收敛,优先于 dip):剩余<rescue_secs、未补救过、某边 ask 进收敛带
        //    (0.78-0.83)→ 市场已选定该边(6/6回测:未锁盘到此无一不收敛,该边赢≈88%)。
        //    分笔顺势补强势边到"该边赢结算>rescue_goal"(每笔20+零头,间隔500ms,动态重算),
        //    补完即 locked 停手裸持——押定这边、不再 dip 补反向主腿(否则两边对冲互抵,见21:50盘bug)。
        //    放大下注(正EV×杠杆):命中赚/翻盘亏更大,靠88%命中撑——多天验证为先。
        if !leg.rescued && seconds_left < self.config.accum_rescue_secs {
            let (lo, hi) = (self.config.accum_rescue_lo, self.config.accum_rescue_hi);
            let fired = if up_ask > lo && up_ask < hi { Some(("Up", up_ask)) }
                        else if dn_ask > lo && dn_ask < hi { Some(("Down", dn_ask)) }
                        else { None };
            if let Some((side, p)) = fired {
                if let Some(l) = self.accum.get_mut(&market.slug) { l.rescued = true; }   // 每盘只补救一次
                let denom = 1.0 - full_cost_per_share(p);
                if denom > 0.001 {
                    let goal = self.config.accum_rescue_goal;
                    // 分笔补:每笔最多 qty,不足 1 份停止;FOK 不接受部分成交零头。
                    for _step in 0..50 {
                        let cur = {
                            let pos = self.state.get_or_create(&market.slug, market.end_ts);
                            if side == "Up" { pos.pnl_if_up_wins() } else { pos.pnl_if_down_wins() }
                        };
                        let need = ((goal - cur) / denom).max(0.0).ceil();
                        if need < 1.0 { break; }
                        let this = need.min(qty);
                        info!("[ACCUM {mode}] {} 晚场补救:顺势补{side}@{p:.3}×{this:.0}份(剩需{need:.0},该边赢→>{goal:.0}) T-{seconds_left}s",
                            market.title);
                        self.accum_buy(market, side, p, this, "accum_rescue", price_to_beat).await?;
                        if need > qty {
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                        }
                    }
                }
                // 押定强势边,停止一切后续下单裸持到结算(不再 dip 补反向主腿)
                if let Some(l) = self.accum.get_mut(&market.slug) { l.locked = true; }
                info!("[ACCUM {mode}] {} 补救完成,押定{side}停止下单裸持 T-{seconds_left}s", market.title);
                return Ok(());
            }
        }

        // ③ 谁跌补谁(计算模块):Up/Down 两边,ask≤补档且未补过 → 分笔补。
        //    每笔最多 qty(20)份、最后一笔补不足 20 的零头,笔间隔 500ms。
        //    每笔后重算需求(动态收敛):实盘 FOK 保证整数份额,失败则下轮重试。
        for side in ["Up", "Down"] {
            let side_ask = if side == "Up" { up_ask } else { dn_ask };
            let dipped = if side == "Up" { &up_dip } else { &dn_dip };
            for (j, &lv) in dip.iter().enumerate() {
                if dipped.contains(&j) || side_ask > lv { continue; }
                if let Some(l) = self.accum.get_mut(&market.slug) {
                    if side == "Up" { l.up_dip.push(j); } else { l.dn_dip.push(j); }
                }
                // 分笔补:循环算"还差多少到达标",每笔补 min(剩余, 20),笔间隔 500ms。
                for _step in 0..50 {                           // 上限50笔(1000份),防异常死循环
                    let need = self.accum_calc_qty(&market.slug, market.end_ts, &main_dir, side, side_ask, target, maxloss);
                    if need < 1.0 { break; }                   // 已达标/无需再补
                    let this = need.min(qty);                  // 每笔最多20,最后一笔=零头
                    info!("[ACCUM {mode}] {} 补{side}#{j}(ask{side_ask:.3}≤{lv:.2}) ×{this:.0}份(剩需{need:.0}) T-{seconds_left}s",
                        market.title);
                    self.accum_buy(market, side, side_ask, this, "accum_dip", price_to_beat).await?;
                    let (wm, wo) = self.accum_pnl(&market.slug, market.end_ts, &main_dir);
                    if wm >= target && wo >= -maxloss {
                        if let Some(l) = self.accum.get_mut(&market.slug) { l.locked = true; }
                        info!("[ACCUM {mode}] {} 盈亏锁住,停止下单裸持 T-{seconds_left}s", market.title);
                        return Ok(());
                    }
                    if need > qty {                            // 还要补,等 500ms 让盘口恢复
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    }
                }
            }
        }

        Ok(())
    }

    async fn decide_accum_late_cascade(&mut self, market: &Market, up_ask: f64, dn_ask: f64,
                            seconds_left: i64, mode: &str) -> Result<()> {
        let price_to_beat = self.model.chainlink_at(market.start_ts)
            .or_else(|| self.model.chainlink_latest())
            .unwrap_or(0.0);

        if !self.accum.contains_key(&market.slug) {
            if let Some(pos) = self.state.get(&market.slug) {
                if !pos.trades.is_empty() {
                    let cascade_done = pos.trades.iter()
                        .any(|t| t.phase.starts_with("accum_late_cascade_confirm"));
                    let cascade_base = pos.trades.iter()
                        .all(|t| t.phase.starts_with("accum_late_cascade"));
                    let main_dir = if pos.up_shares >= pos.down_shares { "Up" } else { "Down" };
                    self.accum.insert(market.slug.clone(), AccumLeg {
                        main_dir: main_dir.to_string(),
                        up_chase: Vec::new(), dn_chase: Vec::new(),
                        up_dip: Vec::new(), dn_dip: Vec::new(),
                        locked: cascade_done || !cascade_base,
                        rescued: false,
                    });
                    self.write_signal(&serde_json::json!({
                        "phase": "accum_late_cascade_resume_existing",
                        "market": market.slug,
                        "existing_trades": pos.trades.len(),
                        "up_shares": pos.up_shares,
                        "down_shares": pos.down_shares,
                        "locked": cascade_done || !cascade_base,
                        "seconds_left": seconds_left,
                        "ts": chrono::Utc::now().timestamp(),
                    })).await?;
                    return Ok(());
                }
            }

            let base_usdc = self.config.accum_late_cascade_base_usdc.max(1.0);
            let up_q = Self::accum_shares_for_usdc(up_ask, base_usdc);
            let dn_q = Self::accum_shares_for_usdc(dn_ask, base_usdc);
            let (up_q, dn_q) = if self.config.accum_late_cascade_base_equal_shares {
                let q = up_q.max(dn_q);
                (q, q)
            } else {
                (up_q, dn_q)
            };
            if up_q < 1.0 || dn_q < 1.0 { return Ok(()); }

            let base_cost = full_cost_per_share(up_ask) * up_q + full_cost_per_share(dn_ask) * dn_q;
            if self.config.accum_max_exposure > 0.0 && base_cost > self.config.accum_max_exposure {
                self.write_signal(&serde_json::json!({
                    "phase": "accum_late_cascade_base_block",
                    "market": market.slug,
                    "reason": "base_cost_exceeds_max_exposure",
                    "base_cost": base_cost,
                    "max_exposure": self.config.accum_max_exposure,
                    "up_ask": up_ask,
                    "dn_ask": dn_ask,
                    "up_shares": up_q,
                    "down_shares": dn_q,
                    "seconds_left": seconds_left,
                    "ts": chrono::Utc::now().timestamp(),
                })).await?;
                return Ok(());
            }

            info!("[ACCUM {mode}] {} late-cascade base Up@{up_ask:.3}×{up_q:.0} Down@{dn_ask:.3}×{dn_q:.0} T-{seconds_left}s",
                market.title);
            self.accum_buy(market, "Up", up_ask, up_q, "accum_late_cascade_base", price_to_beat).await?;
            self.accum_buy(market, "Down", dn_ask, dn_q, "accum_late_cascade_base", price_to_beat).await?;
            let main_dir = if up_ask >= dn_ask { "Up" } else { "Down" };
            self.accum.insert(market.slug.clone(), AccumLeg {
                main_dir: main_dir.to_string(),
                up_chase: Vec::new(), dn_chase: Vec::new(),
                up_dip: Vec::new(), dn_dip: Vec::new(),
                locked: false, rescued: false,
            });
            return Ok(());
        }

        let leg = self.accum.get(&market.slug).unwrap().clone();
        if leg.locked { return Ok(()); }

        let Some((rule_name, bin_idx, main, main_px, hedge, hedge_px, target_req)) =
            Self::accum_late_cascade_rule(seconds_left, up_ask, dn_ask,
                                          self.config.accum_late_cascade_qty)
        else { return Ok(()); };

        let target_main = Self::accum_shares_for_request(
            main_px, target_req, self.config.accum_late_cascade_base_usdc.max(1.0));
        let (main_now, hedge_now, exposure_now) = {
            let pos = self.state.get_or_create(&market.slug, market.end_ts);
            let main_now = if main == "Up" { pos.up_shares } else { pos.down_shares };
            let hedge_now = if hedge == "Up" { pos.up_shares } else { pos.down_shares };
            (main_now, hedge_now, pos.up_cost_total + pos.down_cost_total)
        };
        let add_main = (target_main - main_now).max(0.0).ceil();
        let main_after = main_now + add_main;
        let target_hedge = (main_after * self.config.accum_late_cascade_hedge_frac - 1e-9).ceil();
        let add_hedge = (target_hedge - hedge_now).max(0.0).ceil();
        if add_main < 1.0 && add_hedge < 1.0 {
            if let Some(l) = self.accum.get_mut(&market.slug) { l.locked = true; }
            return Ok(());
        }

        let add_cost = full_cost_per_share(main_px) * add_main
            + full_cost_per_share(hedge_px) * add_hedge;
        let projected_exposure = exposure_now + add_cost;
        if self.config.accum_max_exposure > 0.0 && projected_exposure > self.config.accum_max_exposure {
            self.write_signal(&serde_json::json!({
                "phase": "accum_late_cascade_block",
                "market": market.slug,
                "rule": rule_name,
                "bin": bin_idx,
                "reason": "projected_exposure_exceeds_max",
                "exposure": exposure_now,
                "add_cost": add_cost,
                "projected_exposure": projected_exposure,
                "max_exposure": self.config.accum_max_exposure,
                "main": main,
                "main_px": main_px,
                "add_main": add_main,
                "hedge": hedge,
                "hedge_px": hedge_px,
                "add_hedge": add_hedge,
                "seconds_left": seconds_left,
                "ts": chrono::Utc::now().timestamp(),
            })).await?;
            return Ok(());
        }

        info!("[ACCUM {mode}] {} late-cascade {rule_name} bin#{bin_idx} main {main}@{main_px:.3} add×{add_main:.0}, hedge {hedge}@{hedge_px:.3} add×{add_hedge:.0} T-{seconds_left}s",
            market.title);
        if add_main >= 1.0 {
            self.accum_buy(market, main, main_px, add_main, "accum_late_cascade_confirm_main", price_to_beat).await?;
        }
        if add_hedge >= 1.0 {
            self.accum_buy(market, hedge, hedge_px, add_hedge, "accum_late_cascade_confirm_hedge", price_to_beat).await?;
        }
        if let Some(l) = self.accum.get_mut(&market.slug) {
            l.main_dir = main.to_string();
            l.locked = true;
        }
        Ok(())
    }

    fn accum_late_cascade_rule(seconds_left: i64, up_ask: f64, dn_ask: f64, qty: f64)
        -> Option<(&'static str, usize, &'static str, f64, &'static str, f64, f64)> {
        let (main, main_px, hedge, hedge_px) = if up_ask >= dn_ask {
            ("Up", up_ask, "Down", dn_ask)
        } else {
            ("Down", dn_ask, "Up", up_ask)
        };
        let spread = main_px - hedge_px;
        if seconds_left <= 30 && hedge_px <= 0.20 && spread >= 0.70 {
            if let Some(bin) =
                Self::accum_price_bin(main_px, &[(0.85, 0.87), (0.87, 0.88), (0.94, 0.96)])
            {
                return Some(("C30", bin, main, main_px, hedge, hedge_px, qty));
            }
        }
        if seconds_left <= 25 && hedge_px <= 0.20 && spread >= 0.70 {
            if let Some(bin) = Self::accum_price_bin(main_px, &[(0.85, 0.87), (0.88, 0.92)]) {
                return Some(("C25", bin, main, main_px, hedge, hedge_px, qty));
            }
        }
        if seconds_left <= 10 && hedge_px <= 0.15 && spread >= 0.80 {
            if let Some(bin) = Self::accum_price_bin(main_px, &[(0.90, 0.92), (0.96, 0.985)]) {
                return Some(("C10", bin, main, main_px, hedge, hedge_px, qty));
            }
        }
        None
    }

    fn accum_price_bin(price: f64, bins: &[(f64, f64)]) -> Option<usize> {
        bins.iter().enumerate()
            .find(|(i, (lo, hi))| {
                (*lo <= price && price < *hi)
                    || (*i == bins.len() - 1 && *lo <= price && price <= *hi)
            })
            .map(|(i, _)| i)
    }

    fn accum_shares_for_usdc(price: f64, usdc: f64) -> f64 {
        if price <= 0.0 { return 0.0; }
        (usdc / price - 1e-9).ceil().max(1.0)
    }

    fn accum_shares_for_request(price: f64, shares: f64, min_order_usdc: f64) -> f64 {
        shares.max(Self::accum_shares_for_usdc(price, min_order_usdc))
    }

    /// 当前两个结算情景的 PnL:返回 (主腿方向赢, 主腿方向输)。
    /// 用户口径(输方归零只损本金):直接复用 position 的 pnl_if_*_wins。
    pub(crate) fn accum_pnl(&mut self, slug: &str, end_ts: i64, main_dir: &str) -> (f64, f64) {
        let pos = self.state.get_or_create(slug, end_ts);
        if main_dir == "Up" {
            (pos.pnl_if_up_wins(), pos.pnl_if_down_wins())
        } else {
            (pos.pnl_if_down_wins(), pos.pnl_if_up_wins())
        }
    }

    /// 计算模块:补 `side` 边到对应目标所需的份额(向上取整补够,≤0 返回 0)。
    /// 补 side 边后"该边赢"的结算指标 += q·(1−fc(价));令其=目标解 q。
    /// 指标 = pnl_if_<side>_wins(用户口径:该边含费、对侧只本金)。
    /// side==主腿 → 目标=target(主腿赢);side==对侧 → 目标=−maxloss(主腿输)。
    pub(crate) fn accum_calc_qty(&mut self, slug: &str, end_ts: i64, main_dir: &str, side: &str,
                      price: f64, target: f64, maxloss: f64) -> f64 {
        let pos = self.state.get_or_create(slug, end_ts);
        let cur = if side == "Up" { pos.pnl_if_up_wins() } else { pos.pnl_if_down_wins() };
        let denom = 1.0 - full_cost_per_share(price);
        if denom <= 0.001 { return 0.0; }                      // 价格过高,补也无效
        let goal = if side == main_dir { target } else { -maxloss };
        ((goal - cur) / denom).max(0.0).ceil()                 // ceil 补够,不让 round 少补
    }

    /// accum 专用下单 + 双轨记账(FOK,只接受整数份额整单成交)。
    pub(crate) async fn accum_buy(&mut self, market: &Market, dir: &str, price: f64, shares: f64,
                       label: &str, price_to_beat: f64) -> Result<()> {
        let Some(shares) = strategy_order_shares(shares) else {
            warn!("[ACCUM] {} {dir} {label} 下单份额非法: {shares}", market.title);
            return Ok(());
        };
        // 纯按开关:maker 模式下 accum 补仓也挂 maker 单(收割由 harvest_makers 处理)
        if self.config.order_mode == "maker" {
            return self.do_buy_maker(market, dir, price, shares, label, price_to_beat).await.map(|_| ());
        }
        let Some(token) = market.token_for(dir) else {
            warn!("[ACCUM] {} 找不到 {dir} 的 token_id,跳过", market.title);
            return Ok(());
        };
        // audit:决策要下单、真正发单前记 intent。
        self.write_signal(&serde_json::json!({
            "phase": "intent", "market": market.slug,
            "direction": dir, "shares": shares, "price": price,
            "label": label, "mode": self.config.order_mode,
            "ts": chrono::Utc::now().timestamp(),
        })).await?;
        // 吃单类型按开关:"fak"=FAK(部分成交也要,对累积建仓尤其合适);其余=FOK。
        let fill_res = if self.config.order_mode == "fak" {
            self.executor.buy_fak(token, price, shares, None).await
        } else {
            self.executor.buy(token, price, shares, None).await
        };
        let fill = match fill_res {
            Ok(f) => f,
            Err(e) => { warn!("[ACCUM ORDER ERR] {} {dir} {label}: {e:#}", market.title); return Ok(()); }
        };
        // audit:executor 返回后记 submit。
        self.write_signal(&serde_json::json!({
            "phase": "submit", "order_id": fill.order_id, "success": fill.success,
            "filled_shares": fill.filled_shares, "filled_price": fill.filled_price,
            "market": market.slug, "direction": dir,
            "ts": chrono::Utc::now().timestamp(),
        })).await?;
        if !fill.simulated {
            info!("[ACCUM ORDER] {} {dir} {label} id={} status={} ok={} 成交{:.1}份@{:.3}",
                market.title, fill.order_id, fill.status, fill.success, fill.filled_shares, fill.filled_price);
        }
        // A轨 影子账(仅实盘)
        if !self.config.dry_run {
            record_trade(&mut self.ideal_state, market, dir, price, shares, label, price_to_beat, false);
            self.ideal_state.save().await?;
        }
        // B轨 真实账:只记真正成交的份额
        if !fill.success || fill.filled_shares <= 0.0 { return Ok(()); }
        let (rp, rs) = (fill.filled_price, fill.filled_shares);
        self.write_signal(&serde_json::json!({
            "phase": label, "market": market.slug,
            "direction": dir, "price": rp, "shares": rs,
            "full_cost": full_cost_per_share(rp),
            "dry_run": self.config.dry_run, "ts": chrono::Utc::now().timestamp(),
        })).await?;
        record_trade(&mut self.state, market, dir, rp, rs, label, price_to_beat, false);
        self.state.save().await?;
        Ok(())
    }
}
