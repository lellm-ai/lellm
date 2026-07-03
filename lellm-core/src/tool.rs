//! 工具系统 — 协议层 + 可执行工具描述。
//!
//! 本模块定义了工具的基础类型，不依赖任何运行时（tokio/futures）。
//!
//! **分层：**
//! - 协议层：`ToolArgs`, `ToolDefinition`, `ParallelSafety`, `ToolCategory`
//! - 可执行描述：`ExecutableTool`（定义 + 执行器，但不负责调度/重试/目录）
//!
//! `ExecutableTool` 是 `#[tool]` 宏的产物，可在 graph 层直接使用。
//! 真正的运行时（lookup, dispatch, retry, parallel, snapshot）全部留给 lellm-agent。

use std::borrow::Cow;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;

use crate::ToolResult;

// ─── ToolSchema ─────────────────────────────────────────────────

/// 工具参数 Schema — 清洗后的 JSON Schema（不含 $schema, title 等元数据噪音）。
///
/// Newtype 封装，预留扩展空间（schema hash、version、validation 等）。
/// 通过 `Deref` 透明访问内部 `serde_json::Value`。
#[repr(transparent)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSchema(pub serde_json::Value);

impl ToolSchema {
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }
}

impl Deref for ToolSchema {
    type Target = serde_json::Value;

    fn deref(&self) -> &Self::Target {
        &self.0
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

// ─── ToolArgs ───────────────────────────────────────────────────

/// 工具参数 trait — 由 `#[tool]` 宏自动生成。
///
/// 实现了此 trait 的结构体，即可通过 `tool_definition()` 方法
/// 自动获得 `ToolDefinition`（含 JSON Schema）。
///
/// # 约束
/// - `DeserializeOwned` — 可从任意 JSON Value 反序列化
/// - `JsonSchema` — 可生成参数 Schema
/// - `Send + Sync + 'static` — 可在异步运行时中安全传递
///
/// # 示例
/// ```ignore
/// use lellm_derive::tool;
///
/// #[tool(name = "search", description = "搜索互联网信息")]
/// async fn search(query: String, limit: u32) -> String {
///     format!("results for {}", query)
/// }
/// // 生成 SearchArgs struct + search_tool() 工厂函数
/// ```
pub trait ToolArgs:
    serde::de::DeserializeOwned + schemars::JsonSchema + Send + Sync + 'static
{
    /// 工具名称（蛇形命名）
    const NAME: &'static str;
    /// 工具描述
    const DESCRIPTION: &'static str;
    /// JSON Schema（LazyLock 缓存）
    fn schema() -> ToolSchema;

    /// 从原始 JSON Value 反序列化工具参数。
    ///
    /// 默认实现调用 `serde_json::from_value()`，通常无需重写。
    fn parse(value: serde_json::Value) -> Result<Self, serde_json::Error>
    where
        Self: Sized,
    {
        serde_json::from_value(value)
    }

    /// 自动生成 ToolDefinition（含 JSON Schema）。
    ///
    /// 默认实现无缓存。`#[tool]` 宏会生成带 `LazyLock` 缓存的覆盖版本。
    fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: Self::DESCRIPTION.to_string(),
            parameters: Self::schema().0,
            cache_control: None,
        }
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
    /// JSON Schema 参数定义
    pub parameters: serde_json::Value,
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

    /// 从 `schemars::JsonSchema` 类型计算并清洗 JSON Schema。
    ///
    /// 供 `#[tool]` 宏生成的 `LazyLock` 调用，不在泛型函数中使用 `LazyLock`。
    ///
    /// **清洗规则：** 去除 `$schema`, `$id`, `title`, `description` 等根部元数据，
    /// 保留 `type`, `properties`, `required`, `definitions` 等核心 JSON Schema 字段。
    pub fn compute_and_clean_schema<S: schemars::JsonSchema>() -> ToolSchema {
        let root = schemars::schema_for!(S);
        let val = serde_json::to_value(&root)
            .expect("Failed to serialize JsonSchema; this is a bug in schemars");
        ToolSchema(Self::clean_schema(val))
    }

    /// 清洗 schemars 生成的 RootSchema，去除根部元数据噪音。
    ///
    /// 保留 `type`, `properties`, `required`, `definitions`, `additionalProperties`
    /// 等核心 JSON Schema 字段。Codec 层在此基础上进行 Provider 特定的二次适配。
    fn clean_schema(mut value: serde_json::Value) -> serde_json::Value {
        if let Some(obj) = value.as_object_mut() {
            // 去除标准 JSON Schema 根部的噪声元数据
            obj.remove("$schema");
            obj.remove("$id");
            obj.remove("title");
            obj.remove("description");
        }
        value
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

    // ─── 高层构造 — 强类型输入（自动反序列化） ─────────────────

    /// 强类型便捷构造 — 自动反序列化参数（Safe）。
    ///
    /// 闭包接收反序列化后的 `T`，而非原始 `serde_json::Value`。
    /// 反序列化失败时返回 `ToolErrorKind::InvalidInput`。
    pub fn safe_fn<T, F, Fut>(def: ToolDefinition, f: F) -> Self
    where
        T: ToolArgs + Send + 'static,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        let f = Arc::new(f);
        Self::safe(def, move |value| {
            let f = Arc::clone(&f);
            let result = T::parse(value.clone());
            async move {
                match result {
                    Ok(parsed) => f(parsed).await,
                    Err(e) => Err(crate::ToolError::invalid_input(format!(
                        "invalid tool arguments: {e}"
                    ))),
                }
            }
        })
    }

    /// 强类型便捷构造 — 自动反序列化参数（CategoryExclusive）。
    pub fn category_exclusive_fn<T, F, Fut>(
        def: ToolDefinition,
        category: ToolCategory,
        f: F,
    ) -> Self
    where
        T: ToolArgs + Send + 'static,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        let f = Arc::new(f);
        Self::category_exclusive(def, category, move |value| {
            let f = Arc::clone(&f);
            let result = T::parse(value.clone());
            async move {
                match result {
                    Ok(parsed) => f(parsed).await,
                    Err(e) => Err(crate::ToolError::invalid_input(format!(
                        "invalid tool arguments: {e}"
                    ))),
                }
            }
        })
    }

    /// 强类型便捷构造 — 自动反序列化参数（Exclusive）。
    pub fn exclusive_fn<T, F, Fut>(def: ToolDefinition, f: F) -> Self
    where
        T: ToolArgs + Send + 'static,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        let f = Arc::new(f);
        Self::exclusive(def, move |value| {
            let f = Arc::clone(&f);
            let result = T::parse(value.clone());
            async move {
                match result {
                    Ok(parsed) => f(parsed).await,
                    Err(e) => Err(crate::ToolError::invalid_input(format!(
                        "invalid tool arguments: {e}"
                    ))),
                }
            }
        })
    }
}
