# Changelog

## 0.4.5

### Background deploys escape restrictive Windows hook jobs

- When Windows rejects `CREATE_BREAKAWAY_FROM_JOB`, the deploy queue now starts
  the child through a short-lived WMI broker outside the caller's job instead of
  retrying a plain spawn inside it. The broker keeps `deploy.log` /
  `deploy-error.log`, starts the real child with `CreateNoWindow`, and returns
  the real child pid, so a Codex hook timeout can no longer terminate an active
  build while leaving `.deploy.lock` behind.
- A broker that misses the startup deadline is cancelled through a launch token,
  bound to a uniquely named kill-on-close Windows Job, and terminated through
  that Job while its exact process handle is still held. Temporary files are
  removed with bounded retries. If Job termination fails, the queue reports a
  distinct non-retryable error instead of allowing a late broker or child to
  launch duplicate work.
- Regression coverage assigns a queueing helper to a restrictive Windows Job,
  terminates that Job, and verifies that the queued child survives, writes its
  log, stays windowless, and receives arguments containing spaces, quotes, and
  backslashes verbatim.

### Dead deployment locks are reclaimed immediately

- `DeployLock::acquire` now checks the recorded owner pid. A dead owner is taken
  over immediately; a live owner keeps the lock even after
  `STALE_LOCK_AFTER`. A live lock refreshes its mtime, protecting healthy owners
  whose pid cannot be inspected. The ten-minute threshold remains only when
  liveness is unknown and the heartbeat has stopped. A sidecar takeover marker
  serializes reclaim attempts, which recreate the lock with `create_new`
  instead of truncating a file another process may now own.

## 0.4.4

### Revert the `conhost.exe --headless` hook wrapper shipped in 0.4.3

- 0.4.3 wrapped the generated `Stop` hook command in `conhost.exe --headless`,
  so a console-less (GUI) hook runner would not flash a `cargo` window.
  Measured afterwards on Windows 11 26100, that host cannot carry the command:
  it returns before its payload finishes (a 4 s payload was still unfinished
  when conhost exited after 1.7 s), it never propagates the payload's exit code
  (always 0), and it replaces the payload's stdout with its own console escape
  sequences. Commands that carry quotes are mis-parsed - `cmd /c "echo hi>
  file"` wrote nothing while the unquoted form worked. A `timeout: 30` hook
  whose job is to queue a deployment cannot run under that.
- `codex_stop_hook_command` is back to the plain `cargo run --quiet
  --manifest-path … -- deploy --background`, with a comment that records why the
  wrapper is not used, and the tests that asserted the wrapper are gone. The
  `service::sys::spawn_detached` documentation fix from 0.4.3 stays.
- Not in this release: a library-owned headless launcher (a GUI-subsystem
  executable that starts the payload with `CREATE_NO_WINDOW`) was prototyped and
  measured as a safe alternative - no window, payload completes, stdio and exit
  code forwarded, about +0.1 s per hook - but it needs a binary that consumers
  install, so it is a separate change.

## 0.4.3

> Reverted in 0.4.4: the `conhost --headless` wrapper below drops its payload
> (see the 0.4.4 entry).

### The `Stop` hook command no longer pops a console window

- `background::codex_stop_hook_command` wraps its `cargo run ... -- deploy
  --background` payload in `conhost.exe --headless` on Windows (when
  `%SystemRoot%\System32\conhost.exe` exists; elsewhere the plain command is
  used). A hook runner that owns a console passes it to the hook and nothing
  appears, but a runner with *no* console - a GUI host - makes Windows allocate
  a new, **visible** console for the first console program the hook starts: the
  `cargo` window that flashed on every turn. `conhost.exe --headless` is a
  GUI-subsystem host: it never allocates a console window for itself, owns a
  console without a window, and hands that console to the program it starts, so
  the whole hook is invisible no matter what the runner looks like. The wrapped
  command's stdout/stderr belong to that headless console rather than to the
  runner; `deploy.log` and `deploy-error.log` stay the record of the deploy.
