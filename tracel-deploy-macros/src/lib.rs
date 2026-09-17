#![allow(linker_messages)]

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemEnum, Path, Token, parse_macro_input};

/// Dispatcher name used when `handler = fn` is given without `handler_name`.
const DEFAULT_HANDLER_NAME: &str = "handle_deploy_command";

struct DeployMacroArgs {
    variant: syn::Ident,
    args: Option<Path>,
    handler: Option<Path>,
    handler_name: Option<syn::Ident>,
    packages: Option<Vec<syn::LitStr>>,
}

impl syn::parse::Parse for DeployMacroArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let variant: syn::Ident = input.parse()?;
        let mut args = None;
        let mut handler = None;
        let mut handler_name: Option<syn::Ident> = None;
        let mut packages = None;
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
            } else if key == "handler_name" {
                handler_name = Some(input.parse()?);
            } else if key == "packages" {
                packages = Some(parse_package_list(input)?);
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "expected `args = Type`, `handler = fn`, `handler_name = ident`, \
                     or `packages = [\"crate\"]`",
                ));
            }
        }
        if let (Some(name), None, None) = (&handler_name, &handler, &packages) {
            return Err(syn::Error::new(
                name.span(),
                "`handler_name` renames a generated dispatcher; \
                 add `handler = <function>` or `packages = [\"crate\"]`, or drop `handler_name`",
            ));
        }
        if let (Some(packages), true) = (&packages, args.is_some() || handler.is_some()) {
            return Err(syn::Error::new(
                packages
                    .first()
                    .map_or_else(|| variant.span(), syn::LitStr::span),
                "`packages = [..]` generates the deploy handler itself; \
                 drop `args`/`handler`, or drop `packages` and write the handler yourself",
            ));
        }
        Ok(Self {
            variant,
            args,
            handler,
            handler_name,
            packages,
        })
    }
}

/// Parses `["crate", "another-crate"]` after `packages =`.
fn parse_package_list(input: syn::parse::ParseStream) -> syn::Result<Vec<syn::LitStr>> {
    let content;
    syn::bracketed!(content in input);
    let packages = content.parse_terminated(|input| input.parse::<syn::LitStr>(), Token![,])?;
    if packages.is_empty() {
        return Err(content.error("`packages` needs at least one package name"));
    }
    Ok(packages.into_iter().collect())
}

