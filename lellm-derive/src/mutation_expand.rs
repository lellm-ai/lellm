//! StateMutation derive — 只生成 `impl StateMutation<S>` 的 `apply()` 方法。
//!
//! # 设计理念
//!
//! Mutation 是领域事件，由开发者设计 enum 结构。
//! derive 只负责消除 `match self { ... }` 的样板代码。
//!
//! # 用法
//!
//! ```ignore
//! #[derive(StateMutation)]
//! #[state(AgentState)]
//! pub enum AgentMutation {
//!     #[mutation(state.messages.push(value))]
//!     AppendMessage(Message),
//!
//!     #[mutation(state.iterations += 1)]
//!     IncrementIteration,
//!
//!     #[mutation(state.total_tool_calls += value)]
//!     AddToolCalls(usize),
//!
//!     #[mutation(state.stop_reason = Some(value))]
//!     SetStopReason(StopReason),
//! }
//! ```
//!
//! 生成：
//!
//! ```ignore
//! impl StateMutation<AgentState> for AgentMutation {
//!     fn apply(self, state: &mut AgentState) {
//!         match self {
//!             AgentMutation::AppendMessage(value) => {
//!                 state.messages.push(value);
//!             }
//!             AgentMutation::IncrementIteration => {
//!                 state.iterations += 1;
//!             }
//!             // ...
//!         }
//!     }
//! }
//! ```

use proc_macro2::Span;
use quote::quote;
use syn::{Data, DeriveInput, Expr, Fields, Ident, Meta, parse_quote, spanned::Spanned};

/// 解析 `#[state(TypeName)]` 获取目标 State 类型。
fn parse_state_type(input: &DeriveInput) -> syn::Result<syn::Type> {
    for attr in &input.attrs {
        if !attr.path().is_ident("state") {
            continue;
        }
        return match &attr.meta {
            Meta::List(list) => list.parse_args::<syn::TypePath>().map(syn::Type::Path),
            _ => Err(syn::Error::new_spanned(
                &attr.meta,
                "#[state(TypeName)] expects a type in parentheses",
            )),
        };
    }
    Err(syn::Error::new(
        input.ident.span(),
        "Missing #[state(TypeName)] attribute on enum",
    ))
}

/// 从 variant 的 `#[mutation(expr)]` 中解析表达式。
fn parse_mutation_expr(variant: &syn::Variant) -> syn::Result<Expr> {
    for attr in &variant.attrs {
        if !attr.path().is_ident("mutation") {
            continue;
        }
        if let Meta::List(list) = &attr.meta {
            return list.parse_args::<Expr>();
        }
        return Err(syn::Error::new_spanned(
            &attr.meta,
            "#[mutation(expr)] expects a parenthesized expression",
        ));
    }
    Err(syn::Error::new(
        variant.ident.span(),
        "Missing #[mutation(expr)] attribute on variant. \
         Example: #[mutation(state.field.push(value))]",
    ))
}

/// 提取 variant 的参数名（用于 `value` 绑定）。
/// - Unit variant → None
/// - Named fields → Err（不支持）
/// - Single unnamed field → Some(field_ident)
fn extract_value_ident(variant: &syn::Variant) -> syn::Result<Option<Ident>> {
    match &variant.fields {
        Fields::Unit => Ok(None),
        Fields::Named(_) => Err(syn::Error::new(
            variant.fields.span(),
            "Named fields are not supported. Use tuple variant: Variant(value) instead",
        )),
        Fields::Unnamed(fields) => {
            if fields.unnamed.len() != 1 {
                return Err(syn::Error::new(
                    fields.span(),
                    "Only single-value tuple variants are supported: Variant(value)",
                ));
            }
            // Use the field's ident if present, otherwise generate `value`
            let ident = fields
                .unnamed
                .first()
                .and_then(|f| f.ident.clone())
                .unwrap_or_else(|| Ident::new("value", Span::call_site()));
            Ok(Some(ident))
        }
    }
}

pub fn generate_state_mutation(input: &DeriveInput) -> proc_macro2::TokenStream {
    let state_type = match parse_state_type(input) {
        Ok(ty) => ty,
        Err(e) => return e.to_compile_error(),
    };

    let data = match &input.data {
        Data::Enum(e) => e,
        _ => {
            return syn::Error::new(
                input.ident.span(),
                "StateMutation can only be derived on enums",
            )
            .to_compile_error();
        }
    };

    let enum_name = &input.ident;

    let arms: Vec<_> = data
        .variants
        .iter()
        .map(|variant| {
            let variant_name = &variant.ident;
            let expr = match parse_mutation_expr(variant) {
                Ok(e) => e,
                Err(e) => return e.to_compile_error(),
            };
            let value_ident = match extract_value_ident(variant) {
                Ok(id) => id,
                Err(e) => return e.to_compile_error(),
            };

            match value_ident {
                Some(ref val) => {
                    // Tuple variant with one value: Pattern(value)
                    quote! {
                        #enum_name::#variant_name(#val) => {
                            #expr;
                        }
                    }
                }
                None => {
                    // Unit variant: Pattern
                    quote! {
                        #enum_name::#variant_name => {
                            #expr;
                        }
                    }
                }
            }
        })
        .collect();

    // The trait path — use a well-known path that users can adjust with `use` statements
    let trait_path: syn::TypePath =
        parse_quote!(lellm_graph::state::workflow_state::StateMutation<#state_type>);

    quote! {
        impl #trait_path for #enum_name {
            fn apply(self, state: &mut #state_type) {
                match self {
                    #(#arms),*
                }
            }
        }
    }
}
