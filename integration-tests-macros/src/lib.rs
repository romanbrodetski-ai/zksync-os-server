use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Attribute, FnArg, ItemFn, PatType, Path, Result, Token, Type, TypePath, parse_macro_input,
};

struct CaseList {
    cases: Punctuated<Path, Token![,]>,
}

impl Parse for CaseList {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let content;
        syn::bracketed!(content in input);
        Ok(Self {
            cases: content.parse_terminated(Path::parse, Token![,])?,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParamKind {
    TestCase,
    Tester,
    TesterBuilder,
}

fn param_kind(arg: &FnArg) -> Result<Option<ParamKind>> {
    let FnArg::Typed(PatType { ty, .. }) = arg else {
        return Err(syn::Error::new_spanned(
            arg,
            "methods with `self` are not supported",
        ));
    };
    Ok(type_kind(ty))
}

fn type_kind(ty: &Type) -> Option<ParamKind> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let ident = path.segments.last()?.ident.to_string();
    match ident.as_str() {
        "TestCase" => Some(ParamKind::TestCase),
        "Tester" => Some(ParamKind::Tester),
        "TesterBuilder" => Some(ParamKind::TesterBuilder),
        _ => None,
    }
}

fn split_helper_attrs(attrs: Vec<Attribute>) -> Result<(Vec<Attribute>, Option<syn::Expr>)> {
    let mut output = Vec::with_capacity(attrs.len());
    let mut builder_expr = None;

    for attr in attrs {
        if attr.path().is_ident("test_builder") {
            if builder_expr.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "duplicate `test_builder` attribute",
                ));
            }
            builder_expr = Some(attr.parse_args()?);
        } else {
            output.push(attr);
        }
    }

    Ok((output, builder_expr))
}

fn case_fn_name(case: &Path) -> Result<syn::Ident> {
    let case_name = case
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(case, "expected a case path"))?
        .ident
        .to_string()
        .to_lowercase();
    Ok(format_ident!("{case_name}"))
}

#[proc_macro_attribute]
pub fn test_casing(attr: TokenStream, item: TokenStream) -> TokenStream {
    let cases = parse_macro_input!(attr as CaseList);
    let mut input = parse_macro_input!(item as ItemFn);

    if input.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input.sig.fn_token, "test function must be async")
            .into_compile_error()
            .into();
    }

    let (wrapper_attrs, builder_expr) = match split_helper_attrs(input.attrs) {
        Ok(attrs) => attrs,
        Err(err) => return err.into_compile_error().into(),
    };
    input.attrs = Vec::new();

    let impl_name = format_ident!("{}_impl", input.sig.ident);
    let mod_name = input.sig.ident.clone();
    input.sig.ident = impl_name.clone();

    let mut needs_builder = false;
    let mut needs_tester = false;
    let mut arg_exprs = Vec::with_capacity(input.sig.inputs.len());

    for arg in &input.sig.inputs {
        match param_kind(arg) {
            Ok(Some(ParamKind::TestCase)) => {
                arg_exprs.push(quote!(case));
            }
            Ok(Some(ParamKind::TesterBuilder)) => {
                needs_builder = true;
                arg_exprs.push(quote!(builder.clone()));
            }
            Ok(Some(ParamKind::Tester)) => {
                needs_tester = true;
                arg_exprs.push(quote!(tester));
            }
            Ok(None) => {
                return syn::Error::new_spanned(
                    arg,
                    "supported parameters are `Tester`, `TesterBuilder`, and `TestCase`",
                )
                .into_compile_error()
                .into();
            }
            Err(err) => return err.into_compile_error().into(),
        }
    }

    let builder_setup = if needs_builder || needs_tester {
        if let Some(builder_expr) = builder_expr {
            quote! {
                let builder = {
                    let configure: fn(
                        ::zksync_os_integration_tests::TesterBuilder,
                    ) -> ::zksync_os_integration_tests::TesterBuilder = #builder_expr;
                    let builder: ::zksync_os_integration_tests::TesterBuilder = case.builder();
                    configure(builder)
                };
            }
        } else {
            quote! {
                let builder: ::zksync_os_integration_tests::TesterBuilder = case.builder();
            }
        }
    } else {
        quote! {}
    };
    let tester_setup = if needs_tester {
        quote! {
            let tester = builder.build().await?;
        }
    } else {
        quote! {}
    };

    let wrappers = cases.cases.iter().map(|case| {
        let fn_name = match case_fn_name(case) {
            Ok(name) => name,
            Err(err) => return err.into_compile_error(),
        };
        quote! {
            #(#wrapper_attrs)*
            async fn #fn_name() -> anyhow::Result<()> {
                let case = #case;
                #builder_setup
                #tester_setup
                #impl_name(#(#arg_exprs),*).await
            }
        }
    });

    quote! {
        mod #mod_name {
            use super::*;

            #input

            #(#wrappers)*
        }
    }
    .into()
}
