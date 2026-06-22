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
        if now < market.start_ts {
            return Ok(());
        } // 还没开盘
        let qty = strategy_order_shares(self.config.accum_qty).unwrap_or(20.0);
        let target = self.config.accum_target_win;
        let maxloss = self.config.accum_max_loss;
        let mode = if self.config.dry_run {
            "DRY_RUN"
        } else {
            "LIVE"
        };
        let force_stop = seconds_left <= self.config.accum_force_seconds;
        let selector_enabled = self.accum_selector_enabled();

        // ── 首笔:z 定主腿方向,只 BUY ──
        if !self.accum.contains_key(&market.slug) {
            if force_stop {
                return Ok(());
            } // 临近结算不新开首笔
            let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
            if price_to_beat < 1000.0 {
                return Ok(());
            } // 开盘 Chainlink 价未就绪
            let Some(sig) = self.model.compute(
                price_to_beat,
                seconds_left,
                crate::zscore::DirSource::Chainlink,
            ) else {
                return Ok(());
            };
            let z = if selector_enabled {
                0.0
            } else {
                self.config.accum_entry_z
            };
            let dir = if sig.z >= z {
                "Up"
            } else if sig.z <= -z {
                "Down"
            } else {
                return Ok(());
            };
            let ask = if dir == "Up" { up_ask } else { dn_ask };
            info!(
                "[ACCUM {mode}] {} 首笔 z={:.3}→{dir} 主腿@{ask:.3}×{qty:.0} T-{seconds_left}s",
                market.title, sig.z
            );
            self.accum_buy(market, dir, ask, qty, "accum_first", price_to_beat)
                .await?;
            self.accum.insert(
                market.slug.clone(),
                AccumLeg::new(dir, up_ask, dn_ask, seconds_left),
            );
            if selector_enabled {
                let has_main = self
                    .state
                    .get(&market.slug)
                    .map(|pos| {
                        if dir == "Up" {
                            pos.up_shares
                        } else {
                            pos.down_shares
                        }
                    })
                    .unwrap_or(0.0)
                    >= 1.0;
                if has_main {
                    let cover = if dir == "Up" { "Down" } else { "Up" };
                    let cover_ask = if cover == "Up" { up_ask } else { dn_ask };
                    info!("[ACCUM {mode}] {} selector最小对冲 {cover}@{cover_ask:.3} T-{seconds_left}s",
                        market.title);
                    self.accum_buy(market, cover, cover_ask, 1.0, "accum_cover", price_to_beat)
                        .await?;
                } else {
                    warn!(
                        "[ACCUM {mode}] {} selector首笔未成交,跳过最小对冲 T-{seconds_left}s",
                        market.title
                    );
                    self.accum.remove(&market.slug);
                }
            }
            return Ok(());
        }

        if selector_enabled {
            self.accum_selector_note_tick(&market.slug, up_ask, dn_ask, seconds_left);
        }
        let leg = self.accum.get(&market.slug).unwrap().clone();
        if leg.locked {
            return Ok(());
        } // 已锁住,不再下单
        let main_dir = leg.main_dir.clone();
        let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
        let chase = self.config.accum_chase_levels.clone();
        let dip = self.config.accum_dip_levels.clone();
        let (up_chase, dn_chase) = (leg.up_chase.clone(), leg.dn_chase.clone());
        let (up_dip, dn_dip) = (leg.up_dip.clone(), leg.dn_dip.clone());

        // 进 tick 先判锁住(可能上 tick 刚好达标)
        let (wm, wo) = self.accum_pnl(&market.slug, market.end_ts, &main_dir);
        if wm >= target && wo >= -maxloss {
            if let Some(l) = self.accum.get_mut(&market.slug) {
                l.locked = true;
            }
            info!("[ACCUM {mode}] {} 盈亏锁住(主腿赢{wm:+.1}≥{target:.0} 主腿输{wo:+.1}≥{:.0}),停止下单裸持 T-{seconds_left}s",
                market.title, -maxloss);
            return Ok(());
        }

        let selector_branch = if selector_enabled {
            let branch = self
                .accum_selector_branch(market, up_ask, dn_ask, seconds_left, price_to_beat)
                .await?;
            if branch.is_empty() {
                return Ok(());
            }
            if branch == "fallback" {
                return Ok(());
            }
            Some(branch)
        } else {
            None
        };

        // ① 谁涨追谁:Up/Down 两边,ask≥追涨档且未追过 → 追买 qty 份。
        // 临近结算 force_stop 只停普通建仓,不挡后面的 rescue。
        if !force_stop && !selector_enabled {
            for side in ["Up", "Down"] {
                let side_ask = if side == "Up" { up_ask } else { dn_ask };
                let chased = if side == "Up" { &up_chase } else { &dn_chase };
                for (k, &lv) in chase.iter().enumerate() {
                    if chased.contains(&k) || side_ask < lv {
                        continue;
                    }
                    info!("[ACCUM {mode}] {} 追涨{side}#{k}(ask{side_ask:.3}≥{lv:.2})×{qty:.0} T-{seconds_left}s",
                        market.title);
                    self.accum_buy(market, side, side_ask, qty, "accum_chase", price_to_beat)
                        .await?;
                    if let Some(l) = self.accum.get_mut(&market.slug) {
                        if side == "Up" {
                            l.up_chase.push(k);
                        } else {
                            l.dn_chase.push(k);
                        }
                    }
                    let (wm, wo) = self.accum_pnl(&market.slug, market.end_ts, &main_dir);
                    if wm >= target && wo >= -maxloss {
                        if let Some(l) = self.accum.get_mut(&market.slug) {
                            l.locked = true;
                        }
                        info!(
                            "[ACCUM {mode}] {} 盈亏锁住,停止下单裸持 T-{seconds_left}s",
                            market.title
                        );
                        return Ok(());
                    }
                }
            }
        }

        // ② 晚场顺势补救(临结算收敛,优先于 dip):min_left<剩余<rescue_secs、未补救过、某边 ask 进收敛带
        //    (0.78-0.83)→ 市场已选定该边(6/6回测:未锁盘到此无一不收敛,该边赢≈88%)。
        //    分笔顺势补强势边到"该边赢结算>rescue_goal",但受 rescue 份额上限和押错最坏亏损约束。
        //    补完即 locked 停手持有——押定这边、不再 dip 补反向主腿(否则两边对冲互抵,见21:50盘bug)。
        //    放大下注(正EV×杠杆):命中赚/翻盘亏更大,靠88%命中撑——多天验证为先。
        let selector_rescue = selector_branch
            .as_deref()
            .is_some_and(|b| matches!(b, "stable" | "recover"));
        let rescue_min_left = if selector_rescue {
            50
        } else {
            self.config.accum_rescue_min_seconds_left
        };
        let rescue_max_left = if selector_rescue { 98 } else { 0 };
        let rescue_secs = if selector_rescue {
            100
        } else {
            self.config.accum_rescue_secs
        };
        let rescue_time_ok = seconds_left < rescue_secs
            && (rescue_min_left <= 0 || seconds_left > rescue_min_left)
            && (rescue_max_left <= 0 || seconds_left <= rescue_max_left);
        if !leg.rescued && (selector_rescue || !selector_enabled) && seconds_left < rescue_secs {
            let (lo, hi) = if selector_rescue {
                (0.83, 0.86)
            } else {
                (self.config.accum_rescue_lo, self.config.accum_rescue_hi)
            };
            let fired = if up_ask > lo && up_ask < hi {
                Some(("Up", up_ask))
            } else if dn_ask > lo && dn_ask < hi {
                Some(("Down", dn_ask))
            } else {
                None
            };
            if let Some((side, p)) = fired {
                if let Some(l) = self.accum.get_mut(&market.slug) {
                    l.rescued = true;
                } // 每盘只补救一次
                if !rescue_time_ok {
                    if !selector_rescue {
                        return Ok(());
                    }
                    if let Some(l) = self.accum.get_mut(&market.slug) {
                        l.locked = true;
                    }
                    info!(
                        "[ACCUM {mode}] {} selector rescue窗口外冻结 {side}@{p:.3} T-{seconds_left}s",
                        market.title
                    );
                    return Ok(());
                }
                if selector_rescue
                    && self.accum_selector_rescue_whipsaw_blocked(&leg, side, p, seconds_left)
                {
                    if let Some(l) = self.accum.get_mut(&market.slug) {
                        l.locked = true;
                    }
                    info!(
                        "[ACCUM {mode}] {} selector rescue急拉回扫冻结 {side}@{p:.3} T-{seconds_left}s",
                        market.title
                    );
                    return Ok(());
                }
                if 1.0 - full_cost_per_share(p) > 0.001 {
                    let goal = if selector_rescue {
                        10.0
                    } else {
                        self.config.accum_rescue_goal
                    };
                    let rescue_max_worst_loss = if selector_rescue {
                        35.0
                    } else {
                        self.config.accum_rescue_max_worst_loss
                    };
                    let rescue_max_shares = if selector_rescue {
                        35.0
                    } else {
                        self.config.accum_rescue_max_shares
                    };
                    // 分笔补:最终份额=min(目标份额,风险份额,rescue份额上限,每笔qty)。
                    // 目标/风险都用真实结算 PnL 口径,避免旧逻辑低估输方手续费。
                    for _step in 0..50 {
                        let (target_need, risk_need, cap_left, allowed) = self.accum_rescue_qty(
                            &market.slug,
                            market.end_ts,
                            side,
                            p,
                            goal,
                            rescue_max_worst_loss,
                            rescue_max_shares,
                        );
                        if target_need < 1.0 || allowed < 1.0 {
                            break;
                        }
                        let this = allowed.min(qty);
                        let Some(order_shares) = self.accum_order_shares(p, this) else {
                            break;
                        };
                        if order_shares > allowed {
                            break;
                        }
                        info!("[ACCUM {mode}] {} 晚场补救:顺势补{side}@{p:.3}×{order_shares:.0}份(目标需{target_need:.0},风险允{risk_need:.0},cap余{cap_left:.0},该边赢→>{goal:.0}) T-{seconds_left}s",
                            market.title);
                        self.accum_buy(
                            market,
                            side,
                            p,
                            order_shares,
                            "accum_rescue",
                            price_to_beat,
                        )
                        .await?;
                        if target_need > order_shares && order_shares >= qty {
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                        }
                    }
                }
                // 押定强势边,停止一切后续下单持有到结算(不再 dip 补反向主腿)
                if let Some(l) = self.accum.get_mut(&market.slug) {
                    l.locked = true;
                }
                info!(
                    "[ACCUM {mode}] {} 补救完成,押定{side}停止下单持有 T-{seconds_left}s",
                    market.title
                );
                return Ok(());
            }
        }
        if force_stop {
            return Ok(());
        } // 临近结算不再 chase/dip/首笔,但上面的 rescue 已经有机会执行。

        // ③ 谁跌补谁(计算模块):Up/Down 两边,ask≤补档且未补过 → 分笔补。
        //    每笔最多 qty(20)份、最后一笔补不足 20 的零头,笔间隔 500ms。
        //    每笔后重算需求(动态收敛):实盘 FOK 保证整数份额,失败则下轮重试。
        for side in ["Up", "Down"] {
            let side_ask = if side == "Up" { up_ask } else { dn_ask };
            let dipped = if side == "Up" { &up_dip } else { &dn_dip };
            let selector_dip = selector_enabled
                && selector_branch.as_deref().is_some_and(|b| {
                    matches!(
                        b,
                        "stable" | "pullback" | "recover" | "weak" | "capitulation"
                    )
                });
            let selector_dip_levels = [self.config.accum_selector_dip_level];
            let active_dip: Vec<f64> = if selector_dip {
                selector_dip_levels.to_vec()
            } else {
                dip.clone()
            };
            for (j, &lv) in active_dip.iter().enumerate() {
                if dipped.contains(&j) || side_ask > lv {
                    continue;
                }
                if let Some(l) = self.accum.get_mut(&market.slug) {
                    if side == "Up" {
                        l.up_dip.push(j);
                    } else {
                        l.dn_dip.push(j);
                    }
                }
                // 分笔补:循环算"还差多少到达标",每笔补 min(剩余, 20),笔间隔 500ms。
                for _step in 0..50 {
                    // 上限50笔(1000份),防异常死循环
                    let need = self.accum_calc_qty(
                        &market.slug,
                        market.end_ts,
                        &main_dir,
                        side,
                        side_ask,
                        target,
                        maxloss,
                    );
                    if need < 1.0 {
                        break;
                    } // 已达标/无需再补
                    let this = need.min(qty); // 每笔最多20,最后一笔=零头
                    info!("[ACCUM {mode}] {} 补{side}#{j}(ask{side_ask:.3}≤{lv:.2}) ×{this:.0}份(剩需{need:.0}) T-{seconds_left}s",
                        market.title);
                    self.accum_buy(market, side, side_ask, this, "accum_dip", price_to_beat)
                        .await?;
                    let (wm, wo) = self.accum_pnl(&market.slug, market.end_ts, &main_dir);
                    if wm >= target && wo >= -maxloss {
                        if let Some(l) = self.accum.get_mut(&market.slug) {
                            l.locked = true;
                        }
                        info!(
                            "[ACCUM {mode}] {} 盈亏锁住,停止下单裸持 T-{seconds_left}s",
                            market.title
                        );
                        return Ok(());
                    }
                    if need > qty {
                        // 还要补,等 500ms 让盘口恢复
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    }
                }
            }
        }

        Ok(())
    }

    fn accum_selector_enabled(&self) -> bool {
        matches!(
            self.config.accum_selector_mode.as_str(),
            "weak" | "capitulation"
        )
    }

    fn accum_order_shares(&self, price: f64, shares: f64) -> Option<f64> {
        let mut q = strategy_order_shares(shares)?;
        if self.config.accum_min_order_usdc > 0.0 && price > 0.0 {
            q = q.max((self.config.accum_min_order_usdc / price - 1e-9).ceil());
        }
        Some(q)
    }

    fn accum_selector_note_tick(
        &mut self,
        slug: &str,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) {
        if let Some(leg) = self.accum.get_mut(slug) {
            let main_ask = if leg.main_dir == "Up" { up_ask } else { dn_ask };
            leg.path_max_main = leg.path_max_main.max(main_ask);
            leg.path_min_main = leg.path_min_main.min(main_ask);
            if leg
                .ask_history
                .last()
                .map(|(s, _, _)| *s != seconds_left)
                .unwrap_or(true)
            {
                leg.ask_history.push((seconds_left, up_ask, dn_ask));
                if leg.ask_history.len() > 400 {
                    let drop_n = leg.ask_history.len() - 400;
                    leg.ask_history.drain(0..drop_n);
                }
            }
        }
    }

    async fn accum_selector_branch(
        &mut self,
        market: &Market,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
        price_to_beat: f64,
    ) -> Result<String> {
        let current = self
            .accum
            .get(&market.slug)
            .map(|l| l.selector_branch.clone())
            .unwrap_or_default();
        if !current.is_empty() {
            return Ok(current);
        }
        if seconds_left > self.config.accum_selector_gate_seconds {
            return Ok(String::new());
        }

        let branch = if let Some(sig) = self.model.compute(
            price_to_beat,
            seconds_left,
            crate::zscore::DirSource::Chainlink,
        ) {
            let leg = self.accum.get(&market.slug).unwrap();
            let main_up = leg.main_dir == "Up";
            let obs_main = if main_up { up_ask } else { dn_ask };
            let obs_hedge = if main_up { dn_ask } else { up_ask };
            let obs_ask_sum = up_ask + dn_ask;
            let obs_gap = obs_main - obs_hedge;
            let main_delta = obs_main - leg.entry_main_ask;
            let obs_p_main = if main_up { sig.p_up } else { sig.p_down };
            let obs_z_dir = if main_up { sig.z } else { -sig.z };
            let branch = self.accum_selector_match_branch(
                leg,
                obs_main,
                obs_ask_sum,
                obs_gap,
                main_delta,
                obs_p_main,
                obs_z_dir,
            );
            self.write_signal(&serde_json::json!({
                "phase": "accum_selector",
                "market": market.slug,
                "branch": branch,
                "selector_mode": self.config.accum_selector_mode,
                "main": leg.main_dir,
                "entry_main_ask": leg.entry_main_ask,
                "entry_hedge_ask": leg.entry_hedge_ask,
                "entry_gap": leg.entry_gap,
                "obs_main_ask": obs_main,
                "obs_hedge_ask": obs_hedge,
                "obs_gap": obs_gap,
                "obs_p_main": obs_p_main,
                "obs_z_dir": obs_z_dir,
                "main_delta": main_delta,
                "path_max_main": leg.path_max_main,
                "seconds_left": seconds_left,
                "ts": chrono::Utc::now().timestamp(),
            }))
            .await?;
            branch
        } else {
            self.write_signal(&serde_json::json!({
                "phase": "accum_selector",
                "market": market.slug,
                "branch": "fallback",
                "selector_mode": self.config.accum_selector_mode,
                "reason": "z_unavailable_at_gate",
                "seconds_left": seconds_left,
                "ts": chrono::Utc::now().timestamp(),
            }))
            .await?;
            "fallback"
        };

        if let Some(leg) = self.accum.get_mut(&market.slug) {
            leg.selector_branch = branch.to_string();
        }
        info!(
            "[ACCUM {mode}] {} selector branch={} T-{seconds_left}s",
            market.title,
            branch,
            mode = if self.config.dry_run {
                "DRY_RUN"
            } else {
                "LIVE"
            }
        );
        Ok(branch.to_string())
    }

    fn accum_selector_match_branch(
        &self,
        leg: &AccumLeg,
        obs_main: f64,
        obs_ask_sum: f64,
        obs_gap: f64,
        main_delta: f64,
        obs_p_main: f64,
        obs_z_dir: f64,
    ) -> &'static str {
        if leg.entry_ask_sum <= 1.06
            && leg.entry_gap >= -0.15
            && obs_main >= 0.45
            && obs_ask_sum <= 1.06
            && obs_gap >= 0.02
            && main_delta >= -0.02
            && leg.path_max_main >= 0.58
        {
            return "stable";
        }
        if leg.entry_ask_sum <= 1.06
            && leg.entry_gap >= -0.05
            && (0.25..=0.45).contains(&obs_main)
            && obs_ask_sum <= 1.06
            && (-0.25..=-0.12).contains(&obs_gap)
            && main_delta >= -0.30
            && leg.path_max_main >= 0.58
        {
            return "pullback";
        }
        if leg.entry_ask_sum <= 1.06
            && (-0.45..=0.05).contains(&obs_gap)
            && main_delta >= -0.35
            && leg.path_max_main >= 0.52
            && obs_z_dir >= 0.50
        {
            return "recover";
        }
        if leg.entry_main_ask <= 0.52 && leg.entry_gap >= 0.0 && obs_p_main >= 0.582 {
            return "weak";
        }
        if self.config.accum_selector_mode == "capitulation"
            && (0.43..=0.52).contains(&leg.entry_main_ask)
            && main_delta >= -0.33
            && leg.path_max_main >= 0.50
            && obs_p_main <= 0.02
            && obs_z_dir <= -4.0
        {
            return "capitulation";
        }
        "fallback"
    }

    fn accum_selector_rescue_whipsaw_blocked(
        &self,
        leg: &AccumLeg,
        side: &str,
        price: f64,
        seconds_left: i64,
    ) -> bool {
        if price < 0.85 {
            return false;
        }
        let side_price = |up: f64, dn: f64| if side == "Up" { up } else { dn };
        let recent_max: Vec<f64> = leg
            .ask_history
            .iter()
            .filter(|(s, _, _)| *s >= seconds_left && *s <= seconds_left + 20)
            .map(|(_, up, dn)| side_price(*up, *dn))
            .collect();
        let recent_range: Vec<f64> = leg
            .ask_history
            .iter()
            .filter(|(s, _, _)| *s >= seconds_left && *s <= seconds_left + 30)
            .map(|(_, up, dn)| side_price(*up, *dn))
            .collect();
        let max_seen = recent_max.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let range = if recent_range.is_empty() {
            0.0
        } else {
            let hi = recent_range
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max);
            let lo = recent_range.iter().copied().fold(f64::INFINITY, f64::min);
            hi - lo
        };
        max_seen >= 0.90 && range >= 0.17
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
    pub(crate) fn accum_calc_qty(
        &mut self,
        slug: &str,
        end_ts: i64,
        main_dir: &str,
        side: &str,
        price: f64,
        target: f64,
        maxloss: f64,
    ) -> f64 {
        let pos = self.state.get_or_create(slug, end_ts);
        let cur = if side == "Up" {
            pos.pnl_if_up_wins()
        } else {
            pos.pnl_if_down_wins()
        };
        let denom = 1.0 - full_cost_per_share(price);
        if denom <= 0.001 {
            return 0.0;
        } // 价格过高,补也无效
        let goal = if side == main_dir { target } else { -maxloss };
        ((goal - cur) / denom).max(0.0).ceil() // ceil 补够,不让 round 少补
    }

    /// rescue 份额约束:
    /// 目标份额 = 补到 side 赢真实结算 PnL >= goal 所需份额;
    /// 风险份额 = 保证 side 输时真实结算 PnL >= -max_worst_loss 的最多可补份额;
    /// cap 剩余 = 本盘 rescue 阶段还允许补的份额。
    pub(crate) fn accum_rescue_qty(
        &mut self,
        slug: &str,
        end_ts: i64,
        side: &str,
        price: f64,
        goal: f64,
        max_worst_loss: f64,
        max_shares: f64,
    ) -> (f64, f64, f64, f64) {
        let pos = self.state.get_or_create(slug, end_ts);
        let full_cost = full_cost_per_share(price);
        let denom = 1.0 - full_cost;
        if denom <= 0.001 || full_cost <= 0.0 {
            return (0.0, 0.0, 0.0, 0.0);
        }

        let win_cur = pos.settle_pnl(side);
        let target_need = ((goal - win_cur) / denom).max(0.0).ceil();

        let risk_need = if max_worst_loss > 0.0 {
            let lose_side = if side == "Up" { "Down" } else { "Up" };
            let lose_cur = pos.settle_pnl(lose_side);
            ((lose_cur + max_worst_loss) / full_cost).max(0.0).floor()
        } else {
            f64::INFINITY
        };

        let rescue_done: f64 = pos
            .trades
            .iter()
            .filter(|t| t.phase == "accum_rescue")
            .map(|t| t.shares)
            .sum();
        let cap_left = if max_shares > 0.0 {
            (max_shares - rescue_done).max(0.0).floor()
        } else {
            f64::INFINITY
        };

        let allowed = target_need.min(risk_need).min(cap_left).floor();
        (target_need, risk_need, cap_left, allowed)
    }

    /// accum 专用下单 + 双轨记账(FOK,只接受整数份额整单成交)。
    pub(crate) async fn accum_buy(
        &mut self,
        market: &Market,
        dir: &str,
        price: f64,
        shares: f64,
        label: &str,
        price_to_beat: f64,
    ) -> Result<()> {
        let Some(shares) = self.accum_order_shares(price, shares) else {
            warn!(
                "[ACCUM] {} {dir} {label} 下单份额非法: {shares}",
                market.title
            );
            return Ok(());
        };
        // 纯按开关:maker 模式下 accum 补仓也挂 maker 单(收割由 harvest_makers 处理)
        if self.config.order_mode == "maker" {
            return self
                .do_buy_maker(market, dir, price, shares, label, price_to_beat)
                .await
                .map(|_| ());
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
        }))
        .await?;
        // 吃单类型按开关:"fak"=FAK(部分成交也要,对累积建仓尤其合适);其余=FOK。
        let fill_res = if self.config.order_mode == "fak" {
            self.executor.buy_fak(token, price, shares, None).await
        } else {
            self.executor.buy(token, price, shares, None).await
        };
        let fill = match fill_res {
            Ok(f) => f,
            Err(e) => {
                warn!("[ACCUM ORDER ERR] {} {dir} {label}: {e:#}", market.title);
                return Ok(());
            }
        };
        // audit:executor 返回后记 submit。
        self.write_signal(&serde_json::json!({
            "phase": "submit", "order_id": fill.order_id, "success": fill.success,
            "filled_shares": fill.filled_shares, "filled_price": fill.filled_price,
            "market": market.slug, "direction": dir,
            "ts": chrono::Utc::now().timestamp(),
        }))
        .await?;
        if !fill.simulated {
            info!(
                "[ACCUM ORDER] {} {dir} {label} id={} status={} ok={} 成交{:.1}份@{:.3}",
                market.title,
                fill.order_id,
                fill.status,
                fill.success,
                fill.filled_shares,
                fill.filled_price
            );
        }
        // A轨 影子账(仅实盘)
        if !self.config.dry_run {
            record_trade(
                &mut self.ideal_state,
                market,
                dir,
                price,
                shares,
                label,
                price_to_beat,
                false,
            );
            self.ideal_state.save().await?;
        }
        // B轨 真实账:只记真正成交的份额
        if !fill.success || fill.filled_shares <= 0.0 {
            return Ok(());
        }
        let (rp, rs) = (fill.filled_price, fill.filled_shares);
        self.write_signal(&serde_json::json!({
            "phase": label, "market": market.slug,
            "direction": dir, "price": rp, "shares": rs,
            "full_cost": full_cost_per_share(rp),
            "dry_run": self.config.dry_run, "ts": chrono::Utc::now().timestamp(),
        }))
        .await?;
        record_trade(
            &mut self.state,
            market,
            dir,
            rp,
            rs,
            label,
            price_to_beat,
            false,
        );
        self.state.save().await?;
        Ok(())
    }
}
