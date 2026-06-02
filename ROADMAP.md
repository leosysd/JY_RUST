# JY Bot 计划 / ROADMAP

> 唯一的成文计划。之前待办散在对话里,易丢,从此集中于此。
> 最后更新: 2026-06-02(加延迟/机房实测 + 事件驱动完成 + 实盘盈亏口径校准)

## 当前状态(一句话)
ev_solo 策略实盘运行中(爱尔兰 Lightsail,20份/场,DRY_RUN=0);同时攒 LightGBM 训练样本,够1000自动训。

## 核心结论(已用数据/数学锁定,别再走回头路)
- BTC 5分钟方向 ≈ 50%,7%费 → 任何"出场技巧/对冲/止盈止损/赔率配比"都做不出正期望(数学证明:对冲腿每份必亏;鞅过程止盈止损EV=0)。
- 唯一正期望路径 = **方向预测 >平衡线胜率** + 单边裸持。对冲、低价2:1、JetFadil追涨,全已证伪。
- z-score 方向实测 154场57.8%、ev_solo 模拟31场67.7%(p=0.035显著)→ **唯一活着的 edge 候选**,正在实盘验证。
- JetFadil 不是榜样(真实微亏、追涨、ROI1.1%,lb-api +15万是成交量积分非利润)。
- ⚠️ 盈亏只信链上口径(data-api activity 买入vs赎回),不信 quant_state(混了切实盘前模拟场,会虚高)。
  纯实盘(链上)截至2026-06-02约 -$80,但修复前的单受 Chainlink 开盘价 bug 污染(z方向变形),
  待 Chainlink 修复(已部署 UTC23:46)后的干净场才能判 ev_solo 真实 edge。

## 进行中
- [进行] ev_solo 实盘验证:20份/场,攒到 50-100 场看胜率是否稳 >55%(平衡线~52.6%)。薄样本,谨慎。
- [进行] 训练样本采集:bot 每盘记25+特征到 train_samples.jsonl,标签join settlement。目标1000,约3-4天。
- [自动] systemd timer `jy-train.timer` 每小时跑 train.py:样本<1000跳过,≥1000自动训 LightGBM+isotonic校准+阈值过滤,导出 /opt/jy-data/model/。

## 已完成
- [✅ 2026-06-02] **事件驱动消除轮询延迟**:ws.rs 加 book_updated Notify,盘口更新即唤醒主循环
  决策(select!{ws信号|兜底poll},min_gap=50ms节流)。看到机会延迟从最坏200ms→≈0。扑空率随之
  从~50%降到~29%。
- [✅ 2026-06-02] **Chainlink 开盘价 bug 修复**:HISTORY_SEC 180→420,at_ts 加5s容差返回None,
  smart.rs 去掉 .or_else(latest) 兜底(取不到真开盘价就跳过不入场)。
- [✅] 默认配置 ev_solo+POLL_MS200;删 copy 模式。

## 待做(按优先级)
1. **LightGBM 影子模式**:模型出来后,bot 加载 model.txt,每盘预测并记录"模型预测 vs 实际赢家",不下单。验证模型胜率是否 > z-score baseline。
2. **模型接管下注**:影子验证有效后,让模型预测+阈值决定下不下/下哪边(替代或叠加 z-score)。
3. **【低优先,等edge确认后再说】机房延迟优化**:traceroute查清真相——
   clob.polymarket.com 走 Cloudflare CDN,两台VPS到本地Cloudflare边缘都<1ms(无绕路);
   234ms(爱尔兰)vs 155ms(加拿大温哥华)的差,100%来自"Cloudflare边缘→Polymarket后端"的回源:
   Polymarket后端在北美,欧洲边缘要跨大西洋回源故慢。结论:机房要选离Polymarket后端(北美)近的,
   理论最优=美东VPS(可能<100ms),温哥华次优(155ms),爱尔兰最差(234ms)。
   **但延迟只影响扑空率,不影响方向对错=不是从亏变赚的关键。先确认ev_solo有edge,有才值得迁。**
   加拿大机=vital-wall-2/温哥华(非东部),密码已在对话暴露需改。
4. **模型推理 Python→Rust**(方式A):正式上线时把 LightGBM 推理移入 bot(lleaves/ONNX),自包含+低延迟,去掉Python旁路。
5. TAKER_BUFFER 调优(减FAK扑空):现0.02;若扑空仍高可调0.03-0.04更易成交,代价滑点+。

## 关键参数现值(/opt/jy-data/.env)
- ENTRY_STRATEGY=ev_solo, DRY_RUN=0, EV_SOLO_QTY=20
- EV_SOLO_MIN_ASK=0.35, EV_SOLO_MAX_ASK=0.52(只在正EV价区入场)
- TAKER_BUFFER=0.02, POLL_MS=200, FORCE 7%费(真费,不动)

## 环境
- 当前机器:AWS Lightsail 爱尔兰($24/月),开发+安装+实盘全在此。
- 备选机器:加拿大 VPS(vital-wall-2),实测到 Polymarket 更快(见待做#3),待决策是否迁。
- 流程:本地 /opt/jiaoyi/jy 改 → push GitHub leosysd/JY_RUST → `jy update` 部署。
- 延迟实测(同 /time 接口 TTFB):爱尔兰234ms / 加拿大~150ms。下单往返爱尔兰~330ms。
- 熄火:`jy set-dry-run 1 --restart`。
