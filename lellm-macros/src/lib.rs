//! lellm-macros — 派生宏。
//!
//! 提供 `#[derive(ToolDefinition)]`，自动生成 JSON Schema 和参数反序列化。

use proc_macro::TokenStream;
use syn::{DeriveInput, parse_macro_input};

/// 为结构体生成 `ToolDefinition`。
///
/// 自动生成：
/// - `tool_definition() -> ToolDefinition` — JSON Schema
/// - `from_args(&serde_json::Value) -> Result<Self, ToolError>` — 参数反序列化
///
/// # 示例
/// ```rust
/// use lellm_macros::ToolDefinition;
///
/// #[derive(ToolDefinition)]
/// pub struct ReadFile {
///     /// 文件路径
///     pub path: String,
/// }
/// ```
#[proc_macro_derive(ToolDefinition, attributes(tool_desc))]
pub fn derive_tool_definition(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    // TODO: 实现 JSON Schema 生成 + from_args 反序列化
    let _ = input;
    TokenStream::new()
}
