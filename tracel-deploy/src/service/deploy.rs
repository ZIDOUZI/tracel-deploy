//! Deploy entries for the service lifecycle (`feature = "service"` helpers).
//!
//! [`ServiceStop`] and [`ServiceStart`] are the two entries the
//! `build -> stop service -> rotate -> start service` plan needs: they plug
//! into the phase timeline, they agree with the rotation about what "there is
//! something to install" means ([`crate::install_is_current`]), and they carry
//! the instance across phases in [`RunState`] - the pid lives on
//! [`ServiceState`], not on the phase.

use std::time::Duration;

/// Rewrites the arguments of the instance right before it is started.
pub type ArgRewrite = Box<dyn Fn(Vec<String>) -> Vec<String>>;

use crate::{
    install_is_current,
    plan::{Handler, Outcome, Phase},
    service::{
        ProbeSpec, ServiceSpec, ServiceState, StopOutcome, find_running_instance,
        start_and_wait_ready, start_detached, stop_service,
    },
};

/// The default wait for a stopping instance.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// The default retries/interval/timeout of the readiness probe.
pub const DEFAULT_PROBE_RETRIES: u32 = 20;
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_millis(250);
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Stop the service that owns the installed binary, before the rotation.
///
/// The entry runs at [`Phase::AfterBuild`] by default (register it with
/// [`DeployBuilder::on`](crate::DeployBuilder::on) at another phase if a plan
/// stops its service elsewhere). It
///
/// - remembers the instance it found in [`RunState::set_stopped_service`], so a
///   later [`ServiceStart`] can reuse its arguments,
/// - skips - without spawning anything, and without touching the process -
///   when [`install_is_current`] says the rotation will find the install
///   current (0.3's "no new install, no restart" rule, now library behaviour),
/// - otherwise calls [`stop_service`], which refuses to kill a pid whose image
///   is not the recorded binary (a rotated-aside `<binary>.old-*` copy counts as
///   the same instance).
#[derive(Debug, Clone)]
pub struct ServiceStop {
    /// The service to stop.
    pub spec: ServiceSpec,
    /// How long to wait for the process to exit.
    pub timeout: Duration,
    /// Skip (instead of stopping) when nothing would be installed.
    pub skip_when_current: bool,
}

impl ServiceStop {
    /// A stop entry for `spec`, skipping when nothing would be installed.
    pub fn new(spec: &ServiceSpec) -> Self {
        Self {
            spec: spec.clone(),
            timeout: DEFAULT_STOP_TIMEOUT,
            skip_when_current: true,
        }
    }

    /// Stop even when the rotation would find the install current.
    pub fn stop_anyway(mut self) -> Self {
        self.skip_when_current = false;
        self
    }

    /// How long to wait for the process to exit.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The entry as a phase handler, at [`Phase::AfterBuild`].
    pub fn handler(self) -> Handler {
        let spec = self.spec.clone();
        let timeout = self.timeout;
        let skip_when_current = self.skip_when_current;
        Handler::with_state("stop service", move |context, state| {
            if let Some(recorded) = ServiceState::read(&spec.state_file())? {
                state.set_stopped_service(recorded);
            }
            if skip_when_current && install_is_current(context) {
                return Ok(Outcome::skipped(
                    "nothing to install, the running instance keeps serving",
                ));
            }
            let report = stop_service(&spec, timeout)?;
            Ok(match report.outcome {
                StopOutcome::NotRunning => {
                    Outcome::skipped(format!("{} is not running", spec.name))
                }
                StopOutcome::Terminated => Outcome::Done {
                    detail: Some(format!("{} stopped ({})", spec.name, report.detail)),
                    artifacts: Vec::new(),
                    exit_code: None,
                },
            })
        })
        .at(Phase::AfterBuild)
        .describe(format!(
            "stop {} (skipped when nothing would be installed)",
            self.spec.name
        ))
    }
}

impl From<ServiceStop> for Handler {
    fn from(stop: ServiceStop) -> Self {
        stop.handler()
    }
}

/// Start the service again, after the rotation (and after a failure, when it is
/// registered at [`Phase::Finally`], which is the default).
///
/// The entry
///
/// - keeps an instance that is still running - the stop entry skipped, or the
///   plan failed before it - and reports it as skipped,
/// - otherwise starts `spec.binary` with the arguments of the instance that was
///   stopped when there was one (or `spec.args`), after the closure given to
///   [`ServiceStart::configure`] rewrote them,
/// - publishes the started instance through [`RunState::set_service`], so the
///   caller (or a later entry) reads the pid without parsing output,
/// - waits for the readiness probe when one is configured.
pub struct ServiceStart {
    /// The service to start.
    pub spec: ServiceSpec,
    /// Keep an instance that is already running instead of starting a second one.
    pub keep_if_running: bool,
    /// Reuse the arguments of the instance that was stopped, when there was one.
    pub args_from_state: bool,
    /// Rewrites the arguments right before the start.
    configure: Option<ArgRewrite>,
}

