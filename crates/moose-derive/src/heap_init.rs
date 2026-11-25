//! `#[derive(HeapInit)]`: in-place heap construction without stack temporaries.
//!
//! Generates `impl HeapInit for T` writing every field directly into
//! `Box::<Self>::new_uninit()` via `core::ptr::addr_of_mut!`, recursing into
//! nested `HeapInit` fields via autoref dispatch and falling back to
//! `Default::default()` for other field types (or a per-field
//! `#[heap_init(with = expr)]` override).

use std::collections::HashSet;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Attribute, Data, DeriveInput, Expr, Fields, GenericArgument, GenericParam, Ident, Path,
    PathArguments, Type, WherePredicate, parse_quote,
};

enum FieldInitMode {
    Auto,
    With(Expr),
    Default,
    SomeBox,
}

struct StructAttrs {
    crate_path: Path,
}

fn parse_struct_attrs(attrs: &[Attribute]) -> syn::Result<StructAttrs> {
    let mut crate_path: Path = parse_quote!(::moose::core::heap_init);
    for attr in attrs {
        if !attr.path().is_ident("heap_init") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("crate") {
                let value = meta.value()?;
                if value.peek(syn::LitStr) {
                    let lit: syn::LitStr = value.parse()?;
                    crate_path = lit.parse()?;
                } else {
                    crate_path = value.parse()?;
                }
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported #[heap_init(...)] struct attribute; expected `crate = ...`",
                ))
            }
        })?;
    }
    Ok(StructAttrs { crate_path })
}

fn parse_field_attrs(attrs: &[Attribute]) -> syn::Result<FieldInitMode> {
    let mut mode: Option<FieldInitMode> = None;
    for attr in attrs {
        if !attr.path().is_ident("heap_init") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("with") {
                let expr: Expr = meta.value()?.parse()?;
                if mode.is_some() {
                    return Err(meta.error("duplicate #[heap_init(...)] directive on field"));
                }
                mode = Some(FieldInitMode::With(expr));
                Ok(())
            } else if meta.path.is_ident("default") {
                if mode.is_some() {
                    return Err(meta.error("duplicate #[heap_init(...)] directive on field"));
                }
                mode = Some(FieldInitMode::Default);
                Ok(())
            } else if meta.path.is_ident("some") {
                if mode.is_some() {
                    return Err(meta.error("duplicate #[heap_init(...)] directive on field"));
                }
                mode = Some(FieldInitMode::SomeBox);
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported #[heap_init(...)] field attribute; expected `with = <expr>`, `default`, or `some`",
                ))
            }
        })?;
    }
    Ok(mode.unwrap_or(FieldInitMode::Auto))
}

fn extract_option_box_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(tp) = ty else { return None };
    let seg = tp.path.segments.last()?;
    if seg.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(opt_args) = &seg.arguments else {
        return None;
    };
    let GenericArgument::Type(Type::Path(box_tp)) = opt_args.args.first()? else {
        return None;
    };
    let box_seg = box_tp.path.segments.last()?;
    if box_seg.ident != "Box" {
        return None;
    }
    let PathArguments::AngleBracketed(box_args) = &box_seg.arguments else {
        return None;
    };
    let GenericArgument::Type(inner) = box_args.args.first()? else {
        return None;
    };
    Some(inner)
}

fn collect_type_params(
    ty: &Type,
    declared: &HashSet<Ident>,
    stop_at_unconditional: bool,
    out: &mut Vec<Ident>,
) {
    match ty {
        Type::Path(tp) => {
            if tp.qself.is_none()
                && tp.path.leading_colon.is_none()
                && tp.path.segments.len() == 1
                && tp.path.segments[0].arguments.is_empty()
            {
                let ident = &tp.path.segments[0].ident;
                if declared.contains(ident) && !out.contains(ident) {
                    out.push(ident.clone());
                }
                return;
            }
            if stop_at_unconditional
                && let Some(last) = tp.path.segments.last()
                && matches!(
                    last.ident.to_string().as_str(),
                    "Option" | "Vec" | "PhantomData"
                )
            {
                return;
            }
            for seg in &tp.path.segments {
                if let PathArguments::AngleBracketed(args) = &seg.arguments {
                    for arg in &args.args {
                        if let GenericArgument::Type(inner) = arg {
                            collect_type_params(inner, declared, stop_at_unconditional, out);
                        }
                    }
                }
            }
        }
        Type::Array(arr) => {
            collect_type_params(&arr.elem, declared, stop_at_unconditional, out);
        }
        Type::Tuple(tup) => {
            for elem in &tup.elems {
                collect_type_params(elem, declared, stop_at_unconditional, out);
            }
        }
        Type::Paren(p) => {
            collect_type_params(&p.elem, declared, stop_at_unconditional, out);
        }
        Type::Group(g) => {
            collect_type_params(&g.elem, declared, stop_at_unconditional, out);
        }
        _ => {}
    }
}

fn cfg_attrs(attrs: &[Attribute]) -> Vec<&Attribute> {
    attrs.iter().filter(|a| a.path().is_ident("cfg")).collect()
}

