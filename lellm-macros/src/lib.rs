//! lellm-macros — 派生宏。
//!
//! 提供 `#[derive(ToolDefinition)]`，自动生成：
//! 1. `lellm_agent::ToolArgs` impl — 工具注册 trait
//! 2. 向后兼容的 `__schema()` / `__name()` / `__description()` 方法
//!
//! Schema 生成委托给 [schemars](https://crates.io/crates/schemars)，
//! 用户 struct 需要同时 `#[derive(schemars::JsonSchema)]`。
//!
//! # 示例
//! ```ignore
//! use lellm_macros::ToolDefinition;
//! use lellm_agent::schemars::JsonSchema;
//!
//! #[derive(JsonSchema, ToolDefinition)]
//! #[tool(name = "search", description = "搜索互联网信息")]
//! pub struct SearchArgs {
//!     /// 搜索关键词
//!     pub query: String,
//! }
//! ```

use proc_macro::TokenStream;
use quote::quote;
use syn::{DataStruct, DeriveInput, parse_macro_input};

#[proc_macro_derive(ToolDefinition, attributes(tool))]
pub fn derive_tool_definition(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    match &input.data {
        syn::Data::Struct(data) => generate_for_struct(&input, data),
        _ => {
            let error = syn::Error::new(
                proc_macro2::Span::call_site(),
                "ToolDefinition only supports struct types",
            );
            error.to_compile_error().into()
        }
    }
}

fn generate_for_struct(input: &DeriveInput, _data: &DataStruct) -> TokenStream {
    let struct_name = &input.ident;
    let (name, description) = parse_tool_attrs(&input.attrs, &input.ident);

    let generated = quote! {
        impl ::lellm_agent::ToolArgs for #struct_name {
            const NAME: &'static str = #name;
            const DESCRIPTION: &'static str = #description;

            fn __schema() -> serde_json::Value {
                ::serde_json::to_value(
                    ::lellm_agent::schemars::schema_for!(#struct_name)
                ).expect("schemars schema_for always produces valid JSON")
            }
        }

        impl #struct_name {
            /// 自动生成 JSON Schema（schemars 驱动）— 向后兼容
            pub fn __schema() -> serde_json::Value {
                <Self as ::lellm_agent::ToolArgs>::__schema()
            }

            /// 工具名称 — 向后兼容
            pub fn __name() -> &'static str {
                Self::NAME
            }

            /// 工具描述 — 向后兼容
            pub fn __description() -> &'static str {
                Self::DESCRIPTION
            }
        }
    };

    TokenStream::from(generated)
}

fn parse_tool_attrs(attrs: &[syn::Attribute], ident: &syn::Ident) -> (syn::LitStr, syn::LitStr) {
    let mut name = String::new();
    let mut description = String::new();

    for attr in attrs {
        if !attr.path().is_ident("tool") {
            continue;
        }

        if let syn::Meta::List(meta_list) = &attr.meta {
            let _ = meta_list.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    let lit: syn::Lit = meta.value()?.parse()?;
                    if let syn::Lit::Str(lit_str) = lit {
                        name = lit_str.value();
                    }
                } else if meta.path.is_ident("description") {
                    let lit: syn::Lit = meta.value()?.parse()?;
                    if let syn::Lit::Str(lit_str) = lit {
                        description = lit_str.value();
                    }
                }
                Ok(())
            });
        }
    }

    if name.is_empty() {
        name = ident_to_snake_case(ident.to_string());
    }

    (
        syn::LitStr::new(&name, proc_macro2::Span::call_site()),
        syn::LitStr::new(&description, proc_macro2::Span::call_site()),
    )
}

fn ident_to_snake_case(s: String) -> String {
    let mut result = String::new();
    let mut prev_upper = false;

    for c in s.chars() {
        if c.is_uppercase() && !prev_upper && !result.is_empty() {
            result.push('_');
        }
        result.push(c.to_lowercase().next().unwrap());
        prev_upper = c.is_uppercase();
    }

    result
}
