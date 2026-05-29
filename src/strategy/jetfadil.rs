use crate::clob::{BookCache, ClobClient, Market};
use crate::config::Config;
use crate::state::{JfMarketState, StateStore};
use anyhow::Result;
use rust_decimal::Decimal;
use serde_json::json;
use std::path::PathBuf;
use std::str::FromStr;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

pub struct JetFadilStrategy {
    pub config: Config,
    pub state: StateStore,
    pub client: ClobClient,
    pub cache: BookCache,
    pub signal_file: PathBuf,
    /// timestamp of the first allowed slot (skip the startup slot)
    pub first_allowed_start: i64,
}

impl JetFadilStrategy {
    pub async fn new(config: Config, cache: BookCache) -> Result<Self> {
        let state = StateStore::load(config.state_file.clone()).await?;
        let client = ClobClient::new(&config.clob_api_url, &config.gamma_api_url, &config.market_slug_prefix);
        let signal_file = config.signal_file.clone();

        // only enter markets that opened AFTER we started
        let now = chrono::Utc::now().timestamp();
        let first_allowed_start = next_slot_boundary(now);

        Ok(Self {
            config,
            state,
            client,
            cache,
            signal_file,
            first_allowed_start,
        })
    }

    pub async fn run_once(&mut self) -> Result<()> {
        let Some(market) = self.client.find_current_market().await else {
            info!("[JF] 未找到当前 BTC 5m 市场，等待...");
            return Ok(());
        };

        if market.start_ts < self.first_allowed_start {
            info!(
                "[JF] 等待新盘口: current={} first_allowed={} (BJ: {})",
                market.start_ts,
                self.first_allowed_start,
                beijing_time(self.first_allowed_start)
            );
            return Ok(());
        }

        if market.seconds_left() < self.config.min_seconds_left as i64 {
            return Ok(());
        }

        // ensure WS subscription happens outside - just fetch book via HTTP here
        let up_token = market.token_for("Up").unwrap_or("").to_string();
        let down_token = market.token_for("Down").unwrap_or("").to_string();
        if up_token.is_empty() || down_token.is_empty() {
            return Ok(());
        }

        // 每次都从 HTTP 拉取最新盘口（避免缓存过期导致价格滞后）
        let up_book = match self.client.fetch_book(&up_token).await {
            Ok(b) => b,
            Err(e) => { warn!("[JF] book error Up: {e}"); return Ok(()); }
        };
        let down_book = match self.client.fetch_book(&down_token).await {
            Ok(b) => b,
            Err(e) => { warn!("[JF] book error Down: {e}"); return Ok(()); }
        };

        let Some(up_ask) = up_book.best_ask() else { return Ok(()); };
        let Some(down_ask) = down_book.best_ask() else { return Ok(()); };

        let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
        let jf_state = self.state.get_jf(&market.slug).cloned();

        match jf_state {
            None => {
                // ── 阶段1: 入场 ──────────────────────────────────────
                let elapsed = market.seconds_elapsed();
                if elapsed > self.config.max_entry_delay_sec as i64 {
                    info!(
                        "[JF] {} 已过入场窗口 (+{}s > {}s)",
                        market.title, elapsed, self.config.max_entry_delay_sec
                    );
                    return Ok(());
                }

                // 选最接近 0.50 且在 [min_entry, max_entry] 的边
                let candidates = [
                    ("Up",   up_ask,   up_token.clone()),
                    ("Down", down_ask, down_token.clone()),
                ];
                let entry = candidates.iter().filter(|(_, ask, _)| {
                    *ask >= self.config.min_entry_price && *ask <= self.config.max_entry_price
                })
                .min_by_key(|(_, ask, _)| {
                    let diff = (*ask - Decimal::new(5, 1)).abs();
                    (diff * Decimal::new(10000, 0)).to_i64_saturating()
                });

                let Some((outcome, ask, token)) = entry else {
                    info!("[JF] {} 无合适入场价: Up={} Down={}", market.title, up_ask, down_ask);
                    return Ok(());
                };

                info!(
                    "[JF ENTRY {mode}] {} {}@{} ×{}份  +{}s",
                    market.title, outcome, ask, self.config.order_shares, elapsed
                );

                self.write_signal(&json!({
                    "strategy": "jetfadil",
                    "phase": "entry",
                    "action": if self.config.dry_run { "would_buy" } else { "live_buy" },
                    "market": { "slug": market.slug, "title": market.title, "seconds_left": market.seconds_left() },
                    "outcome": outcome,
                    "price": ask.to_string(),
                    "shares": self.config.order_shares.to_string(),
                    "ts": chrono::Utc::now().timestamp(),
                })).await?;

                let new_state = JfMarketState {
                    entry_outcome: outcome.to_string(),
                    entry_price:   ask.to_string(),
                    entry_token:   token.clone(),
                    locked: false,
                    lock_price: None,
                    lock_profit_pct: None,
                };

                if !self.config.dry_run {
                    self.post_order(&market, outcome, *ask, self.config.order_shares, token.to_string()).await?;
                }

                self.state.set_jf(&market.slug, new_state);
                self.state.save().await?;
            }

            Some(s) if s.locked => {
                // 已锁利，等结算
            }

            Some(s) => {
                // ── 阶段2: 监控锁利 ──────────────────────────────────
                let entry_price = Decimal::from_str(&s.entry_price).unwrap_or_default();
                let (other_outcome, other_ask, other_token) = if s.entry_outcome == "Up" {
                    ("Down", down_ask, down_token.clone())
                } else {
                    ("Up", up_ask, up_token.clone())
                };

                let fee = Decimal::ONE + self.config.fee_rate;
                let total_cost = (entry_price + other_ask) * fee;
                let profit = Decimal::ONE - total_cost;

                if profit < self.config.min_lock_profit {
                    info!(
                        "[JF] {} 等待锁利: {}@{} + {}@{} → cost={:.4} profit={:.4} (需>={:.2})",
                        market.title,
                        s.entry_outcome, entry_price,
                        other_outcome, other_ask,
                        total_cost, profit, self.config.min_lock_profit
                    );
                    return Ok(());
                }

                let profit_pct = (profit * Decimal::new(100, 0))
                    .round_dp(2);

                info!(
                    "[JF LOCK {mode}] {} 买{}@{} ×{}份  entry={}@{}  cost={:.4}  profit={}% T-{}s",
                    market.title,
                    other_outcome, other_ask, self.config.order_shares,
                    s.entry_outcome, entry_price,
                    total_cost, profit_pct, market.seconds_left()
                );

                self.write_signal(&json!({
                    "strategy": "jetfadil",
                    "phase": "lock",
                    "action": if self.config.dry_run { "would_buy" } else { "live_buy" },
                    "market": { "slug": market.slug, "title": market.title, "seconds_left": market.seconds_left() },
                    "entry_outcome": s.entry_outcome,
                    "entry_price": entry_price.to_string(),
                    "lock_outcome": other_outcome,
                    "lock_price": other_ask.to_string(),
                    "total_cost": total_cost.to_string(),
                    "profit_per_share": profit.to_string(),
                    "profit_pct": profit_pct.to_string(),
                    "shares": self.config.order_shares.to_string(),
                    "ts": chrono::Utc::now().timestamp(),
                })).await?;

                if !self.config.dry_run {
                    let shares = self.config.order_shares;
                    self.post_order(&market, other_outcome, other_ask, shares, other_token).await?;
                }

                let mut updated = s;
                updated.locked = true;
                updated.lock_price = Some(other_ask.to_string());
                updated.lock_profit_pct = Some(profit_pct.to_string());
                self.state.set_jf(&market.slug, updated);
                self.state.save().await?;
            }
        }

        Ok(())
    }

