use std::{
    fs,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};

use crate::{
    context::{DeployContext, DeployLock, content_matches, file_fingerprint},
    rotate::{RotateOptions, RotateOutcome, RotateStrategy, rotate},
};

pub type DeployHook = Box<dyn FnMut(&DeployContext) -> Result<()>>;

/// A hook plus the optional name used in errors and reports.
pub struct DeployHookEntry {
    pub name: Option<String>,
    pub hook: DeployHook,
}

impl DeployHookEntry {
    fn new(name: Option<String>, hook: DeployHook) -> Self {
        Self { name, hook }
    }

    fn label(&self, phase: &str, index: usize) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("{phase} hook #{}", index + 1))
    }
}

/// A composable deployment plan.
///
/// Execution order:
///
/// ```text
/// pre hooks -> steps -> post hooks -> finally hooks
/// ```
///
/// On success every phase runs in order. On failure the failing phase stops,
/// `post` hooks are skipped, and the `on_error` and `finally` hooks still run.
/// `finally` hooks always run (also when `on_error` fails) and every failure is
/// aggregated, with the original error kept first in the chain.
pub struct DeployPlan {
    pub pre: Vec<DeployHookEntry>,
    pub steps: Vec<DeployStep>,
    pub post: Vec<DeployHookEntry>,
    pub on_error: Vec<DeployHookEntry>,
    pub finally: Vec<DeployHookEntry>,
}

impl DeployPlan {
    pub fn new() -> Self {
        Self {
            pre: Vec::new(),
            steps: Vec::new(),
            post: Vec::new(),
            on_error: Vec::new(),
            finally: Vec::new(),
        }
    }

    /// The default build + rotate steps for `context`.
    pub fn standard(context: &DeployContext) -> Vec<DeployStep> {
        vec![
            DeployStep::CargoBuild(CargoBuildStep::from_context(context)),
            DeployStep::Rotate(RotateStep::from_context(context)),
        ]
    }

    /// One line per step describing what would run (used by dry-run and
    /// reports).
    pub fn render(&self, context: &DeployContext) -> Vec<String> {
        self.steps
            .iter()
            .map(|step| step.describe(context))
            .collect()
    }

    pub fn run(self, context: &DeployContext) -> Result<DeployReport> {
        let mut report = DeployReport {
            installed: context.install_path.clone(),
            dry_run: context.dry_run,
            plan: Vec::new(),
            steps: Vec::new(),
            hooks: Vec::new(),
        };
        if context.dry_run {
            report.plan = self.render(context);
            if !context.quiet {
                for line in &report.plan {
                    println!("would: {line}");
                }
            }
            return Ok(report);
        }

        let Self {
            pre,
            steps,
            post,
            on_error,
            finally,
        } = self;
        let mut failures = Vec::new();

        run_hook_phase("pre", pre, context, &mut report, &mut failures);
        let mut failed = !failures.is_empty();

        if !failed {
            for step in steps {
                match step.run(context) {
                    Ok(step_report) => report.steps.push(step_report),
                    Err(error) => {
                        failures.push(error);
                        failed = true;
                        break;
                    }
                }
            }
        }

        if !failed {
            run_hook_phase("post", post, context, &mut report, &mut failures);
            failed = !failures.is_empty();
        }

        if failed {
            run_hook_phase("on_error", on_error, context, &mut report, &mut failures);
        }
        run_hook_phase("finally", finally, context, &mut report, &mut failures);

        let mut failures = failures.into_iter();
        if let Some(mut aggregated) = failures.next() {
            for extra in failures {
                aggregated = aggregated.context(format!("additional failure: {extra:#}"));
            }
            return Err(aggregated);
        }
        Ok(report)
    }
}

fn run_hook_phase(
    phase: &str,
    hooks: Vec<DeployHookEntry>,
    context: &DeployContext,
    report: &mut DeployReport,
    failures: &mut Vec<anyhow::Error>,
) {
    for (index, mut entry) in hooks.into_iter().enumerate() {
        let label = entry.label(phase, index);
        match (entry.hook)(context) {
            Ok(()) => report.hooks.push(DeployHookReport {
                name: label,
                succeeded: true,
                error: None,
            }),
            Err(error) => {
                let error = error.context(format!("{label} failed"));
                report.hooks.push(DeployHookReport {
                    name: label,
                    succeeded: false,
                    error: Some(format!("{error:#}")),
                });
                failures.push(error);
            }
        }
    }
}