- Regression tests: `the_hook_command_runs_through_a_console_host` (the shape)
  and `the_hook_command_hides_the_console_a_console_less_runner_never_had` (the
  mechanism - `CREATE_NEW_CONSOLE` models the console-less runner, the helper
  inside the command reports `console=0 visible=0`). The ignored control
  `a_console_less_runner_flashes_a_bare_console_program` shows the failure the
  wrapper prevents.
- Docs: the `service::sys::spawn_detached` wrapper still described the old
  `DETACHED_PROCESS` flags; it now names the delegated `CREATE_NO_WINDOW |
  CREATE_NEW_PROCESS_GROUP` behaviour.
- Migration: the generated `.codex/hooks.json` entry changes, so consumers
  regenerate it and re-approve the hook (Codex `/hooks`). A hook whose trust
  hash no longer matches is skipped silently.

## 0.4.2

### A queued deploy no longer holds the hook's stdio pipes

- `detach::spawn_detached` clears `HANDLE_FLAG_INHERIT` on the calling
  process's std handles for the duration of the spawn and restores them
  afterwards. Windows runs `CreateProcess` with `bInheritHandles = TRUE`, so
  the detached child used to inherit the stdout/stderr pipes of whatever
  started the queue - in the field a Codex `Stop` hook runner. The runner keeps
  the hook "running" until those pipes close, so it reached the hook's
  `timeout` and killed the hook together with the deploy child still in its
  job: the build stopped mid-crate and `.deploy.lock` stayed behind, blocking
  the next deploy for the stale-lock window. Field case (2026-09-17, mcp-hub):
  the release build died 28 s in - `"timeout": 30` - while the session stayed
  open, and the next deploy was refused for 10 minutes.
- Regression test:
  `background::tests::a_queued_child_does_not_hold_the_queueing_commands_stdio`
  - a queueing helper whose stdout is a pipe must see EOF as soon as it exits,
  not when the queued child dies.

## 0.4.1

### A service instance needs an identity, not just an image

- `service::InstanceFilter` + `ServiceSpec::instance_filter(..)`: the image-path
  scan in `find_running_instance` now only reports a candidate whose
  `ProcessInfo` (pid, image, command line) passes the caller's predicate, and a
  **spec without a filter does not scan at all** - an executable is not an
  identity. Field case (2026-09-17): one `codex-scheduler.exe` ran as the
  deploy-managed SSE server *and* as one stdio client per Codex session; the old
  scan saw a stdio instance as "already running" and stopped one instead of the
  server.
- `InstanceFilter::command_line_contains_any(["--mode sse", "--background"])` is
  the ready-made predicate for the common case; `InstanceFilter::new(closure)`
  takes anything else. `ProcessInfo` and `InstanceFilter` are re-exported from
  the prelude.
- The command line is read without spawning a shell: Windows through the
  target's PEB (`NtQueryInformationProcess` + `ReadProcessMemory`), Linux
  through `/proc/<pid>/cmdline`. An unreadable command line reports
  `command_line: None`, which a filter must treat as "not an instance".
- `Instance::Scanned` now means "image path *and* filter matched";
  `stop_service` on a probe that still answers but has no matching instance
  reports `NotRunning` with a message that says whether a filter was set.

## 0.4.0

### Timeline model (0.4.0 groundwork)

- `plan::phase` (new): `Phase` - the lifecycle events (`Preflight`, `Pre`, `Post`,
  `OnError`, `Finally`) plus the timeline slots (`Build`, `AfterBuild`, `Rotate`,
  `AfterRotate`) - and `Handler` (`new` / `with_state` / `describe` /
  `skip_when`), `Outcome` (`Done` / `Skipped`), and `RunState` (what the run
  knows: `installed`, `phases_done`, `notes`, a typed `set` / `get` / `take`
  slot, and `service` / `stopped_service` under the `service` feature). The phase
  enum carries no data on purpose: registration-time configuration lives in the
  handler, per-run facts in `RunState`, per-instance data (pid, args, listen,
  log) in `ServiceState`, and results in `Outcome`.
