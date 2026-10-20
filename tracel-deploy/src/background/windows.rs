use std::{
    env,
    ffi::{OsString, c_void},
    fs, io,
    io::Write as _,
    os::windows::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};

use super::{DEPLOY_ERROR_LOG, DEPLOY_LOG};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BROKER_PID_WAIT: Duration = Duration::from_secs(15);
const PROCESS_TERMINATE: u32 = 0x0001;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const ERROR_INVALID_PARAMETER: i32 = 87;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const JOB_OBJECT_TERMINATE: u32 = 0x0008;

const JOB_TYPE_SOURCE: &str = r#"
using System;
using System.Runtime.InteropServices;
public static class TracelDeployQueueJob {
  [StructLayout(LayoutKind.Sequential)]
  public struct BASIC_LIMIT_INFORMATION {
    public long PerProcessUserTimeLimit;
    public long PerJobUserTimeLimit;
    public uint LimitFlags;
    public UIntPtr MinimumWorkingSetSize;
    public UIntPtr MaximumWorkingSetSize;
    public uint ActiveProcessLimit;
    public UIntPtr Affinity;
    public uint PriorityClass;
    public uint SchedulingClass;
  }
  [StructLayout(LayoutKind.Sequential)]
  public struct IO_COUNTERS {
    public ulong ReadOperationCount;
    public ulong WriteOperationCount;
    public ulong OtherOperationCount;
    public ulong ReadTransferCount;
    public ulong WriteTransferCount;
    public ulong OtherTransferCount;
  }
  [StructLayout(LayoutKind.Sequential)]
  public struct EXTENDED_LIMIT_INFORMATION {
    public BASIC_LIMIT_INFORMATION BasicLimitInformation;
    public IO_COUNTERS IoInfo;
    public UIntPtr ProcessMemoryLimit;
    public UIntPtr JobMemoryLimit;
    public UIntPtr PeakProcessMemoryUsed;
    public UIntPtr PeakJobMemoryUsed;
  }
  [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  public static extern IntPtr CreateJobObject(IntPtr attributes, string name);
  [DllImport("kernel32.dll", SetLastError = true)]
  public static extern bool SetInformationJobObject(IntPtr job, uint infoClass, ref EXTENDED_LIMIT_INFORMATION info, uint length);
  [DllImport("kernel32.dll", SetLastError = true)]
  public static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
  [DllImport("kernel32.dll")]
  public static extern IntPtr GetCurrentProcess();
}
"#;

struct BrokerProcess(*mut c_void);

impl BrokerProcess {
    fn open(pid: u32) -> io::Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn is_exited(&self) -> bool {
        (unsafe { WaitForSingleObject(self.0, 0) }) == WAIT_OBJECT_0
    }

