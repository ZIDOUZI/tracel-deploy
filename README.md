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

The default plan stays in place when steps only need to be *inserted*:
`insert_after_default_build` and `insert_before_default_rotate` put a step
between the build and the rotation, which is where a service stop belongs
(stopping in a `pre` hook would stop the service before the build, and a failed
build would leave it down):

```rust
tracel_deploy::deploy(&context)
    .insert_after_default_build(DeployStep::Run(RunStep::new("my-service").args(["stop"])))
    .finally(|_| run_step("my-service", &["start"]))
    .run()?;
```

`DeployContext::builder(workspace, "my-service")` configures the context
fluently (`release`, `quiet`, `force`, `source`, `install_to`, `backup_dir`,
`state_dir`, `build_args`) and can point the build at a custom cargo executable,
profile, target triple, target directory, and features
(`--cargo`, `--profile`, `--locked`, `--target`, `--target-dir`, `--features`,
`--no-default-features` on the command line). `CARGO_TARGET_DIR` and
`CARGO_BUILD_TARGET` are honoured when locating the built binary;
`resolved_binary_path()` fails with the full list of paths it tried.

## Idempotent Deploys

Default plans are idempotent, and the decision is made *before* the build from
the deployment stamp in `state_dir`. A skip requires both halves of the proof:

- the build inputs (`src/**`, `Cargo.toml`, `Cargo.lock`, `build.rs`,
  `rust-toolchain*`, `.cargo/config.toml`) still hash to the
  `build_inputs_fingerprint` recorded by the stamp (see
  `build_inputs_fingerprint` / `build_inputs_fingerprint_with`), and
- the installed binary still matches the destination fingerprint recorded by
  the stamp.

Only then is the whole plan skipped (`StepStatus::Skipped`) and the destination
left untouched. Because the comparison uses the recorded *build inputs* rather
than the build output on disk, a changed source tree with a stale build output
is never mistaken for "up to date"; a missing or legacy stamp never skips, and
`force = true` / `--force` always runs the full plan.

Callers that replaced the default plan (`without_defaults()`) can opt into the
same behaviour with `DeployBuilder::idempotent_skip`, `verify_install`, and
`write_deploy_stamp`. Those switches default to the old behaviour: on for the
default plan, off when the caller takes over the plan.

Every non-dry-run deployment takes a lock in `state_dir`
(`<binary>.deploy.lock`), so two deployments of the same binary cannot rotate it
concurrently; the lock error names the file to delete after a crashed deploy.
After a successful rotation the installed binary is verified against the source
(content hash, size, and - on Unix - the executable bit), and a
`<binary>.stamp` file with the build-input, source, and destination fingerprints
is written to `state_dir` for diagnostics.

## Running-Binary Rotation

`rotate` replaces the installed binary without requiring the running process to
exit first:

- **Linux/macOS**: `fs::rename` a sibling temporary file over the destination.
  The running process keeps the old inode; the replacement is atomic.
- **Windows**: a normal copy is attempted first. If the destination is locked
  by a running executable (`ERROR_ACCESS_DENIED`, `ERROR_SHARING_VIOLATION`,
  `ERROR_LOCK_VIOLATION`, or `ERROR_USER_MAPPED_FILE`), the old binary is
  renamed aside and the new binary is copied into place. The running process
  keeps the renamed old file.

`RotateStrategy::Auto` picks the above behaviour, and the remaining strategies
make the choice explicit. `describe()` renders what the current platform will
actually do, and dry-run plan lines include `(strategy: ...)`:

| strategy | Windows | Linux / macOS |
| --- | --- | --- |
| `Auto` | in-place copy; running binary (`5`/`32`/`33`/`1224`) falls back to rename-aside | sibling temp file + atomic rename |
| `Replace` | in-place copy; same rename-aside fallback as `Auto` | in-place copy; fails while the destination runs |
| `Atomic` | **degraded** to rename-aside (Windows has no atomic replace) | sibling temp file + atomic rename |
| `RenameAside` | rename aside, copy, undo the rename when the copy fails | same |
| `FailIfLocked` | in-place copy only; a running/locked destination fails instead of being renamed aside | in-place copy only; `ETXTBSY` (running binary) fails instead of being renamed aside |

`FailIfLocked` is the conservative choice for services that must be stopped by
the deploy loop: the rotation never moves a running image out of the way, so a
forgotten stop fails loudly instead of silently replacing the file under the
running process.

When `rename_aside_replace` cannot copy the replacement, the error names the
destination, the backup path, the rollback result, and the underlying cause:

```text
failed to install C:\...\my-service.exe after renaming the old binary aside to
C:\...\my-service.exe.old-1234-1700000000; rollback: ok; cause: <copy error>
```

`destination` may be a symlink: it is canonicalized first, so the rotation
replaces the link *target* and keeps the link itself in place.

