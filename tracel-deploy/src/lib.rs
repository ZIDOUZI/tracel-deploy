//! Composable deployment helpers built on top of `tracel-xtask`.
//!
//! The crate has three layers:
//!
//! - [`DeployCmdArgs`] provides the common `--release`, `--dry-run`, package,
//!   binary, install path and backup arguments for an xtask deploy command.
//! - [`deploy`] returns a [`DeployBuilder`] with a default cargo-build and
//!   rotate plan. Callers can add `pre`/`post` hooks or arbitrary
//!   [`DeployStep`]s for service stop/start, verification, or migrations.
//! - [`rotate`] replaces a binary that may still be running. Windows renames
//!   the running file aside before copying the replacement; Linux and macOS
//!   use an atomic sibling rename so the running process keeps its old inode.
//! - [`background`] queues a redeploy into a detached child process
//!   (`--background` / `--child`), the shape a per-turn Codex `Stop` hook
//!   needs: the hook returns at once and the child builds behind it.
//! - [`run_deploy`] is the whole `deploy` command of a checkout that deploys
//!   what it contains: the [`DeployContext::for_checkout`] conventions, the
//!   queue, and the plan, in one call.
//!
//! The proc-macro [`deploy_commands`] mirrors `tracel_xtask_macros::base_commands`:
//! it adds a `Deploy` variant and optionally generates a
//! `handle_deploy_command` dispatcher.
//!
//! `deploy_commands` must be written *above* `base_commands`, because
//! `base_commands` rebuilds the enum and does not re-emit unknown outer
//! attribute macros. (The macro is aliased as `xtask_macros` in this doctest;
//! an xtask crate usually reaches it through `tracel_xtask::prelude` as
//! `macros`.) The reverse order fails to compile:
//!
//! ```compile_fail
//! use tracel_deploy::prelude::*;
//! use tracel_xtask::prelude::*;
//!
//! #[xtask_macros::base_commands(Build, Check, Test)]
//! #[tracel_deploy::deploy_commands(Deploy, handler = handle_deploy)]
//! pub enum Command {
//!     Custom(CustomArgs),
//! }
//!
//! #[derive(clap::Args, Clone, Debug)]
//! pub struct CustomArgs {
//!     #[arg(long)]
//!     pub name: Option<String>,
//! }
//!
//! fn handle_deploy(
//!     _args: tracel_deploy::DeployCmdArgs,
//!     _environment: Environment,
//!     _context: Context,
//! ) -> anyhow::Result<()> {
//!     Ok(())
//! }
//!
//! fn main() -> anyhow::Result<()> {
//!     let args = init_xtask::<Command>(parse_args::<Command>()?)?;
//!     match args.command {
//!         Command::Custom(_) => Ok(()),
//!         Command::Deploy(command) => {
//!             handle_deploy_command(command, args.environment, args.context)
//!         }
//!         _ => dispatch_base_commands(args),
//!     }
//! }
//! ```

pub mod background;
pub mod batch;
pub mod checkout;
pub mod command;
pub mod context;
mod detach;
pub mod plan;
pub mod rotate;
pub mod service;
pub mod state;
pub mod workspace;

pub use background::{
    DEPLOY_ERROR_LOG, DEPLOY_LOG, DeployQueueArgs, DeploymentLogs, child_argv,
    codex_stop_hook_command, codex_stop_hook_json, queue_deploy, queue_executable,
};
pub use batch::{
    BatchOptions, BatchReport, DeployTarget, TargetReport, TargetStatus, batch_deploy,
};
pub use checkout::{DeployOutcome, run_deploy};
pub use command::{DeployArgs, DeployCmdArgs};
pub use context::{
    DEFAULT_BUILD_INPUTS, DeployContext, DeployContextBuilder, DeployLock, FileFingerprint,
    Profile, STALE_LOCK_AFTER, build_inputs_fingerprint, build_inputs_fingerprint_with,
    checkout_state_dir, content_matches, file_fingerprint,
};
pub use plan::{
    CargoBuildStep, DeployBuilder, DeployHook, DeployPlan, DeployReport, DeployStep,
    DeployStepReport, Handler, NullReporter, Outcome, Phase, Reporter, RotateStep, RunOutcome,
    RunState, RunStep, StdoutReporter, StepStatus, StepVerify, deploy, install_is_current,
};
pub use rotate::{
    BackupRetry, CleanupReport, DiscardedPending, PendingBackup, PendingDropReason, RotateOptions,
    RotateOutcome, RotateStrategy, cleanup_pending_backups, rotate,
};
pub use state::{
    read_state_file, remove_state_file, resolve_install_path, same_file_path, write_state_file,
};
pub use tracel_deploy_macros::deploy_commands;
pub use workspace::{DetectHint, WorkspaceRoot};

#[doc(hidden)]
pub mod __private {
    pub use anyhow;
    pub use tracel_xtask;
}

pub mod prelude {
    pub use crate::context::DEFAULT_BUILD_INPUTS;
    pub use crate::rotate::{BackupRetry, CleanupReport, cleanup_pending_backups};
    #[cfg(feature = "service")]
    pub use crate::service::{
        Instance, InstanceFilter, ProbeKind, ProbeReport, ProbeSpec, ProcessInfo, ServiceSpec,
        ServiceStart, ServiceState, ServiceStop, StopOutcome, StopReport, executable_path,
        find_running_instance, is_alive, probe_once, resolve_binary, start_and_wait_ready,
        start_detached, stop, stop_checked, stop_service, tail_log, wait_ready,
    };
    pub use crate::{
        BatchOptions, BatchReport, CargoBuildStep, DEPLOY_ERROR_LOG, DEPLOY_LOG, DeployArgs,
        DeployBuilder, DeployCmdArgs, DeployContext, DeployContextBuilder, DeployHook, DeployLock,
        DeployOutcome, DeployPlan, DeployQueueArgs, DeployReport, DeployStep, DeployStepReport,
        DeployTarget, DeploymentLogs, FileFingerprint, Handler, NullReporter, Outcome, Phase,
        Profile, Reporter, RotateOptions, RotateOutcome, RotateStep, RotateStrategy, RunOutcome,
        RunState, RunStep, StdoutReporter, StepStatus, TargetReport, TargetStatus, batch_deploy,
        build_inputs_fingerprint, build_inputs_fingerprint_with, checkout_state_dir, child_argv,
        codex_stop_hook_command, codex_stop_hook_json, content_matches, deploy, file_fingerprint,
        queue_deploy, queue_executable, read_state_file, remove_state_file, resolve_install_path,
        rotate, run_deploy, same_file_path, write_state_file,
    };
    pub use tracel_deploy_macros::deploy_commands;
    pub use tracel_xtask::prelude::{Context, Environment, XtaskArgs};
}
