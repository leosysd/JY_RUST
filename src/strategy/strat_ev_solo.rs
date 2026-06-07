use super::smart::{strategy_order_shares, SmartStrategy};
use crate::clob::Market;
use crate::position::{full_cost_per_share, MarketPosition, Phase};
use anyhow::Result;
use tracing::{debug, info};

impl SmartStrategy {
    // ── 路线四：ev_solo 纯单边裸持（数学上唯一正期望路径）──────────────────────
    //
    // z-score 定方向 → 只买该边、不对冲、不锁利、不止损 → 裸持到结算。
    // 依据: 154场实测 z-score 方向胜率 57.8%(>50%有edge)。
    // 数学: 对冲腿在7%费下每份边际EV必<0(已证明),故彻底单边。
    // EV/份 = 胜率×(1-fc) - (1-胜率)×fc。仅在 ev_solo_min_ask≤ask≤max_ask 时入场。
    // 纯记录 entry_signal(含z/价/方向),结算后 join winner 验证胜率是否稳。
    pub(crate) async fn decide_ev_solo(
        &mut self,
        market: &Market,
        pos: MarketPosition,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        // 只在 Waiting 时入场;入场后裸持(Holding/Locked 不做任何动作)
        if !matches!(pos.phase, Phase::Waiting) { return Ok(()); }
        if seconds_left < self.config.ev_solo_min_seconds_left { return Ok(()); }

        // 真开盘价取不到则跳过(不退回最新价,见 decide_waiting 注释)。
        let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
        if price_to_beat < 1000.0 { return Ok(()); }

        let Some(sig) = self.model.compute(price_to_beat, seconds_left) else { return Ok(()); };
        let Some(dir) = sig.direction() else {
            debug!("[EV_SOLO] {} z={:.3} 信号不足,不入场", market.title, sig.z);
            return Ok(());
        };
        let ask = if dir == "Up" { up_ask } else { dn_ask };
        // 价位过滤:只在 [min,max] 入场(避开贵价负EV区 + 过低赔率差区)
        if ask < self.config.ev_solo_min_ask || ask > self.config.ev_solo_max_ask {
            debug!("[EV_SOLO] {} {dir}@{ask:.3} 不在入场区[{:.2},{:.2}],跳过 z={:.3}",
                market.title, self.config.ev_solo_min_ask, self.config.ev_solo_max_ask, sig.z);
            return Ok(());
        }

        // flow_imb_30 同向闸:开盘前30s资金流与 z 方向明确相悖(signed<0)则跳过。
        // 实测该信号一致59%/矛盾51%,是目前最有方向预测力的单特征(AUC 0.528)。
        if self.config.ev_solo_flow_gate {
            let now = chrono::Utc::now().timestamp();
            let imb = self.model.binance_flow(now, 30).imbalance;
            let signed = if dir == "Up" { imb } else { -imb };
            if signed < 0.0 {
                debug!("[EV_SOLO] {} {dir} flow闸拦截:flow_imb_30={imb:.3} 反向,跳过 z={:.3}",
                    market.title, sig.z);
                return Ok(());
            }
        }

        let qty = strategy_order_shares(self.config.ev_solo_qty).unwrap_or(20.0);
        let fc = full_cost_per_share(ask);
        let ev_per = 0.578 * (1.0 - fc) - 0.422 * fc; // 用实测胜率估每份EV(仅日志参考)
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        info!("[EV_SOLO {mode}] {} 单边买{dir}@{ask:.3}×{qty:.0} z={:.3} 估EV{ev_per:+.3}/份 T-{seconds_left}s",
            market.title, sig.z);

        // 记录入场信号(丰富特征,为 LightGBM 铺路;结算后 join winner 作训练标签)
        let mut feat = self.build_features(&sig, dir, ask, up_ask, dn_ask, seconds_left);
        self.add_book_depth(&mut feat, market).await;
        feat["phase"] = serde_json::json!("entry_signal");
        feat["market"] = serde_json::json!(market.slug);
        feat["strategy"] = serde_json::json!("ev_solo");
        self.write_signal(&feat).await?;

        // 买单边,然后标 Locked 裸持到结算(不进任何后续决策)
        if self.do_buy(&market, dir, ask, qty, "ev_solo", price_to_beat).await? {
            let p = self.state.get_or_create(&market.slug, market.end_ts);
            p.phase = Phase::Locked;
            self.state.save().await?;
        }
        Ok(())
    }
}