`RotateOptions::backup_dir` can direct old Windows binaries into a state
directory instead of the install directory. `RotateOptions::backup_retention`
(keep the newest N backups) and `RotateOptions::backup_max_age` remove stale
`<binary>.old-*` files after a successful rotation; backups that are still held
by a running process are left in place, and `--backup-retention` /
`--backup-max-age-days` expose the same settings on the command line.

**Deferred cleanup.** `RotateOptions::backup_cleanup_retry` (`BackupRetry`,
default 7 days / 32 entries) turns on deferred cleanup: when a `<binary>.old-*`
backup cannot be removed - the usual Windows case is a file still held by a
running process - the rotation does not fail. The path, first failure, attempt
count, and reason are registered in `<backup_dir>/<binary>.pending-delete`, the
next rotation retries them first, and `cleanup_pending_backups(&options)` offers
the same retry explicitly. Entries beyond `max_age` / `max_entries` are dropped
and reported in `CleanupReport::discarded` (`expired` / `evicted` /
`out-of-scope`). Dry runs neither delete nor register, and only backups of the
current destination are ever touched.

## Service Helpers (feature `service`)

The `service` feature (off by default) adds the helpers a `stop -> rotate ->
start -> probe` loop needs, instead of every consumer writing its own pid
files, detached spawn, and readiness probe:

```toml
tracel-deploy = { version = "0.1", features = ["service"] }
```

- `ServiceSpec` describes one service: name, binary, args, env, cwd, log file,
  state dir, and an optional `ProbeSpec` (`ProbeKind::Tcp` or `ProbeKind::Http`
  with a path and expected status).
- `stop_checked(&state, timeout)` compares the image path reported by the OS
  with the binary recorded in the state file and refuses to kill on mismatch,
  so a recycled pid cannot take an unrelated process down; `stop(pid, timeout)`
  is the unguarded primitive. Both are idempotent (`StopOutcome::NotRunning`),
  and `stop_service(&spec, timeout)` also covers an instance that runs without
  a state file.
- `start_detached(&spec)` checks the recorded state, then the readiness probe,
  then scans running processes for the resolved image, so an instance started by
  a scheduled task that never wrote a state file is detected instead of spawned
  twice. Windows children get `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`
  (retried without the breakaway flag when the enclosing job forbids it), Linux
  children get their own session.
- `start_and_wait_ready(&spec, retries, interval, timeout)` starts and probes;
  failures report the last `LOG_TAIL_LINES` lines of the service log.
- State files are `<state_dir>/<name>.service` (`key=value`, written to a
  sibling `.tmp` and renamed into place). A missing, truncated, or unparsable
  file counts as "no state", so the guardrails fall back to probing and scanning
  instead of guessing.

```rust
use std::time::Duration;
use tracel_deploy::{DeployStep, service};

let spec = service::ServiceSpec::new("my-service", &install_path, &log_file, &state_dir);

tracel_deploy::deploy(&context)
    .insert_after_default_build(DeployStep::Custom(Box::new({
        let spec = spec.clone();
        move |_| service::stop_service(&spec, Duration::from_secs(10)).map(|_| ())
    })))
    .finally({
        let spec = spec.clone();
        move |_| {
            service::start_and_wait_ready(
                &spec,
                30,
                Duration::from_millis(500),
                Duration::from_secs(2),
            )
            .map(|_| ())
        }
    })
    .run()?;
```

## State and Hooks

The library is intentionally unopinionated about service management. A service
that needs a restart should use pre/post hooks or `DeployStep::Run` (or the
optional helpers above):

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

Every printed line goes through a `Reporter`. The default `StdoutReporter` keeps
the 0.1.0 output and stays silent when the context sets `quiet`,
`NullReporter` silences everything, and `DeployBuilder::reporter(...)` lets a
caller collect structured events instead (`plan`, `step_started`, `note`,
`step_finished`, `warning`).

`DeployStepReport` carries `status`, `duration`, `exit_code`, `artifacts`, and
`detail`. `RunStep` adds `capture_stdout()` / `capture_stderr()`,
`timeout(Duration)` (the child is killed and the error names program, args, cwd,
the timeout, and the exit code), `expected_status(i32)`, and
`verify(|outcome| ...)`; `allow_failure(true)` still succeeds and keeps the exit
code plus the stderr tail.

## State Files and Paths

`state::write_state_file` (atomic `<name>.tmp` + rename), `read_state_file`
(missing file reads as `None`), and `remove_state_file` give consumers the same
state convention the deploy stamp and the service module use.
`same_file_path` compares two paths the way Windows does (strips the `\\?\`
prefix, normalizes separators, ignores case) and case-sensitively elsewhere, and
`resolve_install_path` turns either an install directory or a full file path
into the final binary path.
