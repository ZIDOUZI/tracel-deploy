#![cfg(feature = "service")]
//! Optional service lifecycle helpers for `stop -> rotate -> start -> probe`.
//!
//! The module is compiled only with the `service` feature (off by default):
//!
//! ```toml
//! tracel-deploy = { version = "0.4", features = ["service"] }
//! ```
//!
//! It stays deliberately unopinionated about *what* a service is; it records
//! what was started and offers the two guardrails a deploy loop needs:
//!
//! 1. **Never stop a pid that is not the recorded image.** [`stop_checked`]
//!    compares the image path reported by the OS with the path recorded in the
//!    state file and refuses to kill on mismatch, so a recycled pid cannot take
//!    an unrelated process down. [`stop`] is the unguarded primitive for
//!    callers that verified the pid themselves.
//! 2. **Never start a second instance.** [`start_detached`] looks at the state
//!    file, then at the readiness probe, and finally scans running processes for
//!    the resolved image path. An instance started by a scheduled task that
//!    never wrote a state file is therefore detected before a duplicate is
//!    spawned.
//!
//! Platform support:
//!
//! - Windows: children are created with `CREATE_NO_WINDOW |
//!   CREATE_NEW_PROCESS_GROUP` (a console without a window, which the programs
//!   the child starts inherit), retried without `CREATE_BREAKAWAY_FROM_JOB` when
//!   the enclosing job forbids breakaway (`ERROR_ACCESS_DENIED`). Stop uses
//!   `TerminateProcess` (the `taskkill /F` equivalent) and the image path comes
//!   from `QueryFullProcessImageNameW`; the process scan uses a
//!   `CreateToolhelp32Snapshot` of the process list.
//! - Linux: children get their own session (`setsid`), stop sends `SIGTERM` and
//!   escalates to `SIGKILL`, and the image path comes from `/proc/<pid>/exe`.
//! - Other Unix (macOS, BSD): start and stop behave the same way, but the OS has
//!   no portable way to read another process' image path, so
//!   [`executable_path`] returns `Ok(None)` and [`stop_checked`] refuses instead
//!   of guessing; the process scan is unavailable there, configure a probe.
//! - Other targets (`wasm32-wasip1`, ...): process control reports an error.
//!
//! State files live in [`ServiceSpec::state_dir`] as `<name>.service` and use a
//! line oriented `key=value` format, matching the `<name>.stamp` file written by
//! [`crate::plan`]. They are written to `<name>.service.tmp` in the same
//! directory and renamed into place, so a reader never observes a partial file.
//! A missing, truncated or unparsable state file is reported as "no state"
//! (`Ok(None)`) instead of an error: the guardrails then fall back to probing
//! and scanning, which is the safe behaviour for a deploy loop.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};

pub mod deploy;

pub use deploy::{ServiceStart, ServiceStop};

/// Version of the state file format written by [`ServiceState::write`].
pub const STATE_SCHEMA: u32 = 1;

/// Number of log lines attached to start and readiness failures.
pub const LOG_TAIL_LINES: usize = 20;

/// Timeout used by the "is it already running?" guardrails before spawning.
const GUARD_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// How often [`stop`] polls a process while waiting for it to exit.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Extra grace granted to a forced kill before [`stop`] gives up.
const FORCE_KILL_GRACE: Duration = Duration::from_secs(2);

/// What to start, where its pid is recorded, and how to probe it.
///
/// ```ignore
/// let spec = ServiceSpec::new(
///     "mcp-hub",
///     "C:/Users/me/.cargo/bin/mcp-hub.exe",
///     "C:/Users/me/.local/state/mcp-hub/hub.log",
///     "C:/Users/me/.local/state/mcp-hub",
/// )
/// .args(["--listen", "127.0.0.1:8791"])
/// .env("RUST_LOG", "info")
/// .probe(ProbeSpec::tcp("127.0.0.1:8791".parse()?));
/// ```
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    /// Logical name, used for the state file name and diagnostics.
    pub name: String,
    /// Executable to start; a bare name is resolved through `PATH`.
    pub binary: PathBuf,
    /// Arguments passed to the binary.
    pub args: Vec<String>,
    /// Extra environment variables for the child.
    pub env: Vec<(String, String)>,
    /// Working directory for the child.
    pub cwd: Option<PathBuf>,
    /// File that receives the child's stdout and stderr (appended).
    pub log_file: PathBuf,
    /// Directory that holds `<name>.service`.
    pub state_dir: PathBuf,
    /// Optional readiness probe, used by [`wait_ready`] and the start guardrail.
    pub probe: Option<ProbeSpec>,
}

impl ServiceSpec {
    pub fn new(
        name: impl Into<String>,
        binary: impl Into<PathBuf>,
        log_file: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            binary: binary.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            log_file: log_file.into(),
            state_dir: state_dir.into(),
            probe: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn probe(mut self, probe: ProbeSpec) -> Self {
        self.probe = Some(probe);
        self
    }

    /// `<state_dir>/<name>.service`, the file used by the stop/start guardrails.
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join(format!("{}.service", self.name))
    }

    /// Last `lines` lines of [`ServiceSpec::log_file`]; empty when it is missing.
    pub fn tail_log(&self, lines: usize) -> Result<String> {
        tail_log(&self.log_file, lines)
    }
}

