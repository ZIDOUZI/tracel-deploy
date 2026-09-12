# tracel-deploy-macros

Proc macros for [`tracel-deploy`](https://crates.io/crates/tracel-deploy).

## `deploy_commands`

The `deploy_commands` attribute mirrors `tracel_xtask_macros::base_commands`:

```rust
use tracel_deploy::prelude::*;
use tracel_xtask::prelude::*;

#[tracel_deploy::deploy_commands(Deploy, handler = crate::commands::deploy::run)]
#[macros::base_commands(Build, Check, Test)]
pub enum Command {
    Setup(SetupArgs),
}
```

Attribute arguments:

| argument | default | effect |
| --- | --- | --- |
| `Variant` (positional) | – | name of the generated subcommand variant, for example `Deploy` |
| `args = Type` | `tracel_deploy::DeployCmdArgs` | clap args type carried by the generated variant |
| `handler = fn` | – | when present, also generates a dispatcher that calls `fn` |
| `handler_name = ident` | `handle_deploy_command` | name of that generated dispatcher; requires `handler` |

Unknown keys are rejected at compile time with:

```text
expected `args = Type`, `handler = fn`, or `handler_name = ident`
```

### Customizing the generated command

```rust
#[derive(clap::Args, Clone, Debug)]
pub struct MyDeployArgs {
    #[command(flatten)]
    pub deploy: tracel_deploy::DeployCmdArgs,
    #[arg(long)]
    pub verify_timeout: Option<u64>,
}

#[tracel_deploy::deploy_commands(
    Deploy,
    args = MyDeployArgs,
    handler = crate::commands::deploy::run,
    handler_name = run_deploy
)]
#[macros::base_commands(Build, Check, Test)]
pub enum Command {
    Setup(SetupArgs),
}
```

This adds a `Deploy(MyDeployArgs)` variant and generates

```rust
pub fn run_deploy(
    command: MyDeployArgs,
    environment: Environment,
    context: Context,
) -> anyhow::Result<()> { /* calls crate::commands::deploy::run */ }
```

Without `handler` only the variant is generated and you dispatch it yourself.
`handler_name` without `handler` is a compile error, because there is no
generated dispatcher to rename. `tracel-deploy/examples/xtask_like.rs` is a
complete working example.

## Ordering: `deploy_commands` goes first

`deploy_commands` is intentionally placed before `base_commands` because
`base_commands` reconstructs the enum and does not re-emit unknown outer
attribute macros.

Both attributes are applied outermost first, so with the correct order
`deploy_commands` runs first, rewrites the enum, leaves the `base_commands`
attribute in place, and `base_commands` still expands afterwards. In the
reverse order `base_commands` expands first, drops the inner
`#[tracel_deploy::deploy_commands(..)]` attribute, and `deploy_commands` never
runs. No macro reports an error; the mistake shows up where the generated items
should have been:

```text
error[E0599]: no variant, associated function, or constant named `Deploy` found for enum `Command` in the current scope
...
error[E0425]: cannot find function `handle_deploy_command` in this scope
```

Troubleshooting a `deploy` subcommand that "does not exist":

1. Check the attribute order: `deploy_commands` goes on top of
   `base_commands`, not below it.
2. That pair of errors (`no variant ... named Deploy` plus `cannot find
   function handle_deploy_command`, or your own `handler_name`) is the
   signature of the reversed order. Swap the two attributes and re-run
   `cargo check`.
3. The variant error points at the `#[macros::base_commands(..)]` line: that is
   the macro that built the enum, so nothing added a `Deploy` variant to it.
4. `cargo expand` shows what a macro actually produced. A `Deploy` variant in
   the expansion means `deploy_commands` ran; its absence means the attribute
   was dropped.

The same rule applies to any other attribute macro that rebuilds the enum: keep
`deploy_commands` outermost.

## Why not a combined macro

A combined macro that added both the deploy command and the base commands would
have to re-implement `tracel_xtask_macros::base_commands`: its private
command-to-args-type map (15 base commands in `tracel-xtask-macros` 3.2.0), the
doc string of every variant, and the generated `dispatch_base_commands`
function. That map is an implementation detail of a crate versioned separately
from this one, so every added, renamed or re-typed base command in a new
`tracel-xtask` release would silently desynchronise the copy here; consumers
would first notice it as a missing variant or an unknown args type. That
tracking cost is not worth saving one attribute line, so the ordering rule is
documented (and pinned by the `deploy_commands` doctests) instead.

See `tracel-deploy` for the runtime deploy builder, pre/post hooks, and the
cross-platform running-binary rotation implementation.