#[allow(clippy::too_many_lines)]
pub(crate) fn derive_heap_init(input: TokenStream) -> TokenStream {
    let ast: DeriveInput = syn::parse(input).expect("Failed to parse input for HeapInit derive");
    let struct_attrs = match parse_struct_attrs(&ast.attrs) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error().into(),
    };
    let crate_path = &struct_attrs.crate_path;
    let name = &ast.ident;

    let Data::Struct(data_struct) = &ast.data else {
        return syn::Error::new_spanned(&ast, "HeapInit can only be derived on structs")
            .to_compile_error()
            .into();
    };

    let declared_type_params: HashSet<Ident> = ast
        .generics
        .params
        .iter()
        .filter_map(|p| match p {
            GenericParam::Type(tp) => Some(tp.ident.clone()),
            _ => None,
        })
        .collect();

    let mut heap_init_bound_params: Vec<Ident> = Vec::new();
    let mut default_bound_params: Vec<Ident> = Vec::new();

    let mut completeness_fields: Vec<TokenStream2> = Vec::new();
    let mut init_stmts: Vec<TokenStream2> = Vec::new();
    let mut disarm_stmts: Vec<TokenStream2> = Vec::new();

    let is_named = matches!(&data_struct.fields, Fields::Named(_));
    let is_unit = matches!(&data_struct.fields, Fields::Unit);

    for (idx, field) in data_struct.fields.iter().enumerate() {
        let mode = match parse_field_attrs(&field.attrs) {
            Ok(m) => m,
            Err(e) => return e.to_compile_error().into(),
        };
        let cfgs = cfg_attrs(&field.attrs);
        let field_ty = &field.ty;
        let guard_ident = format_ident!("__moose_heap_init_guard_{idx}");

        let member = if let Some(ident) = &field.ident {
            completeness_fields.push(quote! {
                #(#cfgs)*
                #ident: ::core::unreachable!()
            });
            quote! { #ident }
        } else {
            let index = syn::Index::from(idx);
            completeness_fields.push(quote! {
                #(#cfgs)*
                ::core::unreachable!()
            });
            quote! { #index }
        };

        let init_expr = match mode {
            FieldInitMode::With(expr) => {
                quote! {
                    ::core::ptr::write(__field_ptr, #expr);
                }
            }
            FieldInitMode::Default => {
                if cfgs.is_empty() {
                    collect_type_params(
                        field_ty,
                        &declared_type_params,
                        false,
                        &mut default_bound_params,
                    );
                }
                quote! {
                    ::core::ptr::write(__field_ptr, <#field_ty as ::core::default::Default>::default());
                }
            }
            FieldInitMode::SomeBox => {
                let Some(inner_ty) = extract_option_box_inner(field_ty) else {
                    return syn::Error::new_spanned(
                        field_ty,
                        "#[heap_init(some)] requires a field of type `Option<Box<T>>`",
                    )
                    .to_compile_error()
                    .into();
                };
                if cfgs.is_empty() {
                    collect_type_params(
                        inner_ty,
                        &declared_type_params,
                        true,
                        &mut heap_init_bound_params,
                    );
                }
                quote! {
                    ::core::ptr::write(
                        __field_ptr,
                        ::core::option::Option::Some(
                            <#inner_ty as #crate_path::HeapInit>::heap_init(),
                        ),
                    );
                }
            }
            FieldInitMode::Auto => {
                if cfgs.is_empty() {
                    collect_type_params(
                        field_ty,
                        &declared_type_params,
                        true,
                        &mut heap_init_bound_params,
                    );
                }
                quote! {
                    {
                        #[allow(unused_imports)]
                        use #crate_path::__private::{Tag, ViaDefault as _, ViaHeapInit as _};
                        let __tag = &Tag::<#field_ty>::NEW;
                        __tag.__moose_heap_init(__field_ptr);
                    }
                }
            }
        };

        init_stmts.push(quote! {
            #(#cfgs)*
            let #guard_ident = {
                let __field_ptr = ::core::ptr::addr_of_mut!((*__dst).#member);
                #init_expr
                #crate_path::__private::FieldDropGuard::new(__field_ptr)
            };
        });
        disarm_stmts.push(quote! {
            #(#cfgs)*
            ::core::mem::forget(#guard_ident);
        });
    }

    let completeness_check = if is_unit {
        quote! {
            #[allow(unreachable_code, clippy::diverging_sub_expression, unused_variables)]
            if false {
                let _: Self = Self;
            }
        }
    } else if is_named {
        quote! {
            #[allow(unreachable_code, clippy::diverging_sub_expression, unused_variables)]
            if false {
                let _: Self = Self {
                    #(#completeness_fields,)*
                };
            }
        }
    } else {
        quote! {
            #[allow(unreachable_code, clippy::diverging_sub_expression, unused_variables)]
            if false {
                let _: Self = Self(
                    #(#completeness_fields,)*
                );
            }
        }
    };

    let mut generics = ast.generics.clone();
    {
        let where_clause = generics.make_where_clause();
        for param in heap_init_bound_params {
            let pred: WherePredicate = parse_quote!(#param: #crate_path::HeapInit);
            where_clause.predicates.push(pred);
        }
        for param in default_bound_params {
            let pred: WherePredicate = parse_quote!(#param: ::core::default::Default);
            where_clause.predicates.push(pred);
        }
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let expanded = quote! {
        #[allow(
            unsafe_code,
            clippy::undocumented_unsafe_blocks,
            clippy::large_stack_frames,
            clippy::large_stack_arrays,
            clippy::ptr_as_ptr
        )]
        impl #impl_generics #crate_path::HeapInit for #name #ty_generics #where_clause {
            unsafe fn heap_init_in_place(__dst: *mut Self) {
                #completeness_check
                #[allow(unused_unsafe)]
                unsafe {
                    #(#init_stmts)*
                    #(#disarm_stmts)*
                }
            }
        }
    };

    expanded.into()
}
