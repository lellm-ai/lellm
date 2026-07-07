#!/bin/bash
# 测试 MCP SSE 模式
#
# 启动 Tencent Map MCP Server (SSE 模式)
# 然后用 curl 测试 SSE 连接
#
# 环境变量:
#   TENCENT_MAP_KEY  - 腾讯地图 API key (必需)
#   MCP_SERVER_PORT  - MCP Server 监听端口 (可选, 默认 3100)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# 加载 .env (如果存在)
if [[ -f "$PROJECT_DIR/.env" ]]; then
    set -a
    source "$PROJECT_DIR/.env"
    set +a
    echo "[INFO] Loaded .env"
fi

# 检查必需的环境变量
if [[ -z "${TENCENT_MAP_KEY:-}" ]]; then
    echo "[ERROR] TENCENT_MAP_KEY not set" >&2
    echo "        请在 .env 文件中设置，或 export TENCENT_MAP_KEY=xxx" >&2
    exit 1
fi

MCP_PORT="${MCP_SERVER_PORT:-3100}"
MCP_SSE_URL="http://localhost:${MCP_PORT}/sse"
MCP_MESSAGES_URL="http://localhost:${MCP_PORT}/messages"

cleanup() {
    echo ""
    echo "[INFO] 清理 MCP Server 进程..."
    if [[ -n "${MCP_PID:-}" ]] && kill -0 "$MCP_PID" 2>/dev/null; then
        kill "$MCP_PID" 2>/dev/null || true
        wait "$MCP_PID" 2>/dev/null || true
    fi
    echo "[INFO] 完成"
}
trap cleanup EXIT

cd "$PROJECT_DIR"

echo "=== MCP SSE 模式测试 ==="
echo ""

# 1. 编译 MCP Server
echo "[1/3] 编译 MCP Server..."
cargo build --example mcp_tencent_map_server --features server -p lellm-mcp 2>&1 | tail -1
echo "      ✓ 编译完成"

# 2. 启动 MCP Server (SSE 模式)
echo "[2/3] 启动 Tencent Map MCP Server (SSE 模式, port ${MCP_PORT})..."
TENCENT_MAP_KEY="$TENCENT_MAP_KEY" \
MCP_SERVER_PORT="$MCP_PORT" \
cargo run --example mcp_tencent_map_server --features server -p lellm-mcp \
    &>/tmp/mcp_sse_server.log &
MCP_PID=$!
echo "      PID: $MCP_PID"

# 3. 等待 MCP Server 就绪
echo "[3/3] 等待 MCP Server 就绪..."
MAX_WAIT=10
WAITED=0
while ! curl -sf "$MCP_SSE_URL" -o /dev/null -w "%{http_code}" 2>/dev/null | grep -q "200"; do
    if [[ $WAITED -ge $MAX_WAIT ]]; then
        echo "[ERROR] MCP Server 启动超时 (${MAX_WAIT}s)" >&2
        echo "[LOG] Server 日志:" >&2
        tail -30 /tmp/mcp_sse_server.log >&2
        exit 1
    fi
    sleep 0.5
    WAITED=$((WAITED + 1))
done
echo "      ✓ MCP Server 已就绪 (耗时 ${WAITED}x500ms)"
echo ""

# 测试 SSE 连接
echo "=== 测试 SSE 连接 ==="
echo "SSE 端点: $MCP_SSE_URL"
echo "按 Ctrl+C 停止测试"
echo ""

# 用 curl 测试 SSE 连接（5秒超时）
timeout 5 curl -N -H "Accept: text/event-stream" "$MCP_SSE_URL" 2>&1 || true

echo ""
echo "=== 测试完成 ==="
