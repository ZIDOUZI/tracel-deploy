use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};

/// How a running binary should be rotated out of the way.
///
/// The matrix below is the behaviour that is actually implemented, not a
/// restatement of the variant names: `Atomic` cannot be atomic on Windows,
/// and `Replace` only falls back to a rename-aside on Windows. A destination
/// that does not exist yet is simply copied, whatever the strategy.
///
/// | Strategy       | Windows                                            | Linux / macOS                     |
/// |----------------|----------------------------------------------------|-----------------------------------|
/// | `Auto`         | `Replace`, rename-aside when the file is running    | `Atomic`                          |
/// | `Replace`      | in-place copy, rename-aside on a running file       | in-place copy, fails when running |
/// | `Atomic`       | *degraded* to rename-aside (no atomic replacement)  | sibling temp file + atomic rename |
/// | `RenameAside`  | rename aside, then copy (rollback when copy fails)  | rename aside, then copy           |
/// | `FailIfLocked` | in-place copy, fail on a running or locked file     | in-place copy, fail when running  |
///
/// The Windows fallback only triggers for the running-binary error codes
/// (`ERROR_ACCESS_DENIED` 5, `ERROR_SHARING_VIOLATION` 32,
/// `ERROR_LOCK_VIOLATION` 33, `ERROR_USER_MAPPED_FILE` 1224).
///
/// [`RotateStrategy::describe`] renders the same information for the current
/// platform, so a dry-run plan can state which path a deployment takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RotateStrategy {
    /// Best platform-specific behaviour: atomic rename on Unix, copy with a
    /// rename-aside fallback on Windows.
    #[default]
    Auto,
    /// Try an in-place copy first.
    ///
    /// Windows: a running-binary error falls back to
    /// [`RotateStrategy::RenameAside`]. Linux/macOS: an in-place copy, which
    /// fails while the destination is being executed.
    Replace,
    /// Use a sibling rename on Unix; on Windows fall back to rename-aside.
    ///
    /// Windows has no atomic replace for a running image, so the strategy is
    /// *degraded* there to [`RotateStrategy::RenameAside`] for every
    /// destination, locked or not.
    Atomic,
    /// Rename the destination aside first, then copy the replacement.
    ///
    /// When the copy fails the rename is rolled back, and the returned error
    /// names the backup path, the rollback result, and the copy failure.
    RenameAside,
    /// Never move the destination aside: fail when it is in use.
    ///
    /// This is [`RotateStrategy::Replace`] without the rename-aside fallback,
    /// for conservative deployments that must not swap a running service. The
    /// in-use check is the Windows running-binary codes plus `ETXTBSY` on
    /// Unix.
    FailIfLocked,
}