impl Default for DeployPlan {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder returned by [`deploy`].
///
/// By default it runs:
///
/// 1. [`CargoBuildStep`] for `context.package`.
/// 2. [`RotateStep`] from `context.built_binary_path()` to
///    `context.install_path`.
///
/// Use [`DeployBuilder::without_defaults`] to fully replace the default plan.
pub struct DeployBuilder<'a> {
    context: &'a DeployContext,
    plan: DeployPlan,
    defaults: bool,
    insert_after_build: Vec<DeployStep>,
    insert_before_rotate: Vec<DeployStep>,
}

impl<'a> DeployBuilder<'a> {
    fn new(context: &'a DeployContext) -> Self {
        Self {
            context,
            plan: DeployPlan::new(),
            defaults: true,
            insert_after_build: Vec::new(),
            insert_before_rotate: Vec::new(),
        }
    }

    /// Add an unnamed hook that runs before the deployment steps. The usual
    /// use is stopping a service that owns the installed binary.
    pub fn pre<F>(self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.pre_named_entry(None, Box::new(hook))
    }

    /// Add a named pre hook; the name is used in errors and reports.
    pub fn pre_named<F>(self, name: impl Into<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.pre_named_entry(Some(name.into()), Box::new(hook))
    }

    fn pre_named_entry(mut self, name: Option<String>, hook: DeployHook) -> Self {
        self.plan.pre.push(DeployHookEntry::new(name, hook));
        self
    }

    /// Add an unnamed hook that runs after all deployment steps succeeded. The
    /// usual use is restarting or verifying a service.
    pub fn post<F>(self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.post_named_entry(None, Box::new(hook))
    }

    /// Add a named post hook; the name is used in errors and reports.
    pub fn post_named<F>(self, name: impl Into<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.post_named_entry(Some(name.into()), Box::new(hook))
    }

    fn post_named_entry(mut self, name: Option<String>, hook: DeployHook) -> Self {
        self.plan.post.push(DeployHookEntry::new(name, hook));
        self
    }

    /// Add an unnamed hook that runs when a pre hook, a step, or a post hook
    /// failed. Use it for rollbacks, logging, or alerting. The `finally` hooks
    /// run after `on_error`, even when an `on_error` hook fails itself.
    pub fn on_error<F>(self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.on_error_named_entry(None, Box::new(hook))
    }

    /// Add a named `on_error` hook; the name is used in errors and reports.
    pub fn on_error_named<F>(self, name: impl Into<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.on_error_named_entry(Some(name.into()), Box::new(hook))
    }

    fn on_error_named_entry(mut self, name: Option<String>, hook: DeployHook) -> Self {
        self.plan.on_error.push(DeployHookEntry::new(name, hook));
        self
    }

    /// Add an unnamed hook that always runs: after `post` on success, or after
    /// `on_error` on failure. The usual use is restarting a service that was
    /// stopped before the rotation, even when the rotation failed.
    pub fn finally<F>(self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.finally_named_entry(None, Box::new(hook))
    }

    /// Add a named `finally` hook; the name is used in errors and reports.
    pub fn finally_named<F>(self, name: impl Into<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.finally_named_entry(Some(name.into()), Box::new(hook))
    }

    fn finally_named_entry(mut self, name: Option<String>, hook: DeployHook) -> Self {
        self.plan.finally.push(DeployHookEntry::new(name, hook));
        self
    }

    /// Append a custom step after the defaults (or as the only step when
    /// [`without_defaults`](Self::without_defaults) is used).
    pub fn step(mut self, step: DeployStep) -> Self {
        self.plan.steps.push(step);
        self
    }

    /// Insert a step between the default cargo build and the default rotation.
    ///
    /// The classic use case is `build -> stop service -> rotate -> start
    /// service`: stopping a service in a `pre` hook would stop it before the
    /// build, so a failed build keeps the service down unnecessarily.
    pub fn insert_after_default_build(mut self, step: DeployStep) -> Self {
        self.insert_after_build.push(step);
        self
    }

    /// Insert a step between the default cargo build and the default rotation.
    ///
    /// Steps inserted here run in the order they were added. When several
    /// steps are inserted with both methods, all
    /// [`insert_after_default_build`](Self::insert_after_default_build) steps
    /// run first.
    pub fn insert_before_default_rotate(mut self, step: DeployStep) -> Self {
        self.insert_before_rotate.push(step);
        self
    }

