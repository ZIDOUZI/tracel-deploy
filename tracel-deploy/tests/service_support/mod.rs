//! Shared harness for the `service` integration tests.
//!
//! Every test that starts a process uses its own copy of the test binary as the
//! service image, driven through `--exact <helper> --ignored`:
//!
//! - the copy has a unique path, so a scan of the fixture's image cannot match
//!   another test's process (or the test process itself),
//! - the helper is plain Rust, so the tests do not depend on shell built-ins and
//!   behave the same on Windows and Unix (the same trick
//!   `rotate::tests::replaces_a_running_binary` uses for a running binary),
//! - every started pid is stopped again in `Drop`, so no test leaves a process
//!   or a temporary directory behind.

// The two service test binaries use different subsets of this harness.
#![allow(dead_code)]

use std::{
    fs, io,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tracel_deploy::service::{self, ServiceSpec, ServiceState};

/// Harness name of the helper that writes to stdout/stderr and exits.
pub const STDIO_HELPER: &str = "stdio_helper_for_service_tests";
/// Harness name of the helper that stays alive.
pub const SLEEP_HELPER: &str = "sleep_helper_for_service_tests";
/// Harness name of the helper that reports whether it has a console (Windows).
#[cfg(windows)]
pub const CONSOLE_HELPER: &str = "console_helper_for_service_tests";
/// Harness name of the helper that reports its session (Unix).
#[cfg(unix)]
pub const SESSION_HELPER: &str = "session_helper_for_service_tests";

/// How long the sleep helper stays alive.
pub const SLEEP: Duration = Duration::from_secs(20);

/// Text both log markers contain, so a test can find the line it wants.
pub const LOG_MARKER: &str = "service-log-marker";

/// Arguments that run exactly one ignored helper test.
pub fn helper_args(helper: &'static str) -> [&'static str; 4] {
    ["--exact", helper, "--ignored", "--nocapture"]
}

/// A libtest option value that is inert (no test name matches it) but shows up
/// in the instance's command line, so tests can use `--skip <INSTANCE_MARKER>`
/// as the "started by the deploy loop" marker an `InstanceFilter` keys on.
pub const INSTANCE_MARKER: &str = "tracel-deploy-instance-marker";

/// [`helper_args`] plus the `--skip <INSTANCE_MARKER>` marker pair.
pub fn marked_helper_args(helper: &'static str) -> Vec<String> {
    let mut args: Vec<String> = helper_args(helper).into_iter().map(str::to_owned).collect();
    args.push("--skip".to_owned());
    args.push(INSTANCE_MARKER.to_owned());
    args
}

pub fn helper_stdio() {
    println!("{LOG_MARKER} stdout");
    eprintln!("{LOG_MARKER} stderr");
}

pub fn helper_sleep() {
    println!("{LOG_MARKER} sleeping");
    std::thread::sleep(SLEEP);
}

#[cfg(windows)]
pub fn helper_console() {
    println!("{LOG_MARKER} console={}", u8::from(console::attached()));
}

#[cfg(windows)]
pub mod console {
    use std::ffi::c_void;

    #[allow(non_snake_case)]
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleWindow() -> *mut c_void;
    }

    /// Whether the current process has a console window.
    pub fn attached() -> bool {
        !unsafe { GetConsoleWindow() }.is_null()
    }
}

#[cfg(unix)]
pub fn helper_session() {
    // SAFETY: both calls take no pointers and are async-signal-safe.
    let pid = unsafe { getpid() };
    let session = unsafe { getsid(0) };
    println!("{LOG_MARKER} pid={pid} session={session}");
}

#[cfg(unix)]
unsafe extern "C" {
    fn getpid() -> i32;
    fn getsid(pid: i32) -> i32;
}

/// Temporary directory, fixture image and process bookkeeping for one test.
pub struct Fixture {
    dir: PathBuf,
    image: PathBuf,
    started: Vec<u32>,
}

