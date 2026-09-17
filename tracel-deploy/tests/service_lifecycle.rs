#![cfg(feature = "service")]
//! 14a: process and state handling of the optional `service` module.
//!
//! Covers the stop idempotency, the detached start (pid + log file), both
//! guardrails (image path before stop, probe/scan before start), the atomic
//! state file including corrupt content, and the platform specific detachment
//! (no console on Windows, own session on Unix).

#[path = "service_support/mod.rs"]
mod support;

use std::{fs, path::PathBuf, time::Duration};

use support::{Fixture, LOG_MARKER, SLEEP_HELPER, STDIO_HELPER, helper_args};
use tracel_deploy::service::{self, Instance, ProbeSpec, ServiceState, StopOutcome};

/// Helper: writes to stdout and stderr, then exits.
#[test]
#[ignore]
fn stdio_helper_for_service_tests() {
    support::helper_stdio();
}

/// Helper: stays alive long enough for a stop.
#[test]
#[ignore]
fn sleep_helper_for_service_tests() {
    support::helper_sleep();
}

/// Helper: reports whether Windows gave it a console.
#[cfg(windows)]
#[test]
#[ignore]
fn console_helper_for_service_tests() {
    support::helper_console();
}

/// Helper: reports the session it runs in.
#[cfg(unix)]
#[test]
#[ignore]
fn session_helper_for_service_tests() {
    support::helper_session();
}

fn state_temporary(path: &std::path::Path) -> PathBuf {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    PathBuf::from(temporary)
}

#[test]
fn state_file_round_trips_atomically_and_tolerates_corruption() {
    let fixture = Fixture::new("state");
    let path = fixture.spec::<[&str; 0], &str>([]).state_file();

    assert!(
        ServiceState::read(&path)
            .expect("read a missing state file")
            .is_none(),
        "a missing state file is no state"
    );

    let state = ServiceState {
        name: "demo".to_owned(),
        pid: 4242,
        binary: fixture.image().to_path_buf(),
        args: vec![
            "--path".to_owned(),
            r"C:\Program Files\a|b".to_owned(),
            "line\nbreak".to_owned(),
        ],
        log_file: Some(fixture.log_file()),
        listen: Some("tcp://127.0.0.1:8791".to_owned()),
        started_at: 1_700_000_000,
    };
    state.write(&path).expect("write the state file");

    assert!(
        !state_temporary(&path).exists(),
        "the temporary sibling is renamed away"
    );
    assert_eq!(
        ServiceState::read(&path)
            .expect("read the state file")
            .expect("state file present"),
        state,
        "arguments, backslashes and pipes survive the round trip"
    );

    let updated = ServiceState {
        pid: 4343,
        ..state.clone()
    };
    updated.write(&path).expect("replace the state file");
    assert_eq!(
        ServiceState::read(&path)
            .expect("read the replaced state file")
            .expect("state file present"),
        updated
    );

    // Unknown keys are ignored so a newer writer stays readable.
    fs::write(
        &path,
        "schema=1\nname=older\npid=7\nbinary=C:\\\\bin\\\\older.exe\nfuture=ignored\n",
    )
    .expect("write a forward compatible state file");
    let older = ServiceState::read(&path)
        .expect("read the forward compatible state file")
        .expect("state file present");
    assert_eq!(older.pid, 7);
    assert_eq!(older.binary, PathBuf::from(r"C:\bin\older.exe"));
    assert!(older.args.is_empty());
    assert_eq!(older.started_at, 0);

    for corrupt in [
        "",
        "not a state file at all\n",
        "schema=1\nname=demo\npid=not-a-number\nbinary=somewhere\n",
        "schema=1\nname=demo\npid=1\n",
        "schema=1\nname=demo\nbinary=C:\\\\bin\\\\demo.exe\n",
        "schema=99\nname=demo\npid=1\nbinary=C:\\\\bin\\\\demo.exe\n",
        "schema=1\nname=demo\npid=1\nbinary=C:\\\\bin\\\\demo.exe\nargs=a\\q\n",
    ] {
        fs::write(&path, corrupt).expect("write corrupt content");
        assert!(
            ServiceState::read(&path)
                .expect("reading corrupt content must not fail")
                .is_none(),
            "corrupt content reads as no state: {corrupt:?}"
        );
    }
    assert!(
        !fixture.dir().join("boom").exists(),
        "sanity: the fixture directory is still the temporary one"
    );
}

