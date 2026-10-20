use std::{
    fs,
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};

use crate::context::{
    DeployContext, DeployLock, FileFingerprint, STALE_LOCK_AFTER, build_inputs_fingerprint,
    file_fingerprint,
};

pub mod phase;

pub use phase::{Handler, Outcome, Phase, RunState};

pub type DeployHook = Box<dyn FnMut(&DeployContext) -> Result<()>>;

mod report;
mod steps;

pub use report::{
    DeployEventReport, DeployReport, DeployStepReport, NullReporter, Reporter, StdoutReporter,
    StepStatus,
};
pub use steps::{
    CargoBuildStep, DeployStep, RotateStep, RunOutcome, RunStep, StepVerify, install_is_current,
};

use report::default_reporter;

/// A composable deployment plan: the timeline of `(phase, handler)` entries.
///
/// Execution order - the same for the default plan, a caller-owned plan, and a
/// plan that mixes handlers with built-in steps:
///
/// ```text
/// preflight (dry run only) -> pre -> build -> after-build -> rotate
///     -> after-rotate -> post -> on-error (only after a failure) -> finally (always)
/// ```
///
/// Entries registered at the same phase run in registration order, and a phase
/// without entries is skipped. On failure the failing **step** stops the rest of
/// its phase and every later phase except `on-error` and `finally`; a failing
/// handler does not stop the rest of its own phase (the 0.3 hook behaviour).
pub struct DeployPlan {
    entries: Vec<(Phase, Handler)>,
}

impl DeployPlan {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// The default build + rotate steps for `context`.
    pub fn standard(context: &DeployContext) -> Vec<DeployStep> {
        vec![
            DeployStep::CargoBuild(CargoBuildStep::from_context(context)),
            DeployStep::Rotate(RotateStep::from_context(context)),
        ]
    }

    /// Register `handler` at `phase`.
    pub fn push(&mut self, phase: Phase, handler: impl Into<Handler>) {
        self.entries.push((phase, handler.into()));
    }

    /// Register a built-in step at `phase`.
    pub fn push_step(&mut self, phase: Phase, step: DeployStep) {
        self.push(phase, step);
    }

    /// The handlers registered at `phase`, in registration order.
    pub fn handlers(&self, phase: Phase) -> impl Iterator<Item = &Handler> {
        self.entries
            .iter()
            .filter(move |(entry_phase, _)| *entry_phase == phase)
            .map(|(_, handler)| handler)
    }

    /// Whether the plan has no entries at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Put `entries` in front of the plan: the default build and rotation are
    /// registered first, so a handler a caller added at the same phase runs
    /// after them.
    pub(crate) fn prepend(&mut self, mut entries: Vec<(Phase, Handler)>) {
        entries.append(&mut self.entries);
        self.entries = entries;
    }

    /// One line per entry describing what would run, in execution order.
    ///
    /// A built-in step renders as [`DeployStep::describe`], a handler as
    /// [`Handler::render`] (`after-build: stop my-service`). The deprecated 0.3
    /// builder methods keep the exact 0.3 lines (`pre: stop service`,
    /// `pre hook #1`).
    pub fn render(&self, context: &DeployContext) -> Vec<String> {
        let mut lines = Vec::new();
        if context.dry_run {
            lines.extend(self.lines_for(Phase::Preflight, context));
        }
        for phase in Phase::ORDER {
            lines.extend(self.lines_for(phase, context));
        }
        lines
    }

    fn lines_for(&self, phase: Phase, context: &DeployContext) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(entry_phase, _)| *entry_phase == phase)
            .map(|(entry_phase, handler)| handler.render(*entry_phase, context))
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
        self.run_with_state(context, reporter)
            .map(|(report, _state)| report)
    }

    /// [`run_with`](Self::run_with), plus what the run learned: a caller that
    /// has to act on "nothing was installed" or on the service instance reads
    /// the returned [`RunState`].
    pub(crate) fn run_with_state(
        self,
        context: &DeployContext,
        reporter: &mut dyn Reporter,
    ) -> Result<(DeployReport, RunState)> {
        let mut report = DeployReport {
            installed: context.install_path.clone(),
            dry_run: context.dry_run,
            plan: Vec::new(),
            events: Vec::new(),
        };
        let mut state = RunState::default();

        if context.dry_run {
            // The plan is rendered before the checks run so that it can be
            // reported even when a preflight check fails.
            let plan = self.render(context);
            let mut failures = Vec::new();
            for (phase, mut handler) in self
                .entries
                .into_iter()
                .filter(|(phase, _)| *phase == Phase::Preflight)
            {
                run_check(
                    phase,
                    &mut handler,
                    context,
                    reporter,
                    &mut report,
                    &mut state,
                    &mut failures,
                );
            }
            report.plan = plan;
            reporter.plan(&report.plan);
            if !failures.is_empty() {
                return Err(preflight_failure(failures, &report.events, &report.plan));
            }
            return Ok((report, state));
        }

        // Preflight checks are read-only checks for a dry run; a real deploy
        // never runs them.
        let mut entries: Vec<(Phase, Handler)> = self
            .entries
            .into_iter()
            .filter(|(phase, _)| *phase != Phase::Preflight)
            .collect();
        entries.sort_by_key(|(phase, _)| phase_rank(*phase));

        let mut failures = Vec::new();
        let mut failed = false;
        let mut current: Option<Phase> = None;
        let mut skip_phase = false;
        for (phase, mut handler) in entries {
            if current != Some(phase) {
                if let Some(previous) = current
                    && !failed
                {
                    state.mark_phase_done(previous);
                }
                current = Some(phase);
                skip_phase = !phase_runs(phase, failed);
            }
            if skip_phase {
                continue;
            }
            let is_step = handler.is_step();
            let label = handler.name().to_owned();
            match run_entry(
                phase,
                &mut handler,
                context,
                reporter,
                &mut report,
                &mut state,
            ) {
                Ok(()) => {}
                Err(error) => {
                    failed = true;
                    if is_step {
                        // A failing step stops the rest of its phase: the
                        // rotation of a failed deploy must not run.
                        skip_phase = true;
                        let event = DeployEventReport::failed(phase, label.clone(), &error);
                        reporter.handler_finished(&event);
                        report.events.push(event);
                        failures.push(error);
                    } else {
                        let error = error.context(format!("{label} failed"));
                        let event = DeployEventReport::failed(phase, label.clone(), &error);
                        reporter.handler_finished(&event);
                        report.events.push(event);

                        failures.push(error);
                    }
                }
            }
        }
        if let Some(last) = current
            && !failed
        {
            state.mark_phase_done(last);
        }

        let mut failures = failures.into_iter();
        if let Some(mut aggregated) = failures.next() {
            for extra in failures {
                aggregated = aggregated.context(format!("additional failure: {extra:#}"));
            }
            return Err(aggregated);
        }
        Ok((report, state))
    }
}

impl Default for DeployPlan {
    fn default() -> Self {
        Self::new()
    }
}

/// Where `phase` sits in the execution order. `Preflight` is not part of it (a
/// real deploy filters it out) and sorts last.
fn phase_rank(phase: Phase) -> usize {
    Phase::ORDER
        .iter()
        .position(|candidate| *candidate == phase)
        .unwrap_or(usize::MAX)
}

/// Whether `phase` still runs, given whether the run failed so far.
fn phase_runs(phase: Phase, failed: bool) -> bool {
    if phase.is_failure_only() {
        failed
    } else {
        !failed || phase.is_always()
    }
}

