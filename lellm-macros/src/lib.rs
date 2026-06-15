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

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{
    Attribute, DeriveInput, Expr, FnArg, Item, ItemFn, Lit, Meta, Pat, parse_macro_input,
    parse_quote, punctuated::Punctuated,
};

// ─────────────────────────────────────────────────────────────────
// Entry: #[tool] attribute macro (handles both fn and struct)
// ─────────────────────────────────────────────────────────────────

#[proc_macro_attribute]
pub fn tool(args: TokenStream, input: TokenStream) -> TokenStream {
    let parsed: Item = parse_macro_input!(input as Item);

    match parsed {
        Item::Fn(func) => {
            // Level 1: function → generate Args struct + factory functions
            match expand_tool_for_fn(args.into(), func) {
                Ok(out) => out.into(),
                Err(e) => e.to_compile_error().into(),
            }
        }
        Item::Struct(s) => {
            // Level 2: struct → generate ToolArgs impl
            match expand_tool_for_struct(args.into(), s) {
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
        syn::Data::Struct(data) => generate_tool_for_struct(&input, data),
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

// ─────────────────────────────────────────────────────────────────
// Level 1: #[tool] on function
// ─────────────────────────────────────────────────────────────────

fn expand_tool_for_fn(args: TokenStream2, func: ItemFn) -> Result<TokenStream2, syn::Error> {
    // Parse #[tool(name = "...", description = "...")] meta
    let meta = parse_tool_meta_tokens(&args);

    // Determine name
    let name = if meta.name.is_empty() {
        ident_to_snake_case(&func.sig.ident.to_string())
    } else {
        meta.name.clone()
    };

    // Determine description (from attribute or doc comment)
    let description = if !meta.description.is_empty() {
        meta.description.clone()
    } else {
        extract_doc_from_attrs(&func.attrs).unwrap_or_default()
    };

    // Extract parameters
    let params = extract_fn_params(&func.sig.inputs)?;
    // Check if function is async
    let is_async = func.sig.asyncness.is_some();
    // Convert fn name to PascalCase for struct name: test_search_fn → TestSearchFn
    let pascal_name = snake_to_pascal(&func.sig.ident.to_string());
    let struct_name = format_ident!("{}Args", pascal_name);
    let reg_fn_name = format_ident!("{}_tool", func.sig.ident);
    let reg_fn_name_with = format_ident!("{}_tool_with", func.sig.ident);
    let fn_name = &func.sig.ident;

    // Generate struct fields using parse_quote for each param
    let fields: Vec<syn::Field> = params
        .iter()
        .map(|p| {
            let ident = &p.ident;
            let ty = &p.ty;
            let doc_attrs = &p.doc_attrs;
            parse_quote! {
                #(#doc_attrs)*
                pub #ident: #ty
            }
        })
        .collect();

    // Build arg references for function call: args.query, args.limit
    let arg_refs: Vec<proc_macro2::TokenStream> = params
        .iter()
        .map(|p| {
            let ident = &p.ident;
            quote! { args.#ident }
        })
        .collect();

    // await suffix for async functions
    let await_suffix = if is_async {
        quote! { .await }
    } else {
        quote! {}
    };

    // Clean the original function (remove #[tool] attr)
    let mut cleaned_func = func.clone();
    cleaned_func
        .attrs
        .retain(|attr| !attr.path().is_ident("tool"));

    let visibility = &func.vis;

    // Generate ToolArgs impl + helper methods
    let schema_fn = generate_schema_impl(&struct_name);
    let compat_methods = generate_compat_methods(&struct_name);
    let safe_methods = generate_safe_methods(&struct_name);

    Ok(quote! {
        // 1. 原始函数 — 保留语义，可直接调用
        #cleaned_func

        // 2. 自动生成的参数结构体
        /// Auto-generated tool arguments for `#fn_name`
        #[derive(
            ::lellm_agent::serde::Deserialize,
            ::lellm_agent::schemars::JsonSchema
        )]
        #visibility struct #struct_name {
            #(#fields),*
        }

        // 3. ToolArgs trait 实现（含 LazyLock schema 缓存）
        impl ::lellm_agent::ToolArgs for #struct_name {
            const NAME: &'static str = #name;
            const DESCRIPTION: &'static str = #description;

            #schema_fn
        }

        // 4. 向后兼容方法
        #compat_methods

        // 5. safe / category_exclusive / exclusive 便捷注册
        #safe_methods

        // 6. 无依赖工厂函数 — 调用原始函数
        /// Auto-generated tool registration for `#fn_name` (no dependency injection).
        #visibility fn #reg_fn_name() -> ::lellm_agent::ToolRegistration {
            #reg_fn_name_with(|args| async move {
                #fn_name(#(#arg_refs),*) #await_suffix
            })
        }

        // 7. 依赖注入工厂函数 — 用户传入闭包
        /// Tool registration factory with dependency injection.
        ///
        /// Pass a closure that receives `#struct_name` and returns `ToolResult`.
        ///
        /// # Example
        /// ```ignore
        /// let client = MyClient::new();
        /// builder.tool(#reg_fn_name_with({
        ///     let client = client.clone();
        ///     move |args| async move {
        ///         client.do_something(&args.field).await
        ///     }
        /// }));
        /// ```
        #visibility fn #reg_fn_name_with<F, Fut>(f: F) -> ::lellm_agent::ToolRegistration
        where
            F: Fn(#struct_name) -> Fut + Send + Sync + 'static,
            Fut: ::core::future::Future<Output = ::lellm_agent::ToolResult> + Send + 'static,
        {
            #struct_name::safe(f)
        }
    })
}

