//! Queue a deployment into a detached child process.
//!
//! Codex runs a repository's `Stop` hook after every turn (see
//! [`codex_stop_hook_json`]), and that hook must not hold the turn while cargo
//! builds. The pattern this module implements:
//!
//! 1. The hook calls `xtask deploy --background`.
//! 2. That process does not deploy: [`queue_deploy`] re-spawns the same
//!    executable detached, with `--background` swapped for `--child`, redirects
//!    stdout to `<state_dir>/deploy.log` and stderr to
//!    `<state_dir>/deploy-error.log`, and returns at once with the child's pid.
//! 3. The child runs the ordinary plan, so the deployment stamp skips it when
//!    nothing changed, the lock keeps two deployments of one binary apart, and a
//!    real change is built, rotated, verified, and stamped.
//!
//! [`DeployQueueArgs`] carries the two switches, so a consumer's `deploy`
//! command needs no argv rewriting, log handling or detached spawn of its own:
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
//!             env::args_os().skip(1),
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
    process::{Child, Command, Stdio},
};

use anyhow::{Context as _, Result};

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
/// `working_dir` is the directory the child runs in (a Cargo workspace root, so
/// the child finds the same workspace the hook was invoked from); `None` keeps
/// the caller's working directory.
pub fn queue_deploy(
    state_dir: &Path,
    working_dir: Option<&Path>,
    argv: impl IntoIterator<Item = OsString>,
) -> Result<u32> {
    let executable = env::current_exe().context("locate the current executable")?;
    queue_executable(&executable, state_dir, working_dir, argv)
}

/// [`queue_deploy`] with an explicit executable, for callers that queue
/// something other than themselves (and for tests).
pub fn queue_executable(
    executable: &Path,
    state_dir: &Path,
    working_dir: Option<&Path>,
    argv: impl IntoIterator<Item = OsString>,
) -> Result<u32> {
    let logs = DeploymentLogs::open(state_dir)?;
    let mut command = Command::new(executable);
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(logs.stdout)
        .stderr(logs.stderr);
    if let Some(working_dir) = working_dir {
        command.current_dir(working_dir);
    }
    let child: Child = crate::detach::spawn_detached(&mut command)
        .with_context(|| format!("queue {}", executable.display()))?;
    Ok(child.id())
}

/// The command line a Codex `Stop` hook runs: `deploy --background` through
/// `cargo run --quiet`, so an edited xtask is rebuilt before it queues.
///
/// `xtask_manifest` is the manifest of the xtask crate (`<root>/xtask/Cargo.toml`).
/// A path containing whitespace is quoted.
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
