//! Queue a deployment into a detached child process.
//!
//! Codex runs a repository's `Stop` hook after every turn (see
//! [`codex_stop_hook_json`]), and that hook must not hold the turn while cargo
//! builds. The pattern this module implements:
//!
//! 1. The hook calls `xtask deploy --background`.
//! 2. That process does not deploy: [`child_argv`] swaps `--background` for
//!    `--child`, and [`queue_deploy`] re-spawns the same executable - detached,
//!    with that argv, and with a console that has no window, so neither the
//!    child nor the programs it starts (cargo, a plan's shell) can put one on
//!    the screen - redirecting stdout to `<state_dir>/deploy.log` and stderr to
//!    `<state_dir>/deploy-error.log`, and returns the child's pid at once.
//!    If a Windows hook job forbids breakaway, the queue starts the child
//!    through an independent WMI broker instead of retrying inside that job.
//! 3. The child runs the ordinary plan: cargo decides whether there is anything
//!    to build, the rotation is a no-op while the installed binary is already
//!    that build output, and a real change is rotated, verified, and stamped.
//!
//! [`DeployQueueArgs`] carries the two switches and [`child_argv`] builds the
//! child's command line, so a consumer's `deploy` command needs no argv
//! rewriting, log handling or detached spawn of its own:
//!
//! ```ignore
//! #[derive(clap::Args, Clone, Debug)]
//! pub struct DeployArgs {
//!     #[command(flatten)]
//!     queue: tracel_deploy::DeployQueueArgs,
//!     #[command(flatten)]
//!     common: tracel_deploy::DeployCmdArgs,
//! }
//!
//! fn deploy(args: DeployArgs) -> anyhow::Result<()> {
//!     let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask");
//!     let context = tracel_deploy::DeployContext::from_args(&args.common, root, "my-app")?;
//!     if args.queue.should_queue(args.common.dry_run) {
//!         let pid = tracel_deploy::queue_deploy(
//!             &context.state_dir,
//!             Some(root),
//!             tracel_deploy::child_argv(env::args_os().skip(1)),
//!         )?;
//!         println!("queued my-app redeploy (pid={pid})");
//!         return Ok(());
//!     }
//!     tracel_deploy::deploy(&context).run()?;
//!     Ok(())
//! }
//! ```
//!
//! A dry run is never queued - [`DeployQueueArgs::should_queue`] is false while
//! `--dry-run` is set - so the plan is rendered by the calling process and stays
//! in the output the caller can read.

use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context as _, Result};

#[cfg(windows)]
mod windows;

/// File a queued run writes its stdout to, inside the state directory.
pub const DEPLOY_LOG: &str = "deploy.log";
/// File a queued run writes its stderr to, inside the state directory.
pub const DEPLOY_ERROR_LOG: &str = "deploy-error.log";

/// The `--background` / `--child` switches a hook-driven deploy needs.
///
/// Flatten it next to [`crate::DeployCmdArgs`]:
///
/// ```ignore
/// #[derive(clap::Args, Clone, Debug)]
/// pub struct DeployArgs {
///     #[command(flatten)]
///     pub queue: tracel_deploy::DeployQueueArgs,
///     #[command(flatten)]
///     pub common: tracel_deploy::DeployCmdArgs,
/// }
/// ```
#[derive(Debug, Clone, Default, clap::Args)]
pub struct DeployQueueArgs {
    /// Queue the deployment into a detached child process and return at once.
    #[arg(long)]
    pub background: bool,
    /// Internal: this process is the child a `--background` deploy queued.
    #[arg(long, hide = true)]
    pub child: bool,
}

impl DeployQueueArgs {
    /// Whether this invocation queues a child instead of deploying here.
    ///
    /// True only for `--background` without `--child`: a dry run renders the
    /// plan in the calling process, never in a queued child.
    pub fn should_queue(&self, dry_run: bool) -> bool {
        self.background && !self.child && !dry_run
    }
}

/// The two files a queued run writes: stdout and stderr of the child.
#[derive(Debug)]
pub struct DeploymentLogs {
    /// `<state_dir>/deploy.log`, truncated by [`Self::open`].
    pub stdout: File,
    /// `<state_dir>/deploy-error.log`, truncated by [`Self::open`].
    pub stderr: File,
}

