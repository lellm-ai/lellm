//! Helper functions for parsing macro metadata and extracting information.
//!
//! - ToolMeta parsing from token streams and attributes
//! - Doc comment extraction
//! - Function parameter extraction
//! - Name conversion utilities

use proc_macro2::TokenStream as TokenStream2;
use syn::{Attribute, DeriveInput, Expr, FnArg, Lit, Meta, Pat, punctuated::Punctuated};

// ─────────────────────────────────────────────────────────────────
// ToolMeta — parsed metadata from #[tool(...)]
// ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub(crate) struct ToolMeta {
    pub(crate) name: String,
    pub(crate) description: String,
}

/// Parse #[tool(name = "...", description = "...")] from TokenStream2
///
/// The args from proc_macro_attribute are the content inside #[tool(...)],
/// i.e.: `name = "test_search" , description = "..."`
pub(crate) fn parse_tool_meta_tokens(args: &TokenStream2) -> ToolMeta {
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
pub(crate) fn extract_helper_meta(attrs: &[Attribute]) -> ToolMeta {
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
pub(crate) fn extract_doc_from_attrs(attrs: &[Attribute]) -> Option<String> {
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

// ─────────────────────────────────────────────────────────────────
// FnParam — extracted function parameter info
// ─────────────────────────────────────────────────────────────────

pub(crate) struct FnParam {
    pub(crate) ident: syn::Ident,
    pub(crate) ty: syn::Type,
    pub(crate) doc_attrs: Vec<Attribute>,
}

pub(crate) fn extract_fn_params(
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

// ─────────────────────────────────────────────────────────────────
// Struct metadata parsing
// ─────────────────────────────────────────────────────────────────

pub(crate) fn parse_struct_meta(attrs: &[Attribute], input: &DeriveInput) -> (String, String) {
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

// ─────────────────────────────────────────────────────────────────
// Name conversion utilities
// ─────────────────────────────────────────────────────────────────

pub(crate) fn ident_to_snake_case(s: &str) -> String {
    use heck::ToSnakeCase;
    s.to_snake_case()
}

/// Convert snake_case or camelCase to PascalCase
pub(crate) fn snake_to_pascal(s: &str) -> String {
    use heck::ToPascalCase;
    s.to_pascal_case()
}
