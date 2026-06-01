#!/usr/bin/env python3
"""JY LightGBM 方向预测训练脚本(方式B:Python训练,bot影子模式用)。

流程:
  1. 读 train_samples.jsonl(每盘特征快照) join settlement(quant_signals.jsonl)拿赢家标签
  2. 样本 <MIN_SAMPLES(默认1000) → 打印进度退出(不训练,防过拟合)
  3. ≥MIN_SAMPLES → 时间序列分割训 LightGBM + isotonic概率校准 + 阈值过滤
  4. 导出 model.txt(LightGBM) + calib.json(校准) + meta.json(特征列/阈值/CV指标)
由 cron 每小时调用;够样本自动开训,之后定期重训。
"""
import json, os, sys, glob

DATA = "/opt/jy-data"
BOOKS = f"{DATA}/data/books"
SIGNALS = f"{DATA}/data/quant_signals.jsonl"
OUT = f"{DATA}/model"
MIN_SAMPLES = int(os.environ.get("ML_MIN_SAMPLES", "1000"))

# 特征列(与 bot build_features 对齐;只用纯数值、入场时刻可得的)
FEATURES = [
    "entry_ask","up_ask","dn_ask","ask_sum",
    "z","p_up","p_down","e","v","sigma120","basis60",
    "ct_minus_b","xt_minus_ct",
    "flow_imb_30","flow_imb_60","flow_imb_120","flow_buy_60","flow_sell_60","flow_trades_60",
    "mom_10","mom_30","mom_60","mom_120",
    "seconds_left","bj_hour",
]

def load_winners():
    w = {}
    if not os.path.exists(SIGNALS): return w
    for line in open(SIGNALS):
        try: o = json.loads(line)
        except: continue
        if o.get("phase") == "settlement":
            w[o["slug"]] = o["winner"]
    return w

def load_samples():
    rows = []
    for f in sorted(glob.glob(f"{BOOKS}/train_samples.jsonl")):
        for line in open(f):
            try: o = json.loads(line)
            except: continue
            if o.get("kind") == "train_sample": rows.append(o)
    return rows

def main():
    winners = load_winners()
    samples = load_samples()
    # join: 标签 = 该盘记录方向(dir)是否==赢家 → 二分类(1=方向对,0=错)
    data = []
    for s in samples:
        w = winners.get(s.get("slug"))
        if not w: continue
        if not all(k in s for k in FEATURES): continue
        y = 1 if s.get("direction") == w else 0
        x = [float(s[k]) for k in FEATURES]
        data.append((s.get("end_ts", 0), x, y))
    n = len(data)
    print(f"[train] 已标注样本: {n}/{MIN_SAMPLES}")
    if n < MIN_SAMPLES:
        print(f"[train] 样本不足,跳过训练(还需 {MIN_SAMPLES-n})")
        return 0

    import numpy as np, lightgbm as lgb
    from sklearn.isotonic import IsotonicRegression
    from sklearn.metrics import roc_auc_score, log_loss

    data.sort(key=lambda r: r[0])  # 按时间排序(时序分割,防未来泄露)
    X = np.array([d[1] for d in data]); Y = np.array([d[2] for d in data])
    split = int(n * 0.8)
    Xtr,Ytr,Xva,Yva = X[:split],Y[:split],X[split:],Y[split:]

    ds = lgb.Dataset(Xtr, Ytr, feature_name=FEATURES)
    params = dict(objective="binary", metric="binary_logloss",
                  num_leaves=15, max_depth=4, learning_rate=0.03,
                  min_data_in_leaf=30, feature_fraction=0.8, bagging_fraction=0.8,
                  bagging_freq=1, verbose=-1)  # 保守参数防过拟合(小数据)
    model = lgb.train(params, ds, num_boost_round=200)

    # 概率校准(isotonic)用验证集
    pva = model.predict(Xva)
    auc = roc_auc_score(Yva, pva) if len(set(Yva))>1 else 0.5
    iso = IsotonicRegression(out_of_bounds="clip").fit(pva, Yva)
    pcal = iso.predict(pva)
    base_rate = float(Y.mean())

    # 阈值过滤:扫描校准后概率阈值,找"下注子集胜率"最高且样本够的阈值
    best = {"thr":0.5,"winrate":base_rate,"n":len(Yva)}
    for thr in [0.50,0.52,0.55,0.58,0.60,0.62,0.65]:
        mask = pcal >= thr
        if mask.sum() >= max(10, len(Yva)*0.1):
            wr = float(Yva[mask].mean())
            if wr > best["winrate"]:
                best = {"thr":thr,"winrate":wr,"n":int(mask.sum())}

    os.makedirs(OUT, exist_ok=True)
    model.save_model(f"{OUT}/model.txt")
    # isotonic 导出为 (x,y) 折线点,bot端线性插值用
    xs = np.linspace(0,1,101)
    json.dump({"iso_x":list(xs),"iso_y":[float(iso.predict([x])[0]) for x in xs]},
              open(f"{OUT}/calib.json","w"))
    json.dump({"features":FEATURES,"n_samples":n,"base_rate":base_rate,
               "val_auc":float(auc),"threshold":best["thr"],
               "thr_winrate":best["winrate"],"thr_n":best["n"]},
              open(f"{OUT}/meta.json","w"), indent=2)
    print(f"[train] 训练完成 n={n} AUC={auc:.3f} 基础胜率={base_rate:.3f}")
    print(f"[train] 最优阈值={best['thr']} 子集胜率={best['winrate']:.3f}(n={best['n']})")
    print(f"[train] 模型已导出到 {OUT}/")
    return 0

if __name__ == "__main__":
    sys.exit(main())
