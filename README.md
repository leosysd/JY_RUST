# JY Bot (Rust)

Polymarket BTC 5 分钟 Up/Down 自动交易机器人。自带命令行工具 `jy` 管理服务、配置与统计。

---

## 安装

### 方式 A：预编译二进制（推荐，秒装，VPS 不用编译）

代码由 GitHub Actions 自动编译好并发布到 Release，VPS 直接下载二进制即可。
**适用 x86_64 + Ubuntu 22.04+/Debian 12+（glibc ≥ 2.35）。** 一条命令：

```bash
curl -fsSL https://raw.githubusercontent.com/leosysd/JY_RUST/main/scripts/install-bin.sh | bash
```

脚本会：下载 `jy`/`jy-bot`（带 SHA256 校验）→ 装到 `/usr/local/bin` → 建 `/opt/jy-data` 与 `.env` → 注册 systemd 服务。
默认装滚动版 `latest`；要装指定版本：`JY_TAG=v0.1.0 curl ... | bash`。

升级（重新拉最新二进制并重启）：

```bash
curl -fsSL https://raw.githubusercontent.com/leosysd/JY_RUST/main/scripts/install-bin.sh | bash && sudo systemctl restart jy-bot
```

### 方式 B：源码编译安装（其它架构 / 想自己编）

在 VPS 上装 Rust 并从源码编译（约 5–8 分钟）：

```bash
# 1. 先建好目录（安装脚本不会自动建，必须先手动建，否则报 Permission denied）
sudo mkdir -p /opt/jy-rust /opt/jy-data
sudo chown -R "$(id -un):$(id -gn)" /opt/jy-rust /opt/jy-data

# 2. 一键安装（约 5–8 分钟，含编译）
curl -fsSL https://raw.githubusercontent.com/leosysd/JY_RUST/main/scripts/install.sh | bash
```

装完后：

| 路径 | 内容 |
|------|------|
| `/opt/jy-data` | 配置 `.env`、状态 `quant_state.json`、日志、盘口数据 |
| `/usr/local/bin/jy` | 命令行工具 |
| `/usr/local/bin/jy-bot` | 后台交易进程（systemd 服务 `jy-bot`）|
| `/opt/jy-rust` | 源码（仅方式 B 源码安装时存在）|

> 默认 `DRY_RUN=1`（模拟，不下真单）。实盘前需先填钱包私钥并切到 `DRY_RUN=0`。
>
> **更新**：`jy update`（或菜单 14）现在直接下载 GitHub 预编译二进制并重启，秒级完成、无需在 VPS 编译；
> 不论你当初用方式 A 还是 B 装的都能用。要源码编译更新：在 `/opt/jy-rust` 执行 `git pull && cargo build --release`。

---

## 使用

直接运行 `jy` 打开交互菜单，或用子命令：

```bash
jy                     # 交互式管理菜单

# 配置
jy set-dry-run 1|0            # 1=模拟  0=实盘（实盘需先填 PRIVATE_KEY）
jy set-bot-mode quant|copy    # quant=量化  copy=跟单
jy set-entry-strategy zscore|maker_scalein   # 入场策略
jy params                     # 查看所有可调策略参数
jy set-param <KEY> <VALUE> [--restart]        # 改单个参数，如 jy set-param SCALEIN_QTY 5

# 服务
jy start | stop | restart | status
jy logs                       # 查看日志
jy update                     # 从 GitHub 拉最新代码并重新编译部署

# 统计
jy stats                      # 交易统计表
jy stats --diff               # 真实账 vs 理想账对比（实盘双轨）
jy clear                      # 清空状态数据
```

首次实盘上手：

```bash
jy                     # 菜单 1 填写 PRIVATE_KEY / DEPOSIT_WALLET_ADDRESS
jy set-dry-run 0       # 切实盘
jy start               # 启动
jy logs                # 看实时成交
```

紧急停止真金下单：`jy set-dry-run 1 --restart`

---

## 卸载

```bash
# 停止并移除服务
sudo systemctl stop jy-bot
sudo systemctl disable jy-bot
sudo rm -f /etc/systemd/system/jy-bot.service
sudo systemctl daemon-reload

# 删除程序
sudo rm -f /usr/local/bin/jy /usr/local/bin/jy-bot

# 删除代码与数据（⚠️ /opt/jy-data 含 .env 私钥和交易记录，确认后再删）
sudo rm -rf /opt/jy-rust /opt/jy-data
```
