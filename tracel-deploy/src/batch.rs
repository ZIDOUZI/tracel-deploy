//! Multi-target batch deployment.
//!
//! [`batch_deploy`] deploys several binaries of one workspace in a single
//! call. Every [`DeployTarget`] builds its own [`DeployContext`], so targets
//! cannot leak their overrides into each other, and each one runs the ordinary
//! [`deploy`](crate::deploy) plan: the default cargo-build + rotate pair with
//! the same lock, idempotent skip, verification, and deployment stamp a
//! single-target deploy has. Nothing about the plan is re-implemented here.
//!
//! Failures are collected instead of aborting the call: the default is
//! continue-on-error, and [`BatchOptions::fail_fast`] stops after the first
//! failure. The remaining targets are then reported as
//! [`TargetStatus::NotRun`], which is *not* the same as
//! [`TargetStatus::Skipped`]: a skip means the idempotent short-circuit found
//! nothing left to do, a not-run target was never touched.
//!
//! ```ignore
//! let targets = [
//!     DeployTarget::new("server").binary("server"),
//!     DeployTarget::new("worker").source(built_worker),
//! ];
//! let report = batch_deploy(&workspace, &targets, BatchOptions::default())?;
//! if !report.is_success() {
//!     for failure in report.failures() {
//!         eprintln!(
//!             "{}: {}",
//!             failure.package,
//!             failure.error().unwrap_or("was not run")
//!         );
//!     }
//! }
//! ```

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::{
    context::DeployContext,
    plan::{DeployReport, StepStatus, deploy},
};

/// One target of a [`batch_deploy`] call: a package plus the per-target
/// overrides that are global flags for a single deploy.
///
/// Only the fields a caller has to be able to vary per package live here; the
/// knobs shared by the whole batch (dry-run, lock/state directory, cargo
/// executable) are part of [`BatchOptions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployTarget {
    /// Cargo package to build (`cargo build -p <package>`).
    pub package: String,
    /// Binary name; defaults to `package`.
    pub binary: Option<String>,
    /// Installed binary path; defaults to the cargo bin directory plus the
    /// binary name, exactly like [`DeployContext::new`].
    pub install_to: Option<PathBuf>,
    /// Directory that holds the rotated-out backups.
    pub backup_dir: Option<PathBuf>,
    /// Replace the built binary with this file (skips nothing: the build step
    /// still runs).
    pub source: Option<PathBuf>,
    /// Build with `--release`.
    pub release: bool,
    /// Custom cargo profile (`--profile <name>`), overrides `release`.
    pub profile: Option<String>,
}

impl DeployTarget {
    pub fn new(package: impl Into<String>) -> Self {
        Self {
            package: package.into(),
            binary: None,
            install_to: None,
            backup_dir: None,
            source: None,
            release: false,
            profile: None,
        }
    }

    pub fn binary(mut self, name: impl Into<String>) -> Self {
        self.binary = Some(name.into());
        self
    }

    pub fn install_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.install_to = Some(path.into());
        self
    }

    pub fn backup_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.backup_dir = Some(path.into());
        self
    }

    pub fn source(mut self, path: impl Into<PathBuf>) -> Self {
        self.source = Some(path.into());
        self
    }

    pub fn release(mut self, release: bool) -> Self {
        self.release = release;
        self
    }

    pub fn profile(mut self, name: impl Into<String>) -> Self {
        self.profile = Some(name.into());
        self
    }

    /// The [`DeployContext`] this target deploys with.
    ///
    /// This is the exact context [`batch_deploy`] uses, so callers can inspect
    /// (or deploy) a target themselves with the same result.
    pub fn context(&self, workspace_root: &Path, options: &BatchOptions) -> DeployContext {
        let mut builder = DeployContext::builder(workspace_root, self.package.clone())
            .quiet(options.quiet.unwrap_or(true))
            .release(self.release)
            .dry_run(options.dry_run)
            .force(options.force);
        if let Some(binary) = &self.binary {
            builder = builder.binary(binary.clone());
        }
        if let Some(install_to) = &self.install_to {
            builder = builder.install_to(install_to.clone());
        }
        if let Some(backup_dir) = &self.backup_dir {
            builder = builder.backup_dir(backup_dir.clone());
        }
        if let Some(source) = &self.source {
            builder = builder.source(source.clone());
        }
        if let Some(profile) = &self.profile {
            builder = builder.profile(profile.clone());
        }
        if let Some(state_dir) = &options.state_dir {
            builder = builder.state_dir(state_dir.clone());
        }
        if let Some(cargo) = &options.cargo {
            builder = builder.cargo(cargo.clone());
        }
        builder.build()
    }
}

