# Changelog

## 0.3.0

The staleness decision moves to the tools that own it - cargo decides whether
there is anything to build, the rotation decides whether there is anything to
install - and the deploy command of a checkout collapses into one call. The
behaviour change is in the default plan; the old comparison stays available as
`DeployBuilder::idempotent_skip(true)`.

### Cargo decides what is stale

- The default plan no longer compares a build-input list before the build. The
  build step always runs - cargo no-ops on its own fingerprints, which cover
  `include_bytes!` / `include_str!` files, `build.rs` rerun inputs, features,
  profile, `RUSTFLAGS`, and the toolchain - and the rotation is a no-op when the
  built binary and the installed one hold the same bytes.
- A rotation with nothing to install reports `StepStatus::Skipped`; the
  post-install verification and the stamp write are skipped with it, so
  `<binary>.stamp` keeps describing the last real install.
- The before-the-build comparison stays available as
  `DeployBuilder::idempotent_skip(true)`; it is no longer the default.
  `TargetStatus::Skipped` in a batch report now also covers "the rotation found
  nothing to install".
- `STALE_LOCK_AFTER` (10 minutes): a deployment lock left behind by a killed
  deployment is taken over instead of blocking every later deploy. A fresh lock
  names the owning pid and refuses, and dropping a lock that another deployment
  took over leaves the file alone.

### One-call checkout deploys

- `DeployArgs`: the complete deploy argument surface (queue switches plus the
  common flags) as one flattens-anywhere type, so a consumer no longer writes
  its own struct with two `#[command(flatten)]` fields.
- `DeployContext::for_checkout` and `checkout_state_dir`: `from_args` plus the
  conventions a checkout deploy uses (release build unless `--profile`, state,
  locks, stamps, and logs in `<root>/target/codex-deploy`, and a rotation that
  sweeps the backups it replaced). Every convention is a default the matching
  flag overrides.
- `run_deploy` / `DeployOutcome`: the whole deploy command in one call -
  conventions, the `--background` queue with its `queued <packages> redeploy
  (pid=...)` line, and one plan per package, failing fast.
- `#[deploy_commands(Deploy, packages = ["my-app"])]` writes the variant *and*
  the dispatcher for that command, turning a 0.2.1-era xtask deploy (context
  setup, queue branch, root/package helpers: ~80 lines) into the attribute plus
  one match arm. `packages` cannot be combined with `args`/`handler`.

### Documentation

- The `background` module example built the queued child's command line without
  `child_argv`, so a copied example queued a child that queues another child:
  `queue_deploy` passes its `argv` through verbatim and never swaps
  `--background` for `--child` itself. The example and the module overview now
  call `child_argv`, and both queue functions document the pass-through.

## 0.2.1

Hook-driven background deploys plus a dependency floor. The 0.2 API is
unchanged, so consumers only have to bump the version.

### Hook-driven deploys

- `DeployQueueArgs` (`--background`, hidden `--child`) plus `queue_deploy` /
  `queue_executable`: the per-turn hook shape, so a hook returns at once and a
  detached child runs the deployment with its output in
  `<state_dir>/deploy.log` and `<state_dir>/deploy-error.log` (both truncated
  per queued run). No consumer has to rewrite argv, open logs, or build its own
  detached spawn again.
- `codex_stop_hook_command` and `codex_stop_hook_json` render the
  `.codex/hooks.json` `Stop` entry (`async: true`, hook timeout, status message)
  that calls `deploy --background`.
- The Windows detached spawn (breakaway, `ERROR_ACCESS_DENIED` retry) now lives
  in one place and is shared with the `service` feature.

### Dependencies

- `tracel-xtask` is a floor only (`>=0.3`, no upper bound): cargo resolves the
  version a consumer already uses (3.x, 4.x, 5.x, ...) instead of adding a
  second copy of the framework to their xtask graph. Verified both ways: the
  locked 3.2.0 still builds this workspace, and a consumer resolving 5.3.0
  compiles `tracel-deploy` against it.

## 0.2.0

The 0.2.0 line is the hardening release: hooks with a real failure model,
idempotent deploys, structured reports, richer rotation, and two optional
modules. The 0.1 API keeps working; consumers only have to bump the version.

### Hooks, plans, and dry runs

