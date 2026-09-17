//! The 0.4 deploy timeline: [`Phase`], [`Handler`], [`Outcome`], [`RunState`].
//!
//! 0.3 had two shapes for the same thing - `DeployStep` for the plan's steps and
//! `DeployHookEntry` for the lifecycle hooks. 0.4 keeps one: a [`Handler`] is
//! registered at a [`Phase`], and the phase decides when it runs and what a
//! failure does to the rest of the run.
//!
//! Where data lives:
//!
//! - the phase enum carries none (it is a registration key and a report tag),
//! - registration-time configuration lives inside the handler,
//! - per-run facts (what was installed, which phases ran, the service instance)
//!   live in [`RunState`],
//! - per-instance data (pid, args, listen, log) lives in `ServiceState`,
//! - what one execution produced lives in [`Outcome`].

use std::{
    any::{Any, TypeId},
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
};

use anyhow::Result;

use crate::context::DeployContext;
#[cfg(feature = "service")]
use crate::service::ServiceState;

use super::{CargoBuildStep, DeployStep, Reporter, RotateStep, RunStep, StepStatus};

/// One point on the deploy timeline.
///
/// The lifecycle events keep their 0.3 meaning:
///
/// - `Preflight` runs only on a dry run, after the plan was rendered,
/// - `Pre` runs before the first slot,
/// - `Post` runs once every slot succeeded,
/// - `OnError` runs when `Pre`, a slot, or `Post` failed,
/// - `Finally` always runs (after `Post`, or after `OnError`).
///
/// The slots are the timeline itself; a default plan registers its build step
/// at [`Phase::Build`] and its rotation at [`Phase::Rotate`], so
/// [`Phase::AfterBuild`] is the "build has finished, nothing was replaced yet"
/// point - where a service is stopped - and [`Phase::AfterRotate`] is the
/// "installed" point.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Phase {
    /// Dry run only: read-only checks, after the plan was rendered.
    Preflight,
    /// Before the first slot.
    Pre,
    /// Every slot succeeded.
    Post,
    /// `Pre`, a slot, or `Post` failed.
    OnError,
    /// Always runs.
    Finally,
    /// The build slot.
    Build,
    /// Build finished, nothing was replaced yet.
    AfterBuild,
    /// The rotation slot.
    Rotate,
    /// The rotation slot finished.
    AfterRotate,
}

impl Phase {
    /// The order the phases run in on a real deploy. [`Phase::Preflight`] is
    /// not part of it: a real deploy never runs it.
    pub const ORDER: [Self; 8] = [
        Self::Pre,
        Self::Build,
        Self::AfterBuild,
        Self::Rotate,
        Self::AfterRotate,
        Self::Post,
        Self::OnError,
        Self::Finally,
    ];

    /// The name used in plans, reports, and errors.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Pre => "pre",
            Self::Post => "post",
            Self::OnError => "on-error",
            Self::Finally => "finally",
            Self::Build => "build",
            Self::AfterBuild => "after-build",
            Self::Rotate => "rotate",
            Self::AfterRotate => "after-rotate",
        }
    }

    /// Whether this phase is a timeline slot (and therefore reported as a
    /// step rather than as a hook).
    pub const fn is_slot(self) -> bool {
        matches!(
            self,
            Self::Build | Self::AfterBuild | Self::Rotate | Self::AfterRotate
        )
    }

    /// Whether the phase runs only after a failure.
    pub const fn is_failure_only(self) -> bool {
        matches!(self, Self::OnError)
    }

    /// Whether the phase runs after success and after failure alike.
    pub const fn is_always(self) -> bool {
        matches!(self, Self::Finally)
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What one execution of a handler produced.
#[derive(Debug)]
pub enum Outcome {
    /// The handler ran to completion. `detail` is what the report shows,
    /// `artifacts` are the files it created or replaced, and `exit_code` is the
    /// status of the process it ran, when it ran one.
    Done {
        detail: Option<String>,
        artifacts: Vec<PathBuf>,
        exit_code: Option<i32>,
    },
    /// The handler decided there was nothing to do; `reason` is reported.
    Skipped { reason: String },
}

impl Outcome {
    /// A successful run with no detail to report.
    pub fn done() -> Self {
        Self::default()
    }

    /// Nothing to do; the reason is what the report shows.
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self::Skipped {
            reason: reason.into(),
        }
    }

    /// Whether the handler reported nothing to do.
    pub fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }

    /// The reported detail, or the skip reason.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Done { detail, .. } => detail.as_deref(),
            Self::Skipped { reason } => Some(reason),
        }
    }

    /// The files the handler created or replaced.
    pub fn artifacts(&self) -> &[PathBuf] {
        match self {
            Self::Done { artifacts, .. } => artifacts,
            Self::Skipped { .. } => &[],
        }
    }

    /// The exit code of the process the handler ran, when it ran one.
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Done { exit_code, .. } => *exit_code,
            Self::Skipped { .. } => None,
        }
    }
}

