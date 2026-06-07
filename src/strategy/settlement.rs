use super::smart::SmartStrategy;
use crate::position::Phase;
use anyhow::Result;
use tracing::info;

impl SmartStrategy {
    // ── 结算 ──────────────────────────────────────────────────────────────

    pub(crate) async fn check_settlements(&mut self) -> Result<()> {
        let pending = self.state.pending_settlement();
        if pending.is_empty() { return Ok(()); }

        let mut changed = false;
        let mut ideal_changed = false;
        for (slug, pos) in pending {
            let Some(winner) = self.client.fetch_winning_outcome(&slug).await else { continue };
            // 结算账面用 settle_pnl(含费实付口径)→ realized_pnl 与链上一致;
            // 决策用的 pnl_if_*_wins(乐观本金口径)不动。
            let pnl = pos.settle_pnl(&winner);
            let emoji = if pnl >= 0.0 { "✅" } else { "❌" };
            info!(
                "[SMART SETTLE] {} | 赢={} | Up={:.0}@{:.3} Down={:.0}@{:.3} | PNL={:+.2} {}",
                slug, winner,
                pos.up_shares, pos.up_avg_full(),
                pos.down_shares, pos.down_avg_full(),
                pnl, emoji
            );
            let p = self.state.get_or_create(&slug, pos.end_ts);
            p.phase = Phase::Settled;
            p.winner = Some(winner.clone());
            p.realized_pnl = Some(pnl);
            // 路线二：结算时清理残留挂单引用（防 maker open_orders 跨盘口泄漏到 state）
            p.open_orders.clear();

            // 同步结算影子账（实盘双轨；模拟时影子账为空，跳过）
            if let Some(ipos) = self.ideal_state.get(&slug).cloned() {
                if !matches!(ipos.phase, Phase::Settled) && !ipos.trades.is_empty() {
                    let ipnl = ipos.settle_pnl(&winner);
                    let ip = self.ideal_state.get_or_create(&slug, ipos.end_ts);
                    ip.phase = Phase::Settled;
                    ip.winner = Some(winner.clone());
                    ip.realized_pnl = Some(ipnl);
                    ideal_changed = true;
                }
            }

            self.write_signal(&serde_json::json!({
                "phase":"settlement","slug":slug,"winner":winner,"pnl":pnl,
                "ts":chrono::Utc::now().timestamp()
            })).await?;
            changed = true;
        }

        if changed {
            self.state.save().await?;
            let s = self.state.summary();
            info!("[SMART STATS] 共{}盘 锁{} 赢{} 输{}  净PNL ${:.2}",
                s.total, s.locked, s.win, s.lose, s.total_pnl);
        }
        if ideal_changed {
            self.ideal_state.save().await?;
        }
        Ok(())
    }
}
