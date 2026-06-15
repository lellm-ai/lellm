//! lellm-macros — 派生宏与属性宏。
//!
//! # 三级 API
//!
//! ## Level 1: `#[tool]` 函数宏（推荐，95% 用户）
//!
//! ```ignore
//! use lellm_agent::ToolResult;
//! use lellm_macros::tool;
//!
//! #[tool(name = "search", description = "搜索互联网信息")]
//! async fn search(query: String, limit: Option<u32>) -> ToolResult {
//!     // 实现逻辑
//!     Ok(format!("搜索结果: {}", query))
//! }
//!
//! // 无依赖 — 直接调用生成的工厂函数：
//! builder.tool(search_tool());
//!
//! // 有依赖 — 使用 _with 后缀工厂函数：
//! let client = SearchClient::new();
//! builder.tool(search_tool_with({
//!     let client = client.clone();
//!     move |args| async move {
//!         client.search(&args.query, args.limit).await
//!     }
//! }));
//! ```
//!
//! ## Level 2: `#[derive(Tool)]` struct 宏（高级用户）
//!
//! ```ignore
//! use lellm_agent::ToolResult;
//! use lellm_macros::Tool;
//!
//! #[derive(Tool, JsonSchema)]
//! #[tool(name = "search", description = "搜索互联网信息")]
//! struct SearchArgs {
//!     /// 搜索关键词
//!     query: String,
//!     /// 返回数量
//!     limit: Option<u32>,
//! }
//!
//! // 注册：
//! let reg = SearchArgs::safe(|args| async move {
//!     Ok(format!("搜索结果: {}", args.query))
//! });
//! ```
//!
//! ## Level 3: `ToolRegistration::safe()`（框架开发者）
//!
//! ```ignore
//! use lellm_agent::{ToolDefinition, ToolRegistration};
//!
//! let reg = ToolRegistration::safe(
//!     ToolDefinition {
//!         name: "search".to_string(),
//!         description: "搜索".to_string(),
//!         parameters: serde_json::json!({
//!             "type": "object",
//!             "properties": { "query": { "type": "string" } }
//!         }),
//!     },
//!     |args| async { Ok(args["query"].as_str().unwrap().to_string()) }
//! );
//! ```

mod codegen;
mod fn_expand;
mod helpers;
mod struct_expand;

use proc_macro::TokenStream;
use proc_macro2::Span;
use syn::{DeriveInput, Item, parse_macro_input};

// ─────────────────────────────────────────────────────────────────
// Entry: #[tool] attribute macro (handles both fn and struct)
// ─────────────────────────────────────────────────────────────────

#[proc_macro_attribute]
pub fn tool(args: TokenStream, input: TokenStream) -> TokenStream {
    let parsed: Item = parse_macro_input!(input as Item);

    match parsed {
        Item::Fn(func) => {
            // Level 1: function → generate Args struct + factory functions
            match fn_expand::expand_tool_for_fn(args.into(), func) {
                Ok(out) => out.into(),
                Err(e) => e.to_compile_error().into(),
            }
        }
        Item::Struct(s) => {
            // Level 2: struct → generate ToolArgs impl
            match struct_expand::expand_tool_for_struct(args.into(), s) {
                Ok(out) => out.into(),
                Err(e) => e.to_compile_error().into(),
            }
        }
        other => {
            syn::Error::new_spanned(other, "#[tool] can only be applied to functions or structs")
                .to_compile_error()
                .into()
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Entry: #[derive(Tool)] derive macro
// ─────────────────────────────────────────────────────────────────

#[proc_macro_derive(Tool, attributes(tool))]
pub fn derive_tool(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    match &input.data {
        syn::Data::Struct(data) => struct_expand::generate_tool_for_struct(&input, data),
        _ => {
            let error = syn::Error::new(Span::call_site(), "Tool only supports struct types");
            error.to_compile_error().into()
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Backward compatibility: ToolDefinition alias
// ─────────────────────────────────────────────────────────────────

#[proc_macro_derive(ToolDefinition, attributes(tool))]
#[deprecated(since = "0.2.0", note = "Use `Tool` instead of `ToolDefinition`")]
pub fn derive_tool_definition(input: TokenStream) -> TokenStream {
    derive_tool(input)
}
