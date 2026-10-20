//! Detached child processes, shared by the service helpers and the deploy
//! queue.

use std::io;
use std::process::{Child, Command};

/// Starts `command` outside the caller's process tree when the enclosing job
/// permits breakaway: no console window, its own console and process group, and
/// `CREATE_BREAKAWAY_FROM_JOB`. A job that forbids breakaway answers
/// `ERROR_ACCESS_DENIED`.
///
/// `CREATE_NO_WINDOW`, not `DETACHED_PROCESS`: a process with no console at
/// all makes the *first* console program it starts allocate one, and Windows
/// puts that new console in a *visible window*. A queued `--background` deploy
/// is such a process - it builds with cargo, and a plan can run any shell - so
/// every deploy popped a console window over the desktop. `CREATE_NO_WINDOW`
/// gives the child a console without a window instead, which the programs it
/// starts inherit, so no window can appear anywhere below it.
///
/// The child also does not inherit the caller's std handles
/// ([`StdioInheritGuard`]). Someone else usually owns them: a Codex `Stop` hook
/// runner owns the stdout/stderr pipes of the command it started and keeps that
/// command "running" until the pipes close. A detached child that inherited
/// them would hold them open for its whole life, so the runner would kill the
/// hook - and everything still in the hook's job - when the hook's timeout
/// fires: mid-build, with the deployment lock still on disk.
#[cfg(windows)]
pub(crate) fn spawn_breakaway(command: &mut Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let _stdio = StdioInheritGuard::clear();

    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    command.spawn()
}

/// Starts a detached child, retrying without `CREATE_BREAKAWAY_FROM_JOB` when
/// the enclosing job forbids it.
///
/// This compatibility fallback is appropriate for service processes. A deploy
/// queue uses [`spawn_breakaway`] directly and sends the rejected case to a
/// broker outside the job, because retrying in the job lets a hook timeout
/// terminate the build.
#[cfg(windows)]
#[cfg_attr(not(feature = "service"), allow(dead_code))]
pub(crate) fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    match spawn_breakaway(command) {
        Err(error) if breakaway_denied(&error) => {
            let _stdio = StdioInheritGuard::clear();
            command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
            command.spawn()
        }
        result => result,
    }
}

/// Whether Windows rejected a child because the enclosing job forbids
/// breakaway.
#[cfg(windows)]
pub(crate) fn breakaway_denied(error: &io::Error) -> bool {
    error.raw_os_error() == Some(5)
}

/// Probes whether this process is in a Windows job that rejects an explicit
/// breakaway. The probe uses a known console executable, so an access-denied
/// result from the caller's real command cannot be mistaken for a job limit.
#[cfg(windows)]
pub(crate) fn job_forbids_breakaway() -> io::Result<bool> {
    use std::{ffi::c_void, process::Stdio};

    const ERROR_ACCESS_DENIED: i32 = 5;

    #[allow(non_snake_case)]
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn IsProcessInJob(process: *mut c_void, job: *mut c_void, result: *mut i32) -> i32;
    }

    let mut in_job = 0;
    let queried = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
    if queried == 0 {
        return Err(io::Error::last_os_error());
    }
    if in_job == 0 {
        return Ok(false);
    }

    let system_cmd = std::path::PathBuf::from(
        std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
    )
    .join(r"System32\cmd.exe");
    let command_processor = if system_cmd.is_file() {
        system_cmd
    } else {
        std::env::var_os("ComSpec")
            .map(std::path::PathBuf::from)
            .unwrap_or(system_cmd)
    };
    let mut probe = Command::new(command_processor);
    probe
        .args(["/D", "/C", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match spawn_breakaway(&mut probe) {
        Ok(mut child) => {
            let _ = child.wait();
            Ok(false)
        }
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => Ok(true),
        Err(error) => Err(error),
    }
}

/// On other platforms a detached child is a plain spawn: it inherits no
/// controlling terminal and is not ended by the parent exiting.
#[cfg(not(windows))]
pub(crate) fn spawn_breakaway(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

#[cfg(not(windows))]
pub(crate) fn breakaway_denied(_error: &io::Error) -> bool {
    false
}

#[cfg(not(windows))]
#[cfg_attr(not(feature = "service"), allow(dead_code))]
pub(crate) fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

/// Clears `HANDLE_FLAG_INHERIT` from this process's std handles for one spawn
/// and restores the flags afterwards.
///
/// `CreateProcess` runs with `bInheritHandles = TRUE`, which copies *every*
/// handle flagged inheritable into the child - not only the handles that become
/// its stdio. The ones that matter here belong to somebody else: a `Stop` hook
/// runner owns the stdout/stderr pipes of the command it started and treats
/// that command as running until the pipes close. A detached child that
/// inherited them would keep them open for its whole life, so a hook with
/// `"timeout": 30` was killed, together with the deploy child still in its job,
/// 30 seconds into any real build. Clearing the flag keeps the pipes with their
/// owner; the child still gets its null stdin and its two log files, because
/// `std::process` makes exactly those handles inheritable for this one spawn.
///
/// The inherit flag is process-wide state, so this is only sound while no other
/// thread spawns a process: the deploy queue spawns from the thread that is
/// about to exit and restores the flags as soon as `spawn` returns.
#[cfg(windows)]
struct StdioInheritGuard {
    cleared: Vec<*mut core::ffi::c_void>,
}

#[cfg(windows)]
impl StdioInheritGuard {
    fn clear() -> Self {
        let mut cleared = Vec::new();
        for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = unsafe { GetStdHandle(kind) };
            if handle.is_null() || handle as isize == -1 {
                continue;
            }
            let mut flags = 0u32;
            let known = unsafe { GetHandleInformation(handle, &mut flags) } != 0;
            let inheritable = flags & HANDLE_FLAG_INHERIT != 0;
            let cleared_now =
                inheritable && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0;
            if known && cleared_now {
                cleared.push(handle);
            }
        }
        Self { cleared }
    }
}

#[cfg(windows)]
impl Drop for StdioInheritGuard {
    fn drop(&mut self) {
        for handle in self.cleared.drain(..) {
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
        }
    }
}

#[cfg(windows)]
const STD_INPUT_HANDLE: u32 = -10i32 as u32;
#[cfg(windows)]
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
#[cfg(windows)]
const STD_ERROR_HANDLE: u32 = -12i32 as u32;
#[cfg(windows)]
const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

#[cfg(windows)]
#[allow(non_snake_case)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(kind: u32) -> *mut core::ffi::c_void;
    fn GetHandleInformation(handle: *mut core::ffi::c_void, flags: *mut u32) -> i32;
    fn SetHandleInformation(handle: *mut core::ffi::c_void, mask: u32, flags: u32) -> i32;
}
