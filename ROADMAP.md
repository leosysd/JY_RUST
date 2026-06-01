# JY Bot 计划 / ROADMAP

> 唯一的成文计划。之前待办散在对话里,易丢,从此集中于此。
> 最后更新: 2026-06-02

## 当前状态(一句话)
ev_solo 策略实盘运行中(爱尔兰 Lightsail,20份/场,DRY_RUN=0);同时攒 LightGBM 训练样本,够1000自动训。

## 核心结论(已用数据/数学锁定,别再走回头路)
- BTC 5分钟方向 ≈ 50%,7%费 → 任何"出场技巧/对冲/止盈止损/赔率配比"都做不出正期望(数学证明:对冲腿每份必亏;鞅过程止盈止损EV=0)。
- 唯一正期望路径 = **方向预测 >平衡线胜率** + 单边裸持。对冲、低价2:1、JetFadil追涨,全已证伪。
- z-score 方向实测 154场57.8%、ev_solo 31场67.7%(p=0.035显著)→ **唯一活着的 edge 候选**,正在实盘验证。
- JetFadil 不是榜样(真实微亏、追涨、ROI1.1%,lb-api +15万是成交量积分非利润)。

## 进行中
- [进行] ev_solo 实盘验证:20份/场,攒到 50-100 场看胜率是否稳 >55%(平衡线~52.6%)。薄样本,谨慎。
- [进行] 训练样本采集:bot 每盘记25+特征到 train_samples.jsonl,标签join settlement。目标1000,约3-4天。
- [自动] systemd timer `jy-train.timer` 每小时跑 train.py:样本<1000跳过,≥1000自动训 LightGBM+isotonic校准+阈值过滤,导出 /opt/jy-data/model/。

## 待做(按优先级)
1. **LightGBM 影子模式**:模型出来后,bot 加载 model.txt,每盘预测并记录"模型预测 vs 实际赢家",不下单。验证模型胜率是否 > z-score baseline。
2. **模型接管下注**:影子验证有效后,让模型预测+阈值决定下不下/下哪边(替代或叠加 z-score)。
3. **事件驱动消除轮询延迟**(用户2026-06-02提):现为轮询(POLL_MS=200,最坏晚200ms看到机会)。改成 WS 收到盘口更新即回调触发决策,看到机会延迟≈0。工程量较大(ws.rs更新cache后回调策略层)。下单往返~330ms是网络硬延迟降不动,但轮询延迟可消除。
4. **模型推理 Python→Rust**(方式A):正式上线时把 LightGBM 推理移入 bot(lleaves/ONNX),自包含+低延迟,去掉Python旁路。
5. TAKER_BUFFER 调优(减FAK扑空):现0.02,薄盘口偶尔扑空;调0.03-0.04更易成交,代价滑点+。观察扑空率再定。

## 关键参数现值(/opt/jy-data/.env)
- ENTRY_STRATEGY=ev_solo, DRY_RUN=0, EV_SOLO_QTY=20
- EV_SOLO_MIN_ASK=0.35, EV_SOLO_MAX_ASK=0.52(只在正EV价区入场)
- TAKER_BUFFER=0.02, POLL_MS=200, FORCE 7%费(真费,不动)

## 环境
- 机器:AWS Lightsail 爱尔兰($24/月),唯一机器,开发+安装+实盘全在此。
- 流程:本地 /opt/jiaoyi/jy 改 → push GitHub leosysd/JY_RUST → `jy update` 部署。
- 下单延迟实测~330ms(爱尔兰→Polymarket美东)。
- 熄火:`jy set-dry-run 1 --restart`。
