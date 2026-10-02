#!/bin/bash
# 运行 MCP Geocoder 示例

# shell 全局 DYLD_LIBRARY_PATH（Homebrew llvm/sqlite）会劫持 rustc 1.98+ 的 dylib 解析 → SIGABRT
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH

# 请在这里设置你的 API Key
# export TENCENT_MAP_KEY="你的API_KEY"

# 检查环境变量
if [ -z "$TENCENT_MAP_KEY" ]; then
    echo "请先设置环境变量 TENCENT_MAP_KEY"
    echo "export TENCENT_MAP_KEY=\"你的API_KEY\""
    exit 1
fi

echo "=== 运行 MCP Geocoder 示例 (SSE) ==="
cargo run --example mcp_weather_sse --features sse -p lellm-mcp