    fn wait_exit(&self, timeout_ms: u32) -> io::Result<()> {
        match unsafe { WaitForSingleObject(self.0, timeout_ms) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "WMI broker tree did not exit in time",
            )),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn terminate_exact(&self) -> io::Result<()> {
        if self.is_exited() {
            return Ok(());
        }
        if unsafe { TerminateProcess(self.0, 1) } == 0 && !self.is_exited() {
            return Err(io::Error::last_os_error());
        }
        self.wait_exit(5_000)
    }
}

impl Drop for BrokerProcess {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[allow(non_snake_case)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
    fn OpenJobObjectW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn TerminateJobObject(job: *mut c_void, code: u32) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// Starts a queued command through WMI when the enclosing job forbids
/// `CREATE_BREAKAWAY_FROM_JOB`.
///
/// WMI creates the broker outside the caller's job. The broker then starts the
/// real command with a windowless console, forwards its stdout/stderr to the
/// same deployment logs, and writes the real child's pid for this function to
/// return.
pub(super) fn queue_outside_job(
    executable: &Path,
    state_dir: &Path,
    working_dir: Option<&Path>,
    argv: &[OsString],
) -> Result<u32> {
    let powershell = powershell_path()?;
    let current_dir = env::current_dir().context("read the current directory for WMI")?;
    let state_dir = absolute_path(&current_dir, state_dir);
    let working_dir = match working_dir {
        Some(path) => absolute_path(&current_dir, path),
        None => current_dir.clone(),
    };
    let executable = if executable
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        absolute_path(&current_dir, executable)
    } else {
        executable.to_path_buf()
    };
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stem = format!("queue-{}-{nonce}", std::process::id());
    let job_name = format!(r"Local\TracelDeployQueue-{stem}");
    let client_script = state_dir.join(format!("{stem}.client.ps1"));
    let broker_script = state_dir.join(format!("{stem}.broker.ps1"));
    let launch_file = state_dir.join(format!("{stem}.launch"));
    let pid_file = state_dir.join(format!("{stem}.pid"));
    let stdout_path = state_dir.join(DEPLOY_LOG);
    let stderr_path = state_dir.join(DEPLOY_ERROR_LOG);

    fs::write(&launch_file, "go").with_context(|| format!("write {}", launch_file.display()))?;
    if let Err(error) = fs::write(
        &client_script,
        render_client_script(&powershell, &broker_script, &state_dir),
    )
    .with_context(|| format!("write {}", client_script.display()))
    {
        cleanup_file(&launch_file, &stderr_path, "WMI launch token");
        return Err(error);
    }
    let broker_contents = render_broker_script(
        &executable,
        &working_dir,
        argv,
        &BrokerPaths {
            job_name: &job_name,
            broker_script: &broker_script,
            launch: &launch_file,
            pid: &pid_file,
            stdout: &stdout_path,
            stderr: &stderr_path,
        },
    );
    if let Err(error) = fs::write(&broker_script, broker_contents)
        .with_context(|| format!("write {}", broker_script.display()))
    {
        cleanup_file(&client_script, &stderr_path, "WMI client script");
        cleanup_file(&launch_file, &stderr_path, "WMI launch token");
        return Err(error);
    }

    let client_output = Command::new(&powershell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&client_script)
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    cleanup_file(&client_script, &stderr_path, "WMI client script");
    let client_output = match client_output {
        Ok(output) => output,
        Err(error) => {
            cleanup_file(&broker_script, &stderr_path, "WMI broker script");
            cleanup_file(&launch_file, &stderr_path, "WMI launch token");
            return Err(error).context("start the Windows WMI queue client");
        }
    };
    if !client_output.status.success() {
        cleanup_file(&broker_script, &stderr_path, "WMI broker script");
        cleanup_file(&launch_file, &stderr_path, "WMI launch token");
        let stderr = String::from_utf8_lossy(&client_output.stderr);
        let detail = error_log_tail(&stderr_path);
        bail!(
            "Windows WMI could not start an independent deploy broker: {}{detail}",
            stderr.trim()
        );
    }
    let broker_pid = String::from_utf8_lossy(&client_output.stdout)
        .trim()
        .parse::<u32>()
        .context("parse the WMI broker pid")
        .inspect_err(|_| {
            cleanup_file(&broker_script, &stderr_path, "WMI broker script");
            cleanup_file(&launch_file, &stderr_path, "WMI launch token");
        })?;
    let broker_process = BrokerProcess::open(broker_pid).ok();

    let child_pid = wait_and_cleanup(&BrokerStartup {
        pid_file: &pid_file,
        broker_script: &broker_script,
        launch_file: &launch_file,
        job_name: &job_name,
        stderr: &stderr_path,
        broker_pid,
        broker_process: broker_process.as_ref(),
        timeout: BROKER_PID_WAIT,
    })?;
    Ok(child_pid)
}

fn absolute_path(current_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    }
}

fn powershell_path() -> Result<PathBuf> {
    let root = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let powershell = root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    if powershell.is_file() {
        Ok(powershell)
    } else {
        bail!(
            "Windows PowerShell was not found at {}",
            powershell.display()
        )
    }
}

fn render_client_script(powershell: &Path, broker_script: &Path, state_dir: &Path) -> String {
    let broker_command = format!(
        "\"{}\" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
        powershell.display(),
        broker_script.display()
    );
    format!(
        r#"$ErrorActionPreference = 'Stop'
try {{
  $startup = New-CimInstance -Namespace root/cimv2 -ClassName Win32_ProcessStartup -ClientOnly -Property @{{ ShowWindow = [uint16]0 }}
  $result = Invoke-CimMethod -Namespace root/cimv2 -ClassName Win32_Process -MethodName Create -Arguments @{{
    CommandLine = {}
    CurrentDirectory = {}
    ProcessStartupInformation = $startup
  }}
  if ($result.ReturnValue -ne 0) {{
    throw "Win32_Process.Create failed with code $($result.ReturnValue)"
  }}
  [Console]::Out.Write([string]$result.ProcessId)
}} catch {{
  [Console]::Error.Write($_.Exception.Message)
  exit 1
}}
"#,
        powershell_literal(&broker_command),
        powershell_literal(&state_dir.display().to_string()),
    )
}

fn job_bootstrap(job_name: &str) -> String {
    format!(
        r#"Add-Type -TypeDefinition {}
$jobName = {}
$job = [TracelDeployQueueJob]::CreateJobObject([IntPtr]::Zero, $jobName)
if ($job -eq [IntPtr]::Zero) {{ throw 'CreateJobObject failed' }}
$limits = New-Object TracelDeployQueueJob+EXTENDED_LIMIT_INFORMATION
$limits.BasicLimitInformation.LimitFlags = [uint32]0x2000
$size = [uint32][System.Runtime.InteropServices.Marshal]::SizeOf($limits)
if (-not [TracelDeployQueueJob]::SetInformationJobObject($job, 9, [ref]$limits, $size)) {{
  throw 'SetInformationJobObject failed'
}}
if (-not [TracelDeployQueueJob]::AssignProcessToJobObject($job, [TracelDeployQueueJob]::GetCurrentProcess())) {{
  throw 'AssignProcessToJobObject failed'
}}
"#,
        powershell_literal(JOB_TYPE_SOURCE),
        powershell_literal(job_name),
    )
}

struct BrokerPaths<'a> {
    job_name: &'a str,
    broker_script: &'a Path,
    launch: &'a Path,
    pid: &'a Path,
    stdout: &'a Path,
    stderr: &'a Path,
}