impl Default for Outcome {
    fn default() -> Self {
        Self::Done {
            detail: None,
            artifacts: Vec::new(),
            exit_code: None,
        }
    }
}

impl From<()> for Outcome {
    fn from((): ()) -> Self {
        Self::default()
    }
}

/// What the run knows so far.
///
/// The library fills `installed` and `phases_done`; a handler fills the rest.
/// `service` and `stopped_service` carry the service instance across phases
/// (the pid lives on `ServiceState`, not on the phase enum), and `set`/`get`
/// is a typed slot for a consumer's own payload.
#[derive(Default)]
pub struct RunState {
    /// `None` while no rotation slot ran; `Some(false)` for a rotation that
    /// found nothing to install; `Some(true)` for an install.
    rotation: Option<bool>,
    phases_done: BTreeSet<Phase>,
    notes: Vec<String>,
    warnings: Vec<String>,
    extras: BTreeMap<TypeId, Box<dyn Any>>,
    #[cfg(feature = "service")]
    service: Option<ServiceState>,
    #[cfg(feature = "service")]
    stopped_service: Option<ServiceState>,
}

impl RunState {
    /// Whether the rotation slot replaced the installed binary in this run.
    /// The library sets this from the `Rotate` slot's outcome; a plan without a
    /// rotation leaves it `false`.
    pub fn installed(&self) -> bool {
        self.rotation == Some(true)
    }

    /// The library's own bookkeeping: `true` for a rotation that installed
    /// something, `false` for one that found the install current.
    pub(crate) fn mark_rotation(&mut self, installed: bool) {
        self.rotation = Some(installed);
    }

    /// Whether a rotation slot ran at all, and whether it installed something:
    /// `None` for a plan without a rotation, `Some(false)` for one that found
    /// the install current, `Some(true)` for an install.
    pub fn rotation(&self) -> Option<bool> {
        self.rotation
    }

    /// Whether a phase finished without a failure.
    pub fn phase_completed(&self, phase: Phase) -> bool {
        self.phases_done.contains(&phase)
    }

    pub(crate) fn mark_phase_done(&mut self, phase: Phase) {
        self.phases_done.insert(phase);
    }

    /// The service instance this run started, if any.
    #[cfg(feature = "service")]
    pub fn service(&self) -> Option<&ServiceState> {
        self.service.as_ref()
    }

    /// Publish the service instance this run started.
    #[cfg(feature = "service")]
    pub fn set_service(&mut self, state: ServiceState) {
        self.service = Some(state);
    }

    /// The service instance this run stopped, if any.
    #[cfg(feature = "service")]
    pub fn stopped_service(&self) -> Option<&ServiceState> {
        self.stopped_service.as_ref()
    }

    /// Publish the service instance this run stopped.
    #[cfg(feature = "service")]
    pub fn set_stopped_service(&mut self, state: ServiceState) {
        self.stopped_service = Some(state);
    }

    /// A free-form line a handler wants the next handler (or the caller) to
    /// see.
    pub fn note(&mut self, message: impl Into<String>) {
        self.notes.push(message.into());
    }

    /// The notes handlers recorded, in order.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Record a non-fatal problem; the runner forwards it to
    /// `Reporter::warning`, which `quiet` does not suppress.
    pub fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    /// The warnings handlers recorded, in order.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Store a consumer payload for later phases of the same run.
    pub fn set<T: Any>(&mut self, value: T) {
        self.extras.insert(TypeId::of::<T>(), Box::new(value));
    }

    /// Borrow a payload stored with [`RunState::set`].
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.extras
            .get(&TypeId::of::<T>())
            .and_then(|value| value.downcast_ref::<T>())
    }

    /// Take a payload stored with [`RunState::set`].
    pub fn take<T: Any>(&mut self) -> Option<T> {
        self.extras
            .remove(&TypeId::of::<T>())
            .and_then(|value| value.downcast::<T>().ok())
            .map(|value| *value)
    }
}

impl fmt::Debug for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunState")
            .field("installed", &self.installed())
            .field("rotation", &self.rotation)
            .field("phases_done", &self.phases_done)
            .field("notes", &self.notes)
            .finish_non_exhaustive()
    }
}

type HandlerFn = Box<dyn FnMut(&DeployContext, &mut RunState) -> Result<Outcome>>;
type SkipFn = Box<dyn Fn(&DeployContext) -> Option<String>>;

/// What a handler does when it runs.
enum HandlerCore {
    /// A closure: what a consumer registers, and where the 0.3 lifecycle hooks
    /// land.
    Closure(HandlerFn),
    /// A built-in step: it prints through the reporter and reports artifacts
    /// and exit codes. `None` once it ran - a handler runs at most once per
    /// plan.
    Step(Option<DeployStep>),
}

