//! lellm-tool — 工具构造框架。
//!
//! 提供 `#[tool]` 宏、`ToolArgs` trait、schema 生成、
//! 以及高级工厂函数（`safe_fn`、`exclusive_fn`、`category_exclusive_fn`）。
//!
//! **与 lellm-core 的区别：**
//! - `lellm-core`：运行时协议契约（ToolDefinition, ExecutableTool）
//! - `lellm-tool`：构造框架（schema 生成、类型安全工厂）

// ─── Re-exports ─────────────────────────────────────────────────────

/// Re-export schemars for user derive macros.
pub use schemars;

/// Re-export serde for user derive macros.
pub use serde;

#[cfg(feature = "derive")]
/// Re-export #[tool] attribute macro.
pub use lellm_derive::tool;

#[cfg(feature = "derive")]
/// Re-export #[derive(Tool)] derive macro.
pub use lellm_derive::Tool;

// Re-export core types used by tool construction.
pub use lellm_core::{
    ExecutableTool, ParallelSafety, ToolCategory, ToolDefinition, ToolSchema,
};

// ─── ToolArgs ────────────────────────────────────────────────────────

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
/// use lellm_tool::tool;
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
            parameters: Self::schema(),
            cache_control: None,
        }
    }
}

// ─── Schema Generation ───────────────────────────────────────────────

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
    ToolSchema::new(clean_schema(val))
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

// ─── Factory Functions ───────────────────────────────────────────────

/// 强类型便捷构造 — 自动反序列化参数（Safe）。
///
/// 闭包接收反序列化后的 `T`，而非原始 `serde_json::Value`。
/// 反序列化失败时返回 `ToolErrorKind::InvalidInput`。
pub fn safe_fn<T, F, Fut>(def: ToolDefinition, f: F) -> ExecutableTool
where
    T: ToolArgs + Send + 'static,
    F: Fn(T) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = lellm_core::ToolResult> + Send + 'static,
{
    let f = std::sync::Arc::new(f);
    ExecutableTool::safe(def, move |value| {
        let f = std::sync::Arc::clone(&f);
        let result = T::parse(value.clone());
        Box::pin(async move {
            match result {
                Ok(parsed) => f(parsed).await,
                Err(e) => Err(lellm_core::ToolError::invalid_input(format!(
                    "invalid tool arguments: {e}"
                ))),
            }
        })
    })
}

/// 强类型便捷构造 — 自动反序列化参数（CategoryExclusive）。
pub fn category_exclusive_fn<T, F, Fut>(
    def: ToolDefinition,
    category: ToolCategory,
    f: F,
) -> ExecutableTool
where
    T: ToolArgs + Send + 'static,
    F: Fn(T) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = lellm_core::ToolResult> + Send + 'static,
{
    let f = std::sync::Arc::new(f);
    ExecutableTool::category_exclusive(
        def,
        category,
        move |value| {
            let f = std::sync::Arc::clone(&f);
            let result = T::parse(value.clone());
            Box::pin(async move {
                match result {
                    Ok(parsed) => f(parsed).await,
                    Err(e) => Err(lellm_core::ToolError::invalid_input(format!(
                        "invalid tool arguments: {e}"
                    ))),
                }
            })
        },
    )
}

/// 强类型便捷构造 — 自动反序列化参数（Exclusive）。
pub fn exclusive_fn<T, F, Fut>(def: ToolDefinition, f: F) -> ExecutableTool
where
    T: ToolArgs + Send + 'static,
    F: Fn(T) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = lellm_core::ToolResult> + Send + 'static,
{
    let f = std::sync::Arc::new(f);
    ExecutableTool::exclusive(def, move |value| {
        let f = std::sync::Arc::clone(&f);
        let result = T::parse(value.clone());
        Box::pin(async move {
            match result {
                Ok(parsed) => f(parsed).await,
                Err(e) => Err(lellm_core::ToolError::invalid_input(format!(
                    "invalid tool arguments: {e}"
                ))),
            }
        })
    })
}
