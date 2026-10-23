#!/usr/bin/env bash
# 检查已下载的文件有没有解密错误：逐个用 ffmpeg 完整解码，报错的就是坏的。
#
#   ./scripts/check-audio.sh /mnt/disk1/AM-DL
#   ./scripts/check-audio.sh /mnt/disk1/AM-DL --fast   # 只解前 60 秒
#
# 解密错误的表现：解码器报 "invalid samples per frame" / "Reserved bit set" /
# "Error : N bits left"，或整段静音。v0.1.2 之前下载的文件（尤其 ALAC / AAC）
# 建议都跑一遍。
set -uo pipefail

ROOT="${1:-.}"
MODE="${2:-}"
[[ -d "$ROOT" ]] || { echo "目录不存在: $ROOT" >&2; exit 2; }
command -v ffmpeg >/dev/null || { echo "需要 ffmpeg" >&2; exit 2; }

TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

total=0
bad=0
find "$ROOT" -type f \( -name '*.m4a' -o -name '*.mp4' \) -print0 |
  while IFS= read -r -d '' f; do
    total=$((total + 1))
    if [[ "$MODE" == "--fast" ]]; then
      err="$(ffmpeg -v error -t 60 -i "$f" -f null - 2>&1 | head -2)"
    else
      err="$(ffmpeg -v error -i "$f" -f null - 2>&1 | head -2)"
    fi
    if [[ -n "$err" ]]; then
      bad=$((bad + 1))
      printf 'BAD  %s\n     %s\n' "$f" "$(printf '%s' "$err" | head -1)"
    fi
    printf '%d %d\n' "$total" "$bad" >"$TMP"
  done

read -r total bad <"$TMP" 2>/dev/null || { total=0; bad=0; }
echo "──────────────"
echo "检查 $total 个文件，$bad 个有解码错误"
[[ "$bad" -eq 0 ]]
