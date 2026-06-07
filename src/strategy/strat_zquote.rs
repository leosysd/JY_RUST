use super::smart::{SmartStrategy, ENTRY_MIN_SECONDS_LEFT};
use crate::clob::Market;
use anyhow::Result;
use tracing::{debug, info};

impl SmartStrategy {
    // ── 路线七:zquote z定方向(开局锁定) + 双边挂单(看订单簿≤上限),买够即收手 ──
    //
    // z 开局只定一次方向并锁定到该盘(之后不随 z 重算翻转)。
    // 挂价"看订单簿":best_ask − 1 tick(尽量贴近成交),但封顶——看好侧 ≤ zquote_up_px(0.54)、
    // 反向侧 ≤ zquote_dn_px(0.48)。各边买够 order_shares 份就收手(不再挂)。
    // **收手是关键**:防止下跌时挂价跟跌、无限追买(之前买到 145 份就是动态挂价但没收手)。
    pub(crate) async fn decide_zquote(
        &mut self,
        market: &Market,
        up_ask: f64,
        dn_ask: f64,
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

        // z 开局只定一次方向,锁定到该盘(之后用锁定方向,不再随 z 翻转)。
        let dir: String = if let Some(d) = self.zquote_dir.get(&market.slug) {
            d.clone()
        } else {
            let Some(sig) = self.model.compute(price_to_beat, seconds_left, crate::zscore::DirSource::Chainlink) else { return Ok(()); };
            let Some(d) = sig.direction() else {
                debug!("[ZQUOTE] {} z={:.3} 信号不足,不挂", market.title, sig.z);
                return Ok(());
            };
            self.zquote_dir.insert(market.slug.clone(), d.to_string());
            info!("[ZQUOTE] {} 开局锁定方向={d}(z={:.3})", market.title, sig.z);
            d.to_string()
        };
        let dir = dir.as_str();
        let opp_dir = if dir == "Up" { "Down" } else { "Up" };

        // 目标固定份额:各边买够即收手。
        let target = self.order_shares();
        // 已成交份额(买到就收手)。
        let (up_filled, dn_filled) = self.state.get(&market.slug)
            .map(|p| (p.up_shares, p.down_shares))
            .unwrap_or((0.0, 0.0));
        let dir_filled = if dir == "Up" { up_filled } else { dn_filled };
        let opp_filled = if opp_dir == "Up" { up_filled } else { dn_filled };

        // 挂价"看订单簿":best_ask − 1 tick,但封顶(看好侧≤up_px、反向≤dn_px)。
        // ask 无效(盘口未就绪)则用上限。post_only 保证 maker(ask-1tick < ask 不 crosses)。
        let dir_ask = if dir == "Up" { up_ask } else { dn_ask };
        let opp_ask = if opp_dir == "Up" { up_ask } else { dn_ask };
        let dir_px = if dir_ask > 0.011 { (dir_ask - 0.01).min(self.config.zquote_up_px) } else { self.config.zquote_up_px };
        let opp_px = if opp_ask > 0.011 { (opp_ask - 0.01).min(self.config.zquote_dn_px) } else { self.config.zquote_dn_px };
        let dir_px = dir_px.clamp(0.01, 0.99);
        let opp_px = opp_px.clamp(0.01, 0.99);

        // 看好侧:买够 target 收手(去重/节流在 do_buy_maker_at 内)。
        if dir_filled + 0.5 < target {
            self.do_buy_maker_at(market, dir, dir_px, target - dir_filled, "zquote_dir", price_to_beat).await?;
        }
        // 反向侧:买够 target 收手。
        if opp_filled + 0.5 < target {
            self.do_buy_maker_at(market, opp_dir, opp_px, target - opp_filled, "zquote_opp", price_to_beat).await?;
        }
        Ok(())
    }
}
