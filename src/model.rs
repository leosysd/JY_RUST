//! LightGBM 影子模型推理(纯 Rust,零 native 依赖)。
//!
//! Python(train.py)训出 `model.txt`(纯文本梯度提升树)+ `calib.json`(isotonic 校准)
//! + `meta.json`(特征列顺序/下注阈值)。本模块直接解析 model.txt 在 Rust 端跑推理,
//! 避免引入 C++ liblightgbm / ONNX,保持单二进制 cp 部署不变。
//!
//! 用法:`LgbModel::load(model_dir)` 加载(目录不全则返回 None,影子静默跳过);
//! `predict_proba(feat_json)` 返回校准后的 P(z方向正确)。
//!
//! 正确性由 model.rs 内的 `cross_check_python` 测试对拍保证(Rust vs Python lightgbm
//! 同输入概率差 < 1e-6),见文件底部。

use serde_json::Value;
use std::path::Path;

/// 单棵回归树。数组下标即 LightGBM 的内部节点编号(0=根)。
struct Tree {
    split_feature: Vec<usize>, // 该内部节点用第几个特征切分
    threshold: Vec<f64>,       // 切分阈值;value <= threshold 走 left
    left_child: Vec<i32>,      // ≥0 内部节点编号;<0 表示叶子,叶子号 = -child-1
    right_child: Vec<i32>,
    leaf_value: Vec<f64>, // 叶子输出(已含 boost_from_average,直接求和)
}

impl Tree {
    fn eval(&self, x: &[f64]) -> f64 {
        // 单叶树(num_leaves=1):无切分,直接给叶值
        if self.split_feature.is_empty() {
            return self.leaf_value.first().copied().unwrap_or(0.0);
        }
        let mut node: i32 = 0;
        loop {
            let i = node as usize;
            let go_left = x[self.split_feature[i]] <= self.threshold[i];
            let child = if go_left { self.left_child[i] } else { self.right_child[i] };
            if child < 0 {
                return self.leaf_value[(-child - 1) as usize];
            }
            node = child;
        }
    }
}

pub struct LgbModel {
    trees: Vec<Tree>,
    /// 特征列顺序(来自 meta.json,= train.py FEATURES);推理时按此从特征 JSON 取值组装向量。
    features: Vec<String>,
    /// isotonic 校准折线点 (iso_x, iso_y);None=不校准。
    calib: Option<(Vec<f64>, Vec<f64>)>,
    /// 下注阈值(meta.json):校准概率 ≥ 此值才算"模型建议下注"。
    pub threshold: f64,
}

impl LgbModel {
    /// 从模型目录加载。缺 model.txt / meta.json 或解析不出树则返回 None(影子静默跳过)。
    pub fn load(model_dir: &Path) -> Option<Self> {
        let txt = std::fs::read_to_string(model_dir.join("model.txt")).ok()?;
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(model_dir.join("meta.json")).ok()?).ok()?;
        let features: Vec<String> = meta
            .get("features")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if features.is_empty() {
            return None;
        }
        let threshold = meta.get("threshold").and_then(|v| v.as_f64()).unwrap_or(0.5);
        let calib = std::fs::read_to_string(model_dir.join("calib.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|c| {
                let xs = c.get("iso_x")?.as_array()?.iter().filter_map(|v| v.as_f64()).collect();
                let ys = c.get("iso_y")?.as_array()?.iter().filter_map(|v| v.as_f64()).collect();
                Some((xs, ys))
            });
        let trees = parse_model_txt(&txt);
        if trees.is_empty() {
            return None;
        }
        Some(Self { trees, features, calib, threshold })
    }

    /// 按 features 顺序从特征 JSON 取值组装输入向量;任一特征缺失/非数值则 None。
    fn vectorize(&self, feat: &Value) -> Option<Vec<f64>> {
        let mut x = Vec::with_capacity(self.features.len());
        for name in &self.features {
            x.push(feat.get(name)?.as_f64()?);
        }
        Some(x)
    }

    /// 模型原始概率(sigmoid(Σ叶值),= Python lightgbm Booster.predict),未做 isotonic 校准。
    pub fn predict_raw_proba(&self, feat: &Value) -> Option<f64> {
        let x = self.vectorize(feat)?;
        let raw: f64 = self.trees.iter().map(|t| t.eval(&x)).sum();
        Some(1.0 / (1.0 + (-raw).exp()))
    }

    /// 校准后的 P(z 方向正确)。供影子记录/将来下注用。
    pub fn predict_proba(&self, feat: &Value) -> Option<f64> {
        let p = self.predict_raw_proba(feat)?;
        Some(self.calibrate(p))
    }

    /// isotonic 折线线性插值校准。
    fn calibrate(&self, p: f64) -> f64 {
        let Some((xs, ys)) = &self.calib else { return p };
        if xs.is_empty() {
            return p;
        }
        if p <= xs[0] {
            return ys[0];
        }
        if p >= xs[xs.len() - 1] {
            return ys[ys.len() - 1];
        }
        // xs 单调递增(linspace),线性扫描足够(101 点)
        for i in 1..xs.len() {
            if p <= xs[i] {
                let (x0, x1) = (xs[i - 1], xs[i]);
                let (y0, y1) = (ys[i - 1], ys[i]);
                let t = if x1 > x0 { (p - x0) / (x1 - x0) } else { 0.0 };
                return y0 + t * (y1 - y0);
            }
        }
        ys[ys.len() - 1]
    }
}