impl RotateStrategy {
    /// Stable lowercase name, suitable for logs and dry-run plan output.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Replace => "replace",
            Self::Atomic => "atomic",
            Self::RenameAside => "rename-aside",
            Self::FailIfLocked => "fail-if-locked",
        }
    }

    /// What this strategy does on the *current* platform, including any
    /// degradation of the guarantee its name implies (Windows `Atomic`).
    ///
    /// Include this in dry-run output (`RotateStep`) so a plan never promises
    /// an atomic swap the platform cannot deliver.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Auto => {
                #[cfg(windows)]
                {
                    "auto: copy over the destination; rename the old binary aside when it is running"
                }
                #[cfg(not(windows))]
                {
                    "auto: stage a sibling copy and atomically rename it over the destination"
                }
            }
            Self::Replace => {
                #[cfg(windows)]
                {
                    "replace: copy over the destination; rename the old binary aside when it is running"
                }
                #[cfg(not(windows))]
                {
                    "replace: copy over the destination in place; fails when the destination is running"
                }
            }
            Self::Atomic => {
                #[cfg(windows)]
                {
                    "atomic: degraded on Windows to rename-aside: rename the old binary aside, then copy"
                }
                #[cfg(not(windows))]
                {
                    "atomic: stage a sibling copy and atomically rename it over the destination"
                }
            }
            Self::RenameAside => {
                "rename-aside: rename the old binary aside, then copy the replacement; undo the rename when the copy fails"
            }
            Self::FailIfLocked => {
                #[cfg(windows)]
                {
                    "fail-if-locked: copy over the destination only; fail on a running or locked destination instead of renaming it aside"
                }
                #[cfg(not(windows))]
                {
                    "fail-if-locked: copy over the destination only; fail on a running destination instead of renaming it aside"
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RotateOptions {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub backup_dir: Option<PathBuf>,
    pub strategy: RotateStrategy,
    pub dry_run: bool,
    /// Keep at most this many `<binary>.old-*` backups (newest first).
    pub backup_retention: Option<usize>,
    /// Remove `<binary>.old-*` backups older than this age.
    pub backup_max_age: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotateOutcome {
    WouldReplace {
        destination: PathBuf,
    },
    Replaced {
        destination: PathBuf,
    },
    ReplacedAfterRename {
        destination: PathBuf,
        backup: PathBuf,
    },
}

/// Replace `source` at `destination`, even when a process is still running the
/// old binary.
///
/// - Unix: copies to a sibling temporary file and atomically renames it over
///   the destination. The running process keeps the old inode.
/// - Windows: tries a normal copy; on a sharing violation, renames the old
///   executable aside and copies the replacement into place.
///
/// The per-platform behaviour of every [`RotateStrategy`] is documented on the
/// enum; [`RotateStrategy::FailIfLocked`] opts into the conservative variant
/// that fails instead of moving a running binary aside.
///
/// # Symlinks
///
/// A symlink `destination` is resolved with [`fs::canonicalize`] first: the
/// rotation replaces the **link target**, and the link itself survives, still
/// pointing at the same target path (see [`resolve_destination`]).
pub fn rotate(options: &RotateOptions) -> Result<RotateOutcome> {
    if !options.source.is_file() {
        anyhow::bail!("rotate source is not a file: {}", options.source.display());
    }
    let destination = resolve_destination(&options.destination)?;
    if options.dry_run {
        return Ok(RotateOutcome::WouldReplace { destination });
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    if !destination.exists() {
        copy_binary(&options.source, &destination).with_context(|| {
            format!(
                "failed to copy {} to {}",
                options.source.display(),
                destination.display()
            )
        })?;
        let outcome = RotateOutcome::Replaced { destination };
        cleanup_old_backups(options, &outcome);
        return Ok(outcome);
    }

    let outcome = match options.strategy {
        RotateStrategy::Replace => replace_in_place(options, &destination)?,
        RotateStrategy::Atomic => atomic_replace(options, &destination)?,
        RotateStrategy::RenameAside => rename_aside_replace(options, &destination)?,
        RotateStrategy::FailIfLocked => fail_if_locked_replace(options, &destination)?,
        RotateStrategy::Auto => {
            #[cfg(windows)]
            {
                replace_in_place(options, &destination)?
            }
            #[cfg(not(windows))]
            {
                atomic_replace(options, &destination)?
            }
        }
    };
    cleanup_old_backups(options, &outcome);
    Ok(outcome)
}

/// Best-effort removal of `<binary>.old-*` backups for the rotated destination.
///
/// Removal is intentionally best effort: on Windows a backup that a still
/// running process occupies cannot be deleted yet, and that must never fail
/// the rotation. Only files matching the destination's own backup prefix are
/// considered, so unrelated binaries are left alone.
fn cleanup_old_backups(options: &RotateOptions, outcome: &RotateOutcome) {
    let destination = match outcome {
        RotateOutcome::Replaced { destination }
        | RotateOutcome::ReplacedAfterRename { destination, .. } => destination,
        RotateOutcome::WouldReplace { .. } => return,
    };
    cleanup_backups_with(
        destination,
        options.backup_dir.as_deref(),
        options.backup_retention,
        options.backup_max_age,
        |path| fs::remove_file(path),
    );
}

fn cleanup_backups_with<F>(
    destination: &Path,
    backup_dir: Option<&Path>,
    backup_retention: Option<usize>,
    backup_max_age: Option<Duration>,
    remove: F,
) where
    F: Fn(&Path) -> io::Result<()>,
{
    if backup_retention.is_none() && backup_max_age.is_none() {
        return;
    }
    let dir = backup_dir.map(Path::to_path_buf).unwrap_or_else(|| {
        destination
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    });
    let Some(file_name) = destination.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!("{file_name}.old-");
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let now = SystemTime::now();
    let mut backups: Vec<(PathBuf, SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        backups.push((
            entry.path(),
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        ));
    }
    backups.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    for (index, (path, modified)) in backups.iter().enumerate() {
        let over_retention = backup_retention.is_some_and(|keep| index >= keep);
        let too_old = backup_max_age.is_some_and(|max_age| match now.duration_since(*modified) {
            Ok(age) => age > max_age,
            Err(_) => true,
        });
        if over_retention || too_old {
            let _ = remove(path);
        }
    }
}

fn replace_in_place(options: &RotateOptions, destination: &Path) -> Result<RotateOutcome> {
    match copy_binary(&options.source, destination) {
        Ok(()) => Ok(RotateOutcome::Replaced {
            destination: destination.to_path_buf(),
        }),
        Err(error) if is_running_binary_error(&error) => rename_aside_replace(options, destination),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to replace {} with {}",
                destination.display(),
                options.source.display()
            )
        }),
    }
}

/// In-place copy that refuses to touch a destination that is in use.
///
/// This is [`RotateStrategy::Replace`] without the rename-aside fallback: a
/// deployment that must not swap a running service fails instead of moving the
/// old binary out of the way.
fn fail_if_locked_replace(options: &RotateOptions, destination: &Path) -> Result<RotateOutcome> {
    match copy_binary(&options.source, destination) {
        Ok(()) => Ok(RotateOutcome::Replaced {
            destination: destination.to_path_buf(),
        }),
        Err(error) if is_destination_in_use(&error) => {
            // Keep the underlying error text in the message itself: callers
            // (for example an xtask writing deploy-error.log) may print only
            // the top-level context.
            let message = format!(
                "failed to replace {}: the destination is in use ({error}); strategy 'fail-if-locked' does not rename the old binary aside; stop the process or service first",
                destination.display()
            );
            Err(error).with_context(|| message)
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to replace {} with {}",
                destination.display(),
                options.source.display()
            )
        }),
    }
}

