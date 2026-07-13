//! 工具系统 — 协议层 + 可执行工具描述。
//!
//! 本模块定义了工具的基础类型，不依赖任何运行时（tokio/futures）和 schemars。
//!
//! **分层：**
//! - 协议层：`ToolDefinition`, `ParallelSafety`, `ToolCategory`
//! - 可执行描述：`ExecutableTool`（定义 + 执行器，但不负责调度/重试/目录）
//! - 构造框架：`lellm-tool` crate（`ToolArgs`, schema 生成，`#[tool]` 宏）
//!
//! `ExecutableTool` 可通过 `ExecutableTool::safe()` 等低级工厂构造，
//! 或由 `lellm-tool` 的 `#[tool]` 宏自动生成。
//! 真正的运行时（lookup, dispatch, retry, parallel, snapshot）全部留给 lellm-agent。

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::ToolResult;

// ─── ToolSchema ─────────────────────────────────────────────────

/// 工具参数 Schema — 清洗后的 JSON Schema（不含 $schema, title 等元数据噪音）。
///
/// Newtype 封装，预留扩展空间（schema hash、version、validation 等）。
/// schema 一旦创建即不可变，通过 `as_value()` 只读访问。
///
/// **注意：** 目前 `ToolSchema` 是 `serde_json::Value` 的语义包装，
/// 不做运行时 JSON Schema 验证（如类型检查、required 字段校验等）。
/// 验证职责留给 Provider 适配层（Codec），因为不同 Provider
///（OpenAI、Anthropic、MCP）对 JSON Schema 的子集支持不同。
/// `ToolSchema` 的类型名是一种**语义承诺**，而非**运行时保证**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSchema(serde_json::Value);

impl ToolSchema {
    /// 创建新的 ToolSchema。
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// 只读访问内部 JSON Value。
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }

    /// 消费自身，返回内部 JSON Value。
    pub fn into_value(self) -> serde_json::Value {
        self.0
    }
}

impl PartialEq<serde_json::Value> for ToolSchema {
    fn eq(&self, other: &serde_json::Value) -> bool {
        &self.0 == other
    }
}

impl From<serde_json::Value> for ToolSchema {
    fn from(value: serde_json::Value) -> Self {
        Self(value)
    }
}

impl From<ToolSchema> for serde_json::Value {
    fn from(schema: ToolSchema) -> Self {
        schema.0
    }
}

impl serde::Serialize for ToolSchema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ToolSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        serde_json::Value::deserialize(deserializer).map(ToolSchema)
    }
}

// ─── ParallelSafety ─────────────────────────────────────────────

/// 工具并行安全分级
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParallelSafety {
    /// 可并行执行（默认）
    Safe,
    /// 同类别内互斥，类别间可并行
    CategoryExclusive,
    /// 全局互斥
    Exclusive,
}

// ─── ToolCategory ───────────────────────────────────────────────

/// 工具类别 — 用于 `CategoryExclusive` 的分组
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCategory(pub Cow<'static, str>);

impl ToolCategory {
    pub const FILE_IO: Self = Self(Cow::Borrowed("file_io"));
    pub const NETWORK: Self = Self(Cow::Borrowed("network"));
    pub const DATABASE: Self = Self(Cow::Borrowed("database"));

    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }
}

// ─── ToolDefinition ─────────────────────────────────────────────

/// 工具定义（纯数据，协议层）。
///
/// Schema 由 `schemars` 在编译期生成，经清洗后存入 `parameters` 字段。
/// Provider 将此结构序列化后发送给 LLM。
///
/// **与 `ExecutableTool` 的区别：**
/// - `ToolDefinition`（core）：纯数据，Provider 序列化发送给 LLM
/// - `ExecutableTool`（core）：可执行，Agent 调用时查找并执行
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    /// 工具名称
    pub name: String,
    /// 工具描述
    pub description: String,
    /// JSON Schema 参数定义（`ToolSchema` 语义包装，目前不做运行时验证）。
    ///
    /// 通过 `as_value()` 只读访问，或 `into_value()` 消费。
    /// Provider 适配层负责根据目标 API 格式进行二次适配。
    pub parameters: ToolSchema,
    /// 缓存控制标记。Anthropic 支持 Tool Definition 级别的缓存。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<crate::message::CacheControl>,
}

