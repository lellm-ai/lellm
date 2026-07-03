#!/bin/bash
# 同时运行 MCP Server 和 Client 进行测试

set -e

echo "=== MCP Server + Client Test ==="
echo ""

# 编译 server 和 client
echo "1. 编译..."
cargo build --example mcp_server --features server -p lellm-mcp 2>&1 | tail -1
cargo build --example mcp_client --features bridge,sse,http,server -p lellm-mcp 2>&1 | tail -1

echo ""
echo "2. 启动 Server (stdio 模式)..."

# 使用管道连接 server 和 client
# Server 从 stdin 读取，输出到 stdout
# Client 向 server 发送请求
(
    echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-client","version":"1.0"}}}'
    echo '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
    echo '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"add","arguments":{"a":3,"b":5}}}'
    echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"multiply","arguments":{"a":4,"b":6}}}'
    sleep 1
) | cargo run --example mcp_server --features server -p lellm-mcp 2>&1

echo ""
echo "=== Done ==="
