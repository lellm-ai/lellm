//! 工具注册 — Schema、安全分级、执行函数合一。
//!
//! 本模块定义了工具的基础类型，不依赖任何运行时（tokio/futures）。
//! `ToolRegistration` 是 `#[tool]` 宏的产物，可在 graph 层直接使用。

use std::borrow::Cow;
use std::pin::Pin;
use std::sync::Arc;

use crate::{ToolDefinition, ToolResult};

// ─── ToolArgs ───────────────────────────────────────────────────

/// 工具参数 trait — 由 `#[tool]` 宏自动生成。
///
/// 实现了此 trait 的结构体，即可通过 `tool_definition()` 方法
/// 自动获得 `ToolDefinition`（含 JSON Schema）。
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
pub trait ToolArgs {
    /// 工具名称（蛇形命名）
    const NAME: &'static str;
    /// 工具描述
    const DESCRIPTION: &'static str;
    /// 由 `#[tool]` 宏生成的 JSON Schema（LazyLock 缓存）
    fn __schema() -> serde_json::Value;
    /// 自动生成 ToolDefinition（含 JSON Schema）
    fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: Self::DESCRIPTION.to_string(),
            parameters: Self::__schema(),
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

// ─── ToolFn ─────────────────────────────────────────────────────

/// 异步工具函数类型 — 接受 JSON 参数，返回 ToolResult。
///
/// 使用 `UnpinWrapper` 避免 `pin_project` 依赖，保持 core 零运行时依赖。
type ToolFn = Arc<dyn Fn(&serde_json::Value) -> UnpinWrapper<ToolResult> + Send + Sync>;

/// 让 `Box<dyn Future>` 可 `Unpin`，避免引入 pin_project 依赖。
pub struct UnpinWrapper<T>(pub Pin<Box<dyn std::future::Future<Output = T> + Send>>);

impl<T> Unpin for UnpinWrapper<T> {}

impl<T> std::future::Future for UnpinWrapper<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<T> {
        self.get_mut().0.as_mut().poll(cx)
    }
}

// ─── ToolRegistration ───────────────────────────────────────────

/// 工具注册信息 — Schema、安全分级、执行函数合一。
///
/// 用户通过 `ToolRegistration::safe()` 等工厂方法构造，
/// 或由 `#[tool]` 宏自动生成。
#[derive(Clone)]
pub struct ToolRegistration {
    definition: ToolDefinition,
    safety: ParallelSafety,
    category: Option<ToolCategory>,
    func: ToolFn,
}

impl ToolRegistration {
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
    ///
    /// 调用方负责 `poll` / `.await`。
    pub fn execute(&self, args: &serde_json::Value) -> UnpinWrapper<ToolResult> {
        (self.func)(args)
    }

    /// 并行安全（Safe）工具注册。
    pub fn safe<F, Fut>(def: ToolDefinition, f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            definition: def,
            safety: ParallelSafety::Safe,
            category: None,
            func: Arc::new(move |args: &serde_json::Value| UnpinWrapper(Box::pin(f(args)))),
        }
    }

    /// 强类型便捷构造 — 自动反序列化参数。
    ///
    /// 与 `safe()` 的区别：闭包接收反序列化后的 `T`，而非原始 `serde_json::Value`。
    /// 反序列化失败时返回 `ToolErrorKind::InvalidInput`。
    pub fn safe_fn<T, F, Fut>(def: ToolDefinition, f: F) -> Self
    where
        T: for<'de> serde::Deserialize<'de> + Send + 'static,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        let f = Arc::new(f);
        Self::safe(def, move |value| {
            let f = Arc::clone(&f);
            let result = serde_json::from_value::<T>(value.clone());
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

    /// 分类内互斥（CategoryExclusive）工具注册。
    pub fn category_exclusive<F, Fut>(def: ToolDefinition, category: ToolCategory, f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            definition: def,
            safety: ParallelSafety::CategoryExclusive,
            category: Some(category),
            func: Arc::new(move |args: &serde_json::Value| UnpinWrapper(Box::pin(f(args)))),
        }
    }

    /// 全局互斥（Exclusive）工具注册。
    pub fn exclusive<F, Fut>(def: ToolDefinition, f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            definition: def,
            safety: ParallelSafety::Exclusive,
            category: None,
            func: Arc::new(move |args: &serde_json::Value| UnpinWrapper(Box::pin(f(args)))),
        }
    }
}
