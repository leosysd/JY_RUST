use crate::position::{MarketPosition, Phase};
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::fs;

pub struct SmartStateStore {
    path: PathBuf,
    positions: HashMap<String, MarketPosition>,
}

impl SmartStateStore {
    pub async fn load(path: PathBuf) -> Result<Self> {
        // 启动时一次性建好目录,之后 save() 不再每次 create_dir_all(那是热路径上的多余 syscall)。
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let positions = if path.exists() {
            let text = fs::read_to_string(&path).await?;
            serde_json::from_str::<HashMap<String, MarketPosition>>(&text)
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        Ok(Self { path, positions })
    }

    pub fn get_or_create(&mut self, slug: &str, end_ts: i64) -> &mut MarketPosition {
        self.positions
            .entry(slug.to_string())
            .or_insert_with(|| MarketPosition::new(slug, end_ts, 0.0))
    }

    pub fn get(&self, slug: &str) -> Option<&MarketPosition> {
        self.positions.get(slug)
    }

    /// 返回需要结算查询的盘口（已结束 ≥10s，未结算）
    pub fn pending_settlement(&self) -> Vec<(String, MarketPosition)> {
        let now = chrono::Utc::now().timestamp();
        self.positions.iter()
            .filter(|(_, p)| {
                !matches!(p.phase, Phase::Settled)
                    && !p.trades.is_empty()
                    && p.end_ts > 0
                    && now > p.end_ts + 10
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn summary(&self) -> Summary {
        let mut total = 0;
        let mut locked = 0;
        let mut win = 0;
        let mut lose = 0;
        let mut total_pnl = 0.0f64;

        for p in self.positions.values() {
            if p.trades.is_empty() { continue; }
            total += 1;
            let has_both = p.up_shares > 0.0 && p.down_shares > 0.0;
            if has_both { locked += 1; }
            if let Some(pnl) = p.realized_pnl {
                total_pnl += pnl;
                if pnl >= 0.0 { win += 1; } else { lose += 1; }
            }
        }
        Summary { total, locked, win, lose, total_pnl }
    }

    pub async fn save(&self) -> Result<()> {
        // 目录已在 load() 建好;此处不再 create_dir_all。紧凑序列化(非 pretty)减少
        // 序列化与写入字节数——状态文件供程序读,无需人肉缩进。
        let text = serde_json::to_string(&self.positions)?;
        fs::write(&self.path, text).await?;
        Ok(())
    }
}

pub struct Summary {
    pub total: usize,
    pub locked: usize,
    pub win: usize,
    pub lose: usize,
    pub total_pnl: f64,
}
