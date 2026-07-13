---
description: "发布新版本到 crates.io。自动完成版本号升级、README 更新、格式化、提交、发布全流程。"
---

# 发布 LeLLM 新版本

执行以下步骤将 $1 版本发布到 crates.io。

## 流程

1. **确认当前版本**: 读取 `Cargo.toml` 的 workspace version，确认目标版本号
2. **更新版本号**: 修改 `Cargo.toml` 中的 `version = "..."` 为新版本
3. **更新 README 徽章**:
   - `README.md`: 将 `version-X.Y.Z-green` 替换为新版本
   - `README_zh.md`: 将 `version-X.Y.Z-green` 替换为新版本
4. **格式化代码**: 运行 `cargo fmt`
5. **提交**: `git add Cargo.toml README.md README_zh.md && git commit -m "bump: v$1"`
6. **发布**: 运行 `./scripts/publish.sh 1`（正式发布）
7. **推送**: `git push`

## 注意事项

- 如果某个 crate 发布超时，用 `cargo publish --registry crates-io --no-verify --allow-dirty` 单独重试
- 发布顺序由 `scripts/publish.sh` 控制：lellm-core → lellm-derive → lellm-provider → lellm-graph → lellm-mcp → lellm-agent → lellm
- 仅在用户明确要求时才执行 `git push`，否则提交后停止