/// How to decide that a started service is ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProbeKind {
    /// A TCP connect to the listening socket.
    #[default]
    Tcp,
    /// A hand written `GET`; requires a 2xx, or [`ProbeSpec::expect_status`].
    Http,
}

impl ProbeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Http => "http",
        }
    }
}

/// Readiness probe target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSpec {
    pub kind: ProbeKind,
    pub addr: std::net::SocketAddr,
    /// Request path for [`ProbeKind::Http`]; must start with `/`.
    pub path: String,
    /// Exact HTTP status that counts as ready; any 2xx when `None`.
    pub expect_status: Option<u16>,
}

impl ProbeSpec {
    pub fn tcp(addr: std::net::SocketAddr) -> Self {
        Self {
            kind: ProbeKind::Tcp,
            addr,
            path: "/".to_owned(),
            expect_status: None,
        }
    }

    pub fn http(addr: std::net::SocketAddr) -> Self {
        Self {
            kind: ProbeKind::Http,
            addr,
            path: "/healthz".to_owned(),
            expect_status: None,
        }
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    pub fn expect_status(mut self, code: u16) -> Self {
        self.expect_status = Some(code);
        self
    }

    /// Human readable target, recorded in the state file as `listen`.
    pub fn target(&self) -> String {
        match self.kind {
            ProbeKind::Tcp => format!("tcp://{}", self.addr),
            ProbeKind::Http => format!("http://{}{}", self.addr, self.path),
        }
    }
}

/// Outcome of one probe attempt.
#[derive(Debug, Clone)]
pub struct ProbeReport {
    /// Whether the probe answered the way the spec requires.
    pub ok: bool,
    /// How many attempts were made (always 1 for [`probe_once`]).
    pub attempts: u32,
    /// Wall clock time spent probing.
    pub elapsed: Duration,
    /// HTTP status code, when the probe was HTTP and a status line was read.
    pub status: Option<u16>,
    /// Human readable detail (the failure reason when `ok` is false).
    pub detail: String,
}

/// The process a service instance runs under, as recorded in `<name>.service`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceState {
    /// Logical service name.
    pub name: String,
    /// Process id of the started instance.
    pub pid: u32,
    /// Absolute path of the image the instance was started from.
    pub binary: PathBuf,
    /// Arguments the instance was started with.
    pub args: Vec<String>,
    /// File that receives the instance's stdout and stderr.
    pub log_file: Option<PathBuf>,
    /// Readiness target (`tcp://host:port` or `http://host:port/path`).
    pub listen: Option<String>,
    /// Start time as Unix seconds.
    pub started_at: u64,
}

impl ServiceState {
    /// Read a state file. A missing, truncated, corrupt or future-schema file is
    /// reported as `Ok(None)`; IO errors other than "not found" are errors.
    pub fn read(path: &Path) -> Result<Option<Self>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not read state {}", path.display()));
            }
        };
        Ok(parse_state(&String::from_utf8_lossy(&bytes)))
    }

    /// Atomically write the state file: `<path>.tmp` first, then rename.
    pub fn write(&self, path: &Path) -> Result<()> {
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(directory)
            .with_context(|| format!("could not create state dir {}", directory.display()))?;
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);

        let mut contents = format!(
            "schema={STATE_SCHEMA}\nname={}\npid={}\nbinary={}\n",
            encode_field(&self.name),
            self.pid,
            encode_field(&self.binary.to_string_lossy())
        );
        if !self.args.is_empty() {
            let args: Vec<String> = self.args.iter().map(|arg| encode_field(arg)).collect();
            contents.push_str(&format!("args={}\n", args.join("|")));
        }
        if let Some(log_file) = &self.log_file {
            contents.push_str(&format!(
                "log={}\n",
                encode_field(&log_file.to_string_lossy())
            ));
        }
        if let Some(listen) = &self.listen {
            contents.push_str(&format!("listen={}\n", encode_field(listen)));
        }
        contents.push_str(&format!("started_at={}\n", self.started_at));

        fs::write(&temporary, contents)
            .with_context(|| format!("could not write state {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("could not replace state {}", path.display()))?;
        Ok(())
    }

    /// Remove a state file, ignoring failures.
    pub fn remove(path: &Path) {
        let _ = fs::remove_file(path);
    }

    /// Last `lines` lines of the recorded log; empty when there is none.
    pub fn tail_log(&self, lines: usize) -> Result<String> {
        match &self.log_file {
            Some(log_file) => tail_log(log_file, lines),
            None => Ok(String::new()),
        }
    }

    /// One line summary for status output and error messages.
    pub fn describe(&self) -> String {
        format!(
            "pid={} binary={} args=[{}] log={} listen={} started_at={}",
            self.pid,
            self.binary.display(),
            self.args.join(" "),
            self.log_file
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "-".to_owned()),
            self.listen.as_deref().unwrap_or("-"),
            self.started_at
        )
    }
}

/// What [`stop_checked`] or [`stop_service`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Nothing matched, or the recorded pid was already gone (idempotent success).
    NotRunning,
    /// This call terminated the instance.
    Terminated,
}

/// Result of [`stop_checked`] or [`stop_service`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReport {
    pub outcome: StopOutcome,
    /// The pid that was stopped, when there was one.
    pub pid: Option<u32>,
    pub detail: String,
}

