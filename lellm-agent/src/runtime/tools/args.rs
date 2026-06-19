//! 工具参数 trait — 由 derive(ToolDefinition) 自动生成。
//!
//! 实现了此 trait 的结构体，即可通过 `tool_definition()` 方法
//! 自动获得 JSON Schema 工具定义。Schema 由宏在编译时生成，
//! 采用 schemars 兼容的 JSON Schema 格式。
//!
//! # 示例
//! ```ignore
//! use lellm_derive::ToolDefinition;
//!
//! #[derive(ToolDefinition)]
//! #[tool(name = "search", description = "搜索互联网信息")]
//! pub struct SearchArgs {
//!     /// 搜索关键词
//!     pub query: String,
//! }
//!
//! // 自动实现 ToolArgs trait
//! let def = SearchArgs::tool_definition();
//! assert_eq!(def.name, "search");
//! ```

/// 工具参数 trait
pub trait ToolArgs {
    /// 工具名称（蛇形命名）
    const NAME: &'static str;
    /// 工具描述
    const DESCRIPTION: &'static str;
    /// 由 derive(ToolDefinition) 宏生成的 JSON Schema
    fn __schema() -> serde_json::Value;
    /// 自动生成 ToolDefinition（含 JSON Schema）
    fn tool_definition() -> lellm_core::ToolDefinition {
        lellm_core::ToolDefinition {
            name: Self::NAME.to_string(),
            description: Self::DESCRIPTION.to_string(),
            parameters: Self::__schema(),
            cache_control: None,
        }
    }
}
