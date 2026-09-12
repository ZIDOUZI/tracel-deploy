use std::{
    fmt, fs,
    io::Read,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};

use crate::{
    context::{
        DeployContext, DeployLock, FileFingerprint, build_inputs_fingerprint, file_fingerprint,
    },
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

/// Where a deploy plan sends its human-readable output.
///
/// Every message the library prints goes through a reporter: a
/// [`StdoutReporter`] by default (the 0.1.0 output), a [`NullReporter`] for a
/// fully silent deploy, or a caller-owned implementation for logs, structured
/// events, or a CI summary. [`DeployBuilder::reporter`] injects one.
///
/// Every method has a no-op default, so an implementation can react to only the
/// events it cares about. An injected reporter replaces the `quiet` handling:
/// it runs even for a context with `quiet` set.
///
/// ```ignore
/// use tracel_deploy::plan::{DeployStepReport, Reporter};
///
/// #[derive(Default)]
/// struct Summary {
///     steps: Vec<String>,
/// }
///
/// impl Reporter for Summary {
///     fn step_finished(&mut self, report: &DeployStepReport) {
///         self.steps.push(format!("{}: {:?}", report.name, report.status));
///     }
/// }
///
/// tracel_deploy::deploy(&context)
///     .reporter(Summary::default())
///     .run()?;
/// ```
pub trait Reporter {
    /// The dry-run plan: one line per step that would run, the same lines as
    /// [`DeployReport::plan`]. [`StdoutReporter`] renders each line as
    /// `would: <line>` unless it is quiet.
    fn plan(&mut self, lines: &[String]) {
        let _ = lines;
    }

    /// A step is about to run. `description` is the single line of
    /// [`DeployStep::describe`]; `name` is the name the step reports itself
    /// under, for example `cargo build` or `run my-service`.
    ///
    /// [`StdoutReporter`] prints nothing here: the steps that echoed a line in
    /// 0.1.0 report that line through [`Reporter::note`] instead, so a reporter
    /// that prints both would print it twice.
    fn step_started(&mut self, name: &str, description: &str) {
        let _ = (name, description);
    }

    /// A message without step context: the cargo command line and the `run …`
    /// description a step echoes before it runs, and the idempotent-skip
    /// notice. [`StdoutReporter`] prints these lines to stdout unless it is
    /// quiet.
    fn note(&mut self, message: &str) {
        let _ = message;
    }

    /// A step finished and produced a report; a step that failed returns an
    /// error instead, so no report reaches the reporter.
    fn step_finished(&mut self, report: &DeployStepReport) {
        let _ = report;
    }

    /// A non-fatal problem: the deploy succeeded but something around it did
    /// not (for example a deployment stamp that could not be written).
    /// [`StdoutReporter`] writes these to stderr and `quiet` does not suppress
    /// them; use [`NullReporter`] for complete silence.
    fn warning(&mut self, message: &str) {
        let _ = message;
    }
}

/// The default reporter: the stdout behaviour of 0.1.0.
///
/// The cargo command line, the `run …` description, and the skip notice are
/// printed to stdout, the dry-run plan as `would: <line>` lines, and warnings
/// to stderr. A quiet reporter suppresses every stdout line (that is what
/// `DeployContext::quiet` maps onto) but still writes warnings to stderr.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdoutReporter {
    quiet: bool,
}

impl StdoutReporter {
    /// A reporter that prints the deploy progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// A reporter that prints nothing to stdout, like a context with `quiet`.
    pub fn quiet() -> Self {
        Self { quiet: true }
    }

    /// Map a `quiet` flag (for example [`DeployContext::quiet`]) onto a
    /// reporter.
    pub fn with_quiet(quiet: bool) -> Self {
        Self { quiet }
    }

    /// Whether the stdout lines are suppressed.
    pub fn is_quiet(&self) -> bool {
        self.quiet
    }
}

impl Reporter for StdoutReporter {
    fn plan(&mut self, lines: &[String]) {
        if self.quiet {
            return;
        }
        for line in lines {
            println!("would: {line}");
        }
    }

    fn note(&mut self, message: &str) {
        if !self.quiet {
            println!("{message}");
        }
    }

    fn warning(&mut self, message: &str) {
        eprintln!("warning: {message}");
    }
}

/// A reporter that discards every message: a fully silent deploy for a caller
/// that consumes [`DeployReport`] (or the exit status) instead of the output.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullReporter;

impl Reporter for NullReporter {}