/// One unit of work on the deploy timeline.
///
/// The simple constructor takes a closure that has nothing to report beyond
/// success; [`Handler::with_state`] exposes [`RunState`] and
/// [`Outcome::Skipped`], which is what "there is nothing to install, so do not
/// restart anything" needs. A built-in step converts into a handler, so a
/// `CargoBuildStep`, a `RotateStep`, and a consumer closure are all registered
/// the same way.
pub struct Handler {
    name: String,
    description: Option<String>,
    plan_line: Option<String>,
    skip: Option<SkipFn>,
    default_phase: Option<Phase>,
    core: HandlerCore,
}

impl Handler {
    /// A handler that has no outcome to report beyond success.
    pub fn new(
        name: impl Into<String>,
        mut run: impl FnMut(&DeployContext) -> Result<()> + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: None,
            plan_line: None,
            skip: None,
            default_phase: None,
            core: HandlerCore::Closure(Box::new(move |context, _state| {
                run(context)?;
                Ok(Outcome::default())
            })),
        }
    }

    /// A handler that reads and writes [`RunState`] and reports an [`Outcome`].
    pub fn with_state(
        name: impl Into<String>,
        run: impl FnMut(&DeployContext, &mut RunState) -> Result<Outcome> + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: None,
            plan_line: None,
            skip: None,
            default_phase: None,
            core: HandlerCore::Closure(Box::new(run)),
        }
    }

    /// The line a dry run renders for this handler (falls back to the name).
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Render this exact line in the dry-run plan, verbatim.
    ///
    /// The deprecated 0.3 builder methods use it to keep the 0.3 plan lines
    /// (`pre: stop service`, `pre hook #1`) byte-identical; new code does not
    /// need it.
    pub fn plan_line(mut self, line: impl Into<String>) -> Self {
        self.plan_line = Some(line.into());
        self
    }

    /// Skip the handler when `predicate` returns a reason.
    pub fn skip_when(
        mut self,
        predicate: impl Fn(&DeployContext) -> Option<String> + 'static,
    ) -> Self {
        self.skip = Some(Box::new(predicate));
        self
    }

    /// The phase [`DeployBuilder::insert`](crate::DeployBuilder::insert) uses.
    pub fn at(mut self, phase: Phase) -> Self {
        self.default_phase = Some(phase);
        self
    }

    /// The phase this handler asks to run at, when it has one.
    pub fn default_phase(&self) -> Option<Phase> {
        self.default_phase
    }

    /// Whether this handler is a built-in step (which prints through the
    /// reporter and reports artifacts).
    pub(crate) fn is_step(&self) -> bool {
        matches!(self.core, HandlerCore::Step(_))
    }

    /// The name the handler reports itself under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The dry-run description, when the handler has one.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The line a dry run renders for this handler at `phase`.
    pub fn render(&self, phase: Phase, context: &DeployContext) -> String {
        if let Some(line) = &self.plan_line {
            return line.clone();
        }
        match &self.core {
            HandlerCore::Step(Some(step)) => step.describe(context),
            _ => format!(
                "{}: {}",
                phase.label(),
                self.description.as_deref().unwrap_or(&self.name)
            ),
        }
    }

    /// Run the handler.
    ///
    /// `reporter` is what a built-in step prints through; a closure handler
    /// ignores it and records lines with [`RunState::note`] instead.
    pub(crate) fn call(
        &mut self,
        context: &DeployContext,
        state: &mut RunState,
        reporter: &mut dyn Reporter,
    ) -> Result<Outcome> {
        if let Some(skip) = &self.skip
            && let Some(reason) = skip(context)
        {
            return Ok(Outcome::skipped(reason));
        }
        match &mut self.core {
            HandlerCore::Closure(run) => run(context, state),
            HandlerCore::Step(step) => {
                let Some(step) = step.take() else {
                    return Ok(Outcome::default());
                };
                let report = step.run(context, reporter)?;
                Ok(match report.status {
                    StepStatus::Skipped => Outcome::skipped(
                        report
                            .detail
                            .unwrap_or_else(|| format!("{} skipped", self.name)),
                    ),
                    StepStatus::Success => Outcome::Done {
                        detail: report.detail,
                        artifacts: report.artifacts,
                        exit_code: report.exit_code,
                    },
                    // A step reports a failure by returning an error; a report
                    // that says `Failed` is not something a step produces.
                    StepStatus::Failed => anyhow::bail!("{} failed", self.name),
                })
            }
        }
    }
}

