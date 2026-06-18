//! Code generation functions for tool macros.
//!
//! Generates the TokenStream code for:
//! - LazyLock schema caching
//! - Backward-compatible methods (__schema, __name, __description)
//! - Safe registration methods (safe, category_exclusive, exclusive)

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

/// Generate the ToolArgs::__schema() impl with LazyLock caching.
pub(crate) fn generate_schema_impl(struct_name: &syn::Ident) -> TokenStream2 {
    quote! {
        fn __schema() -> serde_json::Value {
            static SCHEMA: ::std::sync::LazyLock<serde_json::Value> =
                ::std::sync::LazyLock::new(|| {
                    let full = ::serde_json::to_value(
                        ::lellm_agent::schemars::schema_for!(#struct_name)
                    ).expect("schema generation failed");
                    #struct_name::extract_inner_schema(&full)
                });
            SCHEMA.clone()
        }
    }
}

/// Generate backward-compatible methods: __schema(), __name(), __description().
pub(crate) fn generate_compat_methods(struct_name: &syn::Ident) -> TokenStream2 {
    quote! {
        impl #struct_name {
            /// 从 schemars 完整 schema 中提取 inner schema（LazyLock 缓存）。
            pub fn __schema() -> serde_json::Value {
                <#struct_name as ::lellm_agent::ToolArgs>::__schema()
            }

            /// 工具名称 — 向后兼容
            pub fn __name() -> &'static str {
                <#struct_name as ::lellm_agent::ToolArgs>::NAME
            }

            /// 工具描述 — 向后兼容
            pub fn __description() -> &'static str {
                <#struct_name as ::lellm_agent::ToolArgs>::DESCRIPTION
            }

            fn extract_inner_schema(full: &serde_json::Value) -> serde_json::Value {
                let source = if let Some(obj) = full.as_object() {
                    obj
                } else {
                    return full.clone();
                };

                let skip = ["$schema", "title", "description", "definitions", "$id", "$ref"];
                let mut cleaned = serde_json::Map::new();
                for (k, v) in source {
                    if !skip.contains(&k.as_str()) {
                        cleaned.insert(k.clone(), v.clone());
                    }
                }
                serde_json::Value::Object(cleaned)
            }
        }
    }
}

/// Generate safe registration methods: safe(), category_exclusive(), exclusive().
pub(crate) fn generate_safe_methods(struct_name: &syn::Ident) -> TokenStream2 {
    quote! {
        impl #struct_name {
            /// 便捷注册 — 并行安全（Safe）。
            ///
            /// 闭包接收反序列化后的 `#struct_name`，直接操作强类型参数。
            pub fn safe<F, Fut>(f: F) -> ::lellm_agent::ToolRegistration
            where
                F: Fn(#struct_name) -> Fut + Send + Sync + 'static,
                Fut: ::core::future::Future<Output = ::lellm_agent::ToolResult> + Send + 'static,
            {
                let f = ::std::sync::Arc::new(f);
                ::lellm_agent::ToolRegistration::safe(
                    <#struct_name as ::lellm_agent::ToolArgs>::tool_definition(),
                    {
                        let f = ::std::sync::Arc::clone(&f);
                        move |args: &serde_json::Value| -> ::std::pin::Pin<Box<dyn ::core::future::Future<Output = ::lellm_agent::ToolResult> + Send + 'static>> {
                            match ::serde_json::from_value::<#struct_name>(args.clone()) {
                                Ok(parsed) => {
                                    let f = ::std::sync::Arc::clone(&f);
                                    Box::pin(async move { f(parsed).await })
                                }
                                Err(e) => Box::pin(async move {
                                    ::lellm_agent::ToolResult::Err(::lellm_core::ToolError {
                                        kind: ::lellm_core::ToolErrorKind::InvalidInput,
                                        message: format!(
                                            "Failed to parse tool arguments: {}",
                                            e
                                        ),
                                    })
                                }),
                            }
                        }
                    }
                )
            }

            /// 便捷注册 — 分类内互斥（CategoryExclusive）。
            pub fn category_exclusive<F, Fut>(
                category: ::lellm_agent::ToolCategory,
                f: F,
            ) -> ::lellm_agent::ToolRegistration
            where
                F: Fn(#struct_name) -> Fut + Send + Sync + 'static,
                Fut: ::core::future::Future<Output = ::lellm_agent::ToolResult> + Send + 'static,
            {
                let f = ::std::sync::Arc::new(f);
                ::lellm_agent::ToolRegistration::category_exclusive(
                    <#struct_name as ::lellm_agent::ToolArgs>::tool_definition(),
                    category,
                    {
                        let f = ::std::sync::Arc::clone(&f);
                        move |args: &serde_json::Value| -> ::std::pin::Pin<Box<dyn ::core::future::Future<Output = ::lellm_agent::ToolResult> + Send + 'static>> {
                            match ::serde_json::from_value::<#struct_name>(args.clone()) {
                                Ok(parsed) => {
                                    let f = ::std::sync::Arc::clone(&f);
                                    Box::pin(async move { f(parsed).await })
                                }
                                Err(e) => Box::pin(async move {
                                    ::lellm_agent::ToolResult::Err(::lellm_core::ToolError {
                                        kind: ::lellm_core::ToolErrorKind::InvalidInput,
                                        message: format!(
                                            "Failed to parse tool arguments: {}",
                                            e
                                        ),
                                    })
                                }),
                            }
                        }
                    }
                )
            }

            /// 便捷注册 — 全局互斥（Exclusive）。
            pub fn exclusive<F, Fut>(f: F) -> ::lellm_agent::ToolRegistration
            where
                F: Fn(#struct_name) -> Fut + Send + Sync + 'static,
                Fut: ::core::future::Future<Output = ::lellm_agent::ToolResult> + Send + 'static,
            {
                let f = ::std::sync::Arc::new(f);
                ::lellm_agent::ToolRegistration::exclusive(
                    <#struct_name as ::lellm_agent::ToolArgs>::tool_definition(),
                    {
                        let f = ::std::sync::Arc::clone(&f);
                        move |args: &serde_json::Value| -> ::std::pin::Pin<Box<dyn ::core::future::Future<Output = ::lellm_agent::ToolResult> + Send + 'static>> {
                            match ::serde_json::from_value::<#struct_name>(args.clone()) {
                                Ok(parsed) => {
                                    let f = ::std::sync::Arc::clone(&f);
                                    Box::pin(async move { f(parsed).await })
                                }
                                Err(e) => Box::pin(async move {
                                    ::lellm_agent::ToolResult::Err(::lellm_core::ToolError {
                                        kind: ::lellm_core::ToolErrorKind::InvalidInput,
                                        message: format!(
                                            "Failed to parse tool arguments: {}",
                                            e
                                        ),
                                    })
                                }),
                            }
                        }
                    }
                )
            }
        }
    }
}
