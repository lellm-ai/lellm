# LeLLM v0.1 Grill 行动计划

> 日期：2026-06-15 | 来源：Grill-me 讨论
> 状态：待执行 | 负责人：cunge

---

## 一、已达成决策

### P0 — 行为缺陷（必须修复）

#### 1. 流式工具执行改为分组并发

**问题：** `execute_stream()` 强制串行执行所有工具，即使 `ParallelSafety::Safe`。5 个独立工具串行执行 = 5 倍耗时。

**决策：** 复用 `execute_batch_with()` 的 `ParallelSafety` 分组逻辑，流式模式按安全级别分组并发。事件允许乱序（`ToolStart(A)` → `ToolStart(B)` → `ToolEnd(B)` → `ToolEnd(A)`），消费者用 `tool_call_id` 配对。

**影响文件：**
- `lellm-agent/src/runtime/iteration.rs` — `emit_and_execute_tools_with()` 改为分组并发
- `lellm-agent/src/runtime/runtime.rs` — `execute_stream()` 调用点

**验收标准：**
- 非流式和流式对 `Safe` 工具的并发行为一致
- `Exclusive` 工具仍然串行
- `CategoryExclusive` 组内串行、组间并发
- 事件流允许乱序，`tool_call_id` 正确配对

---

#### 2. MCP 提前纳入 v0.1 发布

**问题：** `lellm-mcp` crate 已实现但蓝图标记为 v0.3。代码已就绪，应提前发布。

**决策：** 更新蓝图和文档，将 MCP（Tools only）纳入 v0.1 发布范围。

**影响文件：**
- `docs/BLUEPRINT.md` — 更新 v0.1 范围和实现状态
- `docs/adr/` — 确认 ADR 系列与 v0.1 对齐

**验收标准：**
- BLUEPRINT.md v0.1 范围包含 MCP
- 实现状态表标记 MCP 模块
- 版本号对齐

---

### P1 — 架构完善（v0.1 发布前完成）

#### 3. 统一 Fallback Engine

**问题：** 非流式和流式执行有两套 fallback 逻辑。非流式用 `execute_with_fallback()`，流式内联在 `execute_stream()` 中。未来加新功能需改两处。

**决策：** 抽取统一的 Fallback Engine，将"是否允许重试"策略参数化（`RetryGate` trait 或闭包）。

```rust
// 统一入口
execute_with_fallback(
    fallback: &Arc<dyn FallbackStrategy>,
    can_retry: impl Fn(&LlmError) -> bool,  // 或 RetryGate trait
    op: impl FnMut() -> Fut,
    ctx: FallbackContext,
) -> Result<T, LlmError>

// 非流式：can_retry = || true
// 流式：can_retry = |err| !stream_started
```

**影响文件：**
- `lellm-agent/src/runtime/iteration.rs` — 统一 fallback 函数
- `lellm-agent/src/runtime/runtime.rs` — 两处调用点统一

**验收标准：**
- 非流式和流式使用同一 fallback 入口
- `stream_started` 守卫作为策略注入
- 重试统计统一记录

---

#### 4. CompositeCatalog 冲突解决 API

**问题：** 当前工具遮蔽只发 `tracing::warn!`，用户不知道谁覆盖了谁。

**决策：** 引入 `ConflictPolicy` + Catalog Identity。

```rust
pub enum ConflictPolicy {
    Shadow,  // 默认，前面优先级高
    Error,   // 严格模式，冲突即报错
}

pub struct CatalogConflict {
    pub tool_name: String,
    pub winner: String,      // 获胜 catalog 名称
    pub loser: String,       // 被覆盖 catalog 名称
    pub strategy: ConflictPolicy,
}

// Builder API
CompositeCatalog::builder()
    .conflict_policy(ConflictPolicy::Shadow)  // 默认
    .add("local", local_catalog)
    .add("filesystem", mcp_catalog)
    .build();

// 查询冲突
let catalog = CompositeCatalog::builder()...build();
for c in catalog.conflicts() {
    println!("tool '{}' from '{}' shadowed by '{}'",
        c.tool_name, c.loser, c.winner);
}
```

**影响文件：**
- `lellm-agent/src/runtime/tools/mod.rs` — `CompositeCatalog` 重构
- 新增冲突相关类型

**验收标准：**
- 默认行为不变（Shadow）
- 日志明确显示获胜者和失败者
- `conflicts()` 方法可查询冲突详情
- `Error` 策略直接返回错误

---

#### 5. ToolRegistration::safe_fn<T>() — 强类型便捷构造

**问题：** 手写工具需手动 `serde_json::from_value`，错误质量不统一。与 `#[tool]` 宏体验不一致。

**决策：** 新增 `safe_fn<T: ToolArgs>()` 便捷方法，保留 `safe()` 作为原始 ABI。

```rust
impl ToolRegistration {
    pub fn safe_fn<T, F, Fut>(definition: ToolDefinition, f: F) -> Self
    where
        T: ToolArgs + Send + 'static,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        Self::safe(definition, move |value| {
            let parsed = match serde_json::from_value::<T>(value.clone()) {
                Ok(v) => v,
                Err(e) => {
                    return Box::pin(async move {
                        Err(ToolError::invalid_input(
                            format!("invalid tool arguments: {e}")
                        ))
                    });
                }
            };
            Box::pin(f(parsed))
        })
    }
}
```

**影响文件：**
- `lellm-agent/src/runtime/tools/executor.rs` — `ToolRegistration` 新方法
- `lellm-macros/` — `#[tool]` 宏改用 `safe_fn` 内部实现

**验收标准：**
- `safe()` 行为不变
- `safe_fn<T>()` 自动反序列化 + 统一错误格式
- `#[tool]` 宏内部使用 `safe_fn`

---

#### 6. 更新 DESIGN.md — ToolResult 类型

**问题：** DESIGN.md §7.1 写 `ToolResult = Result<String, ToolError>`，实际代码是 `Result<serde_json::Value, ToolError>`。

**决策：** 更新文档，记录从 String 到 serde_json::Value 的演进。

**影响文件：**
- `docs/DESIGN.md` — §7.1 更新 ToolResult 类型

---

## 二、已确认无需改动

| 项目 | 结论 |
|------|------|
| ToolSnapshot 原子性 | 已满足版本一致性。snapshot 不可变，Catalog 刷新只影响后续 round |
| ToolSnapshot 双快照 | 不存在问题。`ResolvedRound` 保证定义和执行绑定同一快照 |
| Doctest 覆盖 | v0.1 先不管，后续处理 |
| Fallback 双实现 | 确认是代码异味，P1 优先级统一 |

---

## 三、执行顺序建议

```
1. 流式工具并发（P0，行为缺陷）
2. MCP 文档对齐（P0，发布前必须）
3. Fallback Engine 统一（P1，架构完善）
4. CompositeCatalog 冲突 API（P1，架构完善）
5. safe_fn<T>()（P1，API 完善）
6. DESIGN.md 更新（P1，文档对齐）
```

---

## 四、待讨论问题

以下问题在本次 Grill 中未深入，留待后续：

1. **Provider Codec 覆盖** — AnthropicCodec 和 OpenAICompatCodec 的 `encode`/`decode_sse` 完整度
2. **错误类型设计** — `LlmError` 变体是否覆盖所有场景
3. **Context Compaction** — `LocalCompactor` 摘要质量评估
4. **测试覆盖** — 集成测试和边界场景
5. **Google Provider** — `providers/google.rs` 实现状态