/// How a running instance was detected by [`find_running_instance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instance {
    /// The pid recorded in the state file is alive.
    Recorded(u32),
    /// A running process was matched by its image path.
    Scanned(u32),
    /// The readiness probe already answers (no pid known).
    Ready,
}

impl Instance {
    pub fn pid(self) -> Option<u32> {
        match self {
            Self::Recorded(pid) | Self::Scanned(pid) => Some(pid),
            Self::Ready => None,
        }
    }

    pub fn describe(self) -> String {
        match self {
            Self::Recorded(pid) => format!("state file records live pid {pid}"),
            Self::Scanned(pid) => format!("running pid {pid} matches the image path"),
            Self::Ready => "the readiness probe already answers".to_owned(),
        }
    }
}

/// Whether `pid` exists. A process that cannot be inspected because of
/// permissions counts as alive; pids 0 and out-of-range pids are not processes.
pub fn is_alive(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Ok(false);
    }
    sys::is_alive(pid)
}

/// Absolute image path of `pid`.
///
/// `Ok(None)` means "unknown": the process is gone, it belongs to another user,
/// or the platform cannot report image paths (macOS, BSD). A caller that needs
/// the path for a safety check must treat `None` as "cannot verify".
pub fn executable_path(pid: u32) -> Result<Option<PathBuf>> {
    if pid == 0 {
        return Ok(None);
    }
    sys::executable_path(pid)
}

/// Terminate `pid` and wait up to `timeout` for it to exit.
///
/// Idempotent: a pid that does not exist is a success. Unix sends `SIGTERM`
/// first and escalates to `SIGKILL` once `timeout` elapsed; Windows terminates
/// the process directly (console programs have no graceful signal there).
///
/// This is the unguarded primitive; prefer [`stop_checked`] or [`stop_service`],
/// which verify the image path first.
pub fn stop(pid: u32, timeout: Duration) -> Result<()> {
    if !is_alive(pid)? {
        return Ok(());
    }
    sys::request_exit(pid)?;
    if wait_for_exit(pid, timeout)? {
        return Ok(());
    }
    sys::force_kill(pid)?;
    let grace = timeout.min(FORCE_KILL_GRACE).max(EXIT_POLL_INTERVAL);
    if wait_for_exit(pid, grace)? {
        return Ok(());
    }
    bail!("process {pid} did not exit within {timeout:?} even after a forced kill");
}

/// Stop the process recorded in `state`, but only after checking that its image
/// path is still the recorded binary.
///
/// Refuses (with an error) when the pid is alive but its image path differs from
/// `state.binary`, or when the image path cannot be read at all: that keeps a
/// recycled pid from taking an unrelated process down. The state file is left
/// untouched so the operator can inspect it.
///
/// A rotated-aside copy of the recorded binary (`<binary>.old`,
/// `<binary>.old-<stamp>`, written by the rotation when it moves a running
/// destination out of the way) counts as the same service.
pub fn stop_checked(state: &ServiceState, timeout: Duration) -> Result<StopReport> {
    if !is_alive(state.pid)? {
        return Ok(StopReport {
            outcome: StopOutcome::NotRunning,
            pid: Some(state.pid),
            detail: format!("pid {} is not running", state.pid),
        });
    }
    let expected = &state.binary;
    match executable_path(state.pid)? {
        Some(actual) if same_binary_or_rotated(&actual, expected) => {}
        Some(actual) => bail!(
            "refusing to stop pid {}: image {} does not match the recorded {}",
            state.pid,
            actual.display(),
            expected.display()
        ),
        None => bail!(
            "refusing to stop pid {}: cannot read its image path (expected {}); \
             verify manually and use stop(pid, timeout) if it is the right process",
            state.pid,
            expected.display()
        ),
    }
    stop(state.pid, timeout)?;
    Ok(StopReport {
        outcome: StopOutcome::Terminated,
        pid: Some(state.pid),
        detail: format!("stopped pid {}", state.pid),
    })
}

/// Stop the instance described by `spec` and remove the state file.
///
/// Order: the state file (image checked by [`stop_checked`]) -> a running
/// process matching the resolved image path (the scheduled-task case that wrote
/// no state file) -> nothing. Idempotent: stopping a service that is not running
/// reports [`StopOutcome::NotRunning`].
pub fn stop_service(spec: &ServiceSpec, timeout: Duration) -> Result<StopReport> {
    let state_file = spec.state_file();
    if let Some(state) = ServiceState::read(&state_file)? {
        if is_alive(state.pid)? {
            let report = stop_checked(&state, timeout)?;
            ServiceState::remove(&state_file);
            return Ok(report);
        }
        ServiceState::remove(&state_file);
    }

    match find_running_instance(spec)? {
        Some(Instance::Recorded(pid) | Instance::Scanned(pid)) => {
            stop(pid, timeout)?;
            Ok(StopReport {
                outcome: StopOutcome::Terminated,
                pid: Some(pid),
                detail: format!("stopped pid {pid} (no state file)"),
            })
        }
        Some(Instance::Ready) => Ok(StopReport {
            outcome: StopOutcome::NotRunning,
            pid: None,
            detail: format!(
                "the readiness probe for {} still answers but no running process matches {}",
                spec.name,
                spec.binary.display()
            ),
        }),
        None => Ok(StopReport {
            outcome: StopOutcome::NotRunning,
            pid: None,
            detail: format!("{} is not running", spec.name),
        }),
    }
}

