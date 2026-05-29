#!/usr/bin/env bash
set -euo pipefail

REPO="https://github.com/leosysd/JY_RUST.git"
INSTALL_DIR="/opt/jy-rust"
DATA_DIR="/opt/jy-data"
SERVICE="jy-bot"

echo "[1/5] 安装系统依赖..."
sudo apt-get update -qq
sudo apt-get install -y -qq git build-essential pkg-config libssl-dev curl

echo "[2/5] 安装 Rust..."
if ! command -v cargo &>/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  source "$HOME/.cargo/env"
fi
source "$HOME/.cargo/env"
echo "Rust $(rustc --version)"

echo "[3/5] 克隆并编译..."
if [ -d "$INSTALL_DIR/.git" ]; then
  git -C "$INSTALL_DIR" pull --ff-only
else
  git clone "$REPO" "$INSTALL_DIR"
fi
cd "$INSTALL_DIR"
cargo build --release
sudo cp target/release/jy-bot /usr/local/bin/jy-bot
sudo cp target/release/jy     /usr/local/bin/jy

echo "[4/5] 创建数据目录..."
mkdir -p "$DATA_DIR/data" "$DATA_DIR/logs"
if [ ! -f "$DATA_DIR/.env" ]; then
  cp "$INSTALL_DIR/.env.example" "$DATA_DIR/.env"
  chmod 600 "$DATA_DIR/.env"
  echo "已创建配置文件: $DATA_DIR/.env"
fi

echo "[5/5] 安装 systemd 服务..."
sudo bash "$INSTALL_DIR/scripts/install-service.sh" "$DATA_DIR"

echo ""
echo "安装完成！运行以下命令开始配置："
echo ""
echo "  jy"
echo ""
echo "第一次建议按顺序操作："
echo "  1. 初始化/修改配置（填写钱包信息）"
echo "  4. 启动服务"
echo "  7. 查看实时日志"