impl ToolDefinition {
    /// 克隆并设置缓存标记。
    pub fn with_cache(self, cache: crate::message::CacheControl) -> Self {
        Self {
            cache_control: Some(cache),
            ..self
        }
    }
}

// ─── ToolFn ─────────────────────────────────────────────────────

/// 异步工具执行函数类型 — 接受 JSON 参数，返回 boxed future。
pub type ToolFn = Arc<
    dyn Fn(&serde_json::Value) -> Pin<Box<dyn Future<Output = ToolResult> + Send>> + Send + Sync,
>;

/// 内部辅助 — 将 concrete future coerc 到 trait object。
///
/// 用于 `#[tool]` 宏生成的代码，解决 `Box::pin(async move { ... })`
/// 无法自动 coerc 到 `Pin<Box<dyn Future>>` 的问题。
#[doc(hidden)]
pub fn __tool_box<F>(f: F) -> Pin<Box<dyn Future<Output = ToolResult> + Send>>
where
    F: Future<Output = ToolResult> + Send + 'static,
{
    Box::pin(f)
}

// ─── ExecutableTool ─────────────────────────────────────────────

/// 可执行的工具 — 定义 + 安全元数据 + 执行器。
///
/// **与 `ToolDefinition` 的区别：**
/// - `ToolDefinition`：纯数据，Provider 序列化发送给 LLM
/// - `ExecutableTool`：可执行，Agent 调用时查找并执行
///
/// **与运行时（lellm-agent）的区别：**
/// - `ExecutableTool`：描述"这个工具能做什么 + 怎么执行"，但不负责调度
/// - `ToolExecutor` / `ToolCatalog` / `ToolSnapshot`：负责 lookup, dispatch, retry, parallel
///
/// 用户通过 `ExecutableTool::safe()` 等工厂方法构造，
/// 或由 `#[tool]` 宏自动生成。
#[derive(Clone)]
pub struct ExecutableTool {
    /// 工具定义（纯元数据，可被 Provider 序列化）
    pub definition: ToolDefinition,
    /// 并行安全级别
    pub safety: ParallelSafety,
    /// 工具类别（仅 `CategoryExclusive` 时使用）
    pub category: Option<ToolCategory>,
    /// 执行函数（运行时，不被序列化）
    executor: ToolFn,
}

impl ExecutableTool {
    // ─── 访问器 ───────────────────────────────────────────────

    /// 获取工具定义的引用。
    pub fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    /// 获取并行安全级别。
    pub fn safety(&self) -> &ParallelSafety {
        &self.safety
    }

    /// 获取工具类别（如果有）。
    pub fn category(&self) -> Option<&ToolCategory> {
        self.category.as_ref()
    }

    /// 执行工具调用，返回未来对象。
    pub fn execute(
        &self,
        args: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send>> {
        (self.executor)(args)
    }

    // ─── 低层构造 — 接受原始 ToolFn（用于 MCP bridge 等场景） ──

    /// 从原始执行函数构造。
    ///
    /// 用于 MCP bridge 等需要直接控制执行函数的场景。
    pub fn from_fn(
        def: ToolDefinition,
        safety: ParallelSafety,
        category: Option<ToolCategory>,
        f: ToolFn,
    ) -> Self {
        Self {
            definition: def,
            safety,
            category,
            executor: f,
        }
    }

    // ─── 高层构造 — 原始 JSON 输入 ────────────────────────────

    /// 并行安全（Safe）工具注册。
    pub fn safe<F, Fut>(def: ToolDefinition, f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            definition: def,
            safety: ParallelSafety::Safe,
            category: None,
            executor: Arc::new(move |args: &serde_json::Value| Box::pin(f(args))),
        }
    }

    /// 分类内互斥（CategoryExclusive）工具注册。
    pub fn category_exclusive<F, Fut>(def: ToolDefinition, category: ToolCategory, f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            definition: def,
            safety: ParallelSafety::CategoryExclusive,
            category: Some(category),
            executor: Arc::new(move |args: &serde_json::Value| Box::pin(f(args))),
        }
    }

    /// 全局互斥（Exclusive）工具注册。
    pub fn exclusive<F, Fut>(def: ToolDefinition, f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            definition: def,
            safety: ParallelSafety::Exclusive,
            category: None,
            executor: Arc::new(move |args: &serde_json::Value| Box::pin(f(args))),
        }
    }
}