- `on_error` and `finally` phases: a failing pre hook, step, or post hook runs
  `on_error` and then `finally`; `finally` always runs, and failures are
  aggregated with the original error kept first.
- Named hooks (`pre_named`, `post_named`, `on_error_named`, `finally_named`) and
  `DeployReport::hooks` / `DeployHookReport`.
- `DeployPlan::standard`, `insert_after_default_build`,
  `insert_before_default_rotate`, and `DeployContext::builder`, so a service
  stop can sit between the build and the rotation without rebuilding the plan.
- Dry-run renders the full plan (`DeployStep::describe`, `DeployReport::plan`)
  including hook phases, and `preflight` hooks run only during a dry run for
  read-only checks.

### Idempotent deploys

- `DeployLock` (one deployment per binary), a `<binary>.stamp` file with the
  build-input, source, and destination fingerprints, and post-rotation
  verification of the installed binary.
- `build_inputs_fingerprint` / `build_inputs_fingerprint_with`: the default plan
  skips only when the recorded build inputs still hash the same *and* the
  installed binary still matches the stamp, so a changed source tree with a
  stale build output is never mistaken for "up to date".
- `DeployBuilder::idempotent_skip`, `verify_install`, and
  `write_deploy_stamp` expose the same behaviour to callers that replaced the
  default plan.

### Rotation

- `RotateStrategy::FailIfLocked` for conservative deploys, plus a documented
  platform x strategy behaviour matrix (`label()` / `describe()`) and
  `(strategy: ...)` in dry-run plan lines.
- Windows running-binary classification now includes `ERROR_USER_MAPPED_FILE`
  (1224) next to 5 / 32 / 33, and a real "replace a running executable" test.
- Rename-aside failures report the destination, the backup path, the rollback
  result, and the underlying cause; symlink semantics (the link target is
  replaced, the link survives) are documented and tested.
- `backup_retention` / `backup_max_age` clean stale `<binary>.old-*` files, and
  `backup_cleanup_retry` (`BackupRetry`, `cleanup_pending_backups`) registers
  backups that could not be removed yet (a running process holds them on
  Windows) in `<binary>.pending-delete` for the next rotation.

### Reports and steps

- `DeployStepReport` carries `status`, `duration`, `exit_code`, `artifacts`, and
  `detail`; `StepStatus` reports `Success` / `Skipped` / `IgnoredFailure` /
  `Failed`.
- Every printed line goes through `Reporter`: `StdoutReporter` (0.1 output,
  silent when `quiet` is set), `NullReporter`, and caller-provided reporters via
  `DeployBuilder::reporter`.
- `RunStep` gained `capture_stdout()`, `capture_stderr()`, `timeout()`,
  `expected_status()`, and `verify()`; `allow_failure(true)` keeps the exit code
  and the stderr tail.

### Optional modules

- `service` feature: `ServiceSpec`, `ProbeSpec` (TCP/HTTP), `stop_checked` (image
  path guard before killing), `stop_service`, `start_detached` (no duplicate
  instances), `start_and_wait_ready`, `wait_ready`, `tail_log`,
  `executable_path`, `find_running_instance`, and atomic `key=value` state
  files.
- `state` module: `write_state_file` / `read_state_file` / `remove_state_file`
  (atomic tmp + rename), `same_file_path` (Windows `\\?\`, separators, case),
  and `resolve_install_path`.
- `deploy_commands` accepts `handler_name = my_dispatch` and documents the
  `base_commands` ordering trap with a `compile_fail` example.

### Consumer-repo follow-ups

Consumers on 0.1.0 can delete their hand-written copies of these helpers after
migrating: build-input fingerprints and deploy stamps (judo, zellij-plugins,
codex-router), service lifecycle and readiness probing (mcp-hub, codex-router,
zellij-plugins), multi-package deploy loops (mcp-hub), and plan rendering (all).
Two migration notes: adding fields to `DeployCmdArgs` and
`RotateOptions`/`DeployStepReport` breaks exhaustive struct literals, and
mcp-hub's dry-run rendering should read `DeployReport::plan` instead of
`DeployStepReport::detail`.

## 0.1.0

Initial release: `deploy_commands`, `DeployCmdArgs`, the `deploy` builder with
pre/post hooks, cargo build / rotate / run steps, and cross-platform
running-binary rotation.
