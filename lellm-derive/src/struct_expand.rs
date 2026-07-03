//! Expansion of `#[tool]` on structs and `#[derive(Tool)]`.
//!
//! - `#[tool]` on struct: attribute macro path for struct types
//! - `derive(Tool)`: derive macro path for struct types
//!
//! Both generate the same ToolArgs impl + helper methods.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, ItemStruct, Meta};

use crate::codegen::{generate_compat_methods, generate_safe_methods, generate_tool_args_impl};
use crate::helpers::{
    extract_doc_from_attrs, extract_helper_meta, parse_struct_meta, parse_tool_meta_tokens,
};

/// Expand `#[tool]` applied to a struct (attribute macro path).
pub(crate) fn expand_tool_for_struct(
    args: TokenStream2,
    mut s: ItemStruct,
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
        crate::helpers::ident_to_snake_case(&s.ident.to_string())
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
    let tool_args_impl = generate_tool_args_impl(struct_name, &name, &description);
    let compat_methods = generate_compat_methods(struct_name);
    let safe_methods = generate_safe_methods(struct_name);

    Ok(quote! {
        #s

        #tool_args_impl

        #compat_methods
        #safe_methods
    })
}

/// Derive(Tool) implementation (shared by derive & attribute paths).
pub(crate) fn generate_tool_for_struct(
    input: &DeriveInput,
    _data: &syn::DataStruct,
) -> TokenStream {
    let struct_name = &input.ident;
    let (name, description) = parse_struct_meta(&input.attrs, input);

    let tool_args_impl = generate_tool_args_impl(struct_name, &name, &description);
    let compat_methods = generate_compat_methods(struct_name);
    let safe_methods = generate_safe_methods(struct_name);

    let generated = quote! {
        #tool_args_impl

        #compat_methods
        #safe_methods
    };

    TokenStream::from(generated)
}