/// Start `spec` detached and record its pid in `<state_dir>/<name>.service`.
///
/// The guardrails run first: a recorded live pid, an already answering probe, or
/// a running process whose image path matches the resolved binary abort the start
/// with an error instead of spawning a duplicate.
///
/// stdout and stderr are appended to [`ServiceSpec::log_file`]. The returned
/// [`ServiceState`] only reports that the process was created; use
/// [`wait_ready`], or [`start_and_wait_ready`], to wait until it serves.
pub fn start_detached(spec: &ServiceSpec) -> Result<ServiceState> {
    if let Some(instance) = find_running_instance(spec)? {
        bail!("{} is already running: {}", spec.name, instance.describe());
    }

    let binary = match resolve_binary(&spec.binary) {
        Ok(binary) => binary,
        Err(error) => return Err(with_log_tail(error, &spec.log_file, LOG_TAIL_LINES)),
    };
    if let Some(directory) = spec.log_file.parent() {
        fs::create_dir_all(directory)
            .with_context(|| format!("could not create log dir {}", directory.display()))?;
    }
    let stdout = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log_file)
        .with_context(|| format!("could not open log {}", spec.log_file.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("could not open log {}", spec.log_file.display()))?;

    let mut command = Command::new(&binary);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    if let Some(directory) = &spec.cwd {
        command.current_dir(directory);
    }

    let child = sys::spawn_detached(&mut command).map_err(|error| {
        with_log_tail(
            anyhow::Error::new(error).context(format!(
                "could not start {} ({})",
                spec.name,
                binary.display()
            )),
            &spec.log_file,
            LOG_TAIL_LINES,
        )
    })?;
    let pid = child.id();
    // Detach: the child keeps running and its pid is the handle. Dropping the
    // `Child` closes our process handle without waiting for it.
    drop(child);

    let state = ServiceState {
        name: spec.name.clone(),
        pid,
        binary,
        args: spec.args.clone(),
        log_file: Some(spec.log_file.clone()),
        listen: spec.probe.as_ref().map(ProbeSpec::target),
        started_at: unix_secs(),
    };
    let state_file = spec.state_file();
    if let Err(error) = state.write(&state_file) {
        // Never leave an unrecorded instance behind.
        let _ = stop(pid, Duration::from_secs(5));
        return Err(with_log_tail(
            error.context(format!(
                "could not record service state {}",
                state_file.display()
            )),
            &spec.log_file,
            LOG_TAIL_LINES,
        ));
    }
    Ok(state)
}

/// [`start_detached`] plus [`wait_ready`].
///
/// An instance that never becomes ready is terminated again, so a half started
/// service does not keep a port, and reported as an error carrying the last
/// [`LOG_TAIL_LINES`] lines of the log.
pub fn start_and_wait_ready(
    spec: &ServiceSpec,
    retries: u32,
    interval: Duration,
    timeout: Duration,
) -> Result<ServiceState> {
    let Some(probe) = spec.probe.clone() else {
        bail!(
            "{} has no readiness probe; use start_detached plus is_alive",
            spec.name
        );
    };
    let state = start_detached(spec)?;
    if let Err(error) = wait_ready(&probe, retries, interval, timeout) {
        let _ = stop(state.pid, Duration::from_secs(5));
        return Err(with_log_tail(
            error.context(format!(
                "{} did not become ready (pid {})",
                spec.name, state.pid
            )),
            &spec.log_file,
            LOG_TAIL_LINES,
        ));
    }
    Ok(state)
}

/// One probe attempt; never fails, the outcome is in [`ProbeReport::ok`].
pub fn probe_once(probe: &ProbeSpec, timeout: Duration) -> ProbeReport {
    let started = Instant::now();
    let outcome = match probe.kind {
        ProbeKind::Tcp => tcp_probe(probe.addr, timeout).map(|()| None),
        ProbeKind::Http => http_probe(probe, timeout),
    };
    match outcome {
        Ok(status) => ProbeReport {
            ok: true,
            attempts: 1,
            elapsed: started.elapsed(),
            status,
            detail: format!(
                "{} ready{}",
                probe.target(),
                status
                    .map(|code| format!(" (HTTP {code})"))
                    .unwrap_or_default()
            ),
        },
        Err(error) => ProbeReport {
            ok: false,
            attempts: 1,
            elapsed: started.elapsed(),
            status: None,
            detail: format!("{error:#}"),
        },
    }
}

/// Probe up to `retries` times (at least once), sleeping `interval` between
/// attempts, and return the reporting probe.
///
/// Returns an error when every attempt failed; the message carries the target,
/// the attempt count and the last failure. `timeout` bounds each attempt's
/// connect, read and write.
pub fn wait_ready(
    probe: &ProbeSpec,
    retries: u32,
    interval: Duration,
    timeout: Duration,
) -> Result<ProbeReport> {
    let attempts = retries.max(1);
    let started = Instant::now();
    let mut report = probe_once(probe, timeout);
    let mut taken = 1;
    while !report.ok && taken < attempts {
        std::thread::sleep(interval);
        report = probe_once(probe, timeout);
        taken += 1;
    }
    if !report.ok {
        bail!(
            "{} not ready after {taken} attempt(s): {}",
            probe.target(),
            report.detail
        );
    }
    Ok(ProbeReport {
        attempts: taken,
        elapsed: started.elapsed(),
        ..report
    })
}

