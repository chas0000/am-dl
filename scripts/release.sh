#!/usr/bin/env bash
# 本地发版：改 Cargo.toml 版本 → 提交 → 打 tag → 推送。
# 推 tag 之后 GitHub Actions 会自动构建并发布 Release（见 .github/workflows/release.yml）。
#
#   ./scripts/release.sh 0.2.0        # 发 v0.2.0
#   ./scripts/release.sh v0.2.0       # 等价
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

die() { printf '\033[1;31m错误:\033[0m %s\n' "$*" >&2; exit 1; }
log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

VERSION="${1:-}"
[[ -n "$VERSION" ]] || die "用法: ./scripts/release.sh <版本号>，例如 0.2.0"
VERSION="${VERSION#v}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] || die "版本号不是 semver: $VERSION"

git rev-parse --git-dir >/dev/null 2>&1 || die "当前目录不是 git 仓库"
[[ -z "$(git status --porcelain)" ]] || die "工作区不干净，先提交或 stash"
git rev-parse "v$VERSION" >/dev/null 2>&1 && die "tag v$VERSION 已存在"

log "版本 $VERSION"
sed -i -E "0,/^version = \".*\"\$/s//version = \"$VERSION\"/" Cargo.toml
cargo update -p am-dl --offline >/dev/null

log "构建校验（fmt / clippy / test）"
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
AM_DL_VERSION_OVERRIDE="v$VERSION" cargo build --release --locked >/dev/null
GOT="$(./target/release/am-dl --version)"
[[ "$GOT" == "am-dl v$VERSION" ]] || die "二进制版本不对: $GOT"
log "二进制报告: $GOT"

log "提交并打 tag"
git add Cargo.toml Cargo.lock
git commit -q -m "chore: 版本 $VERSION"
git tag -a "v$VERSION" -m "v$VERSION"

log "推送（推 tag 后 CI 自动构建发布）"
git push origin HEAD
git push origin "v$VERSION"

log "完成：https://github.com/$(git remote get-url origin | sed -E 's#.*github.com[:/]([^/]+/[^/.]+)(\.git)?#\1#')/releases/tag/v$VERSION"