impl std::fmt::Debug for ServiceStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceStart")
            .field("spec", &self.spec)
            .field("keep_if_running", &self.keep_if_running)
            .field("args_from_state", &self.args_from_state)
            .field("configure", &self.configure.is_some())
            .finish()
    }
}

impl ServiceStart {
    /// A start entry for `spec`, keeping a running instance and reusing the
    /// stopped instance's arguments.
    pub fn new(spec: &ServiceSpec) -> Self {
        Self {
            spec: spec.clone(),
            keep_if_running: true,
            args_from_state: true,
            configure: None,
        }
    }

    /// Start even when an instance is already running (`start_detached` refuses
    /// to start a second one, so this turns that refusal into an error).
    pub fn always_start(mut self) -> Self {
        self.keep_if_running = false;
        self
    }

    /// Ignore the stopped instance's arguments and use `spec.args` as given.
    pub fn args_from_spec(mut self) -> Self {
        self.args_from_state = false;
        self
    }

    /// Rewrite the arguments right before the start - the place a consumer
    /// injects `--config`, a port, or a profile.
    pub fn configure(mut self, configure: impl Fn(Vec<String>) -> Vec<String> + 'static) -> Self {
        self.configure = Some(Box::new(configure));
        self
    }

    /// The entry as a phase handler, at [`Phase::Finally`].
    pub fn handler(self) -> Handler {
        let spec = self.spec.clone();
        let keep_if_running = self.keep_if_running;
        let args_from_state = self.args_from_state;
        let configure = self.configure;
        Handler::with_state("start service", move |_context, state| {
            if keep_if_running && let Some(instance) = find_running_instance(&spec)? {
                if let Some(pid) = instance.pid()
                    && let Some(recorded) = ServiceState::read(&spec.state_file())?
                    && recorded.pid == pid
                {
                    state.set_service(recorded);
                }
                return Ok(Outcome::skipped(format!(
                    "{} keeps running ({})",
                    spec.name,
                    instance.describe()
                )));
            }

            let mut start_spec = spec.clone();
            let from_state = args_from_state
                .then(|| state.stopped_service().map(|stopped| stopped.args.clone()))
                .flatten()
                .filter(|args| !args.is_empty());
            start_spec.args = from_state.unwrap_or_else(|| spec.args.clone());
            if let Some(configure) = &configure {
                start_spec.args = configure(start_spec.args);
            }
            let started = if start_spec.probe.is_some() {
                start_and_wait_ready(
                    &start_spec,
                    DEFAULT_PROBE_RETRIES,
                    DEFAULT_PROBE_INTERVAL,
                    DEFAULT_PROBE_TIMEOUT,
                )
                .map_err(|error| {
                    anyhow::anyhow!(
                        "{} did not become ready: {error:#}{}",
                        spec.name,
                        log_tail(&start_spec)
                    )
                })?
            } else {
                start_detached(&start_spec)?
            };
            state.set_service(started.clone());
            Ok(Outcome::Done {
                detail: Some(format!(
                    "started {} pid={}{}",
                    spec.name,
                    started.pid,
                    started
                        .listen
                        .as_deref()
                        .map(|listen| format!(" listen={listen}"))
                        .unwrap_or_default()
                )),
                artifacts: Vec::new(),
                exit_code: None,
            })
        })
        .at(Phase::Finally)
        .describe(format!(
            "start {} (keeps a running instance)",
            self.spec.name
        ))
    }

    /// The probe this entry waits for, when one is configured on the spec.
    pub fn probe(&self) -> Option<&ProbeSpec> {
        self.spec.probe.as_ref()
    }
}

impl From<ServiceStart> for Handler {
    fn from(start: ServiceStart) -> Self {
        start.handler()
    }
}

