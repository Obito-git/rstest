pub mod crate_resolver;
pub(crate) mod fixture;
mod test;
mod wrapper;

use std::collections::HashMap;

use syn::token::Async;

use proc_macro2::{Span, TokenStream};
use syn::{parse_quote, Attribute, Expr, FnArg, Ident, ItemFn, Pat, Path, ReturnType, Stmt};

use quote::{format_ident, quote};

use crate::refident::MaybePat;
use crate::utils::{attr_ends_with, sanitize_ident};
use crate::{
    parse::{
        rstest::{RsTestAttributes, RsTestInfo},
        testcase::TestCase,
        vlist::ValueList,
    },
    utils::attr_is,
};
use crate::{
    refident::MaybeIdent,
    resolver::{self, Resolver},
};
use wrapper::WrapByModule;

pub(crate) use fixture::render as fixture;

use self::apply_arguments::ApplyArguments;
use self::crate_resolver::crate_name;
pub(crate) mod apply_arguments;
pub(crate) mod inject;

pub(crate) fn single(mut test: ItemFn, mut info: RsTestInfo) -> TokenStream {
    test.apply_arguments(&mut info.arguments, &mut ());

    let resolver = resolver::fixtures::get(&info.arguments, info.data.fixtures());

    let args = test.sig.inputs.iter().cloned().collect::<Vec<_>>();
    let attrs = std::mem::take(&mut test.attrs);
    let asyncness = test.sig.asyncness;

    single_test_case(
        &test.sig.ident,
        &test.sig.ident,
        &args,
        &attrs,
        &test.sig.output,
        asyncness,
        Some(&test),
        resolver,
        &info,
        &test.sig.generics,
        &None,
    )
}

pub(crate) fn parametrize(mut test: ItemFn, info: RsTestInfo) -> TokenStream {
    let mut arguments_info = info.arguments.clone();
    test.apply_arguments(&mut arguments_info, &mut ());

    let resolver_fixtures = resolver::fixtures::get(&info.arguments, info.data.fixtures());

    let rendered_cases = cases_data(&info, test.sig.ident.span())
        .map(|c| {
            CaseDataValues::new(
                c.ident,
                c.attributes,
                Box::new((c.resolver, &resolver_fixtures)),
                c.info,
            )
        })
        .map(|case| case.render(&test, &info))
        .collect();

    test_group(test, rendered_cases)
}

type ArgumentDataResolver<'a> = Box<(&'a dyn Resolver, (Pat, Expr))>;

impl ValueList {
    fn render(
        &self,
        test: &ItemFn,
        resolver: &dyn Resolver,
        attrs: &[syn::Attribute],
        info: &RsTestInfo,
        case_info: &Option<CaseInfo>,
    ) -> TokenStream {
        let span = test.sig.ident.span();
        let test_cases = self
            .argument_data(resolver, info)
            .map(|(name, r)| {
                CaseDataValues::new(Ident::new(&name, span), attrs, r, case_info.clone())
            })
            .map(|test_case| test_case.render(test, info));

        quote! { #(#test_cases)* }
    }

    fn argument_data<'a>(
        &'a self,
        resolver: &'a dyn Resolver,
        info: &'a RsTestInfo,
    ) -> impl Iterator<Item = (String, ArgumentDataResolver<'a>)> + 'a {
        let max_len = self.values.len();
        self.values.iter().enumerate().map(move |(index, value)| {
            let description = sanitize_ident(&value.description());
            let arg = info.arguments.inner_pat(&self.arg);

            let arg_name = arg
                .maybe_ident()
                .expect("BUG: Here all arguments should be PatIdent types")
                .to_string();

            let name = format!(
                "{}_{:0len$}_{description:.64}",
                arg_name,
                index + 1,
                len = max_len.display_len()
            );
            let resolver_this = (arg.clone(), value.expr.clone());
            (name, Box::new((resolver, resolver_this)))
        })
    }
}

#[derive(Clone, Debug)]
struct CaseInfo {
    description: Option<Ident>,
    pos: usize,
}

impl CaseInfo {
    fn new(description: Option<Ident>, pos: usize) -> Self {
        Self { description, pos }
    }
}

fn _matrix_recursive<'a>(
    test: &ItemFn,
    list_values: &'a [&'a ValueList],
    resolver: &dyn Resolver,
    attrs: &'a [syn::Attribute],
    info: &RsTestInfo,
    case_info: &Option<CaseInfo>,
) -> TokenStream {
    if list_values.is_empty() {
        return Default::default();
    }
    let vlist = list_values[0];
    let list_values = &list_values[1..];

    if list_values.is_empty() {
        let mut attrs = attrs.to_vec();
        attrs.push(parse_quote!(
            #[allow(non_snake_case)]
        ));
        vlist.render(test, resolver, &attrs, info, case_info)
    } else {
        let span = test.sig.ident.span();
        let modules = vlist
            .argument_data(resolver, info)
            .map(move |(name, resolver)| {
                _matrix_recursive(test, list_values, &resolver, attrs, info, case_info)
                    .wrap_by_mod(&Ident::new(&name, span))
            });

        quote! { #(
            #[allow(non_snake_case)]
            #modules
        )* }
    }
}

