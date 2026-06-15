//! Expansion of `#[tool]` on functions.
//!
//! Transforms a function into:
//! - Original function (cleaned of #[tool] attr)
//! - Args struct with Deserialize + JsonSchema
//! - ToolArgs trait impl with LazyLock schema
//! - Backward-compatible methods
//! - Safe registration methods
//! - `_tool()` factory function (no dependency injection)
//! - `_tool_with()` factory function (dependency injection)

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::ItemFn;

use crate::codegen::{generate_compat_methods, generate_safe_methods, generate_schema_impl};
use crate::helpers::{
    extract_doc_from_attrs, extract_fn_params, parse_tool_meta_tokens, snake_to_pascal,
};

/// Expand `#[tool]` applied to a function.
///
/// Generates an Args struct, factory functions, and all supporting code.
pub(crate) fn expand_tool_for_fn(
    args: TokenStream2,
    func: ItemFn,
) -> Result<TokenStream2, syn::Error> {
    // Parse #[tool(name = "...", description = "...")] meta
    let meta = parse_tool_meta_tokens(&args);

    // Determine name
    let name = if meta.name.is_empty() {
        crate::helpers::ident_to_snake_case(&func.sig.ident.to_string())
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
            syn::parse_quote! {
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
