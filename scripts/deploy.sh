#!/usr/bin/env bash
# am-dl 一键部署（幂等）
#
#   ./scripts/deploy.sh                      # 构建 + 装 systemd + 启动（无鉴权）
#   ./scripts/deploy.sh --root /mnt/music
#   ./scripts/deploy.sh --port 8890 --token abc123
#   ./scripts/deploy.sh --upstream http://127.0.0.1:8899   # 兼做上游 am-hook 的反向代理
#   ./scripts/deploy.sh --no-build           # 只重装服务
#   ./scripts/deploy.sh --uninstall
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVICE=am-dl
UNIT=/etc/systemd/system/${SERVICE}.service
TEMPLATE="$ROOT_DIR/deploy/${SERVICE}.service.in"

DL_ROOT="/root/music"
DL_ROOTS=""
LISTEN="0.0.0.0"
PORT="8888"
WRAPPER_URL="http://127.0.0.1:12340"
UPSTREAM=""
SET_ROOT=0; SET_ROOTS=0; SET_LISTEN=0; SET_PORT=0; SET_WRAPPER=0; SET_TOKEN=0; SET_UPSTREAM=0; SET_CONC=0
CONCURRENCY="2"
TOKEN=""
DO_BUILD=1
DO_START=1
DO_UNINSTALL=0

usage() { sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)        DL_ROOT="$2"; SET_ROOT=1; shift 2 ;;
    --roots)       DL_ROOTS="$2"; SET_ROOTS=1; shift 2 ;;
    --listen)      LISTEN="$2"; SET_LISTEN=1; shift 2 ;;
    --port)        PORT="$2"; SET_PORT=1; shift 2 ;;
    --upstream)    UPSTREAM="$2"; SET_UPSTREAM=1; shift 2 ;;
    --wrapper)     WRAPPER_URL="$2"; SET_WRAPPER=1; shift 2 ;;
    --concurrency) CONCURRENCY="$2"; SET_CONC=1; shift 2 ;;
    --token)       TOKEN="$2"; SET_TOKEN=1; shift 2 ;;
    --no-build)    DO_BUILD=0; shift ;;
    --no-start)    DO_START=0; shift ;;
    --uninstall)   DO_UNINSTALL=1; shift ;;
    -h|--help)     usage; exit 0 ;;
    *) echo "未知参数: $1" >&2; usage; exit 2 ;;
  esac
done

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m错误:\033[0m %s\n' "$*" >&2; exit 1; }
[[ $EUID -eq 0 ]] || die "需要 root 权限"

if [[ $DO_UNINSTALL -eq 1 ]]; then
  systemctl disable --now "$SERVICE" 2>/dev/null || true
  rm -f "$UNIT"
  systemctl daemon-reload
  echo "已移除 am-dl 服务（音乐文件保留）"
  exit 0
fi

if [[ $DO_BUILD -eq 1 ]]; then
  command -v cargo >/dev/null 2>&1 || { [[ -x "$HOME/.cargo/bin/cargo" ]] && source "$HOME/.cargo/env"; } || die "缺少 Rust 工具链"
  log "编译"
  ( cd "$ROOT_DIR" && cargo build --release )
  mkdir -p "$ROOT_DIR/dist"
  cp -f "$ROOT_DIR/target/release/am-dl" "$ROOT_DIR/dist/.am-dl.new"
  mv -f "$ROOT_DIR/dist/.am-dl.new" "$ROOT_DIR/dist/am-dl"
else
  [[ -x "$ROOT_DIR/dist/am-dl" ]] || die "dist/am-dl 不存在，去掉 --no-build"
fi

# 没传的参数沿用已安装单元里的值：重跑脚本不该把下载目录、令牌、上游地址改回默认
CUR_UNIT="$UNIT"
inherit() { # $1=环境变量名 $2=默认值
  local v=""
  [[ -f "$CUR_UNIT" ]] && v="$(sed -n "s/^Environment=$1=//p" "$CUR_UNIT" | head -1)"
  printf '%s' "${v:-$2}"
}
if [[ -f "$CUR_UNIT" ]]; then
  [[ $SET_ROOT     == 0 ]] && DL_ROOT="$(inherit AM_DL_ROOT /root/music)"
  [[ $SET_ROOTS    == 0 ]] && DL_ROOTS="$(inherit AM_DL_ROOTS '')"
  [[ $SET_TOKEN    == 0 ]] && TOKEN="$(inherit AM_DL_TOKEN '')"
  [[ $SET_UPSTREAM == 0 ]] && UPSTREAM="$(inherit AM_DL_UPSTREAM '')"
  [[ $SET_WRAPPER  == 0 ]] && WRAPPER_URL="$(inherit AM_DL_WRAPPER http://127.0.0.1:12340)"
  [[ $SET_CONC     == 0 ]] && CONCURRENCY="$(inherit AM_DL_CONCURRENCY 2)"
  cur_exec="$(grep -m1 '^ExecStart=' "$CUR_UNIT" || true)"
  # ExecStart 里是 --listen host:port 的合并形式，拆开取，避免每次重跑叠加 ":port"
  if [[ "$cur_exec" =~ --listen[[:space:]]+([^[:space:]]+) ]]; then
    _addr="${BASH_REMATCH[1]}"
    [[ $SET_LISTEN == 0 ]] && LISTEN="${_addr%%:*}"
    [[ $SET_PORT   == 0 ]] && PORT="${_addr##*:}"
  fi
fi

if [[ -n "$DL_ROOTS" ]]; then
  IFS=':' read -r -a ROOT_LIST <<< "$DL_ROOTS"
else
  ROOT_LIST=("$DL_ROOT")
fi
for r in "${ROOT_LIST[@]}"; do
  [[ -n "$r" ]] || continue
  mkdir -p "$r" || die "无法创建下载目录 $r"
done
DL_DEFAULT="${ROOT_LIST[0]}"

render() {
  python3 - "$@" <<'PY'
import sys
path, key, value = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path, encoding='utf-8').read().replace(key, value)
open(path, 'w', encoding='utf-8').write(text)
PY
}

