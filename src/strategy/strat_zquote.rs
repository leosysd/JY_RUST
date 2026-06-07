use super::smart::{SmartStrategy, ENTRY_MIN_SECONDS_LEFT};
use crate::clob::Market;
use anyhow::Result;
use tracing::{debug, info};

impl SmartStrategy {
    // ── 路线七:zquote z定方向 + 双边固定价 maker 挂单 ─────────────────────
    //
    // z-score 定方向。z 看好那边挂稍高价(zquote_up_px,默认0.52)争成交;
    // 反向挂稍低价(zquote_dn_px,默认0.488)捡便宜。双边都挂 GTC maker 单等成交,
    // 由 maker lifecycle(TTL/盘末撤,固定报价跳过盘口移动撤单)管理。
    // 去重保证同方向不重复挂,故每 tick 调用安全。
    pub(crate) async fn decide_zquote(
        &mut self,
        market: &Market,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        // 入场时机:太晚不挂(留出成交窗口)。
        if seconds_left < ENTRY_MIN_SECONDS_LEFT { return Ok(()); }

        // 必须用真开盘价(start_ts处的chainlink),取不到就跳过(不退回最新价)。
        let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
        if price_to_beat < 1000.0 {
            debug!("[ZQUOTE] {} 无真开盘价,跳过", market.title);
            return Ok(());
        }

        // z 方向:信号不足不挂。
        let Some(sig) = self.model.compute(price_to_beat, seconds_left) else { return Ok(()); };
        let Some(dir) = sig.direction() else {
            debug!("[ZQUOTE] {} z={:.3} 信号不足,不挂", market.title, sig.z);
            return Ok(());
        };
        let opp_dir = if dir == "Up" { "Down" } else { "Up" };

        // 两边对应的当前 ask:post-only 买单价必须 < ask,否则 "crosses book" 被拒。
        let dir_ask = if dir == "Up" { up_ask } else { dn_ask };
        let opp_ask = if opp_dir == "Up" { up_ask } else { dn_ask };
        // 挂价 = 目标价与 (ask-1tick) 取低,确保 < ask 不 crosses;ask 无效(盘口未就绪)用目标价。
        let up_px = if dir_ask > 0.011 { self.config.zquote_up_px.min(dir_ask - 0.01) } else { self.config.zquote_up_px };
        let dn_px = if opp_ask > 0.011 { self.config.zquote_dn_px.min(opp_ask - 0.01) } else { self.config.zquote_dn_px };
        let up_px = up_px.clamp(0.01, 0.99);
        let dn_px = dn_px.clamp(0.01, 0.99);
        let shares = self.order_shares();
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        info!(
            "[ZQUOTE {mode}] {} z={:.3}→{dir} 挂{dir}@{up_px:.3} / {opp_dir}@{dn_px:.3} ×{shares:.0}份  T-{seconds_left}s",
            market.title, sig.z
        );

        // 双边各挂一次(去重保证同方向不重复)。phase_label 必须以 "zquote" 开头,
        // 才能让 maker lifecycle 跳过盘口移动撤单(只 TTL+盘末)。
        self.do_buy_maker_at(market, dir, up_px, shares, "zquote_dir", price_to_beat).await?;
        self.do_buy_maker_at(market, opp_dir, dn_px, shares, "zquote_opp", price_to_beat).await?;
        Ok(())
    }
}