/// Run one entry.
///
/// A built-in step reports a [`DeployStepReport`] through
/// [`DeployReport::steps`]; a handler at a slot phase reports there too, and a
/// handler at a lifecycle phase reports through [`DeployReport::hooks`]. Lines a
/// handler recorded with [`RunState::note`] go to the reporter.
fn run_entry(
    phase: Phase,
    handler: &mut Handler,
    context: &DeployContext,
    reporter: &mut dyn Reporter,
    report: &mut DeployReport,
    state: &mut RunState,
) -> Result<()> {
    let name = handler.name().to_owned();
    let description = handler.render(phase, context);
    reporter.handler_started(phase, &name, &description);
    let slot = phase.is_slot();
    // A built-in step reports its own start and finish (0.1.0 behaviour); a
    // closure handler is reported here.
    let self_reporting = handler.is_step();
    if slot && !self_reporting {
        reporter.step_started(&name, &description);
    }
    let started = Instant::now();
    let notes_before = state.notes().len();
    let warnings_before = state.warnings().len();
    let outcome = handler.call(context, state, reporter)?;
    let notes: Vec<String> = state.notes()[notes_before..].to_vec();
    for note in notes {
        reporter.note(&note);
    }
    let warnings: Vec<String> = state.warnings()[warnings_before..].to_vec();
    for warning in warnings {
        reporter.warning(&warning);
    }
    if phase == Phase::Rotate {
        state.mark_rotation(!outcome.is_skipped());
    }
    let step_report = DeployStepReport {
        name: name.clone(),
        status: if outcome.is_skipped() {
            StepStatus::Skipped
        } else {
            StepStatus::Success
        },
        detail: outcome.detail().map(str::to_owned),
        duration: Some(started.elapsed()),
        exit_code: outcome.exit_code(),
        artifacts: outcome.artifacts().to_vec(),
    };
    let event = DeployEventReport::from_step_report(phase, &step_report);
    reporter.handler_finished(&event);
    report.events.push(event);
    if slot && !self_reporting {
        reporter.step_finished(&step_report);
    }
    Ok(())
}

/// Run one preflight check: a failure is collected and reported, not
/// propagated, so the error can list every check that ran.
fn run_check(
    phase: Phase,
    handler: &mut Handler,
    context: &DeployContext,
    reporter: &mut dyn Reporter,
    report: &mut DeployReport,
    state: &mut RunState,
    failures: &mut Vec<anyhow::Error>,
) {
    let label = handler.name().to_owned();
    if let Err(error) = run_entry(phase, handler, context, reporter, report, state) {
        let error = error.context(format!("{label} failed"));
        let event = DeployEventReport::failed(phase, label.clone(), &error);
        reporter.handler_finished(&event);
        report.events.push(event);
        failures.push(error);
    }
}