/// The tail of the service log, appended to a startup failure the way the 0.3
/// helpers did it.
fn log_tail(spec: &ServiceSpec) -> String {
    let Ok(tail) = spec.tail_log(crate::service::LOG_TAIL_LINES) else {
        return String::new();
    };
    if tail.trim().is_empty() {
        String::new()
    } else {
        format!("; log tail:\n{tail}")
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs, path::PathBuf, time::SystemTime};

    use super::*;
    use crate::context::DeployContext;
    use crate::plan::RunState;
    use crate::service::{ServiceState, StopReport};

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = env::temp_dir().join(format!("tracel-deploy-service-entry-{label}-{unique}"));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn spec(name: &str, dir: &std::path::Path, binary: &std::path::Path) -> ServiceSpec {
        ServiceSpec::new(
            name,
            binary,
            dir.join(format!("{name}.log")),
            dir.to_path_buf(),
        )
    }

    /// The stop entry has to skip *before* touching the process when the
    /// rotation would find the install current.
    #[test]
    fn stop_skips_without_spawning_when_the_install_is_current() {
        let dir = temp_dir("stop-skip");
        let built = dir.join("built-binary");
        let installed = dir.join("installed-binary");
        fs::write(&built, "same bytes").expect("built");
        fs::write(&installed, "same bytes").expect("installed");

        let mut context = DeployContext::new(&dir, "app");
        context.source_binary = Some(built);
        context.install_path = installed;
        context.state_dir = dir.join("state");

        let spec = spec("app", &dir, &dir.join("app.exe"));
        let mut handler = ServiceStop::new(&spec).handler();
        assert_eq!(handler.default_phase(), Some(Phase::AfterBuild));
        assert_eq!(
            handler.render(Phase::AfterBuild, &context),
            "after-build: stop app (skipped when nothing would be installed)"
        );

        let mut state = RunState::default();
        let outcome = handler
            .call(&context, &mut state, &mut crate::NullReporter)
            .expect("the skip is a success");
        assert!(outcome.is_skipped(), "outcome: {outcome:?}");
        assert_eq!(
            outcome.detail(),
            Some("nothing to install, the running instance keeps serving")
        );

        fs::remove_dir_all(&dir).ok();
    }

    /// Keeping a running instance is a skip: the probe answers, so nothing is
    /// started. (`find_running_instance` ignores the caller's own pid, so a test
    /// cannot play the instance itself.)
    #[test]
    fn start_keeps_a_responding_instance() {
        let dir = temp_dir("start-keep");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let addr = listener.local_addr().expect("addr");
        let mut spec = spec("app", &dir, &dir.join("app.exe"));
        spec.probe = Some(crate::service::ProbeSpec::tcp(addr));

        let mut context = DeployContext::new(&dir, "app");
        context.state_dir = dir.join("state");
        let mut handler = ServiceStart::new(&spec).handler();
        assert_eq!(handler.default_phase(), Some(Phase::Finally));
        assert_eq!(
            handler.render(Phase::Finally, &context),
            "finally: start app (keeps a running instance)"
        );

        let mut state = RunState::default();
        let outcome = handler
            .call(&context, &mut state, &mut crate::NullReporter)
            .expect("keeping the instance is a success");
        assert!(outcome.is_skipped(), "outcome: {outcome:?}");
        assert!(
            outcome
                .detail()
                .unwrap_or_default()
                .contains("keeps running"),
            "detail: {outcome:?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    /// The start path spawns the binary, writes the state file, and publishes the
    /// new instance to the run state - the pid a later entry reads instead of
    /// parsing output.
    #[test]
    fn start_spawns_writes_the_state_file_and_publishes_the_pid() {
        let dir = temp_dir("start-spawn");
        let binary = dir.join(if cfg!(windows) { "app.exe" } else { "app" });
        fs::copy(env::current_exe().expect("current exe"), &binary).expect("fixture image");
        let mut spec = spec("app", &dir, &binary);
        // The copy runs one (non-existent) ignored test and exits.
        spec.args = vec!["--exact".into(), "tracel-deploy-no-such-helper".into()];

        let mut context = DeployContext::new(&dir, "app");
        context.state_dir = dir.join("state");
        let mut handler = ServiceStart::new(&spec).args_from_spec().handler();
        let mut state = RunState::default();
        let outcome = handler
            .call(&context, &mut state, &mut crate::NullReporter)
            .expect("the start is a success");
        assert!(!outcome.is_skipped(), "outcome: {outcome:?}");

        let started = state.service().expect("the started instance is published");
        assert!(started.pid > 0, "pid: {}", started.pid);
        let recorded = ServiceState::read(&spec.state_file())
            .expect("read the state file")
            .expect("the state file exists");
        assert_eq!(recorded.pid, started.pid);
        assert_eq!(
            recorded.binary,
            crate::service::resolve_binary(&binary).expect("resolved")
        );

        fs::remove_dir_all(&dir).ok();
    }
    /// The stop entry reports a service that is not running as skipped.
    #[test]
    fn stop_reports_a_missing_instance_as_skipped() {
        let dir = temp_dir("stop-none");
        let spec = spec("app", &dir, &dir.join("app.exe"));
        let mut context = DeployContext::new(&dir, "app");
        context.state_dir = dir.join("state");

        let mut handler = ServiceStop::new(&spec).stop_anyway().handler();
        let mut state = RunState::default();
        let outcome = handler
            .call(&context, &mut state, &mut crate::NullReporter)
            .expect("stopping nothing is a success");
        assert!(outcome.is_skipped(), "outcome: {outcome:?}");
        assert_eq!(outcome.detail(), Some("app is not running"));
        let _: Option<StopReport> = None;

        fs::remove_dir_all(&dir).ok();
    }
}
