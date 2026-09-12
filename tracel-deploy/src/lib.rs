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

pub mod batch;
pub mod command;
pub mod context;
pub mod plan;
pub mod rotate;
pub mod service;
pub mod state;

pub use batch::{
    BatchOptions, BatchReport, DeployTarget, TargetReport, TargetStatus, batch_deploy,
};
pub use command::DeployCmdArgs;
pub use context::{
    DEFAULT_BUILD_INPUTS, DeployContext, DeployContextBuilder, DeployLock, FileFingerprint,
    Profile, build_inputs_fingerprint, build_inputs_fingerprint_with, content_matches,
    file_fingerprint,
};
pub use plan::{
    CargoBuildStep, DeployBuilder, DeployHook, DeployHookEntry, DeployHookReport, DeployPlan,
    DeployReport, DeployStep, DeployStepReport, NullReporter, Reporter, RotateStep, RunOutcome,
    RunStep, StdoutReporter, StepStatus, StepVerify, deploy,
};
pub use rotate::{
    BackupRetry, CleanupReport, DiscardedPending, PendingBackup, PendingDropReason, RotateOptions,
    RotateOutcome, RotateStrategy, cleanup_pending_backups, rotate,
};
pub use state::{
    read_state_file, remove_state_file, resolve_install_path, same_file_path, write_state_file,
};
pub use tracel_deploy_macros::deploy_commands;

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
        Instance, ProbeKind, ProbeReport, ProbeSpec, ServiceSpec, ServiceState, StopOutcome,
        StopReport, executable_path, find_running_instance, is_alive, probe_once, resolve_binary,
        start_and_wait_ready, start_detached, stop, stop_checked, stop_service, tail_log,
        wait_ready,
    };
    pub use crate::{
        BatchOptions, BatchReport, CargoBuildStep, DeployBuilder, DeployCmdArgs, DeployContext,
        DeployContextBuilder, DeployHook, DeployHookEntry, DeployHookReport, DeployLock,
        DeployPlan, DeployReport, DeployStep, DeployStepReport, DeployTarget, FileFingerprint,
        NullReporter, Profile, Reporter, RotateOptions, RotateOutcome, RotateStep, RotateStrategy,
        RunOutcome, RunStep, StdoutReporter, StepStatus, TargetReport, TargetStatus, batch_deploy,
        build_inputs_fingerprint, build_inputs_fingerprint_with, content_matches, deploy,
        file_fingerprint, read_state_file, remove_state_file, resolve_install_path, rotate,
        same_file_path, write_state_file,
    };
    pub use tracel_deploy_macros::deploy_commands;
    pub use tracel_xtask::prelude::{Context, Environment, XtaskArgs};
}