/// Last `lines` lines of a log file, joined with `\n`.
///
/// A missing file is an empty string (diagnostics must never mask the original
/// failure) and non UTF-8 bytes are replaced lossily. The whole file is read:
/// deploy logs are small compared to the diagnostics value.
pub fn tail_log(log_file: &Path, lines: usize) -> Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }
    let bytes = match fs::read(log_file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not read log {}", log_file.display()));
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let tail: Vec<&str> = text.lines().rev().take(lines).collect();
    Ok(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
}

/// Detect an already running instance of `spec` without starting anything.
///
/// Checks, in order: the live pid recorded in the state file (plus its image
/// path when the OS can report it), the readiness probe, and finally a scan of
/// running processes for the resolved image path. The calling process is never
/// reported, so a deploy loop cannot mistake itself for the service.
pub fn find_running_instance(spec: &ServiceSpec) -> Result<Option<Instance>> {
    let state_file = spec.state_file();
    if let Some(state) = ServiceState::read(&state_file)?
        && state.pid != std::process::id()
        && is_alive(state.pid)?
    {
        let recorded = match executable_path(state.pid)? {
            Some(actual) => same_binary_or_rotated(&actual, &state.binary),
            // Unverifiable (another user, platform without image paths): trust
            // the state file, like a human operator would.
            None => true,
        };
        if recorded {
            return Ok(Some(Instance::Recorded(state.pid)));
        }
    }

    if let Some(probe) = &spec.probe
        && probe_once(probe, GUARD_PROBE_TIMEOUT).ok
    {
        return Ok(Some(Instance::Ready));
    }

    let Ok(binary) = resolve_binary(&spec.binary) else {
        return Ok(None);
    };
    for pid in sys::candidate_pids(&binary)? {
        if pid == std::process::id() {
            continue;
        }
        if let Some(actual) = executable_path(pid)?
            && same_binary_or_rotated(&actual, &binary)
        {
            return Ok(Some(Instance::Scanned(pid)));
        }
    }
    Ok(None)
}

/// Resolve `binary` to an existing file: a path is used as given, a bare name is
/// searched in `PATH` (also trying the `.exe` suffix on Windows).
pub fn resolve_binary(binary: &Path) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let path_like = binary.is_absolute() || binary.components().count() > 1;
    if path_like {
        candidates.push(binary.to_path_buf());
    } else {
        if let Some(path) = env::var_os("PATH") {
            for directory in env::split_paths(&path) {
                candidates.push(directory.join(binary));
            }
        }
        candidates.push(binary.to_path_buf());
    }

    for candidate in &candidates {
        if is_executable_file(candidate) {
            return Ok(canonical_or(candidate.clone()));
        }
        #[cfg(windows)]
        if candidate.extension().is_none() {
            let with_suffix = candidate.with_extension("exe");
            if is_executable_file(&with_suffix) {
                return Ok(canonical_or(with_suffix));
            }
        }
    }
    bail!(
        "executable not found: {} (searched {} path entries)",
        binary.display(),
        candidates.len().saturating_sub(1)
    )
}

fn tcp_probe(addr: std::net::SocketAddr, timeout: Duration) -> Result<()> {
    let stream = std::net::TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("connect {addr}"))?;
    drop(stream);
    Ok(())
}

