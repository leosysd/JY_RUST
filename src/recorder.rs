//! 全盘口 tick 级数据采集器（与交易无关，常驻 WS 录制）。
//!
//! 设计：WS 热路径（ws::handle_message）每次盘口更新后，把一行 JSON 通过
//! 无界 channel 发给后台 writer，**不阻塞行情处理**。writer 持有文件句柄，
//! 按天切到 `book_record_dir/quant_book-YYYYMMDD.jsonl`，批量写后定期 flush。
//!
//! 一启动即采集，不受 DRY_RUN 影响（无论模拟/实盘/不交易都录）。
//! 数据为 JSONL（每行一条），事后可用脚本导入 SQLite 做 SQL 分析。

use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// 采集器句柄：克隆廉价（仅克隆 sender）。
#[derive(Clone)]
pub struct Recorder {
    tx: mpsc::UnboundedSender<String>,
}

impl Recorder {
    /// 创建采集器并 spawn 后台 writer。dir 不存在会自动创建。
    pub fn spawn(dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(writer_loop(dir, rx));
        Self { tx }
    }

    /// 非阻塞投递一行（已含 JSON，不含换行）。channel 关闭时静默丢弃。
    pub fn record(&self, line: String) {
        let _ = self.tx.send(line);
    }
}

/// 后台写盘循环：持有当天文件句柄，跨天自动切换；每批 drain 后 flush。
async fn writer_loop(dir: PathBuf, mut rx: mpsc::UnboundedReceiver<String>) {
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        warn!("[REC] 创建采集目录失败 {}: {e}", dir.display());
    }
    info!("[REC] 盘口采集启动，输出目录 {}", dir.display());

    let mut cur_day = String::new();
    let mut file: Option<tokio::fs::File> = None;

    while let Some(first) = rx.recv().await {
        // 把通道里已积压的行一次性取出，批量写后只 flush 一次。
        let mut batch = vec![first];
        while let Ok(line) = rx.try_recv() {
            batch.push(line);
            if batch.len() >= 1000 { break; }
        }

        let day = beijing_day();
        if day != cur_day || file.is_none() {
            let path = dir.join(format!("quant_book-{day}.jsonl"));
            match tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await {
                Ok(f) => { file = Some(f); cur_day = day; }
                Err(e) => { warn!("[REC] 打开采集文件失败 {}: {e}", path.display()); continue; }
            }
        }

        if let Some(f) = file.as_mut() {
            let mut buf = String::with_capacity(batch.len() * 96);
            for line in &batch {
                buf.push_str(line);
                buf.push('\n');
            }
            if let Err(e) = f.write_all(buf.as_bytes()).await {
                warn!("[REC] 写采集文件失败: {e}");
                file = None; // 下轮重开
                continue;
            }
            let _ = f.flush().await;
        }
    }
    info!("[REC] 盘口采集通道关闭，writer 退出");
}

/// 当前北京时间日期 YYYYMMDD（与 book 数据按北京日切分一致）。
fn beijing_day() -> String {
    let bj = chrono::Utc::now() + chrono::Duration::hours(8);
    bj.format("%Y%m%d").to_string()
}