pub(crate) fn matrix(mut test: ItemFn, mut info: RsTestInfo) -> TokenStream {
    test.apply_arguments(&mut info.arguments, &mut ());
    let span = test.sig.ident.span();

    let cases = cases_data(&info, span).collect::<Vec<_>>();

    let resolver = resolver::fixtures::get(&info.arguments, info.data.fixtures());
    let rendered_cases = if cases.is_empty() {
        let list_values = info.data.list_values().collect::<Vec<_>>();
        _matrix_recursive(&test, &list_values, &resolver, &[], &info, &None)
    } else {
        cases
            .into_iter()
            .map(|c| {
                let list_values = info.data.list_values().collect::<Vec<_>>();
                _matrix_recursive(
                    &test,
                    &list_values,
                    &(&c.resolver, &resolver),
                    c.attributes,
                    &info,
                    &c.info,
                )
                .wrap_by_mod(&c.ident)
            })
            .collect()
    };

    test_group(test, rendered_cases)
}

fn resolve_test_attr(
    is_async: bool,
    explicit_test_attr: Option<TokenStream>,
    attributes: &[Attribute],
) -> Option<TokenStream> {
    if let Some(explicit_attr) = explicit_test_attr {
        Some(explicit_attr)
    } else if attributes
        .iter()
        .any(|attr| attr_ends_with(attr, &parse_quote! {test}))
    {
        // test attr is already in the attributes; we don't need to re-inject it
        None
    } else if !is_async {
        Some(quote! { #[test] })
    } else {
        Some(
            quote! { compile_error!{"async tests require either an explicit `test_attr` or an attribute whose path ends with `test`"} },
        )
    }
}

fn render_exec_call(fn_path: Path, args: &[Expr], is_async: bool) -> TokenStream {
    if is_async {
        quote! {#fn_path(#(#args),*).await}
    } else {
        quote! {#fn_path(#(#args),*)}
    }
}

fn render_test_call(
    fn_path: Path,
    args: &[Expr],
    timeout: Option<Expr>,
    is_async: bool,
) -> TokenStream {
    let timeout = timeout.map(|x| quote! {#x}).or_else(|| {
        std::env::var("RSTEST_TIMEOUT")
            .ok()
            .map(|to| quote! { core::time::Duration::from_secs( (#to).parse().unwrap()) })
    });
    let rstest_path = crate_name();
    match (timeout, is_async) {
        (Some(to_expr), true) => quote! {
            use #rstest_path::timeout::*;
            execute_with_timeout_async(move || #fn_path(#(#args),*), #to_expr).await
        },
        (Some(to_expr), false) => quote! {
            use #rstest_path::timeout::*;
            execute_with_timeout_sync(move || #fn_path(#(#args),*), #to_expr)
        },
        _ => render_exec_call(fn_path, args, is_async),
    }
}

fn generics_types_ident(generics: &syn::Generics) -> impl Iterator<Item = &'_ Ident> {
    generics.type_params().map(|tp| &tp.ident)
}

/// Render a single test case:
///
/// * `name` - Test case name
/// * `testfn_name` - The name of test function to call
/// * `args` - The arguments of the test function
/// * `attrs` - The expected test attributes
/// * `output` - The expected test return type
/// * `asyncness` - The `async` fn token
/// * `test_impl` - If you want embed test function (should be the one called by `testfn_name`)
/// * `resolver` - The resolver used to resolve injected values
/// * `info` - `RsTestInfo` that's expose the requested test behavior
/// * `generic_types` - The generic types used in signature
///
// Ok I need some refactoring here but now that not a real issue
#[allow(clippy::too_many_arguments)]
fn single_test_case(
    name: &Ident,
    testfn_name: &Ident,
    args: &[FnArg],
    attrs: &[Attribute],
    output: &ReturnType,
    asyncness: Option<Async>,
    test_impl: Option<&ItemFn>,
    resolver: impl Resolver,
    info: &RsTestInfo,
    generics: &syn::Generics,
    case_info: &Option<CaseInfo>,
) -> TokenStream {
    let (attrs, trace_me): (Vec<_>, Vec<_>) =
        attrs.iter().cloned().partition(|a| !attr_is(a, "trace"));
    let mut attributes = info.attributes.clone();
    if !trace_me.is_empty() {
        attributes.add_trace(format_ident!("trace"));
    }

    let generics_types = generics_types_ident(generics).cloned().collect::<Vec<_>>();
    let args = info
        .arguments
        .replace_fn_args_with_related_inner_pat(args.iter().cloned())
        .collect::<Vec<_>>();

    let (injectable_args, ignored_args): (Vec<_>, Vec<_>) =
        args.iter().partition(|arg| match arg.maybe_pat() {
            Some(pat) => !info.arguments.is_ignore(pat),
            None => true,
        });
    let test_fn_name_str = testfn_name.to_string();
    let description = match case_info
        .as_ref()
        .and_then(|c| c.description.as_ref())
        .map(|d| d.to_string())
    {
        Some(s) => quote! { Some(#s) },
        None => quote! { None },
    };
    let pos = match case_info.as_ref().map(|c| c.pos) {
        Some(p) => quote! { Some(#p) },
        None => quote! { None },
    };
    let context_resolver = info
        .arguments
        .contexts()
        .map(|p| {
            (p.clone(), {
                let e: Expr = parse_quote! {
                    Context::new(module_path!(), #test_fn_name_str, #description, #pos)
                };
                e
            })
        })
        .collect::<HashMap<_, _>>();

    let inject = inject::resolve_arguments(
        injectable_args.into_iter(),
        &(context_resolver, &resolver),
        &generics_types,
    );

    let args = args
        .iter()
        .filter_map(MaybePat::maybe_pat)
        .cloned()
        .collect::<Vec<_>>();
    let trace_args = trace_arguments(args.iter(), &attributes);

    let is_async = asyncness.is_some();
    let (attrs, timeouts): (Vec<_>, Vec<_>) =
        attrs.iter().cloned().partition(|a| !attr_is(a, "timeout"));

    let timeout = timeouts
        .into_iter()
        .last()
        .map(|attribute| attribute.parse_args::<Expr>().unwrap());

    let explicit_test_attr = attrs.iter().find_map(|attr| {
        if !attr_is(attr, "test_attr") {
            return None;
        }
        match &attr.meta {
            syn::Meta::List(meta_list) => {
                let tokens = &meta_list.tokens;
                Some(quote! { #[#tokens] })
            },
            syn::Meta::Path(_) | syn::Meta::NameValue(_) => Some( quote! { compile_error!{"invalid `test_attr` syntax; should be `#[test_attr(<test attribute>)]`"}}),
        }
    });
    let test_attr = resolve_test_attr(is_async, explicit_test_attr, &attrs);

    let args = args
        .iter()
        .map(|arg| (arg, info.arguments.is_by_refs(arg)))
        .filter_map(|(a, by_refs)| a.maybe_ident().map(|id| (id, by_refs)))
        .map(|(arg, by_ref)| {
            if by_ref {
                parse_quote! { &#arg }
            } else {
                parse_quote! { #arg }
            }
        })
        .collect::<Vec<_>>();

    let execute = render_test_call(testfn_name.clone().into(), &args, timeout, is_async);
    let lifetimes = generics.lifetimes();

    quote! {
        #(#attrs)*
        #test_attr
        #asyncness fn #name<#(#lifetimes,)*>(#(#ignored_args,)*) #output {
            #test_impl
            #inject
            #trace_args
            #execute
        }
    }
}

fn trace_arguments<'a>(
    args: impl Iterator<Item = &'a Pat>,
    attributes: &RsTestAttributes,
) -> Option<TokenStream> {
    let mut statements = args
        .filter(|&arg| attributes.trace_me(arg))
        .map(|arg| {
            let s: Stmt = parse_quote! {
                println!("{} = {:?}", stringify!(#arg), #arg);
            };
            s
        })
        .peekable();
    if statements.peek().is_some() {
        Some(quote! {
            println!("{:-^40}", " TEST ARGUMENTS ");
            #(#statements)*
            println!("{:-^40}", " TEST START ");
        })
    } else {
        None
    }
}

impl CaseDataValues<'_> {
    fn render(self, testfn: &ItemFn, info: &RsTestInfo) -> TokenStream {
        let args = testfn.sig.inputs.iter().cloned().collect::<Vec<_>>();
        let mut attrs = testfn.attrs.clone();
        attrs.extend(self.attributes.iter().cloned());
        let asyncness = testfn.sig.asyncness;

        single_test_case(
            &self.ident,
            &testfn.sig.ident,
            &args,
            &attrs,
            &testfn.sig.output,
            asyncness,
            None,
            self.resolver,
            info,
            &testfn.sig.generics,
            &self.info,
        )
    }
}

fn test_group(mut test: ItemFn, rendered_cases: TokenStream) -> TokenStream {
    let fname = &test.sig.ident;
    test.attrs = vec![];

    quote! {
        #[cfg(test)]
        #test

        #[cfg(test)]
        mod #fname {
            use super::*;

            #rendered_cases
        }
    }
}

trait DisplayLen {
    fn display_len(&self) -> usize;
}

impl<D: std::fmt::Display> DisplayLen for D {
    fn display_len(&self) -> usize {
        format!("{self}").len()
    }
}

fn format_case_name(case: &TestCase, index: usize, display_len: usize) -> String {
    let description = case
        .description
        .as_ref()
        .map(|d| format!("_{d}"))
        .unwrap_or_default();
    format!("case_{index:0display_len$}{description}")
}

struct CaseDataValues<'a> {
    ident: Ident,
    attributes: &'a [syn::Attribute],
    resolver: Box<dyn Resolver + 'a>,
    info: Option<CaseInfo>,
}

impl<'a> CaseDataValues<'a> {
    fn new(
        ident: Ident,
        attributes: &'a [syn::Attribute],
        resolver: Box<dyn Resolver + 'a>,
        info: Option<CaseInfo>,
    ) -> Self {
        Self {
            ident,
            attributes,
            resolver,
            info,
        }
    }
}

fn cases_data(info: &RsTestInfo, name_span: Span) -> impl Iterator<Item = CaseDataValues<'_>> {
    let display_len = info.data.cases().count().display_len();
    info.data.cases().enumerate().map({
        move |(n, case)| {
            let resolver_case = info
                .data
                .case_args()
                .cloned()
                .map(|arg| info.arguments.inner_pat(&arg).clone())
                .zip(case.args.iter())
                .collect::<HashMap<_, _>>();
            CaseDataValues::new(
                Ident::new(&format_case_name(case, n + 1, display_len), name_span),
                case.attrs.as_slice(),
                Box::new(resolver_case),
                Some(CaseInfo::new(case.description.clone(), n)),
            )
        }
    })
}