    /// Replace the default cargo-build + rotate plan with exactly the steps
    /// added by the caller.
    pub fn without_defaults(mut self) -> Self {
        self.defaults = false;
        self
    }

    pub fn run(mut self) -> Result<DeployReport> {
        if self.defaults {
            let mut steps = DeployPlan::standard(self.context);
            let rotate = steps
                .pop()
                .expect("the standard plan always contains a rotation step");
            steps.append(&mut self.insert_after_build);
            steps.append(&mut self.insert_before_rotate);
            steps.push(rotate);
            steps.append(&mut self.plan.steps);
            self.plan.steps = steps;
        }
        let context = self.context;
        let _lock = if context.dry_run {
            None
        } else {
            Some(DeployLock::acquire(
                &context.state_dir,
                &context.binary_name,
            )?)
        };
        if self.defaults
            && !context.dry_run
            && let Some(report) = skip_current_report(context)?
        {
            return Ok(report);
        }
        let report = self.plan.run(context)?;
        if self.defaults && !context.dry_run {
            verify_install(context)?;
            if let Err(error) = write_deploy_stamp(context) {
                eprintln!("warning: deployed, but the deployment stamp was not written: {error:#}");
            }
        }
        Ok(report)
    }
}

/// When the installed binary already matches the built source and `force` is
/// not set, the default plan is skipped without touching the destination.
fn skip_current_report(context: &DeployContext) -> Result<Option<DeployReport>> {
    let source = context.built_binary_path();
    let destination = &context.install_path;
    if !source.is_file() || !destination.is_file() {
        return Ok(None);
    }
    let matches = content_matches(&source, destination).with_context(|| {
        format!(
            "could not compare {} with {}",
            source.display(),
            destination.display()
        )
    })?;
    if !matches {
        return Ok(None);
    }
    if context.force {
        return Ok(None);
    }
    if !context.quiet {
        println!(
            "skipping deploy: {} already matches {} (use --force to deploy anyway)",
            destination.display(),
            source.display()
        );
    }
    Ok(Some(DeployReport {
        installed: destination.clone(),
        dry_run: false,
        plan: Vec::new(),
        steps: vec![
            DeployStepReport {
                name: "cargo build".to_owned(),
                status: StepStatus::Skipped,
                detail: Some("installed binary already matches the built source".to_owned()),
            },
            DeployStepReport {
                name: "rotate".to_owned(),
                status: StepStatus::Skipped,
                detail: Some(format!(
                    "{} already matches {}",
                    destination.display(),
                    source.display()
                )),
            },
        ],
        hooks: Vec::new(),
    }))
}

