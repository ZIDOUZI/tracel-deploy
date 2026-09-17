//! The deploy report types, and the reporter a plan writes through.

use std::{path::PathBuf, time::Duration};

use crate::{context::DeployContext, plan::phase::Phase};
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
    /// The dry-run plan: one line per hook and step that would run, in
    /// execution order, the same lines as [`DeployReport::plan`].
    /// [`StdoutReporter`] renders each line as `would: <line>` unless it is
    /// quiet.
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

    /// A plan entry (a built-in step or a handler) is about to run, at `phase`.
    ///
    /// [`StdoutReporter`] prints nothing here, like [`Reporter::step_started`].
    fn handler_started(&mut self, phase: Phase, name: &str, description: &str) {
        let _ = (phase, name, description);
    }

    /// A plan entry finished; the report carries the phase it ran at. A failed
    /// entry returns an error instead of a report, and is reported through
    /// [`DeployReport::events`].
    fn handler_finished(&mut self, report: &DeployEventReport) {
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
pub(super) fn default_reporter(context: &DeployContext) -> StdoutReporter {
    StdoutReporter::with_quiet(context.quiet)
}

#[derive(Debug, Default)]
pub struct DeployReport {
    pub installed: PathBuf,
    pub dry_run: bool,
    /// The hook and step lines that would run, in execution order; filled by
    /// dry-run.
    pub plan: Vec<String>,
    /// Every entry that ran, in execution order, tagged with the [`Phase`] it
    /// ran at and carrying the failure - when one happened - in
    /// [`DeployEventReport::error`].
    pub events: Vec<DeployEventReport>,
}

impl DeployReport {
    /// The entries that ran at a timeline slot, in execution order: the 0.3
    /// `steps` view of [`DeployReport::events`].
    pub fn steps(&self) -> Vec<&DeployEventReport> {
        self.events.iter().filter(|event| event.is_step()).collect()
    }

    /// The entries that ran at a lifecycle phase, in execution order: the 0.3
    /// `hooks` view of [`DeployReport::events`].
    pub fn hooks(&self) -> Vec<&DeployEventReport> {
        self.events
            .iter()
            .filter(|event| !event.is_step())
            .collect()
    }
}

/// Completion state of a step or plan entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepStatus {
    #[default]
    Success,
    Skipped,
    /// Only a [`DeployEventReport`] carries this: a failed entry aborts the
    /// plan and is reported as an error rather than as a completed step.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// One executed plan entry: the 0.4 shape of a step or hook report.
///
/// [`DeployReport::events`] holds one per entry that ran, in execution order.
/// [`DeployReport::steps`] and [`DeployReport::hooks`] are the 0.3 views of the
/// same run: the slot phases and the lifecycle phases, respectively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployEventReport {
    /// The phase the entry ran at.
    pub phase: Phase,
    /// The name the entry reports itself under.
    pub name: String,
    /// `Success`, `Skipped` when the entry reported nothing to do, or `Failed`.
    pub status: StepStatus,
    /// The skip reason, the rotated file, the verified binary, ...
    pub detail: Option<String>,
    /// How long the entry took.
    pub duration: Option<Duration>,
    /// The exit code of the process the entry ran, when it ran one.
    pub exit_code: Option<i32>,
    /// The files the entry created or replaced.
    pub artifacts: Vec<PathBuf>,
    /// The failure, when the entry failed (the run fails with it too).
    pub error: Option<String>,
}

impl DeployEventReport {
    /// Whether the entry reported that it had nothing to do.
    pub fn is_skipped(&self) -> bool {
        self.status == StepStatus::Skipped
    }

    /// Whether the entry failed.
    pub fn is_failed(&self) -> bool {
        self.status == StepStatus::Failed
    }

    /// Whether the entry ran at a timeline slot (the 0.3 "step").
    pub fn is_step(&self) -> bool {
        self.phase.is_slot()
    }

    pub(crate) fn from_step_report(phase: Phase, report: &DeployStepReport) -> Self {
        Self {
            phase,
            name: report.name.clone(),
            status: report.status,
            detail: report.detail.clone(),
            duration: report.duration,
            exit_code: report.exit_code,
            artifacts: report.artifacts.clone(),
            error: None,
        }
    }

    pub(crate) fn failed(phase: Phase, name: String, error: &anyhow::Error) -> Self {
        Self {
            phase,
            name,
            status: StepStatus::Failed,
            detail: None,
            duration: None,
            exit_code: None,
            artifacts: Vec::new(),
            error: Some(format!("{error:#}")),
        }
    }
}
