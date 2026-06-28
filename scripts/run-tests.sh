#!/bin/bash
# Run full test suite for the workspace
set -euo pipefail

cd "$(dirname "$0")/.."

echo "🧪 Running full test suite..."
cargo test --workspace 2>&1 | tee logs/test.log
echo ""
echo "✅ Tests completed. See logs/test.log for details."
