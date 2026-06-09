#!/bin/bash
# weather_agent 启动脚本
#
# 用法:
#   ./scripts/run-weather-agent.sh [地址]
#
# 环境变量:
#   LLAMA_API_KEY    - LLaMA provider API key (必需)
#   LLAMA_BASE_URL   - LLaMA provider base URL (可选, 默认 http://localhost:8080/v1)
#   TENCENT_MAP_KEY  - 腾讯地图 API key (resolve_via_nominatim 必需)
#
# 示例:
#   ./scripts/run-weather-agent.sh "陆家嘴/新宿/奇台"
#   ./scripts/run-weather-agent.sh  # 默认查询

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
    echo "[WARN] TENCENT_MAP_KEY not set - resolve_via_nominatim 将降级" >&2
fi

# 运行 weather_agent
cd "$PROJECT_DIR"

if [[ $# -gt 0 ]]; then
    echo "[INFO] Running weather_agent with address: $*"
    cargo run --example weather_agent -- "$@"
else
    echo "[INFO] Running weather_agent with default addresses"
    cargo run --example weather_agent
fi