/// The reporter used when the caller does not inject one.
fn default_reporter(context: &DeployContext) -> StdoutReporter {
    StdoutReporter::with_quiet(context.quiet)
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
        let mut reporter = default_reporter(context);
        self.run_with(context, &mut reporter)
    }

    /// Like [`run`](Self::run) but sends every message to `reporter`.
    ///
    /// The injected reporter also runs for a context with `quiet` set; it is
    /// the caller's decision what to do with the messages then.
    pub fn run_with(
        self,
        context: &DeployContext,
        reporter: &mut dyn Reporter,
    ) -> Result<DeployReport> {
        let mut report = DeployReport {
            installed: context.install_path.clone(),
            dry_run: context.dry_run,
            plan: Vec::new(),
            steps: Vec::new(),
            hooks: Vec::new(),
        };
        if context.dry_run {
            report.plan = self.render(context);
            reporter.plan(&report.plan);
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
                match step.run(context, reporter) {
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
///
/// The three behaviours the default plan adds around the steps — the
/// idempotent short-circuit, the install verification, and the deployment
/// stamp — are controlled by [`DeployBuilder::idempotent_skip`],
/// [`DeployBuilder::verify_install`], and [`DeployBuilder::write_deploy_stamp`].
/// They follow the plan by default (on for the default plan, off after
/// [`without_defaults`](Self::without_defaults)) and can be enabled
/// explicitly, so a caller-owned plan can opt into them.
pub struct DeployBuilder<'a> {
    context: &'a DeployContext,
    plan: DeployPlan,
    defaults: bool,
    idempotent_skip: Option<bool>,
    verify_install: Option<bool>,
    write_deploy_stamp: Option<bool>,
    insert_after_build: Vec<DeployStep>,
    insert_before_rotate: Vec<DeployStep>,
    reporter: Option<Box<dyn Reporter>>,
}

impl<'a> DeployBuilder<'a> {
    fn new(context: &'a DeployContext) -> Self {
        Self {
            context,
            plan: DeployPlan::new(),
            defaults: true,
            idempotent_skip: None,
            verify_install: None,
            write_deploy_stamp: None,
            insert_after_build: Vec::new(),
            insert_before_rotate: Vec::new(),
            reporter: None,
        }
    }

    /// Send the deploy output to `reporter` instead of the default
    /// [`StdoutReporter`].
    ///
    /// The injected reporter replaces the `quiet` handling, so a caller that
    /// wants a silent deploy passes [`NullReporter`] and a caller that wants
    /// structured events implements [`Reporter`].
    pub fn reporter(mut self, reporter: impl Reporter + 'static) -> Self {
        self.reporter = Some(Box::new(reporter));
        self
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

    /// Enable or disable the idempotent short-circuit.
    ///
    /// The default follows the plan: on for the default build + rotate plan,
    /// off after [`without_defaults`](Self::without_defaults) — exactly the
    /// behaviour of earlier releases. Call it explicitly to opt a caller-owned
    /// plan into the short-circuit (or to force the default plan to always
    /// run):
    ///
    /// ```ignore
    /// deploy(&context)
    ///     .without_defaults()
    ///     .idempotent_skip(true)
    ///     .step(DeployStep::CargoBuild(CargoBuildStep::from_context(&context)))
    ///     .run()?;
    /// ```
    ///
    /// A skip requires the deployment stamp to prove both that the build
    /// inputs still hash to the recorded value and that the installed binary
    /// still matches the recorded fingerprint. A changed source tree is never
    /// skipped just because the previously built binary happens to equal the
    /// installed one, and a missing or legacy stamp never skips.
    pub fn idempotent_skip(mut self, enabled: bool) -> Self {
        self.idempotent_skip = Some(enabled);
        self
    }

    /// Enable or disable the install verification after the steps ran.
    ///
    /// The default follows the plan (on for the default plan, off after
    /// [`without_defaults`](Self::without_defaults)). Verification compares the
    /// built binary with the installed one and fails the deploy on a mismatch;
    /// it is not part of the skip path.
    pub fn verify_install(mut self, enabled: bool) -> Self {
        self.verify_install = Some(enabled);
        self
    }

    /// Enable or disable writing the deployment stamp after a successful plan.
    ///
    /// The default follows the plan (on for the default plan, off after
    /// [`without_defaults`](Self::without_defaults)). The stamp records the
    /// source, destination, and build-input fingerprints plus the install time
    /// in `state_dir`; a failure to write it only warns, because the deploy
    /// itself succeeded.
    pub fn write_deploy_stamp(mut self, enabled: bool) -> Self {
        self.write_deploy_stamp = Some(enabled);
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
        let skip_when_unchanged = self.idempotent_skip.unwrap_or(self.defaults);
        let verify_after_rotate = self.verify_install.unwrap_or(self.defaults);
        let stamp_after_rotate = self.write_deploy_stamp.unwrap_or(self.defaults);
        let context = self.context;
        let mut reporter: Box<dyn Reporter> = self
            .reporter
            .unwrap_or_else(|| Box::new(default_reporter(context)));
        let _lock = if context.dry_run {
            None
        } else {
            Some(DeployLock::acquire(
                &context.state_dir,
                &context.binary_name,
            )?)
        };
        if skip_when_unchanged
            && !context.dry_run
            && let Some(report) = skip_current_report(context)?
        {
            reporter.note(&format!(
                "skipping deploy: build inputs are unchanged and {} still matches the stamped binary (use --force to deploy anyway)",
                context.install_path.display()
            ));
            return Ok(report);
        }
        let report = self.plan.run_with(context, reporter.as_mut())?;
        if !context.dry_run && verify_after_rotate {
            verify_installed_binary(context)?;
        }
        if !context.dry_run
            && stamp_after_rotate
            && let Err(error) = write_deploy_stamp(context)
        {
            reporter.warning(&format!(
                "deployed, but the deployment stamp was not written: {error:#}"
            ));
        }
        Ok(report)
    }
}

/// Idempotent short-circuit: skip the whole plan when the deployment stamp
/// proves that nothing changed.
///
/// A skip requires both halves of the proof:
///
/// - the build inputs still hash to the fingerprint recorded by the stamp
///   (the decision is made *before* the build, so a changed source tree with a
///   stale build output is never mistaken for "up to date"), and
/// - the installed binary still matches the fingerprint recorded by the stamp.
///
/// `force` disables the short-circuit, a missing stamp never skips, and a
/// legacy stamp without the build-input fingerprint never skips either.
///
/// The returned report describes the skipped work as the default `cargo
/// build` + `rotate` pair; callers that replaced the plan still get the same
/// proof of "nothing changed", only the step labels are generic.
fn skip_current_report(context: &DeployContext) -> Result<Option<DeployReport>> {
    let destination = &context.install_path;
    if context.force {
        return Ok(None);
    }
    let Some(stamp) = read_deploy_stamp(context)? else {
        return Ok(None);
    };
    let (Some(stamped_inputs), Some(stamped_destination)) = (
        stamp.build_inputs_fingerprint,
        stamp.destination_fingerprint,
    ) else {
        return Ok(None);
    };
    if !destination.is_file() {
        return Ok(None);
    }
    if build_inputs_fingerprint(context)? != stamped_inputs {
        return Ok(None);
    }
    let installed = file_fingerprint(destination).with_context(|| {
        format!(
            "could not fingerprint installed binary {}",
            destination.display()
        )
    })?;
    if installed != stamped_destination {
        return Ok(None);
    }
    // The skip notice is printed by the caller through the reporter, so that
    // this function stays free of output.
    Ok(Some(DeployReport {
        installed: destination.clone(),
        dry_run: false,
        plan: Vec::new(),
        steps: vec![
            DeployStepReport {
                name: "cargo build".to_owned(),
                status: StepStatus::Skipped,
                detail: Some("build inputs are unchanged since the last deploy".to_owned()),
                duration: None,
                exit_code: None,
                artifacts: Vec::new(),
            },
            DeployStepReport {
                name: "rotate".to_owned(),
                status: StepStatus::Skipped,
                detail: Some(format!(
                    "{} already matches the stamped binary",
                    destination.display(),
                )),
                duration: None,
                exit_code: None,
                artifacts: Vec::new(),
            },
        ],
        hooks: Vec::new(),
    }))
}

/// Verify that the rotation installed exactly the source binary.
fn verify_installed_binary(context: &DeployContext) -> Result<()> {
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

/// Path of the deployment stamp inside `state_dir`.
fn deploy_stamp_path(context: &DeployContext) -> PathBuf {
    context
        .state_dir
        .join(format!("{}.stamp", context.binary_name))
}

/// Fields of a deployment stamp. A field is `None` when the stamp does not
/// contain it: stamps written before the build-input fingerprint existed only
/// carry the source and destination fingerprints.
#[derive(Debug, Clone, Copy, Default)]
struct DeployStamp {
    source_fingerprint: Option<FileFingerprint>,
    destination_fingerprint: Option<FileFingerprint>,
    build_inputs_fingerprint: Option<FileFingerprint>,
}

/// Read the deployment stamp; `Ok(None)` means "no stamp yet" (including an
/// unreadable one, which is treated as absent rather than fatal).
fn read_deploy_stamp(context: &DeployContext) -> Result<Option<DeployStamp>> {
    let path = deploy_stamp_path(context);
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(parse_deploy_stamp(&contents))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("could not read deployment stamp {}", path.display())),
    }
}

/// Parse a stamp leniently: unknown keys are ignored and missing keys stay
/// `None`, so old (schema 1) stamps keep parsing and simply never skip.
fn parse_deploy_stamp(contents: &str) -> DeployStamp {
    let mut stamp = DeployStamp::default();
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "source_fingerprint" => stamp.source_fingerprint = parse_fingerprint(value),
            "destination_fingerprint" => stamp.destination_fingerprint = parse_fingerprint(value),
            "build_inputs_fingerprint" => stamp.build_inputs_fingerprint = parse_fingerprint(value),
            _ => {}
        }
    }
    stamp
}