/// Knobs shared by every target of one [`batch_deploy`] call.
///
/// The defaults match a bare [`DeployContext::new`] (quiet, debug profile, no
/// dry-run, no force), so `BatchOptions::default()` adds nothing to what a
/// single deploy would do.
#[derive(Debug, Clone, Default)]
pub struct BatchOptions {
    /// Stop after the first failing target; the remaining targets are reported
    /// as [`TargetStatus::NotRun`]. Defaults to continue-on-error.
    pub fail_fast: bool,
    /// Render the plan of every target without touching any file.
    pub dry_run: bool,
    /// Deploy even when the deployment stamp says nothing changed.
    pub force: bool,
    /// Print progress; `None` keeps the [`DeployContext`] default (quiet).
    pub quiet: Option<bool>,
    /// Directory for the deployment locks and stamps of every target.
    pub state_dir: Option<PathBuf>,
    /// Cargo executable override for the build step of every target.
    pub cargo: Option<PathBuf>,
}

impl BatchOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fail_fast(mut self, fail_fast: bool) -> Self {
        self.fail_fast = fail_fast;
        self
    }

    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub fn force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    pub fn quiet(mut self, quiet: bool) -> Self {
        self.quiet = Some(quiet);
        self
    }

    pub fn state_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(path.into());
        self
    }

    pub fn cargo(mut self, path: impl Into<PathBuf>) -> Self {
        self.cargo = Some(path.into());
        self
    }
}

/// Completion state of one target of a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetStatus {
    /// The plan ran to the end (including a dry run, which only rendered it).
    Succeeded,
    /// The plan failed; the payload is the full error chain.
    Failed { error: String },
    /// The idempotent short-circuit found nothing to do.
    Skipped,
    /// The target was never started because an earlier target failed under
    /// [`BatchOptions::fail_fast`].
    NotRun,
}

impl TargetStatus {
    /// Whether nothing failed: both a deploy that ran and a skipped one are
    /// successes, a failure and a not-run target are not.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Skipped)
    }

    pub fn is_failure(&self) -> bool {
        !self.is_success()
    }
}

/// Result of one target of a batch.
#[derive(Debug)]
pub struct TargetReport {
    pub package: String,
    /// Installed binary path of the target, whether it ran or not.
    pub installed: PathBuf,
    pub status: TargetStatus,
    /// The deploy report of a target that ran; `None` for
    /// [`TargetStatus::Failed`] and [`TargetStatus::NotRun`].
    pub deploy: Option<DeployReport>,
}

impl TargetReport {
    /// The failure reason, when the target failed.
    pub fn error(&self) -> Option<&str> {
        match &self.status {
            TargetStatus::Failed { error } => Some(error),
            _ => None,
        }
    }

    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }
}

/// What a [`batch_deploy`] call did, one entry per requested target and in
/// request order.
#[derive(Debug, Default)]
pub struct BatchReport {
    pub targets: Vec<TargetReport>,
}

impl BatchReport {
    /// True when no target failed and none was left unstarted.
    pub fn is_success(&self) -> bool {
        self.targets.iter().all(TargetReport::is_success)
    }

    /// Failed and not-run targets, in request order.
    pub fn failures(&self) -> impl Iterator<Item = &TargetReport> {
        self.targets
            .iter()
            .filter(|target| target.status.is_failure())
    }
}

