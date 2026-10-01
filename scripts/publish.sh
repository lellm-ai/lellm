#!/bin/bash
# LeLLM Workspace 发布脚本
#
# 用法:
#   ./scripts/publish.sh              # 默认：仅验证（不发布、不打 tag、不 bump 版本）
#   ./scripts/publish.sh --publish    # 正式发布到 crates.io（需 CARGO_REGISTRY_TOKEN）
#
# 验证（两种模式都做），任一步失败即返回非零：
#   1. 工作区干净检查（git status --porcelain 为空 —— 发布包必须与 commit 对应）
#   2. workspace 构建 + 测试 + 针对性 feature 矩阵（非 --all-features，覆盖易碎组合）
#   3. 本批包组合验证（scripts/verify-package-build.sh：打包+解包+构建）
#
# 发布（仅 --publish）：
#   4. 按依赖顺序发布；每个 crate 先做「包名+确切版本」三态检查
#      （确认存在→跳过；确认不存在→发布；查询失败→中止，不当作未发布）；
#      发布后做有上限的可见性重试，依赖它下游的 crate 在其可解析前不发布。
#
# 明确边界：
#   - 验证模式的「本批包组合验证」≠ registry 验证（内部依赖尚不在 crates.io）。
#   - 实际发布时，registry 验证随发布顺序自然完成（先发 core，再发依赖它的 crate）。
#   - 不使用 --no-verify / --allow-dirty；不 rm -rf 任何路径；完整日志落 logs/。
set -euo pipefail

CRATES="lellm-core lellm-derive lellm-provider lellm-graph lellm-mcp lellm-agent lellm"
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(grep '^version' "$PROJECT_ROOT/Cargo.toml" | head -1 | sed 's/.*= *"\([^"]*\)".*/\1/')"
TOOLCHAIN="${CARGO_TOOLCHAIN:-1.88.0}"

MODE="verify"
[ "${1:-}" = "--publish" ] && MODE="publish"

LOG_DIR="$PROJECT_ROOT/logs/publish-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$LOG_DIR"
LOG_SEQ=0

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log_info() { echo -e "${CYAN}[publish]${NC}  $*"; }
log_ok()   { echo -e "${GREEN}[publish][OK]${NC}   $*"; }
log_warn() { echo -e "${YELLOW}[publish][WARN]${NC}  $*"; }
log_err()  { echo -e "${RED}[publish][ERROR]${NC} $*" >&2; }

# ─── registry 查询（sparse index，非 crates.io API）─────────────────

# 目标 registry 的 sparse index 基址：env 覆盖 > cargo 配置推导 > 回退 rsproxy。
get_index_base() {
  if [ -n "${LELLM_REGISTRY_INDEX:-}" ]; then
    echo "${LELLM_REGISTRY_INDEX%/}"; return
  fi
  local cfg="${CARGO_HOME:-$HOME/.cargo}/config.toml"
  [ -f "$cfg" ] || cfg="$HOME/.cargo/config.toml"
  local src reg
  src=$(grep -A3 '^\[source\.crates-io\]' "$cfg" 2>/dev/null | grep -E '^\s*replace-with' \
        | sed -E "s/.*replace-with[[:space:]]*=[[:space:]]*['\"]?//; s/['\"].*//" || true)
  if [ -n "$src" ]; then
    reg=$(grep -A3 "^\[source\.${src}\]" "$cfg" 2>/dev/null | grep -E '^\s*registry' \
          | sed -E "s/.*registry[[:space:]]*=[[:space:]]*['\"]?//; s/['\"].*//" || true)
    reg="${reg#sparse+}"
  fi
  if [ -n "$reg" ]; then echo "${reg%/}"; else echo "https://rsproxy.cn/index"; fi
}

# 查询「包名+确切版本」是否存在于目标 registry。三态：
#   0=确认存在；1=确认不存在；2=查询失败（网络/解析 —— 不可当作不存在）。
check_version_exists() {
  local crate="$1" version="$2"
  local name="${crate}" path
  case ${#name} in
    1) path="1/${name}" ;;
    2) path="2/${name}" ;;
    3) path="3/${name:0:1}/${name}" ;;
    *) path="${name:0:2}/${name:2:2}/${name}" ;;
  esac
  local base url body
  base="$(get_index_base)"
  url="${base%/}/${path}"
  if ! body="$(curl -fsS --max-time 20 "$url" 2>/dev/null)"; then
    return 2  # 网络/HTTP 失败 → 查询失败
  fi
  if printf '%s\n' "$body" | grep -qE "\"vers\"[[:space:]]*:[[:space:]]*\"${version}\""; then
    return 0  # 确认存在
  else
    return 1  # 确认不存在
  fi
}

# 发布后等待 crate 在 registry 可解析（有上限重试）。0=可解析；1=耗尽仍不可解析。
wait_until_resolvable() {
  local crate="$1" version="$2"
  local max="${LELLM_PUBLISH_RETRY:-12}" delay="${LELLM_PUBLISH_RETRY_DELAY:-15}" i rc
  for i in $(seq 1 "$max"); do
    rc=0
    check_version_exists "$crate" "$version" || rc=$?
    if [ "$rc" = "0" ]; then
      log_ok "  [${crate}] v${version} 已在 registry 可解析"
      return 0
    elif [ "$rc" = "1" ]; then
      log_info "  [${crate}] v${version} 尚未可见（${i}/${max}），${delay}s 后重试..."
      sleep "$delay"
    else
      log_err "  [${crate}] 查询 registry 失败（网络/解析），中止以避免误判"
      return 1
    fi
  done
  log_err "  [${crate}] 重试 ${max} 次后仍不可解析"
  return 1
}