impl From<DeployStep> for Handler {
    fn from(step: DeployStep) -> Self {
        Self {
            name: step.name(),
            description: None,
            plan_line: None,
            skip: None,
            default_phase: None,
            core: HandlerCore::Step(Some(step)),
        }
    }
}

impl From<CargoBuildStep> for Handler {
    fn from(step: CargoBuildStep) -> Self {
        DeployStep::CargoBuild(step).into()
    }
}

impl From<RotateStep> for Handler {
    fn from(step: RotateStep) -> Self {
        DeployStep::Rotate(step).into()
    }
}

impl From<RunStep> for Handler {
    fn from(step: RunStep) -> Self {
        DeployStep::Run(step).into()
    }
}
impl fmt::Debug for Handler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handler")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::NullReporter;

    fn context() -> DeployContext {
        let mut context = DeployContext::new(std::env::temp_dir(), "tracel-deploy-phase");
        context.state_dir = std::env::temp_dir().join("tracel-deploy-phase-state");
        context
    }

    #[test]
    fn the_execution_order_is_the_timeline() {
        assert_eq!(
            Phase::ORDER.map(Phase::label),
            [
                "pre",
                "build",
                "after-build",
                "rotate",
                "after-rotate",
                "post",
                "on-error",
                "finally"
            ]
        );
        assert!(!Phase::ORDER.contains(&Phase::Preflight), "dry run only");
        assert!(Phase::Build.is_slot() && Phase::AfterBuild.is_slot());
        assert!(Phase::Rotate.is_slot() && Phase::AfterRotate.is_slot());
        assert!(!Phase::Post.is_slot());
        assert!(Phase::OnError.is_failure_only());
        assert!(Phase::Finally.is_always());
    }

    #[test]
    fn a_simple_handler_reports_done() {
        let mut handler = Handler::new("check", |_context| Ok(())).describe("检查运行实例");
        assert_eq!(handler.name(), "check");
        assert_eq!(
            handler.render(Phase::AfterBuild, &context()),
            "after-build: 检查运行实例"
        );

        let mut state = RunState::default();
        let outcome = handler
            .call(&context(), &mut state, &mut NullReporter)
            .expect("the handler runs");
        assert!(!outcome.is_skipped());
        assert!(outcome.detail().is_none());
    }

    #[test]
    fn a_stateful_handler_can_skip_and_see_the_run() {
        let mut handler = Handler::with_state("install gate", |_context, state| {
            if state.installed() {
                Ok(Outcome::done())
            } else {
                Ok(Outcome::skipped("nothing was installed"))
            }
        });

        let mut state = RunState::default();
        let outcome = handler
            .call(&context(), &mut state, &mut NullReporter)
            .expect("the handler runs");
        assert!(outcome.is_skipped());
        assert_eq!(outcome.detail(), Some("nothing was installed"));

        state.mark_rotation(true);
        let outcome = handler
            .call(&context(), &mut state, &mut NullReporter)
            .expect("the handler runs");
        assert!(!outcome.is_skipped());
    }

    #[test]
    fn skip_when_short_circuits_the_handler() {
        let mut handler = Handler::new("stop service", |_context| {
            panic!("must not run when the predicate skips it")
        })
        .skip_when(|_context| Some("already up to date".to_string()));

        let outcome = handler
            .call(&context(), &mut RunState::default(), &mut NullReporter)
            .expect("the skip counts as success");
        assert_eq!(outcome.detail(), Some("already up to date"));
    }

    #[test]
    fn the_run_state_carries_facts_notes_and_payloads() {
        struct Payload(u32);

        let mut state = RunState::default();
        assert!(!state.installed());
        assert!(!state.phase_completed(Phase::Build));

        state.mark_rotation(true);
        state.mark_phase_done(Phase::Build);
        state.note("built");
        state.set(Payload(7));

        assert!(state.installed());
        assert!(state.phase_completed(Phase::Build));
        assert_eq!(state.notes(), ["built"]);
        assert_eq!(state.get::<Payload>().map(|payload| payload.0), Some(7));

        let taken = state.take::<Payload>().expect("the payload is there");
        assert_eq!(taken.0, 7);
        assert!(state.get::<Payload>().is_none());
    }

    #[cfg(feature = "service")]
    #[test]
    fn the_run_state_carries_the_service_instance() {
        use std::path::PathBuf;

        let state = ServiceState {
            name: "app".to_string(),
            pid: 4242,
            binary: PathBuf::from("/tmp/app"),
            args: vec!["--mode".into(), "sse".into()],
            log_file: None,
            listen: Some("127.0.0.1:18003".into()),
            started_at: 1,
        };

        let mut run = RunState::default();
        run.set_stopped_service(state.clone());
        run.set_service(state);
        assert_eq!(run.service().map(|service| service.pid), Some(4242));
        assert_eq!(run.stopped_service().map(|service| service.pid), Some(4242));
    }
}