- `plan.rs` moved to `plan/mod.rs` for the planned module split
  (`plan/phase.rs` today; `plan/steps.rs` and `plan/report.rs` later).
- `RotateStep::backup_cleanup_retry(Some(BackupRetry::new()))`: the plan step can
  use the deferred backup cleanup `RotateOptions` already had, so a consumer no
  longer has to call `rotate()` from a custom step (which a dry run rendered as
  `custom step (opaque)`).
- `service`: an image that is a rotated-aside copy of the recorded binary
  (`<binary>.old`, `<binary>.old-<stamp>`) counts as the same instance in
  `stop_checked` and `find_running_instance`. The rotation moves a running
  destination aside on Windows, and refusing to stop that copy left the
  deployment without a rotation target.
- Still to come for 0.4.0: `DeployBuilder::on` / `insert` running the plan
  through the phases, `DeployEventReport`, `service::ServiceStop` /
  `service::ServiceStart`, and `run_deploy_with` plus workspace detection.

### The plan runs on the phase timeline

- `DeployBuilder::on(phase, handler)`, `insert(handler)` (uses
  `Handler::at` / the handler's default phase), and `defaults(bool)`: the plan is
  one timeline of `(Phase, Handler)` entries, executed in `Phase::ORDER` with
  the 0.3 failure rules (a failing step stops its phase and every later phase
  except `on-error` and `finally`; a failing hook does not stop its own phase).
- The 0.3 builder methods (`preflight`, `pre`, `post`, `on_error`, `finally`,
  `step`, `insert_after_default_build`, `insert_before_default_rotate`,
  `without_defaults`) are thin wrappers over `on` / `insert`, and keep the 0.3
  plan lines and report names byte-identical.
- `insert_after_default_build` / `insert_before_default_rotate` now apply to a
  caller-owned plan too (`defaults(false)`): `AfterBuild` is a slot on the
  timeline, not part of the default plan, and steps registered there run in call
  order.
- The "the rotation found nothing to install" decision reads the `Rotate` slot's
  outcome (`RunState::rotation`) instead of a recorded step index, so it works
  for a caller-owned plan as well.
- `Handler` accepts built-in steps (`From<DeployStep>`, and the three step
  types), so `on(Phase::Rotate, RotateStep::from_context(context))` and a
  closure handler are registered the same way. Lines a handler records with
  `RunState::note` are forwarded to the reporter.
- Removed: `DeployHookEntry` (the `DeployHook` alias stays). `DeployPlan`'s
  public fields are gone; use `handlers(phase)`.
### Structured events, and the post-rotation entries on the timeline

- `DeployReport::events: Vec<DeployEventReport>`: every entry that ran, in
  execution order, with `phase`, `name`, `status`, `detail`, `duration`,
  `exit_code`, `artifacts`, and `error`. `report.steps` and `report.hooks` keep
  the 0.3 shape (the slot phases and the lifecycle phases) and document
  themselves as the 0.3 views of the same run.
- `StepStatus::Failed`: only a [`DeployEventReport`] carries it. A failed entry
  is reported through `Reporter::handler_finished` and `report.events`; the run
  itself returns the error, as before.
- `Reporter::handler_started(phase, name, description)` and
  `Reporter::handler_finished(&DeployEventReport)`: a reporter can now follow
  the timeline phase by phase. The 0.3 `step_started` / `step_finished` are
  still called (by the built-in steps, and for handlers at a slot phase).
- `RunState::{warn, warnings}`: a non-fatal problem a handler records is
  forwarded to `Reporter::warning`, which `quiet` does not suppress.
- The install verification and the deployment stamp are entries at
  `Phase::AfterRotate` now, registered ahead of the caller's entries, instead of
  calls after the plan: they show up in a dry run (`after-rotate: verify
  install`, `after-rotate: write deploy stamp`), they report
  `Outcome::Skipped` when the rotation found the install current, and a
  caller-owned plan gets them too (`verify_install(true)` /
  `write_deploy_stamp(true)`). A plan without a rotation still verifies and
  stamps, like 0.3 did.
### Service entries on the timeline

- `service::ServiceStop` (default phase `AfterBuild`) and
  `service::ServiceStart` (default phase `Finally`): the
  `build -> stop service -> rotate -> start service` pair as library entries.
  `ServiceStop` remembers the instance it found (`RunState::set_stopped_service`),
  skips without touching the process when `install_is_current` says the rotation
  will find the install current, and otherwise calls `stop_service` (which
  refuses to kill a pid that is not the recorded image, and accepts a
  rotated-aside copy). `ServiceStart` keeps an instance that is still running,
  reuses the stopped instance's arguments (rewritten by a caller's
  `configure` closure), starts the binary, waits for the readiness probe when
  one is configured, and publishes the new `ServiceState` (pid, args, listen)
  through `RunState::set_service` - so a later entry, or the caller, reads the
  pid instead of parsing output.