    async fn post_order(
        &self,
        _market: &Market,
        outcome: &str,
        price: Decimal,
        shares: Decimal,
        token_id: String,
    ) -> Result<()> {
        // DRY_RUN はここに来ない。LIVE モードはサブプロセスで Python 署名を呼ぶ
        info!("[JF ORDER] {} {} {}@{} → calling python signer", _market.slug, outcome, shares, price);
        let output = tokio::process::Command::new("jy")
            .args(["_post_order", &token_id, &price.to_string(), &shares.to_string(), "BUY"])
            .output()
            .await;
        match output {
            Ok(o) => info!("[JF ORDER RESP] {}", String::from_utf8_lossy(&o.stdout)),
            Err(e) => warn!("[JF ORDER ERR] {e}"),
        }
        Ok(())
    }

    async fn write_signal(&self, record: &serde_json::Value) -> Result<()> {
        if let Some(parent) = self.signal_file.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.signal_file)
            .await?;
        let line = serde_json::to_string(record)? + "\n";
        file.write_all(line.as_bytes()).await?;
        Ok(())
    }
}

fn next_slot_boundary(ts: i64) -> i64 {
    ((ts / 300) + 1) * 300
}

fn beijing_time(ts: i64) -> String {
    use chrono::TimeZone;
    let dt = chrono::Utc.timestamp_opt(ts, 0).single().unwrap_or_default();
    let bj = dt + chrono::Duration::hours(8);
    bj.format("%Y-%m-%dT%H:%M:%S+08:00").to_string()
}

trait ToI64Saturating {
    fn to_i64_saturating(self) -> i64;
}

impl ToI64Saturating for Decimal {
    fn to_i64_saturating(self) -> i64 {
        self.try_into().unwrap_or(i64::MAX)
    }
}
