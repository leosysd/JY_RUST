#!/usr/bin/env bash
set -euo pipefail

DATA_DIR="${1:-/opt/jy-data}"
SERVICE="jy-bot"

sudo tee /etc/systemd/system/${SERVICE}.service > /dev/null << SVCEOF
[Unit]
Description=JY Bot (Rust) - Polymarket 高频锁利机器人
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
echo "服务 $SERVICE 已安装，开机自启。"
echo "使用 'jy start' 启动服务。"
