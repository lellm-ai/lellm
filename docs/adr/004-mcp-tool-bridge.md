# ADR-004: MCP Tool 桥接设计

> **日期**: 2026-06-12
> **状态**: Accepted
> **依赖**: [ADR-001](./001-mcp-as-tool-runtime-extension.md), [ADR-003](./003-mcp-transport-abstraction.md)

## 背景

LeLLM Agent 通过 `ToolExecutor` 执行工具，`ToolRegistration` 绑定定义 + 函数。
MCP 远程工具需要桥接到这个体系。

**关键变化**：现有 `ToolExecutor` 持有 `Arc<HashMap<String, ToolRegistration>>`（静态注册）。
MCP 工具是动态的（Server 可能增删工具），所以需要引入 `ToolCatalog` 抽象。

## 决策

### ToolCatalog 抽象

```rust
pub trait ToolCatalog: Send + Sync {
    /// 获取当前所有工具定义（用于注入 ChatRequest）
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// 获取当前所有注册（用于内部执行）
    fn snapshot(&self) -> Arc<HashMap<String, ToolRegistration>>;
}

/// 静态目录（现有行为，零开销）
pub struct StaticCatalog {
    tools: Arc<HashMap<String, ToolRegistration>>,
}

/// MCP 动态目录（RwLock 保护，微秒级开销）
pub struct McpCatalog {
    inner: RwLock<Arc<HashMap<String, ToolRegistration>>>,
    client: Arc<McpClient>,
}

/// 组合目录（静态 + 动态）
pub struct CompositeCatalog {
    static_tools: Arc<HashMap<String, ToolRegistration>>,
    dynamic_sources: Vec<Box<dyn ToolCatalog>>,
}
```

`ToolExecutor` 改为持有 `Box<dyn ToolCatalog>`：
- `definitions()` → `catalog.definitions()`（每次读最新）
- `execute()` → `catalog.snapshot().get(&call.name)`（每次读最新）

**工具消失时的行为**：正在执行的调用不受影响（`ToolFn` 持有 `Arc<McpClient>`），
后续查找返回 `ToolErrorKind::NotFound`。

### McpTool 结构

```rust
pub struct McpTool {
    server: Arc<McpClient>,       // 共享客户端连接
    definition: ToolDefinition,   // MCP 工具定义 → LeLLM 格式
    tool_name: String,            // MCP 工具名称
}
```

### 桥接为 ToolRegistration

```rust
impl McpTool {
    pub fn into_registration(
        self,
        safety: Option<ParallelSafety>,
        trust: TrustLevel,
    ) -> ToolRegistration {
        let safety = safety.unwrap_or(ParallelSafety::Safe);

        ToolRegistration {
            definition: self.definition,
            safety,
            category: Some("mcp".into()),
            func: Arc::new(move |args: &serde_json::Value| {
                let server = self.server.clone();
                let tool_name = self.tool_name.clone();
                async move {
                    server.call_tool(&tool_name, args).await
                }
            }),
            trust,  // stdio 默认 Confirm
        }
    }
}
```

### 映射规则

| MCP | LeLLM |
|-----|-------|
| `tool.name` | `ToolDefinition.name` |
| `tool.description` | `ToolDefinition.description` |
| `tool.inputSchema` | `ToolDefinition.parameters` |

### 默认安全级别

**`ParallelSafety::Safe`**——假设远程工具独立执行。

**理由**：
- 大多数 MCP Server（filesystem read, github search, browser）是无状态的或自身管理并发
- 保守的 `Exclusive` 会不必要地限制并行性能

**覆盖机制**：
```rust
let tools = client
    .discover()
    .await?
    .with_policy(|tool_name| {
        match tool_name {
            "filesystem/write" => ParallelSafety::CategoryExclusive("filesystem".into()),
            "dangerous_tool"   => ParallelSafety::Exclusive,
            _                  => ParallelSafety::Safe,
        }
    })
    .into_registrations();
```

### ToolExecutor 最小改动

不改执行逻辑，只改数据源：`Arc<HashMap>` → `Box<dyn ToolCatalog>`。

`definitions()` 和 `execute()` 改为通过 catalog 读取，其余不变。

**刷新机制（双模）**：
```rust
// Push: Server 发 notifications/tools/list_changed → 自动刷新
// Pull: 用户显式调用
let new_tools = client.discover().await?;
catalog.update(new_tools);  // RwLock write，不影响正在执行的调用
```

### 错误映射

```rust
// McpClient.call_tool 返回 ToolResult
impl McpClient {
    async fn call_tool(&self, name: &str, args: &Value) -> ToolResult {
        match self.transport.request(...).await {
            Ok(resp) => {
                if resp.content[0].is_error {
                    Err(ToolError {
                        kind: ToolErrorKind::Internal,  // Server 报告错误
                        message: resp.content[0].text,
                    })
                } else {
                    Ok(resp.content[0].text)
                }
            }
            Err(McpError::Timeout) => Err(ToolError { kind: ToolErrorKind::Timeout, .. }),
            Err(McpError::Disconnected) => Err(ToolError { kind: ToolErrorKind::Network, .. }),
            Err(McpError::Protocol) => Err(ToolError { kind: ToolErrorKind::InvalidInput, .. }),
        }
    }
}
```

## 后果

### 正面
- 远程工具和本地工具统一执行路径
- 策略覆盖灵活
- `ToolCatalog` 抽象让动态工具成为可能，不破坏静态场景

### 负面
- 每次工具调用都有网络开销（序列化 + transport + 反序列化）
- 远程工具错误映射可能丢失原始上下文
- `ToolCatalog` 引入 `RwLock` 开销（微秒级，可忽略）和 trait 间接层
- `ToolExecutor` 需要小改（`Arc<HashMap>` → `Box<dyn ToolCatalog>`）

### 风险
- MCP Server 返回的内容格式不一致（有些返回结构化 JSON，有些返回纯文本）
- 缓解：桥接层统一转为 `ToolResult = Result<String, ToolError>`
- `ToolCatalog` trait 边界可能随需求变化（如需要优先级、过滤等）
- 缓解：先实现核心接口，后续扩展通过 trait method 默认实现

### Human Approval

审批不侵入 `ToolExecutor`。在 Agent loop 中加 hook：
```
Agent Loop:
  1. 检查 trust.requires_approval()
  2. 如需确认 → 弹出 prompt → 用户 y/N
  3. 拒绝 → ToolError::PermissionDenied
  4. 通过 → 继续执行
```