- `pub fn install_is_current(&DeployContext) -> bool`: the public form of the
  rotation's skip condition, what a stop entry decides its skip on.
- `service/deploy.rs` and the `plan/phase.rs` split: `service.rs` and `plan.rs`
  became module directories for the planned layout.
### Workspace detection and `run_deploy_with`

- `WorkspaceRoot::detect(DetectHint)`: the root is detected in a documented
  order - the explicit `--workspace <dir>` wins, then the consumer's
  `CARGO_MANIFEST_DIR` (what `#[deploy_commands(...)]` bakes in; the library
  cannot supply it, because `env!` inside the library expands to the library's
  own directory), then the `CARGO_MANIFEST_DIR` the environment carries (set by
  `cargo run`, inherited by the queued child), then the nearest ancestor with a
  `[workspace]` manifest, and finally an error that names the flag.
- `--workspace <dir>` on `DeployCmdArgs`. The queued child gets
  `--workspace <root>` appended to its command line (unless it carries one
  already), so it resolves the same root wherever it is started, and a
  `--workspace` that contradicts the root the command was given is an error
  instead of a silent override.
- `run_deploy_with(args, root, packages, plan)`: the checkout deploy with a
  caller-owned plan - the queue branch, the state directory, the
  `queued ... (pid=...)` line, and the per-package loop stay in the library
  while the entries are the consumer's.
- The `deploy_commands(packages = [...])` dispatcher resolves the root through
  `WorkspaceRoot::detect` with the manifest directory baked in.
### Layout and report cleanup

- `plan.rs` is `plan/{mod,phase,steps,report}.rs` and `service.rs` is
  `service/{mod,deploy}.rs`: the plan engine (builder, timeline execution,
  locks, stamps) stays in `plan/mod.rs`, the built-in entries (`CargoBuildStep`,
  `RotateStep`, `RunStep`, the verification predicate) live in `plan/steps.rs`,
  and the report types plus the reporter live in `plan/report.rs`.
- `DeployReport` carries `events` only. `report.steps()` and `report.hooks()`
  are the 0.3 views (the slot phases, and the lifecycle phases) computed from
  it, and `DeployHookReport` is gone: a lifecycle entry's `succeeded` is
  `DeployEventReport::error.is_none()`. `Reporter::step_finished` still takes a
  `DeployStepReport` - that is what a built-in step reports.
- Still to come for 0.4.0: the consumer migrations (mcp-hub, codex-router,
  judo, zellij-plugins) and the release itself.

### No console window anywhere below a detached child

- `detach::spawn_detached` - what the `--background` deploy queue and the
  `service` helpers use - creates the child with `CREATE_NO_WINDOW` instead of
  `DETACHED_PROCESS`. A process with no console at all makes the first console
  program it starts allocate a new one, and Windows puts that console in a
  visible window: every queued deploy popped a console window over the desktop
  as soon as its plan started cargo - or any shell (a `pwsh`/`powershell` step
  showed up as that window). The replacement child owns a console without a
  window and hands it down to the programs it starts, so the whole subtree stays
  invisible; the process group, the breakaway and the `ERROR_ACCESS_DENIED`
  retry are unchanged.

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

