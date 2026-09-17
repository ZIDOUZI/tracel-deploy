//! One-call checkout deploys.
//!
//! Most repositories deploy what they contain: build this workspace's binary
//! (or binaries) and rotate it into the Cargo bin directory, skipping when
//! nothing changed. [`run_deploy`] is that whole command - the conventions of
//! [`DeployContext::for_checkout`], the `--background` queue, and the plan -
//! so the deploy command of an xtask is one call:
//!
//! ```ignore
//! #[tracel_deploy::deploy_commands(Deploy, packages = ["my-app"])]
//! #[macros::base_commands(Build, Check, Test)]
//! pub enum Command {}
//!
//! fn main() -> anyhow::Result<()> {
//!     let (args, environment) = init_xtask::<Command>(parse_args::<Command>()?)?;
//!     match args.command {
//!         Command::Deploy(command) => handle_deploy_command(command, environment, args.context),
//!         _ => dispatch_base_commands(args, environment),
//!     }
//! }
//! ```
//!
//! Without the macro, the handler is still three lines:
//!
//! ```ignore
//! fn handle_deploy(
//!     args: tracel_deploy::DeployArgs,
//!     _environment: Environment,
//!     _context: Context,
//! ) -> anyhow::Result<()> {
//!     let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask");
//!     tracel_deploy::run_deploy(&args, root, &["my-app"])?;
//!     Ok(())
//! }
//! ```
//!
//! Everything the library still has to be told is the list of packages: a
//! checkout whose binaries cannot be named (a workspace that builds targets
//! outside its own members, per-target install paths, artifacts built by
//! something other than cargo) wants the explicit
//! [`DeployContext`](crate::DeployContext) /
//! [`batch_deploy`](crate::batch_deploy) instead.

use std::{env, ffi::OsString, path::Path};

use anyhow::{Result, bail};

use crate::{
    background::{child_argv, queue_deploy},
    command::DeployArgs,
    context::{DeployContext, checkout_state_dir},
    plan::DeployReport,
    state::same_file_path,
};

/// What [`run_deploy`] did.
#[derive(Debug)]
pub enum DeployOutcome {
    /// `--background`: a detached child runs the deployment; the pid is the
    /// handle on it.
    Queued { pid: u32 },
    /// The plan ran (or the idempotent skip short-circuited it) in this
    /// process, one report per package, in the order they were asked for; a
    /// package whose install was already current reports a skipped rotation.
    Deployed(Vec<DeployReport>),
}

/// Deploys `packages` of one checkout, or queues that deployment.
///
/// One call replaces the context setup, the `--background` branch, the
/// `queued ... (pid=...)` line, and the plan run a consumer would otherwise
/// write by hand:
///
/// - `--background` (without `--child`, and never on a dry run) queues a
///   detached run of the same command through [`crate::queue_deploy`], with
///   `<state_dir>/deploy.log` and `<state_dir>/deploy-error.log` capturing the
///   child's output, prints `queued <packages> redeploy (pid=...)` unless
///   `--quiet`, and returns [`DeployOutcome::Queued`].
/// - Anything else deploys every package in this process through
///   [`DeployContext::for_checkout`] and the ordinary plan. The first failure
///   stops the run; [`batch_deploy`](crate::batch_deploy) is the variant that
///   reports every target instead.
///
/// `workspace_root` is the workspace the packages belong to (the `.codex` hook
/// pattern runs the xtask through `cargo run --manifest-path`, so the child gets
/// this as its working directory too).
///
/// Flags that name a single binary (`--package`, `--binary`, `--install-to`,
/// `--backup-dir`, `--source`) are rejected for more than one package: they
/// cannot say which package they belong to.
pub fn run_deploy(
    args: &DeployArgs,
    workspace_root: &Path,
    packages: &[&str],
) -> Result<DeployOutcome> {
    run_deploy_inner(
        args,
        workspace_root,
        packages,
        env::args_os().skip(1),
        &queue_deploy,
        &default_checkout_plan,
    )
}

