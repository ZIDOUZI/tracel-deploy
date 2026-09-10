#![allow(linker_messages)]

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemEnum, Path, Token, parse_macro_input};

struct DeployMacroArgs {
    variant: syn::Ident,
    args: Option<Path>,
    handler: Option<Path>,
}

impl syn::parse::Parse for DeployMacroArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let variant = input.parse()?;
        let mut args = None;
        let mut handler = None;
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let key: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            if key == "args" {
                args = Some(input.parse()?);
            } else if key == "handler" {
                handler = Some(input.parse()?);
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "expected `args = Type` or `handler = fn`",
                ));
            }
        }
        Ok(Self {
            variant,
            args,
            handler,
        })
    }
}

/// Add a `Deploy` subcommand to an xtask command enum.
///
/// Usage:
///
/// ```ignore
/// #[tracel_deploy::deploy_commands(Deploy, handler = crate::commands::deploy::run)]
/// #[macros::base_commands(Build, Check, Test)]
/// pub enum Command { /* ... */ }
/// ```
///
/// The macro adds `Deploy(tracel_deploy::DeployCmdArgs)` unless `args = MyArgs`
/// is supplied. When `handler` is supplied it also generates:
///
/// ```ignore
/// pub fn handle_deploy_command(
///     command: tracel_deploy::DeployCmdArgs,
///     environment: tracel_xtask::prelude::Environment,
///     context: tracel_xtask::prelude::Context,
/// ) -> anyhow::Result<()> { /* delegates to handler */ }
/// ```
#[proc_macro_attribute]
pub fn deploy_commands(attr: TokenStream, item: TokenStream) -> TokenStream {
    let macro_args = parse_macro_input!(attr as DeployMacroArgs);
    let mut item = parse_macro_input!(item as ItemEnum);
    let variant = &macro_args.variant;
    if item
        .variants
        .iter()
        .any(|existing| existing.ident == *variant)
    {
        return syn::Error::new(
            variant.span(),
            "a deploy variant with this name already exists",
        )
        .to_compile_error()
        .into();
    }

    let args_type: Path = macro_args
        .args
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(tracel_deploy::DeployCmdArgs));
    let variant_args_type = args_type.clone();
    item.variants.push(syn::parse_quote! {
        #[doc = "Build, install, rotate, and verify a deployment."]
        #variant(#variant_args_type)
    });

    let handler_fn = macro_args.handler.map(|handler| {
        quote! {
            #[allow(dead_code)]
            pub fn handle_deploy_command(
                command: #args_type,
                environment: tracel_deploy::__private::tracel_xtask::prelude::Environment,
                context: tracel_deploy::__private::tracel_xtask::prelude::Context,
            ) -> tracel_deploy::__private::anyhow::Result<()> {
                #handler(command, environment, context)
            }
        }
    });

    let output = quote! {
        #item
        #handler_fn
    };
    TokenStream::from(output)
}
