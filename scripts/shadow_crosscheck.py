#!/usr/bin/env python3
"""影子模型对拍夹具生成器。

用现有训练样本训一个临时 LightGBM,导出到 OUT 目录(model.txt/meta.json/calib.json),
并 dump 样本特征(fix.jsonl)与 Python 推理概率(py_pred.txt)。配合 Rust 端单测
`cargo test --bin jy-bot cross_check_python`(设环境 SHADOW_CHECK_DIR=OUT)逐样本比对,
验证 src/model.rs 的纯 Rust 树推理与 Python lightgbm 完全一致(差 < 1e-6)。

用法: python3 scripts/shadow_crosscheck.py [OUT_DIR]   (默认 /tmp/shadowchk)
之后:  SHADOW_CHECK_DIR=OUT_DIR cargo test --bin jy-bot cross_check_python -- --nocapture
"""
import json, os, glob, sys
import numpy as np, lightgbm as lgb
from sklearn.isotonic import IsotonicRegression

DATA = "/opt/jy-data"
OUT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/shadowchk"
os.makedirs(OUT, exist_ok=True)
# 必须与 scripts/train.py 的 FEATURES 完全一致
FEATURES = ["entry_ask","up_ask","dn_ask","ask_sum","z","p_up","p_down","e","v","sigma120","basis60","ct_minus_b","xt_minus_ct","flow_imb_30","flow_imb_60","flow_imb_120","flow_buy_60","flow_sell_60","flow_trades_60","mom_10","mom_30","mom_60","mom_120","seconds_left","bj_hour"]

win = {}
for l in open(f"{DATA}/data/quant_signals.jsonl"):
    try: o = json.loads(l)
    except: continue
    if o.get("phase") == "settlement": win[o["slug"]] = o["winner"]

samples = []
for f in glob.glob(f"{DATA}/data/books/train_samples.jsonl"):
    for l in open(f):
        try: o = json.loads(l)
        except: continue
        if o.get("kind") == "train_sample": samples.append(o)

data = []
for s in samples:
    w = win.get(s.get("slug"))
    if not w or not all(k in s for k in FEATURES): continue
    y = 1 if s.get("direction") == w else 0
    data.append((s, [float(s[k]) for k in FEATURES], y))
print(f"可用样本 {len(data)}")
data.sort(key=lambda r: r[0].get("end_ts", 0))
X = np.array([d[1] for d in data]); Y = np.array([d[2] for d in data])

ds = lgb.Dataset(X, Y, feature_name=FEATURES)
params = dict(objective="binary", metric="binary_logloss", num_leaves=15, max_depth=4,
              learning_rate=0.03, min_data_in_leaf=30, feature_fraction=0.8,
              bagging_fraction=0.8, bagging_freq=1, verbose=-1)
model = lgb.train(params, ds, num_boost_round=200)
model.save_model(f"{OUT}/model.txt")

p = model.predict(X)
iso = IsotonicRegression(out_of_bounds="clip").fit(p, Y)
xs = np.linspace(0, 1, 101)
json.dump({"iso_x": list(xs), "iso_y": [float(iso.predict([x])[0]) for x in xs]}, open(f"{OUT}/calib.json", "w"))
json.dump({"features": FEATURES, "threshold": 0.55, "n_samples": len(data)}, open(f"{OUT}/meta.json", "w"), indent=2)

with open(f"{OUT}/fix.jsonl", "w") as f:
    for s, _, _ in data: f.write(json.dumps(s) + "\n")
with open(f"{OUT}/py_pred.txt", "w") as f:
    for v in p: f.write(f"{v:.17g}\n")
print(f"导出 {OUT}/ : model.txt + {len(data)} 条夹具 + py_pred.txt;树={model.num_trees()}")
