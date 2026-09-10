# tracel-deploy

Composable deployment helpers for [`tracel-xtask`](https://github.com/tracel-ai/xtask).

The workspace contains:

- `tracel-deploy`: runtime library with a `deploy` builder, pre/post hooks,
  generic build/run/rotate steps, and a cross-platform `rotate` function.
- `tracel-deploy-macros`: `#[deploy_commands(...)]`, which mirrors
  `tracel_xtask_macros::base_commands` by adding a `Deploy` subcommand to an
  xtask command enum.

## Macro

```rust
use tracel_deploy::prelude::*;
use tracel_xtask::prelude::*;

#[tracel_deploy::deploy_commands(Deploy, handler = crate::commands::deploy::run)]
#[macros::base_commands(Build, Check, Test)]
pub enum Command {
    Setup(SetupArgs),
}

fn main() -> anyhow::Result<()> {
    let args = init_xtask::<Command>(parse_args::<Command>()?)?;
    match args.command {
        Command::Setup(_) => Ok(()),
        Command::Deploy(command) => {
            handle_deploy_command(command, args.environment, args.context)
        }
        _ => dispatch_base_commands(args),
    }
}
```

When used with `tracel_xtask_macros::base_commands`, put
`deploy_commands` first. `base_commands` reconstructs the enum and does not
re-emit unknown outer attribute macros, while `deploy_commands` preserves the
remaining `base_commands` attribute.

`args = MyDeployArgs` can be used when the project needs extra flattenable
arguments. Without `handler`, the macro only adds the variant; dispatch it
yourself.

## Deploy Function

```rust
pub fn run(args: tracel_deploy::DeployCmdArgs, _env: Environment, _ctx: Context)
    -> anyhow::Result<()>
{
    let workspace = tracel_deploy::context::current_workspace_root()?;
    let context = tracel_deploy::DeployContext::from_args(&args, workspace, "my-service")?;

    tracel_deploy::deploy(&context)
        .pre(|_| stop_my_service())
        .post(|_| start_and_verify_my_service())
        .run()?;
    Ok(())
}
```

For a totally custom ordering, use `without_defaults()` and append
`DeployStep::Run`, `DeployStep::Rotate`, `DeployStep::Custom`, etc.

## Running-Binary Rotation

`rotate` replaces the installed binary without requiring the running process to
exit first:

- **Linux/macOS**: `fs::rename` a sibling temporary file over the destination.
  The running process keeps the old inode; the replacement is atomic.
- **Windows**: a normal copy is attempted first. If the destination is locked
  by a running executable (`ERROR_ACCESS_DENIED`, `ERROR_SHARING_VIOLATION`, or
  `ERROR_LOCK_VIOLATION`), the old binary is renamed aside and the new binary is
  copied into place. The running process keeps the renamed old file.

`RotateStrategy::Auto` picks the above behaviour. `Replace`, `Atomic`, and
`RenameAside` are available for callers that want an explicit strategy.

`RotateOptions::backup_dir` can direct old Windows binaries into a state
directory instead of the install directory. `RotateOptions::backup_retention`
(keep the newest N backups) and `RotateOptions::backup_max_age` remove stale
`<binary>.old-*` files after a successful rotation; backups that are still held
by a running process are left in place, and `--backup-retention` /
`--backup-max-age-days` expose the same settings on the command line.

## State and Hooks

The library is intentionally unopinionated about service management. A service
that needs a restart should use pre/post hooks or `DeployStep::Run`:

```rust
tracel_deploy::deploy(&context)
    .pre(|_| run_step("my-service", &["stop"]))
    .step(DeployStep::Run(RunStep::new("my-service").args(["migrate"])))
    .post(|_| run_step("my-service", &["start"]))
    .run()?;
```

Hooks run in four phases: `pre -> steps -> post -> finally`. A failing pre
hook, step, or post hook skips `post`, then runs the `on_error` hooks (rollback,
alerting) followed by the `finally` hooks. `finally` hooks always run - also
when an `on_error` hook fails - and every failure is aggregated with the
original error kept first. A service that is stopped before the rotation should
be restarted from `finally`, so a failed rotation cannot leave it down:

```rust
tracel_deploy::deploy(&context)
    .step(DeployStep::Run(RunStep::new("my-service").args(["stop"])))
    .on_error(|_| eprintln!("deploy failed; restarting the previous service"))
    .finally(|_| run_step("my-service", &["start"]))
    .run()?;
```

Hooks can be named (`pre_named`, `post_named`, `on_error_named`,
`finally_named`); the name shows up in error messages and in
`DeployReport::hooks`. Best-effort cleanup inside a step can be modelled with
`RunStep::allow_failure(true)`, which keeps the exit code and the stderr tail in
the step report.

`--dry-run` renders the full plan without executing any hook or step. Unless
`quiet` is set, the plan is printed as `would: ...` lines and it is always
available as `DeployReport::plan`:

```text
would: cargo build -p my-service --color always --release
would: rotate D:\work\my-service\target\release\my-service.exe -> C:\Users\me\.cargo\bin\my-service.exe
```
