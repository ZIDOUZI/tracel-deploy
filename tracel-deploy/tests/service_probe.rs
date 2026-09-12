#![cfg(feature = "service")]
//! 14b: readiness probing and failure diagnostics of the optional `service`
//! module.
//!
//! Covers the TCP and the hand written HTTP probe (success and timeout), the log
//! tail helper, and the start-failure diagnostics that a deploy loop needs.

#[path = "service_support/mod.rs"]
mod support;

use std::{fs, time::Duration};

use support::{
    Fixture, LOG_MARKER, STDIO_HELPER, helper_args, spawn_http_server, unused_local_addr,
};
use tracel_deploy::service::{self, ProbeSpec, ServiceSpec};

/// Helper: writes to stdout/stderr and exits without listening.
#[test]
#[ignore]
fn stdio_helper_for_service_tests() {
    support::helper_stdio();
}

#[test]
fn wait_ready_succeeds_against_a_listening_tcp_socket() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a listening socket");
    let addr = listener.local_addr().expect("listener address");
    let probe = ProbeSpec::tcp(addr);

    let report = service::wait_ready(&probe, 3, Duration::from_millis(50), Duration::from_secs(5))
        .expect("a listening socket is ready");
    assert!(report.ok);
    assert_eq!(report.attempts, 1);
    assert_eq!(report.status, None);
    assert!(
        report.detail.contains(&format!("tcp://{addr}")),
        "detail: {}",
        report.detail
    );
    drop(listener);
}

#[test]
fn wait_ready_reports_a_timeout_with_the_attempt_count() {
    let addr = unused_local_addr();
    let probe = ProbeSpec::tcp(addr);

    let report = service::probe_once(&probe, Duration::from_millis(200));
    assert!(!report.ok, "nothing is listening on {addr}");
    assert_eq!(report.attempts, 1);

    let error = service::wait_ready(
        &probe,
        3,
        Duration::from_millis(20),
        Duration::from_millis(200),
    )
    .expect_err("a closed port must time out");
    let message = format!("{error:#}");
    assert!(
        message.contains("not ready after 3 attempt"),
        "error: {message}"
    );
    assert!(
        message.contains(&format!("tcp://{addr}")),
        "error: {message}"
    );
}

#[test]
fn http_probe_accepts_a_2xx_response() {
    let server = spawn_http_server("/healthz", 200);
    let probe = ProbeSpec::http(server.addr);

    let report = service::wait_ready(&probe, 3, Duration::from_millis(50), Duration::from_secs(5))
        .expect("HTTP 200 is ready");
    assert_eq!(report.status, Some(200));
    assert!(report.ok);

    let requests = server.finish();
    assert!(
        requests.iter().any(|line| line == "GET /healthz HTTP/1.1"),
        "the hand written request line reached the server: {requests:?}"
    );
}

#[test]
fn http_probe_rejects_a_5xx_response_and_a_wrong_path() {
    let server = spawn_http_server("/healthz", 503);
    let probe = ProbeSpec::http(server.addr);

    let error = service::wait_ready(&probe, 2, Duration::from_millis(20), Duration::from_secs(5))
        .expect_err("HTTP 503 is not ready");
    assert!(
        format!("{error:#}").contains("HTTP 503"),
        "error: {error:#}"
    );
    assert!(
        format!("{error:#}").contains("not ready after 2 attempt"),
        "the probe retried: {error:#}"
    );
    let requests = server.finish();
    assert!(
        requests.iter().all(|line| line == "GET /healthz HTTP/1.1"),
        "requests: {requests:?}"
    );

    // The server answers 400 for any other path, so the request line is checked
    // rather than assumed.
    let server = spawn_http_server("/healthz", 200);
    let report = service::probe_once(
        &ProbeSpec::http(server.addr).path("/other"),
        Duration::from_secs(1),
    );
    assert!(!report.ok, "detail: {}", report.detail);
    assert!(
        report.detail.contains("HTTP 400"),
        "detail: {}",
        report.detail
    );
    server.finish();
}

