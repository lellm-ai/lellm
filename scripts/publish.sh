#!/bin/bash
# LeLLM Workspace 发布脚本
# 用法: ./scripts/publish.sh [1]
#   (无参数) - 模拟发布（默认），检查是否能通过 crates.io 校验
#   1        - 正式发布到 crates.io

set -euo pipefail

CRATES="lellm-core lellm-macros lellm-provider lellm-agent lellm-mcp lellm-graph lellm"
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# 读取 workspace version
WORKSPACE_VERSION=$(grep '^version' "$PROJECT_ROOT/Cargo.toml" | head -1 | sed 's/.*= *"\([^"]*\)".*/\1/')

# 颜色
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log_info()  { echo -e "${CYAN}[INFO]${NC}  $*"; }
log_ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
log_err()   { echo -e "${RED}[ERROR]${NC} $*"; }

if [ "${1:-}" = "1" ]; then
  LOG_MODE="正式发布"
  PUBLISH_FLAG=""
else
  LOG_MODE="模拟发布"
  PUBLISH_FLAG="--dry-run"
fi

echo "========================================"
log_info "LeLLM Workspace - $LOG_MODE"
echo "========================================"
echo ""

# 1. 检查 cargo login
if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
    log_info "未设置 CARGO_REGISTRY_TOKEN，检查本地登录..."
    # cargo verify-project 不需要 token，但 publish 需要
    if [ "$PUBLISH_FLAG" = "" ]; then
        log_warn "正式发布需要 token，请运行: cargo login <TOKEN>"
        log_warn "或: export CARGO_REGISTRY_TOKEN=<TOKEN>"
    fi
fi

# 2. 全量构建 + 测试
log_info "全量构建与检查..."
cd "$PROJECT_ROOT"
cargo build --workspace 2>&1 | tail -5
cargo check --workspace --all-features 2>&1 | tail -5

if [ $? -ne 0 ]; then
    log_err "构建失败，中止发布"
    exit 1
fi
log_ok "构建通过"
echo ""

# 3. 按依赖顺序逐个发布
log_info "按依赖顺序依次 $LOG_MODE ..."
log_info "发布顺序: $CRATES"
echo ""

# 清除 rsproxy 索引缓存（避免发布时使用旧版本依赖）
if [ "$PUBLISH_FLAG" = "" ]; then
    log_info "清除 rsproxy 索引缓存..."
    find ~/.cargo/registry/cache -name "rsproxy.cn-*" -type d -exec rm -rf {} + 2>/dev/null || true
    find ~/.cargo/registry/index -name "rsproxy.cn-*" -type d -exec rm -rf {} + 2>/dev/null || true
    rm -rf /Users/pengh/data/app/target/package/ 2>/dev/null || true
    log_ok "缓存已清除"
    echo ""
fi

FAILED=()
for crate in $CRATES; do
    crate_dir="$PROJECT_ROOT/$crate"
    if [ ! -d "$crate_dir" ]; then
        log_err "目录不存在: $crate_dir"
        FAILED+=("$crate")
        continue
    fi

    version="$WORKSPACE_VERSION"
    log_info "[$crate] v$version ..."

    cd "$crate_dir"

    # 检查是否已经发布过该版本
    if [ "$PUBLISH_FLAG" = "" ]; then
        EXISTING=$(cargo search "lellm" 2>/dev/null | grep "^\"$crate\"" || true)
        if [ -n "$EXISTING" ]; then
            PUBLISHED_VER=$(echo "$EXISTING" | sed "s/.*= \"\([0-9.]*\)\".*/\1/")
            if [ "$PUBLISHED_VER" = "$version" ]; then
                log_warn "[$crate] v$version 已发布，跳过"
                continue
            fi
        fi
    fi

    # 执行 publish（使用官方 crates.io，跳过验证避免下载旧版本依赖）
    log_info "[$crate] cargo publish $PUBLISH_FLAG ..."
    if cargo publish $PUBLISH_FLAG --registry crates-io --no-verify 2>&1; then
        if [ "$PUBLISH_FLAG" = "--dry-run" ]; then
            log_ok "[$crate] v$version 模拟发布通过"
        else
            log_ok "[$crate] v$version 发布成功"
            # 发布后等待索引更新
            log_info "等待索引更新..."
            sleep 5
        fi
    else
        log_err "[$crate] v$version 发布失败"
        FAILED+=("$crate")
    fi
    echo ""
done

# 4. 总结
echo "========================================"
if [ ${#FAILED[@]} -eq 0 ]; then
    log_ok "$LOG_MODE 完成！所有 crate 通过检查"
else
    log_err "以下 crate 发布失败: ${FAILED[*]}"
    echo "========================================"
    exit 1
fi
echo "========================================"
