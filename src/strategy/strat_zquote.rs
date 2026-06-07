use super::smart::{SmartStrategy, ENTRY_MIN_SECONDS_LEFT};
use crate::clob::Market;
use anyhow::Result;
use tracing::debug;

impl SmartStrategy {
    // ── 路线七:zquote z定方向 + 双边固定价 maker 挂单,买够即收手 ──────────
    //
    // z 定方向。z 看好那边挂固定价 zquote_up_px(默认0.54)、反向挂 zquote_dn_px(默认0.48)。
    // 各边买够 order_shares 份就**收手**(不再挂)。固定价、不追跌。
    // crosses book(对边太便宜时 post-only 被拒)属正常保护:不去追买要输的便宜边。
    pub(crate) async fn decide_zquote(
        &mut self,
        market: &Market,
        _up_ask: f64,
        _dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        // 入场时机:太晚不挂(留出成交窗口)。
        if seconds_left < ENTRY_MIN_SECONDS_LEFT { return Ok(()); }

        // 必须用真开盘价(start_ts处的chainlink),取不到就跳过。
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

        // 目标固定份额:各边买够即收手。
        let target = self.order_shares();
        // 已成交份额(buy 到就收手)。
        let (up_filled, dn_filled) = self.state.get(&market.slug)
            .map(|p| (p.up_shares, p.down_shares))
            .unwrap_or((0.0, 0.0));
        let dir_filled = if dir == "Up" { up_filled } else { dn_filled };
        let opp_filled = if opp_dir == "Up" { up_filled } else { dn_filled };

        // 固定挂价(不追跌)。phase_label 以 "zquote" 开头 → lifecycle 跳过盘口移动撤单。
        let up_px = self.config.zquote_up_px; // z 看好侧,固定(默认 0.54)
        let dn_px = self.config.zquote_dn_px; // 反向侧,固定(默认 0.48)

        // z 看好那边:固定价挂单,买够 target 收手(去重/节流在 do_buy_maker_at 内)。
        if dir_filled + 0.5 < target {
            self.do_buy_maker_at(market, dir, up_px, target - dir_filled, "zquote_dir", price_to_beat).await?;
        }
        // 反向那边:固定价挂单,买够 target 收手。
        if opp_filled + 0.5 < target {
            self.do_buy_maker_at(market, opp_dir, dn_px, target - opp_filled, "zquote_opp", price_to_beat).await?;
        }
        Ok(())
    }
}