/// Parse the `Display` form of a [`FileFingerprint`] (`size=… hash=0x…`).
fn parse_fingerprint(value: &str) -> Option<FileFingerprint> {
    let mut size = None;
    let mut hash = None;
    for part in value.split_whitespace() {
        if let Some(rest) = part.strip_prefix("size=") {
            size = rest.parse::<u64>().ok();
        } else if let Some(rest) = part.strip_prefix("hash=") {
            hash = rest
                .strip_prefix("0x")
                .and_then(|hex| u64::from_str_radix(hex, 16).ok());
        }
    }
    Some(FileFingerprint {
        size: size?,
        hash: hash?,
    })
}

/// Record the deployed fingerprints in `state_dir` for diagnostics and for the
/// idempotent short-circuit.
fn write_deploy_stamp(context: &DeployContext) -> Result<()> {
    let source = context.built_binary_path();
    let destination = &context.install_path;
    let source_fingerprint = file_fingerprint(&source)?;
    let destination_fingerprint = file_fingerprint(destination)?;
    let build_inputs_fingerprint = build_inputs_fingerprint(context)?;
    fs::create_dir_all(&context.state_dir)
        .with_context(|| format!("could not create state dir {}", context.state_dir.display()))?;
    let path = deploy_stamp_path(context);
    let temporary = context
        .state_dir
        .join(format!("{}.stamp.tmp", context.binary_name));
    let deployed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let contents = format!(
        "schema=2\nbinary={}\nsource={}\ndestination={}\nsource_fingerprint={source_fingerprint}\ndestination_fingerprint={destination_fingerprint}\nbuild_inputs_fingerprint={build_inputs_fingerprint}\ndeployed_at={deployed_at}\n",
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

    /// The name this step reports itself under.
    fn name(&self) -> String {
        match self {
            Self::CargoBuild(_) => "cargo build".to_owned(),
            Self::Rotate(_) => "rotate".to_owned(),
            Self::Run(step) => format!("run {}", step.program.display()),
            Self::Custom(_) => "custom".to_owned(),
        }
    }

    fn run(self, context: &DeployContext, reporter: &mut dyn Reporter) -> Result<DeployStepReport> {
        let name = self.name();
        reporter.step_started(&name, &self.describe(context));
        let started = Instant::now();
        let mut report = self.run_inner(context, reporter)?;
        report.duration = Some(started.elapsed());
        reporter.step_finished(&report);
        Ok(report)
    }

    fn run_inner(
        self,
        context: &DeployContext,
        reporter: &mut dyn Reporter,
    ) -> Result<DeployStepReport> {
        match self {
            Self::CargoBuild(step) => step.run(context, reporter),
            Self::Rotate(step) => step.run(context, reporter),
            Self::Run(step) => step.run(context, reporter),
            Self::Custom(mut step) => {
                step(context).with_context(|| "step 'custom' failed")?;
                Ok(DeployStepReport {
                    name: "custom".to_owned(),
                    status: StepStatus::Success,
                    detail: None,
                    duration: None,
                    exit_code: None,
                    artifacts: Vec::new(),
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

    fn run(self, context: &DeployContext, reporter: &mut dyn Reporter) -> Result<DeployStepReport> {
        let line = self.command_line(context);
        let command = line.join(" ");
        reporter.note(&command);
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
            duration: None,
            exit_code: status.code(),
            artifacts: vec![context.built_binary_path()],
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
        // Record the strategy (including the Windows degradation of `Atomic`)
        // so a dry-run plan never promises a guarantee the platform cannot
        // deliver.
        line.push_str(&format!(" (strategy: {}", self.strategy.label()));
        #[cfg(windows)]
        if self.strategy == RotateStrategy::Atomic {
            line.push_str(" -> rename-aside on Windows");
        }
        line.push(')');
        line
    }

    fn run(
        self,
        context: &DeployContext,
        _reporter: &mut dyn Reporter,
    ) -> Result<DeployStepReport> {
        let description = self.description();
        let outcome = rotate(&RotateOptions {
            source: self.source,
            destination: self.destination,
            backup_dir: self.backup_dir,
            strategy: self.strategy,
            backup_retention: self.backup_retention,
            backup_max_age: self.backup_max_age,
            dry_run: context.dry_run,
            // The deferred-cleanup policy belongs to the rotation ticket;
            // `None` keeps the best-effort cleanup `RotateStep` always had.
            backup_cleanup_retry: None,
        })
        .with_context(|| format!("step 'rotate' failed: {description}"))?;
        // The rotation prints nothing (0.1.0 behaviour); what it produced is
        // reported through the artifacts instead.
        let (detail, artifacts) = match &outcome {
            RotateOutcome::WouldReplace { destination } => (
                format!("would replace {}", destination.display()),
                Vec::new(),
            ),
            RotateOutcome::Replaced { destination } => (
                format!("replaced {}", destination.display()),
                vec![destination.clone()],
            ),
            RotateOutcome::ReplacedAfterRename {
                destination,
                backup,
            } => (
                format!(
                    "replaced {}; previous binary preserved at {}",
                    destination.display(),
                    backup.display()
                ),
                vec![destination.clone(), backup.clone()],
            ),
        };
        Ok(DeployStepReport {
            name: "rotate".to_owned(),
            status: StepStatus::Success,
            detail: Some(detail),
            duration: None,
            exit_code: None,
            artifacts,
        })
    }
}

/// What a [`RunStep`] ran and what came back: the argument of the predicate
/// attached with [`RunStep::verify`].
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// The program that was spawned.
    pub program: PathBuf,
    /// The arguments the program was spawned with.
    pub args: Vec<String>,
    /// The working directory of the child; `None` means the deploy process's
    /// own directory.
    pub current_dir: Option<PathBuf>,
    /// Whether the child exited zero.
    pub success: bool,
    /// The child's exit code; `None` when it was terminated by a signal.
    pub exit_code: Option<i32>,
    /// The captured stdout; empty unless [`RunStep::capture_stdout`] is set.
    pub stdout: String,
    /// The captured stderr; empty unless [`RunStep::capture_stderr`] is set or
    /// [`RunStep::allow_failure`] captured it for a failure report.
    pub stderr: String,
}

/// The predicate a [`StepVerify`] holds: shared so that a [`RunStep`] stays
/// `Clone`.
type VerifyPredicate = Arc<dyn Fn(&RunOutcome) -> Result<()>>;

/// The predicate attached to a [`RunStep`] by [`RunStep::verify`].
///
/// It is kept behind an [`Arc`] so that a step stays `Clone`, and is called at
/// most once per run. Any `Fn` is accepted: the deploy plan runs its steps on
/// one thread, exactly like the `Custom` steps that take a
/// `Box<dyn FnMut(&DeployContext) -> Result<()>>`.
#[derive(Clone)]
pub struct StepVerify(VerifyPredicate);

impl StepVerify {
    /// Wrap `verify`.
    pub fn new<F>(verify: F) -> Self
    where
        F: Fn(&RunOutcome) -> Result<()> + 'static,
    {
        Self(Arc::new(verify))
    }

    /// Check `outcome`, returning the predicate's error unchanged.
    pub fn call(&self, outcome: &RunOutcome) -> Result<()> {
        (self.0)(outcome)
    }
}

impl fmt::Debug for StepVerify {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StepVerify(<predicate>)")
    }
}

#[derive(Debug, Clone)]
pub struct RunStep {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub current_dir: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub allow_failure: bool,
    /// Capture the child's stdout into [`RunOutcome::stdout`] and the report
    /// detail.
    pub capture_stdout: bool,
    /// Capture the child's stderr into [`RunOutcome::stderr`] and the report
    /// detail; [`RunStep::allow_failure`] captures it on its own.
    pub capture_stderr: bool,
    /// Kill the child and fail the step when it is still running after this
    /// long.
    pub timeout: Option<Duration>,
    /// Require this exit code instead of the "the child succeeded" default.
    pub expected_status: Option<i32>,
    /// Predicate checked once the exit status was accepted.
    pub verify: Option<StepVerify>,
}

impl RunStep {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: Vec::new(),
            allow_failure: false,
            capture_stdout: false,
            capture_stderr: false,
            timeout: None,
            expected_status: None,
            verify: None,
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

    /// Capture the child's stdout: it reaches [`RunStep::verify`] as
    /// [`RunOutcome::stdout`] and the step report as a tail of it.
    pub fn capture_stdout(mut self) -> Self {
        self.capture_stdout = true;
        self
    }

    /// Capture the child's stderr: it reaches [`RunStep::verify`] as
    /// [`RunOutcome::stderr`] and the step report as a tail of it.
    pub fn capture_stderr(mut self) -> Self {
        self.capture_stderr = true;
        self
    }

    /// Fail the step when the child is still running after `timeout`, killing
    /// it first ([`Child::kill`]) so the deploy cannot hang on a command that
    /// never returns. The error names the program, its arguments, the working
    /// directory, the timeout, and the exit status after the kill.
    ///
    /// A timeout fails the step even with [`RunStep::allow_failure`]: the
    /// command did not finish, so an exit code would say nothing about it.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Require the child to exit with this status instead of the default "the
    /// child must succeed". A mismatch fails the step, or - with
    /// [`RunStep::allow_failure`] - is reported as an ignored failure.
    pub fn expected_status(mut self, status: i32) -> Self {
        self.expected_status = Some(status);
        self
    }

    /// Check a predicate once the exit status was accepted. It receives the
    /// [`RunOutcome`], including whatever [`RunStep::capture_stdout`] and
    /// [`RunStep::capture_stderr`] captured, and fails the step when it returns
    /// an error.
    pub fn verify<F>(mut self, verify: F) -> Self
    where
        F: Fn(&RunOutcome) -> Result<()> + 'static,
    {
        self.verify = Some(StepVerify::new(verify));
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

    fn run(
        self,
        _context: &DeployContext,
        reporter: &mut dyn Reporter,
    ) -> Result<DeployStepReport> {
        let description = self.description();
        reporter.note(&description);
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(dir) = &self.current_dir {
            command.current_dir(dir);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        // `allow_failure` needs the stderr tail for the report; the capture
        // switches add the streams the caller asked for.
        if self.capture_stdout {
            command.stdout(Stdio::piped());
        }
        if self.capture_stderr || self.allow_failure {
            command.stderr(Stdio::piped());
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "step '{description}' failed: could not spawn `{}`",
                self.program.display()
            )
        })?;
        // Drain the pipes while the child runs: a full pipe would block it.
        let stdout = child.stdout.take().map(read_pipe);
        let stderr = child.stderr.take().map(read_pipe);
        let waited = match self.timeout {
            Some(timeout) => wait_with_timeout(&mut child, timeout),
            None => child
                .wait()
                .map(Waited::Exited)
                .map_err(anyhow::Error::from),
        }
        .with_context(|| {
            format!(
                "step '{description}' failed: could not wait for `{}`",
                self.program.display()
            )
        })?;
        let stdout = join_pipe(stdout);
        let stderr = join_pipe(stderr);
        let status = match waited {
            Waited::Exited(status) => status,
            Waited::TimedOut(status) => {
                let timeout = self.timeout.unwrap_or_default();
                anyhow::bail!(
                    "step '{description}' timed out after {timeout:?} and the child was killed ({})",
                    exit_code_text(&status)
                );
            }
        };
        let outcome = RunOutcome {
            program: self.program.clone(),
            args: self.args.clone(),
            current_dir: self.current_dir.clone(),
            success: status.success(),
            exit_code: status.code(),
            stdout,
            stderr,
        };
        let mismatched = match self.expected_status {
            Some(expected) => status.code() != Some(expected),
            None => !status.success(),
        };
        if mismatched && !self.allow_failure {
            if let Some(expected) = self.expected_status {
                anyhow::bail!(
                    "step '{description}' failed: expected exit code {expected} but got {}",
                    exit_code_text(&status)
                );
            }
            anyhow::bail!("step '{description}' failed with {status}");
        }
        let mut chunks: Vec<String> = Vec::new();
        if mismatched {
            // An allowed failure keeps the exit code and the stderr tail.
            let mut detail = format!("exit {status} (allowed failure)");
            if let Some(tail) = captured_tail(&outcome.stderr) {
                detail.push_str(&format!("; stderr tail:\n{tail}"));
            }
            chunks.push(detail);
        } else if self.capture_stderr
            && let Some(tail) = captured_tail(&outcome.stderr)
        {
            chunks.push(format!("stderr tail:\n{tail}"));
        }
        if self.capture_stdout
            && let Some(tail) = captured_tail(&outcome.stdout)
        {
            chunks.push(format!("stdout tail:\n{tail}"));
        }
        if let Some(verify) = &self.verify {
            verify
                .call(&outcome)
                .with_context(|| format!("step '{description}' failed verification"))?;
        }
        Ok(DeployStepReport {
            name: format!("run {}", self.program.display()),
            status: StepStatus::Success,
            detail: (!chunks.is_empty()).then(|| chunks.join("\n")),
            duration: None,
            exit_code: outcome.exit_code,
            artifacts: Vec::new(),
        })
    }
}

/// How long [`wait_with_timeout`] sleeps between two child-status polls.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How waiting for a child ended.
enum Waited {
    /// The child exited on its own.
    Exited(ExitStatus),
    /// The timeout expired: the child was killed and reaped, and this is its
    /// status after the kill.
    TimedOut(ExitStatus),
}

/// Wait for `child` for at most `timeout`, killing it when the timeout
/// expires: a deploy must not hang on a command that never returns.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<Waited> {
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .context("could not check whether the child process is still running")?
        {
            return Ok(Waited::Exited(status));
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            break;
        }
        std::thread::sleep(WAIT_POLL_INTERVAL.min(timeout - elapsed));
    }
    child
        .kill()
        .context("could not kill the child process that timed out")?;
    let status = child
        .wait()
        .context("could not reap the child process that timed out")?;
    Ok(Waited::TimedOut(status))
}

/// Read a child pipe to a string in the background, so a full pipe cannot
/// block the child while the parent waits for it.
fn read_pipe<R: Read + Send + 'static>(mut pipe: R) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = pipe.read_to_string(&mut buffer);
        buffer
    })
}