// ─────────────────────────────────────────────────────────────────
// Level 2: #[tool] on struct (attribute macro path)
// ─────────────────────────────────────────────────────────────────

fn expand_tool_for_struct(
    args: TokenStream2,
    mut s: syn::ItemStruct,
) -> Result<TokenStream2, syn::Error> {
    // Parse #[tool(name = "...", description = "...")] meta
    let meta = parse_tool_meta_tokens(&args);

    let struct_name = &s.ident;

    // If no explicit name/description in args, check existing #[tool(...)] helper attrs
    let helper_meta = extract_helper_meta(&s.attrs);

    let name = if !meta.name.is_empty() {
        meta.name.clone()
    } else if !helper_meta.name.is_empty() {
        helper_meta.name.clone()
    } else {
        ident_to_snake_case(&s.ident.to_string())
    };

    let description = if !meta.description.is_empty() {
        meta.description.clone()
    } else if !helper_meta.description.is_empty() {
        helper_meta.description.clone()
    } else {
        extract_doc_from_attrs(&s.attrs).unwrap_or_default()
    };

    // Remove old #[tool(...)] helper attrs to avoid leaking
    s.attrs
        .retain(|attr| !(attr.path().is_ident("tool") && matches!(&attr.meta, Meta::List(_))));

    // Generate the same output as derive(Tool) would
    let schema_fn = generate_schema_impl(struct_name);
    let compat_methods = generate_compat_methods(struct_name);
    let safe_methods = generate_safe_methods(struct_name);

    Ok(quote! {
        #s

        impl ::lellm_agent::ToolArgs for #struct_name {
            const NAME: &'static str = #name;
            const DESCRIPTION: &'static str = #description;

            #schema_fn
        }

        #compat_methods
        #safe_methods
    })
}

// ─────────────────────────────────────────────────────────────────
// Derive(Tool) implementation (shared by derive & attribute paths)
// ─────────────────────────────────────────────────────────────────

fn generate_tool_for_struct(input: &DeriveInput, _data: &syn::DataStruct) -> TokenStream {
    let struct_name = &input.ident;
    let (name, description) = parse_struct_meta(&input.attrs, input);

    let schema_fn = generate_schema_impl(struct_name);
    let compat_methods = generate_compat_methods(struct_name);
    let safe_methods = generate_safe_methods(struct_name);

    let generated = quote! {
        impl ::lellm_agent::ToolArgs for #struct_name {
            const NAME: &'static str = #name;
            const DESCRIPTION: &'static str = #description;

            #schema_fn
        }

        #compat_methods
        #safe_methods
    };

    TokenStream::from(generated)
}