#[cfg(unix)]
fn atomic_replace(options: &RotateOptions, destination: &Path) -> Result<RotateOutcome> {
    let temp = unique_sibling(destination, "new");
    copy_binary(&options.source, &temp).with_context(|| {
        format!(
            "failed to stage {} at {}",
            options.source.display(),
            temp.display()
        )
    })?;
    match fs::rename(&temp, destination) {
        Ok(()) => Ok(RotateOutcome::Replaced {
            destination: destination.to_path_buf(),
        }),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
                .with_context(|| format!("failed to atomically replace {}", destination.display()))
        }
    }
}

#[cfg(windows)]
fn atomic_replace(options: &RotateOptions, destination: &Path) -> Result<RotateOutcome> {
    // Windows cannot atomically replace a running executable. Rename-aside
    // works whether the destination is locked or not.
    rename_aside_replace(options, destination)
}

fn rename_aside_replace(options: &RotateOptions, destination: &Path) -> Result<RotateOutcome> {
    rename_aside_replace_with(options, destination, copy_binary)
}

/// Rename-aside rotation with an injectable installer.
///
/// `copy` is called with `(source, destination)` once the old binary has been
/// moved to its backup path; production rotations pass [`copy_binary`]. The
/// indirection exists so tests can force the install step to fail and check the
/// diagnostic: backup path, rollback result, and the copy failure itself.
fn rename_aside_replace_with<C>(
    options: &RotateOptions,
    destination: &Path,
    copy: C,
) -> Result<RotateOutcome>
where
    C: FnOnce(&Path, &Path) -> io::Result<()>,
{
    let backup = unique_backup_path(destination, options.backup_dir.as_deref());
    if let Some(parent) = backup.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(destination, &backup).with_context(|| {
        format!(
            "failed to rename {} aside to {}; stop the process or service first",
            destination.display(),
            backup.display()
        )
    })?;
    match copy(&options.source, destination) {
        Ok(()) => Ok(RotateOutcome::ReplacedAfterRename {
            destination: destination.to_path_buf(),
            backup,
        }),
        Err(error) => {
            let cause = error.to_string();
            let rollback = fs::rename(&backup, destination);
            let rollback = match rollback {
                Ok(()) => "ok".to_owned(),
                Err(rollback_error) => format!("failed: {rollback_error}"),
            };
            Err(error).with_context(|| {
                format!(
                    "failed to install {} after renaming the old binary aside to {}; rollback: {rollback}; cause: {cause}",
                    destination.display(),
                    backup.display()
                )
            })
        }
    }
}

