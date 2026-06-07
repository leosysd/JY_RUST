#!/usr/bin/env bash
# VPS 一键安装（预编译二进制版）：直接从 GitHub Release 下载 jy/jy-bot，无需在 VPS 编译。
# 用法：
#   curl -fsSL https://raw.githubusercontent.com/leosysd/JY_RUST/main/scripts/install-bin.sh | bash
# 可选环境变量：
#   JY_TAG=v0.1.0   指定版本（默认 latest 滚动版）
#   JY_DATA_DIR=/opt/jy-data   数据/配置目录（默认 /opt/jy-data）
set -euo pipefail

REPO="leosysd/JY_RUST"
TAG="${JY_TAG:-latest}"
DATA_DIR="${JY_DATA_DIR:-/opt/jy-data}"
SERVICE="jy-bot"

# 预编译产物是 x86_64-linux-gnu（glibc≥2.35，Ubuntu 22.04+/Debian 12+）。其它架构请用源码版 install.sh。
arch="$(uname -m)"
if [ "$arch" != "x86_64" ]; then
  echo "✗ 预编译仅提供 x86_64，当前为 $arch。请改用源码编译安装：" >&2
  echo "  curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/install.sh | bash" >&2
  exit 1
fi

echo "[1/5] 装最小依赖 (curl / ca-certificates)..."
if command -v apt-get >/dev/null 2>&1; then
  sudo apt-get update -qq
  sudo apt-get install -y -qq curl ca-certificates
fi

echo "[2/5] 下载预编译二进制 ($TAG)..."
base="https://github.com/$REPO/releases/download/$TAG"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fSL "$base/jy-bot" -o "$tmp/jy-bot"
curl -fSL "$base/jy"     -o "$tmp/jy"
# 有校验文件就校验
if curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" 2>/dev/null; then
  (cd "$tmp" && sha256sum -c SHA256SUMS) || { echo "✗ SHA256 校验失败" >&2; exit 1; }
  echo "  SHA256 校验通过"
fi
sudo install -m755 "$tmp/jy-bot" /usr/local/bin/jy-bot
sudo install -m755 "$tmp/jy"     /usr/local/bin/jy
echo "  已安装 /usr/local/bin/{jy,jy-bot}"

echo "[3/5] 建数据目录 $DATA_DIR ..."
sudo mkdir -p "$DATA_DIR/data" "$DATA_DIR/logs"
sudo chown -R "$(id -un):$(id -gn)" "$DATA_DIR"

echo "[4/5] 配置文件..."
if [ ! -f "$DATA_DIR/.env" ]; then
  curl -fsSL "https://raw.githubusercontent.com/$REPO/main/.env.example" -o "$DATA_DIR/.env"
  chmod 600 "$DATA_DIR/.env"
  echo "  已创建 $DATA_DIR/.env（默认 DRY_RUN=1 模拟，不下真单）"
else
  echo "  已存在 $DATA_DIR/.env，保留不动"
fi

echo "[5/5] 安装 systemd 服务..."
sudo tee /etc/systemd/system/${SERVICE}.service >/dev/null <<SVCEOF
[Unit]
Description=JY Bot (Rust) - Polymarket BTC 5m Up/Down
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=${DATA_DIR}
ExecStart=/usr/local/bin/jy-bot ${DATA_DIR}/.env
Restart=always
RestartSec=3
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
SVCEOF
sudo systemctl daemon-reload
sudo systemctl enable "$SERVICE"

cat <<DONE

✓ 安装完成（预编译版，未在本机编译）。

下一步：
  jy                          # 管理菜单：填钱包私钥、启动服务、看实时日志/统计
  sudo systemctl start jy-bot # 或直接启动后台服务

升级到最新版（重新拉二进制并重启）：
  curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/install-bin.sh | bash && sudo systemctl restart jy-bot
DONE
