# JY Bot (Rust)

Polymarket BTC 5 分钟 Up/Down 市场高频锁利机器人，复刻 JetFadil 策略。

**核心策略：**
1. 每个 5 分钟盘口开盘 **30 秒内**，价格在 0.44~0.65 时买入一边（固定 20 份）
2. 持续监控，等另一边跌到使 **总成本 × 1.07 < 0.85**（利润 ≥ 15%）时立刻买入另一边
3. 两边到期必赢其一，**零方向风险锁定利润**

数据来源：对 JetFadil 钱包 14 小时 3,500 笔交易的完整分析
- 81% 盘口成功锁利
- 平均锁利利润 **+29.4%**
- 最高单盘口 **+67.9%**

---

## 快速安装

### 前提

- VPS：Ubuntu / Debian（推荐 1 核 1GB 以上）
- 有 sudo 权限

### 一键安装

```bash
curl -fsSL https://raw.githubusercontent.com/leosysd/JY_RUST/main/scripts/install.sh | bash
```

安装完成后运行：

```bash
jy
```

---

## 手动安装

### 1. 安装 Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

### 2. 安装系统依赖

```bash
sudo apt-get update && sudo apt-get install -y git build-essential pkg-config libssl-dev
```

### 3. 克隆并编译

```bash
git clone https://github.com/leosysd/JY_RUST.git /opt/jy-rust
cd /opt/jy-rust
cargo build --release
```

### 4. 安装命令

```bash
sudo cp target/release/jy-bot /usr/local/bin/jy-bot
sudo cp target/release/jy     /usr/local/bin/jy
```

### 5. 创建配置文件

```bash
mkdir -p /opt/jy-data
cp .env.example /opt/jy-data/.env
chmod 600 /opt/jy-data/.env
```

### 6. 安装 systemd 服务

```bash
sudo bash scripts/install-service.sh
```

---

## 首次配置

```bash
jy
```

选择 **1. 初始化/修改配置**，按提示填写：

| 参数 | 说明 |
|------|------|
| `QUANT_STRATEGY` | 策略：`jetfadil`（锁利）/ `copy`（跟单） |
| `PRIVATE_KEY` | 交易钱包私钥（实盘必填，模拟可留空） |
| `DEPOSIT_WALLET_ADDRESS` | Polymarket 个人资料页的 `0x...` 地址 |
| `DRY_RUN` | `1` = 模拟，`0` = 实盘 |
| `TARGET_WALLET` | 跟单目标地址（仅 copy 模式） |

**建议先用 `DRY_RUN=1` 跑 24 小时观察信号，再切实盘。**

---

## 常用命令

```bash
jy                    # 打开交互式管理菜单
jy status             # 查看服务状态
jy start              # 启动服务
jy stop               # 停止服务
jy restart            # 重启服务
jy logs               # 查看实时日志
jy set-dry-run 0      # 切换实盘模式（需先配置私钥）
jy set-dry-run 1      # 切换模拟模式
jy set-bot-mode copy  # 切换为跟单模式
```

---

## 策略说明

### jetfadil 锁利策略（推荐）

复刻 JetFadil（Polymarket 头部交易者）的锁利操作：

```
盘口开始（T-300s）BTC 价格平稳，Up/Down 定价约 50/50
  ↓ 开盘 30s 内买一边 @ 0.50~0.60
  ↓ 持续监控价格
  ↓ BTC 出现单边大幅移动
  ↓ 另一边跌到 ≤0.25
  ↓ 立刻买入另一边（锁利单）
  → 总成本 × 1.07 < 1.0 → 到期无论涨跌稳赚
```

### copy 跟单策略

实时监听目标钱包（默认 JetFadil）的每笔成交，按比例自动跟单。

---

## 目录结构

```
/opt/jy-rust/          源码目录
  src/
    main.rs            后台服务入口
    config.rs          配置加载
    state.rs           状态持久化
    clob.rs            Polymarket CLOB API
    ws.rs              WebSocket 盘口缓存
    signing.rs         EIP-712 签名（实盘下单）
    strategy/
      jetfadil.rs      JetFadil 锁利策略
      copy.rs          跟单策略
    cli/
      main.rs          jy 命令行管理工具

/opt/jy-data/          数据目录（不含私钥）
  .env                 配置文件（600 权限）
  quant_state.json     策略状态
  data/                信号记录
  logs/                日志
```

---

## 更新

```bash
cd /opt/jy-rust
git pull
cargo build --release
sudo cp target/release/jy-bot /usr/local/bin/jy-bot
sudo cp target/release/jy     /usr/local/bin/jy
jy restart
```

---

## 安全提醒

- 私钥只保存在 VPS 的 `.env` 文件中，权限设为 `600`
- 不要截图、复制、上传私钥
- 实盘前务必先用 `DRY_RUN=1` 验证策略正常运行
- `.env` 文件已加入 `.gitignore`，不会被上传到 GitHub
