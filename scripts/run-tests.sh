#!/bin/bash
# Run full test suite for the workspace
set -euo pipefail

cd "$(dirname "$0")/.."

# shell 全局 DYLD_LIBRARY_PATH（Homebrew llvm/sqlite）会劫持 rustc 1.98+ 的
# dylib 解析，导致 `dyld: missing symbol called` (SIGABRT)。测试运行不需要它。
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH

echo "🧪 Running full test suite..."
cargo test --workspace 2>&1 | tee logs/test.log
echo ""
echo "✅ Tests completed. See logs/test.log for details."