impl Fixture {
    pub fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "tracel-deploy-service-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create the service test directory");
        let image = dir.join(format!(
            "service-fixture-{label}{}",
            std::env::consts::EXE_SUFFIX
        ));
        fs::copy(
            std::env::current_exe().expect("current test binary"),
            &image,
        )
        .expect("copy the fixture image");
        Self {
            dir,
            image,
            started: Vec::new(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn image(&self) -> &Path {
        &self.image
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dir.join("state")
    }

    pub fn log_file(&self) -> PathBuf {
        self.log_file_at("service.log")
    }

    pub fn log_file_at(&self, name: &str) -> PathBuf {
        self.dir.join("logs").join(name)
    }

    /// A spec whose binary is this fixture's image.
    pub fn spec<I, S>(&self, args: I) -> ServiceSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        ServiceSpec::new(
            "tracel-deploy-service-test",
            self.image.clone(),
            self.log_file(),
            self.state_dir(),
        )
        .args(args)
    }

    pub fn track(&mut self, pid: u32) {
        self.started.push(pid);
    }

    /// Start through the library and remember the pid for cleanup.
    pub fn start(&mut self, spec: &ServiceSpec) -> ServiceState {
        let state = service::start_detached(spec).expect("start_detached");
        self.track(state.pid);
        state
    }

    /// Spawn the fixture image directly, without the library and without a state
    /// file, and wait until it is alive; `Drop` stops it again.
    pub fn spawn_unrecorded<I, S>(&mut self, args: I) -> u32
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut command = Command::new(&self.image);
        command
            .args(args.into_iter().map(Into::into))
            .stdin(std::process::Stdio::null());
        let child = command.spawn().expect("spawn the fixture instance");
        let pid = child.id();
        drop(child);
        self.track(pid);
        assert!(
            self.wait_for_alive(pid, Duration::from_secs(10)),
            "spawned fixture pid {pid} did not come up"
        );
        pid
    }

    pub fn wait_for_alive(&self, pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if service::is_alive(pid).expect("is_alive") {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn wait_for_exit(&self, pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !service::is_alive(pid).expect("is_alive") {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Wait until `needle` shows up in `path` and return the file contents.
    pub fn wait_for_log(&self, path: &Path, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let text = String::from_utf8_lossy(&fs::read(path).unwrap_or_default()).into_owned();
            if text.contains(needle) || Instant::now() >= deadline {
                return text;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn read_log(&self, path: &Path) -> String {
        String::from_utf8_lossy(&fs::read(path).unwrap_or_default()).into_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for pid in &self.started {
            let _ = service::stop(*pid, Duration::from_secs(10));
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A local address that nothing is listening on.
///
/// Port 1 is preferred because the OS never hands privileged ports out as
/// ephemeral ports, so a probe against it cannot accidentally reach a listener
/// another test (or another process) just created. On the unlikely machine that
/// serves port 1 anyway, a bound and released ephemeral port is used instead.
pub fn unused_local_addr() -> SocketAddr {
    let reserved = SocketAddr::from(([127, 0, 0, 1], 1));
    if std::net::TcpStream::connect_timeout(&reserved, Duration::from_millis(200)).is_err() {
        return reserved;
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an unused port");
    let addr = listener.local_addr().expect("unused port address");
    drop(listener);
    addr
}

/// A minimal HTTP server for the readiness tests: it answers `status` when the
/// request line asks for `path`, and 400 otherwise. It keeps accepting until the
/// test calls [`HttpServer::finish`], so a retrying probe can always reconnect.
pub struct HttpServer {
    pub addr: SocketAddr,
    stopping: Arc<AtomicBool>,
    requests: JoinHandle<Vec<String>>,
}

impl HttpServer {
    /// Stop accepting and return the request lines the server handled.
    pub fn finish(self) -> Vec<String> {
        self.stopping.store(true, Ordering::SeqCst);
        self.requests.join().expect("http server thread")
    }
}

pub fn spawn_http_server(path: &'static str, status: u16) -> HttpServer {
    const MAX_CONNECTIONS: usize = 64;
    const DEADLINE: Duration = Duration::from_secs(10);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the http server");
    let addr = listener.local_addr().expect("http server address");
    listener
        .set_nonblocking(true)
        .expect("non blocking http server");
    let stopping = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stopping);
    let requests = std::thread::spawn(move || {
        use std::{
            io::{Read as _, Write as _},
            net::Shutdown,
        };

        let deadline = Instant::now() + DEADLINE;
        let mut requests = Vec::new();
        while !stopped.load(Ordering::SeqCst)
            && requests.len() < MAX_CONNECTIONS
            && Instant::now() < deadline
        {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };
            // Windows lets an accepted socket inherit the non-blocking mode of
            // the listener, which would make the read below fail with
            // WSAEWOULDBLOCK before the request has arrived.
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let mut request = Vec::new();
            let mut chunk = [0_u8; 128];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        request.extend_from_slice(&chunk[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let text = String::from_utf8_lossy(&request).into_owned();
            let line = text.lines().next().unwrap_or_default().to_owned();
            let (code, reason) = if line.starts_with(&format!("GET {path} HTTP/1.1")) {
                (status, "Test Response")
            } else {
                (400, "Bad Request")
            };
            let response = format!(
                "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            // Close gracefully (FIN plus a drain), so a probing client can still
            // read the status line instead of seeing an aborted connection.
            let _ = stream.shutdown(Shutdown::Write);
            let mut sink = [0_u8; 128];
            while matches!(stream.read(&mut sink), Ok(read) if read > 0) {}
            requests.push(line);
        }
        requests
    });
    HttpServer {
        addr,
        stopping,
        requests,
    }
}

/// Spawn the fixture image directly (no detachment), for control runs.
pub fn direct_command(image: &Path, args: [&'static str; 4]) -> Command {
    let mut command = Command::new(image);
    command.args(args).stdin(std::process::Stdio::null());
    command
}