/// Verify that the rotation installed exactly the source binary.
fn verify_install(context: &DeployContext) -> Result<()> {
    let source = context.built_binary_path();
    let destination = &context.install_path;
    if !destination.is_file() {
        anyhow::bail!(
            "installed binary is missing after rotation: {}",
            destination.display()
        );
    }
    let source_fingerprint = file_fingerprint(&source)
        .with_context(|| format!("could not fingerprint source {}", source.display()))?;
    let destination_fingerprint = file_fingerprint(destination).with_context(|| {
        format!(
            "could not fingerprint installed binary {}",
            destination.display()
        )
    })?;
    if source_fingerprint != destination_fingerprint {
        anyhow::bail!(
            "installed binary {} ({destination_fingerprint}) does not match the source {} ({source_fingerprint})",
            destination.display(),
            source.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(destination)?.permissions().mode();
        if mode & 0o111 == 0 {
            anyhow::bail!(
                "installed binary {} is not executable (mode {mode:o})",
                destination.display()
            );
        }
    }
    Ok(())
}

/// Record the deployed fingerprints in `state_dir` for diagnostics.
fn write_deploy_stamp(context: &DeployContext) -> Result<()> {
    let source = context.built_binary_path();
    let destination = &context.install_path;
    let source_fingerprint = file_fingerprint(&source)?;
    let destination_fingerprint = file_fingerprint(destination)?;
    fs::create_dir_all(&context.state_dir)
        .with_context(|| format!("could not create state dir {}", context.state_dir.display()))?;
    let path = context
        .state_dir
        .join(format!("{}.stamp", context.binary_name));
    let temporary = context
        .state_dir
        .join(format!("{}.stamp.tmp", context.binary_name));
    let deployed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let contents = format!(
        "schema=1\nbinary={}\nsource={}\ndestination={}\nsource_fingerprint={source_fingerprint}\ndestination_fingerprint={destination_fingerprint}\ndeployed_at={deployed_at}\n",
        context.binary_name,
        source.display(),
        destination.display()
    );
    fs::write(&temporary, contents)
        .with_context(|| format!("could not write stamp {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("could not replace stamp {}", path.display()))?;
    Ok(())
}

/// Start a deploy plan. The returned builder includes the default build and
/// rotate steps; call [`DeployBuilder::run`] to execute it.
///
/// ```ignore
/// let context = tracel_deploy::DeployContext::from_args(&args, workspace, "my-app")?;
/// tracel_deploy::deploy(&context)
///     .pre(|_| stop_service())
///     .post(|_| start_and_verify_service())
///     .run()?;
/// ```
pub fn deploy(context: &DeployContext) -> DeployBuilder<'_> {
    DeployBuilder::new(context)
}

pub enum DeployStep {
    /// Run `cargo build -p <package>` in the workspace.
    CargoBuild(CargoBuildStep),
    /// Replace the installed binary, possibly while it is running.
    Rotate(RotateStep),
    /// Run a command. Useful for service stop/start steps.
    Run(RunStep),
    /// Arbitrary project-specific logic.
    Custom(DeployHook),
}

impl DeployStep {
    /// Single-line description of the work this step would do.
    pub fn describe(&self, context: &DeployContext) -> String {
        match self {
            Self::CargoBuild(step) => step.command_line(context).join(" "),
            Self::Rotate(step) => step.description(),
            Self::Run(step) => step.description(),
            Self::Custom(_) => "custom step (opaque)".to_owned(),
        }
    }

    fn run(self, context: &DeployContext) -> Result<DeployStepReport> {
        match self {
            Self::CargoBuild(step) => step.run(context),
            Self::Rotate(step) => step.run(context),
            Self::Run(step) => step.run(context),
            Self::Custom(mut step) => {
                step(context).with_context(|| "step 'custom' failed")?;
                Ok(DeployStepReport {
                    name: "custom".to_owned(),
                    status: StepStatus::Success,
                    detail: None,
                })
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CargoBuildStep {
    pub package: String,
    pub release: bool,
    pub cargo: Option<PathBuf>,
    pub profile: Option<String>,
    pub locked: bool,
    pub target: Option<String>,
    pub target_dir: Option<PathBuf>,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub extra_args: Vec<String>,
}

impl CargoBuildStep {
    pub fn from_context(context: &DeployContext) -> Self {
        Self {
            package: context.package.clone(),
            release: context.release,
            cargo: context.cargo.clone(),
            profile: context.profile_name.clone(),
            locked: context.locked,
            target: context.target.clone(),
            target_dir: context.target_dir.clone(),
            features: context.features.clone(),
            no_default_features: context.no_default_features,
            extra_args: context.build_args.clone(),
        }
    }

    /// Full command line (program first) used for building. Execution, dry-run
    /// rendering, and error messages all share this source.
    pub fn command_line(&self, _context: &DeployContext) -> Vec<String> {
        let program = self
            .cargo
            .clone()
            .or_else(|| std::env::var_os("CARGO").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("cargo"));
        let mut line = vec![
            program.display().to_string(),
            "build".to_owned(),
            "-p".to_owned(),
            self.package.clone(),
            "--color".to_owned(),
            "always".to_owned(),
        ];
        match (&self.profile, self.release) {
            (Some(profile), _) => {
                line.push("--profile".to_owned());
                line.push(profile.clone());
            }
            (None, true) => line.push("--release".to_owned()),
            (None, false) => {}
        }
        if self.locked {
            line.push("--locked".to_owned());
        }
        if let Some(target) = &self.target {
            line.push("--target".to_owned());
            line.push(target.clone());
        }
        if let Some(target_dir) = &self.target_dir {
            line.push("--target-dir".to_owned());
            line.push(target_dir.display().to_string());
        }
        if !self.features.is_empty() {
            line.push("--features".to_owned());
            line.push(self.features.join(","));
        }
        if self.no_default_features {
            line.push("--no-default-features".to_owned());
        }
        line.extend(self.extra_args.iter().cloned());
        line
    }

    fn run(self, context: &DeployContext) -> Result<DeployStepReport> {
        let line = self.command_line(context);
        let command = line.join(" ");
        if !context.quiet {
            println!("{command}");
        }
        let status = Command::new(&line[0])
            .current_dir(&context.workspace_root)
            .args(&line[1..])
            .status()
            .with_context(|| {
                format!(
                    "step 'cargo build' (package {}) failed in {}: could not run `{command}`",
                    self.package,
                    context.workspace_root.display()
                )
            })?;
        if !status.success() {
            anyhow::bail!(
                "step 'cargo build' (package {}) failed in {} with {status} (command: `{command}`)",
                self.package,
                context.workspace_root.display()
            );
        }
        Ok(DeployStepReport {
            name: "cargo build".to_owned(),
            status: StepStatus::Success,
            detail: Some(self.package),
        })
    }
}

#[derive(Debug, Clone)]
pub struct RotateStep {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub backup_dir: Option<PathBuf>,
    pub strategy: RotateStrategy,
    pub backup_retention: Option<usize>,
    pub backup_max_age: Option<std::time::Duration>,
}

impl RotateStep {
    pub fn from_context(context: &DeployContext) -> Self {
        Self {
            source: context.built_binary_path(),
            destination: context.install_path.clone(),
            backup_dir: context.backup_dir.clone(),
            strategy: RotateStrategy::Auto,
            backup_retention: context.backup_retention,
            backup_max_age: context.backup_max_age,
        }
    }

    /// Single-line description of the rotation this step would perform.
    pub fn description(&self) -> String {
        let mut line = format!(
            "rotate {} -> {}",
            self.source.display(),
            self.destination.display()
        );
        if let Some(dir) = &self.backup_dir {
            line.push_str(&format!(" (backup dir: {})", dir.display()));
        }
        line
    }

    fn run(self, context: &DeployContext) -> Result<DeployStepReport> {
        let description = self.description();
        let outcome = rotate(&RotateOptions {
            source: self.source,
            destination: self.destination,
            backup_dir: self.backup_dir,
            strategy: self.strategy,
            backup_retention: self.backup_retention,
            backup_max_age: self.backup_max_age,
            dry_run: context.dry_run,
        })
        .with_context(|| format!("step 'rotate' failed: {description}"))?;
        let detail = match &outcome {
            RotateOutcome::WouldReplace { destination } => {
                format!("would replace {}", destination.display())
            }
            RotateOutcome::Replaced { destination } => {
                format!("replaced {}", destination.display())
            }
            RotateOutcome::ReplacedAfterRename {
                destination,
                backup,
            } => {
                format!(
                    "replaced {}; previous binary preserved at {}",
                    destination.display(),
                    backup.display()
                )
            }
        };
        Ok(DeployStepReport {
            name: "rotate".to_owned(),
            status: StepStatus::Success,
            detail: Some(detail),
        })
    }
}

#[derive(Debug, Clone)]
pub struct RunStep {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub current_dir: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub allow_failure: bool,
}

impl RunStep {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: Vec::new(),
            allow_failure: false,
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn allow_failure(mut self, allow: bool) -> Self {
        self.allow_failure = allow;
        self
    }

    /// Single-line description of the command this step would run.
    pub fn description(&self) -> String {
        let mut line = format!("run {}", self.program.display());
        if !self.args.is_empty() {
            line.push(' ');
            line.push_str(&self.args.join(" "));
        }
        if let Some(dir) = &self.current_dir {
            line.push_str(&format!(" (cwd: {})", dir.display()));
        }
        line
    }

    fn run(self, context: &DeployContext) -> Result<DeployStepReport> {
        let description = self.description();
        if !context.quiet {
            println!("{description}");
        }
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(dir) = &self.current_dir {
            command.current_dir(dir);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        if self.allow_failure {
            command.stderr(Stdio::piped());
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "step '{description}' failed: could not spawn `{}`",
                self.program.display()
            )
        })?;
        let stderr = child.stderr.take().map(|mut pipe| {
            let mut buffer = String::new();
            let _ = pipe.read_to_string(&mut buffer);
            buffer
        });
        let status = child.wait().with_context(|| {
            format!(
                "step '{description}' failed: could not wait for `{}`",
                self.program.display()
            )
        })?;
        if !status.success() {
            if !self.allow_failure {
                anyhow::bail!("step '{description}' failed with {status}");
            }
            let mut detail = format!("exit {status} (allowed failure)");
            if let Some(tail) = stderr.as_deref().and_then(|text| tail_lines(text, 20))
                && !tail.is_empty()
            {
                detail.push_str(&format!("; stderr tail:\n{tail}"));
            }
            return Ok(DeployStepReport {
                name: format!("run {}", self.program.display()),
                status: StepStatus::Success,
                detail: Some(detail),
            });
        }
        Ok(DeployStepReport {
            name: format!("run {}", self.program.display()),
            status: StepStatus::Success,
            detail: None,
        })
    }
}

/// Last `count` lines of `text`, or `None` when the text is empty.
fn tail_lines(text: &str, count: usize) -> Option<String> {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    let start = lines.len().saturating_sub(count);
    Some(lines[start..].join("\n"))
}

#[derive(Debug, Default)]
pub struct DeployReport {
    pub installed: PathBuf,
    pub dry_run: bool,
    /// Steps that would run; filled by dry-run.
    pub plan: Vec<String>,
    pub steps: Vec<DeployStepReport>,
    /// Hook execution results across pre / post / on_error / finally.
    pub hooks: Vec<DeployHookReport>,
}

/// Result of one executed hook.
#[derive(Debug)]
pub struct DeployHookReport {
    pub name: String,
    pub succeeded: bool,
    pub error: Option<String>,
}

/// Completion state of a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepStatus {
    #[default]
    Success,
    Skipped,
}

#[derive(Debug)]
pub struct DeployStepReport {
    pub name: String,
    pub status: StepStatus,
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::{
        CargoBuildStep, DeployStep, RunStep, StepStatus, deploy, skip_current_report,
        verify_install, write_deploy_stamp,
    };
    use crate::context::DeployContext;

    fn test_context() -> DeployContext {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let mut context = DeployContext::new(std::env::temp_dir(), "tracel-deploy-test");
        context.quiet = true;
        context.state_dir = std::env::temp_dir().join(format!(
            "tracel-deploy-state-{}-{unique}",
            std::process::id()
        ));
        context
    }

    #[test]
    fn pre_steps_and_post_hooks_run_in_order() {
        let context = test_context();
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let pre_events = Rc::clone(&events);
        let step_events = Rc::clone(&events);
        let post_events = Rc::clone(&events);

        deploy(&context)
            .without_defaults()
            .pre(move |_| {
                pre_events.borrow_mut().push("pre");
                Ok(())
            })
            .step(DeployStep::Custom(Box::new(move |_| {
                step_events.borrow_mut().push("step");
                Ok(())
            })))
            .post(move |_| {
                post_events.borrow_mut().push("post");
                Ok(())
            })
            .run()
            .expect("deploy plan");

        assert_eq!(*events.borrow(), ["pre", "step", "post"]);
    }

    #[test]
    fn dry_run_skips_hooks_and_steps() {
        let mut context = test_context();
        context.dry_run = true;
        let called = Rc::new(RefCell::new(false));
        let hook_called = Rc::clone(&called);

        deploy(&context)
            .without_defaults()
            .pre(move |_| {
                *hook_called.borrow_mut() = true;
                Ok(())
            })
            .run()
            .expect("dry-run plan");

        assert!(!*called.borrow());
    }

    #[test]
    fn dry_run_renders_default_plan_without_side_effects() {
        let mut context = test_context();
        context.dry_run = true;

        let report = deploy(&context).run().expect("dry-run plan");

        assert!(
            report
                .plan
                .iter()
                .any(|line| line.contains("-p tracel-deploy-test")
                    && line.contains("--color always")),
            "plan: {:?}",
            report.plan
        );
        assert!(
            report.plan.iter().any(|line| line.starts_with("rotate ")),
            "plan: {:?}",
            report.plan
        );
        assert!(report.steps.is_empty());
        assert!(report.hooks.is_empty());
    }

    #[test]
    fn finally_restarts_service_after_rotate_failure() {
        let context = test_context();
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let stop_events = Rc::clone(&events);
        let rotate_events = Rc::clone(&events);
        let start_events = Rc::clone(&events);

        let error = deploy(&context)
            .without_defaults()
            .step(DeployStep::Custom(Box::new(move |_| {
                stop_events.borrow_mut().push("stop");
                Ok(())
            })))
            .step(DeployStep::Custom(Box::new(move |_| {
                rotate_events.borrow_mut().push("rotate");
                anyhow::bail!("rotate failed")
            })))
            .finally(move |_| {
                start_events.borrow_mut().push("start");
                Ok(())
            })
            .run()
            .expect_err("rotation failure must surface");

        assert_eq!(*events.borrow(), ["stop", "rotate", "start"]);
        assert!(format!("{error:#}").contains("rotate failed"));
    }

    #[test]
    fn post_is_skipped_but_on_error_and_finally_run() {
        let context = test_context();
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let pre_events = Rc::clone(&events);
        let post_events = Rc::clone(&events);
        let error_events = Rc::clone(&events);
        let finally_events = Rc::clone(&events);

        let error = deploy(&context)
            .without_defaults()
            .pre(move |_| {
                pre_events.borrow_mut().push("pre");
                Ok(())
            })
            .step(DeployStep::Custom(Box::new(|_| {
                anyhow::bail!("step failed")
            })))
            .post(move |_| {
                post_events.borrow_mut().push("post");
                Ok(())
            })
            .on_error(move |_| {
                error_events.borrow_mut().push("on_error");
                Ok(())
            })
            .finally(move |_| {
                finally_events.borrow_mut().push("finally");
                Ok(())
            })
            .run()
            .expect_err("step failure must surface");

        assert_eq!(*events.borrow(), ["pre", "on_error", "finally"]);
        assert!(format!("{error:#}").contains("step failed"));
    }

    #[test]
    fn on_error_failure_does_not_stop_finally() {
        let context = test_context();
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let error_events = Rc::clone(&events);
        let finally_events = Rc::clone(&events);

        let error = deploy(&context)
            .without_defaults()
            .step(DeployStep::Custom(Box::new(|_| anyhow::bail!("primary"))))
            .on_error(move |_| {
                error_events.borrow_mut().push("on_error");
                anyhow::bail!("cleanup failed")
            })
            .finally(move |_| {
                finally_events.borrow_mut().push("finally");
                Ok(())
            })
            .run()
            .expect_err("failures must surface");

        assert_eq!(*events.borrow(), ["on_error", "finally"]);
        let text = format!("{error:#}");
        assert!(text.contains("primary"), "text: {text}");
        assert!(text.contains("on_error hook #1 failed"), "text: {text}");
        assert!(text.contains("additional failure"), "text: {text}");
    }

    #[test]
    fn hook_results_are_reported() {
        let context = test_context();

        let report = deploy(&context)
            .without_defaults()
            .finally_named("restart service", |_| Ok(()))
            .run()
            .expect("plan");

        assert_eq!(report.hooks.len(), 1);
        assert_eq!(report.hooks[0].name, "restart service");
        assert!(report.hooks[0].succeeded);
        assert!(report.hooks[0].error.is_none());
    }

    #[test]
    fn cargo_build_errors_include_step_name_and_paths() {
        let dir = std::env::temp_dir().join(format!(
            "tracel-deploy-build-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut context = test_context();
        context.workspace_root = dir.clone();
        context.package = "no-such-package".to_owned();

        let step = CargoBuildStep::from_context(&context);
        let error = step.run(&context).expect_err("cargo build must fail");

        let text = format!("{error:#}");
        assert!(text.contains("step 'cargo build'"), "text: {text}");
        assert!(text.contains("no-such-package"), "text: {text}");
        assert!(text.contains(&dir.display().to_string()), "text: {text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_step_errors_include_program_and_args() {
        let context = test_context();

        let error = RunStep::new("tracel-deploy-missing-program")
            .args(["--flag"])
            .run(&context)
            .expect_err("spawning a missing program must fail");

        let text = format!("{error:#}");
        assert!(
            text.contains("step 'run tracel-deploy-missing-program --flag'"),
            "text: {text}"
        );
    }

    #[test]
    fn named_hook_failures_use_the_given_name() {
        let context = test_context();

        let error = deploy(&context)
            .without_defaults()
            .pre_named("stop service", |_| anyhow::bail!("boom"))
            .run()
            .expect_err("hook failure must surface");

        let text = format!("{error:#}");
        assert!(text.contains("stop service failed"), "text: {text}");
    }

    #[test]
    fn allow_failure_keeps_exit_code_and_stderr_tail() {
        let context = test_context();
        #[cfg(windows)]
        let step = RunStep::new("cmd")
            .args(["/C", "echo boom 1>&2 & exit 3"])
            .allow_failure(true);
        #[cfg(unix)]
        let step = RunStep::new("sh")
            .args(["-c", "echo boom 1>&2; exit 3"])
            .allow_failure(true);

        let report = deploy(&context)
            .without_defaults()
            .step(DeployStep::Run(step))
            .run()
            .expect("allowed failure still succeeds");

        let detail = report.steps[0].detail.clone().unwrap_or_default();
        assert!(detail.contains("exit"), "detail: {detail}");
        assert!(detail.contains("boom"), "detail: {detail}");
        assert_eq!(report.steps[0].status, StepStatus::Success);
    }

    #[test]
    fn identical_artifacts_skip_the_default_plan() {
        let dir = std::env::temp_dir().join(format!(
            "tracel-deploy-skip-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let source = dir.join("source");
        let destination = dir.join("installed");
        std::fs::write(&source, "same bytes").expect("source");
        std::fs::write(&destination, "same bytes").expect("destination");

        let context = crate::context::DeployContext::builder(&dir, "tracel-deploy-test")
            .quiet(true)
            .source(&source)
            .install_to(&destination)
            .state_dir(dir.join("state"))
            .build();

        let report = deploy(&context).run().expect("skipped deploy");

        assert_eq!(report.steps.len(), 2);
        assert_eq!(report.steps[0].status, StepStatus::Skipped);
        assert_eq!(report.steps[1].status, StepStatus::Skipped);
        assert_eq!(
            std::fs::read_to_string(&destination).expect("destination"),
            "same bytes"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn force_disables_the_skip() {
        let mut context = test_context();
        let dir = std::env::temp_dir().join(format!(
            "tracel-deploy-force-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let source = dir.join("source");
        let destination = dir.join("installed");
        std::fs::write(&source, "same bytes").expect("source");
        std::fs::write(&destination, "same bytes").expect("destination");
        context.source_binary = Some(source);
        context.install_path = destination;

        assert!(skip_current_report(&context).expect("compare").is_some());
        context.force = true;
        assert!(skip_current_report(&context).expect("compare").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deploy_lock_blocks_concurrent_deploys() {
        let state_dir = std::env::temp_dir().join(format!(
            "tracel-deploy-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        let lock = crate::context::DeployLock::acquire(&state_dir, "test-binary").expect("lock");
        assert!(lock.path().is_file());

        let error = crate::context::DeployLock::acquire(&state_dir, "test-binary")
            .expect_err("second lock must fail");
        assert!(
            format!("{error:#}").contains("deployment already running"),
            "error: {error:#}"
        );

        drop(lock);
        let lock = crate::context::DeployLock::acquire(&state_dir, "test-binary")
            .expect("lock after release");
        drop(lock);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn verify_install_reports_mismatches() {
        let dir = std::env::temp_dir().join(format!(
            "tracel-deploy-verify-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let source = dir.join("source");
        let destination = dir.join("installed");
        std::fs::write(&source, "new").expect("source");
        std::fs::write(&destination, "old").expect("destination");

        let mut context = test_context();
        context.source_binary = Some(source.clone());
        context.install_path = destination.clone();
        let error = verify_install(&context).expect_err("mismatch must fail");
        let text = format!("{error:#}");
        assert!(text.contains("does not match"), "text: {text}");
        assert!(text.contains(&source.display().to_string()), "text: {text}");
        assert!(
            text.contains(&destination.display().to_string()),
            "text: {text}"
        );

        std::fs::write(&destination, "new").expect("destination");
        verify_install(&context).expect("matching fingerprints pass");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deploy_stamp_records_fingerprints() {
        let dir = std::env::temp_dir().join(format!(
            "tracel-deploy-stamp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let source = dir.join("source");
        let destination = dir.join("installed");
        std::fs::write(&source, "bytes").expect("source");
        std::fs::write(&destination, "bytes").expect("destination");

        let mut context = test_context();
        context.state_dir = dir.join("state");
        context.source_binary = Some(source);
        context.install_path = destination;

        write_deploy_stamp(&context).expect("write stamp");

        let stamp = std::fs::read_to_string(context.state_dir.join("tracel-deploy-test.stamp"))
            .expect("stamp file");
        assert!(stamp.contains("schema=1"), "stamp: {stamp}");
        assert!(stamp.contains("source_fingerprint="), "stamp: {stamp}");
        assert!(stamp.contains("destination_fingerprint="), "stamp: {stamp}");
        assert!(stamp.contains("deployed_at="), "stamp: {stamp}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