fn render_broker_script(
    executable: &Path,
    working_dir: &Path,
    argv: &[OsString],
    paths: &BrokerPaths<'_>,
) -> String {
    let arguments = encode_windows_command_line(argv);
    let cleanup_message = format!(
        "queue broker cleanup failed: could not remove {}",
        paths.broker_script.display()
    );
    let pid_cleanup_message = format!(
        "queue broker cleanup failed: could not remove {}",
        paths.pid.display()
    );
    let launch_cleanup_message = format!(
        "queue broker cleanup failed: could not remove {}",
        paths.launch.display()
    );
    format!(
        r#"$ErrorActionPreference = 'Stop'
{}
$exitCode = 1
$stdout = $null
$stderr = $null
try {{
  $stdout = [System.IO.File]::Open({}, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::ReadWrite)
  $stderr = [System.IO.File]::Open({}, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::ReadWrite)
  $startInfo = New-Object System.Diagnostics.ProcessStartInfo
  $startInfo.FileName = {}
  $startInfo.Arguments = {}
  $startInfo.WorkingDirectory = {}
  $startInfo.UseShellExecute = $false
  $startInfo.CreateNoWindow = $true
  $startInfo.RedirectStandardInput = $true
  $startInfo.RedirectStandardOutput = $true
  $startInfo.RedirectStandardError = $true
  if (-not (Test-Path -LiteralPath {})) {{
    throw 'queue launch was cancelled'
  }}
  $process = [System.Diagnostics.Process]::Start($startInfo)
  if ($null -eq $process) {{
    throw 'Process.Start returned no process'
  }}
  if (-not (Test-Path -LiteralPath {})) {{
    $process.Kill()
    throw 'queue launch was cancelled after process start'
  }}
  $process.StandardInput.Close()
  $stdoutTask = $process.StandardOutput.BaseStream.CopyToAsync($stdout)
  $stderrTask = $process.StandardError.BaseStream.CopyToAsync($stderr)
  [System.IO.File]::WriteAllText({}, [string]$process.Id)
  $process.WaitForExit()
  [System.Threading.Tasks.Task]::WaitAll([System.Threading.Tasks.Task[]]@($stdoutTask, $stderrTask))
  $exitCode = $process.ExitCode
}} catch {{
  try {{
    [System.IO.File]::AppendAllText({}, "queue broker failed: $($_.Exception.ToString())`r`n")
  }} catch {{}}
}} finally {{
  if ($null -ne $stdout) {{ $stdout.Dispose() }}
  if ($null -ne $stderr) {{ $stderr.Dispose() }}
  function Remove-QueueFile([string]$Path) {{
    if (-not (Test-Path -LiteralPath $Path)) {{
      return $true
    }}
    for ($attempt = 0; $attempt -lt 5; $attempt++) {{
      try {{
        Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
        return $true
      }} catch {{
        Start-Sleep -Milliseconds 100
      }}
    }}
    return $false
  }}
  if (-not (Remove-QueueFile {})) {{
    try {{
      [System.IO.File]::AppendAllText({}, {})
    }} catch {{}}
  }}
  if (-not (Remove-QueueFile {})) {{
    try {{
      [System.IO.File]::AppendAllText({}, {})
    }} catch {{}}
  }}
  if (-not (Remove-QueueFile {})) {{
    try {{
      [System.IO.File]::AppendAllText({}, {})
    }} catch {{}}
  }}
}}
exit $exitCode
"#,
        job_bootstrap(paths.job_name),
        powershell_literal(&paths.stdout.display().to_string()),
        powershell_literal(&paths.stderr.display().to_string()),
        powershell_literal(&executable.display().to_string()),
        powershell_literal(&arguments),
        powershell_literal(&working_dir.display().to_string()),
        powershell_literal(&paths.launch.display().to_string()),
        powershell_literal(&paths.launch.display().to_string()),
        powershell_literal(&paths.pid.display().to_string()),
        powershell_literal(&paths.stderr.display().to_string()),
        powershell_literal(&paths.broker_script.display().to_string()),
        powershell_literal(&paths.stderr.display().to_string()),
        powershell_literal(&cleanup_message),
        powershell_literal(&paths.pid.display().to_string()),
        powershell_literal(&paths.stderr.display().to_string()),
        powershell_literal(&pid_cleanup_message),
        powershell_literal(&paths.launch.display().to_string()),
        powershell_literal(&paths.stderr.display().to_string()),
        powershell_literal(&launch_cleanup_message),
    )
}