/// Add a `Deploy` subcommand to an xtask command enum.
///
/// # Attribute syntax
///
/// ```text
/// #[tracel_deploy::deploy_commands(Variant, args = MyArgs, handler = path::to::run, handler_name = my_dispatch)]
/// ```
///
/// | argument | default | effect |
/// | --- | --- | --- |
/// | `Variant` (positional) | – | name of the generated subcommand variant, for example `Deploy` |
/// | `args = Type` | `tracel_deploy::DeployCmdArgs` | clap args type carried by the generated variant |
/// | `handler = fn` | – | when present, also generates a dispatcher that calls `fn` |
/// | `handler_name = ident` | `handle_deploy_command` | name of that generated dispatcher; requires `handler` or `packages` |
/// | `packages = ["crate", ...]` | – | writes the whole deploy command for these packages: a variant with `tracel_deploy::DeployArgs` plus a dispatcher that calls `tracel_deploy::run_deploy`; cannot be combined with `args` or `handler` |
///
/// # Order with `base_commands`
///
/// Write `deploy_commands` **before** `base_commands`:
///
/// ```ignore
/// #[tracel_deploy::deploy_commands(Deploy, handler = crate::commands::deploy::run)]
/// #[macros::base_commands(Build, Check, Test)]
/// pub enum Command { /* ... */ }
/// ```
///
/// Attribute macros expand outermost first, so `deploy_commands` rewrites the
/// enum, keeps the `base_commands` attribute it found, and `base_commands`
/// still runs afterwards. The reverse order does not work, and it fails
/// *silently* rather than with a macro error: `base_commands` rebuilds the enum
/// from its own command list plus the variants it can see and does not re-emit
/// unknown attributes, so the inner `deploy_commands` never expands.
/// `rustc` then reports:
///
/// ```text
/// error[E0599]: no variant, associated function, or constant named `Deploy` found for enum `Command` in the current scope
/// ...
/// error[E0425]: cannot find function `handle_deploy_command` in this scope
/// ```
///
/// Troubleshooting a `deploy` subcommand that does not exist:
///
/// 1. Check the attribute order: `deploy_commands` goes on top of
///    `base_commands`, not below it.
/// 2. `no variant ... named Deploy` together with `cannot find function
///    handle_deploy_command` (or your own `handler_name`) is the signature of
///    the reversed order. Swap the two attributes and re-run `cargo check`.
/// 3. The variant error points at the `#[macros::base_commands(..)]` line: that
///    is the macro that built the enum, so nothing added a `Deploy` variant to
///    it.
/// 4. `cargo expand` shows what a macro actually produced. A `Deploy` variant
///    in the expansion means `deploy_commands` ran; its absence means the
///    attribute was dropped.
///
/// # Why not a combined macro
///
/// A combined macro that added both the deploy command and the base commands
/// would have to re-implement `tracel_xtask_macros::base_commands`: its private
/// command-to-args-type map (15 base commands in `tracel-xtask-macros` 3.2.0),
/// their doc strings, and the generated `dispatch_base_commands` function. That
/// map is an implementation detail of a separately versioned crate, so every
/// added, renamed, or re-typed base command in a new `tracel-xtask` release
/// would silently desynchronise the copy here until a consumer hits a missing
/// variant or an unknown args type. Paying that tracking cost to save one
/// attribute line is not worth it, so the ordering rule is documented instead.
///
/// # Generated dispatcher
///
/// The macro adds `Deploy(tracel_deploy::DeployCmdArgs)` unless `args = MyArgs`
/// is supplied. When `handler` is supplied it also generates (named by
/// `handler_name`, `handle_deploy_command` by default):
///
/// ```ignore
/// pub fn handle_deploy_command(
///     command: tracel_deploy::DeployCmdArgs,
///     environment: tracel_xtask::prelude::Environment,
///     context: tracel_xtask::prelude::Context,
/// ) -> anyhow::Result<()> { /* delegates to handler */ }
/// ```
///
/// The first parameter uses the `args` type, so `args = MyDeployArgs` makes the
/// dispatcher take `MyDeployArgs`.
///
/// # One-line deploys
///
/// A checkout that deploys what it contains needs neither `args` nor `handler`:
/// `packages = [..]` writes the variant and the body.
///
/// ```ignore
/// #[tracel_deploy::deploy_commands(Deploy, packages = ["my-app"])]
/// #[macros::base_commands(Build, Check, Test)]
/// pub enum Command {}
///
/// fn main() -> anyhow::Result<()> {
///     let (args, environment) = init_xtask::<Command>(parse_args::<Command>()?)?;
///     match args.command {
///         Command::Deploy(command) => handle_deploy_command(command, environment, args.context),
///         _ => dispatch_base_commands(args, environment),
///     }
/// }
/// ```
///
/// The generated dispatcher resolves the workspace root with
/// `tracel_deploy::WorkspaceRoot::detect`: `--workspace` wins, then this crate's
/// `CARGO_MANIFEST_DIR` (the xtask lives in `<root>/xtask`), then the
/// environment, then the current directory. It deploys every listed package with
/// `tracel_deploy::run_deploy` and prefixes a failure with `<xtask crate>
/// deploy: ` so the deploy log stays greppable.
/// ```compile_fail
/// // `packages` writes the handler, so it cannot be combined with `handler`.
/// #[tracel_deploy_macros::deploy_commands(Deploy, packages = ["my-app"], handler = run)]
/// pub enum Command {}
/// ```
///
/// # Argument errors
///
/// Unknown attribute keys are rejected at compile time:
///
/// ```compile_fail
/// #[tracel_deploy_macros::deploy_commands(Deploy, dispatcher = crate::run)]
/// pub enum Command {
///     Setup,
/// }
/// ```
#[proc_macro_attribute]
pub fn deploy_commands(attr: TokenStream, item: TokenStream) -> TokenStream {
    let DeployMacroArgs {
        variant,
        args,
        handler,
        handler_name,
        packages,
    } = parse_macro_input!(attr as DeployMacroArgs);
    let mut item = parse_macro_input!(item as ItemEnum);
    if item
        .variants
        .iter()
        .any(|existing| existing.ident == variant)
    {
        return syn::Error::new(
            variant.span(),
            "a deploy variant with this name already exists",
        )
        .to_compile_error()
        .into();
    }

    let args_type: Path = args.unwrap_or_else(|| {
        if packages.is_some() {
            syn::parse_quote!(tracel_deploy::DeployArgs)
        } else {
            syn::parse_quote!(tracel_deploy::DeployCmdArgs)
        }
    });
    let variant_args_type = args_type.clone();
    item.variants.push(syn::parse_quote! {
        #[doc = "Build, install, rotate, and verify a deployment."]
        #variant(#variant_args_type)
    });

    let handler_fn = if let Some(handler) = handler {
        let dispatcher = handler_name.unwrap_or_else(|| {
            syn::Ident::new(DEFAULT_HANDLER_NAME, proc_macro2::Span::call_site())
        });
        Some(quote! {
            #[allow(dead_code)]
            pub fn #dispatcher(
                command: #args_type,
                environment: tracel_deploy::__private::tracel_xtask::prelude::Environment,
                context: tracel_deploy::__private::tracel_xtask::prelude::Context,
            ) -> tracel_deploy::__private::anyhow::Result<()> {
                #handler(command, environment, context)
            }
        })
    } else if let Some(packages) = packages {
        let dispatcher = handler_name.unwrap_or_else(|| {
            syn::Ident::new(DEFAULT_HANDLER_NAME, proc_macro2::Span::call_site())
        });
        let packages = packages.into_iter().map(|package| package.value());
        Some(quote! {
            #[allow(dead_code)]
            pub fn #dispatcher(
                command: tracel_deploy::DeployArgs,
                _environment: tracel_deploy::__private::tracel_xtask::prelude::Environment,
                _context: tracel_deploy::__private::tracel_xtask::prelude::Context,
            ) -> tracel_deploy::__private::anyhow::Result<()> {
                // The workspace root is detected here, where this crate's
                // `CARGO_MANIFEST_DIR` is known: `--workspace` wins, then this
                // manifest directory, then the environment, then the current
                // directory.
                let root = tracel_deploy::WorkspaceRoot::detect(
                    tracel_deploy::DetectHint::new(command.common.workspace.as_deref())
                        .manifest_dir(::std::path::Path::new(env!("CARGO_MANIFEST_DIR"))),
                )?;
                tracel_deploy::run_deploy(&command, root.path(), &[#(#packages),*])
                    .map(|_| ())
                    .map_err(|error| {
                        tracel_deploy::__private::anyhow::anyhow!(
                            "{} deploy: {error:#}",
                            env!("CARGO_PKG_NAME")
                        )
                    })
            }
        })
    } else {
        None
    };

    let output = quote! {
        #item
        #handler_fn
    };
    TokenStream::from(output)
}
