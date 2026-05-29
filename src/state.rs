use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JfMarketState {
    pub entry_outcome: String,
    pub entry_price: String,
    pub entry_token: String,
    pub locked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_profit_pct: Option<String>,
    // 结算追踪
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_ts: Option<i64>,
    #[serde(default)]
    pub settled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winning_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realized_pnl: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct StateFile {
    #[serde(default)]
    jetfadil: HashMap<String, JfMarketState>,
}

pub struct StateStore {
    path: PathBuf,
    inner: StateFile,
}

impl StateStore {
    pub async fn load(path: PathBuf) -> Result<Self> {
        let inner = if path.exists() {
            let text = fs::read_to_string(&path).await?;
            serde_json::from_str(&text).unwrap_or_default()
        } else {
            StateFile::default()
        };
        Ok(Self { path, inner })
    }

    pub fn get_jf(&self, slug: &str) -> Option<&JfMarketState> {
        self.inner.jetfadil.get(slug)
    }

    pub fn set_jf(&mut self, slug: &str, state: JfMarketState) {
        self.inner.jetfadil.insert(slug.to_string(), state);
    }

    /// 返回所有需要结算检查的盘口（已结束但未结算）
    pub fn pending_settlement(&self) -> Vec<(String, JfMarketState)> {
        let now = chrono::Utc::now().timestamp();
        self.inner.jetfadil.iter()
            .filter(|(_, s)| {
                !s.settled
                    && s.end_ts.map(|t| now > t + 10).unwrap_or(false)
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// 统计摘要
    pub fn summary(&self) -> StatsSummary {
        let mut total = 0;
        let mut locked = 0;
        let mut settled_win = 0;
        let mut settled_lose = 0;
        let mut total_pnl = 0.0f64;

        for s in self.inner.jetfadil.values() {
            total += 1;
            if s.locked { locked += 1; }
            if s.settled {
                if let Some(pnl) = s.realized_pnl.as_ref()
                    .and_then(|p| p.parse::<f64>().ok()) {
                    total_pnl += pnl;
                    if pnl >= 0.0 { settled_win += 1; } else { settled_lose += 1; }
                }
            }
        }
        StatsSummary { total, locked, settled_win, settled_lose, total_pnl }
    }

    pub async fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let text = serde_json::to_string_pretty(&self.inner)?;
        fs::write(&self.path, text).await?;
        Ok(())
    }
}

pub struct StatsSummary {
    pub total: usize,
    pub locked: usize,
    pub settled_win: usize,
    pub settled_lose: usize,
    pub total_pnl: f64,
}