/// Join a pipe reader, tolerating a reader thread that did not finish.
fn join_pipe(handle: Option<JoinHandle<String>>) -> String {
    match handle {
        Some(handle) => handle.join().unwrap_or_default(),
        None => String::new(),
    }
}

/// The exit status of a process as it appears in error messages.
fn exit_code_text(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => format!("no exit code ({status})"),
    }
}

/// The tail of a captured stream, or `None` when it is empty.
fn captured_tail(text: &str) -> Option<String> {
    tail_lines(text, 20).filter(|tail| !tail.is_empty())
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
    /// How long the step took; `None` for a step that did not run (a skipped
    /// one), because there is nothing to measure then.
    pub duration: Option<Duration>,
    /// The exit code of the process the step ran, when it ran one.
    pub exit_code: Option<i32>,
    /// The files the step created or replaced: the installed binary - and the
    /// backup it was moved aside to - for `rotate`, the built binary for
    /// `cargo build`, and nothing for a step that did not run.
    pub artifacts: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        rc::Rc,
        time::{Duration, Instant},
    };

    use super::{
        CargoBuildStep, DeployStep, DeployStepReport, NullReporter, Reporter, RotateStep, RunStep,
        StdoutReporter, StepStatus, default_reporter, deploy, skip_current_report,
        verify_installed_binary, write_deploy_stamp,
    };
    use crate::context::{DeployContext, build_inputs_fingerprint};
    use crate::rotate::RotateStrategy;

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
        let error = step
            .run(&context, &mut NullReporter)
            .expect_err("cargo build must fail");

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
            .run(&context, &mut NullReporter)
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
        write_deploy_stamp(&context).expect("stamp");

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
        write_deploy_stamp(&context).expect("stamp");

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
        let error = verify_installed_binary(&context).expect_err("mismatch must fail");
        let text = format!("{error:#}");
        assert!(text.contains("does not match"), "text: {text}");
        assert!(text.contains(&source.display().to_string()), "text: {text}");
        assert!(
            text.contains(&destination.display().to_string()),
            "text: {text}"
        );

        std::fs::write(&destination, "new").expect("destination");
        verify_installed_binary(&context).expect("matching fingerprints pass");
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
        context.workspace_root = dir.clone();
        context.state_dir = dir.join("state");
        context.source_binary = Some(source);
        context.install_path = destination;

        write_deploy_stamp(&context).expect("write stamp");

        let stamp = std::fs::read_to_string(context.state_dir.join("tracel-deploy-test.stamp"))
            .expect("stamp file");
        assert!(stamp.contains("schema=2"), "stamp: {stamp}");
        assert!(stamp.contains("source_fingerprint="), "stamp: {stamp}");
        assert!(stamp.contains("destination_fingerprint="), "stamp: {stamp}");
        assert!(
            stamp.contains(&format!(
                "build_inputs_fingerprint={}",
                build_inputs_fingerprint(&context).expect("fingerprint")
            )),
            "stamp: {stamp}"
        );
        assert!(stamp.contains("deployed_at="), "stamp: {stamp}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Temp directory with unique name for one test.
    fn temp_workspace(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "tracel-deploy-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// A workspace with build inputs, a matching built/installed "binary" and
    /// a freshly written deployment stamp.
    fn stamped_workspace(label: &str) -> (std::path::PathBuf, DeployContext) {
        let dir = temp_workspace(label);
        std::fs::create_dir_all(dir.join("src")).expect("src dir");
        // Deliberately not the deployed package: `cargo build -p
        // tracel-deploy-test` must fail fast when the plan really runs.
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"other-package\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .expect("manifest");
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").expect("source file");
        let source = dir.join("built-binary");
        let destination = dir.join("installed");
        std::fs::write(&source, "built bytes").expect("built binary");
        std::fs::write(&destination, "built bytes").expect("installed binary");

        let context = DeployContext::builder(&dir, "tracel-deploy-test")
            .quiet(true)
            .source(&source)
            .install_to(&destination)
            .state_dir(dir.join("state"))
            .build();
        write_deploy_stamp(&context).expect("stamp");
        (dir, context)
    }

    #[test]
    fn changed_build_inputs_are_not_skipped() {
        let (dir, context) = stamped_workspace("skip-changed-inputs");
        std::fs::write(dir.join("src/main.rs"), "fn main() { /* changed */ }\n").expect("edit");

        assert!(
            skip_current_report(&context).expect("compare").is_none(),
            "source changed after the stamp was written; the plan must not be skipped"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_or_legacy_stamps_do_not_skip() {
        let (dir, context) = stamped_workspace("skip-missing-stamp");
        let stamp = context
            .state_dir
            .join(format!("{}.stamp", context.binary_name));
        std::fs::remove_file(&stamp).expect("remove stamp");
        assert!(
            skip_current_report(&context).expect("compare").is_none(),
            "a missing stamp must not skip"
        );

        std::fs::create_dir_all(&context.state_dir).expect("state dir");
        std::fs::write(
            &stamp,
            "schema=1\nbinary=tracel-deploy-test\nsource_fingerprint=size=4 hash=0x1\ndestination_fingerprint=size=4 hash=0x1\ndeployed_at=0\n",
        )
        .expect("legacy stamp");
        assert!(
            skip_current_report(&context).expect("compare").is_none(),
            "a legacy stamp without build inputs must not skip"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn changed_build_inputs_run_the_default_plan() {
        let (dir, context) = stamped_workspace("run-changed-inputs");
        std::fs::write(dir.join("src/main.rs"), "fn main() { /* changed */ }\n").expect("edit");

        let error = deploy(&context)
            .run()
            .expect_err("changed build inputs must run the plan");
        let text = format!("{error:#}");
        assert!(text.contains("cargo build"), "text: {text}");
        assert_eq!(
            std::fs::read_to_string(&context.install_path).expect("destination"),
            "built bytes",
            "a failed plan must not touch the installed binary"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn installed_artifact_drift_is_not_skipped() {
        let (dir, context) = stamped_workspace("skip-drifted-install");
        std::fs::write(&context.install_path, "tampered").expect("tamper");

        assert!(
            skip_current_report(&context).expect("compare").is_none(),
            "an installed binary that differs from the stamp must not skip"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn force_runs_the_default_plan_even_when_nothing_changed() {
        let (dir, context) = stamped_workspace("force-run");
        let mut forced = context.clone();
        forced.force = true;

        let error = deploy(&forced)
            .run()
            .expect_err("force must run the plan even when nothing changed");
        let text = format!("{error:#}");
        assert!(text.contains("cargo build"), "text: {text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn default_plan_switches_can_disable_the_skip() {
        let (dir, context) = stamped_workspace("switches-off");

        let error = deploy(&context)
            .idempotent_skip(false)
            .run()
            .expect_err("disabling the skip must run the build");
        let text = format!("{error:#}");
        assert!(text.contains("cargo build"), "text: {text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn without_defaults_idempotent_skip_skips_unchanged_work() {
        let (dir, context) = stamped_workspace("without-defaults-skip");
        let ran = Rc::new(RefCell::new(false));
        let step_ran = Rc::clone(&ran);

        let report = deploy(&context)
            .without_defaults()
            .idempotent_skip(true)
            .step(DeployStep::Custom(Box::new(move |_| {
                *step_ran.borrow_mut() = true;
                Ok(())
            })))
            .run()
            .expect("skipped plan");

        assert!(
            !*ran.borrow(),
            "the custom step must not run when the deploy is skipped"
        );
        assert!(
            report
                .steps
                .iter()
                .all(|step| step.status == StepStatus::Skipped),
            "steps: {:?}",
            report.steps
        );
        assert_eq!(
            std::fs::read_to_string(&context.install_path).expect("installed"),
            "built bytes"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn without_defaults_keeps_the_old_behaviour_without_explicit_switches() {
        let (dir, context) = stamped_workspace("without-defaults-legacy");
        let ran = Rc::new(RefCell::new(false));
        let step_ran = Rc::clone(&ran);
        let stamp = context
            .state_dir
            .join(format!("{}.stamp", context.binary_name));
        std::fs::remove_file(&stamp).expect("remove stamp");

        deploy(&context)
            .without_defaults()
            .step(DeployStep::Custom(Box::new(move |_| {
                *step_ran.borrow_mut() = true;
                Ok(())
            })))
            .run()
            .expect("plan without defaults");

        assert!(
            *ran.borrow(),
            "a caller-owned plan keeps running its steps and is not skipped"
        );
        assert!(!stamp.exists(), "no stamp is written by default");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn without_defaults_verify_install_checks_the_installed_binary() {
        let (dir, context) = stamped_workspace("without-defaults-verify");
        std::fs::write(&context.install_path, "drifted").expect("drift");

        let error = deploy(&context)
            .without_defaults()
            .idempotent_skip(true)
            .verify_install(true)
            .step(DeployStep::Custom(Box::new(|_| Ok(()))))
            .run()
            .expect_err("verification must fail on a drifted install");
        let text = format!("{error:#}");
        assert!(text.contains("does not match"), "text: {text}");

        deploy(&context)
            .without_defaults()
            .idempotent_skip(true)
            .verify_install(false)
            .step(DeployStep::Custom(Box::new(|_| Ok(()))))
            .run()
            .expect("verification is switched off");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn without_defaults_write_deploy_stamp_records_build_inputs() {
        let (dir, context) = stamped_workspace("without-defaults-stamp");
        let stamp = context
            .state_dir
            .join(format!("{}.stamp", context.binary_name));
        std::fs::remove_file(&stamp).expect("remove stamp");

        deploy(&context)
            .without_defaults()
            .idempotent_skip(true)
            .write_deploy_stamp(true)
            .step(DeployStep::Custom(Box::new(|_| Ok(()))))
            .run()
            .expect("plan without a stamp");

        let contents = std::fs::read_to_string(&stamp).expect("stamp file");
        assert!(contents.contains("schema=2"), "stamp: {contents}");
        assert!(
            contents.contains(&format!(
                "build_inputs_fingerprint={}",
                build_inputs_fingerprint(&context).expect("fingerprint")
            )),
            "stamp: {contents}"
        );

        std::fs::remove_file(&stamp).expect("remove stamp");
        deploy(&context)
            .without_defaults()
            .idempotent_skip(true)
            .write_deploy_stamp(false)
            .step(DeployStep::Custom(Box::new(|_| Ok(()))))
            .run()
            .expect("plan without the stamp switch");
        assert!(!stamp.exists(), "the stamp must not be written");
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- RunStep capabilities, structured reports, and pluggable output ---

    /// A step that writes `boom` to stderr and exits with 3.
    fn failing_step() -> RunStep {
        #[cfg(windows)]
        let step = RunStep::new("cmd").args(["/C", "echo boom 1>&2 & exit 3"]);
        #[cfg(unix)]
        let step = RunStep::new("sh").args(["-c", "echo boom 1>&2; exit 3"]);
        step
    }

    /// A step that prints `out` on stdout and `err` on stderr and exits 0.
    fn greeter_step() -> RunStep {
        #[cfg(windows)]
        let step = RunStep::new("cmd").args(["/C", "echo out & echo err 1>&2"]);
        #[cfg(unix)]
        let step = RunStep::new("sh").args(["-c", "echo out; echo err 1>&2"]);
        step
    }

    /// A [`RotateStep`] for the rotate-report tests.
    fn rotate_step(
        source: std::path::PathBuf,
        destination: std::path::PathBuf,
        backup_dir: Option<std::path::PathBuf>,
        strategy: RotateStrategy,
    ) -> RotateStep {
        RotateStep {
            source,
            destination,
            backup_dir,
            strategy,
            backup_retention: None,
            backup_max_age: None,
        }
    }

    #[test]
    fn run_step_captures_stdout_and_stderr() {
        let context = test_context();
        let observed = Rc::new(RefCell::new(None::<(String, String)>));
        let observed_in_step = Rc::clone(&observed);

        let report = deploy(&context)
            .without_defaults()
            .step(DeployStep::Run(
                greeter_step()
                    .capture_stdout()
                    .capture_stderr()
                    .verify(move |outcome| {
                        *observed_in_step.borrow_mut() =
                            Some((outcome.stdout.clone(), outcome.stderr.clone()));
                        Ok(())
                    }),
            ))
            .run()
            .expect("captured run");

        let (stdout, stderr) = observed
            .borrow()
            .clone()
            .expect("the predicate saw the outcome");
        assert!(stdout.contains("out"), "stdout: {stdout:?}");
        assert!(stderr.contains("err"), "stderr: {stderr:?}");
        assert_eq!(report.steps[0].exit_code, Some(0));
        assert!(report.steps[0].duration.is_some());
        let detail = report.steps[0].detail.clone().unwrap_or_default();
        assert!(detail.contains("stdout tail"), "detail: {detail}");
        assert!(detail.contains("stderr tail"), "detail: {detail}");
    }

    #[test]
    fn run_step_timeout_kills_the_child() {
        let dir = temp_workspace("run-timeout");
        let context = test_context();

        #[cfg(windows)]
        let step = RunStep::new("cmd").args(["/C", "ping -n 6 127.0.0.1 >nul"]);
        #[cfg(unix)]
        let step = RunStep::new("sh").args(["-c", "sleep 5"]);

        let started = Instant::now();
        let error = step
            .current_dir(&dir)
            .timeout(Duration::from_millis(250))
            .run(&context, &mut NullReporter)
            .expect_err("the timeout must fail the step");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(4),
            "the child must be killed instead of awaited: {elapsed:?}"
        );
        let text = format!("{error:#}");
        assert!(text.contains("timed out after 250ms"), "text: {text}");
        assert!(text.contains(&dir.display().to_string()), "cwd: {text}");
        #[cfg(windows)]
        {
            assert!(text.contains("run cmd"), "program: {text}");
            assert!(text.contains("ping -n 6"), "args: {text}");
            assert!(text.contains("exit code 1"), "exit code: {text}");
        }
        #[cfg(unix)]
        {
            assert!(text.contains("run sh"), "program: {text}");
            assert!(text.contains("sleep 5"), "args: {text}");
            assert!(text.contains("no exit code"), "exit code: {text}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_step_expected_status_mismatch_is_an_error() {
        let context = test_context();

        let error = failing_step()
            .expected_status(0)
            .run(&context, &mut NullReporter)
            .expect_err("a mismatching exit code must fail the step");

        let text = format!("{error:#}");
        assert!(text.contains("expected exit code 0"), "text: {text}");
        assert!(text.contains("exit code 3"), "text: {text}");
    }

    #[test]
    fn run_step_expected_status_accepts_that_code() {
        let context = test_context();

        let report = failing_step()
            .expected_status(3)
            .run(&context, &mut NullReporter)
            .expect("the expected exit code is not a failure");

        assert_eq!(report.status, StepStatus::Success);
        assert_eq!(report.exit_code, Some(3));
    }

    #[test]
    fn run_step_verify_failure_is_an_error() {
        let context = test_context();

        let error = greeter_step()
            .capture_stdout()
            .verify(|outcome| {
                anyhow::ensure!(
                    outcome.stdout.contains("goodbye"),
                    "the script was expected to greet us, stdout was {:?}",
                    outcome.stdout
                );
                Ok(())
            })
            .run(&context, &mut NullReporter)
            .expect_err("a failing predicate must fail the step");

        let text = format!("{error:#}");
        assert!(text.contains("failed verification"), "text: {text}");
        assert!(text.contains("stdout was"), "text: {text}");
    }

    #[test]
    fn allowed_failure_reports_exit_code_and_stderr_tail() {
        let context = test_context();

        let report = deploy(&context)
            .without_defaults()
            .step(DeployStep::Run(failing_step().allow_failure(true)))
            .run()
            .expect("an allowed failure still succeeds");

        assert_eq!(report.steps[0].status, StepStatus::Success);
        assert_eq!(report.steps[0].exit_code, Some(3));
        assert!(report.steps[0].duration.is_some());
        let detail = report.steps[0].detail.clone().unwrap_or_default();
        assert!(detail.contains("allowed failure"), "detail: {detail}");
        assert!(detail.contains("stderr tail"), "detail: {detail}");
        assert!(detail.contains("boom"), "detail: {detail}");
    }

    #[test]
    fn rotate_reports_artifacts_and_duration() {
        let dir = temp_workspace("rotate-report");
        let source = dir.join("source");
        let destination = dir.join("installed");
        std::fs::write(&source, "new bytes").expect("source");
        let mut context = test_context();
        context.source_binary = Some(source.clone());
        context.install_path = destination.clone();

        let report = deploy(&context)
            .without_defaults()
            .step(DeployStep::Rotate(rotate_step(
                source,
                destination.clone(),
                None,
                RotateStrategy::Auto,
            )))
            .run()
            .expect("rotate step");

        assert_eq!(report.steps[0].status, StepStatus::Success);
        assert!(
            report.steps[0].duration.is_some(),
            "a step that ran reports how long it took"
        );
        assert!(report.steps[0].exit_code.is_none());
        assert!(
            report.steps[0].artifacts.contains(&destination),
            "artifacts: {:?}",
            report.steps[0].artifacts
        );
        assert_eq!(
            std::fs::read_to_string(&destination).expect("installed"),
            "new bytes"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rotate_reports_the_backup_it_replaced() {
        let dir = temp_workspace("rotate-backup");
        let source = dir.join("source");
        let destination = dir.join("installed");
        std::fs::write(&source, "new bytes").expect("source");
        std::fs::write(&destination, "old bytes").expect("destination");
        let mut context = test_context();
        context.source_binary = Some(source.clone());
        context.install_path = destination.clone();

        let report = deploy(&context)
            .without_defaults()
            .step(DeployStep::Rotate(rotate_step(
                source,
                destination.clone(),
                None,
                RotateStrategy::RenameAside,
            )))
            .run()
            .expect("rename-aside rotation");

        let artifacts = &report.steps[0].artifacts;
        assert_eq!(
            artifacts.first(),
            Some(&destination),
            "artifacts: {artifacts:?}"
        );
        let backup = artifacts.get(1).cloned().expect("the backup artifact");
        assert_ne!(backup, destination);
        assert!(backup.is_file(), "backup {backup:?} must exist");
        assert_eq!(
            std::fs::read_to_string(&backup).expect("backup"),
            "old bytes"
        );
        assert_eq!(
            std::fs::read_to_string(&destination).expect("installed"),
            "new bytes"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Reporter that records every event it receives, in order.
    #[derive(Default)]
    struct RecordingReporter {
        events: Rc<RefCell<Vec<String>>>,
    }

    impl RecordingReporter {
        fn new(events: &Rc<RefCell<Vec<String>>>) -> Self {
            Self {
                events: Rc::clone(events),
            }
        }

        fn record(&self, event: String) {
            self.events.borrow_mut().push(event);
        }
    }

    impl Reporter for RecordingReporter {
        fn plan(&mut self, lines: &[String]) {
            for line in lines {
                self.record(format!("plan: {line}"));
            }
        }

        fn step_started(&mut self, name: &str, description: &str) {
            self.record(format!("started: {name}: {description}"));
        }

        fn note(&mut self, message: &str) {
            self.record(format!("note: {message}"));
        }

        fn step_finished(&mut self, report: &DeployStepReport) {
            self.record(format!(
                "finished: {} {:?} duration={:?}",
                report.name, report.status, report.duration
            ));
        }

        fn warning(&mut self, message: &str) {
            self.record(format!("warning: {message}"));
        }
    }

    #[test]
    fn dry_run_plan_goes_through_the_reporter() {
        let mut context = test_context();
        context.dry_run = true;
        let events = Rc::new(RefCell::new(Vec::new()));

        let report = deploy(&context)
            .reporter(RecordingReporter::new(&events))
            .run()
            .expect("dry-run plan");

        assert!(!report.plan.is_empty(), "plan: {:?}", report.plan);
        let recorded: Vec<String> = events.borrow().clone();
        assert_eq!(recorded.len(), report.plan.len(), "recorded: {recorded:?}");
        for (event, line) in recorded.iter().zip(&report.plan) {
            assert_eq!(event, &format!("plan: {line}"));
        }
    }

    #[test]
    fn executed_steps_go_through_the_reporter() {
        // The default test context is quiet; an injected reporter runs anyway.
        let context = test_context();
        assert!(context.quiet);
        let events = Rc::new(RefCell::new(Vec::new()));

        let report = deploy(&context)
            .without_defaults()
            .reporter(RecordingReporter::new(&events))
            .step(DeployStep::Run(greeter_step()))
            .run()
            .expect("run step");

        assert_eq!(report.steps.len(), 1);
        let recorded: Vec<String> = events.borrow().clone();
        for prefix in ["started: run ", "note: run ", "finished: run "] {
            assert!(
                recorded.iter().any(|event| event.starts_with(prefix)),
                "missing {prefix} in {recorded:?}"
            );
        }
        assert!(
            recorded
                .iter()
                .any(|event| event.contains("duration=Some(")),
            "the finished event carries the duration: {recorded:?}"
        );
    }

    #[test]
    fn default_reporter_follows_context_quiet() {
        let mut context = test_context();
        assert!(context.quiet);
        assert!(default_reporter(&context).is_quiet());

        context.quiet = false;
        assert!(!default_reporter(&context).is_quiet());
        assert!(StdoutReporter::quiet().is_quiet());
        assert!(!StdoutReporter::new().is_quiet());
    }

    /// Run the stdout probe in a child copy of this test binary, which is the
    /// only way to look at what the default reporter really printed.
    fn stdout_probe(case: &str) -> String {
        let executable = std::env::current_exe().expect("the test binary");
        let output = std::process::Command::new(executable)
            .args([
                "--exact",
                "plan::tests::stdout_reporter_keeps_the_printed_lines",
                "--nocapture",
            ])
            .env("TRACEL_DEPLOY_PROBE_CASE", case)
            .output()
            .expect("run the stdout probe");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            stdout.contains("PROBE-DONE"),
            "the probe did not run for {case}: stdout {stdout:?}, stderr {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }

    #[test]
    fn stdout_reporter_keeps_the_printed_lines() {
        if let Some(case) = std::env::var_os("TRACEL_DEPLOY_PROBE_CASE") {
            let case = case.to_string_lossy().into_owned();
            let mut context = test_context();
            context.dry_run = true;
            context.quiet = case == "quiet";
            match case.as_str() {
                "injected" => {
                    deploy(&context)
                        .reporter(NullReporter)
                        .run()
                        .expect("dry-run plan");
                }
                _ => {
                    deploy(&context).run().expect("dry-run plan");
                }
            }
            println!("PROBE-DONE");
            return;
        }

        let verbose = stdout_probe("verbose");
        assert!(
            verbose.contains("would: ") && verbose.contains("build -p tracel-deploy-test"),
            "verbose: {verbose:?}"
        );
        assert!(verbose.contains("would: rotate "), "verbose: {verbose:?}");

        let quiet = stdout_probe("quiet");
        assert!(!quiet.contains("would:"), "quiet: {quiet:?}");

        let injected = stdout_probe("injected");
        assert!(!injected.contains("would:"), "injected: {injected:?}");
    }
}
