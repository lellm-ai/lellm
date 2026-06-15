# ADR-005: Prompt Cache Control 原语

> **日期**: 2026-06-15
> **状态**: Proposed — 待评审

## 背景

Prompt Caching 是 LLM Provider 的重要成本优化手段：

- **Anthropic**: 显式 `cache_control: {"type": "ephemeral"}` 标记在 ContentBlock 和 Tool Definition 上
- **OpenAI**: 隐式前缀缓存，相同前缀自动生效，无需标记
- **Google**: 无显式缓存控制
- **本地模型**: 完全不支持

devops-agent 已验证七层 Prompt Cache 架构（静态 System → 工具定义 → 半静态规则 → 动态记忆 → Session → 动态工具 → Messages），通过在层边界插入缓存断点实现高缓存命中率。

**核心矛盾**：缓存断点的**放置位置**是业务层决策（哪里稳定、哪里易变），但**信号载体**必须在 Core 类型上表达，否则 Provider Adapter 无法序列化。

## 决策

### 1. CacheControl 枚举

```rust
/// 缓存控制标记 — Provider 无关的语义抽象。
///
/// 由 Provider Codec 映射为各 Provider 的具体格式：
/// - Anthropic: `{"type": "ephemeral"}`
/// - OpenAI: ignore（隐式缓存）
/// - Google: ignore
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CacheControl {
    /// 缓存断点 — 标记此处为缓存边界。
    /// 业务层在稳定性递减的层边界处插入。
    Breakpoint,
}
```

**设计理由**：

- `Breakpoint` 是语义名称，不绑定任何 Provider 术语（避免 `EphemeralBuffer` 这类 Anthropic 特有概念泄露到 Core）
- 未来可扩展：`CacheControl::Provider(serde_json::Value)` 用于 Provider 特有参数
- 当前只有一个变体足够，YAGNI

### 2. TextBlock 扩展

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextBlock {
    pub text: String,

    /// 缓存控制标记。业务层在 System prompt 的稳定性层边界处设置。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}
```

**便利方法**：

```rust
impl ContentBlock {
    /// 创建带缓存标记的文本块。
    pub fn text_with_cache(s: String, cache: CacheControl) -> Self {
        ContentBlock::Text(TextBlock {
            text: s,
            cache_control: Some(cache),
        })
    }
}
```

**影响范围**：

- `lellm-core/src/message.rs` — `TextBlock` 结构体加字段
- `lellm-core/src/message.rs` — `ContentBlock::text()` 初始化 `cache_control: None`
- `lellm-agent/src/runtime/context/estimation.rs` — 无影响（只读 `text` 字段）
- 所有 `TextBlock { text: "..." }` 构造点需要补 `cache_control: None`（或用 `ContentBlock::text()` 工厂方法）

### 3. ToolDefinition 扩展

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,

    /// 缓存控制标记。Anthropic 支持 Tool Definition 级别的缓存。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}
```

**便利方法**：

```rust
impl ToolDefinition {
    /// 克隆并设置缓存标记。
    pub fn with_cache(self, cache: CacheControl) -> Self {
        Self {
            cache_control: Some(cache),
            ..self
        }
    }
}
```

**影响范围**：

- `lellm-core/src/request.rs` — `ToolDefinition` 结构体加字段
- `lellm-macros/src/codegen.rs` — `tool_definition()` 生成代码补 `cache_control: None`
- `lellm-mcp/src/bridge/mod.rs` — 构造 `ToolDefinition` 补字段
- Provider Codec — 检查 `cache_control` 并序列化

### 4. Provider Codec 映射

| Provider | `CacheControl::Breakpoint` 映射 |
|---|---|
| `AnthropicCodec` | ContentBlock: `{"cache_control": {"type": "ephemeral"}}` |
| `AnthropicCodec` | Tool: 在该工具后插入 `{"cache_control": {"type": "ephemeral"}}` |
| `OpenAICompatCodec` | ignore（OpenAI 隐式前缀缓存） |
| `GoogleCodec` | ignore |

**Anthropic 特殊处理**：工具列表中的缓存断点是「在工具后插入一个只含 `cache_control` 的 JSON 对象」，不是在工具本身上加字段。Codec 需要遍历工具列表，在带 `cache_control` 的工具后插入断点。

### 5. 不做的事情

| 项目 | 决定 | 理由 |
|---|---|---|
| `TextMetadata` | ❌ 不引入 | 无活跃消费者，ADR 记录 Future extensions |
| `ToolDefinition.description` → `Option<String>` | ❌ 保持 `String` | Tool 必须可描述，Option 收益太小 |
| Provider 特定缓存 JSON 进入 Core | ❌ | Core 只存语义枚举，Codec 映射 |
| 完整 PromptBuilder | ❌ 不在 Core | 业务层职责，Core 只提供原语 |

## Future Extensions

记录未来可能的扩展方向（当前无活跃消费者，不实现）：

- **TextMetadata**: `priority`, `source_id`, `provenance` — 等 RAG Source Tracking、Prompt Ranking 需求出现时设计
- **CacheControl::Provider(serde_json::Value)**: Provider 特有缓存参数透传
- **Cache statistics**: 缓存命中率可观测性
- **Automatic breakpoint placement**: 框架层根据内容稳定性自动插入断点

## 参考

- [devops-agent prompt_builder.rs](/Users/pengh/www/enjoy/devops-agent/backend/src/llm/prompt_builder.rs) — 七层组装验证
- [Anthropic Prompt Caching](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching)
- [OpenAI Prompt Caching](https://platform.openai.com/docs/guides/prompt-caching)