/// Deploy every target of `targets` against `workspace_root`.
///
/// Each target is deployed independently - its own [`DeployContext`], its own
/// lock, its own stamp - through [`deploy`](crate::deploy), so a single target
/// behaves exactly like a direct `deploy(&context).run()` call. Failures of
/// individual targets are recorded in the returned [`BatchReport`] instead of
/// failing the batch; the `Result` only carries a pre-flight error that
/// prevented the batch from starting at all (an empty package or binary name).
pub fn batch_deploy(
    workspace_root: &Path,
    targets: &[DeployTarget],
    options: BatchOptions,
) -> Result<BatchReport> {
    for (index, target) in targets.iter().enumerate() {
        if target.package.trim().is_empty() {
            anyhow::bail!("batch target #{index} has an empty package name");
        }
        if target
            .binary
            .as_deref()
            .is_some_and(|binary| binary.trim().is_empty())
        {
            anyhow::bail!(
                "batch target #{index} ({}) has an empty binary name",
                target.package
            );
        }
    }

    let mut report = BatchReport {
        targets: Vec::with_capacity(targets.len()),
    };
    let mut aborted = false;
    for target in targets {
        let context = target.context(workspace_root, &options);
        if aborted {
            report.targets.push(TargetReport {
                package: target.package.clone(),
                installed: context.install_path,
                status: TargetStatus::NotRun,
                deploy: None,
            });
            continue;
        }
        match deploy(&context).run() {
            Ok(deploy_report) => {
                let status = status_of(&deploy_report);
                report.targets.push(TargetReport {
                    package: target.package.clone(),
                    installed: deploy_report.installed.clone(),
                    status,
                    deploy: Some(deploy_report),
                });
            }
            Err(error) => {
                report.targets.push(TargetReport {
                    package: target.package.clone(),
                    installed: context.install_path.clone(),
                    status: TargetStatus::Failed {
                        error: format!("{error:#}"),
                    },
                    deploy: None,
                });
                aborted = options.fail_fast;
            }
        }
    }
    Ok(report)
}