/// [`run_deploy`] with a caller-owned plan.
///
/// `plan` turns a package's [`DeployContext`] into a [`DeployReport`]: the
/// checkout conventions stay the same, the entries are the caller's
/// (`defaults(false)` plus whatever the checkout needs - a service stop at
/// `AfterBuild`, a start at `Finally`, a check at `AfterRotate`). The queue
/// branch, the state directory, the `queued ... (pid=...)` line, and the
/// per-package loop still belong to the library:
///
/// ```ignore
/// tracel_deploy::run_deploy_with(&args, root, &["my-app"], |context| {
///     tracel_deploy::deploy(context)
///         .on(Phase::AfterBuild, ServiceStop::new(&spec))
///         .on(Phase::Finally, ServiceStart::new(&spec).probe(probe))
///         .run()
/// })?;
/// ```
pub fn run_deploy_with(
    args: &DeployArgs,
    workspace_root: &Path,
    packages: &[&str],
    plan: impl Fn(&DeployContext) -> Result<DeployReport>,
) -> Result<DeployOutcome> {
    run_deploy_inner(
        args,
        workspace_root,
        packages,
        env::args_os().skip(1),
        &queue_deploy,
        &plan,
    )
}

/// The default plan of a checkout deploy: the ordinary build + rotation (plus
/// the install verification and the deployment stamp the default plan adds).
fn default_checkout_plan(context: &DeployContext) -> Result<DeployReport> {
    crate::deploy(context).run()
}

fn run_deploy_inner(
    args: &DeployArgs,
    workspace_root: &Path,
    packages: &[&str],
    argv: impl IntoIterator<Item = OsString>,
    queue: &impl Fn(&Path, Option<&Path>, Vec<OsString>) -> Result<u32>,
    plan: &impl Fn(&DeployContext) -> Result<DeployReport>,
) -> Result<DeployOutcome> {
    if packages.is_empty() {
        bail!("run_deploy needs at least one package: name the binaries this checkout installs");
    }
    if packages.iter().any(|package| package.trim().is_empty()) {
        bail!("run_deploy got an empty package name");
    }
    if packages.len() > 1 {
        reject_single_binary_flags(args)?;
    }
    // The explicit root has to agree with the one the caller resolved: a
    // `--workspace` that names a different checkout is a mistake, not an
    // override (the macro passes the flag into `WorkspaceRoot::detect`).
    if let Some(explicit) = &args.common.workspace
        && !same_file_path(explicit, workspace_root)
    {
        bail!(
            "`--workspace {}` contradicts the workspace root {} this command was given",
            explicit.display(),
            workspace_root.display()
        );
    }

    if args.queue.should_queue(args.common.dry_run) {
        let state_dir = args
            .common
            .state_dir
            .clone()
            .unwrap_or_else(|| checkout_state_dir(workspace_root));
        let argv = with_workspace(child_argv(argv), workspace_root);
        let pid = queue(&state_dir, Some(workspace_root), argv)?;
        if !args.common.quiet {
            println!("queued {} redeploy (pid={pid})", packages.join(", "));
        }
        return Ok(DeployOutcome::Queued { pid });
    }

    let mut reports = Vec::with_capacity(packages.len());
    for package in packages {
        let context = DeployContext::for_checkout(&args.common, workspace_root, *package)?;
        reports.push(plan(&context)?);
    }
    Ok(DeployOutcome::Deployed(reports))
}

