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
#[cfg(windows)]
pub(crate) fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;

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
