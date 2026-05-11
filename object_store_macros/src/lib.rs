// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Internal proc-macro derives for `object_store`.
//!
//! Emits paths rooted at `crate::config::*` etc., so this derive is only
//! valid when used inside the `object_store` crate itself.

extern crate proc_macro;

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, spanned::Spanned, Attribute, Data, DeriveInput, Expr, ExprArray, ExprLit,
    Field, Fields, GenericArgument, Ident, Lit, LitStr, Path, PathArguments, Type,
};

// ---------------------------------------------------------------------------
// Attribute model
// ---------------------------------------------------------------------------

struct StructAttrs {
    config_key: Ident,
    error_path: Path,
    error_store: Option<LitStr>,
}

struct FieldDef {
    ty: Type,
    field_ident: Ident,
    attrs: FieldAttrs,
}

struct FieldAttrs {
    key: String,
    key_span: Span,
    aliases: Vec<String>,
    strategy: Strategy,
    formatter: Formatter,
    get_via: Option<Ident>,
    variant: Ident,
    setter: SetterMode,
    setter_name: Ident,
    doc_inherit: bool,
    cfg_attrs: Vec<Attribute>,
    docs: Vec<Attribute>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Strategy {
    OptionString,
    ConfigValue,
    OptionConfigValueDeferred,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Formatter {
    ToString,
    Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetterMode {
    Auto,
    Skip,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[proc_macro_derive(ObjectStoreConfig, attributes(object_store, config))]
pub fn derive_object_store_config(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    let struct_attrs = parse_struct_attrs(&input)?;
    let struct_name = &input.ident;

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    &input,
                    "ObjectStoreConfig requires a struct with named fields",
                ))
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                &input,
                "ObjectStoreConfig can only be derived on structs",
            ))
        }
    };

    let mut field_defs: Vec<FieldDef> = Vec::new();
    for f in fields {
        if let Some(attrs) = parse_field_attrs(f)? {
            let ident = f
                .ident
                .clone()
                .ok_or_else(|| syn::Error::new(f.span(), "field must be named"))?;
            field_defs.push(FieldDef {
                ty: f.ty.clone(),
                field_ident: ident,
                attrs,
            });
        }
    }

    if field_defs.is_empty() {
        return Err(syn::Error::new_spanned(
            &input,
            "ObjectStoreConfig requires at least one field with #[config(...)]",
        ));
    }

    check_duplicate_keys(&field_defs)?;

    let enum_ts = generate_enum(struct_name, &struct_attrs, &field_defs);
    let asref_ts = generate_asref(&struct_attrs, &field_defs);
    let fromstr_ts = generate_fromstr(&struct_attrs, &field_defs);
    let dispatch_ts = generate_dispatch(struct_name, &struct_attrs, &field_defs);
    let setters_ts = generate_setters(struct_name, &field_defs)?;

    Ok(quote! {
        #enum_ts
        #asref_ts
        #fromstr_ts
        #dispatch_ts
        #setters_ts
    })
}

// ---------------------------------------------------------------------------
// Parse #[object_store(...)] on the struct
// ---------------------------------------------------------------------------

fn parse_struct_attrs(input: &DeriveInput) -> syn::Result<StructAttrs> {
    let mut config_key: Option<Ident> = None;
    let mut error_path: Option<Path> = None;
    let mut error_store: Option<LitStr> = None;

    for attr in &input.attrs {
        if !attr.path().is_ident("object_store") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("config_key") {
                config_key = Some(meta.value()?.parse::<Ident>()?);
            } else if meta.path.is_ident("error_path") {
                error_path = Some(meta.value()?.parse::<Path>()?);
            } else if meta.path.is_ident("error_store") {
                let value: LitStr = meta.value()?.parse()?;
                error_store = Some(value);
            } else {
                return Err(meta.error("unknown attribute on #[object_store(...)]"));
            }
            Ok(())
        })?;
    }

    let config_key = config_key.ok_or_else(|| {
        syn::Error::new_spanned(
            input,
            "missing required attribute `#[object_store(config_key = ...)]`",
        )
    })?;
    let error_path = error_path.ok_or_else(|| {
        syn::Error::new_spanned(
            input,
            "missing required attribute `#[object_store(error_path = ...)]`",
        )
    })?;

    Ok(StructAttrs {
        config_key,
        error_path,
        error_store,
    })
}

// ---------------------------------------------------------------------------
// Parse #[config(...)] on a field
// ---------------------------------------------------------------------------