/// Add `--workspace <root>` to a queued child's command line unless it already
/// carries the flag.
///
/// The child then resolves the same root the parent did, no matter which
/// directory it happens to start in; the queue already starts it in the root,
/// this makes it explicit.
fn with_workspace(mut argv: Vec<OsString>, workspace_root: &Path) -> Vec<OsString> {
    let present = argv.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        arg == "--workspace" || arg.starts_with("--workspace=")
    });
    if !present {
        argv.push("--workspace".into());
        argv.push(workspace_root.as_os_str().to_os_string());
    }
    argv
}
fn reject_single_binary_flags(args: &DeployArgs) -> Result<()> {
    let named = [
        ("--package", args.common.package.is_some()),
        ("--binary", args.common.binary.is_some()),
        ("--install-to", args.common.install_to.is_some()),
        ("--backup-dir", args.common.backup_dir.is_some()),
        ("--source", args.common.source.is_some()),
    ];
    for (flag, set) in named {
        if set {
            bail!(
                "`{flag}` names one binary, so it cannot be used for several packages; \
                 deploy them one call at a time, or use batch_deploy with per-target values"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use clap::Parser as _;

    use super::*;

    /// `DeployArgs` as clap parses them for an xtask: the flags a hook passes.
    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        deploy: DeployArgs,
    }

    fn deploy_args(argv: &[&str]) -> DeployArgs {
        Cli::try_parse_from(argv).expect("deploy args parse").deploy
    }

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = env::temp_dir().join(format!("tracel-deploy-checkout-{label}-{unique}"));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_dry_run_renders_the_checkout_plan_in_this_process() {
        let root = temp_dir("dry-run");
        let args = deploy_args(&["xtask", "--dry-run", "--quiet"]);

        let outcome = run_deploy(&args, &root, &["my-app"]).expect("dry run");
        let DeployOutcome::Deployed(reports) = outcome else {
            panic!("a dry run never queues");
        };
        assert_eq!(reports.len(), 1);
        let plan = reports[0].plan.join("\n");
        assert!(
            plan.contains("build -p my-app") && plan.contains("--release"),
            "the checkout convention is a release build: {plan}"
        );
        assert!(
            plan.contains(&root.join("target").join("release").display().to_string()),
            "the build output comes from the workspace: {plan}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_background_invocation_queues_the_child_with_the_swapped_argv() {
        let root = temp_dir("queue");
        let args = deploy_args(&["xtask", "--background", "--force"]);
        let queued = RefCell::new(None);
        let queue = |state_dir: &Path, working_dir: Option<&Path>, argv: Vec<OsString>| {
            *queued.borrow_mut() = Some((
                state_dir.to_path_buf(),
                working_dir.map(Path::to_path_buf),
                argv,
            ));
            Ok(4242)
        };

        let outcome = run_deploy_inner(
            &args,
            &root,
            &["my-app"],
            ["deploy", "--background"].map(OsString::from),
            &queue,
            &default_checkout_plan,
        )
        .expect("queue");
        assert!(matches!(outcome, DeployOutcome::Queued { pid: 4242 }));

        let queued = queued.borrow();
        let (state_dir, working_dir, argv) = queued.as_ref().expect("queued once");
        assert_eq!(*state_dir, checkout_state_dir(&root));
        assert_eq!(working_dir.as_deref(), Some(root.as_path()));
        assert!(
            argv.contains(&OsString::from("--child"))
                && !argv.contains(&OsString::from("--background")),
            "the child deploys instead of queueing again: {argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "--workspace" && Path::new(&pair[1]) == root.as_path()),
            "the queued child is told which workspace to deploy: {argv:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// A caller-owned plan replaces the default one per package, and still gets
    /// the checkout conventions.
    #[test]
    fn a_caller_owned_plan_runs_for_every_package() {
        let root = temp_dir("custom-plan");
        let args = deploy_args(&["xtask", "--dry-run", "--quiet"]);
        let seen = RefCell::new(Vec::new());

        let outcome = run_deploy_with(&args, &root, &["server", "worker"], |context| {
            seen.borrow_mut().push(context.package.clone());
            crate::deploy(context).run()
        })
        .expect("the caller-owned plan runs");

        let DeployOutcome::Deployed(reports) = outcome else {
            panic!("a dry run never queues");
        };
        assert_eq!(reports.len(), 2);
        assert_eq!(*seen.borrow(), ["server", "worker"]);
        fs::remove_dir_all(&root).ok();
    }

    /// An explicit `--workspace` that names a different checkout is a mistake:
    /// the resolved root and the flag have to agree.
    #[test]
    fn a_workspace_flag_naming_another_checkout_is_rejected() {
        let root = temp_dir("root");
        let other = temp_dir("other");
        let other = other.to_str().expect("utf-8 path");
        let args = deploy_args(&["xtask", "--dry-run", "--workspace", other]);

        let error = run_deploy(&args, &root, &["my-app"]).expect_err("the roots disagree");
        assert!(
            format!("{error:#}").contains("contradicts the workspace root"),
            "error: {error:#}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn every_package_gets_its_own_report() {
        let root = temp_dir("two");
        let args = deploy_args(&["xtask", "--dry-run", "--quiet"]);

        let outcome = run_deploy(&args, &root, &["server", "worker"]).expect("dry run");
        let DeployOutcome::Deployed(reports) = outcome else {
            panic!("a dry run never queues");
        };
        assert_eq!(reports.len(), 2);
        assert!(reports[0].plan.join("\n").contains("build -p server"));
        assert!(reports[1].plan.join("\n").contains("build -p worker"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn single_binary_flags_are_rejected_for_several_packages() {
        let root = temp_dir("flags");
        let args = deploy_args(&["xtask", "--dry-run", "--install-to", "C:/tmp/my-app.exe"]);

        let error = run_deploy(&args, &root, &["server", "worker"]).expect_err("ambiguous flag");
        assert!(error.to_string().contains("--install-to"), "{error}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_empty_package_list_is_an_error() {
        let args = deploy_args(&["xtask"]);
        let error = run_deploy(&args, Path::new("C:/tmp/none"), &[]).expect_err("no packages");
        assert!(
            error.to_string().contains("at least one package"),
            "{error}"
        );
    }
}
