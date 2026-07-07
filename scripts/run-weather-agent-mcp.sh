#!/bin/bash
# weather_agent MCP 版本启动脚本
#
# 启动流程：
#   1. 启动 Tencent Map MCP Server（HTTP，端口 3100）
#   2. 等待 Server 就绪
#   3. 运行 Weather Agent（通过 MCP 调用 resolve_city）
#   4. 清理 MCP Server 进程
#
# 环境变量:
#   LLAMA_API_KEY    - LLaMA provider API key (必需)
#   LLAMA_BASE_URL   - LLaMA provider base URL (可选)
#   TENCENT_MAP_KEY  - 腾讯地图 API key (必需)
#   MCP_SERVER_PORT  - MCP Server 监听端口 (可选, 默认 3100)
#
# 用法:
#   ./scripts/run-weather-agent-mcp.sh [地址]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# 加载 .env (如果存在)
if [[ -f "$PROJECT_DIR/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "$PROJECT_DIR/.env"
    set +a
    echo "[INFO] Loaded .env"
fi

# 检查必需的环境变量
if [[ -z "${LLAMA_API_KEY:-}" ]]; then
    echo "[ERROR] LLAMA_API_KEY not set" >&2
    echo "        请在 .env 文件中设置，或 export LLAMA_API_KEY=xxx" >&2
    exit 1
fi

if [[ -z "${TENCENT_MAP_KEY:-}" ]]; then
    echo "[ERROR] TENCENT_MAP_KEY not set" >&2
    echo "        请在 .env 文件中设置，或 export TENCENT_MAP_KEY=xxx" >&2
    exit 1
fi

MCP_PORT="${MCP_SERVER_PORT:-3100}"
MCP_URL="http://localhost:${MCP_PORT}/mcp"

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

echo "=== Weather Agent — MCP 版本 ==="
echo ""

# 1. 启动 MCP Server
echo "[1/3] 启动 Tencent Map MCP Server (port ${MCP_PORT})..."
TENCENT_MAP_KEY="$TENCENT_MAP_KEY" \
MCP_SERVER_PORT="$MCP_PORT" \
cargo run --example mcp_tencent_map_server --features server -p lellm-mcp \
    &>/tmp/mcp_server.log &
MCP_PID=$!
echo "      PID: $MCP_PID"

# 2. 等待 MCP Server 就绪
echo "[2/3] 等待 MCP Server 就绪..."
MAX_WAIT=15
WAITED=0
while ! curl -sf "$MCP_URL" &>/dev/null; do
    if [[ $WAITED -ge $MAX_WAIT ]]; then
        echo "[ERROR] MCP Server 启动超时 (${MAX_WAIT}s)" >&2
        echo "[LOG] Server 日志:" >&2
        cat /tmp/mcp_server.log >&2
        exit 1
    fi
    sleep 0.5
    WAITED=$((WAITED + 1))
done
echo "      ✓ MCP Server 已就绪 (耗时 ${WAITED}x500ms)"

# 3. 运行 Weather Agent
echo "[3/3] 运行 Weather Agent..."
echo ""

if [[ $# -gt 0 ]]; then
    MCP_SERVER_URL="$MCP_URL" \
    cargo run -p lellm-agent --example weather_agent_mcp -- "$@"
else
    MCP_SERVER_URL="$MCP_URL" \
    cargo run -p lellm-agent --example weather_agent_mcp
fi

# cleanup 由 trap 处理
