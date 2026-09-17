//! The built-in plan entries: `cargo build`, the rotation, a command, and the
//! verification predicate.

use std::{
    fmt,
    io::Read,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};

use crate::{
    context::{DeployContext, content_matches},
    plan::DeployHook,
    plan::report::{DeployStepReport, Reporter, StepStatus},
    rotate::{BackupRetry, RotateOptions, RotateOutcome, RotateStrategy, rotate},
};
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
    pub(crate) fn describe(&self, context: &DeployContext) -> String {
        match self {
            Self::CargoBuild(step) => step.command_line(context).join(" "),
            Self::Rotate(step) => step.description(),
            Self::Run(step) => step.description(),
            Self::Custom(_) => "custom step (opaque)".to_owned(),
        }
    }

    /// The name this step reports itself under.
    pub(crate) fn name(&self) -> String {
        match self {
            Self::CargoBuild(_) => "cargo build".to_owned(),
            Self::Rotate(_) => "rotate".to_owned(),
            Self::Run(step) => format!("run {}", step.program.display()),
            Self::Custom(_) => "custom".to_owned(),
        }
    }

    pub(crate) fn run(
        self,
        context: &DeployContext,
        reporter: &mut dyn Reporter,
    ) -> Result<DeployStepReport> {
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

    pub(crate) fn run(
        self,
        context: &DeployContext,
        reporter: &mut dyn Reporter,
    ) -> Result<DeployStepReport> {
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

/// Whether the rotation would find the installed binary already current: the
/// built binary and the installed one both exist and hold the same bytes, and
/// neither a dry run nor `--force` is in play.
///
/// This is the condition a default [`RotateStep`] (`RotateStep::from_context`)
/// skips on. A consumer that has to decide *before* the rotation whether
/// anything will be installed - a service-stop entry, for example - asks this
/// instead of repeating the comparison, so both agree.
pub fn install_is_current(context: &DeployContext) -> bool {
    let built = context.built_binary_path();
    !context.dry_run
        && !context.force
        && built.is_file()
        && context.install_path.is_file()
        && content_matches(&built, &context.install_path).unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct RotateStep {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub backup_dir: Option<PathBuf>,
    pub strategy: RotateStrategy,
    pub backup_retention: Option<usize>,
    pub backup_max_age: Option<std::time::Duration>,
    /// Deferred cleanup for backups a running process still holds: `Some`
    /// registers them in `<backup_dir>/<binary>.pending-delete` so the next
    /// rotation (or [`cleanup_pending_backups`](crate::cleanup_pending_backups))
    /// retries, `None` (the default) keeps the best-effort cleanup.
    pub backup_cleanup_retry: Option<BackupRetry>,
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
            backup_cleanup_retry: None,
        }
    }

    /// Register backups that a running process still holds instead of failing
    /// the rotation - the same option [`RotateOptions::backup_cleanup_retry`]
    /// carries. Pass `Some(BackupRetry::new())` for the default policy
    /// (7 days / 32 entries).
    pub fn backup_cleanup_retry(mut self, retry: Option<BackupRetry>) -> Self {
        self.backup_cleanup_retry = retry;
        self
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

    pub(crate) fn run(
        self,
        context: &DeployContext,
        _reporter: &mut dyn Reporter,
    ) -> Result<DeployStepReport> {
        let description = self.description();
        // Cargo already answered "is the build output stale?"; this answers the
        // second question - "is the installed binary that output?". Identical
        // bytes mean the install is current: nothing is copied, no backup is
        // created, and a running binary is left alone (on Windows the Windows
        // rotation would otherwise rename it aside on every run).
        if !context.dry_run
            && !context.force
            && self.source.is_file()
            && self.destination.is_file()
            && content_matches(&self.source, &self.destination).unwrap_or(false)
        {
            return Ok(DeployStepReport {
                name: "rotate".to_owned(),
                status: StepStatus::Skipped,
                detail: Some(format!(
                    "{} is already the built binary",
                    self.destination.display()
                )),
                duration: None,
                exit_code: None,
                artifacts: Vec::new(),
            });
        }
        let outcome = rotate(&RotateOptions {
            source: self.source,
            destination: self.destination,
            backup_dir: self.backup_dir,
            strategy: self.strategy,
            backup_retention: self.backup_retention,
            backup_max_age: self.backup_max_age,
            dry_run: context.dry_run,
            // `None` keeps the best-effort cleanup `RotateStep` always had;
            // `backup_cleanup_retry(Some(..))` turns on the deferred policy.
            backup_cleanup_retry: self.backup_cleanup_retry,
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

    pub(crate) fn run(
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