#[test]
fn stop_is_idempotent_for_missing_processes() {
    let fixture = Fixture::new("stop-idempotent");

    let mut child = support::direct_command(fixture.image(), helper_args(STDIO_HELPER))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the short lived helper");
    let dead_pid = child.id();
    child.wait().expect("wait for the short lived helper");
    assert!(
        !service::is_alive(dead_pid).expect("is_alive on an exited pid"),
        "the helper exited"
    );

    service::stop(dead_pid, Duration::from_secs(2)).expect("stopping an exited pid is a success");
    service::stop(9_999_999, Duration::from_secs(2)).expect("stopping an unknown pid is a success");
    assert!(!service::is_alive(0).expect("pid 0 is never a process"));
    assert_eq!(
        service::executable_path(0).expect("the image of pid 0"),
        None
    );

    let stale = ServiceState {
        name: "stale".to_owned(),
        pid: dead_pid,
        binary: fixture.image().to_path_buf(),
        args: Vec::new(),
        log_file: None,
        listen: None,
        started_at: 0,
    };
    let report = service::stop_checked(&stale, Duration::from_secs(2)).expect("stale state file");
    assert_eq!(report.outcome, StopOutcome::NotRunning);
    assert_eq!(report.pid, Some(dead_pid));
}

#[test]
fn start_detached_records_the_pid_and_writes_the_log() {
    let mut fixture = Fixture::new("start-log");
    let spec = fixture.spec(helper_args(STDIO_HELPER));

    let state = fixture.start(&spec);
    assert_eq!(
        state.binary,
        fs::canonicalize(fixture.image()).expect("canonical fixture image")
    );

    let recorded = ServiceState::read(&spec.state_file())
        .expect("read the state file")
        .expect("start_detached writes the state file");
    assert_eq!(recorded.pid, state.pid);
    assert_eq!(recorded.name, spec.name);
    assert_eq!(recorded.args, spec.args);
    assert_eq!(recorded.log_file.as_deref(), Some(spec.log_file.as_path()));

    let text = fixture.wait_for_log(&spec.log_file, LOG_MARKER, Duration::from_secs(10));
    assert!(
        text.contains(&format!("{LOG_MARKER} stdout")),
        "stdout lands in the log: {text}"
    );
    assert!(
        text.contains(&format!("{LOG_MARKER} stderr")),
        "stderr lands in the log: {text}"
    );
    assert!(
        fixture.wait_for_exit(state.pid, Duration::from_secs(10)),
        "holding no handle keeps the detached helper independent of us"
    );
}

#[test]
fn stop_terminates_a_started_service_and_stays_idempotent() {
    let mut fixture = Fixture::new("stop-live");
    let spec = fixture.spec(helper_args(SLEEP_HELPER));

    let state = fixture.start(&spec);
    assert!(
        fixture.wait_for_alive(state.pid, Duration::from_secs(10)),
        "the detached helper runs"
    );

    let image = service::executable_path(state.pid)
        .expect("read the image path")
        .expect("the image path of a process we started is readable");
    assert_eq!(
        fs::canonicalize(&image).expect("canonical image"),
        fs::canonicalize(fixture.image()).expect("canonical fixture image"),
        "the image guardrail compares against the recorded binary"
    );

    service::stop(state.pid, Duration::from_secs(10)).expect("stop the running service");
    assert!(
        fixture.wait_for_exit(state.pid, Duration::from_secs(10)),
        "stop waits for the process to exit"
    );
    service::stop(state.pid, Duration::from_secs(2)).expect("stopping again is a success");
}

#[test]
fn stop_checked_refuses_a_pid_whose_image_does_not_match() {
    let fixture = Fixture::new("guard-image");
    let state = ServiceState {
        name: "not-ours".to_owned(),
        pid: std::process::id(),
        binary: fixture.image().to_path_buf(),
        args: Vec::new(),
        log_file: None,
        listen: None,
        started_at: 0,
    };

    let error = service::stop_checked(&state, Duration::from_secs(2))
        .expect_err("a recycled pid must not be killed");
    let message = format!("{error:#}");
    assert!(message.contains("refusing to stop"), "error: {message}");
    assert!(
        message.contains(&std::process::id().to_string()),
        "error: {message}"
    );
    assert!(
        service::is_alive(std::process::id()).expect("our own process"),
        "the refused stop must not have signalled anything"
    );
}

#[test]
fn start_detached_refuses_a_second_instance_when_the_probe_answers() {
    let fixture = Fixture::new("guard-probe");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a listening socket");
    let addr = listener.local_addr().expect("listener address");
    let spec = fixture
        .spec::<[&str; 0], &str>([])
        .probe(ProbeSpec::tcp(addr));

    let error = service::start_detached(&spec).expect_err("a serving instance must be detected");
    let message = format!("{error:#}");
    assert!(message.contains("already running"), "error: {message}");
    assert!(
        message.contains("readiness probe already answers"),
        "error: {message}"
    );
    assert!(
        !spec.state_file().exists(),
        "a refused start writes no state file"
    );
    drop(listener);
}

