//! Code generation functions for tool macros.
//!
//! Generates the TokenStream code for:
//! - Schema caching (delegates to lellm-tool's compute_and_clean_schema)
//! - ToolArgs trait impl
//! - Backward-compatible methods
//! - Safe registration methods
//!
//! **原则：宏只拼 AST，所有运行逻辑都在 lellm-tool。**
//! 宏不直接引用 serde_json / schemars，只通过 lellm-tool 的 API 调用。

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

/// Generate the ToolArgs trait impl.
///
/// 委托给 lellm-tool 的 `compute_and_clean_schema::<T>()`，
/// 宏不需要知道 serde_json / schemars 的存在。
pub(crate) fn generate_tool_args_impl(
    struct_name: &syn::Ident,
    name: &str,
    description: &str,
) -> TokenStream2 {
    quote! {
        impl ::lellm_tool::ToolArgs for #struct_name {
            const NAME: &'static str = #name;
            const DESCRIPTION: &'static str = #description;

            fn schema() -> ::lellm_core::ToolSchema {
                static SCHEMA: ::std::sync::LazyLock<::lellm_core::ToolSchema> =
                    ::std::sync::LazyLock::new(|| {
                        ::lellm_tool::compute_and_clean_schema::<#struct_name>()
                    });
                SCHEMA.clone()
            }
        }
    }
}

/// Generate backward-compatible methods: schema(), __name(), __description(), tool_definition().
///
/// tool_definition() 覆盖 trait 默认方法，使用 LazyLock 缓存。
pub(crate) fn generate_compat_methods(struct_name: &syn::Ident) -> TokenStream2 {
    quote! {
        impl #struct_name {
            /// JSON Schema（LazyLock 缓存）。
            pub fn schema() -> ::lellm_core::ToolSchema {
                <#struct_name as ::lellm_tool::ToolArgs>::schema()
            }

            /// 工具名称 — 向后兼容
            pub fn __name() -> &'static str {
                <#struct_name as ::lellm_tool::ToolArgs>::NAME
            }

            /// 工具描述 — 向后兼容
            pub fn __description() -> &'static str {
                <#struct_name as ::lellm_tool::ToolArgs>::DESCRIPTION
            }

            /// ToolDefinition（LazyLock 缓存，避免重复 clone）。
            pub fn tool_definition() -> ::lellm_core::ToolDefinition {
                static DEF: ::std::sync::LazyLock<::lellm_core::ToolDefinition> =
                    ::std::sync::LazyLock::new(|| {
                        <#struct_name as ::lellm_tool::ToolArgs>::tool_definition()
                    });
                DEF.clone()
            }
        }
    }
}

/// Generate safe registration methods: safe(), category_exclusive(), exclusive().
///
/// 直接委托给 lellm-tool 的 `safe_fn()` 等强类型工厂函数，
/// 不再在宏中手动处理 parse / error / boxing。
pub(crate) fn generate_safe_methods(struct_name: &syn::Ident) -> TokenStream2 {
    quote! {
        impl #struct_name {
            /// 便捷注册 — 并行安全（Safe）。
            pub fn safe<F, Fut>(f: F) -> ::lellm_core::ExecutableTool
            where
                F: Fn(#struct_name) -> Fut + Send + Sync + 'static,
                Fut: ::core::future::Future<Output = ::lellm_core::ToolResult> + Send + 'static,
            {
                ::lellm_tool::safe_fn(
                    <#struct_name as ::lellm_tool::ToolArgs>::tool_definition(),
                    f,
                )
            }

            /// 便捷注册 — 分类内互斥（CategoryExclusive）。
            pub fn category_exclusive<F, Fut>(
                category: ::lellm_core::ToolCategory,
                f: F,
            ) -> ::lellm_core::ExecutableTool
            where
                F: Fn(#struct_name) -> Fut + Send + Sync + 'static,
                Fut: ::core::future::Future<Output = ::lellm_core::ToolResult> + Send + 'static,
            {
                ::lellm_tool::category_exclusive_fn(
                    <#struct_name as ::lellm_tool::ToolArgs>::tool_definition(),
                    category,
                    f,
                )
            }

            /// 便捷注册 — 全局互斥（Exclusive）。
            pub fn exclusive<F, Fut>(f: F) -> ::lellm_core::ExecutableTool
            where
                F: Fn(#struct_name) -> Fut + Send + Sync + 'static,
                Fut: ::core::future::Future<Output = ::lellm_core::ToolResult> + Send + 'static,
            {
                ::lellm_tool::exclusive_fn(
                    <#struct_name as ::lellm_tool::ToolArgs>::tool_definition(),
                    f,
                )
            }
        }
    }
}
