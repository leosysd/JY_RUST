# JY Bot 交接文档(新会话/迁移机器读这份)

> 生成 2026-06-02。配合 ROADMAP.md 一起看。代码权威源 = GitHub leosysd/JY_RUST(commit 5473ec5)。

## 一、当前项目状态(一句话)
**ev_solo 策略实盘运行中**(AWS Lightsail 爱尔兰,真金 DRY_RUN=0,20份/场)。
**核心验证结果:Chainlink bug 修复后 67 场,胜率 62.7%,二项检验 p=0.025 显著,前后半 63%/61% 稳定**
→ ev_solo 是目前唯一被数据证实有真 edge 的策略(平衡线只需 52.6%,实测远超)。

## 二、策略是什么
- 标的:Polymarket BTC 5分钟 Up/Down(`btc-updown-5m`)。
- **ev_solo**:z-score 定方向 → 只在该方向 ask∈[0.35,0.52] 时 taker 买单边 → 不对冲/不锁利/裸持到结算。
  - 靠 z-score 方向胜率(>平衡线)赚正期望。盈亏比由入场价定(约1:1),靠胜率不靠盈亏比。
- 为什么是这个:对冲/低价2:1/止盈止损 全被数学+数据证伪(见 ROADMAP 核心结论);唯一正期望路径=方向预测+单边。
- baseline:zscore(完整锁利/追单/减险,留作对比,没在跑)。

## 三、已完成的改动(全在 GitHub,按时间)
1. 删死策略 dual_hedge/maker_scalein/log_book(-343行)
2. ev_solo 策略 + CLI 开关 + 调参命令(jy params / set-param)
3. 每盘记训练样本(train_samples.jsonl,25+特征)+ train.py(LightGBM,够1000自动训)
4. **事件驱动**消除轮询延迟(ws.rs notify→主循环select);扑空率 50%→20%
5. **Chainlink开盘价bug修复**(关键):HISTORY_SEC 180→420,at_ts加5s容差返回None,
   去掉 smart.rs 的 .or_else(latest) 兜底(取不到真开盘价就跳过)。修前实盘被污染过。
6. 默认配置改 ev_solo+POLL_MS200;删 copy 模式;.env.example 重写
7. **token订阅累积bug修复**:ensure_subscribed 改替换订阅集(原只增不减,累积24个致WS卡死停单)

## 四、关键参数(/opt/jy-data/.env;未列项=用代码默认)
- BOT_MODE=quant, DRY_RUN=0(实盘!), ENTRY_STRATEGY=ev_solo, POLL_MS=200
- DEPOSIT_WALLET_ADDRESS=0xE690DA4ce6FbECf7bef11648D504b43e3620B11E, PRIVATE_KEY=(已设)
- 代码默认(.env未显式写): EV_SOLO_QTY=20, EV_SOLO_MIN_ASK=0.35, EV_SOLO_MAX_ASK=0.52, TAKER_BUFFER=0.02
- 费率=真7%(position.rs写死0.07*p*(1-p),别动)
- 熄火: `jy set-dry-run 1 --restart`

## 五、环境与运维
- 机器:AWS Lightsail 爱尔兰($24),开发(/opt/jiaoyi/jy)+安装(/opt/jy-rust)+数据(/opt/jy-data)全在此一台。
- 部署流程:本地 /opt/jiaoyi/jy/JY/bot-rs 改 → push GitHub → 实盘机 `jy update`(拉+编译+重启)。
- ML: /opt/jy-data/mlenv venv(lightgbm4.6);systemd `jy-train.timer` 每小时跑 train.py。
- Rust: cargo 1.96 在 ~/.cargo。
- 监控:每小时对账(链上 data-api activity vs bot日志)+ 修复后胜率跟踪。
- 盈亏口径警告:**只信链上**(data-api activity 买入vs赎回);quant_state 混过模拟场会虚高,胜率可信、美元盈亏要链上核对。

## 六、迁移到新机器步骤(如迁加拿大/美东)
1. 新机:`sudo mkdir -p /opt/jy-rust /opt/jy-data; sudo chown -R $(id -un):$(id -gn) /opt/jy-rust /opt/jy-data`
2. `curl -fsSL https://raw.githubusercontent.com/leosysd/JY_RUST/main/scripts/install.sh | bash`
3. 装ML: `python3 -m venv /opt/jy-data/mlenv && /opt/jy-data/mlenv/bin/pip install lightgbm scikit-learn numpy`
4. 配 .env:填 PRIVATE_KEY/DEPOSIT_WALLET_ADDRESS,设 DRY_RUN=0(确认要实盘),ENTRY_STRATEGY=ev_solo
   (注意 install.sh 缺sudo建目录,第1步已手动建好)
5. 装训练timer(复制本机 /etc/systemd/system/jy-train.{service,timer},enable)
6. **关键:同一钱包不能两台同时实盘!** 迁移=先在旧机 `jy set-dry-run 1 --restart` 停实盘,再启新机。
7. 训练样本可选迁移:scp 旧机 /opt/jy-data/data/{books,quant_signals.jsonl} 到新机,接着攒。
8. 延迟参考(同/time接口TTFB):爱尔兰234ms / 加拿大温哥华155ms / 美东最优(<100ms,理论)。

## 七、下一步任务(优先级)
1. **继续攒修复后场到100+**,确认62.7% edge 稳定(目前67场p=0.025已显著)。
2. **减扑空增利**:edge已确认,可调 TAKER_BUFFER 0.02→0.03,扑空20%→~10%,利润预计+15-20%。
   (扑空17次≈漏掉~$30利润;扑空不亏钱只漏机会)
3. **链上核对真实美元盈亏**(state口径+$143不一定准)。
4. LightGBM:样本146/1000,够1000自动训→影子模式→接管下注(ROADMAP#1,2)。
5. 机房延迟优化(低优先,edge确认后):迁美东/温哥华降延迟减扑空。

## 八、给新会话AI的提醒
- 用户多次凭直觉抓出我的统计/判断错误(报漏场数、读错lb-api、误判机房)——**报数前务必核实口径,别报喜过头**。
- 改代码=本地改→push→jy update,**绝不直接动安装目录运行文件**。
- 改 .env 必须 jy restart 才生效(启动时一次性读)。
- 重启前确认无进行中持仓(或接受打断1盘,重启会认回仓+结算走HTTP不受影响)。
- 温哥华VPS密码曾在对话暴露,提醒用户改(IP 65.49.232.140)。