#[test]
fn start_detached_refuses_a_second_instance_for_the_recorded_pid() {
    let mut fixture = Fixture::new("guard-state");
    let spec = fixture.spec(helper_args(SLEEP_HELPER));
    let state = fixture.start(&spec);

    assert_eq!(
        service::find_running_instance(&spec).expect("detect the instance"),
        Some(Instance::Recorded(state.pid))
    );
    let error =
        service::start_detached(&spec).expect_err("a recorded live pid must refuse a second start");
    let message = format!("{error:#}");
    assert!(message.contains("already running"), "error: {message}");
    assert!(
        message.contains("state file records live pid"),
        "error: {message}"
    );

    let report =
        service::stop_service(&spec, Duration::from_secs(10)).expect("stop the recorded instance");
    assert_eq!(report.outcome, StopOutcome::Terminated);
    assert_eq!(report.pid, Some(state.pid));
    assert!(
        !spec.state_file().exists(),
        "stop_service removes the state file it acted on"
    );
    assert!(fixture.wait_for_exit(state.pid, Duration::from_secs(10)));
}

#[test]
fn stop_service_finds_an_instance_that_wrote_no_state_file() {
    let mut fixture = Fixture::new("scan");
    let spec = fixture.spec(helper_args(SLEEP_HELPER));
    let state = fixture.start(&spec);

    ServiceState::remove(&spec.state_file());
    assert!(!spec.state_file().exists());
    assert_eq!(
        service::find_running_instance(&spec).expect("scan for the instance"),
        Some(Instance::Scanned(state.pid)),
        "the image path scan finds an instance that wrote no state file"
    );

    let report = service::stop_service(&spec, Duration::from_secs(10))
        .expect("stop the instance that has no state file");
    assert_eq!(report.outcome, StopOutcome::Terminated);
    assert_eq!(report.pid, Some(state.pid));
    assert!(fixture.wait_for_exit(state.pid, Duration::from_secs(10)));

    let again = service::stop_service(&spec, Duration::from_secs(5)).expect("stop is idempotent");
    assert_eq!(again.outcome, StopOutcome::NotRunning);
    assert_eq!(again.pid, None);
}

#[test]
fn resolve_binary_finds_path_entries_and_reports_missing_files() {
    let fixture = Fixture::new("resolve");
    assert_eq!(
        service::resolve_binary(fixture.image()).expect("an absolute path"),
        fs::canonicalize(fixture.image()).expect("canonical fixture image")
    );

    #[cfg(windows)]
    let name = "cmd";
    #[cfg(unix)]
    let name = "sh";
    let resolved = service::resolve_binary(std::path::Path::new(name))
        .unwrap_or_else(|error| panic!("{name} must resolve through PATH: {error:#}"));
    assert!(resolved.is_absolute(), "resolved: {}", resolved.display());
    assert!(resolved.is_file(), "resolved: {}", resolved.display());

    let error = service::resolve_binary(std::path::Path::new("tracel-deploy-definitely-missing"))
        .expect_err("a missing binary must be reported");
    assert!(
        format!("{error:#}").contains("executable not found"),
        "error: {error:#}"
    );
}

/// Windows: the detached child gets its own console without a window, which is
/// the observable difference to a plain `spawn` that inherits ours.
#[cfg(windows)]
#[test]
fn start_detached_leaves_the_windows_child_without_a_console() {
    let mut fixture = Fixture::new("console");
    let spec = fixture.spec(helper_args(support::CONSOLE_HELPER));
    let state = fixture.start(&spec);

    let text = fixture.wait_for_log(&spec.log_file, "console=", Duration::from_secs(10));
    assert!(
        text.contains("console=0"),
        "a detached child must have no console: {text}"
    );
    assert!(fixture.wait_for_exit(state.pid, Duration::from_secs(10)));

    if support::console::attached() {
        // Control: without the creation flags the child inherits our console, so
        // the assertion above really discriminates.
        let control_log = fixture.log_file_at("control.log");
        let stdout = fs::File::create(&control_log).expect("create the control log");
        let stderr = stdout.try_clone().expect("clone the control log");
        let status = support::direct_command(fixture.image(), helper_args(support::CONSOLE_HELPER))
            .stdout(stdout)
            .stderr(stderr)
            .status()
            .expect("run the control helper");
        assert!(status.success(), "control helper exit: {status}");
        let control = fixture.read_log(&control_log);
        assert!(
            control.contains("console=1"),
            "the control child inherits our console: {control}"
        );
    } else {
        println!("skipping the console control: this test process has no console either");
    }
}

/// Unix: the child gets its own session, so it survives the deploy process and
/// is not signalled with the caller's process group.
#[cfg(unix)]
#[test]
fn start_detached_gives_the_unix_child_its_own_session() {
    let mut fixture = Fixture::new("session");
    let spec = fixture.spec(helper_args(support::SESSION_HELPER));
    let state = fixture.start(&spec);

    let text = fixture.wait_for_log(&spec.log_file, "session=", Duration::from_secs(10));
    let field = |name: &str| -> i64 {
        text.lines()
            .find(|line| line.contains(name))
            .and_then(|line| {
                line.split_whitespace()
                    .find_map(|word| word.strip_prefix(name))
            })
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("{name} missing from: {text}"))
    };
    let pid = field("pid=");
    let session = field("session=");
    assert_eq!(
        session, pid,
        "setsid makes the child its own session leader: {text}"
    );
    assert!(fixture.wait_for_exit(state.pid, Duration::from_secs(10)));
}