struct BrokerStartup<'a> {
    pid_file: &'a Path,
    broker_script: &'a Path,
    launch_file: &'a Path,
    job_name: &'a str,
    stderr: &'a Path,
    broker_pid: u32,
    broker_process: Option<&'a BrokerProcess>,
    timeout: Duration,
}

fn wait_and_cleanup(startup: &BrokerStartup<'_>) -> Result<u32> {
    let child_pid = wait_for_child_pid(
        startup.pid_file,
        startup.stderr,
        startup.broker_pid,
        startup.timeout,
    );
    if child_pid.is_err() {
        let cancellation = remove_with_retry(startup.launch_file);
        let termination =
            terminate_broker_job(startup.job_name, startup.broker_pid, startup.broker_process);
        if let Err(error) = &cancellation {
            append_error(
                startup.stderr,
                &format!("could not remove timed-out WMI launch token: {error}\n"),
            );
        }
        if let Err(error) = &termination {
            append_error(
                startup.stderr,
                &format!(
                    "could not terminate timed-out WMI broker {}: {error}\n",
                    startup.broker_pid
                ),
            );
        }
        cleanup_file(startup.pid_file, startup.stderr, "WMI child pid file");
        cleanup_file(startup.broker_script, startup.stderr, "WMI broker script");
        if termination.is_err() {
            let original = child_pid.expect_err("the child pid is absent");
            let cancellation = match cancellation {
                Ok(()) => "launch token removed".to_owned(),
                Err(error) => format!("launch token removal also failed: {error}"),
            };
            bail!(
                "timed-out WMI broker {} could not be terminated; {cancellation}; automatic \
                 retry must wait for external verification: {original:#}",
                startup.broker_pid
            );
        }
    } else {
        cleanup_file(startup.pid_file, startup.stderr, "WMI child pid file");
        cleanup_file(startup.launch_file, startup.stderr, "WMI launch token");
    }
    child_pid
}

