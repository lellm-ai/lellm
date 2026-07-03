#!/bin/bash
# 运行 MCP Weather 示例

# 请在这里设置你的 API Key
# export TENCENT_MAP_KEY="你的API_KEY"

# 检查环境变量
if [ -z "$TENCENT_MAP_KEY" ]; then
    echo "请先设置环境变量 TENCENT_MAP_KEY"
    echo "export TENCENT_MAP_KEY=\"你的API_KEY\""
    exit 1
fi

echo "=== 运行 MCP Weather 示例 ==="
cargo run --example mcp_weather --features sse -p lellm-mcp