/// 解析 LightGBM model.txt 中的所有 `Tree=N` 段。忽略全局头部与统计行。
fn parse_model_txt(text: &str) -> Vec<Tree> {
    let mut trees = Vec::new();
    let mut cur: Option<std::collections::HashMap<String, String>> = None;
    let flush = |cur: &mut Option<std::collections::HashMap<String, String>>,
                 trees: &mut Vec<Tree>| {
        if let Some(m) = cur.take() {
            if let Some(t) = build_tree(&m) {
                trees.push(t);
            }
        }
    };
    for line in text.lines() {
        if line.starts_with("Tree=") {
            flush(&mut cur, &mut trees);
            cur = Some(std::collections::HashMap::new());
            continue;
        }
        if line.starts_with("end of trees") {
            flush(&mut cur, &mut trees);
            continue;
        }
        if let Some(m) = cur.as_mut() {
            if let Some((k, v)) = line.split_once('=') {
                m.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    flush(&mut cur, &mut trees);
    trees
}

fn nums_usize(s: Option<&String>) -> Vec<usize> {
    s.map(|v| v.split_whitespace().filter_map(|t| t.parse().ok()).collect())
        .unwrap_or_default()
}
fn nums_i32(s: Option<&String>) -> Vec<i32> {
    s.map(|v| v.split_whitespace().filter_map(|t| t.parse().ok()).collect())
        .unwrap_or_default()
}
fn nums_f64(s: Option<&String>) -> Vec<f64> {
    s.map(|v| v.split_whitespace().filter_map(|t| t.parse().ok()).collect())
        .unwrap_or_default()
}

fn build_tree(m: &std::collections::HashMap<String, String>) -> Option<Tree> {
    let leaf_value = nums_f64(m.get("leaf_value"));
    if leaf_value.is_empty() {
        return None;
    }
    Some(Tree {
        split_feature: nums_usize(m.get("split_feature")),
        threshold: nums_f64(m.get("threshold")),
        left_child: nums_i32(m.get("left_child")),
        right_child: nums_i32(m.get("right_child")),
        leaf_value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 对拍:与 Python lightgbm 同输入概率必须一致(< 1e-6),验证树解析/推理正确。
    /// 需 SHADOW_CHECK_DIR 指向含 model.txt/meta.json/calib.json + fix.jsonl + py_pred.txt
    /// 的目录(由 scripts 的对拍脚本生成);未设则跳过,不阻塞常规测试。
    #[test]
    fn cross_check_python() {
        let Ok(dir) = std::env::var("SHADOW_CHECK_DIR") else {
            eprintln!("SHADOW_CHECK_DIR 未设,跳过对拍");
            return;
        };
        let model = LgbModel::load(Path::new(&dir)).expect("加载模型失败");
        let fix = std::fs::read_to_string(format!("{dir}/fix.jsonl")).expect("读 fix.jsonl");
        let py: Vec<f64> = std::fs::read_to_string(format!("{dir}/py_pred.txt"))
            .expect("读 py_pred.txt")
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        let mut maxd = 0.0f64;
        for (i, line) in fix.lines().enumerate() {
            let v: Value = serde_json::from_str(line).unwrap();
            let p = model.predict_raw_proba(&v).expect("推理失败");
            let d = (p - py[i]).abs();
            if d > maxd {
                maxd = d;
            }
        }
        println!("对拍样本数={} 最大概率差={maxd:e}", py.len());
        assert!(maxd < 1e-6, "Rust 与 Python 推理不一致: {maxd:e}");
    }
}