fn copy_binary(source: &Path, destination: &Path) -> io::Result<()> {
    fs::copy(source, destination)?;
    set_executable(destination)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn is_running_binary_error(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        // 5: ERROR_ACCESS_DENIED, 32: ERROR_SHARING_VIOLATION,
        // 33: ERROR_LOCK_VIOLATION, 1224: ERROR_USER_MAPPED_FILE.
        const RUNNING_BINARY_ERRORS: [i32; 4] = [5, 32, 33, 1224];
        error
            .raw_os_error()
            .is_some_and(|code| RUNNING_BINARY_ERRORS.contains(&code))
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

/// Whether `error` means the destination is currently in use.
///
/// This is the classification used by [`RotateStrategy::FailIfLocked`]: the
/// Windows running-binary codes (see [`is_running_binary_error`]) plus
/// `ETXTBSY` on Unix, which the kernel reports when an executing file is opened
/// for writing.
///
/// The rename-aside fallback of `Replace`/`Auto` deliberately keeps using the
/// narrower [`is_running_binary_error`] so the existing fallback behaviour (and
/// its error-code coverage) does not change.
fn is_destination_in_use(error: &io::Error) -> bool {
    if is_running_binary_error(error) {
        return true;
    }
    #[cfg(unix)]
    {
        // ETXTBSY ("Text file busy"), 26 on Linux and macOS.
        const ETXTBSY: i32 = 26;
        error.raw_os_error() == Some(ETXTBSY)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Resolve the real file a rotation touches.
///
/// A symlink destination is canonicalized, so the rotation replaces the link
/// *target*; the link itself is left in place and keeps pointing at that
/// target. `canonicalize` also resolves `.`/`..` and (on Windows) returns the
/// `\\?\`-prefixed absolute path. When the destination is not a symlink, or the
/// link cannot be resolved, the path is used as-is.
fn resolve_destination(destination: &Path) -> Result<PathBuf> {
    if destination.is_symlink()
        && let Ok(target) = fs::canonicalize(destination)
    {
        return Ok(target);
    }
    Ok(destination.to_path_buf())
}

#[cfg(unix)]
fn unique_sibling(destination: &Path, label: &str) -> PathBuf {
    let parent = destination
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("binary");
    let base = timestamp_nanos();
    for attempt in 0..1000 {
        let candidate = parent.join(format!(
            ".{file_name}.{label}-{}-{}",
            std::process::id(),
            base.saturating_add(attempt)
        ));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!(".{file_name}.{label}-{}", std::process::id()))
}

fn unique_backup_path(destination: &Path, backup_dir: Option<&Path>) -> PathBuf {
    let dir = backup_dir.map(Path::to_path_buf).unwrap_or_else(|| {
        destination
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    });
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("binary");
    let base = timestamp_nanos();
    for attempt in 0..1000 {
        let candidate = dir.join(format!(
            "{file_name}.old-{}-{}",
            std::process::id(),
            base.saturating_add(attempt)
        ));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{file_name}.old-{}", std::process::id()))
}

fn timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::{
        RotateOptions, RotateOutcome, RotateStrategy, cleanup_backups_with,
        rename_aside_replace_with, rotate,
    };

    #[cfg(unix)]
    use std::io::Read;

    #[cfg(any(windows, target_os = "linux"))]
    use std::process::{Child, Command, Stdio};

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tracel-deploy-rotate-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn touch_aged(path: &PathBuf, age: Duration) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open backup");
        file.set_modified(SystemTime::now() - age)
            .expect("set backup mtime");
    }

    fn backup_names(dir: &Path) -> Vec<String> {
        let mut names = fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.contains(".old-"))
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    }

    /// Rotate `source` over an existing, non-running `destination` and report
    /// the outcome together with the temp dir that holds both files.
    fn rotate_with_strategy(label: &str, strategy: RotateStrategy) -> (RotateOutcome, PathBuf) {
        let dir = temp_dir(label);
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");

        let outcome = rotate(&RotateOptions {
            source,
            destination,
            backup_dir: None,
            strategy,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect("rotate");
        (outcome, dir)
    }

    /// Copy the test executable into `dir` and run it on the ignored helper
    /// test, so the copy is an *executing* binary that a rotation has to deal
    /// with. Returns `None` when the copy cannot be executed (for example a
    /// `noexec` temporary directory).
    #[cfg(any(windows, target_os = "linux"))]
    fn stage_running_binary(dir: &Path) -> Option<Child> {
        let holder = dir.join("holder.exe");
        fs::copy(
            std::env::current_exe().expect("current test executable"),
            &holder,
        )
        .expect("stage holder copy");
        let child = Command::new(&holder)
            .args([
                "--exact",
                "rotate::tests::hold_binary_alive_for_rotate",
                "--ignored",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        std::thread::sleep(Duration::from_millis(500));
        Some(child)
    }

    #[test]
    fn replaces_missing_destination() {
        let dir = temp_dir("missing");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");

        let outcome = rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect("rotate");
        assert!(matches!(outcome, RotateOutcome::Replaced { .. }));
        assert_eq!(fs::read_to_string(&destination).expect("read"), "new");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dry_run_does_not_modify_destination() {
        let dir = temp_dir("dry-run");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");

        let outcome = rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: true,
            ..RotateOptions::default()
        })
        .expect("rotate");
        assert!(matches!(outcome, RotateOutcome::WouldReplace { .. }));
        assert_eq!(fs::read_to_string(&destination).expect("read"), "old");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rename_aside_keeps_backup_and_installs_new_binary() {
        let dir = temp_dir("rename-aside");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");

        let outcome = rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::RenameAside,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect("rotate");
        match outcome {
            RotateOutcome::ReplacedAfterRename { backup, .. } => {
                assert_eq!(fs::read_to_string(&backup).expect("backup"), "old");
            }
            other => panic!("expected rename-aside outcome, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&destination).expect("read"), "new");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replace_keeps_running_handle_on_old_inode() {
        let dir = temp_dir("atomic");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");
        let mut running = fs::File::open(&destination).expect("open running binary");

        let outcome = rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Atomic,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect("rotate");
        assert!(matches!(outcome, RotateOutcome::Replaced { .. }));

        let mut old = String::new();
        running
            .read_to_string(&mut old)
            .expect("read running inode");
        assert_eq!(old, "old");
        assert_eq!(fs::read_to_string(&destination).expect("read new"), "new");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backup_retention_keeps_newest_backups() {
        let dir = temp_dir("retention");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");
        for (name, age_hours) in [
            ("destination.old-1-1", 3_u64),
            ("destination.old-2-2", 2),
            ("destination.old-3-3", 1),
        ] {
            let path = dir.join(name);
            fs::write(&path, "backup").expect("backup");
            touch_aged(&path, Duration::from_secs(age_hours * 3_600));
        }
        let other = dir.join("other.old-1-1");
        fs::write(&other, "other").expect("other backup");

        rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::RenameAside,
            dry_run: false,
            backup_retention: Some(2),
            backup_max_age: None,
        })
        .expect("rotate");

        let backups = backup_names(&dir);
        assert_eq!(backups.len(), 3, "kept: {backups:?}");
        assert!(
            backups
                .iter()
                .any(|name| name.starts_with(&format!("destination.old-{}", std::process::id()))),
            "the backup created by this rotation is kept: {backups:?}"
        );
        assert!(
            backups.iter().any(|name| name == "other.old-1-1"),
            "backups of other binaries are not touched: {backups:?}"
        );
        assert!(
            !backups.iter().any(|name| name == "destination.old-1-1"),
            "the oldest backup is removed: {backups:?}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backup_max_age_removes_old_backups() {
        let dir = temp_dir("max-age");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");
        for (name, age_hours) in [
            ("destination.old-1-1", 72_u64),
            ("destination.old-2-2", 48),
            ("destination.old-3-3", 1),
        ] {
            let path = dir.join(name);
            fs::write(&path, "backup").expect("backup");
            touch_aged(&path, Duration::from_secs(age_hours * 3_600));
        }

        rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::RenameAside,
            dry_run: false,
            backup_retention: None,
            backup_max_age: Some(Duration::from_secs(36 * 3_600)),
        })
        .expect("rotate");

        let backups = backup_names(&dir);
        assert_eq!(backups.len(), 2, "kept: {backups:?}");
        assert!(backups.iter().any(|name| name == "destination.old-3-3"));
        assert!(!backups.iter().any(|name| name == "destination.old-1-1"));
        assert!(!backups.iter().any(|name| name == "destination.old-2-2"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cleanup_ignores_removal_failures() {
        let dir = temp_dir("cleanup-failure");
        let destination = dir.join("destination");
        fs::write(&destination, "old").expect("destination");
        for name in ["destination.old-1-1", "destination.old-2-2"] {
            fs::write(dir.join(name), "backup").expect("backup");
        }

        cleanup_backups_with(&destination, None, Some(0), None, |_path| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "still in use",
            ))
        });

        assert_eq!(backup_names(&dir).len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dry_run_keeps_backups() {
        let dir = temp_dir("dry-run-backups");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");
        fs::write(dir.join("destination.old-1-1"), "backup").expect("backup");

        rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: true,
            backup_retention: Some(0),
            backup_max_age: None,
        })
        .expect("rotate");

        assert_eq!(backup_names(&dir).len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn running_binary_error_classification() {
        use std::io::Error;

        for code in [5, 32, 33, 1224] {
            let error = Error::from_raw_os_error(code);
            assert!(
                super::is_running_binary_error(&error),
                "code {code} must be treated as a running-binary error"
            );
            assert!(
                super::is_destination_in_use(&error),
                "code {code} must be treated as in use by fail-if-locked"
            );
        }
        let other = Error::from_raw_os_error(2);
        assert!(!super::is_running_binary_error(&other));
        assert!(!super::is_destination_in_use(&other));
    }

    #[cfg(windows)]
    #[test]
    fn replaces_a_running_binary() {
        let dir = temp_dir("running-binary");
        let holder = dir.join("holder.exe");
        let executable = std::env::current_exe().expect("current test executable");
        fs::copy(&executable, &holder).expect("stage holder copy");

        let mut child = std::process::Command::new(&holder)
            .args([
                "--exact",
                "rotate::tests::hold_binary_alive_for_rotate",
                "--ignored",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the running binary");
        std::thread::sleep(Duration::from_millis(500));

        let source = dir.join("replacement.exe");
        fs::write(&source, "replacement").expect("write replacement");
        let outcome = rotate(&RotateOptions {
            source,
            destination: holder.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect("rotate a running binary");

        assert!(
            matches!(outcome, RotateOutcome::ReplacedAfterRename { .. }),
            "outcome: {outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(&holder).expect("read replacement"),
            "replacement"
        );
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "the running process must keep running"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn strategy_labels_are_stable() {
        let labels = [
            RotateStrategy::Auto,
            RotateStrategy::Replace,
            RotateStrategy::Atomic,
            RotateStrategy::RenameAside,
            RotateStrategy::FailIfLocked,
        ]
        .map(|strategy| strategy.label());
        assert_eq!(
            labels,
            [
                "auto",
                "replace",
                "atomic",
                "rename-aside",
                "fail-if-locked"
            ]
        );
        // `RotateStep::from_context` relies on the default staying `Auto`.
        assert_eq!(RotateStrategy::default(), RotateStrategy::Auto);
    }

    #[cfg(windows)]
    #[test]
    fn strategy_matrix_documents_windows_degradation() {
        let atomic = RotateStrategy::Atomic.describe();
        assert!(
            atomic.contains("degraded on Windows to rename-aside"),
            "atomic on windows: {atomic}"
        );
        for strategy in [RotateStrategy::Auto, RotateStrategy::Replace] {
            let described = strategy.describe();
            assert!(
                described.contains("rename the old binary aside when it is running"),
                "{} on windows: {described}",
                strategy.label()
            );
        }
        assert!(
            !RotateStrategy::Auto
                .describe()
                .contains("atomically rename"),
            "windows auto must not promise an atomic swap: {}",
            RotateStrategy::Auto.describe()
        );
        assert!(
            RotateStrategy::RenameAside
                .describe()
                .contains("undo the rename when the copy fails"),
            "rename-aside on windows: {}",
            RotateStrategy::RenameAside.describe()
        );
        let fail_if_locked = RotateStrategy::FailIfLocked.describe();
        assert!(
            fail_if_locked.contains("running or locked destination")
                && fail_if_locked.contains("instead of renaming it aside"),
            "fail-if-locked on windows: {fail_if_locked}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn strategy_matrix_documents_unix_behaviour() {
        for strategy in [RotateStrategy::Auto, RotateStrategy::Atomic] {
            let described = strategy.describe();
            assert!(
                described.contains("atomically rename it over the destination"),
                "{} on unix: {described}",
                strategy.label()
            );
        }
        assert!(
            RotateStrategy::Replace
                .describe()
                .contains("fails when the destination is running"),
            "replace on unix: {}",
            RotateStrategy::Replace.describe()
        );
        assert!(
            RotateStrategy::RenameAside
                .describe()
                .contains("undo the rename when the copy fails"),
            "rename-aside on unix: {}",
            RotateStrategy::RenameAside.describe()
        );
        let fail_if_locked = RotateStrategy::FailIfLocked.describe();
        assert!(
            fail_if_locked.contains("fails")
                && fail_if_locked.contains("instead of renaming it aside"),
            "fail-if-locked on unix: {fail_if_locked}"
        );
        assert!(
            !fail_if_locked.contains("locked destination"),
            "unix reports a running binary as ETXTBSY, not as a lock: {fail_if_locked}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_strategy_dispatch_matches_the_matrix() {
        let (outcome, dir) = rotate_with_strategy("matrix-replace", RotateStrategy::Replace);
        assert!(
            matches!(outcome, RotateOutcome::Replaced { .. }),
            "replace copies in place: {outcome:?}"
        );
        assert_eq!(backup_names(&dir).len(), 0, "replace keeps no backup");
        let _ = fs::remove_dir_all(dir);

        let (outcome, dir) = rotate_with_strategy("matrix-atomic", RotateStrategy::Atomic);
        assert!(
            matches!(outcome, RotateOutcome::ReplacedAfterRename { .. }),
            "atomic degrades to rename-aside on Windows: {outcome:?}"
        );
        let _ = fs::remove_dir_all(dir);

        for (label, strategy, renames_aside) in [
            ("matrix-rename-aside", RotateStrategy::RenameAside, true),
            ("matrix-auto", RotateStrategy::Auto, false),
            ("matrix-fail-if-locked", RotateStrategy::FailIfLocked, false),
        ] {
            let (outcome, dir) = rotate_with_strategy(label, strategy);
            assert_eq!(
                matches!(outcome, RotateOutcome::ReplacedAfterRename { .. }),
                renames_aside,
                "{label}: {outcome:?}"
            );
            assert_eq!(
                backup_names(&dir).len(),
                usize::from(renames_aside),
                "{label} backups"
            );
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_strategy_dispatch_matches_the_matrix() {
        // `Replace` and `FailIfLocked` rewrite the existing inode, so a handle
        // opened before the rotation sees the new bytes.
        for (label, strategy) in [
            ("matrix-replace", RotateStrategy::Replace),
            ("matrix-fail-if-locked", RotateStrategy::FailIfLocked),
        ] {
            let dir = temp_dir(label);
            let source = dir.join("source");
            let destination = dir.join("destination");
            fs::write(&source, "new").expect("source");
            fs::write(&destination, "old").expect("destination");
            let mut handle = fs::File::open(&destination).expect("open destination");

            let outcome = rotate(&RotateOptions {
                source,
                destination,
                backup_dir: None,
                strategy,
                dry_run: false,
                ..RotateOptions::default()
            })
            .expect("rotate");
            assert!(
                matches!(outcome, RotateOutcome::Replaced { .. }),
                "{label}: {outcome:?}"
            );

            let mut seen = String::new();
            handle
                .read_to_string(&mut seen)
                .expect("read the open handle");
            assert_eq!(seen, "new", "{label} rewrites the existing inode");
            assert_eq!(backup_names(&dir).len(), 0, "{label} keeps no backup");
            let _ = fs::remove_dir_all(dir);
        }

        // `Auto` and `Atomic` stage a sibling file and rename it over the
        // destination, so the old inode survives for the open handle.
        for (label, strategy) in [
            ("matrix-auto", RotateStrategy::Auto),
            ("matrix-atomic", RotateStrategy::Atomic),
        ] {
            let dir = temp_dir(label);
            let source = dir.join("source");
            let destination = dir.join("destination");
            fs::write(&source, "new").expect("source");
            fs::write(&destination, "old").expect("destination");
            let mut handle = fs::File::open(&destination).expect("open destination");

            let outcome = rotate(&RotateOptions {
                source,
                destination: destination.clone(),
                backup_dir: None,
                strategy,
                dry_run: false,
                ..RotateOptions::default()
            })
            .expect("rotate");
            assert!(
                matches!(outcome, RotateOutcome::Replaced { .. }),
                "{label}: {outcome:?}"
            );

            let mut seen = String::new();
            handle
                .read_to_string(&mut seen)
                .expect("read the open handle");
            assert_eq!(seen, "old", "{label} swaps in a new inode");
            assert_eq!(
                fs::read_to_string(&destination).expect("read new"),
                "new",
                "{label} installs the replacement"
            );
            let _ = fs::remove_dir_all(dir);
        }

        let (outcome, dir) =
            rotate_with_strategy("matrix-rename-aside", RotateStrategy::RenameAside);
        assert!(
            matches!(outcome, RotateOutcome::ReplacedAfterRename { .. }),
            "rename-aside: {outcome:?}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn fail_if_locked_rejects_a_running_binary() {
        let dir = temp_dir("fail-if-locked");
        let mut child = stage_running_binary(&dir).expect("spawn the running binary");
        let destination = dir.join("holder.exe");
        let staged = fs::read(&destination).expect("read the running copy");
        let source = dir.join("replacement.exe");
        fs::write(&source, "replacement").expect("write replacement");

        let error = rotate(&RotateOptions {
            source: source.clone(),
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::FailIfLocked,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect_err("fail-if-locked must not rotate a running binary");
        let text = format!("{error:#}");
        assert!(text.contains("the destination is in use"), "error: {text}");
        assert!(text.contains("'fail-if-locked'"), "error: {text}");
        assert!(
            text.contains("stop the process or service first"),
            "error: {text}"
        );
        assert!(
            text.contains(&destination.display().to_string()),
            "error: {text}"
        );
        assert_eq!(
            fs::read(&destination).expect("read destination"),
            staged,
            "the running binary must be left untouched"
        );
        assert_eq!(
            backup_names(&dir).len(),
            0,
            "fail-if-locked must not rename the binary aside"
        );

        // The same destination rotates under the default strategy, so the
        // refusal above came from the strategy and not from the platform.
        let outcome = rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect("auto rotates a running binary");
        assert!(
            matches!(outcome, RotateOutcome::ReplacedAfterRename { .. }),
            "outcome: {outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(&destination).expect("read replacement"),
            "replacement"
        );
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "the running process must keep running"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fail_if_locked_rejects_an_executing_binary() {
        let dir = temp_dir("fail-if-locked-linux");
        let Some(mut child) = stage_running_binary(&dir) else {
            // A `noexec` temporary directory cannot stage the ETXTBSY
            // scenario; the classification unit test still covers the code.
            eprintln!("skipping: the staged copy cannot be executed here");
            let _ = fs::remove_dir_all(dir);
            return;
        };
        let destination = dir.join("holder.exe");
        let staged = fs::read(&destination).expect("read the executing copy");
        let source = dir.join("replacement");
        fs::write(&source, "replacement").expect("write replacement");

        let error = rotate(&RotateOptions {
            source: source.clone(),
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::FailIfLocked,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect_err("fail-if-locked must not rotate an executing binary");
        let text = format!("{error:#}");
        assert!(text.contains("the destination is in use"), "error: {text}");
        assert_eq!(
            fs::read(&destination).expect("read destination"),
            staged,
            "the executing binary must be left untouched"
        );
        assert_eq!(
            backup_names(&dir).len(),
            0,
            "fail-if-locked must not rename the binary aside"
        );

        // The default strategy stages and renames instead, which works while
        // the old image is still being executed.
        let outcome = rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect("auto rotates an executing binary");
        assert!(
            matches!(outcome, RotateOutcome::Replaced { .. }),
            "outcome: {outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(&destination).expect("read replacement"),
            "replacement"
        );
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "the executing process must keep running"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn unix_in_use_classification_covers_text_file_busy() {
        use std::io::Error;

        // 26 is ETXTBSY, reported when an executing file is opened for writing.
        assert!(
            super::is_destination_in_use(&Error::from_raw_os_error(26)),
            "ETXTBSY means the destination is in use"
        );
        // 13 is EACCES (for example a read-only file): a failure, but not a
        // sign that the binary is running.
        assert!(!super::is_destination_in_use(&Error::from_raw_os_error(13)));
        // The rename-aside fallback keeps the narrower, Windows-only check.
        assert!(!super::is_running_binary_error(&Error::from_raw_os_error(
            26
        )));
    }

    #[cfg(unix)]
    #[test]
    fn rotate_replaces_the_symlink_target_and_keeps_the_link() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("symlink");
        let target = dir.join("installed");
        let link = dir.join("current");
        let source = dir.join("new-binary");
        fs::write(&target, "old").expect("target");
        fs::write(&source, "new").expect("source");
        symlink(&target, &link).expect("symlink");

        let outcome = rotate(&RotateOptions {
            source,
            destination: link.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect("rotate");

        assert!(
            fs::symlink_metadata(&link)
                .expect("link metadata")
                .file_type()
                .is_symlink(),
            "the symlink itself survives the rotation"
        );
        assert_eq!(
            fs::read_link(&link).expect("link target"),
            target,
            "the link keeps pointing at the same target"
        );
        assert_eq!(fs::read_to_string(&target).expect("target"), "new");
        assert_eq!(
            outcome,
            RotateOutcome::Replaced {
                destination: fs::canonicalize(&target).expect("canonical target"),
            },
            "the rotation reports the resolved target"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn rename_aside_keeps_the_backup_next_to_the_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("symlink-rename-aside");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).expect("bin dir");
        let target = dir.join("installed");
        let link = bin.join("current");
        let source = dir.join("new-binary");
        fs::write(&target, "old").expect("target");
        fs::write(&source, "new").expect("source");
        symlink(&target, &link).expect("symlink");

        let outcome = rotate(&RotateOptions {
            source,
            destination: link.clone(),
            backup_dir: None,
            strategy: RotateStrategy::RenameAside,
            dry_run: false,
            ..RotateOptions::default()
        })
        .expect("rotate");

        match outcome {
            RotateOutcome::ReplacedAfterRename { backup, .. } => {
                assert_eq!(
                    backup.parent(),
                    Some(dir.as_path()),
                    "the old binary is renamed aside next to the resolved target"
                );
                assert_eq!(fs::read_to_string(&backup).expect("backup"), "old");
            }
            other => panic!("expected rename-aside outcome, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&target).expect("target"), "new");
        assert!(
            fs::symlink_metadata(&link)
                .expect("link metadata")
                .file_type()
                .is_symlink(),
            "the symlink itself survives the rotation"
        );
        assert_eq!(
            fs::read_link(&link).expect("link target"),
            target,
            "the link keeps pointing at the same target"
        );
        let _ = fs::remove_dir_all(dir);
    }

    fn rename_aside_options(source: PathBuf, destination: PathBuf) -> RotateOptions {
        RotateOptions {
            source,
            destination,
            backup_dir: None,
            strategy: RotateStrategy::RenameAside,
            dry_run: false,
            ..RotateOptions::default()
        }
    }

    #[test]
    fn rename_aside_reports_backup_rollback_and_cause() {
        let dir = temp_dir("rename-aside-diagnostics");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");

        let error = rename_aside_replace_with(
            &rename_aside_options(source, destination.clone()),
            &destination,
            |_source, _destination| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected copy failure",
                ))
            },
        )
        .expect_err("the injected copy failure must fail the rotation");

        let text = format!("{error:#}");
        assert!(
            text.contains(&format!(
                "failed to install {} after renaming the old binary aside to {}.old-",
                destination.display(),
                destination.display()
            )),
            "the error must name the backup path: {text}"
        );
        assert!(text.contains("rollback: ok"), "error: {text}");
        assert!(
            text.contains("cause: injected copy failure"),
            "error: {text}"
        );
        assert_eq!(
            error
                .root_cause()
                .downcast_ref::<io::Error>()
                .map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied),
            "the copy error stays reachable as the cause"
        );

        assert_eq!(
            fs::read_to_string(&destination).expect("destination"),
            "old",
            "the rollback restores the previous binary"
        );
        assert_eq!(
            backup_names(&dir).len(),
            0,
            "a successful rollback leaves no backup behind"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rename_aside_reports_a_failed_rollback() {
        let dir = temp_dir("rename-aside-rollback-failure");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");

        let error = rename_aside_replace_with(
            &rename_aside_options(source, destination.clone()),
            &destination,
            |_source, destination| {
                // Occupy the destination again so the rollback rename cannot
                // put the old binary back.
                fs::create_dir(destination)?;
                Err(io::Error::other("injected copy failure"))
            },
        )
        .expect_err("the injected copy failure must fail the rotation");

        let text = format!("{error:#}");
        assert!(
            text.contains("rollback: failed:"),
            "the error must report the failed rollback: {text}"
        );
        assert!(
            text.contains("cause: injected copy failure"),
            "error: {text}"
        );
        assert!(
            destination.is_dir(),
            "the injected directory still blocks the destination"
        );

        let backups = backup_names(&dir);
        assert_eq!(backups.len(), 1, "backups: {backups:?}");
        assert_eq!(
            fs::read_to_string(dir.join(&backups[0])).expect("backup"),
            "old",
            "a failed rollback keeps the old binary in the backup file"
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// Helper for the running-binary test: keeps the copied executable alive
    /// so the rotation has to move a running image out of the way.
    #[test]
    #[ignore]
    fn hold_binary_alive_for_rotate() {
        std::thread::sleep(Duration::from_secs(30));
    }
}
