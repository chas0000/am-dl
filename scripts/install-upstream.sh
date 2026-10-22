#!/usr/bin/env bash
# 安装 / 更新上游 am-hook 官方预编译二进制，并装成 systemd 服务 am-hook.service。
#
#   ./scripts/install-upstream.sh              # 装最新版
#   ./scripts/install-upstream.sh v0.5.7       # 装指定版本
#   PORT=8899 ./scripts/install-upstream.sh    # 换端口
#   BIN_DIR=/srv/am-hook ./scripts/install-upstream.sh   # 换安装目录
#
# 这个服务只监听 127.0.0.1，对外由 am-dl 反向代理（见 deploy.sh --upstream）。
set -euo pipefail

BIN_DIR="${BIN_DIR:-/opt/am-hook-bin}"
PORT="${PORT:-8899}"
LISTEN="${LISTEN:-127.0.0.1}"
WRAPPER="${WRAPPER:-http://127.0.0.1:12340}"
REF="${1:-latest}"
SERVICE=am-hook

die() { echo "错误: $*" >&2; exit 1; }
[[ $EUID -eq 0 ]] || die "请用 root 运行"

if [[ "$REF" == "latest" ]]; then
  echo "==> 查询最新版本"
  URL=$(curl -fsSL https://api.github.com/repos/itouakirai/am-hook/releases/latest |
    python3 -c 'import json,sys;d=json.load(sys.stdin);print([a["browser_download_url"] for a in d["assets"] if a["name"]=="am-hook-linux-x86_64"][0])')
else
  URL="https://github.com/itouakirai/am-hook/releases/download/$REF/am-hook-linux-x86_64"
fi
echo "==> 下载 $URL"
mkdir -p "$BIN_DIR"
curl -fsSL -o "$BIN_DIR/am-hook.new" "$URL" || die "下载失败"
chmod +x "$BIN_DIR/am-hook.new"
VER="$("$BIN_DIR/am-hook.new" --version 2>&1 | head -1)"
echo "==> 校验: $VER"

# 先停服务再替换，避免 Text file busy
systemctl stop "$SERVICE" 2>/dev/null || true
mv -f "$BIN_DIR/am-hook.new" "$BIN_DIR/am-hook"

cat > "/etc/systemd/system/$SERVICE.service" <<EOF
[Unit]
Description=am-hook (Apple Music 播放器 + wrapper-lite 转发，预编译版)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
WorkingDirectory=$BIN_DIR
ExecStart=$BIN_DIR/am-hook --listen $LISTEN:$PORT --wrapper-url $WRAPPER
Restart=always
RestartSec=3
LimitNOFILE=65535
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=yes
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now "$SERVICE" >/dev/null
sleep 2
for _ in $(seq 1 15); do
  if curl -fsS -m 3 -o /dev/null "http://$LISTEN:$PORT/"; then
    echo "==> 服务已就绪: http://$LISTEN:$PORT  ($VER)"
    echo "==> 注意：对外请通过 am-dl 的端口访问（它会注入下载浮窗）"
    exit 0
  fi
  sleep 1
done
die "服务启动后 15 秒内没有响应，查看: journalctl -u $SERVICE -n 30"
