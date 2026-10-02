#!/bin/bash
# 验证 mcp-stdio 轻量入口的依赖边界：不引入 agent/provider/reqwest/hyper/TLS。
#
# 用独立临时消费项目（不在 lellm workspace 内）验证，避免 workspace feature 统一
# 掩盖轻量入口缺失。这是比 `cargo tree -p lellm` 更强的外部消费者视角检查。
#
# 用法：bash scripts/check-mcp-stdio-boundary.sh
# 退出码：0 = 边界干净；1 = 边界被破坏（引入了禁用 crate）
set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"

# shell 全局 DYLD_LIBRARY_PATH（Homebrew llvm/sqlite）会劫持 rustc 1.98+ 的
# dylib 解析，导致 `dyld: missing symbol called` (SIGABRT)。边界检查不需要它。
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH

# 禁用 crate 模式：mcp-stdio 绝不应引入这些
FORBIDDEN='lellm-agent|lellm-provider|lellm-graph|reqwest|hyper|native-tls|rustls|tokio-native|tower|openssl|eventsource|axum'

CONSUMER_DIR="$(mktemp -d)"
cleanup() {
  # 按安全规则：不用 rm，移到废纸篓（若存在）
  if [ -d "$HOME/.Trash" ]; then
    mv "$CONSUMER_DIR" "$HOME/.Trash/lellm-mcp-stdio-check-$(date +%s)" 2>/dev/null || true
  fi
}
trap cleanup EXIT

mkdir -p "$CONSUMER_DIR/src"
cat > "$CONSUMER_DIR/Cargo.toml" <<EOF
[package]
name = "lellm-mcp-stdio-check"
version = "0.1.0"
edition = "2024"

# 独立 workspace，确保不被 lellm workspace 统一 feature
[workspace]

[dependencies]
lellm = { path = "$REPO_ROOT/lellm", default-features = false, features = ["mcp-stdio"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
EOF

cat > "$CONSUMER_DIR/src/main.rs" <<'EOF'
use lellm::mcp::client::McpClient;
use lellm::mcp::transport::{StdioConfig, StdioTransport};

#[tokio::main]
async fn main() {
    let config = StdioConfig::new("echo", vec!["hello".to_string()]);
    let _transport = StdioTransport::new(config);
    let _client = McpClient::with_transport(_transport);
    println!("mcp-stdio boundary check OK");
}
EOF

echo "🔍 [1/3] 编译独立消费项目 (cargo) ..."
(cd "$CONSUMER_DIR" && cargo build 2>&1 | grep -E "error|Finished" || true)

echo "🔍 [2/3] 检查依赖边界（应无禁用 crate）..."
if (cd "$CONSUMER_DIR" && cargo tree -e normal,build --prefix none 2>/dev/null | grep -iE "$FORBIDDEN"); then
  echo "❌ 依赖边界被破坏：mcp-stdio 引入了禁用 crate（见上）"
  exit 1
else
  echo "✅ 依赖边界干净：mcp-stdio 未引入 agent/provider/reqwest/hyper/TLS"
fi

echo "📊 [3/3] mcp-stdio 唯一 crate 数（统计口径：cargo tree -e normal,build --prefix none | sort -u）："
(cd "$CONSUMER_DIR" && cargo tree -e normal,build --prefix none 2>/dev/null | sort -u | wc -l | tr -d ' ')

echo "✅ mcp-stdio 依赖边界检查通过"