fn http_probe(probe: &ProbeSpec, timeout: Duration) -> Result<Option<u16>> {
    use std::{
        io::{Read as _, Write as _},
        net::{SocketAddr, TcpStream},
    };

    if !probe.path.starts_with('/') {
        bail!("HTTP probe path must start with '/' (got {:?})", probe.path);
    }
    let addr = probe.addr;
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).with_context(|| format!("connect {addr}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .with_context(|| format!("set the read timeout for {addr}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .with_context(|| format!("set the write timeout for {addr}"))?;
    let host = match addr {
        SocketAddr::V4(_) => addr.to_string(),
        SocketAddr::V6(_) => format!("[{addr}]"),
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n",
        probe.path
    );
    stream
        .write_all(request.as_bytes())
        .with_context(|| format!("write {addr}"))?;
    let _ = stream.flush();

    // The status line decides readiness; stop reading there.
    let mut buffer = Vec::with_capacity(256);
    let mut chunk = [0_u8; 256];
    loop {
        let read = stream
            .read(&mut chunk)
            .with_context(|| format!("read {addr}"))?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.contains(&b'\n') || buffer.len() >= 8_192 {
            break;
        }
    }
    let response = String::from_utf8_lossy(&buffer);
    let status_line = response
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty HTTP response from {addr}"))?;
    let code = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed HTTP status line {status_line:?}"))?;
    let code: u16 = code
        .parse()
        .with_context(|| format!("malformed HTTP status code {code:?}"))?;
    match probe.expect_status {
        Some(expected) if code != expected => bail!("HTTP {code} (expected {expected})"),
        Some(_) => Ok(Some(code)),
        None if (200..300).contains(&code) => Ok(Some(code)),
        None => bail!("HTTP {code}"),
    }
}

fn wait_for_exit(pid: u32, timeout: Duration) -> Result<bool> {
    let started = Instant::now();
    loop {
        if !is_alive(pid)? {
            return Ok(true);
        }
        let remaining = match timeout.checked_sub(started.elapsed()) {
            Some(remaining) => remaining,
            None => return Ok(false),
        };
        std::thread::sleep(remaining.min(EXIT_POLL_INTERVAL));
    }
}

fn with_log_tail(error: anyhow::Error, log_file: &Path, lines: usize) -> anyhow::Error {
    match tail_log(log_file, lines) {
        Ok(tail) if !tail.trim().is_empty() => {
            error.context(format!("log tail {}:\n{tail}", log_file.display()))
        }
        _ => error,
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn parse_state(contents: &str) -> Option<ServiceState> {
    let mut schema: Option<u32> = None;
    let mut name: Option<String> = None;
    let mut pid: Option<u32> = None;
    let mut binary: Option<PathBuf> = None;
    let mut args: Vec<String> = Vec::new();
    let mut log_file: Option<PathBuf> = None;
    let mut listen: Option<String> = None;
    let mut started_at: u64 = 0;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let value = value.trim();
        match key {
            "schema" => schema = value.parse().ok(),
            "name" => name = Some(decode_field(value)?),
            "pid" => pid = value.parse().ok(),
            "binary" => binary = Some(PathBuf::from(decode_field(value)?)),
            "args" => {
                args = if value.is_empty() {
                    Vec::new()
                } else {
                    value
                        .split('|')
                        .map(decode_field)
                        .collect::<Option<Vec<String>>>()?
                };
            }
            "log" => log_file = Some(PathBuf::from(decode_field(value)?)),
            "listen" => listen = Some(decode_field(value)?),
            "started_at" => started_at = value.parse().unwrap_or_default(),
            // Unknown keys are ignored so newer writers stay readable.
            _ => {}
        }
    }

    if schema != Some(STATE_SCHEMA) {
        return None;
    }
    Some(ServiceState {
        name: name?,
        pid: pid?,
        binary: binary?,
        args,
        log_file,
        listen,
        started_at,
    })
}

/// Escape one field: `\` -> `\\`, `|` -> `\p`, newline and CR escaped.
fn encode_field(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => encoded.push_str("\\\\"),
            '|' => encoded.push_str("\\p"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            other => encoded.push(other),
        }
    }
    encoded
}

fn decode_field(value: &str) -> Option<String> {
    let mut decoded = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters.next()? {
            '\\' => decoded.push('\\'),
            'p' => decoded.push('|'),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            _ => return None,
        }
    }
    Some(decoded)
}