/// A deploy report with nothing to install is the idempotent short-circuit:
/// either every step was skipped (the opt-in pre-build comparison), or the
/// rotation found the installed binary already equal to the built one (the
/// default, where cargo decides whether there is anything to build).
fn status_of(report: &DeployReport) -> TargetStatus {
    let skipped = !report.steps.is_empty()
        && report
            .steps
            .iter()
            .all(|step| step.status == StepStatus::Skipped);
    let rotation_skipped = report
        .steps
        .iter()
        .any(|step| step.name == "rotate" && step.status == StepStatus::Skipped);
    if skipped || rotation_skipped {
        TargetStatus::Skipped
    } else {
        TargetStatus::Succeeded
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{Duration, SystemTime},
    };

    use super::{BatchOptions, DeployTarget, TargetStatus, batch_deploy};
    use crate::{
        context::{binary_name_with_extension, default_cargo_bin_dir},
        plan::{DeployReport, StepStatus, deploy},
    };

    /// Temp directory with a unique name for one test.
    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "tracel-deploy-batch-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent dir");
        }
        fs::write(path, contents).expect("write file");
    }

    /// A `cargo` stand-in that succeeds without building anything, so the
    /// default plan of a fake workspace can run its build step.
    fn fake_cargo(dir: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            // `Command::new` runs `.cmd` files through `cmd.exe`.
            let path = dir.join("fake-cargo.cmd");
            write_file(&path, "@echo off\r\nexit /b 0\r\n");
            path
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let path = dir.join("fake-cargo");
            write_file(&path, "#!/bin/sh\nexit 0\n");
            let mut permissions = fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).expect("chmod");
            path
        }
    }

    /// A fake workspace with a source binary, a destination and a build that
    /// succeeds, so the default build + rotate plan can actually run.
    struct Fixture {
        dir: PathBuf,
        cargo: PathBuf,
        source: PathBuf,
        destination: PathBuf,
    }

    fn fixture(label: &str, contents: &str) -> Fixture {
        let dir = temp_dir(label);
        let cargo = fake_cargo(&dir);
        let source = dir.join("source-binary");
        write_file(&source, contents);
        Fixture {
            destination: dir.join("installed-binary"),
            dir,
            cargo,
            source,
        }
    }

    impl Fixture {
        fn target(&self, package: &str) -> DeployTarget {
            DeployTarget::new(package)
                .binary(package)
                .source(&self.source)
                .install_to(&self.destination)
        }

        fn options(&self) -> BatchOptions {
            BatchOptions::new()
                .state_dir(self.dir.join("state"))
                .cargo(&self.cargo)
        }

        fn installed(&self) -> String {
            fs::read_to_string(&self.destination).expect("installed binary")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Step names and statuses: the part of a report both deploy paths must
    /// agree on.
    fn steps(report: &DeployReport) -> Vec<(String, StepStatus)> {
        report
            .steps
            .iter()
            .map(|step| (step.name.clone(), step.status))
            .collect()
    }

    #[test]
    fn two_targets_both_deploy() {
        let first = fixture("two-first", "binary a");
        let second = fixture("two-second", "binary b");
        let targets = [first.target("pkg-a"), second.target("pkg-b")];

        let report = batch_deploy(&first.dir, &targets, first.options()).expect("batch deploy");

        assert_eq!(report.targets.len(), 2);
        assert!(report.is_success(), "report: {report:?}");
        assert_eq!(report.failures().count(), 0);
        for (target, fixture) in report.targets.iter().zip([&first, &second]) {
            assert_eq!(target.status, TargetStatus::Succeeded);
            assert_eq!(target.installed, fixture.destination);
            assert_eq!(
                fixture.installed(),
                fs::read_to_string(&fixture.source).expect("source")
            );
            // Every target ran the default build + rotate plan, not a copy of it.
            let deploy = target
                .deploy
                .as_ref()
                .expect("an executed target reports its steps");
            assert_eq!(
                steps(deploy)
                    .iter()
                    .map(|step| step.0.as_str())
                    .collect::<Vec<_>>(),
                ["cargo build", "rotate"]
            );
        }
    }

    #[test]
    fn a_failing_target_does_not_stop_the_rest() {
        let broken = fixture("continue-broken", "never deployed");
        let good = fixture("continue-good", "binary b");
        let broken_target = broken
            .target("pkg-broken")
            .source(broken.dir.join("missing-source"));
        let targets = [broken_target, good.target("pkg-good")];

        let report = batch_deploy(&broken.dir, &targets, broken.options()).expect("batch deploy");

        assert!(!report.is_success());
        assert!(matches!(
            report.targets[0].status,
            TargetStatus::Failed { .. }
        ));
        assert!(!broken.destination.exists());
        let error = report.targets[0]
            .error()
            .expect("a failure carries a reason");
        assert!(error.contains("rotate"), "error: {error}");
        assert!(report.targets[0].deploy.is_none());
        assert_eq!(report.targets[0].installed, broken.destination);

        // The remaining target still deployed.
        assert_eq!(report.targets[1].status, TargetStatus::Succeeded);
        assert_eq!(good.installed(), "binary b");
        assert_eq!(report.failures().count(), 1);
    }

    #[test]
    fn fail_fast_marks_the_unexecuted_targets_not_run() {
        let broken = fixture("fail-fast-broken", "never deployed");
        let second = fixture("fail-fast-second", "binary b");
        let third = fixture("fail-fast-third", "binary c");
        let broken_target = broken
            .target("pkg-broken")
            .source(broken.dir.join("missing-source"));
        let targets = [
            broken_target,
            second.target("pkg-second"),
            third.target("pkg-third"),
        ];

        let report = batch_deploy(&broken.dir, &targets, broken.options().fail_fast(true))
            .expect("batch deploy");

        assert!(matches!(
            report.targets[0].status,
            TargetStatus::Failed { .. }
        ));
        for target in &report.targets[1..] {
            assert_eq!(target.status, TargetStatus::NotRun);
            assert_ne!(target.status, TargetStatus::Skipped);
            assert!(target.deploy.is_none());
        }
        assert_eq!(report.targets[1].installed, second.destination);
        assert_eq!(report.targets[2].installed, third.destination);
        assert!(
            !second.destination.exists(),
            "a not-run target was deployed"
        );
        assert!(!third.destination.exists(), "a not-run target was deployed");
        assert_eq!(
            report.failures().count(),
            3,
            "the failed target plus the two not-run ones"
        );
        assert!(!report.is_success());
    }

    #[test]
    fn a_second_batch_run_skips_unchanged_targets() {
        let fixture = fixture("idempotent", "binary a");
        let targets = [fixture.target("pkg-a")];
        let options = fixture.options();

        let first = batch_deploy(&fixture.dir, &targets, options.clone()).expect("first run");
        assert_eq!(first.targets[0].status, TargetStatus::Succeeded);
        assert_eq!(fixture.installed(), "binary a");

        // Age the installed binary: a rewrite would move its timestamp forward.
        let aged = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let installed = fs::File::options()
            .write(true)
            .open(&fixture.destination)
            .expect("open installed binary");
        installed
            .set_times(fs::FileTimes::new().set_modified(aged))
            .expect("age the installed binary");
        drop(installed);
        let before = fs::metadata(&fixture.destination)
            .expect("metadata")
            .modified()
            .expect("mtime");
        assert!(
            before < SystemTime::now() - Duration::from_secs(86_400),
            "the installed binary must carry the aged timestamp"
        );

        let second = batch_deploy(&fixture.dir, &targets, options).expect("second run");

        assert_eq!(second.targets[0].status, TargetStatus::Skipped);
        assert!(second.is_success(), "a skipped target is not a failure");
        let deploy = second.targets[0]
            .deploy
            .as_ref()
            .expect("a skipped target reports its steps");
        assert!(
            deploy
                .steps
                .iter()
                .any(|step| step.name == "rotate" && step.status == StepStatus::Skipped),
            "nothing was installed, so the rotation is skipped: {deploy:?}"
        );
        let after = fs::metadata(&fixture.destination)
            .expect("metadata")
            .modified()
            .expect("mtime");
        assert_eq!(
            after, before,
            "a skipped deploy must not rewrite the binary"
        );
        assert_eq!(fixture.installed(), "binary a");
    }

    #[test]
    fn a_single_target_matches_a_direct_deploy() {
        let direct = fixture("parity-direct", "same bytes");
        let batched = fixture("parity-batch", "same bytes");

        let target = direct.target("pkg-a");
        let context = target.context(&direct.dir, &direct.options());
        let direct_report = deploy(&context).run().expect("direct deploy");

        let batch_report =
            batch_deploy(&batched.dir, &[batched.target("pkg-a")], batched.options())
                .expect("batch deploy");

        assert_eq!(batch_report.targets[0].status, TargetStatus::Succeeded);
        let batched_report = batch_report.targets[0].deploy.as_ref().expect("report");
        assert_eq!(steps(&direct_report), steps(batched_report));
        assert_eq!(context.install_path, direct.destination);
        assert_eq!(batched_report.installed, batched.destination);
        assert_eq!(direct.installed(), batched.installed());
        // Both paths wrote the deployment stamp that the skip below relies on.
        assert!(direct.dir.join("state").join("pkg-a.stamp").is_file());
        assert!(batched.dir.join("state").join("pkg-a.stamp").is_file());

        // The idempotent short-circuit behaves the same on both paths.
        let direct_again = deploy(&context).run().expect("second direct deploy");
        assert!(
            direct_again
                .steps
                .iter()
                .any(|step| step.name == "rotate" && step.status == StepStatus::Skipped),
            "the install is already current: {direct_again:?}"
        );
        let batch_again = batch_deploy(&batched.dir, &[batched.target("pkg-a")], batched.options())
            .expect("second batch deploy");
        assert_eq!(batch_again.targets[0].status, TargetStatus::Skipped);
        let batch_again_report = batch_again.targets[0].deploy.as_ref().expect("report");
        assert_eq!(steps(&direct_again), steps(batch_again_report));
    }

    #[test]
    fn per_target_overrides_reach_the_context_and_the_plan() {
        let dir = temp_dir("overrides");
        let cargo = fake_cargo(&dir);
        let source = dir.join("source-binary");
        write_file(&source, "new bytes");
        let destination = dir.join("installed-binary");
        let backup_dir = dir.join("backups");

        let target = DeployTarget::new("pkg-a")
            .binary("custom-bin")
            .install_to(&destination)
            .backup_dir(&backup_dir)
            .source(&source)
            .release(true)
            .profile("dist");

        let context = target.context(&dir, &BatchOptions::new());
        assert_eq!(context.package, "pkg-a");
        assert_eq!(context.binary_name, "custom-bin");
        assert_eq!(context.install_path, destination);
        assert_eq!(context.backup_dir.as_deref(), Some(backup_dir.as_path()));
        assert_eq!(context.source_binary.as_deref(), Some(source.as_path()));
        assert!(context.release);
        assert_eq!(context.profile_name.as_deref(), Some("dist"));

        // Without `install_to`, the default install path follows `binary`,
        // exactly like `DeployContext::new` does.
        let default_context = DeployTarget::new("pkg-a")
            .binary("custom-bin")
            .context(&dir, &BatchOptions::new());
        assert_eq!(
            default_context.install_path,
            default_cargo_bin_dir().join(binary_name_with_extension("custom-bin"))
        );

        // The overrides reach the default plan, which a dry run renders.
        let report = batch_deploy(
            &dir,
            &[target],
            BatchOptions::new()
                .state_dir(dir.join("state"))
                .cargo(&cargo)
                .dry_run(true),
        )
        .expect("dry run");
        let plan = report.targets[0]
            .deploy
            .as_ref()
            .expect("dry-run report")
            .plan
            .join("\n");
        assert!(
            plan.contains("build -p pkg-a --color always --profile dist"),
            "the build step must use the package and profile override: {plan}"
        );
        for expected in [
            source.display().to_string(),
            destination.display().to_string(),
            backup_dir.display().to_string(),
        ] {
            assert!(
                plan.contains(&expected),
                "plan must mention {expected}: {plan}"
            );
        }
        assert!(!destination.exists(), "a dry run must not install anything");
        assert!(
            !source.exists() || source.is_file(),
            "the source is untouched"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
