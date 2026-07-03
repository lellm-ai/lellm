//! Code generation functions for tool macros.
//!
//! Generates the TokenStream code for:
//! - Schema caching (delegates to core's compute_and_clean_schema)
//! - Backward-compatible methods (__schema, __name, __description)
//! - Safe registration methods (safe, category_exclusive, exclusive)
//!
//! **原则：宏只拼 AST，所有运行逻辑都在 core。**
//! 宏不直接引用 serde_json / schemars，只通过 core 的 API 调用。

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

/// Generate the ToolArgs::__schema() impl.
///
/// 委托给 core 的 `ToolDefinition::compute_and_clean_schema::<T>()`，
/// 宏不需要知道 serde_json / schemars 的存在。
pub(crate) fn generate_schema_impl(struct_name: &syn::Ident) -> TokenStream2 {
    quote! {
        fn __schema() -> ::lellm_core::ToolSchema {
            static SCHEMA: ::std::sync::LazyLock<::lellm_core::ToolSchema> =
                ::std::sync::LazyLock::new(|| {
                    ::lellm_core::ToolDefinition::compute_and_clean_schema::<#struct_name>()
                });
            SCHEMA.clone()
        }
    }
}

/// Generate backward-compatible methods: __schema(), __name(), __description().
///
/// 不再生成 `extract_inner_schema()`——清洗逻辑已统一在 core 的 `clean_schema()` 中。
pub(crate) fn generate_compat_methods(struct_name: &syn::Ident) -> TokenStream2 {
    quote! {
        impl #struct_name {
            /// 从 schemars 完整 schema 中提取 inner schema（LazyLock 缓存）。
            pub fn __schema() -> ::lellm_core::ToolSchema {
                <#struct_name as ::lellm_core::ToolArgs>::__schema()
            }

            /// 工具名称 — 向后兼容
            pub fn __name() -> &'static str {
                <#struct_name as ::lellm_core::ToolArgs>::NAME
            }

            /// 工具描述 — 向后兼容
            pub fn __description() -> &'static str {
                <#struct_name as ::lellm_core::ToolArgs>::DESCRIPTION
            }
        }
    }
}

/// Generate safe registration methods: safe(), category_exclusive(), exclusive().
///
/// 直接委托给 core 的 `ExecutableTool::safe_fn()` 等强类型工厂方法，
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
                ::lellm_core::ExecutableTool::safe_fn(
                    <#struct_name as ::lellm_core::ToolArgs>::tool_definition(),
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
                ::lellm_core::ExecutableTool::category_exclusive_fn(
                    <#struct_name as ::lellm_core::ToolArgs>::tool_definition(),
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
                ::lellm_core::ExecutableTool::exclusive_fn(
                    <#struct_name as ::lellm_core::ToolArgs>::tool_definition(),
                    f,
                )
            }
        }
    }
}