fn canonical_or(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

/// Compare two image paths, resolving symlinks and ignoring Windows case and the
/// `\\?\` prefix.
fn same_binary(left: &Path, right: &Path) -> bool {
    let left = normalize_path(&canonical_or(left.to_path_buf()));
    let right = normalize_path(&canonical_or(right.to_path_buf()));
    left == right
}

/// [`same_binary`], plus a rotated-aside copy of the expected binary
/// (`<binary>.old`, `<binary>.old-<stamp>`) in the same directory.
///
/// The rotation renames the running destination aside on Windows, so the
/// instance that owns the install path can still be running from its backup -
/// that is the recorded instance, not a recycled pid, and refusing to stop it
/// would leave the deployment without a rotation target.
fn same_binary_or_rotated(left: &Path, right: &Path) -> bool {
    if same_binary(left, right) {
        return true;
    }
    let (Some(left_name), Some(right_name)) = (left.file_name(), right.file_name()) else {
        return false;
    };
    let left_name = left_name.to_string_lossy().to_lowercase();
    let right_name = right_name.to_string_lossy().to_lowercase();
    if !left_name.starts_with(&format!("{right_name}.old")) {
        return false;
    }
    match (left.parent(), right.parent()) {
        (Some(left_dir), Some(right_dir)) => normalize_path(left_dir) == normalize_path(right_dir),
        _ => false,
    }
}

#[cfg(windows)]
fn normalize_path(path: &Path) -> String {
    let lowered = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let stripped = lowered.strip_prefix("\\\\?\\").unwrap_or(lowered.as_str());
    stripped.trim_end_matches('\\').to_owned()
}

#[cfg(not(windows))]
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().trim_end_matches('/').to_owned()
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    path.is_file()
        && path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Platform process primitives. The behaviour callers depend on is documented on
/// the public wrappers above.
#[cfg(windows)]
mod sys {
    use std::{
        ffi::c_void,
        io,
        os::windows::ffi::OsStringExt as _,
        path::{Path, PathBuf},
        process::{Child, Command},
    };

    use anyhow::{Result, bail};

    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const WAIT_TIMEOUT: u32 = 258;
    const MAX_PATH: usize = 260;

    #[allow(non_snake_case)]
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn TerminateProcess(handle: *mut c_void, code: u32) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn QueryFullProcessImageNameW(
            handle: *mut c_void,
            flags: u32,
            buffer: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
        fn Process32FirstW(snapshot: *mut c_void, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: *mut c_void, entry: *mut ProcessEntry32W) -> i32;
    }

    /// `PROCESSENTRY32W`, the subset `Process32FirstW` fills in.
    #[repr(C)]
    struct ProcessEntry32W {
        size: u32,
        usage: u32,
        pid: u32,
        default_heap_id: usize,
        module_id: u32,
        threads: u32,
        parent_pid: u32,
        priority_class: i32,
        flags: u32,
        executable: [u16; MAX_PATH],
    }

    impl ProcessEntry32W {
        fn new() -> Self {
            Self {
                size: std::mem::size_of::<Self>() as u32,
                usage: 0,
                pid: 0,
                default_heap_id: 0,
                module_id: 0,
                threads: 0,
                parent_pid: 0,
                priority_class: 0,
                flags: 0,
                executable: [0; MAX_PATH],
            }
        }

        fn image_name(&self) -> String {
            let length = self
                .executable
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(MAX_PATH);
            std::ffi::OsString::from_wide(&self.executable[..length])
                .to_string_lossy()
                .into_owned()
        }
    }

    /// Process handle that closes itself.
    struct Handle(*mut c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    enum Open {
        Alive(Handle),
        /// The process exists but this user may not inspect it.
        Denied,
        Gone,
    }

    fn open(pid: u32, access: u32) -> Result<Open> {
        let raw = unsafe { OpenProcess(access, 0, pid) };
        if !raw.is_null() {
            return Ok(Open::Alive(Handle(raw)));
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(ERROR_ACCESS_DENIED) => Ok(Open::Denied),
            // Windows reports an unknown pid as an invalid parameter.
            Some(ERROR_INVALID_PARAMETER) => Ok(Open::Gone),
            other => bail!("OpenProcess({pid}) failed: {other:?}"),
        }
    }

    pub fn is_alive(pid: u32) -> Result<bool> {
        match open(pid, SYNCHRONIZE)? {
            Open::Denied => Ok(true),
            Open::Gone => Ok(false),
            // A terminated process that someone still holds a handle to is
            // signalled; a running one is not.
            Open::Alive(handle) => Ok(unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_TIMEOUT),
        }
    }

    pub fn executable_path(pid: u32) -> Result<Option<PathBuf>> {
        let handle = match open(pid, PROCESS_QUERY_LIMITED_INFORMATION)? {
            Open::Alive(handle) => handle,
            Open::Denied | Open::Gone => return Ok(None),
        };
        let mut buffer = vec![0_u16; 32 * 1024];
        let mut size = buffer.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle.0, 0, buffer.as_mut_ptr(), &mut size) };
        if ok == 0 || size == 0 {
            // Protected or otherwise inaccessible process: report "unknown".
            return Ok(None);
        }
        buffer.truncate(size as usize);
        Ok(Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer))))
    }

    /// There is no graceful signal for console programs on Windows, so both
    /// phases are `TerminateProcess` (the `taskkill /F` equivalent).
    pub fn request_exit(pid: u32) -> Result<()> {
        terminate(pid)
    }

    pub fn force_kill(pid: u32) -> Result<()> {
        terminate(pid)
    }

    fn terminate(pid: u32) -> Result<()> {
        let handle = match open(pid, PROCESS_TERMINATE | SYNCHRONIZE)? {
            Open::Alive(handle) => handle,
            Open::Gone => return Ok(()),
            Open::Denied => bail!("no permission to terminate pid {pid}"),
        };
        if unsafe { TerminateProcess(handle.0, 1) } == 0 {
            let error = io::Error::last_os_error();
            bail!("TerminateProcess({pid}) failed: {error}");
        }
        Ok(())
    }

    /// `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, plus
    /// `CREATE_BREAKAWAY_FROM_JOB` when the enclosing job allows breakaway. A
    /// job that forbids it answers `ERROR_ACCESS_DENIED`; the retry then keeps
    /// the plain detachment.
    pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
        crate::detach::spawn_detached(command)
    }

    /// Candidate pids for `image`, filtered by image name; the caller verifies
    /// the full image path.
    ///
    /// The process list comes from `CreateToolhelp32Snapshot` rather than from
    /// `tasklist`: it needs no child process (no console window when the deploy
    /// runs detached) and works where `tasklist` is blocked by policy.
    pub fn candidate_pids(image: &Path) -> Result<Vec<u32>> {
        let Some(name) = image
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            return Ok(Vec::new());
        };
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot as isize == -1 {
            bail!(
                "CreateToolhelp32Snapshot failed: {}",
                io::Error::last_os_error()
            );
        }
        let snapshot = Handle(snapshot);
        let mut entry = ProcessEntry32W::new();
        let mut pids = Vec::new();
        if unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0 {
            loop {
                if entry.image_name().eq_ignore_ascii_case(&name) {
                    pids.push(entry.pid);
                }
                if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
                    break;
                }
            }
        }
        Ok(pids)
    }
}

/// Unix process primitives: signals through `kill(2)`, sessions through
/// `setsid(2)`.
#[cfg(unix)]
mod sys {
    use std::{
        ffi::c_int,
        io,
        os::unix::process::CommandExt as _,
        path::{Path, PathBuf},
        process::{Child, Command},
    };

    use anyhow::{Result, bail};

    const SIGTERM: c_int = 15;
    const SIGKILL: c_int = 9;
    const EPERM: i32 = 1;
    const ESRCH: i32 = 3;
    const EACCES: i32 = 13;
    const ENOENT: i32 = 2;

    #[allow(non_snake_case)]
    unsafe extern "C" {
        fn kill(pid: c_int, signal: c_int) -> c_int;
        fn setsid() -> c_int;
    }