fn parse_field_attrs(field: &Field) -> syn::Result<Option<FieldAttrs>> {
    let field_ident = field
        .ident
        .clone()
        .ok_or_else(|| syn::Error::new(field.span(), "field must be named"))?;

    let mut config_attr: Option<&Attribute> = None;
    for attr in &field.attrs {
        if attr.path().is_ident("config") {
            if config_attr.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "duplicate #[config(...)] attribute on field",
                ));
            }
            config_attr = Some(attr);
        }
    }

    let mut key: Option<LitStr> = None;
    let mut aliases: Vec<String> = Vec::new();
    let mut strategy: Option<(Strategy, Span)> = None;
    let mut formatter: Option<Formatter> = None;
    let mut get_via: Option<Ident> = None;
    let mut variant: Option<Ident> = None;
    let mut setter: SetterMode = SetterMode::Auto;
    let mut setter_name: Option<Ident> = None;
    let mut doc_inherit = true;
    let mut skip = false;

    if let Some(attr) = config_attr {
        if matches!(attr.meta, syn::Meta::List(_)) {
            attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("skip") {
            skip = true;
        } else if meta.path.is_ident("key") {
            let value: LitStr = meta.value()?.parse()?;
            if value.value().is_empty() {
                return Err(syn::Error::new(value.span(), "config key must be non-empty"));
            }
            key = Some(value);
        } else if meta.path.is_ident("aliases") {
            let arr: ExprArray = meta.value()?.parse()?;
            for elem in arr.elems.iter() {
                if let Expr::Lit(ExprLit {
                    lit: Lit::Str(s), ..
                }) = elem
                {
                    aliases.push(s.value());
                } else {
                    return Err(syn::Error::new_spanned(
                        elem,
                        "aliases must be a list of string literals",
                    ));
                }
            }
        } else if meta.path.is_ident("strategy") {
            let ident: Ident = meta.value()?.parse()?;
            let s = match ident.to_string().as_str() {
                "option_string" => Strategy::OptionString,
                "config_value" => Strategy::ConfigValue,
                "option_config_value_deferred" => Strategy::OptionConfigValueDeferred,
                _ => {
                    return Err(syn::Error::new(
                        ident.span(),
                        format!(
                            "unknown strategy `{ident}`; expected one of `option_string`, `config_value`, `option_config_value_deferred`"
                        ),
                    ));
                }
            };
            strategy = Some((s, ident.span()));
        } else if meta.path.is_ident("get_via") {
            get_via = Some(meta.value()?.parse::<Ident>()?);
        } else if meta.path.is_ident("formatter") {
            let ident: Ident = meta.value()?.parse()?;
            formatter = Some(match ident.to_string().as_str() {
                "to_string" => Formatter::ToString,
                "duration" => Formatter::Duration,
                _ => {
                    return Err(syn::Error::new(
                        ident.span(),
                        format!("unknown formatter `{ident}`; expected `to_string` or `duration`"),
                    ));
                }
            });
        } else if meta.path.is_ident("variant") {
            variant = Some(meta.value()?.parse::<Ident>()?);
        } else if meta.path.is_ident("setter") {
            let ident: Ident = meta.value()?.parse()?;
            setter = match ident.to_string().as_str() {
                "auto" => SetterMode::Auto,
                "skip" => SetterMode::Skip,
                _ => {
                    return Err(syn::Error::new(
                        ident.span(),
                        format!("unknown setter mode `{ident}`; expected `auto` or `skip`"),
                    ));
                }
            };
        } else if meta.path.is_ident("setter_name") {
            setter_name = Some(meta.value()?.parse::<Ident>()?);
        } else if meta.path.is_ident("doc_inherit") {
            let value: syn::LitBool = meta.value()?.parse()?;
            doc_inherit = value.value;
        } else {
            return Err(meta.error("unknown attribute on #[config(...)]"));
        }
        Ok(())
            })?;
        }
    }

    if skip {
        return Ok(None);
    }

    let (key_value, key_span) = match key {
        Some(lit) => (lit.value(), lit.span()),
        None => (field_ident.to_string(), field_ident.span()),
    };
    let strategy_span_for_validation = strategy.map(|(_, s)| s);
    let strategy = match strategy {
        Some((s, _)) => s,
        None => match infer_strategy(&field.ty) {
            Some(s) => s,
            None => {
                // No #[config] attribute *and* type isn't inferable → silently skip.
                // If `#[config(...)]` is present, the user opted in and we owe a clear error.
                if config_attr.is_some() {
                    return Err(syn::Error::new_spanned(
                        &field.ty,
                        "could not infer strategy from field type; expected `Option<String>`, \
                         `ConfigValue<T>`, or `Option<ConfigValue<T>>`. Specify explicitly with \
                         `#[config(strategy = ...)]`",
                    ));
                }
                return Ok(None);
            }
        },
    };

    // Validate formatter is only used with option_config_value_deferred
    if formatter.is_some() && strategy != Strategy::OptionConfigValueDeferred {
        return Err(syn::Error::new(
            strategy_span_for_validation.unwrap_or_else(|| field.ty.span()),
            "`formatter` is only valid with `strategy = option_config_value_deferred`",
        ));
    }
    let formatter = formatter.unwrap_or_else(|| infer_formatter(strategy, &field.ty));

    let variant = variant.unwrap_or_else(|| snake_to_pascal(&field_ident));
    let setter_name = setter_name.unwrap_or_else(|| format_ident!("with_{}", field_ident));

    let mut cfg_attrs = Vec::new();
    let mut docs = Vec::new();
    for a in &field.attrs {
        if a.path().is_ident("cfg") {
            cfg_attrs.push(a.clone());
        } else if a.path().is_ident("doc") {
            docs.push(a.clone());
        }
    }

    Ok(Some(FieldAttrs {
        key: key_value,
        key_span,
        aliases,
        strategy,
        formatter,
        get_via,
        variant,
        setter,
        setter_name,
        doc_inherit,
        cfg_attrs,
        docs,
    }))
}