fn generate_schema_impl(struct_name: &syn::Ident) -> proc_macro2::TokenStream {
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

fn generate_compat_methods(struct_name: &syn::Ident) -> proc_macro2::TokenStream {
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

fn generate_safe_methods(struct_name: &syn::Ident) -> proc_macro2::TokenStream {
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

// ─────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct ToolMeta {
    name: String,
    description: String,
}

/// Parse #[tool(name = "...", description = "...")] from TokenStream2
///
/// The args from proc_macro_attribute are the content inside #[tool(...)],
/// i.e.: `name = "test_search" , description = "..."`
fn parse_tool_meta_tokens(args: &TokenStream2) -> ToolMeta {
    let mut name = String::new();
    let mut description = String::new();

    if args.is_empty() {
        return ToolMeta::default();
    }

    // Manually iterate over tokens to find name = "..." and description = "..."
    let tokens: Vec<proc_macro2::TokenTree> = args.clone().into_iter().collect();
    let mut i = 0;
    while i < tokens.len() {
        // Look for identifier
        if let proc_macro2::TokenTree::Ident(ident) = &tokens[i] {
            let ident_name = ident.to_string();
            // Look for = sign
            if i + 1 < tokens.len() {
                if let proc_macro2::TokenTree::Punct(p) = &tokens[i + 1] {
                    if p.as_char() == '=' {
                        // Look for string literal
                        if i + 2 < tokens.len() {
                            if let proc_macro2::TokenTree::Literal(lit) = &tokens[i + 2] {
                                let lit_str = lit.to_string();
                                // Strip quotes
                                let val =
                                    lit_str.trim_matches(|c| c == '"' || c == '\'').to_string();
                                if ident_name == "name" {
                                    name = val;
                                } else if ident_name == "description" {
                                    description = val;
                                }
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }

    ToolMeta { name, description }
}

/// Extract #[tool(name = "...", description = "...")] from struct attrs (helper attrs)
fn extract_helper_meta(attrs: &[Attribute]) -> ToolMeta {
    let mut name = String::new();
    let mut description = String::new();

    for attr in attrs {
        if !attr.path().is_ident("tool") {
            continue;
        }
        if let Meta::List(ml) = &attr.meta {
            let _ = ml.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    let content = meta.value()?;
                    let lit: syn::LitStr = content.parse()?;
                    name = lit.value();
                } else if meta.path.is_ident("description") {
                    let content = meta.value()?;
                    let lit: syn::LitStr = content.parse()?;
                    description = lit.value();
                }
                Ok(())
            });
        }
    }

    ToolMeta { name, description }
}

/// Extract doc comment from attributes
fn extract_doc_from_attrs(attrs: &[Attribute]) -> Option<String> {
    let mut docs = Vec::new();
    for attr in attrs {
        if attr.path().is_ident("doc") {
            if let Meta::NameValue(nv) = &attr.meta {
                if let Expr::Lit(el) = &nv.value {
                    if let Lit::Str(s) = &el.lit {
                        for line in s.value().lines() {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                docs.push(trimmed.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    if docs.is_empty() {
        None
    } else {
        Some(docs.join(" "))
    }
}

struct FnParam {
    ident: syn::Ident,
    ty: syn::Type,
    doc_attrs: Vec<Attribute>,
}

fn extract_fn_params(
    inputs: &Punctuated<FnArg, syn::Token![,]>,
) -> Result<Vec<FnParam>, syn::Error> {
    let mut result = Vec::new();

    for arg in inputs.iter() {
        match arg {
            FnArg::Typed(typed) => {
                let ident = match &*typed.pat {
                    Pat::Ident(pat_ident) => pat_ident.ident.clone(),
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &typed.pat,
                            "only simple identifiers are supported as tool parameters",
                        ));
                    }
                };

                // Extract doc comments from the parameter
                let doc_attrs: Vec<Attribute> = typed
                    .attrs
                    .iter()
                    .filter(|a| a.path().is_ident("doc"))
                    .cloned()
                    .collect();

                result.push(FnParam {
                    ident,
                    ty: (*typed.ty).clone(),
                    doc_attrs,
                });
            }
            FnArg::Receiver(_recv) => {
                return Err(syn::Error::new_spanned(
                    _recv,
                    "self parameters are not supported in #[tool] functions",
                ));
            }
        }
    }

    Ok(result)
}

fn parse_struct_meta(attrs: &[Attribute], input: &DeriveInput) -> (String, String) {
    let helper_meta = extract_helper_meta(attrs);
    let doc = extract_doc_from_attrs(attrs);

    let name = if !helper_meta.name.is_empty() {
        helper_meta.name
    } else {
        ident_to_snake_case(&input.ident.to_string())
    };

    let description = if !helper_meta.description.is_empty() {
        helper_meta.description
    } else {
        doc.unwrap_or_default()
    };

    (name, description)
}

fn ident_to_snake_case(s: &str) -> String {
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

/// Convert snake_case or camelCase to PascalCase
fn snake_to_pascal(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;

    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else {
            if capitalize_next {
                result.push(c.to_uppercase().next().unwrap());
                capitalize_next = false;
            } else {
                result.push(c);
            }
        }
    }

    result
}