    fn checked_pid(pid: u32) -> Result<c_int> {
        if pid == 0 || pid > c_int::MAX as u32 {
            bail!("invalid pid {pid}");
        }
        Ok(pid as c_int)
    }

    pub fn is_alive(pid: u32) -> Result<bool> {
        let pid = checked_pid(pid)?;
        if unsafe { kill(pid, 0) } == 0 {
            return Ok(true);
        }
        match io::Error::last_os_error().raw_os_error() {
            // EPERM means the process exists, we just may not signal it.
            Some(EPERM) => Ok(true),
            Some(ESRCH) => Ok(false),
            other => bail!("kill({pid}, 0) failed: {other:?}"),
        }
    }

    pub fn executable_path(pid: u32) -> Result<Option<PathBuf>> {
        #[cfg(target_os = "linux")]
        {
            let link = PathBuf::from(format!("/proc/{pid}/exe"));
            match std::fs::read_link(&link) {
                Ok(target) => {
                    let text = target.to_string_lossy().into_owned();
                    // A binary that was replaced while running shows up as
                    // "<path> (deleted)".
                    let cleaned = text
                        .strip_suffix(" (deleted)")
                        .map(PathBuf::from)
                        .unwrap_or(target);
                    Ok(Some(cleaned))
                }
                // No such process, or not ours to inspect: "unknown".
                Err(error) if matches!(error.raw_os_error(), Some(ENOENT) | Some(EACCES)) => {
                    Ok(None)
                }
                Err(error) => bail!("could not read {}: {error}", link.display()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // macOS and the BSDs have no portable equivalent (`proc_pidpath` is
            // libproc specific); report "unknown" rather than guessing.
            let _ = pid;
            Ok(None)
        }
    }

    pub fn request_exit(pid: u32) -> Result<()> {
        signal(pid, SIGTERM)
    }

    pub fn force_kill(pid: u32) -> Result<()> {
        signal(pid, SIGKILL)
    }

    fn signal(pid: u32, signal: c_int) -> Result<()> {
        let pid = checked_pid(pid)?;
        if unsafe { kill(pid, signal) } == 0 {
            return Ok(());
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(ESRCH) => Ok(()),
            Some(EPERM) => bail!("no permission to signal pid {pid}"),
            other => bail!("kill({pid}, {signal}) failed: {other:?}"),
        }
    }

    /// Start the child in its own session, so it survives the deploy process and
    /// is not signalled together with the caller's session or job.
    pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
        unsafe {
            command.pre_exec(|| {
                if setsid() == -1 {
                    let error = io::Error::last_os_error();
                    // Nothing to do when the child is already a session leader.
                    if error.raw_os_error() != Some(EPERM) {
                        return Err(error);
                    }
                }
                Ok(())
            });
        }
        command.spawn()
    }

    /// Candidate pids for `image`; the caller verifies the full image path.
    pub fn candidate_pids(_image: &Path) -> Result<Vec<u32>> {
        #[cfg(target_os = "linux")]
        {
            let mut pids = Vec::new();
            for entry in std::fs::read_dir("/proc")? {
                let entry = entry?;
                if let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
                {
                    pids.push(pid);
                }
            }
            Ok(pids)
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Without `/proc` (or `proc_pidpath`) there is nothing to match an
            // image path against; configure a probe instead.
            Ok(Vec::new())
        }
    }
}

/// Targets without process control: the API stays usable, process operations
/// report an error and the guardrails find no instances.
#[cfg(not(any(windows, unix)))]
mod sys {
    use std::{
        io,
        path::{Path, PathBuf},
        process::{Child, Command},
    };

    use anyhow::{Result, bail};

    pub fn is_alive(_pid: u32) -> Result<bool> {
        Ok(false)
    }

    pub fn executable_path(_pid: u32) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    pub fn request_exit(pid: u32) -> Result<()> {
        bail!("stopping processes is not supported on this target (pid {pid})")
    }

    pub fn force_kill(pid: u32) -> Result<()> {
        bail!("killing processes is not supported on this target (pid {pid})")
    }

    pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
        command.spawn()
    }

    pub fn candidate_pids(_image: &Path) -> Result<Vec<u32>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::same_binary_or_rotated;

    #[test]
    fn a_rotated_aside_copy_still_counts_as_the_same_service() {
        let installed = Path::new(r"C:\Users\me\.cargo\bin\app.exe");

        assert!(same_binary_or_rotated(installed, installed));
        assert!(same_binary_or_rotated(
            Path::new(r"C:\Users\me\.cargo\bin\app.exe.old"),
            installed
        ));
        assert!(same_binary_or_rotated(
            Path::new(r"C:\Users\me\.cargo\bin\app.exe.old-20260916T101010"),
            installed
        ));
        // A different image, and the same name in another directory, are not it.
        assert!(!same_binary_or_rotated(
            Path::new(r"C:\Users\me\.cargo\bin\other.exe"),
            installed
        ));
        assert!(!same_binary_or_rotated(
            Path::new(r"C:\tmp\app.exe.old"),
            installed
        ));
        assert!(!same_binary_or_rotated(
            Path::new(r"C:\Users\me\.cargo\bin\appx.exe.old"),
            installed
        ));
    }
}