fn snake_to_pascal(ident: &Ident) -> Ident {
    let s = ident.to_string();
    let mut out = String::with_capacity(s.len());
    let mut next_upper = true;
    for c in s.chars() {
        if c == '_' {
            next_upper = true;
        } else if next_upper {
            out.push(c.to_ascii_uppercase());
            next_upper = false;
        } else {
            out.push(c);
        }
    }
    Ident::new(&out, ident.span())
}

fn check_duplicate_keys(fields: &[FieldDef]) -> syn::Result<()> {
    use std::collections::HashMap;
    let mut seen: HashMap<&str, Span> = HashMap::new();
    for f in fields {
        for k in std::iter::once(f.attrs.key.as_str())
            .chain(f.attrs.aliases.iter().map(String::as_str))
        {
            if let Some(&prev) = seen.get(k) {
                let mut err = syn::Error::new(
                    f.attrs.key_span,
                    format!("duplicate config key `{}`", k),
                );
                err.combine(syn::Error::new(prev, "previously defined here"));
                return Err(err);
            }
            seen.insert(k, f.attrs.key_span);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------

fn build_supported_keys_doc(attrs: &FieldAttrs) -> Vec<Attribute> {
    if !attrs.doc_inherit {
        return Vec::new();
    }
    let header = [String::new(), "Supported keys:".into()].into_iter();
    let bullets = std::iter::once(&attrs.key)
        .chain(attrs.aliases.iter())
        .map(|k| format!("- `{}`", k));
    header
        .chain(bullets)
        .map(|line| {
            let lit = LitStr::new(format!(" {}", line).trim_end(), Span::call_site());
            syn::parse_quote!(#[doc = #lit])
        })
        .collect()
}

fn generate_enum(
    struct_name: &Ident,
    struct_attrs: &StructAttrs,
    fields: &[FieldDef],
) -> TokenStream2 {
    let enum_name = &struct_attrs.config_key;
    let enum_doc = LitStr::new(
        &format!(" Configuration keys for [`{}`]", struct_name),
        Span::call_site(),
    );

    let variants = fields.iter().map(|f| {
        let cfgs = &f.attrs.cfg_attrs;
        let docs = &f.attrs.docs;
        let extra_docs = build_supported_keys_doc(&f.attrs);
        let variant = &f.attrs.variant;
        quote! {
            #(#cfgs)*
            #(#docs)*
            #(#extra_docs)*
            #variant
        }
    });

    quote! {
        #[doc = #enum_doc]
        #[derive(::std::cmp::PartialEq, ::std::cmp::Eq, ::std::hash::Hash, ::std::clone::Clone, ::std::fmt::Debug, ::std::marker::Copy, ::serde::Deserialize, ::serde::Serialize)]
        #[non_exhaustive]
        pub enum #enum_name {
            #(#variants,)*
        }
    }
}

fn generate_asref(struct_attrs: &StructAttrs, fields: &[FieldDef]) -> TokenStream2 {
    let enum_name = &struct_attrs.config_key;
    let arms = fields.iter().map(|f| {
        let cfgs = &f.attrs.cfg_attrs;
        let variant = &f.attrs.variant;
        let key = &f.attrs.key;
        quote! {
            #(#cfgs)*
            Self::#variant => #key,
        }
    });
    quote! {
        impl ::std::convert::AsRef<str> for #enum_name {
            fn as_ref(&self) -> &str {
                match self {
                    #(#arms)*
                }
            }
        }
    }
}

fn generate_fromstr(struct_attrs: &StructAttrs, fields: &[FieldDef]) -> TokenStream2 {
    let enum_name = &struct_attrs.config_key;
    let error_path = &struct_attrs.error_path;
    let arms = fields.iter().map(|f| {
        let cfgs = &f.attrs.cfg_attrs;
        let variant = &f.attrs.variant;
        let patterns = std::iter::once(&f.attrs.key)
            .chain(f.attrs.aliases.iter())
            .map(|k| quote! { #k });
        quote! {
            #(#cfgs)*
            #(#patterns)|* => ::std::result::Result::Ok(Self::#variant),
        }
    });

    let err_expr = if let Some(store) = &struct_attrs.error_store {
        quote! { #error_path { store: #store, key: s.into() } }
    } else {
        quote! { #error_path { key: s.into() } }
    };

    quote! {
        impl ::std::str::FromStr for #enum_name {
            type Err = crate::Error;
            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                match s {
                    #(#arms)*
                    _ => ::std::result::Result::Err(#err_expr.into()),
                }
            }
        }
    }
}

fn generate_dispatch(
    struct_name: &Ident,
    struct_attrs: &StructAttrs,
    fields: &[FieldDef],
) -> TokenStream2 {
    let enum_name = &struct_attrs.config_key;

    let set_arms = fields.iter().map(|f| {
        let cfgs = &f.attrs.cfg_attrs;
        let variant = &f.attrs.variant;
        let field_ident = &f.field_ident;
        let body = match f.attrs.strategy {
            Strategy::OptionString => quote! {
                self.#field_ident = ::std::option::Option::Some(value.into());
            },
            Strategy::ConfigValue => quote! {
                self.#field_ident.parse(value);
            },
            Strategy::OptionConfigValueDeferred => quote! {
                self.#field_ident = ::std::option::Option::Some(
                    crate::config::ConfigValue::Deferred(value.into())
                );
            },
        };
        quote! {
            #(#cfgs)*
            #enum_name::#variant => { #body }
        }
    });

    let get_arms = fields.iter().map(|f| {
        let cfgs = &f.attrs.cfg_attrs;
        let variant = &f.attrs.variant;
        let field_ident = &f.field_ident;
        let body = if let Some(getter) = &f.attrs.get_via {
            quote! { self.#getter() }
        } else {
            match f.attrs.strategy {
                Strategy::OptionString => quote! {
                    self.#field_ident.clone()
                },
                Strategy::ConfigValue => quote! {
                    ::std::option::Option::Some(self.#field_ident.to_string())
                },
                Strategy::OptionConfigValueDeferred => match f.attrs.formatter {
                    Formatter::Duration => quote! {
                        self.#field_ident.as_ref().map(crate::config::fmt_duration)
                    },
                    Formatter::ToString => quote! {
                        self.#field_ident.as_ref().map(|v| v.to_string())
                    },
                },
            }
        };
        quote! {
            #(#cfgs)*
            #enum_name::#variant => #body,
        }
    });

    quote! {
        impl #struct_name {
            /// Set a config option by key.
            pub fn with_config(mut self, key: #enum_name, value: impl ::std::convert::Into<::std::string::String>) -> Self {
                let value: ::std::string::String = value.into();
                match key {
                    #(#set_arms)*
                }
                self
            }

            /// Get a config value by key.
            pub fn get_config_value(&self, key: &#enum_name) -> ::std::option::Option<::std::string::String> {
                match key {
                    #(#get_arms)*
                }
            }
        }
    }
}

fn generate_setters(struct_name: &Ident, fields: &[FieldDef]) -> syn::Result<TokenStream2> {
    let mut out = TokenStream2::new();
    for f in fields {
        if f.attrs.setter == SetterMode::Skip {
            continue;
        }
        let cfgs = &f.attrs.cfg_attrs;
        let docs = &f.attrs.docs;
        let setter_name = &f.attrs.setter_name;
        let field_ident = &f.field_ident;
        let ty = &f.ty;

        let method = match f.attrs.strategy {
            Strategy::OptionString => quote! {
                #(#cfgs)*
                #(#docs)*
                pub fn #setter_name(mut self, value: impl ::std::convert::Into<::std::string::String>) -> Self {
                    self.#field_ident = ::std::option::Option::Some(value.into());
                    self
                }
            },
            Strategy::ConfigValue => {
                let inner = extract_config_value_inner(ty)?;
                quote! {
                    #(#cfgs)*
                    #(#docs)*
                    pub fn #setter_name(mut self, value: #inner) -> Self {
                        self.#field_ident = value.into();
                        self
                    }
                }
            }
            Strategy::OptionConfigValueDeferred => {
                let inner = extract_option_config_value_inner(ty)?;
                quote! {
                    #(#cfgs)*
                    #(#docs)*
                    pub fn #setter_name(mut self, value: #inner) -> Self {
                        self.#field_ident = ::std::option::Option::Some(value.into());
                        self
                    }
                }
            }
        };
        out.extend(method);
    }

    Ok(quote! {
        impl #struct_name {
            #out
        }
    })
}

/// Extract `T` from a token-tree-matched `ConfigValue<T>`.
fn extract_config_value_inner(ty: &Type) -> syn::Result<Type> {
    extract_single_generic_arg_as_type(ty, "ConfigValue").ok_or_else(|| {
        syn::Error::new_spanned(ty, "expected field type to be `ConfigValue<T>`")
    })
}

/// Extract `T` from a token-tree-matched `Option<ConfigValue<T>>`.
fn extract_option_config_value_inner(ty: &Type) -> syn::Result<Type> {
    let inner_opt = extract_single_generic_arg_as_type(ty, "Option").ok_or_else(|| {
        syn::Error::new_spanned(
            ty,
            "expected field type to be `Option<ConfigValue<T>>` for strategy `option_config_value_deferred`",
        )
    })?;
    extract_config_value_inner(&inner_opt)
}

/// Infer a `Strategy` from the field's type tokens.
///
/// Recognized shapes:
/// - `Option<String>`              → `OptionString`
/// - `ConfigValue<T>`              → `ConfigValue`
/// - `Option<ConfigValue<T>>`      → `OptionConfigValueDeferred`
fn infer_strategy(ty: &Type) -> Option<Strategy> {
    if let Some(inner) = extract_single_generic_arg_as_type(ty, "Option") {
        if extract_single_generic_arg_as_type(&inner, "ConfigValue").is_some() {
            return Some(Strategy::OptionConfigValueDeferred);
        }
        if last_segment_is(&inner, "String") {
            return Some(Strategy::OptionString);
        }
        return None;
    }
    if extract_single_generic_arg_as_type(ty, "ConfigValue").is_some() {
        return Some(Strategy::ConfigValue);
    }
    None
}

/// For `Option<ConfigValue<Duration>>`, pick the duration formatter; otherwise `ToString`.
fn infer_formatter(strategy: Strategy, ty: &Type) -> Formatter {
    if strategy != Strategy::OptionConfigValueDeferred {
        return Formatter::ToString;
    }
    let inner = match extract_single_generic_arg_as_type(ty, "Option")
        .and_then(|t| extract_single_generic_arg_as_type(&t, "ConfigValue"))
    {
        Some(t) => t,
        None => return Formatter::ToString,
    };
    if last_segment_is(&inner, "Duration") {
        Formatter::Duration
    } else {
        Formatter::ToString
    }
}

fn last_segment_is(ty: &Type, name: &str) -> bool {
    matches!(
        ty,
        Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == name)
    )
}

fn extract_single_generic_arg_as_type(ty: &Type, wrapper: &str) -> Option<Type> {
    let Type::Path(p) = ty else { return None };
    // Match the last path segment by name.
    let last = p.path.segments.last()?;
    if last.ident != wrapper {
        return None;
    }
    let PathArguments::AngleBracketed(ab) = &last.arguments else {
        return None;
    };
    for arg in &ab.args {
        if let GenericArgument::Type(t) = arg {
            return Some(t.clone());
        }
    }
    None
}
