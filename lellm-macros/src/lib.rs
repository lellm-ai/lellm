//! lellm-macros — 派生宏。
//!
//! 提供 `#[derive(ToolDefinition)]`，自动生成：
//! 1. `lellm_agent::ToolArgs` impl — 工具注册 trait
//! 2. 向后兼容的 `__schema()` / `__name()` / `__description()` 方法
//!
//! Schema 生成使用 schemars 兼容的类型推断逻辑。
//!
//! # 示例
//! ```ignore
//! use lellm_macros::ToolDefinition;
//!
//! #[derive(ToolDefinition)]
//! #[tool(name = "search", description = "搜索互联网信息")]
//! pub struct SearchArgs {
//!     /// 搜索关键词
//!     pub query: String,
//! }
//! ```

use proc_macro::TokenStream;
use quote::quote;
use syn::{DataStruct, DeriveInput, Field, parse_macro_input};

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

fn generate_for_struct(input: &DeriveInput, data: &DataStruct) -> TokenStream {
    let struct_name = &input.ident;
    let (name, description) = parse_tool_attrs(&input.attrs, &input.ident);

    // 提取字段信息
    let mut field_names = Vec::new();
    let mut field_schemas = Vec::new();
    let mut field_required = Vec::new();

    for field in &data.fields {
        let fname = field.ident.as_ref().expect("named fields required");
        let fname_str = fname.to_string();
        let fname_lit = syn::LitStr::new(&fname_str, proc_macro2::Span::call_site());
        let schema = field_schema(field);
        let is_req = !is_option_type(&field.ty);

        field_names.push(fname_lit);
        field_schemas.push(schema);
        if is_req {
            field_required.push(syn::LitStr::new(&fname_str, proc_macro2::Span::call_site()));
        }
    }

    let generated = quote! {
        impl #struct_name {
            /// 自动生成 JSON Schema（schemars 兼容格式）— 向后兼容
            pub fn __schema() -> serde_json::Value {
                <Self as lellm_agent::ToolArgs>::__schema()
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

        impl lellm_agent::ToolArgs for #struct_name {
            const NAME: &'static str = #name;
            const DESCRIPTION: &'static str = #description;

            fn __schema() -> serde_json::Value {
                let mut properties = serde_json::Map::new();
                #(
                    properties.insert(#field_names.to_string(), #field_schemas);
                )*

                let mut schema = serde_json::json!({
                    "type": "object",
                    "properties": properties,
                });

                let required: Vec<String> = vec![#(#field_required.to_string()),*];
                if !required.is_empty() {
                    schema["required"] = serde_json::Value::Array(
                        required.iter().map(|s| serde_json::json!(s)).collect()
                    );
                }

                schema
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

/// 生成字段的 JSON Schema 表达式。
/// 使用 schemars 兼容的类型映射。
fn field_schema(field: &Field) -> proc_macro2::TokenStream {
    let doc = field_doc(&field.attrs);
    let basic_type = field_json_type(field);

    quote! {
        serde_json::json!({
            "type": #basic_type,
            "description": #doc
        })
    }
}

fn field_doc(attrs: &[syn::Attribute]) -> syn::LitStr {
    let doc_string: String = attrs
        .iter()
        .filter_map(|a| {
            if a.path().is_ident("doc")
                && let syn::Meta::NameValue(nv) = &a.meta
                && let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(lit_str),
                    ..
                }) = &nv.value
            {
                // 移除 doc comment 常见的空白字符
                return Some(lit_str.value().trim().to_string());
            }
            None
        })
        .next()
        .unwrap_or_default();

    syn::LitStr::new(&doc_string, proc_macro2::Span::call_site())
}

/// 推断字段的 JSON Schema 类型（schemars 兼容）。
fn field_json_type(field: &Field) -> syn::LitStr {
    let ty = &field.ty;
    // Option<T> — 推导内部类型
    let (ty, _is_optional) = if let Some(inner_ty) = option_inner_type(ty) {
        (inner_ty, true)
    } else {
        (ty.clone(), false)
    };

    let json_type = if is_string_type(&ty) {
        "string"
    } else if is_number_type(&ty) {
        "number"
    } else if is_integer_type(&ty) {
        "integer"
    } else if is_bool_type(&ty) {
        "boolean"
    } else if is_array_type(&ty) {
        "array"
    } else {
        "string"
    };

    syn::LitStr::new(json_type, proc_macro2::Span::call_site())
}

/// 如果类型是 Option<T>，返回 T 的类型。
fn option_inner_type(ty: &syn::Type) -> Option<syn::Type> {
    let type_path = if let syn::Type::Path(p) = ty {
        p
    } else {
        return None;
    };

    let segment = type_path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }

    // 提取 Option<T> 的泛型参数
    let first_arg = match &segment.arguments {
        syn::PathArguments::AngleBracketed(args) => args.args.first(),
        _ => return None,
    };
    let first_arg = first_arg?;
    let inner_ty = match first_arg {
        syn::GenericArgument::Type(ty) => ty.clone(),
        _ => return None,
    };
    Some(inner_ty)
}

fn is_option_type(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(type_path) if type_path.path.segments.last().map(|s| s.ident == "Option").unwrap_or(false))
}

fn is_string_type(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(type_path) if type_path.path.segments.last().map(|s| s.ident == "String").unwrap_or(false))
}

fn is_integer_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty {
        type_path
            .path
            .segments
            .last()
            .map(|s| {
                let n = s.ident.to_string();
                [
                    "u8", "u16", "u32", "u64", "usize", "i8", "i16", "i32", "i64", "isize",
                ]
                .contains(&n.as_str())
            })
            .unwrap_or(false)
    } else {
        false
    }
}

fn is_number_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty {
        type_path
            .path
            .segments
            .last()
            .map(|s| matches!(s.ident.to_string().as_str(), "f32" | "f64"))
            .unwrap_or(false)
    } else {
        false
    }
}

fn is_bool_type(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(type_path) if type_path.path.segments.last().map(|s| s.ident == "bool").unwrap_or(false))
}

fn is_array_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty {
        type_path
            .path
            .segments
            .last()
            .map(|s| s.ident == "Vec")
            .unwrap_or(false)
    } else {
        false
    }
}