impl DeploymentLogs {
    /// Opens (and truncates) both logs, creating `state_dir` if needed.
    ///
    /// Each queued run replaces the previous log, so the newest deployment
    /// stays readable instead of growing a file per turn.
    pub fn open(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir).with_context(|| format!("create {}", state_dir.display()))?;
        Ok(Self {
            stdout: truncate(&state_dir.join(DEPLOY_LOG))?,
            stderr: truncate(&state_dir.join(DEPLOY_ERROR_LOG))?,
        })
    }
}

fn truncate(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

/// The queued child's command line: `argv` with `--background` swapped for
/// `--child`, so the child builds exactly the deployment that was asked for.
///
/// `--child` is appended when `--background` is absent, which keeps a hand
/// written command line working.
pub fn child_argv(argv: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = argv.into_iter().collect();
    match args.iter().position(|arg| arg == "--background") {
        Some(position) => args[position] = "--child".into(),
        None => args.push("--child".into()),
    }
    args
}

/// Queues `argv` as a detached child of the current executable and returns its
/// pid.
///
/// `argv` is passed through verbatim: the `--background` -> `--child` swap is
/// [`child_argv`]'s job. Queueing the caller's own arguments without it makes
/// the child see `--background` again and queue one more child forever.
///
/// `working_dir` is the directory the child runs in (a Cargo workspace root, so
/// the child finds the same workspace the hook was invoked from); `None` keeps
/// the caller's working directory.
///
/// The child does not inherit the caller's std handles, so a supervisor that
/// owns the queueing command's stdout/stderr (Codex's `Stop` hook runner) sees
/// them close as soon as the queueing command exits. Otherwise it would keep
/// the hook "running" for the whole deployment and kill it - and this child,
/// while it is still in the hook's job - at the hook's timeout.
pub fn queue_deploy(
    state_dir: &Path,
    working_dir: Option<&Path>,
    argv: impl IntoIterator<Item = OsString>,
) -> Result<u32> {
    let executable = env::current_exe().context("locate the current executable")?;
    queue_executable(&executable, state_dir, working_dir, argv)
}

/// [`queue_deploy`] with an explicit executable, for callers that queue
/// something other than themselves (and for tests). `argv` is passed through
/// verbatim, exactly like [`queue_deploy`].
///
/// On Windows, a job that rejects `CREATE_BREAKAWAY_FROM_JOB` sends the launch
/// through a WMI-created broker outside the caller's job. The broker keeps the
/// normal log files and reports the real deployment child's pid.
pub fn queue_executable(
    executable: &Path,
    state_dir: &Path,
    working_dir: Option<&Path>,
    argv: impl IntoIterator<Item = OsString>,
) -> Result<u32> {
    let argv: Vec<OsString> = argv.into_iter().collect();
    let logs = DeploymentLogs::open(state_dir)?;
    let mut command = Command::new(executable);
    command
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(logs.stdout)
        .stderr(logs.stderr);
    if let Some(working_dir) = working_dir {
        command.current_dir(working_dir);
    }
    #[cfg(windows)]
    if crate::detach::job_forbids_breakaway().context("probe Windows job breakaway support")? {
        drop(command);
        return windows::queue_outside_job(executable, state_dir, working_dir, &argv)
            .with_context(|| format!("queue {} outside the hook job", executable.display()));
    }
    match crate::detach::spawn_breakaway(&mut command) {
        Ok(child) => Ok(child.id()),
        Err(error) => Err(error).with_context(|| format!("queue {}", executable.display())),
    }
}

/// The command line a Codex `Stop` hook runs: `deploy --background` through
/// `cargo run --quiet`, so an edited xtask is rebuilt before it queues.
///
/// `xtask_manifest` is the manifest of the xtask crate (`<root>/xtask/Cargo.toml`).
/// A path containing whitespace is quoted.
///
/// A hook runner that owns a console (a terminal pane, `codex exec`) passes it
/// down and nothing appears; a runner with *no* console of its own makes
/// Windows allocate a new console for the first console program the hook
/// starts, and Windows puts a new console in a *visible* window - the `cargo`
/// window that can flash on every turn. Wrapping the command in a GUI-subsystem
/// host looked like the fix, but `conhost.exe --headless` was measured to
/// return before its payload finished and to drop the payload's stdio and exit
/// code, so the wrapper was reverted in 0.4.4. See
/// `.scratch/background-deploy/issues/04-conhost-wrapper-rejected.md`.
pub fn codex_stop_hook_command(xtask_manifest: &Path) -> String {
    let manifest = xtask_manifest.display().to_string();
    let manifest = if manifest.contains(char::is_whitespace) {
        format!("\"{manifest}\"")
    } else {
        manifest
    };
    format!("cargo run --quiet --manifest-path {manifest} -- deploy --background")
}

/// The Codex `Stop` hook entry that queues `command` after every turn, ready to
/// merge into the repository's `.codex/hooks.json` (or to be the whole file when
/// the repository has no other hook).
///
/// Three properties are deliberate:
///
/// - `async: true` keeps the turn from waiting for the command. Without it the
///   hook blocks the turn until the command returns.
/// - `timeout` is the *hook's* budget, not the deployment's: only the queue call
///   runs inside it, the build happens in the detached child.
/// - `Stop` (not `SessionEnd`) is the event that fires per turn; `SessionEnd`
///   only runs when a session actually ends.
///
/// Any edit to `.codex/hooks.json` invalidates the recorded hook trust hash, so
/// the hook has to be re-approved (Codex `/hooks`) after merging this entry, or
/// it is skipped silently.
pub fn codex_stop_hook_json(command: &str, status_message: &str, timeout_secs: u32) -> String {
    let command = json_escape(command);
    let status_message = json_escape(status_message);
    format!(
        r#"{{
  "hooks": {{
    "Stop": [
      {{
        "hooks": [
          {{
            "type": "command",
            "command": "{command}",
            "statusMessage": "{status_message}",
            "timeout": {timeout_secs},
            "async": true
          }}
        ]
      }}
    ]
  }}
}}
"#
    )
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            control if control < ' ' => {
                escaped.push_str(&format!("\\u{:04x}", control as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::*;

    /// The hook entry verified in judo's `.codex/hooks.json` (a real
    /// repository that runs this pattern): generated JSON has to stay
    /// byte-identical to it.
    const VERIFIED_STOP_HOOK: &str = r#"{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "cargo run --quiet --manifest-path D:\\rust\\judo\\xtask\\Cargo.toml -- deploy --background",
            "statusMessage": "Judo: queueing local redeploy",
            "timeout": 30,
            "async": true
          }
        ]
      }
    ]
  }
}
"#;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let dir = env::temp_dir().join(format!("tracel-deploy-{name}-{unique}"));
        fs::create_dir_all(&dir).expect("create the temp dir");
        dir
    }

    /// A command that echoes a marker line, Windows and Unix.
    fn echo_command(marker: &str) -> (PathBuf, Vec<OsString>) {
        #[cfg(windows)]
        {
            (
                PathBuf::from("cmd"),
                ["/c", "echo", marker].map(OsString::from).to_vec(),
            )
        }
        #[cfg(not(windows))]
        {
            (
                PathBuf::from("sh"),
                vec!["-c".into(), format!("echo {marker}").into()],
            )
        }
    }

    /// This test binary's name of the helper that stands in for the console
    /// program a queued child starts (`cargo build`, a plan's shell). The
    /// helpers are re-run through `--exact <name> --ignored`, so the name is
    /// the one the harness filters on, module path included.
    #[cfg(windows)]
    const CONSOLE_HELPER: &str = "background::tests::console_helper_for_background_tests";
    /// This test binary's name of the helper that is the queued child and
    /// starts [`CONSOLE_HELPER`].
    #[cfg(windows)]
    const CONSOLE_CHILD_HELPER: &str =
        "background::tests::console_child_helper_for_background_tests";
    /// This test binary's name of the helper that stands in for a `Stop` hook:
    /// it queues a long-lived child and exits at once. The helpers are re-run
    /// through `--exact <name> --ignored`, so the name is the one the harness
    /// filters on, module path included.
    #[cfg(windows)]
    const QUEUE_HELPER: &str = "background::tests::queue_and_exit_for_background_tests";
    /// This test binary's name of the queued child, which has to outlive the
    /// queueing helper by a wide margin.
    #[cfg(windows)]
    const PIPE_HOLDING_HELPER: &str = "background::tests::pipe_holding_child_for_background_tests";
    /// How long the queued child lives. Long enough that a child which wrongly
    /// held the helper's stdout cannot let the pipe close inside the deadline.
    #[cfg(windows)]
    const PIPE_HOLDING_SECS: u64 = 30;
    #[cfg(windows)]
    const RESTRICTIVE_JOB_QUEUE_HELPER: &str =
        "background::tests::queue_inside_restrictive_job_helper";
    #[cfg(windows)]
    const RESTRICTIVE_JOB_CHILD_HELPER: &str =
        "background::tests::child_inside_restrictive_job_helper";
    #[cfg(windows)]
    const RESTRICTIVE_JOB_RELEASE_ENV: &str = "TRACEL_DEPLOY_TEST_RESTRICTIVE_JOB_RELEASE";
    #[cfg(windows)]
    const RESTRICTIVE_JOB_STATE_ENV: &str = "TRACEL_DEPLOY_TEST_RESTRICTIVE_JOB_STATE";
    #[cfg(windows)]
    const RESTRICTIVE_JOB_MARKER: &str =
        r#"argument with spaces, "quotes", and C:\path with space\"#;

    /// Windows: what console this process runs with.
    #[cfg(windows)]
    mod console {
        use std::ffi::c_void;

        #[allow(non_snake_case)]
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetConsoleWindow() -> *mut c_void;
        }

        #[allow(non_snake_case)]
        #[link(name = "user32")]
        unsafe extern "system" {
            fn IsWindowVisible(hwnd: *mut c_void) -> i32;
        }

        /// `console=<0|1> visible=<0|1>`: whether this process has a console
        /// window, and whether that window is on screen.
        pub fn report() -> String {
            let hwnd = unsafe { GetConsoleWindow() };
            if hwnd.is_null() {
                return "console=0 visible=0".to_string();
            }
            let visible = unsafe { IsWindowVisible(hwnd) } != 0;
            format!("console=1 visible={}", u8::from(visible))
        }
    }

    #[cfg(windows)]
    mod job {
        use std::{ffi::c_void, io, os::windows::io::AsRawHandle as _, process::Child};

        const PROCESS_TERMINATE: u32 = 0x0001;
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const WAIT_TIMEOUT: u32 = 258;

        #[allow(non_snake_case)]
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
            fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
            fn IsProcessInJob(process: *mut c_void, job: *mut c_void, result: *mut i32) -> i32;
            fn TerminateJobObject(job: *mut c_void, code: u32) -> i32;
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
            fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
            fn CloseHandle(handle: *mut c_void) -> i32;
        }

        pub(super) struct Job(*mut c_void);

        impl Job {
            pub(super) fn create() -> io::Result<Self> {
                let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
                if handle.is_null() {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(Self(handle))
                }
            }

            pub(super) fn assign(&self, child: &Child) -> io::Result<()> {
                let assigned =
                    unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle().cast()) };
                if assigned == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }

            pub(super) fn contains(&self, pid: u32) -> io::Result<bool> {
                let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
                if process.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let mut result = 0;
                let queried = unsafe { IsProcessInJob(process, self.0, &mut result) };
                unsafe {
                    CloseHandle(process);
                }
                if queried == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(result != 0)
                }
            }

            pub(super) fn terminate(&self) -> io::Result<()> {
                if unsafe { TerminateJobObject(self.0, 1) } == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        }

        impl Drop for Job {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }

        pub(super) fn is_alive(pid: u32) -> bool {
            let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
            if process.is_null() {
                return false;
            }
            let alive = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
            unsafe {
                CloseHandle(process);
            }
            alive
        }

        pub(super) fn terminate_process(pid: u32) {
            let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
            if process.is_null() {
                return;
            }
            unsafe {
                TerminateProcess(process, 1);
                CloseHandle(process);
            }
        }
    }

    /// Windows helper: the console program started by the queued child.
    #[cfg(windows)]
    #[test]
    #[ignore = "helper for a_queued_child_never_puts_a_console_window_on_screen"]
    fn console_helper_for_background_tests() {
        println!("console-helper={}", console::report());
    }

    /// Windows helper: the queued child, which starts a console program the
    /// way a real plan starts cargo.
    #[cfg(windows)]
    #[test]
    #[ignore = "helper for a_queued_child_never_puts_a_console_window_on_screen"]
    fn console_child_helper_for_background_tests() {
        let executable = env::current_exe().expect("locate the test binary");
        let status = Command::new(executable)
            .args(["--exact", CONSOLE_HELPER, "--ignored", "--nocapture"])
            .status()
            .expect("start the console program");
        println!("console-child-status={}", u8::from(status.success()));
    }

    /// Windows helper: the queued child. It only has to stay alive far longer
    /// than the queueing helper, so a pipe it wrongly held could not close
    /// before the regression test's deadline.
    #[cfg(windows)]
    #[test]
    #[ignore = "helper for a_queued_child_does_not_hold_the_queueing_commands_stdio"]
    fn pipe_holding_child_for_background_tests() {
        std::thread::sleep(Duration::from_secs(PIPE_HOLDING_SECS));
    }

    /// Windows helper: the queueing command - `deploy --background` in a `Stop`
    /// hook. It queues the long-lived child and exits immediately; its stdout
    /// is a pipe owned by whoever started it.
    #[cfg(windows)]
    #[test]
    #[ignore = "helper for a_queued_child_does_not_hold_the_queueing_commands_stdio"]
    fn queue_and_exit_for_background_tests() {
        let dir = temp_dir("stdio-helper");
        let executable = env::current_exe().expect("locate the test binary");
        let argv = ["--exact", PIPE_HOLDING_HELPER, "--ignored"]
            .map(OsString::from)
            .to_vec();
        let pid = queue_executable(&executable, &dir, None, argv).expect("queue the child");
        println!("queued-pid={pid}");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "helper for a_queued_child_escapes_a_job_that_forbids_breakaway"]
    fn child_inside_restrictive_job_helper() {
        assert!(
            env::args_os().any(|argument| argument == RESTRICTIVE_JOB_MARKER),
            "the WMI broker must preserve an argument containing spaces, quotes, and backslashes"
        );
        println!("restrictive-job-child-console={}", console::report());
        std::thread::sleep(Duration::from_secs(3));
        println!("restrictive-job-child-survived");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "helper for a_queued_child_escapes_a_job_that_forbids_breakaway"]
    fn queue_inside_restrictive_job_helper() {
        let release = env::var_os(RESTRICTIVE_JOB_RELEASE_ENV).expect("release path");
        let state_dir = PathBuf::from(env::var_os(RESTRICTIVE_JOB_STATE_ENV).expect("state dir"));
        let deadline = Instant::now() + Duration::from_secs(20);
        while !Path::new(&release).exists() {
            assert!(
                Instant::now() < deadline,
                "the parent never released the restrictive-job helper"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let executable = env::current_exe().expect("locate the test binary");
        let argv = [
            "--exact",
            RESTRICTIVE_JOB_CHILD_HELPER,
            "--ignored",
            "--nocapture",
            "--",
            RESTRICTIVE_JOB_MARKER,
        ]
        .map(OsString::from)
        .to_vec();
        let pid = queue_executable(&executable, &state_dir, None, argv).expect("queue the child");
        println!("queued-pid={pid}");
    }

    fn wait_for_log(path: &Path, marker: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let content = fs::read_to_string(path).unwrap_or_default();
            if content.contains(marker) {
                return content;
            }
            assert!(
                Instant::now() < deadline,
                "{} never received {marker:?} (last content: {content:?})",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn only_a_background_parent_queues() {
        let queued = DeployQueueArgs {
            background: true,
            child: false,
        };
        assert!(queued.should_queue(false));
        assert!(!queued.should_queue(true), "a dry run renders here");
        assert!(
            !DeployQueueArgs {
                background: true,
                child: true,
            }
            .should_queue(false),
            "the queued child deploys instead of queueing again"
        );
        assert!(!DeployQueueArgs::default().should_queue(false));
    }

    #[test]
    fn the_queued_child_inherits_the_requested_flags() {
        let swapped = child_argv(["deploy", "--background", "--force"].map(OsString::from));
        assert_eq!(
            swapped,
            ["deploy", "--child", "--force"].map(OsString::from),
            "the child runs the same deployment, minus the queueing switch"
        );
        assert!(
            !swapped.contains(&OsString::from("--background")),
            "a child that keeps --background queues another child forever"
        );
        assert_eq!(
            child_argv(["deploy", "--install-to", r"C:\tmp\app.exe"].map(OsString::from)),
            ["deploy", "--install-to", r"C:\tmp\app.exe", "--child"].map(OsString::from)
        );
    }

    #[test]
    fn a_queued_run_truncates_the_previous_logs() {
        let dir = temp_dir("logs");
        fs::write(dir.join(DEPLOY_LOG), "previous stdout").expect("seed stdout");
        fs::write(dir.join(DEPLOY_ERROR_LOG), "previous stderr").expect("seed stderr");

        let logs = DeploymentLogs::open(&dir).expect("open the logs");
        assert_eq!(logs.stdout.metadata().expect("stdout metadata").len(), 0);
        assert_eq!(logs.stderr.metadata().expect("stderr metadata").len(), 0);
        drop(logs);

        let reopened = DeploymentLogs::open(&dir).expect("reopen the logs");
        assert_eq!(reopened.stdout.metadata().expect("metadata").len(), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_queued_child_writes_its_log() {
        let dir = temp_dir("child");
        let (program, args) = echo_command("queued by the detached child");

        let pid = queue_executable(&program, &dir, Some(&dir), args).expect("queue");
        assert!(pid > 0, "the child pid is the queue's handle");

        let log = wait_for_log(&dir.join(DEPLOY_LOG), "queued by the detached child");
        assert!(log.contains("queued by the detached child"), "{log:?}");
        fs::remove_dir_all(&dir).ok();
    }

    /// Windows: a queued child must not put a console window on the screen -
    /// neither for itself nor for the console programs it starts, which is
    /// what a `--background` deploy does when its plan builds with cargo or
    /// runs a shell. A child with no console at all (`DETACHED_PROCESS`) made
    /// the first of those programs allocate a new, *visible* console, so every
    /// deploy popped a window over the desktop.
    #[cfg(windows)]
    #[test]
    fn a_queued_child_never_puts_a_console_window_on_screen() {
        let dir = temp_dir("no-console-window");
        let executable = env::current_exe().expect("locate the test binary");
        let argv = ["--exact", CONSOLE_CHILD_HELPER, "--ignored", "--nocapture"]
            .map(OsString::from)
            .to_vec();

        let pid = queue_executable(&executable, &dir, None, argv).expect("queue the child");
        assert!(pid > 0, "the child pid is the queue's handle");

        // The status line is printed after the console program exited, so it
        // also proves the program itself ran.
        let log = wait_for_log(&dir.join(DEPLOY_LOG), "console-child-status=");
        assert!(
            log.contains("console-child-status=1"),
            "the queued child has to run the console program: {log}"
        );
        assert!(
            log.contains("console-helper=console=0 visible=0"),
            "the console program must not have a window: {log}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Windows: the queued child must not keep the queueing command's
    /// stdout/stderr open. A `Stop` hook runner owns those pipes and treats the
    /// hook as running until they close, so a child that inherited them was
    /// killed - with everything else still in the hook's job - when the hook's
    /// `timeout` fired, minutes into a real build, leaving `.deploy.lock`
    /// behind. Before the fix the EOF below arrives only after the child dies;
    /// the assertion fails long before that.
    #[cfg(windows)]
    #[test]
    fn a_queued_child_does_not_hold_the_queueing_commands_stdio() {
        let executable = env::current_exe().expect("locate the test binary");
        let mut helper = Command::new(executable)
            .args(["--exact", QUEUE_HELPER, "--ignored", "--nocapture"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the queueing helper");
        let mut stdout = helper.stdout.take().expect("the helper's stdout");

        let started = Instant::now();
        let mut output = Vec::new();
        stdout
            .read_to_end(&mut output)
            .expect("read the helper's stdout");
        let waited = started.elapsed();

        let output = String::from_utf8_lossy(&output);
        assert!(
            helper
                .wait()
                .expect("wait for the queueing helper")
                .success(),
            "the helper runs the queue path: {output}"
        );
        assert!(
            output.contains("queued-pid="),
            "the helper queued the long-lived child: {output}"
        );
        assert!(
            waited < Duration::from_secs(10),
            "the queued child must not hold the helper's stdout: EOF came only after \
             {waited:?} (the child lives {PIPE_HOLDING_SECS}s)"
        );
    }

    /// Windows: a restrictive outer job rejects `CREATE_BREAKAWAY_FROM_JOB`.
    /// The queued deployment must therefore be started by the independent WMI
    /// broker, not by retrying a plain spawn inside that job.
    #[cfg(windows)]
    #[test]
    fn a_queued_child_escapes_a_job_that_forbids_breakaway() {
        let dir = temp_dir("restrictive-job");
        let release = dir.join("release");
        let state_dir = dir.join("state");
        let executable = env::current_exe().expect("locate the test binary");
        let job = job::Job::create().expect("create restrictive job");
        let helper = Command::new(executable)
            .args([
                "--exact",
                RESTRICTIVE_JOB_QUEUE_HELPER,
                "--ignored",
                "--nocapture",
            ])
            .env(RESTRICTIVE_JOB_RELEASE_ENV, &release)
            .env(RESTRICTIVE_JOB_STATE_ENV, &state_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the restrictive-job helper");

        job.assign(&helper)
            .expect("assign the helper to the restrictive job");
        fs::write(&release, "go").expect("release the helper");
        let output = helper.wait_with_output().expect("wait for the helper");
        assert!(
            output.status.success(),
            "helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let pid = stdout
            .lines()
            .find_map(|line| line.strip_prefix("queued-pid="))
            .and_then(|pid| pid.trim().parse::<u32>().ok())
            .expect("the helper reports the queued pid");

        struct KillOnDrop {
            pid: u32,
            armed: bool,
        }
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                if self.armed {
                    job::terminate_process(self.pid);
                }
            }
        }
        let mut kill = KillOnDrop { pid, armed: true };
        assert!(job::is_alive(pid), "the queued child starts alive");
        assert!(
            !job.contains(pid).expect("query the restrictive job"),
            "the queued child must not remain in the hook's job"
        );

        job.terminate().expect("terminate the restrictive job");
        let log = wait_for_log(
            &state_dir.join(DEPLOY_LOG),
            "restrictive-job-child-survived",
        );
        assert!(log.contains("restrictive-job-child-survived"), "{log}");
        assert!(
            log.contains("restrictive-job-child-console=console=0 visible=0"),
            "the WMI broker must keep the deploy child windowless: {log}"
        );
        let cleanup_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let leftovers = fs::read_dir(&state_dir)
                .expect("read the queue state directory")
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    let name = entry.file_name();
                    name.to_string_lossy().ends_with(".ps1")
                        || name.to_string_lossy().ends_with(".pid")
                        || name.to_string_lossy().ends_with(".launch")
                })
                .count();
            if leftovers == 0 {
                break;
            }
            assert!(
                Instant::now() < cleanup_deadline,
                "the WMI broker left {leftovers} temporary queue files behind"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        kill.armed = false;
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_generated_hook_matches_the_verified_entry() {
        let command = codex_stop_hook_command(Path::new(r"D:\rust\judo\xtask\Cargo.toml"));
        assert_eq!(
            command,
            r"cargo run --quiet --manifest-path D:\rust\judo\xtask\Cargo.toml -- deploy --background"
        );
        assert_eq!(
            codex_stop_hook_json(&command, "Judo: queueing local redeploy", 30),
            VERIFIED_STOP_HOOK
        );
    }

    #[test]
    fn hook_commands_quote_paths_with_whitespace() {
        assert_eq!(
            codex_stop_hook_command(Path::new(r"C:\work my app\xtask\Cargo.toml")),
            "cargo run --quiet --manifest-path \"C:\\work my app\\xtask\\Cargo.toml\" -- deploy --background"
        );
        assert!(
            codex_stop_hook_json("a \"quoted\" \\ command", "status", 5)
                .contains(r#""command": "a \"quoted\" \\ command""#)
        );
    }
}