TMP_UNIT="$(mktemp)"
cp "$TEMPLATE" "$TMP_UNIT"
if [[ -n "$DL_ROOTS" ]]; then
  render "$TMP_UNIT" '@ROOTS_LINE@' "Environment=AM_DL_ROOTS=$DL_ROOTS"
  render "$TMP_UNIT" '@ROOT_LINE@' "# 多根目录模式见 AM_DL_ROOTS"
else
  render "$TMP_UNIT" '@ROOTS_LINE@' "# 单根目录模式"
  render "$TMP_UNIT" '@ROOT_LINE@' "Environment=AM_DL_ROOT=$DL_DEFAULT"
fi
if [[ -n "$TOKEN" ]]; then
  render "$TMP_UNIT" '@TOKEN_LINE@' "Environment=AM_DL_TOKEN=$TOKEN"
else
  render "$TMP_UNIT" '@TOKEN_LINE@' "# 未设置访问令牌（无需鉴权）"
fi
render "$TMP_UNIT" '@CONCURRENCY@' "$CONCURRENCY"
render "$TMP_UNIT" '@UPSTREAM@' "$UPSTREAM"
render "$TMP_UNIT" '@WRAPPER@' "$WRAPPER_URL"
render "$TMP_UNIT" '@LISTEN@' "$LISTEN"
render "$TMP_UNIT" '@PORT@' "$PORT"
render "$TMP_UNIT" '@PROJECT@' "$ROOT_DIR"
render "$TMP_UNIT" '@RW_PATHS@' "${ROOT_LIST[*]}"

log "安装 $UNIT"
install -m 644 "$TMP_UNIT" "$UNIT"
rm -f "$TMP_UNIT"
systemctl daemon-reload

if [[ $DO_START -eq 1 ]]; then
  log "启动"
  systemctl enable "$SERVICE" >/dev/null 2>&1 || true
  systemctl restart "$SERVICE"
  BASE="http://127.0.0.1:${PORT}"
  QS=""
  [[ -n "$TOKEN" ]] && QS="?token=${TOKEN}"
  ok=0
  for _ in $(seq 1 30); do
    sleep 1
    # 必须确认应答的是 am-dl 自己：端口被别的服务占着时，/api/dl/config 也可能 200
    if curl -fsS -m 3 "${BASE}/api/dl/ping" 2>/dev/null | grep -q "am-dl"; then ok=1; break; fi
  done
  [[ $ok -eq 1 ]] || { journalctl -u "$SERVICE" -n 20 --no-pager || true; die "启动后 30 秒无响应"; }
  log "健康检查通过"
fi

IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
cat <<EOF

────────────────────────────────────────────
 am-dl 部署完成
   状态      $(systemctl is-active "$SERVICE" 2>/dev/null || echo unknown)
   二进制    $ROOT_DIR/dist/am-dl
   下载目录  ${ROOT_LIST[*]}
   监听      $LISTEN:$PORT
   鉴权      $([[ -n "$TOKEN" ]] && echo "令牌 $TOKEN" || echo "关闭")

   打开页面  http://${IP:-127.0.0.1}:${PORT}/dl$([[ -n "$TOKEN" ]] && echo "?token=${TOKEN}")
   日志      journalctl -u $SERVICE -f
   卸载      ./scripts/deploy.sh --uninstall
────────────────────────────────────────────
EOF
