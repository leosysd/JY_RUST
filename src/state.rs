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

    pub async fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let text = serde_json::to_string_pretty(&self.inner)?;
        fs::write(&self.path, text).await?;
        Ok(())
    }
}