fn terminate_broker_job(
    job_name: &str,
    broker_pid: u32,
    broker: Option<&BrokerProcess>,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    let name = std::ffi::OsStr::new(job_name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let job = unsafe { OpenJobObjectW(JOB_OBJECT_TERMINATE, 0, name.as_ptr()) };
    if !job.is_null() {
        let terminated = unsafe { TerminateJobObject(job, 1) };
        unsafe {
            CloseHandle(job);
        }
        if terminated == 0 {
            return Err(io::Error::last_os_error());
        }
        if let Some(broker) = broker {
            broker.wait_exit(5_000)?;
        }
        return Ok(());
    }

    let open_error = io::Error::last_os_error();
    if open_error.raw_os_error() != Some(ERROR_FILE_NOT_FOUND) {
        return Err(open_error);
    }
    terminate_broker_exact(broker_pid, broker)
}

fn terminate_broker_exact(pid: u32, broker: Option<&BrokerProcess>) -> io::Result<()> {
    if let Some(broker) = broker {
        return broker.terminate_exact();
    }
    match BrokerProcess::open(pid) {
        Ok(broker) => broker.terminate_exact(),
        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER) => Ok(()),
        Err(error) => Err(error),
    }
}

fn wait_for_child_pid(
    path: &Path,
    stderr_path: &Path,
    broker_pid: u32,
    timeout: Duration,
) -> Result<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read_to_string(path)
            && let Ok(pid) = contents.trim().parse::<u32>()
        {
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            let detail = error_log_tail(stderr_path);
            bail!(
                "the WMI broker {broker_pid} did not report the deploy child pid within {}s{detail}",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn error_log_tail(path: &Path) -> String {
    let Ok(contents) = fs::read_to_string(path) else {
        return String::new();
    };
    let mut lines = contents.lines().rev().take(8).collect::<Vec<_>>();
    lines.reverse();
    let tail = lines.join("\n");
    if tail.is_empty() {
        String::new()
    } else {
        format!("\n{tail}")
    }
}

fn cleanup_file(path: &Path, stderr_path: &Path, label: &str) {
    if let Err(error) = remove_with_retry(path) {
        append_error(
            stderr_path,
            &format!(
                "could not remove stale {label} {}: {error}\n",
                path.display()
            ),
        );
    }
}

fn remove_with_retry(path: &Path) -> std::io::Result<()> {
    for attempt in 0..5 {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if attempt == 4 => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
    Ok(())
}

fn append_error(path: &Path, message: &str) {
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(message.as_bytes());
    }
}

fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn encode_windows_command_line(argv: &[OsString]) -> String {
    argv.iter()
        .map(|arg| quote_windows_argument(&arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_windows_argument(argument: &str) -> String {
    if argument.is_empty() {
        return "\"\"".to_owned();
    }
    if !argument
        .chars()
        .any(|character| character.is_whitespace() || character == '"')
    {
        return argument.to_owned();
    }

    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "helper for a_broker_timeout_terminates_the_broker_before_cleaning_sidecars"]
    fn delayed_launch_helper() {
        let started =
            env::var_os("TRACEL_DEPLOY_TEST_BROKER_CHILD_STARTED").expect("child-started path");
        fs::write(started, "started").expect("write child-started marker");
        thread::sleep(Duration::from_millis(1_500));
        let marker = env::var_os("TRACEL_DEPLOY_TEST_BROKER_MARKER").expect("marker path");
        fs::write(marker, "launched").expect("write marker");
    }

    #[test]
    #[ignore = "helper for exact_broker_fallback_can_terminate_a_live_process"]
    fn long_running_process_helper() {
        thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn exact_broker_fallback_can_terminate_a_live_process() {
        let executable = env::current_exe().expect("current test executable");
        let mut child = Command::new(executable)
            .args([
                "--exact",
                "background::windows::tests::long_running_process_helper",
                "--ignored",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start a live process");
        let broker = BrokerProcess::open(child.id()).expect("open exact process handle");

        broker
            .terminate_exact()
            .expect("PROCESS_TERMINATE is part of the broker handle access");
        child.wait().expect("wait for the terminated process");
    }

    #[test]
    fn a_broker_timeout_terminates_the_broker_before_cleaning_sidecars() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "tracel-deploy-broker-timeout-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let pid_file = dir.join("queue.pid");
        let broker_script = dir.join("queue.broker.ps1");
        let launch_file = dir.join("queue.launch");
        let stderr = dir.join(DEPLOY_ERROR_LOG);
        let child_started = dir.join("child-started.marker");
        let marker = dir.join("late-launch.marker");
        fs::write(&pid_file, "not a pid").expect("pid file");
        fs::write(&broker_script, "exit 1").expect("broker script");
        fs::write(&launch_file, "go").expect("launch token");
        let helper = env::current_exe().expect("current test executable");
        let job_name = format!(r"Local\TracelDeployQueue-test-timeout-{unique}");
        let broker_command = format!(
            "{}; & {} --exact background::windows::tests::delayed_launch_helper --ignored --nocapture",
            job_bootstrap(&job_name),
            powershell_literal(&helper.display().to_string())
        );
        let mut broker_child = Command::new(powershell_path().expect("Windows PowerShell"))
            .args(["-NoProfile", "-NonInteractive", "-Command", &broker_command])
            .env("TRACEL_DEPLOY_TEST_BROKER_CHILD_STARTED", &child_started)
            .env("TRACEL_DEPLOY_TEST_BROKER_MARKER", &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("start delayed broker");
        let broker_handle = BrokerProcess::open(broker_child.id()).expect("open broker handle");
        let started_deadline = Instant::now() + Duration::from_secs(30);
        while !child_started.exists() {
            assert!(
                broker_child
                    .try_wait()
                    .expect("poll delayed broker")
                    .is_none(),
                "the delayed broker exited before starting its child"
            );
            assert!(
                Instant::now() < started_deadline,
                "the delayed broker child did not start"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let error = wait_and_cleanup(&BrokerStartup {
            pid_file: &pid_file,
            broker_script: &broker_script,
            launch_file: &launch_file,
            job_name: &job_name,
            stderr: &stderr,
            broker_pid: broker_child.id(),
            broker_process: Some(&broker_handle),
            timeout: Duration::from_millis(100),
        })
        .expect_err("the broker never reports a child pid");
        assert!(format!("{error:#}").contains("did not report"));
        assert!(
            !launch_file.exists(),
            "the launch token cancels late startup"
        );
        assert!(!pid_file.exists(), "the pid sidecar is cleaned on timeout");
        assert!(
            !broker_script.exists(),
            "the broker script is cleaned on timeout"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if broker_child.try_wait().expect("poll broker").is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the timed-out broker must be terminated"
            );
            thread::sleep(Duration::from_millis(25));
        }
        thread::sleep(Duration::from_millis(1_600));
        assert!(
            !marker.exists(),
            "a timed-out broker must not launch the deployment later"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_termination_failure_is_a_distinct_non_retryable_error() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "tracel-deploy-broker-terminate-failure-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let pid_file = dir.join("queue.pid");
        let broker_script = dir.join("queue.broker.ps1");
        let launch_file = dir.join("queue.launch");
        let stderr = dir.join(DEPLOY_ERROR_LOG);
        let job_name = format!(r"Local\TracelDeployQueue-test-terminate-failure-{unique}");
        fs::write(&pid_file, "not a pid").expect("pid file");
        fs::write(&broker_script, "exit 1").expect("broker script");
        fs::write(&launch_file, "go").expect("launch token");
        let fake_broker = BrokerProcess(std::ptr::null_mut());

        let error = wait_and_cleanup(&BrokerStartup {
            pid_file: &pid_file,
            broker_script: &broker_script,
            launch_file: &launch_file,
            job_name: &job_name,
            stderr: &stderr,
            broker_pid: u32::MAX,
            broker_process: Some(&fake_broker),
            timeout: Duration::ZERO,
        })
        .expect_err("an un-terminated broker is not a retryable queue failure");
        let text = format!("{error:#}");
        assert!(
            text.contains("could not be terminated") && text.contains("automatic retry must wait"),
            "error: {text}"
        );
        assert!(!launch_file.exists(), "the launch is cancelled first");
        fs::remove_dir_all(dir).ok();
    }
}
