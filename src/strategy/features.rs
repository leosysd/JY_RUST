use super::smart::SmartStrategy;
use crate::clob::Market;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

impl SmartStrategy {
    /// 每盘记一条训练样本(特征快照)到 book 目录的 train_samples.jsonl。
    /// 纯记录、不影响交易。z信号缺失时跳过(无特征)。标签由训练脚本join settlement。
    pub(crate) async fn record_train_sample(&self, market: &Market, up_ask: f64, dn_ask: f64, seconds_left: i64) {
        // 训练样本也要真开盘价(否则特征里的 ct-b/z 失真,污染训练集)。
        let price_to_beat = self.model.chainlink_at(market.start_ts).unwrap_or(0.0);
        if price_to_beat < 1000.0 { return; }
        // 训练样本记录沿用原 Chainlink 公式(与历史 z 数据口径一致)。
        let Some(sig) = self.model.compute(price_to_beat, seconds_left, crate::zscore::DirSource::Chainlink) else { return; };
        // 方向取 z 倾向(>0看Up),仅作记录;入场价取该方向 ask
        let dir = if sig.z >= 0.0 { "Up" } else { "Down" };
        let entry_ask = if dir == "Up" { up_ask } else { dn_ask };
        let mut feat = self.build_features(&sig, dir, entry_ask, up_ask, dn_ask, seconds_left);
        self.add_book_depth(&mut feat, market).await;
        // 影子预测:模型就绪时记一条"模型置信 vs z vs(待结算)结果"到 signal 文件,不参与下单。
        // 结算后用 market join settlement(winner)即可评估模型挑盘能力,达标再接管。
        if let Some(m) = &self.shadow {
            if let Some(p) = m.predict_proba(&feat) {
                let rec = serde_json::json!({
                    "phase": "shadow",
                    "market": market.slug,
                    "ts": chrono::Utc::now().timestamp(),
                    "z": sig.z,
                    "z_dir": dir,                 // 模型对 z 方向打置信分,方向同 z
                    "model_p": p,                 // 校准后 P(z 方向正确)
                    "model_bet": p >= m.threshold, // 是否达下注阈值
                    "thr": m.threshold,
                });
                let _ = self.write_signal(&rec).await;
            }
        }
        feat["slug"] = serde_json::json!(market.slug);
        feat["end_ts"] = serde_json::json!(market.end_ts);
        feat["kind"] = serde_json::json!("train_sample");
        let path = self.config.book_record_dir.join("train_samples.jsonl");
        if let Some(p) = path.parent() { let _ = fs::create_dir_all(p).await; }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path).await {
            let _ = f.write_all((feat.to_string() + "\n").as_bytes()).await;
        }
    }

    /// 构造入场时刻的丰富特征集(为 LightGBM 铺路)。纯记录,无副作用。
    /// 含: z全套、多窗口量价、多窗口动量、盘口衍生、时间特征。
    pub(crate) fn build_features(
        &self, sig: &crate::zscore::ZSignal, dir: &str,
        entry_ask: f64, up_ask: f64, dn_ask: f64, seconds_left: i64,
    ) -> serde_json::Value {
        let now = chrono::Utc::now().timestamp();
        // 多窗口量价
        let f30 = self.model.binance_flow(now, 30);
        let f60 = self.model.binance_flow(now, 60);
        let f120 = self.model.binance_flow(now, 120);
        // 多窗口动量
        let m10 = self.model.binance_momentum(now, 10).unwrap_or(0.0);
        let m30 = self.model.binance_momentum(now, 30).unwrap_or(0.0);
        let m60 = self.model.binance_momentum(now, 60).unwrap_or(0.0);
        let m120 = self.model.binance_momentum(now, 120).unwrap_or(0.0);
        // 时间特征(北京小时)
        let bj_hour = ((now + 8*3600) / 3600) % 24;
        serde_json::json!({
            "ts": now, "direction": dir, "entry_ask": entry_ask,
            // 盘口
            "up_ask": up_ask, "dn_ask": dn_ask, "ask_sum": up_ask + dn_ask,
            // z全套
            "z": sig.z, "p_up": sig.p_up, "p_down": sig.p_down,
            "e": sig.e, "v": sig.v, "ct": sig.ct, "xt": sig.xt, "b": sig.b,
            "sigma120": sig.sigma120, "basis60": sig.basis60,
            // 衍生:价差
            "ct_minus_b": sig.ct - sig.b,        // chainlink相对开盘
            "xt_minus_ct": sig.xt - sig.ct,      // binance-chainlink basis
            // 多窗口量价不平衡
            "flow_imb_30": f30.imbalance, "flow_imb_60": f60.imbalance, "flow_imb_120": f120.imbalance,
            "flow_buy_60": f60.buy_vol, "flow_sell_60": f60.sell_vol, "flow_trades_60": f60.trades,
            // 30/120 窗口绝对量(只记不启用,补齐量特征;之前只有60窗口有绝对量)
            "flow_buy_30": f30.buy_vol, "flow_sell_30": f30.sell_vol, "flow_trades_30": f30.trades,
            "flow_buy_120": f120.buy_vol, "flow_sell_120": f120.sell_vol, "flow_trades_120": f120.trades,
            // 多窗口动量
            "mom_10": m10, "mom_30": m30, "mom_60": m60, "mom_120": m120,
            // 时间
            "seconds_left": seconds_left, "bj_hour": bj_hour,
        })
    }

    /// 往特征 json 补 Polymarket 盘口深度:Up/Down 两个 token 各自的 bid/ask 挂单总量。
    /// **只记不启用**(train.py FEATURES 未纳入),为将来"盘口深度"特征铺路;纯记录、零风险。
    pub(crate) async fn add_book_depth(&self, feat: &mut serde_json::Value, market: &Market) {
        let cache = self.cache.read().await;
        let depth = |tok: Option<&str>, ask_side: bool| -> f64 {
            tok.and_then(|t| cache.get(t))
                .map(|b| {
                    let lv = if ask_side { &b.asks } else { &b.bids };
                    lv.iter()
                        .map(|(_, s)| s.to_string().parse::<f64>().unwrap_or(0.0))
                        .sum()
                })
                .unwrap_or(0.0)
        };
        let up = market.token_for("Up");
        let dn = market.token_for("Down");
        feat["up_bid_depth"] = serde_json::json!(depth(up, false));
        feat["up_ask_depth"] = serde_json::json!(depth(up, true));
        feat["dn_bid_depth"] = serde_json::json!(depth(dn, false));
        feat["dn_ask_depth"] = serde_json::json!(depth(dn, true));
    }

    /// 写 token→slug/outcome/end_ts 映射到 book 目录的 token_map.jsonl(复盘join赢家用)。
    /// book 录的只有 token,靠此映射把逐tick价格关联到盘口+方向,再join结算赢家。
    pub(crate) async fn write_token_map(&self, market: &Market) {
        let dir = &self.config.book_record_dir;
        let _ = fs::create_dir_all(dir).await;
        let path = dir.join("token_map.jsonl");
        let mut lines = String::new();
        for (i, tok) in market.token_ids.iter().enumerate() {
            let outcome = market.outcomes.get(i).cloned().unwrap_or_default();
            let rec = serde_json::json!({
                "token": tok, "slug": market.slug, "outcome": outcome,
                "end_ts": market.end_ts, "ts": chrono::Utc::now().timestamp(),
            });
            lines.push_str(&rec.to_string());
            lines.push('\n');
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path).await {
            let _ = f.write_all(lines.as_bytes()).await;
        }
    }
}