#[test]
fn http_probe_reports_a_path_that_is_not_absolute() {
    let addr = unused_local_addr();
    let report = service::probe_once(
        &ProbeSpec::http(addr).path("healthz"),
        Duration::from_millis(200),
    );
    assert!(!report.ok);
    assert!(
        report.detail.contains("must start with '/'"),
        "detail: {}",
        report.detail
    );
}

#[test]
fn tail_log_returns_the_last_lines_and_treats_a_missing_file_as_empty() {
    let fixture = Fixture::new("tail");
    assert_eq!(
        service::tail_log(&fixture.log_file(), 5).expect("a missing log is empty"),
        ""
    );

    let path = fixture.log_file_at("lines.log");
    fs::create_dir_all(path.parent().expect("log directory")).expect("create the log directory");
    let contents: Vec<String> = (1..=30).map(|number| format!("line-{number}")).collect();
    fs::write(&path, contents.join("\n") + "\n").expect("write the log");

    assert_eq!(
        service::tail_log(&path, 3).expect("tail"),
        "line-28\nline-29\nline-30"
    );
    assert_eq!(service::tail_log(&path, 0).expect("no lines"), "");
    assert_eq!(
        service::tail_log(&path, 100).expect("more lines than the file has"),
        contents.join("\n")
    );
}

#[test]
fn start_and_wait_ready_returns_the_log_tail_when_the_service_never_listens() {
    let fixture = Fixture::new("start-failure");
    let spec = fixture
        .spec(helper_args(STDIO_HELPER))
        .probe(ProbeSpec::tcp(unused_local_addr()));

    let error = service::start_and_wait_ready(
        &spec,
        3,
        Duration::from_millis(50),
        Duration::from_millis(300),
    )
    .expect_err("a helper that never listens must fail readiness");
    let message = format!("{error:#}");
    assert!(message.contains("did not become ready"), "error: {message}");
    assert!(
        message.contains("not ready after 3 attempt"),
        "error: {message}"
    );
    assert!(
        message.contains(LOG_MARKER),
        "the failure carries the log tail: {message}"
    );
    assert!(
        message.contains(&spec.log_file.display().to_string()),
        "the log tail names the file: {message}"
    );

    // No half started instance is left behind.
    let stale = service::stop_service(&spec, Duration::from_secs(5)).expect("stop is idempotent");
    assert_eq!(stale.outcome, service::StopOutcome::NotRunning);
}

#[test]
fn start_detached_reports_the_log_tail_when_the_binary_is_missing() {
    let fixture = Fixture::new("missing-binary");
    let log_file = fixture.log_file_at("previous.log");
    fs::create_dir_all(log_file.parent().expect("log directory"))
        .expect("create the log directory");
    fs::write(&log_file, "stale deployment log line\n").expect("seed the log");
    let spec = ServiceSpec::new(
        "tracel-deploy-service-test",
        fixture.dir().join("does-not-exist.exe"),
        &log_file,
        fixture.state_dir(),
    );

    let error = service::start_detached(&spec).expect_err("a missing binary must fail the start");
    let message = format!("{error:#}");
    assert!(message.contains("executable not found"), "error: {message}");
    assert!(
        message.contains("stale deployment log line"),
        "the failure carries the log tail: {message}"
    );
    assert!(
        !spec.state_file().exists(),
        "a failed start writes no state file"
    );
}

#[test]
fn start_and_wait_ready_requires_a_probe() {
    let fixture = Fixture::new("no-probe");
    let spec = fixture.spec::<[&str; 0], &str>([]);
    let error = service::start_and_wait_ready(
        &spec,
        1,
        Duration::from_millis(10),
        Duration::from_millis(100),
    )
    .expect_err("start_and_wait_ready needs a probe");
    assert!(
        format!("{error:#}").contains("has no readiness probe"),
        "error: {error:#}"
    );
    assert!(!spec.state_file().exists(), "nothing was started");
}
