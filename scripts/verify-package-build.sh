#!/bin/bash
# 本批包组合验证 —— 证明 .crate 解包后可独立构建（不指回原 workspace）。
#
# 这是「发布前验证」中最接近真实发布的一环。与 `cargo package --no-verify`
# （只组装包、跳过解包后构建）不同，本脚本真正执行「解包 .crate 并构建」：
#
#   1. 按依赖顺序 `cargo package` 每个 crate（--no-verify：只组装 .crate，不构建）
#   2. 解包全部 .crate 到临时目录
#   3. 用同批解包目录作 [patch.crates-io] 替换内部依赖（不指回原 workspace）
#   4. 构建一个依赖 facade(full) 的临时验证包 → 证明本批包组合可构建
#
# 明确边界（不夸大结论）：
#   - 证明「本批包组合」文件齐全、可编译（未发布内部依赖用同批解包目录替换）。
#   - 这 ≠ registry 验证：内部依赖尚不在 crates.io，真实 registry 解析
#     只能在正式发布顺序中做（发布 core 后才能验证 provider 对其的 registry 依赖）。
#
# 用法：bash scripts/verify-package-build.sh
# 退出码：0 = 本批包组合构建通过；1 = 打包/解包/构建失败
set -euo pipefail

CRATES="lellm-core lellm-derive lellm-provider lellm-graph lellm-mcp lellm-agent lellm"
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(grep '^version' "$PROJECT_ROOT/Cargo.toml" | head -1 | sed 's/.*= *"\([^"]*\)".*/\1/')"

# shell 全局 DYLD_LIBRARY_PATH（Homebrew llvm/sqlite）会劫持 rustc 1.98+ 的
# dylib 解析，导致 `dyld: missing symbol called` (SIGABRT)。打包验证不需要它。
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH

log() { echo -e "\033[0;36m[verify-pkg]\033[0m $*"; }
err() { echo -e "\033[0;31m[verify-pkg][ERROR]\033[0m $*" >&2; }

WORK="$(mktemp -d)"
cleanup() {
  # 按安全规则：不用 rm，移到废纸篓（失败时保留现场供排查）
  if [ -d "$HOME/.Trash" ]; then
    mv "$WORK" "$HOME/.Trash/lellm-pkg-verify-$(date +%s)" 2>/dev/null || true
  fi
}
trap cleanup EXIT

log "版本 v${VERSION}，宿主机默认工具链（裸 cargo）"

# ── 1. 打包（--no-verify：只组装 .crate；.crate 落在临时 target-dir，不污染共享缓存）──
log "[1/4] cargo package 各 crate（--no-verify，只打包不构建）..."
for c in $CRATES; do
  if ! cargo package -p "$c" --no-verify --target-dir "$WORK/pkg-target" >/dev/null 2>&1; then
    err "打包 $c 失败"
    exit 1
  fi
done
log "  打包完成：$CRATES"

# ── 2. 解包到临时目录 ──
log "[2/4] 解包 .crate ..."
mkdir -p "$WORK/extract"
for c in $CRATES; do
  crate_file="$WORK/pkg-target/package/$c-$VERSION.crate"
  if [ ! -f "$crate_file" ]; then
    err "缺少 ${crate_file}（打包可能未成功）"
    exit 1
  fi
  tar -xzf "$crate_file" -C "$WORK/extract"
done
log "  解包完成：$WORK/extract"

# ── 3. 临时验证包：依赖 facade(full)，[patch.crates-io] 指向同批解包目录 ──
# 关键：patch 指向「解包目录」而非原 workspace，保证验证的是打包内容本身。
log "[3/4] 生成验证包（[patch.crates-io] → 同批解包目录，不指回原 workspace）..."
mkdir -p "$WORK/verify/src"
cat > "$WORK/verify/Cargo.toml" <<EOF
[package]
name = "lellm-pkg-verify"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
lellm = { path = "$WORK/extract/lellm-$VERSION", features = ["full"] }

[workspace]

[patch.crates-io]
EOF
for c in $CRATES; do
  echo "$c = { path = \"$WORK/extract/$c-$VERSION\" }" >> "$WORK/verify/Cargo.toml"
done
echo "" > "$WORK/verify/src/lib.rs"

# ── 4. 构建验证包（full feature，证明本批包组合可构建）──
# 不指定 --target-dir：复用全局配置 target 的外部依赖缓存，只重编 7 个 lellm crate。
log "[4/4] 构建验证包（full feature）..."
BUILD_LOG="$WORK/build.log"
if (cd "$WORK/verify" && cargo build >"$BUILD_LOG" 2>&1); then
  log "✅ 本批包组合构建通过：解包后 .crate 可独立编译（full feature）"
  log "   边界：这是「本批包组合验证」，≠ registry 验证（内部依赖尚不在 crates.io）"
else
  err "❌ 本批包组合构建失败，最后 40 行："
  tail -40 "$BUILD_LOG" >&2 || true
  err "完整日志：${BUILD_LOG}（临时目录已保留至废纸篓供排查）"
  exit 1
fi
