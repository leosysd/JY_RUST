use super::smart::{strategy_order_shares, SmartStrategy};
use crate::clob::Market;
use crate::position::Phase;
use anyhow::Result;
use tracing::{debug, info};

impl SmartStrategy {
    /// 延迟套利狙击:盘内监控 binance,BTC 相对开盘价(window=0)或最近 N 秒(window>0)
    /// 动够 ±move_usd 即定方向;仅当对应方向 ask < max_ask(盘口还没反应)时 FOK 买入,
    /// 裸持到结算。每盘只狙一次。赚的是 Polymarket 盘口对 binance 变动的反应延迟。
    pub(crate) async fn decide_sniper(
        &mut self,
        market: &Market,
        up_ask: f64,
        dn_ask: f64,
        seconds_left: i64,
    ) -> Result<()> {
        // 每盘只狙一次
        if self.sniped_slugs.contains(&market.slug) { return Ok(()); }
        let now = chrono::Utc::now().timestamp();
        if now < market.start_ts { return Ok(()); } // 还没开盘

        // 开盘价 + 当前价(binance)
        let Some(open_px) = self.model.binance_at(market.start_ts) else { return Ok(()); };
        let Some(cur_px) = self.model.binance_latest() else { return Ok(()); };

        // 参照价:window=0 用开盘价(相对开盘累计);>0 用最近 N 秒前价(纯速度)
        let ref_px = if self.config.sniper_window_sec > 0 {
            match self.model.binance_at(now - self.config.sniper_window_sec) {
                Some(p) => p,
                None => return Ok(()),
            }
        } else {
            open_px
        };
        let move_usd = cur_px - ref_px;
        if move_usd.abs() < self.config.sniper_move_usd { return Ok(()); } // 没动够,继续等
        let t_sig = std::time::Instant::now(); // 检测到突破信号的时刻

        let dir = if move_usd > 0.0 { "Up" } else { "Down" };
        let ask = if dir == "Up" { up_ask } else { dn_ask };

        // 限价闸(灵魂):盘口还没反应(ask<max)才买;已反应(ask 高)则放弃这盘
        if ask > self.config.sniper_max_ask {
            self.sniped_slugs.insert(market.slug.clone());
            debug!("[SNIPER] {} {dir} 突破${:+.0} 但 ask={ask:.3}>{:.2}(盘口已反应),放弃 T-{seconds_left}s",
                market.title, move_usd, self.config.sniper_max_ask);
            return Ok(());
        }

        let qty = strategy_order_shares(self.config.sniper_qty).unwrap_or(20.0);
        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        info!("[SNIPER {mode}] {} 突破${:+.0}(开盘{open_px:.0}→现{cur_px:.0}) 买{dir}@{ask:.3}×{qty:.0} T-{seconds_left}s",
            market.title, move_usd);

        // 下单前先标记已狙,避免本盘后续 tick 重复触发
        self.sniped_slugs.insert(market.slug.clone());
        // 信号→开始下单延迟:检测突破到调起下单。write_signal 挪到下单之后,
        // 不让写文件 IO 阻塞下单(狙击争分夺秒)。
        info!("[SIGNAL_LAT] 检测突破→开始下单={}ms", t_sig.elapsed().as_millis());

        // 买入 + 裸持到结算
        let bought = self.do_buy(market, dir, ask, qty, "sniper", open_px).await?;
        let _ = self.write_signal(&serde_json::json!({
            "phase": "sniper_entry", "market": market.slug,
            "direction": dir, "ask": ask, "shares": qty,
            "open_px": open_px, "cur_px": cur_px, "move_usd": move_usd,
            "window_sec": self.config.sniper_window_sec,
            "seconds_left": seconds_left, "dry_run": self.config.dry_run,
            "ts": now,
        })).await;
        if bought {
            let p = self.state.get_or_create(&market.slug, market.end_ts);
            p.phase = Phase::Locked;
            self.state.save().await?;
        }
        Ok(())
    }
}