# ─── 验证步骤 ──────────────────────────────────────────────────────

# 运行一条命令：完整日志落 LOG_DIR，成功显示通过，失败显示尾行并返回非零。
run_log() {
  local label="$1"; shift
  LOG_SEQ=$((LOG_SEQ + 1))
  local logf="$LOG_DIR/${LOG_SEQ}.log"
  log_info "  → ${label}"
  if "$@" >"$logf" 2>&1; then
    log_ok "    ${label} 通过"
  else
    log_err "    ${label} 失败，最后 30 行："
    tail -30 "$logf" >&2 || true
    log_err "    完整日志：${logf}"
    return 1
  fi
}

run_verification() {
  log_info "[1/3] 工作区干净检查（发布包须与 commit 对应）..."
  local dirty
  dirty="$(git -C "$PROJECT_ROOT" status --porcelain)"
  if [ -n "$dirty" ]; then
    log_err "工作区不干净，验证/发布要求干净工作区："
    printf '%s\n' "$dirty" | head -20 >&2
    return 1
  fi
  log_ok "工作区干净"

  log_info "[2/3] workspace 构建 + 测试 + 针对性 feature 矩阵（非 --all-features）..."
  run_log "workspace build"            cargo +"$TOOLCHAIN" build --workspace
  run_log "workspace test"             cargo +"$TOOLCHAIN" test --workspace
  run_log "provider 默认构建"           cargo +"$TOOLCHAIN" build -p lellm-provider
  run_log "facade 默认构建"             cargo +"$TOOLCHAIN" build -p lellm
  run_log "facade mcp-stdio(关默认)"    cargo +"$TOOLCHAIN" build -p lellm --no-default-features --features mcp-stdio
  run_log "facade mcp"                 cargo +"$TOOLCHAIN" build -p lellm --features mcp
  run_log "facade full"                cargo +"$TOOLCHAIN" build -p lellm --features full
  run_log "mcp sse"                    cargo +"$TOOLCHAIN" build -p lellm-mcp --features sse

  log_info "[3/3] 本批包组合验证（打包+解包+构建，≠ registry 验证）..."
  if ! bash "$PROJECT_ROOT/scripts/verify-package-build.sh" >"$LOG_DIR/pkg-combo.log" 2>&1; then
    log_err "本批包组合验证失败，最后 30 行："
    tail -30 "$LOG_DIR/pkg-combo.log" >&2 || true
    log_err "完整日志：${LOG_DIR}/pkg-combo.log"
    return 1
  fi
  log_ok "本批包组合验证通过"

  log_ok "验证完成：workspace 可构建/可测试 + 本批包组合可构建（≠ registry 验证）"
}

# ─── 发布步骤（仅 --publish）───────────────────────────────────────

publish_crates() {
  local failed=() rc crate
  for crate in $CRATES; do
    rc=0
    check_version_exists "$crate" "$VERSION" || rc=$?
    if [ "$rc" = "2" ]; then
      log_err "[${crate}] 查询 registry 失败（网络/解析），中止（不当作未发布）"
      return 1
    elif [ "$rc" = "0" ]; then
      log_warn "[${crate}] v${VERSION} 已发布，跳过（支持部分发布后重试）"
      continue
    fi

    log_info "[${crate}] v${VERSION} 未发布，开始 cargo publish..."
    if (cd "$PROJECT_ROOT/$crate" && cargo +"$TOOLCHAIN" publish --registry crates-io 2>&1 | tee -a "$LOG_DIR/publish.log"); then
      log_ok "[${crate}] v${VERSION} 发布成功"
      # 发布后可见性检查；依赖它下游的 crate 在其可解析前不发布
      if ! wait_until_resolvable "$crate" "$VERSION"; then
        log_err "[${crate}] 发布后 registry 不可解析，中止后续发布（避免下游依赖不可解析）"
        return 1
      fi
    else
      log_err "[${crate}] 发布失败"
      failed+=("$crate")
    fi
  done
  if [ ${#failed[@]} -gt 0 ]; then
    log_err "发布失败: ${failed[*]}"
    return 1
  fi
}

# ─── 主流程 ────────────────────────────────────────────────────────

echo "========================================"
log_info "LeLLM Workspace - ${MODE} 模式（v${VERSION}，工具链 +${TOOLCHAIN}）"
echo "========================================"
echo ""

run_verification

if [ "$MODE" = "publish" ]; then
  if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
    log_err "正式发布需要 CARGO_REGISTRY_TOKEN（cargo login 或 export CARGO_REGISTRY_TOKEN=...）"
    exit 1
  fi
  echo ""
  log_info "开始正式发布到 crates.io（顺序：${CRATES}）..."
  publish_crates
  echo "========================================"
  log_ok "发布流程完成"
else
  echo ""
  log_info "默认验证模式结束，未发布。正式发布：./scripts/publish.sh --publish"
fi