/// The error of a dry run whose preflight checks failed.
///
/// The failing hooks keep their cause chain and the message lists every check
/// that ran, plus the plan that was rendered instead of executed; the caller
/// gets the reason and the "what would have happened" in one error.
fn preflight_failure(
    failures: Vec<anyhow::Error>,
    events: &[DeployEventReport],
    plan: &[String],
) -> anyhow::Error {
    let checked = events
        .iter()
        .map(|event| {
            if event.error.is_none() {
                format!("{} (ok)", event.name)
            } else {
                format!("{} (failed)", event.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let planned = if plan.is_empty() {
        "nothing".to_owned()
    } else {
        plan.join("; ")
    };
    let mut failures = failures.into_iter();
    let mut error = failures
        .next()
        .expect("a preflight failure is reported together with its error");
    for extra in failures {
        error = error.context(format!("additional failure: {extra:#}"));
    }
    error.context(format!(
        "dry run: preflight failed, no deploy step was executed; checked: {checked}; planned: {planned}"
    ))
}

/// Builder returned by [`deploy`].
///
/// By default it runs:
///
/// 1. [`CargoBuildStep`] for `context.package`.
/// 2. [`RotateStep`] from `context.built_binary_path()` to
///    `context.install_path`.
///
/// [`DeployBuilder::preflight`] adds read-only checks that run only during a dry
/// run.
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

    /// Register `handler` at `phase` - the 0.4 way to add work at a point on the
    /// timeline. Several handlers at one phase run in registration order.
    ///
    /// The slots are `Build`, `AfterBuild`, `Rotate`, and `AfterRotate`; the
    /// lifecycle phases are `Preflight` (dry run only), `Pre`, `Post`,
    /// `OnError`, and `Finally`. A handler that reports [`Outcome::Skipped`] is
    /// recorded as such and does not stop the plan.
    pub fn on(mut self, phase: Phase, handler: impl Into<Handler>) -> Self {
        self.plan.push(phase, handler);
        self
    }

    /// Register `handler` at the phase it asks for with [`Handler::at`].
    ///
    /// # Panics
    ///
    /// A handler without a default phase has no place to run; register it with
    /// [`on`](Self::on) instead.
    pub fn insert(mut self, handler: impl Into<Handler>) -> Self {
        let handler = handler.into();
        let phase = handler.default_phase().unwrap_or_else(|| {
            panic!(
                "handler '{}' has no default phase; register it with on(Phase::…, handler)",
                handler.name()
            )
        });
        self.plan.push(phase, handler);
        self
    }

    /// Turn the default build + rotation plan on or off.
    ///
    /// This is the 0.4 name of [`without_defaults`](Self::without_defaults): the
    /// default plan is the [`CargoBuildStep`] at [`Phase::Build`] and the
    /// [`RotateStep`] at [`Phase::Rotate`], registered before everything the
    /// caller added.
    pub fn defaults(mut self, enabled: bool) -> Self {
        self.defaults = enabled;
        self
    }

    /// Add an unnamed hook that runs only during a dry run, for read-only
    /// checks: verifying a signature or version, whether `install_to` is
    /// writable, or taking a snapshot of the current service state.
    ///
    /// A dry run renders the plan, runs the preflight hooks, and then reports
    /// the plan through the reporter, so a failing check still shows what would
    /// have run. The hooks are reported through [`DeployReport::hooks`], and a
    /// failure fails the dry run (a non-zero exit status for a caller that maps
    /// the error onto one) with the checked items and the rendered plan in the
    /// error. A real deploy never runs preflight hooks.
    pub fn preflight<F>(self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.preflight_named_entry(None, hook)
    }

    /// Add a named preflight hook; the name is used in errors and reports.
    pub fn preflight_named<F>(self, name: impl Into<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.preflight_named_entry(Some(name.into()), hook)
    }

    fn preflight_named_entry<F>(mut self, name: Option<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        let handler = self.legacy_handler(Phase::Preflight, name, hook);
        self.plan.push(Phase::Preflight, handler);
        self
    }

    /// Add an unnamed hook that runs before the deployment steps. The usual
    /// use is stopping a service that owns the installed binary.
    pub fn pre<F>(self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.pre_named_entry(None, hook)
    }

    /// Add a named pre hook; the name is used in errors and reports.
    pub fn pre_named<F>(self, name: impl Into<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.pre_named_entry(Some(name.into()), hook)
    }

    fn pre_named_entry<F>(mut self, name: Option<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        let handler = self.legacy_handler(Phase::Pre, name, hook);
        self.plan.push(Phase::Pre, handler);
        self
    }

    /// Add an unnamed hook that runs after all deployment steps succeeded. The
    /// usual use is restarting or verifying a service.
    pub fn post<F>(self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.post_named_entry(None, hook)
    }

    /// Add a named post hook; the name is used in errors and reports.
    pub fn post_named<F>(self, name: impl Into<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.post_named_entry(Some(name.into()), hook)
    }

    fn post_named_entry<F>(mut self, name: Option<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        let handler = self.legacy_handler(Phase::Post, name, hook);
        self.plan.push(Phase::Post, handler);
        self
    }

    /// Add an unnamed hook that runs when a pre hook, a step, or a post hook
    /// failed. Use it for rollbacks, logging, or alerting. The `finally` hooks
    /// run after `on_error`, even when an `on_error` hook fails itself.
    pub fn on_error<F>(self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.on_error_named_entry(None, hook)
    }

    /// Add a named `on_error` hook; the name is used in errors and reports.
    pub fn on_error_named<F>(self, name: impl Into<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.on_error_named_entry(Some(name.into()), hook)
    }

    fn on_error_named_entry<F>(mut self, name: Option<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        let handler = self.legacy_handler(Phase::OnError, name, hook);
        self.plan.push(Phase::OnError, handler);
        self
    }

    /// Add an unnamed hook that always runs: after `post` on success, or after
    /// `on_error` on failure. The usual use is restarting a service that was
    /// stopped before the rotation, even when the rotation failed.
    pub fn finally<F>(self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.finally_named_entry(None, hook)
    }

    /// Add a named `finally` hook; the name is used in errors and reports.
    pub fn finally_named<F>(self, name: impl Into<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.finally_named_entry(Some(name.into()), hook)
    }

    fn finally_named_entry<F>(mut self, name: Option<String>, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        let handler = self.legacy_handler(Phase::Finally, name, hook);
        self.plan.push(Phase::Finally, handler);
        self
    }

    /// Wrap a 0.3 hook as a 0.4 handler.
    ///
    /// The reported name and the rendered plan line stay exactly what 0.3
    /// produced (`pre: stop service`, `pre hook #1`), so a migrated plan reads
    /// the same in dry runs, errors, and reports.
    fn legacy_handler<F>(&self, phase: Phase, name: Option<String>, hook: F) -> Handler
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        let label = legacy_label(phase);
        match name {
            Some(name) => Handler::new(name.clone(), hook).plan_line(format!("{label}: {name}")),
            None => {
                let index = self.plan.handlers(phase).count() + 1;
                let label = format!("{label} hook #{index}");
                Handler::new(label.clone(), hook).plan_line(label)
            }
        }
    }

    /// Append a step to the caller-owned part of the plan.
    ///
    /// The phase follows the plan shape: with the default plan the step lands
    /// at [`Phase::AfterRotate`] (0.3 appended it after the default rotation); in
    /// a caller-owned plan it is placed by kind ([`CargoBuildStep`] at
    /// [`Phase::Build`], [`RotateStep`] at [`Phase::Rotate`], everything else at
    /// [`Phase::AfterRotate`]), which keeps the order 0.3 executed.
    pub fn step(mut self, step: DeployStep) -> Self {
        let phase = if self.defaults {
            Phase::AfterRotate
        } else {
            step_phase(&step)
        };
        self.plan.push_step(phase, step);
        self
    }

    /// Insert a step at [`Phase::AfterBuild`]: after the build and before the
    /// rotation, the point where a service is stopped.
    ///
    /// The classic use case is `build -> stop service -> rotate -> start
    /// service`: stopping a service in a `pre` hook would stop it before the
    /// build, so a failed build keeps the service down unnecessarily.
    ///
    /// Unlike 0.3 the insertion applies to a caller-owned plan too
    /// ([`defaults(false)`](Self::defaults)): the slot is part of the timeline,
    /// not part of the default plan.
    pub fn insert_after_default_build(mut self, step: DeployStep) -> Self {
        self.plan.push_step(Phase::AfterBuild, step);
        self
    }

    /// The other name of the [`Phase::AfterBuild`] slot; steps registered with
    /// either method run in call order.
    pub fn insert_before_default_rotate(self, step: DeployStep) -> Self {
        self.insert_after_default_build(step)
    }

    /// Replace the default cargo-build + rotate plan with exactly the steps
    /// added by the caller.
    pub fn without_defaults(mut self) -> Self {
        self.defaults = false;
        self
    }

    /// Enable or disable the *before-the-build* short-circuit.
    ///
    /// It is off by default, for every plan: the default answers "is anything
    /// stale?" by letting **cargo** decide (the build step runs, and cargo
    /// no-ops when its own fingerprints say the output is current) and by
    /// comparing the built binary with the installed one (which makes the
    /// rotation a no-op instead of a rewrite). Turning the switch on restores
    /// the cheaper pre-build comparison, at the price of maintaining the
    /// build-input list and of missing everything cargo tracks but that list
    /// does not (features, profile, `RUSTFLAGS`, `include_bytes!` files, ...):
    ///
    /// ```ignore
    /// deploy(&context)
    ///     .defaults(false)
    ///     .idempotent_skip(true)
    ///     .insert(CargoBuildStep::from_context(&context).at(Phase::Build))
    ///     .run()?;
    /// ```
    ///
    /// A skip requires the deployment stamp to prove both that the build
    /// inputs still hash to the recorded value and that the installed binary
    /// still matches the recorded fingerprint. A changed source tree is never
    /// skipped just because the previously built binary happens to equal the
    /// installed one, and a missing or legacy stamp never skips.
    ///
    /// The stamp itself keeps being written after a successful rotation; with
    /// the switch off it is diagnostics, not the decision.
    pub fn idempotent_skip(mut self, enabled: bool) -> Self {
        self.idempotent_skip = Some(enabled);
        self
    }

    /// Enable or disable the install verification after the steps ran.
    ///
    /// The default follows the plan (on for the default plan, off after
    /// [`without_defaults`](Self::without_defaults)). Verification compares the
    /// built binary with the installed one and fails the deploy on a mismatch;
    /// it is not part of the skip path, and a rotation that found nothing to
    /// install (its step reports [`StepStatus::Skipped`]) skips it too.
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
    /// itself succeeded. A rotation that found nothing to install writes no new
    /// stamp, so the file keeps describing the last real install.
    pub fn write_deploy_stamp(mut self, enabled: bool) -> Self {
        self.write_deploy_stamp = Some(enabled);
        self
    }

    pub fn run(mut self) -> Result<DeployReport> {
        let skip_when_unchanged = self.idempotent_skip.unwrap_or(false);
        let verify_after_rotate = self.verify_install.unwrap_or(self.defaults);
        let stamp_after_rotate = self.write_deploy_stamp.unwrap_or(self.defaults);
        // The default plan and the two post-rotation steps are entries on the
        // timeline, registered before everything the caller added: the install
        // verification and the stamp run at `AfterRotate`, right after the
        // rotation.
        let mut entries: Vec<(Phase, Handler)> = Vec::new();
        if self.defaults {
            let mut steps = DeployPlan::standard(self.context);
            let rotate = steps
                .pop()
                .expect("the standard plan always contains a rotation step");
            let build = steps
                .pop()
                .expect("the standard plan always contains a build step");
            entries.push((Phase::Build, build.into()));
            entries.push((Phase::Rotate, rotate.into()));
        }
        if verify_after_rotate {
            entries.push((Phase::AfterRotate, verify_install_handler()));
        }
        if stamp_after_rotate {
            entries.push((Phase::AfterRotate, write_stamp_handler()));
        }
        if !entries.is_empty() {
            self.plan.prepend(entries);
        }
        let context = self.context;
        let mut reporter: Box<dyn Reporter> = self
            .reporter
            .unwrap_or_else(|| Box::new(default_reporter(context)));
        let lock = if context.dry_run {
            None
        } else {
            Some(DeployLock::acquire(
                &context.state_dir,
                &context.binary_name,
            )?)
        };
        if let Some(pid) = lock.as_ref().and_then(DeployLock::took_over) {
            reporter.note(&format!(
                "cleaned up a stale deployment lock left by pid {pid} (older than {} minutes)",
                STALE_LOCK_AFTER.as_secs() / 60
            ));
        }
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
        // The verification and the stamp are entries on the timeline, so a
        // rotation that found the install current skips them the same way every
        // other entry does (`Outcome::Skipped`), and a caller-owned plan gets
        // them too.
        let (report, _state) = self.plan.run_with_state(context, reporter.as_mut())?;
        Ok(report)
    }
}

/// The phase label 0.3 used in plans, errors, and reports (`on_error`, not
/// `on-error`), so a migrated plan reads the same as before.
fn legacy_label(phase: Phase) -> &'static str {
    match phase {
        Phase::OnError => "on_error",
        other => other.label(),
    }
}

/// The slot a built-in step belongs to in a caller-owned plan.
fn step_phase(step: &DeployStep) -> Phase {
    match step {
        DeployStep::CargoBuild(_) => Phase::Build,
        DeployStep::Rotate(_) => Phase::Rotate,
        _ => Phase::AfterRotate,
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
        events: vec![
            DeployEventReport {
                phase: Phase::Build,
                name: "cargo build".to_owned(),
                status: StepStatus::Skipped,
                detail: Some("build inputs are unchanged since the last deploy".to_owned()),
                duration: None,
                exit_code: None,
                artifacts: Vec::new(),
                error: None,
            },
            DeployEventReport {
                phase: Phase::Rotate,
                name: "rotate".to_owned(),
                status: StepStatus::Skipped,
                detail: Some(format!(
                    "{} already matches the stamped binary",
                    destination.display(),
                )),
                duration: None,
                exit_code: None,
                artifacts: Vec::new(),
                error: None,
            },
        ],
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

/// The `AfterRotate` entry that verifies the installed binary.
///
/// A rotation that found the install current installed nothing, so there is
/// nothing to verify: the entry skips, exactly like a hand-written handler
/// would.
fn verify_install_handler() -> Handler {
    Handler::with_state("verify install", |context, state| {
        // A rotation that found the install current installed nothing, so there
        // is nothing to verify (0.3 skipped the verification there too). A plan
        // without a rotation installs through its own steps, so this still runs.
        if state.rotation() == Some(false) {
            return Ok(Outcome::skipped(
                "the rotation found the installed binary already current",
            ));
        }
        verify_installed_binary(context)?;
        Ok(Outcome::Done {
            detail: Some(format!("verified {}", context.install_path.display())),
            artifacts: Vec::new(),
            exit_code: None,
        })
    })
}

/// The `AfterRotate` entry that writes the deployment stamp.
///
/// A failure to write the stamp only warns: the deploy itself succeeded, and
/// the stamp keeps describing the last real install until the next one.
fn write_stamp_handler() -> Handler {
    Handler::with_state("write deploy stamp", |context, state| {
        if state.rotation() == Some(false) {
            return Ok(Outcome::skipped(
                "the rotation found the installed binary already current",
            ));
        }
        if let Err(error) = write_deploy_stamp(context) {
            state.warn(format!(
                "deployed, but the deployment stamp was not written: {error:#}"
            ));
        }
        Ok(Outcome::default())
    })
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

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        rc::Rc,
        time::{Duration, Instant},
    };

    use super::{
        CargoBuildStep, DeployBuilder, DeployEventReport, DeployStep, DeployStepReport, Handler,
        NullReporter, Outcome, Phase, Reporter, RotateStep, RunStep, StdoutReporter, StepStatus,
        default_reporter, deploy, install_is_current, skip_current_report, verify_installed_binary,
        write_deploy_stamp,
    };
    use crate::context::{DeployContext, build_inputs_fingerprint};
    use crate::rotate::RotateStrategy;

    /// Makes every [`test_context`] state directory unique even for two tests
    /// that ask for one within the same clock tick.
    static TEST_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn test_context() -> DeployContext {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        // Parallel tests can ask for a context in the same instant, so the
        // clock alone is not unique enough for the state directory: the lock
        // file of one test then blocks another (`deployment already running`).
        let sequence = TEST_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut context = DeployContext::new(std::env::temp_dir(), "tracel-deploy-test");
        context.quiet = true;
        context.state_dir = std::env::temp_dir().join(format!(
            "tracel-deploy-state-{}-{unique}-{sequence}",
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
        assert!(report.steps().is_empty());
        assert!(report.hooks().is_empty());
    }

    #[test]
    fn dry_run_runs_preflight_and_nothing_else() {
        let dir = temp_workspace("dry-run-preflight");
        let mut context = test_context();
        context.dry_run = true;
        context.workspace_root = dir.clone();
        context.state_dir = dir.join("state");
        context.install_path = dir.join("bin").join("installed-binary");
        let before = tree(&dir);
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let preflight_events = Rc::clone(&events);
        let pre_events = Rc::clone(&events);
        let step_events = Rc::clone(&events);
        let post_events = Rc::clone(&events);
        let on_error_events = Rc::clone(&events);
        let finally_events = Rc::clone(&events);

        let report = deploy(&context)
            .preflight_named("check signature", move |_| {
                preflight_events.borrow_mut().push("preflight");
                Ok(())
            })
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
            .on_error(move |_| {
                on_error_events.borrow_mut().push("on_error");
                Ok(())
            })
            .finally(move |_| {
                finally_events.borrow_mut().push("finally");
                Ok(())
            })
            .run()
            .expect("dry-run plan");

        assert_eq!(*events.borrow(), ["preflight"]);
        assert_eq!(report.hooks().len(), 1);
        assert_eq!(report.hooks()[0].name, "check signature");
        assert!(report.hooks()[0].error.is_none());
        assert!(report.steps().is_empty());
        assert!(report.dry_run);
        assert_eq!(
            tree(&dir),
            before,
            "a dry run must not create, move, or delete any file"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dry_run_plan_lists_hooks_in_execution_order() {
        let mut context = test_context();
        context.dry_run = true;

        let report = deploy(&context)
            .without_defaults()
            .preflight_named("check signature", |_| Ok(()))
            .preflight(|_| Ok(()))
            .pre_named("stop service", |_| Ok(()))
            .step(DeployStep::Custom(Box::new(|_| Ok(()))))
            .post_named("restart service", |_| Ok(()))
            .on_error_named("rollback", |_| Ok(()))
            .finally(|_| Ok(()))
            .run()
            .expect("dry-run plan");

        assert_eq!(
            report.plan,
            [
                "preflight: check signature",
                "preflight hook #2",
                "pre: stop service",
                "custom step (opaque)",
                "post: restart service",
                "on_error: rollback",
                "finally hook #1",
            ]
        );
    }

    #[test]
    fn preflight_does_not_run_without_dry_run() {
        let context = test_context();
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let preflight_events = Rc::clone(&events);
        let step_events = Rc::clone(&events);

        let report = deploy(&context)
            .without_defaults()
            .preflight_named("check signature", move |_| {
                preflight_events.borrow_mut().push("preflight");
                anyhow::bail!("preflight runs only during a dry run")
            })
            .step(DeployStep::Custom(Box::new(move |_| {
                step_events.borrow_mut().push("step");
                Ok(())
            })))
            .run()
            .expect("a real deploy must not run the preflight hooks");

        assert_eq!(*events.borrow(), ["step"]);
        assert!(report.plan.is_empty(), "plan: {:?}", report.plan);
        assert!(
            report.hooks().is_empty(),
            "hooks: {:?}",
            report
                .hooks()
                .iter()
                .map(|hook| &hook.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_failing_preflight_fails_the_dry_run_with_the_checked_items() {
        let mut context = test_context();
        context.dry_run = true;
        let events = Rc::new(RefCell::new(Vec::new()));

        let error = deploy(&context)
            .reporter(RecordingReporter::new(&events))
            .preflight_named("check signature", |_| Ok(()))
            .preflight_named("check install dir", |_| {
                anyhow::bail!("install dir is not writable")
            })
            .run()
            .expect_err("a failing preflight hook must fail the dry run");

        let text = format!("{error:#}");
        assert!(text.contains("check install dir"), "text: {text}");
        assert!(text.contains("install dir is not writable"), "text: {text}");
        assert!(
            text.contains("check signature (ok)"),
            "the checked items are listed: {text}"
        );
        assert!(
            text.contains("check install dir (failed)"),
            "the failed check is listed: {text}"
        );
        assert!(
            text.contains("-p tracel-deploy-test"),
            "the rendered plan is part of the error: {text}"
        );
        assert!(text.contains("no deploy step was executed"), "text: {text}");

        let recorded: Vec<String> = events.borrow().clone();
        assert!(
            recorded
                .iter()
                .any(|event| event.starts_with("plan: ") && event.contains("-p tracel-deploy-test")),
            "the rendered plan is reported before the failure: {recorded:?}"
        );
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

        assert_eq!(report.hooks().len(), 1);
        assert_eq!(report.hooks()[0].name, "restart service");
        assert!(report.hooks()[0].error.is_none());
        assert!(report.hooks()[0].error.is_none());
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

        let detail = report.steps()[0].detail.clone().unwrap_or_default();
        assert!(detail.contains("exit"), "detail: {detail}");
        assert!(detail.contains("boom"), "detail: {detail}");
        assert_eq!(report.steps()[0].status, StepStatus::Success);
    }

    #[test]
    fn the_pre_build_skip_is_opt_in() {
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

        let report = deploy(&context)
            .idempotent_skip(true)
            .run()
            .expect("skipped deploy");

        assert_eq!(report.steps().len(), 2);
        assert_eq!(report.steps()[0].status, StepStatus::Skipped);
        assert_eq!(report.steps()[1].status, StepStatus::Skipped);
        assert_eq!(
            std::fs::read_to_string(&destination).expect("destination"),
            "same bytes"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_default_plan_lets_cargo_decide() {
        let (dir, context) = stamped_workspace("cargo-decides");

        // The stamp is current, but the default plan does not trust it any
        // more: the build step runs and cargo's own fingerprints decide. This
        // fake workspace has no such package, so that decision surfaces as the
        // build error instead of a silent skip.
        let error = deploy(&context)
            .run()
            .expect_err("the default plan must run the build step");
        let text = format!("{error:#}");
        assert!(text.contains("cargo build"), "text: {text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_install_that_is_already_the_built_binary_is_not_rotated() {
        let dir = temp_workspace("rotate-noop");
        let source = dir.join("built");
        let destination = dir.join("installed");
        std::fs::write(&source, "same bytes").expect("source");
        std::fs::write(&destination, "same bytes").expect("destination");
        let context = DeployContext::builder(&dir, "tracel-deploy-test")
            .quiet(true)
            .source(&source)
            .install_to(&destination)
            .build();

        let report = rotate_step(
            source.clone(),
            destination.clone(),
            None,
            RotateStrategy::Auto,
        )
        .run(&context, &mut NullReporter)
        .expect("rotation");
        assert_eq!(report.status, StepStatus::Skipped);
        assert_eq!(
            std::fs::read_to_string(&destination).expect("installed"),
            "same bytes"
        );
        assert_eq!(
            backup_count(&dir, "installed"),
            0,
            "a no-op rotation must not leave a backup behind"
        );

        // `--force` deploys anyway, so the rotation runs for real.
        let mut forced = context;
        forced.force = true;
        let report = rotate_step(
            source.clone(),
            destination.clone(),
            None,
            RotateStrategy::Auto,
        )
        .run(&forced, &mut NullReporter)
        .expect("forced rotation");
        assert_eq!(report.status, StepStatus::Success);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_default_plan_builds_with_cargo_and_rotates_only_a_real_change() {
        let dir = temp_workspace("cargo-mode");
        std::fs::create_dir_all(dir.join("src")).expect("src dir");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"tracel-deploy-cargo-mode\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .expect("manifest");
        std::fs::write(
            dir.join("src/main.rs"),
            "fn main() { println!(\"one\"); }\n",
        )
        .expect("main");
        let installed = dir.join("installed.exe");
        let context = DeployContext::builder(&dir, "tracel-deploy-cargo-mode")
            .quiet(true)
            .install_to(&installed)
            .state_dir(dir.join("state"))
            .build();
        let stamp = context
            .state_dir
            .join(format!("{}.stamp", context.binary_name));

        // 1. first run: cargo builds, the rotation installs, the stamp lands.
        let first = deploy(&context).run().expect("first deploy");
        assert_eq!(first.steps()[0].status, StepStatus::Success, "cargo builds");
        assert_eq!(first.steps()[1].status, StepStatus::Success, "rotated");
        assert!(installed.is_file(), "the binary is installed");
        let stamped = std::fs::read_to_string(&stamp).expect("stamp");

        // 2. nothing changed: cargo no-ops on its own fingerprints, and the
        //    rotation sees the installed binary is already the built one.
        let second = deploy(&context).run().expect("second deploy");
        assert_eq!(
            second.steps()[0].status,
            StepStatus::Success,
            "the build step always runs; cargo decides"
        );
        assert_eq!(
            second.steps()[1].status,
            StepStatus::Skipped,
            "there is nothing to install"
        );
        assert_eq!(
            std::fs::read_to_string(&stamp).expect("stamp"),
            stamped,
            "a skipped rotation writes no new stamp"
        );
        assert_eq!(backup_count(&dir, "installed"), 0);

        // 3. a real source change: cargo rebuilds and the rotation replaces.
        std::fs::write(
            dir.join("src/main.rs"),
            "fn main() { println!(\"two\"); }\n",
        )
        .expect("edit");
        let third = deploy(&context).run().expect("third deploy");
        assert_eq!(third.steps()[1].status, StepStatus::Success, "replaced");
        assert_ne!(
            std::fs::read_to_string(&stamp).expect("stamp"),
            stamped,
            "a real install refreshes the stamp"
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
        assert_eq!(
            lock.took_over(),
            None,
            "a fresh lock is ours, not taken over"
        );

        let error = crate::context::DeployLock::acquire(&state_dir, "test-binary")
            .expect_err("second lock must fail");
        let text = format!("{error:#}");
        assert!(text.contains("deployment already running"), "error: {text}");
        assert!(
            text.contains(&format!("pid {}", std::process::id())),
            "the running deployment's pid is named: {text}"
        );

        drop(lock);
        let lock = crate::context::DeployLock::acquire(&state_dir, "test-binary")
            .expect("lock after release");
        drop(lock);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn a_lock_left_behind_by_a_killed_deployment_is_taken_over() {
        let state_dir = temp_workspace("stale-lock");
        let path = state_dir.join("test-binary.deploy.lock");
        let dead_pid = exited_process_pid();
        std::fs::write(&path, format!("pid={dead_pid}\n")).expect("stale lock");

        let lock = crate::context::DeployLock::acquire(&state_dir, "test-binary")
            .expect("a lock whose owner is gone is taken over immediately");
        assert_eq!(lock.took_over(), Some(dead_pid));
        assert_eq!(
            std::fs::read_to_string(&path).expect("lock"),
            format!("pid={}\n", std::process::id()),
            "the takeover owns the lock from now on"
        );
        drop(lock);
        assert!(!path.exists(), "the takeover removes the lock it owns");
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn a_lock_owned_by_a_live_process_is_not_taken_over_by_age() {
        let state_dir = temp_workspace("live-lock");
        let path = state_dir.join("test-binary.deploy.lock");
        std::fs::write(&path, format!("pid={}\n", std::process::id())).expect("live lock");
        let old = std::time::SystemTime::now()
            - crate::context::STALE_LOCK_AFTER
            - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open lock")
            .set_modified(old)
            .expect("backdate the lock");

        let error = crate::context::DeployLock::acquire(&state_dir, "test-binary")
            .expect_err("a live owner keeps its lock regardless of age");
        let text = format!("{error:#}");
        assert!(
            text.contains("owner process is still alive"),
            "error: {text}"
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn an_uninspectable_lock_still_uses_the_age_threshold() {
        let state_dir = temp_workspace("unknown-lock");
        let path = state_dir.join("test-binary.deploy.lock");
        std::fs::write(&path, "owner unavailable\n").expect("lock without a pid");
        let old = std::time::SystemTime::now()
            - crate::context::STALE_LOCK_AFTER
            - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open lock")
            .set_modified(old)
            .expect("backdate the lock");

        let lock = crate::context::DeployLock::acquire(&state_dir, "test-binary")
            .expect("an old lock without an inspectable owner is taken over");
        assert_eq!(lock.took_over(), None);
        drop(lock);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn a_takeover_in_progress_blocks_a_second_reclaimer() {
        let state_dir = temp_workspace("takeover-marker");
        let path = state_dir.join("test-binary.deploy.lock");
        let dead_pid = exited_process_pid();
        std::fs::write(&path, format!("pid={dead_pid}\n")).expect("stale lock");
        let mut marker = path.as_os_str().to_os_string();
        marker.push(".takeover");
        std::fs::write(
            std::path::PathBuf::from(marker),
            format!("pid={}\n", std::process::id()),
        )
        .expect("live takeover marker");

        let error = crate::context::DeployLock::acquire(&state_dir, "test-binary")
            .expect_err("only one process may reclaim a stale lock");
        let text = format!("{error:#}");
        assert!(text.contains("being reclaimed"), "error: {text}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("original lock"),
            format!("pid={dead_pid}\n"),
            "a blocked contender must not modify the lock"
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn dropping_a_lock_taken_over_by_someone_else_leaves_it_alone() {
        let state_dir = temp_workspace("foreign-lock");
        let path = state_dir.join("test-binary.deploy.lock");
        let lock = crate::context::DeployLock::acquire(&state_dir, "test-binary").expect("lock");

        std::fs::write(&path, "pid=1\n").expect("another deployment took over");
        drop(lock);
        assert!(
            path.is_file(),
            "a lock that records someone else's pid is not ours to delete"
        );
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

    fn exited_process_pid() -> u32 {
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/D", "/C", "exit 0"])
            .spawn()
            .expect("start a short-lived process");
        #[cfg(unix)]
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("start a short-lived process");
        #[cfg(not(any(windows, unix)))]
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("start a short-lived process");

        let pid = child.id();
        child.wait().expect("wait for the short-lived process");
        pid
    }

    /// How many `<binary>.old-*` backups a rotation left in `dir`.
    fn backup_count(dir: &std::path::Path, binary: &str) -> usize {
        std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{binary}.old"))
            })
            .count()
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
                .steps()
                .iter()
                .all(|step| step.status == StepStatus::Skipped),
            "steps: {:?}",
            report.steps()
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
            backup_cleanup_retry: None,
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
        assert_eq!(report.steps()[0].exit_code, Some(0));
        assert!(report.steps()[0].duration.is_some());
        let detail = report.steps()[0].detail.clone().unwrap_or_default();
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

        assert_eq!(report.steps()[0].status, StepStatus::Success);
        assert_eq!(report.steps()[0].exit_code, Some(3));
        assert!(report.steps()[0].duration.is_some());
        let detail = report.steps()[0].detail.clone().unwrap_or_default();
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

        assert_eq!(report.steps()[0].status, StepStatus::Success);
        assert!(
            report.steps()[0].duration.is_some(),
            "a step that ran reports how long it took"
        );
        assert!(report.steps()[0].exit_code.is_none());
        assert!(
            report.steps()[0].artifacts.contains(&destination),
            "artifacts: {:?}",
            report.steps()[0].artifacts
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

        let artifacts = &report.steps()[0].artifacts;
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

        assert_eq!(report.steps().len(), 1);
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

    // --- Acceptance gaps: insertion points, build command lines, lock cleanup ---

    /// A fake `cargo` that logs the arguments it was called with and exits
    /// successfully, so the default plan can run without a real build (and
    /// without the sccache and target-dir traffic a nested cargo would cause).
    fn fake_cargo(dir: &std::path::Path) -> std::path::PathBuf {
        let log = dir.join("cargo-args.txt");
        #[cfg(windows)]
        {
            let program = dir.join("fake-cargo.cmd");
            std::fs::write(
                &program,
                format!(
                    "@echo off\r\necho %* >> \"{}\"\r\nexit /b 0\r\n",
                    log.display()
                ),
            )
            .expect("fake cargo");
            program
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let program = dir.join("fake-cargo.sh");
            std::fs::write(
                &program,
                format!("#!/bin/sh\necho \"$@\" >> \"{}\"\nexit 0\n", log.display()),
            )
            .expect("fake cargo");
            let mut permissions = std::fs::metadata(&program)
                .expect("fake cargo")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&program, permissions).expect("fake cargo permissions");
            program
        }
    }

    /// A context whose default plan runs to completion without cargo: a fake
    /// cargo that succeeds, an existing "built" binary, and an install
    /// directory.
    fn default_plan_context(dir: &std::path::Path) -> DeployContext {
        let source = dir.join("built-binary");
        std::fs::write(&source, "built bytes").expect("built binary");
        let install = dir.join("bin").join("installed-binary");
        std::fs::create_dir_all(install.parent().expect("install parent")).expect("install dir");

        let mut context = DeployContext::builder(dir, "tracel-deploy-test")
            .quiet(true)
            .source(&source)
            .install_to(&install)
            .state_dir(dir.join("state"))
            .build();
        context.cargo = Some(fake_cargo(dir));
        context
    }

    /// The default plan with the idempotent skip, the install verification, and
    /// the stamp disabled: what remains is the plan itself.
    fn plain_default_builder(context: &DeployContext) -> DeployBuilder<'_> {
        deploy(context)
            .idempotent_skip(false)
            .verify_install(false)
            .write_deploy_stamp(false)
    }

    /// A custom step that records `marker` when it runs.
    fn marker_step(events: &Rc<RefCell<Vec<&'static str>>>, marker: &'static str) -> DeployStep {
        let events = Rc::clone(events);
        DeployStep::Custom(Box::new(move |_| {
            events.borrow_mut().push(marker);
            Ok(())
        }))
    }

    /// Records the default steps by name, so one event list can hold both the
    /// steps and the hook side effects around them.
    #[derive(Default)]
    struct StepOrderReporter {
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    impl StepOrderReporter {
        fn new(events: &Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                events: Rc::clone(events),
            }
        }
    }

    impl Reporter for StepOrderReporter {
        fn step_started(&mut self, name: &str, _description: &str) {
            let marker = match name {
                "cargo build" => "build",
                "rotate" => "rotate",
                _ => return,
            };
            self.events.borrow_mut().push(marker);
        }
    }

    /// Every entry under `dir` with its size and modification time, so a test
    /// can prove a run neither created nor changed anything.
    fn tree(
        dir: &std::path::Path,
    ) -> Vec<(std::path::PathBuf, u64, Option<std::time::SystemTime>)> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&next) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path.clone());
                }
                let metadata = entry.metadata().ok();
                found.push((
                    path,
                    metadata.as_ref().map_or(0, |meta| meta.len()),
                    metadata.and_then(|meta| meta.modified().ok()),
                ));
            }
        }
        found.sort();
        found
    }

    #[test]
    fn inserted_steps_run_between_the_default_build_and_rotate() {
        let dir = temp_workspace("insertion-order");
        let context = default_plan_context(&dir);
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let reporter_events = Rc::clone(&events);
        let start_events = Rc::clone(&events);

        let report = plain_default_builder(&context)
            .reporter(StepOrderReporter::new(&reporter_events))
            .insert_after_default_build(marker_step(&events, "stop"))
            .post(move |_| {
                start_events.borrow_mut().push("start");
                Ok(())
            })
            .run()
            .expect("the default plan with one inserted step");

        assert_eq!(
            *events.borrow(),
            ["build", "stop", "rotate", "start"],
            "steps: {:?}",
            report
                .steps()
                .iter()
                .map(|step| &step.name)
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn inserted_steps_keep_their_call_order() {
        let dir = temp_workspace("insertion-call-order");
        let context = default_plan_context(&dir);
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let reporter_events = Rc::clone(&events);

        plain_default_builder(&context)
            .reporter(StepOrderReporter::new(&reporter_events))
            .insert_after_default_build(marker_step(&events, "stop-1"))
            .insert_before_default_rotate(marker_step(&events, "pre-rotate"))
            .insert_after_default_build(marker_step(&events, "stop-2"))
            .run()
            .expect("the default plan with three inserted steps");

        assert_eq!(
            *events.borrow(),
            ["build", "stop-1", "pre-rotate", "stop-2", "rotate"],
            "0.4: the AfterBuild slot runs its steps in call order"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_default_plan_is_unchanged_without_insertions() {
        let dir = temp_workspace("insertion-default");
        let context = default_plan_context(&dir);
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let reporter_events = Rc::clone(&events);

        let report = plain_default_builder(&context)
            .reporter(StepOrderReporter::new(&reporter_events))
            .run()
            .expect("the default plan");

        assert_eq!(report.steps().len(), 2, "steps: {:?}", report.steps());
        assert_eq!(*events.borrow(), ["build", "rotate"]);

        // The dry-run plan lists the two default entries plus the two entries
        // the default plan adds after the rotation.
        let mut dry_run = test_context();
        dry_run.dry_run = true;
        let plan = deploy(&dry_run).run().expect("dry-run plan").plan;
        assert_eq!(plan.len(), 4, "plan: {plan:?}");
        assert!(plan[0].contains("-p tracel-deploy-test"), "plan: {plan:?}");
        assert!(plan[1].starts_with("rotate "), "plan: {plan:?}");
        assert!(
            plan[2].starts_with("after-rotate: verify install"),
            "plan: {plan:?}"
        );
        assert!(
            plan[3].starts_with("after-rotate: write deploy stamp"),
            "plan: {plan:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn insertion_points_apply_to_a_caller_owned_plan_too() {
        let dir = temp_workspace("insertion-without-defaults");
        let context = default_plan_context(&dir);
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));

        let report = deploy(&context)
            .without_defaults()
            .insert_after_default_build(marker_step(&events, "inserted"))
            .insert_before_default_rotate(marker_step(&events, "also-inserted"))
            .step(marker_step(&events, "own"))
            .run()
            .expect("a caller-owned plan");

        assert_eq!(report.steps().len(), 3, "steps: {:?}", report.steps());
        assert_eq!(
            *events.borrow(),
            ["inserted", "also-inserted", "own"],
            "0.4: the AfterBuild slot is part of the timeline, not of the default plan"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The public "would the rotation install anything?" predicate - the one
    /// the service entries decide their skip on.
    #[test]
    fn install_is_current_answers_whether_the_rotation_would_install() {
        let dir = temp_workspace("install-is-current");
        let context = default_plan_context(&dir);
        assert!(
            !install_is_current(&context),
            "a missing install is not current"
        );

        std::fs::write(context.built_binary_path(), "built bytes").expect("built");
        std::fs::write(&context.install_path, "built bytes").expect("install");
        assert!(install_is_current(&context), "identical bytes are current");

        std::fs::write(&context.install_path, "stale bytes").expect("install");
        assert!(!install_is_current(&context), "different bytes are stale");

        let mut forced = default_plan_context(&dir);
        std::fs::write(forced.built_binary_path(), "built bytes").expect("built");
        std::fs::write(&forced.install_path, "built bytes").expect("install");
        forced.force = true;
        assert!(!install_is_current(&forced), "--force always installs");

        let mut dry_run = default_plan_context(&dir);
        std::fs::write(dry_run.built_binary_path(), "built bytes").expect("built");
        std::fs::write(&dry_run.install_path, "built bytes").expect("install");
        dry_run.dry_run = true;
        assert!(!install_is_current(&dry_run), "a dry run installs nothing");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 0.4: every entry lands in `report.events` with the phase it ran at, and a
    /// handler that reports `Outcome::Skipped` does not fail the plan.
    #[test]
    fn handlers_report_their_phase_and_outcome() {
        let context = test_context();
        let seen = Rc::new(RefCell::new(Vec::<String>::new()));
        let reported = Rc::clone(&seen);

        struct PhaseReporter {
            seen: Rc<RefCell<Vec<String>>>,
        }
        impl Reporter for PhaseReporter {
            fn handler_started(&mut self, phase: Phase, name: &str, _description: &str) {
                self.seen
                    .borrow_mut()
                    .push(format!("start {} {name}", phase.label()));
            }
            fn handler_finished(&mut self, report: &DeployEventReport) {
                self.seen.borrow_mut().push(format!(
                    "finish {} {} {:?}",
                    report.phase.label(),
                    report.name,
                    report.status
                ));
            }
        }

        let report = deploy(&context)
            .defaults(false)
            .reporter(PhaseReporter { seen: reported })
            .on(
                Phase::AfterBuild,
                Handler::with_state("stop service", |_context, _state| {
                    Ok(Outcome::skipped("nothing to install"))
                }),
            )
            .on(
                Phase::Finally,
                Handler::new("start service", |_context| Ok(())),
            )
            .run()
            .expect("a two-entry, caller-owned plan");

        assert_eq!(
            seen.borrow().as_slice(),
            [
                "start after-build stop service",
                "finish after-build stop service Skipped",
                "start finally start service",
                "finish finally start service Success",
            ]
        );
        assert_eq!(report.events.len(), 2, "events: {:?}", report.events);
        assert_eq!(report.events[0].phase, Phase::AfterBuild);
        assert!(report.events[0].is_skipped());
        assert_eq!(
            report.events[0].detail.as_deref(),
            Some("nothing to install")
        );
        assert_eq!(report.events[1].phase, Phase::Finally);
        assert!(!report.events[1].is_failed());
        // The 0.3 views of the same run: a slot entry is a step, a lifecycle
        // entry is a hook.
        assert_eq!(report.steps().len(), 1, "steps: {:?}", report.steps());
        assert_eq!(report.steps()[0].status, StepStatus::Skipped);
        assert_eq!(report.hooks().len(), 1, "hooks: {:?}", report.hooks());
        assert!(report.hooks()[0].error.is_none());
    }

    /// A failing handler reaches the reporter as a `Failed` event, fails the
    /// deploy, and still runs `on_error` and `finally`.
    #[test]
    fn a_failed_handler_is_reported_and_still_runs_on_error_and_finally() {
        let context = test_context();
        let order = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let failure = Rc::new(RefCell::new(None::<StepStatus>));
        let reported_failure = Rc::clone(&failure);

        struct FailureReporter {
            failure: Rc<RefCell<Option<StepStatus>>>,
        }
        impl Reporter for FailureReporter {
            fn handler_finished(&mut self, report: &DeployEventReport) {
                if report.is_failed() {
                    *self.failure.borrow_mut() = Some(report.status);
                }
            }
        }

        let on_error_order = Rc::clone(&order);
        let finally_order = Rc::clone(&order);
        let error = deploy(&context)
            .defaults(false)
            .reporter(FailureReporter {
                failure: reported_failure,
            })
            .on(
                Phase::AfterBuild,
                Handler::new("stop service", |_context| {
                    anyhow::bail!("no service to stop")
                }),
            )
            .on(
                Phase::OnError,
                Handler::new("rollback", move |_context| {
                    on_error_order.borrow_mut().push("on-error");
                    Ok(())
                }),
            )
            .on(
                Phase::Finally,
                Handler::new("start service", move |_context| {
                    finally_order.borrow_mut().push("finally");
                    Ok(())
                }),
            )
            .run()
            .expect_err("the failing entry fails the deploy");

        let text = format!("{error:#}");
        assert!(text.contains("stop service failed"), "text: {text}");
        assert!(text.contains("no service to stop"), "text: {text}");
        assert_eq!(order.borrow().as_slice(), ["on-error", "finally"]);
        assert_eq!(*failure.borrow(), Some(StepStatus::Failed));
    }

    #[test]
    fn cargo_build_command_line_carries_every_option() {
        let context = test_context();
        let step = CargoBuildStep {
            package: "my-service".to_owned(),
            release: true,
            cargo: Some(std::path::PathBuf::from("C:/tools/cargo-ds.exe")),
            profile: Some("dist".to_owned()),
            locked: true,
            target: Some("x86_64-pc-windows-msvc".to_owned()),
            target_dir: Some(std::path::PathBuf::from("D:/cache/target")),
            features: vec!["a".to_owned(), "b".to_owned()],
            no_default_features: true,
            extra_args: vec!["--verbose".to_owned()],
        };

        let line = step.command_line(&context);

        assert_eq!(
            line,
            [
                "C:/tools/cargo-ds.exe",
                "build",
                "-p",
                "my-service",
                "--color",
                "always",
                "--profile",
                "dist",
                "--locked",
                "--target",
                "x86_64-pc-windows-msvc",
                "--target-dir",
                "D:/cache/target",
                "--features",
                "a,b",
                "--no-default-features",
                "--verbose",
            ]
        );
        assert!(
            !line.iter().any(|arg| arg == "--release"),
            "a custom profile replaces --release: {line:?}"
        );
        assert_eq!(
            DeployStep::CargoBuild(step).describe(&context),
            line.join(" "),
            "describe() and the executed command share one source"
        );
    }

    #[test]
    fn cargo_build_command_line_uses_release_without_a_profile() {
        let context = test_context();
        let mut step = CargoBuildStep::from_context(&context);
        step.cargo = Some(std::path::PathBuf::from("cargo"));

        step.release = true;
        assert!(
            step.command_line(&context)
                .contains(&"--release".to_owned()),
            "line: {:?}",
            step.command_line(&context)
        );

        step.release = false;
        assert!(
            !step
                .command_line(&context)
                .contains(&"--release".to_owned()),
            "line: {:?}",
            step.command_line(&context)
        );

        step.profile = Some("dist".to_owned());
        let line = step.command_line(&context);
        assert!(
            line.windows(2).any(|pair| pair == ["--profile", "dist"]),
            "line: {line:?}"
        );
        assert!(!line.iter().any(|arg| arg == "--release"), "line: {line:?}");
    }

    #[test]
    fn cargo_build_executes_the_command_line_it_describes() {
        let dir = temp_workspace("cargo-command-line");
        let cargo = fake_cargo(&dir);
        let context = DeployContext::builder(&dir, "my-service")
            .quiet(true)
            .cargo(&cargo)
            .profile("dist")
            .locked(true)
            .target("x86_64-pc-windows-msvc")
            .target_dir(dir.join("target"))
            .features(["a", "b"])
            .no_default_features(true)
            .build_args(["--verbose"])
            .build();

        let step = CargoBuildStep::from_context(&context);
        let described = DeployStep::CargoBuild(step.clone()).describe(&context);
        let report = step
            .clone()
            .run(&context, &mut NullReporter)
            .expect("the fake cargo succeeds");

        assert_eq!(report.status, StepStatus::Success);
        let executed = std::fs::read_to_string(dir.join("cargo-args.txt"))
            .expect("the fake cargo logged its argv");
        assert_eq!(
            // `cmd.exe` re-quotes an argument like `a,b` when `%*` is echoed.
            executed.trim().replace('"', ""),
            step.command_line(&context)[1..].join(" "),
            "the executed argv is the described command line"
        );
        assert_eq!(described, step.command_line(&context).join(" "));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_deploy_releases_the_lock() {
        let dir = temp_workspace("failed-deploy-lock");
        let mut context = test_context();
        context.state_dir = dir.join("state");
        let lock_path = context
            .state_dir
            .join(format!("{}.deploy.lock", context.binary_name));
        let held = lock_path.clone();

        let error = deploy(&context)
            .without_defaults()
            .step(DeployStep::Custom(Box::new(move |_| {
                assert!(
                    held.is_file(),
                    "the deploy holds the lock while the plan runs: {}",
                    held.display()
                );
                anyhow::bail!("injected step failure")
            })))
            .run()
            .expect_err("the injected failure must surface");

        assert!(format!("{error:#}").contains("injected step failure"));
        assert!(
            !lock_path.is_file(),
            "a failed deploy must not leave {} behind",
            lock_path.display()
        );
        let reacquired =
            crate::context::DeployLock::acquire(&context.state_dir, &context.binary_name)
                .expect("the lock is free again after a failure");
        drop(reacquired);
        let _ = std::fs::remove_dir_all(dir);
    }
}
