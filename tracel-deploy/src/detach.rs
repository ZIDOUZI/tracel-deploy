//! Detached child processes, shared by the service helpers and the deploy
//! queue.

use std::io;
use std::process::{Child, Command};

/// Starts `command` outside the caller's process tree: no console window, its
/// own console and process group, and - when the enclosing job allows it - a
/// breakaway, so a caller that is killed (a hook that hits its timeout, a
/// closing shell) cannot take the detached child down with it.
///
/// Windows: `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`, plus
/// `CREATE_BREAKAWAY_FROM_JOB`. A job that forbids breakaway answers
/// `ERROR_ACCESS_DENIED`; the retry then keeps the plain detachment.
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
pub(crate) fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;

    // Held across both spawn attempts, so the retry below is covered too.
    let _stdio = StdioInheritGuard::clear();

    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    match command.spawn() {
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
            command.spawn()
        }
        result => result,
    }
}

/// On other platforms a detached child is a plain spawn: it inherits no
/// controlling terminal and is not ended by the parent exiting.
#[cfg(not(windows))]
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
