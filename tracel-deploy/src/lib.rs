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

pub mod command;
pub mod context;
pub mod plan;
pub mod rotate;

pub use command::DeployCmdArgs;
pub use context::{
    DeployContext, DeployContextBuilder, DeployLock, FileFingerprint, Profile, content_matches,
    file_fingerprint,
};
pub use plan::{
    CargoBuildStep, DeployBuilder, DeployHook, DeployHookEntry, DeployHookReport, DeployPlan,
    DeployReport, DeployStep, DeployStepReport, RotateStep, RunStep, StepStatus, deploy,
};
pub use rotate::{RotateOptions, RotateOutcome, RotateStrategy, rotate};
pub use tracel_deploy_macros::deploy_commands;

#[doc(hidden)]
pub mod __private {
    pub use anyhow;
    pub use tracel_xtask;
}

pub mod prelude {
    pub use crate::{
        CargoBuildStep, DeployBuilder, DeployCmdArgs, DeployContext, DeployContextBuilder,
        DeployHook, DeployHookEntry, DeployHookReport, DeployLock, DeployPlan, DeployReport,
        DeployStep, DeployStepReport, FileFingerprint, Profile, RotateOptions, RotateOutcome,
        RotateStep, RotateStrategy, RunStep, StepStatus, content_matches, deploy, file_fingerprint,
        rotate,
    };
    pub use tracel_deploy_macros::deploy_commands;
    pub use tracel_xtask::prelude::{Context, Environment, XtaskArgs};
}
