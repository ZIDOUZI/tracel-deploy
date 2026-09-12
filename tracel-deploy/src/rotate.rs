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

/// Deferred-cleanup policy for `<binary>.old-*` backups that could not be
/// deleted.
///
/// On Windows a backup that a still-running process occupies cannot be
/// removed; without a policy the rotation forgets about it and the file stays
/// behind forever. With a policy configured the failed path is registered in a
/// `<binary>.pending-delete` manifest next to the backups and retried by the
/// next rotation or by an explicit [`cleanup_pending_backups`] call.
///
/// Entries leave the registry — without another deletion attempt — once they
/// are older than [`BackupRetry::max_age`] or once the registry exceeds
/// [`BackupRetry::max_entries`]; both drops are reported in
/// [`CleanupReport::discarded`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRetry {
    /// How long a pending entry is retried after its first failed attempt.
    pub max_age: Duration,
    /// How many pending entries are kept for one destination; the oldest
    /// entries go away first.
    pub max_entries: usize,
}

impl BackupRetry {
    /// Default pending lifetime: seven days.
    pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    /// Default number of pending entries kept per destination.
    pub const DEFAULT_MAX_ENTRIES: usize = 32;
}

impl Default for BackupRetry {
    fn default() -> Self {
        Self {
            max_age: Self::DEFAULT_MAX_AGE,
            max_entries: Self::DEFAULT_MAX_ENTRIES,
        }
    }
}

impl BackupRetry {
    /// The default policy: retry for seven days, keep at most 32 entries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace [`BackupRetry::max_age`].
    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// Replace [`BackupRetry::max_entries`].
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }
}

/// One registered deletion that still has to be retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBackup {
    /// The `<binary>.old-*` backup that could not be deleted.
    pub path: PathBuf,
    /// When the first deletion attempt failed.
    pub first_failure: SystemTime,
    /// When the most recent deletion attempt failed.
    pub last_attempt: SystemTime,
    /// Number of deletion attempts that failed so far.
    pub attempts: u32,
    /// Error text of the most recent failed attempt.
    pub reason: String,
}

/// Why a pending entry left the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingDropReason {
    /// The entry is older than [`BackupRetry::max_age`].
    Expired,
    /// The registry exceeded [`BackupRetry::max_entries`].
    Evicted,
    /// The file name does not match `<destination file name>.old-`, so the
    /// entry is not this destination's backup and the file is never touched.
    OutOfScope,
}

impl PendingDropReason {
    /// Stable lowercase name, suitable for logs and reports.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Evicted => "evicted",
            Self::OutOfScope => "out-of-scope",
        }
    }
}

/// A pending entry that was dropped from the registry without another
/// deletion attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardedPending {
    /// The backup the entry pointed at.
    pub path: PathBuf,
    /// Why the entry was dropped.
    pub reason: PendingDropReason,
}

/// Outcome of one pending-backup cleanup pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// True when the pass did not delete anything and did not write a registry.
    pub dry_run: bool,
    /// Registered backups this pass attempted to delete.
    pub retried: Vec<PathBuf>,
    /// Paths that are gone now: deleted by this pass, or already missing.
    pub removed: Vec<PathBuf>,
    /// Entries still registered for a later pass.
    pub pending: Vec<PendingBackup>,
    /// Entries dropped from the registry without another deletion attempt.
    pub discarded: Vec<DiscardedPending>,
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
    /// Deferred cleanup policy for backups whose deletion failed, for example
    /// because a running process still holds the file open on Windows.
    ///
    /// When set, a failed removal is registered in
    /// `<backup_dir>/<binary>.pending-delete` instead of being forgotten, and
    /// the next rotation (or an explicit [`cleanup_pending_backups`] call)
    /// retries it. `None` keeps the best-effort behaviour of
    /// `backup_retention`/`backup_max_age`: a failed removal is dropped.
    pub backup_cleanup_retry: Option<BackupRetry>,
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
    rotate_with_remover(options, delete_backup)
}

/// The deletion primitive used in production, kept as a named function so it
/// can be passed to the pending-cleanup entry points.
fn delete_backup(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

/// [`rotate`] with the file-removal primitive injected.
///
/// The indirection lets the tests force a backup deletion to fail and check
/// that the rotation still succeeds and registers the pending deletion.
fn rotate_with_remover<R>(options: &RotateOptions, remove: R) -> Result<RotateOutcome>
where
    R: Fn(&Path) -> io::Result<()>,
{
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
        cleanup_old_backups(options, &outcome, &remove);
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
    cleanup_old_backups(options, &outcome, &remove);
    Ok(outcome)
}

/// Best-effort removal of `<binary>.old-*` backups for the rotated destination.
///
/// Removal is intentionally best effort: on Windows a backup that a still
/// running process occupies cannot be deleted yet, and that must never fail
/// the rotation. With [`RotateOptions::backup_cleanup_retry`] a failed removal
/// is registered for a later retry; without the policy it is forgotten (the
/// best-effort behaviour of `backup_retention`/`backup_max_age`). Only files
/// matching the destination's own backup prefix are considered, so unrelated
/// binaries are left alone.
fn cleanup_old_backups<R>(options: &RotateOptions, outcome: &RotateOutcome, remove: &R)
where
    R: Fn(&Path) -> io::Result<()>,
{
    let destination = match outcome {
        RotateOutcome::Replaced { destination }
        | RotateOutcome::ReplacedAfterRename { destination, .. } => destination,
        RotateOutcome::WouldReplace { .. } => return,
    };
    if options.dry_run {
        return;
    }
    let policy = options.backup_cleanup_retry.as_ref();
    let backup_dir = options.backup_dir.as_deref();
    let now = SystemTime::now();
    // Retry the registered deletions first: the process that held a backup
    // open may have exited since the last rotation.
    let mut registered = Vec::new();
    if let Some(policy) = policy
        && let Ok(report) =
            retry_pending_backups(destination, backup_dir, policy, now, false, remove)
    {
        registered = report.pending.into_iter().map(|entry| entry.path).collect();
    }
    // Then the retention/max-age sweep. Backups that are still registered were
    // already retried above, so the sweep skips them; every other removal that
    // fails is registered for a later pass.
    let failures = run_backup_cleanup(
        destination,
        backup_dir,
        options.backup_retention,
        options.backup_max_age,
        |path| {
            if registered.iter().any(|pending| pending == path) {
                return Ok(());
            }
            remove(path)
        },
    );
    if let Some(policy) = policy
        && !failures.is_empty()
    {
        let _ = register_pending_backups(destination, backup_dir, policy, now, &failures);
    }
}

/// Injection seam of the `04` retention tests: a best-effort sweep that
/// ignores removal failures (the production path goes through
/// [`run_backup_cleanup`] so failures can be registered).
#[cfg(test)]
fn cleanup_backups_with<F>(
    destination: &Path,
    backup_dir: Option<&Path>,
    backup_retention: Option<usize>,
    backup_max_age: Option<Duration>,
    remove: F,
) where
    F: Fn(&Path) -> io::Result<()>,
{
    let _ = run_backup_cleanup(
        destination,
        backup_dir,
        backup_retention,
        backup_max_age,
        remove,
    );
}

/// Run the `backup_retention`/`backup_max_age` sweep and report which removals
/// failed, so the caller can register them for a later retry.
fn run_backup_cleanup<F>(
    destination: &Path,
    backup_dir: Option<&Path>,
    backup_retention: Option<usize>,
    backup_max_age: Option<Duration>,
    remove: F,
) -> Vec<(PathBuf, String)>
where
    F: Fn(&Path) -> io::Result<()>,
{
    let mut failures = Vec::new();
    if backup_retention.is_none() && backup_max_age.is_none() {
        return failures;
    }
    let dir = backup_directory(destination, backup_dir);
    let Some(prefix) = backup_prefix(destination) else {
        return failures;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return failures;
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
        if (over_retention || too_old)
            && let Err(error) = remove(path)
        {
            failures.push((path.clone(), error.to_string()));
        }
    }
    failures
}

/// Retry the deletions that earlier rotations registered as pending.
///
/// Without [`RotateOptions::backup_cleanup_retry`] the call is a no-op; with
/// `dry_run` nothing is deleted and no registry is written. Otherwise one pass
///
/// - drops entries whose file name does not match `<destination file name>.old-`
///   (another binary's backups are never touched),
/// - drops entries older than [`BackupRetry::max_age`] without another attempt,
/// - retries the rest and drops every entry whose file is gone,
/// - keeps at most [`BackupRetry::max_entries`] entries, oldest first out.
///
/// The returned [`CleanupReport`] lists what was retried, removed, kept, and
/// discarded. A periodic consumer (for example an xtask `clean old` subcommand
/// or an install hook) can call this to collect the backups that a
/// since-exited process had been holding open.
pub fn cleanup_pending_backups(options: &RotateOptions) -> Result<CleanupReport> {
    let Some(policy) = options.backup_cleanup_retry.as_ref() else {
        return Ok(CleanupReport::default());
    };
    let destination = resolve_destination(&options.destination)?;
    retry_pending_backups(
        &destination,
        options.backup_dir.as_deref(),
        policy,
        SystemTime::now(),
        options.dry_run,
        &delete_backup,
    )
}

/// One retry pass over the `<binary>.pending-delete` registry.
fn retry_pending_backups<F>(
    destination: &Path,
    backup_dir: Option<&Path>,
    policy: &BackupRetry,
    now: SystemTime,
    dry_run: bool,
    remove: &F,
) -> Result<CleanupReport>
where
    F: Fn(&Path) -> io::Result<()>,
{
    let mut report = CleanupReport {
        dry_run,
        ..CleanupReport::default()
    };
    let Some(manifest) = pending_manifest_path(destination, backup_dir) else {
        return Ok(report);
    };
    let prefix = backup_prefix(destination);
    let mut retained = Vec::new();
    for entry in read_pending_backups(&manifest).with_context(|| {
        format!(
            "failed to read pending-delete manifest {}",
            manifest.display()
        )
    })? {
        if !path_matches_prefix(&entry.path, prefix.as_deref()) {
            report.discarded.push(DiscardedPending {
                path: entry.path,
                reason: PendingDropReason::OutOfScope,
            });
            continue;
        }
        if is_expired(&entry, policy, now) {
            report.discarded.push(DiscardedPending {
                path: entry.path,
                reason: PendingDropReason::Expired,
            });
            continue;
        }
        if dry_run {
            retained.push(entry);
            continue;
        }
        report.retried.push(entry.path.clone());
        match remove(&entry.path) {
            Ok(()) => report.removed.push(entry.path),
            // The backup disappeared without help: the entry is done.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                report.removed.push(entry.path);
            }
            Err(error) => {
                let mut entry = entry;
                entry.attempts = entry.attempts.saturating_add(1);
                entry.last_attempt = now;
                entry.reason = error.to_string();
                retained.push(entry);
            }
        }
    }
    evict_over_limit(&mut retained, policy, &mut report.discarded);
    if !dry_run {
        write_pending_backups(&manifest, &retained)
            .with_context(|| format!("failed to update {}", manifest.display()))?;
    }
    report.pending = retained;
    Ok(report)
}

/// Merge newly failed removals into the `<binary>.pending-delete` registry.
fn register_pending_backups(
    destination: &Path,
    backup_dir: Option<&Path>,
    policy: &BackupRetry,
    now: SystemTime,
    failures: &[(PathBuf, String)],
) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();
    let Some(manifest) = pending_manifest_path(destination, backup_dir) else {
        return Ok(report);
    };
    let prefix = backup_prefix(destination);
    let mut entries = read_pending_backups(&manifest).with_context(|| {
        format!(
            "failed to read pending-delete manifest {}",
            manifest.display()
        )
    })?;
    for (path, reason) in failures {
        if !path_matches_prefix(path, prefix.as_deref()) {
            continue;
        }
        match entries.iter_mut().find(|entry| entry.path == *path) {
            Some(entry) => {
                entry.attempts = entry.attempts.saturating_add(1);
                entry.last_attempt = now;
                entry.reason.clone_from(reason);
            }
            None => entries.push(PendingBackup {
                path: path.clone(),
                first_failure: now,
                last_attempt: now,
                attempts: 1,
                reason: reason.clone(),
            }),
        }
    }
    let mut retained = Vec::with_capacity(entries.len());
    for entry in entries {
        if is_expired(&entry, policy, now) {
            report.discarded.push(DiscardedPending {
                path: entry.path,
                reason: PendingDropReason::Expired,
            });
            continue;
        }
        retained.push(entry);
    }
    evict_over_limit(&mut retained, policy, &mut report.discarded);
    write_pending_backups(&manifest, &retained)
        .with_context(|| format!("failed to update {}", manifest.display()))?;
    report.pending = retained;
    Ok(report)
}

/// Directory that holds a destination's backups: `backup_dir`, or the
/// destination's own directory.
fn backup_directory(destination: &Path, backup_dir: Option<&Path>) -> PathBuf {
    backup_dir.map(Path::to_path_buf).unwrap_or_else(|| {
        destination
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    })
}

/// `<file name>.old-`, the only names the library treats as backups of
/// `destination`.
fn backup_prefix(destination: &Path) -> Option<String> {
    destination
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{name}.old-"))
}

fn path_matches_prefix(path: &Path, prefix: Option<&str>) -> bool {
    match prefix {
        Some(prefix) => path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(prefix)),
        None => false,
    }
}

/// Suffix of the pending-delete registry of a destination.
const PENDING_MANIFEST_SUFFIX: &str = ".pending-delete";

/// `<backup dir>/<file name>.pending-delete`: one registry per destination.
fn pending_manifest_path(destination: &Path, backup_dir: Option<&Path>) -> Option<PathBuf> {
    let file_name = destination.file_name().and_then(|name| name.to_str())?;
    Some(
        backup_directory(destination, backup_dir)
            .join(format!("{file_name}{PENDING_MANIFEST_SUFFIX}")),
    )
}

fn is_expired(entry: &PendingBackup, policy: &BackupRetry, now: SystemTime) -> bool {
    match now.duration_since(entry.first_failure) {
        Ok(age) => age > policy.max_age,
        // A timestamp in the future (clock skew) is not expired.
        Err(_) => false,
    }
}

fn evict_over_limit(
    retained: &mut Vec<PendingBackup>,
    policy: &BackupRetry,
    discarded: &mut Vec<DiscardedPending>,
) {
    if retained.len() <= policy.max_entries {
        return;
    }
    let mut order: Vec<usize> = (0..retained.len()).collect();
    order.sort_by_key(|index| {
        (
            retained[*index].first_failure,
            retained[*index].path.clone(),
        )
    });
    let overflow = retained.len() - policy.max_entries;
    let mut drop: Vec<usize> = order.into_iter().take(overflow).collect();
    drop.sort_unstable_by(|left, right| right.cmp(left));
    for index in drop {
        let entry = retained.remove(index);
        discarded.push(DiscardedPending {
            path: entry.path,
            reason: PendingDropReason::Evicted,
        });
    }
}

/// Read `<binary>.pending-delete`; a missing file means "nothing pending".
///
/// The format is one record per line, tab-separated:
/// `path`, first failure, last attempt (both seconds since the epoch),
/// attempts, reason. Backslashes, tabs, and newlines are escaped so a Windows
/// path survives the round trip. Corrupt lines are skipped instead of failing
/// the cleanup.
fn read_pending_backups(manifest: &Path) -> io::Result<Vec<PendingBackup>> {
    let text = match fs::read_to_string(manifest) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 5 {
            continue;
        }
        let (Ok(first), Ok(last), Ok(attempts)) = (
            fields[1].parse::<u64>(),
            fields[2].parse::<u64>(),
            fields[3].parse::<u32>(),
        ) else {
            continue;
        };
        entries.push(PendingBackup {
            path: PathBuf::from(unescape_field(fields[0])),
            first_failure: UNIX_EPOCH + Duration::from_secs(first),
            last_attempt: UNIX_EPOCH + Duration::from_secs(last),
            attempts,
            reason: unescape_field(fields[4]),
        });
    }
    Ok(entries)
}

/// Write `<binary>.pending-delete`; an empty registry removes the file.
fn write_pending_backups(manifest: &Path, entries: &[PendingBackup]) -> io::Result<()> {
    if entries.is_empty() {
        return match fs::remove_file(manifest) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    let mut text = String::from(
        "# tracel-deploy pending backup deletions: path, first failure, last attempt, attempts, reason\n",
    );
    for entry in entries {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            escape_field(&entry.path.to_string_lossy()),
            seconds_since_epoch(entry.first_failure),
            seconds_since_epoch(entry.last_attempt),
            entry.attempts,
            escape_field(&entry.reason),
        ));
    }
    let parent = manifest.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    // Write a sibling and rename it over the manifest, so a crash cannot leave
    // a half-written registry behind.
    let temp = pending_temp_path(manifest);
    fs::write(&temp, text)?;
    match fs::rename(&temp, manifest) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

fn pending_temp_path(manifest: &Path) -> PathBuf {
    let file_name = manifest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pending-delete");
    let parent = manifest.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".{file_name}.{}-{}",
        std::process::id(),
        timestamp_nanos()
    ))
}

fn seconds_since_epoch(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|age| age.as_secs())
        .unwrap_or_default()
}

fn escape_field(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn unescape_field(value: &str) -> String {
    let mut unescaped = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            unescaped.push(character);
            continue;
        }
        match characters.next() {
            Some('\\') => unescaped.push('\\'),
            Some('t') => unescaped.push('\t'),
            Some('n') => unescaped.push('\n'),
            Some('r') => unescaped.push('\r'),
            Some(other) => {
                unescaped.push('\\');
                unescaped.push(other);
            }
            None => unescaped.push('\\'),
        }
    }
    unescaped
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
    let dir = backup_directory(destination, backup_dir);
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
        BackupRetry, DiscardedPending, PendingBackup, PendingDropReason, RotateOptions,
        RotateOutcome, RotateStrategy, cleanup_backups_with, cleanup_pending_backups,
        delete_backup, pending_manifest_path, read_pending_backups, register_pending_backups,
        rename_aside_replace_with, retry_pending_backups, rotate, rotate_with_remover,
        write_pending_backups,
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

    fn manifest_path(destination: &Path) -> PathBuf {
        pending_manifest_path(destination, None).expect("manifest path")
    }

    fn pending_entry(path: &Path, failed_at: SystemTime, attempts: u32) -> PendingBackup {
        PendingBackup {
            path: path.to_path_buf(),
            first_failure: failed_at,
            last_attempt: failed_at,
            attempts,
            reason: "still in use".to_owned(),
        }
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
            backup_cleanup_retry: None,
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
            backup_cleanup_retry: None,
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
            backup_cleanup_retry: None,
        })
        .expect("rotate");

        assert_eq!(backup_names(&dir).len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_backup_removal_is_registered_and_rotate_succeeds() {
        let dir = temp_dir("pending-register");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");
        let backup = dir.join("destination.old-1-1");
        fs::write(&backup, "backup").expect("backup");
        // The pre-existing backup is older than the one this rotation makes,
        // so `backup_retention: Some(1)` selects it for removal.
        touch_aged(&backup, Duration::from_secs(3_600));

        let outcome = rotate_with_remover(
            &RotateOptions {
                source,
                destination: destination.clone(),
                backup_dir: None,
                strategy: RotateStrategy::RenameAside,
                dry_run: false,
                backup_retention: Some(1),
                backup_max_age: None,
                backup_cleanup_retry: Some(BackupRetry::default()),
            },
            |_path| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "still in use",
                ))
            },
        )
        .expect("a failing backup deletion must not fail the rotation");

        assert!(
            matches!(outcome, RotateOutcome::ReplacedAfterRename { .. }),
            "outcome: {outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(&destination).expect("destination"),
            "new",
            "the replacement is installed"
        );
        assert!(backup.is_file(), "the occupied backup stays in place");

        let entries = read_pending_backups(&manifest_path(&destination)).expect("read manifest");
        assert_eq!(entries.len(), 1, "entries: {entries:?}");
        assert_eq!(entries[0].path, backup);
        assert_eq!(entries[0].attempts, 1, "one failed attempt is recorded");
        assert!(
            entries[0].reason.contains("still in use"),
            "the reason records the failed removal: {:?}",
            entries[0].reason
        );
        assert!(
            entries[0]
                .last_attempt
                .duration_since(entries[0].first_failure)
                .expect("clock")
                < Duration::from_secs(1),
            "the first failure is recorded: {entries:?}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pending_registry_lives_in_the_backup_directory() {
        let dir = temp_dir("pending-backup-dir");
        let backup_dir = dir.join("state");
        fs::create_dir_all(&backup_dir).expect("backup dir");
        let destination = dir.join("destination");
        fs::write(&destination, "old").expect("destination");
        let backup = backup_dir.join("destination.old-1-1");
        fs::write(&backup, "backup").expect("backup");
        let now = SystemTime::now();

        register_pending_backups(
            &destination,
            Some(&backup_dir),
            &BackupRetry::default(),
            now,
            &[(backup.clone(), "still in use".to_owned())],
        )
        .expect("register");

        let manifest =
            pending_manifest_path(&destination, Some(&backup_dir)).expect("manifest path");
        assert_eq!(manifest.parent(), Some(backup_dir.as_path()));
        assert!(manifest.is_file(), "the registry follows the backup dir");
        assert!(
            !pending_manifest_path(&destination, None)
                .expect("manifest path")
                .exists(),
            "nothing is written next to the destination"
        );

        let report = cleanup_pending_backups(&RotateOptions {
            source: dir.join("source"),
            destination: destination.clone(),
            backup_dir: Some(backup_dir.clone()),
            strategy: RotateStrategy::Auto,
            dry_run: false,
            backup_retention: None,
            backup_max_age: None,
            backup_cleanup_retry: Some(BackupRetry::default()),
        })
        .expect("cleanup");

        assert_eq!(report.removed, vec![backup.clone()]);
        assert!(!backup.exists(), "the registered backup is deleted");
        assert!(!manifest.exists(), "the empty registry is removed");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pending_backups_are_retried_by_the_next_cleanup() {
        let dir = temp_dir("pending-retry");
        let destination = dir.join("destination");
        fs::write(&destination, "old").expect("destination");
        let backup = dir.join("destination.old-1-1");
        // A registered backup that already disappeared must be unregistered
        // without another attempt.
        let missing = dir.join("destination.old-9-9");
        fs::write(&backup, "backup").expect("backup");
        let manifest = manifest_path(&destination);
        let now = SystemTime::now();
        write_pending_backups(
            &manifest,
            &[
                pending_entry(&backup, now, 2),
                pending_entry(&missing, now, 1),
            ],
        )
        .expect("write manifest");

        let report = cleanup_pending_backups(&RotateOptions {
            source: dir.join("source"),
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: false,
            backup_retention: Some(3),
            backup_max_age: None,
            backup_cleanup_retry: Some(BackupRetry::default()),
        })
        .expect("cleanup");

        assert_eq!(report.retried, vec![backup.clone(), missing.clone()]);
        assert_eq!(report.removed, vec![backup.clone(), missing.clone()]);
        assert!(report.pending.is_empty(), "pending: {:?}", report.pending);
        assert!(
            report.discarded.is_empty(),
            "discarded: {:?}",
            report.discarded
        );
        assert!(!backup.exists(), "the released backup is deleted");
        assert!(!missing.exists(), "the vanished backup stays gone");
        assert!(!manifest.exists(), "an empty registry removes the manifest");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_next_rotation_retries_registered_backups() {
        let dir = temp_dir("pending-next-rotate");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");
        let backup = dir.join("destination.old-1-1");
        fs::write(&backup, "backup").expect("backup");
        touch_aged(&backup, Duration::from_secs(3_600));
        let manifest = manifest_path(&destination);
        write_pending_backups(&manifest, &[pending_entry(&backup, SystemTime::now(), 1)])
            .expect("write manifest");

        rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: false,
            backup_retention: Some(1),
            backup_max_age: None,
            backup_cleanup_retry: Some(BackupRetry::default()),
        })
        .expect("rotate");

        assert!(
            !backup.exists(),
            "the next rotation retries the registered backup"
        );
        assert!(
            !manifest.exists(),
            "the registry is gone once the backup is deleted"
        );
        assert!(destination.is_file(), "the rotation itself still succeeded");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn expired_pending_entries_are_discarded_without_an_attempt() {
        let dir = temp_dir("pending-expired");
        let destination = dir.join("destination");
        fs::write(&destination, "old").expect("destination");
        let expired = dir.join("destination.old-1-1");
        let fresh = dir.join("destination.old-2-2");
        fs::write(&expired, "backup").expect("expired backup");
        fs::write(&fresh, "backup").expect("fresh backup");
        let now = SystemTime::now();
        let manifest = manifest_path(&destination);
        write_pending_backups(
            &manifest,
            &[
                pending_entry(&expired, now - Duration::from_secs(8 * 24 * 3_600), 4),
                pending_entry(&fresh, now - Duration::from_secs(3_600), 1),
            ],
        )
        .expect("write manifest");

        let attempted = std::cell::RefCell::new(Vec::new());
        let report = retry_pending_backups(
            &destination,
            None,
            &BackupRetry::default(),
            now,
            false,
            &|path: &Path| {
                attempted.borrow_mut().push(path.to_path_buf());
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "still in use",
                ))
            },
        )
        .expect("cleanup");

        assert_eq!(
            attempted.into_inner(),
            vec![fresh.clone()],
            "an expired entry is not retried"
        );
        assert!(report.removed.is_empty(), "removed: {:?}", report.removed);
        assert_eq!(
            report.discarded,
            vec![DiscardedPending {
                path: expired.clone(),
                reason: PendingDropReason::Expired,
            }],
            "the default seven-day expiry drops the old entry"
        );
        assert_eq!(report.pending.len(), 1);
        assert_eq!(report.pending[0].path, fresh);
        assert_eq!(report.pending[0].attempts, 2, "the retry is recorded");
        assert!(expired.is_file() && fresh.is_file(), "no file was deleted");
        assert_eq!(
            read_pending_backups(&manifest).expect("manifest").len(),
            1,
            "the expired entry is dropped from the registry"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pending_cleanup_leaves_other_binaries_alone() {
        let dir = temp_dir("pending-scope");
        let destination = dir.join("destination");
        fs::write(&destination, "old").expect("destination");
        let ours = dir.join("destination.old-1-1");
        let other = dir.join("other.old-1-1");
        fs::write(&ours, "backup").expect("our backup");
        fs::write(&other, "other backup").expect("other backup");
        let now = SystemTime::now();
        write_pending_backups(
            &manifest_path(&destination),
            &[pending_entry(&ours, now, 1), pending_entry(&other, now, 1)],
        )
        .expect("write manifest");

        let report = retry_pending_backups(
            &destination,
            None,
            &BackupRetry::default(),
            now,
            false,
            &delete_backup,
        )
        .expect("cleanup");

        assert_eq!(report.removed, vec![ours.clone()]);
        assert_eq!(
            report.discarded,
            vec![DiscardedPending {
                path: other.clone(),
                reason: PendingDropReason::OutOfScope,
            }],
            "an entry outside the destination prefix is dropped, not deleted"
        );
        assert!(!ours.exists(), "the destination's own backup is deleted");
        assert!(other.is_file(), "another binary's backup is never touched");
        assert!(report.pending.is_empty(), "pending: {:?}", report.pending);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pending_entries_beyond_the_limit_are_evicted() {
        let dir = temp_dir("pending-evict");
        let destination = dir.join("destination");
        fs::write(&destination, "old").expect("destination");
        let older = dir.join("destination.old-1-1");
        let newer = dir.join("destination.old-2-2");
        let now = SystemTime::now();
        let manifest = manifest_path(&destination);
        write_pending_backups(
            &manifest,
            &[
                pending_entry(&newer, now, 1),
                pending_entry(&older, now - Duration::from_secs(60), 1),
            ],
        )
        .expect("write manifest");

        let report = retry_pending_backups(
            &destination,
            None,
            &BackupRetry::default().with_max_entries(1),
            now,
            false,
            &|_path: &Path| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "still in use",
                ))
            },
        )
        .expect("cleanup");

        assert_eq!(report.retried.len(), 2, "retried: {:?}", report.retried);
        assert_eq!(
            report.discarded,
            vec![DiscardedPending {
                path: older.clone(),
                reason: PendingDropReason::Evicted,
            }],
            "the oldest entry goes away when the registry is full"
        );
        assert_eq!(report.pending.len(), 1);
        assert_eq!(report.pending[0].path, newer);
        let stored = read_pending_backups(&manifest).expect("manifest");
        assert_eq!(stored.len(), 1, "the registry keeps only the newest entry");
        assert_eq!(stored[0].path, newer);
        assert_eq!(stored[0].attempts, 2, "the retry is stored: {stored:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dry_run_neither_registers_nor_deletes_pending_backups() {
        let dir = temp_dir("pending-dry-run");
        let source = dir.join("source");
        let destination = dir.join("destination");
        fs::write(&source, "new").expect("source");
        fs::write(&destination, "old").expect("destination");
        let backup = dir.join("destination.old-1-1");
        fs::write(&backup, "backup").expect("backup");

        rotate(&RotateOptions {
            source,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: true,
            backup_retention: Some(0),
            backup_max_age: None,
            backup_cleanup_retry: Some(BackupRetry::default()),
        })
        .expect("dry run");

        let manifest = manifest_path(&destination);
        assert!(backup.is_file(), "a dry run does not delete backups");
        assert!(!manifest.exists(), "a dry run does not write a registry");

        let now = SystemTime::now();
        write_pending_backups(&manifest, &[pending_entry(&backup, now, 1)])
            .expect("write manifest");
        let report = cleanup_pending_backups(&RotateOptions {
            source: dir.join("source"),
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: true,
            backup_retention: Some(0),
            backup_max_age: None,
            backup_cleanup_retry: Some(BackupRetry::default()),
        })
        .expect("dry-run cleanup");

        assert!(report.dry_run, "the report marks the dry run");
        assert!(
            report.retried.is_empty() && report.removed.is_empty(),
            "a dry run attempts nothing: {report:?}"
        );
        assert_eq!(report.pending.len(), 1, "the entry is reported as pending");
        assert!(
            backup.is_file(),
            "a dry-run cleanup does not delete the backup"
        );
        assert_eq!(
            read_pending_backups(&manifest).expect("manifest").len(),
            1,
            "a dry-run cleanup does not rewrite the registry"
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// End-to-end acceptance of the Windows scenario the ticket is about: the
    /// old image of a *running* process cannot be deleted, so the rotation
    /// registers it, and a later cleanup pass deletes it once the process is
    /// gone.
    #[cfg(windows)]
    #[test]
    fn pending_cleanup_deletes_a_backup_after_the_process_exits() {
        let dir = temp_dir("pending-running");
        let mut child = stage_running_binary(&dir).expect("spawn the running binary");
        let destination = dir.join("holder.exe");
        let replacement = dir.join("replacement.exe");
        fs::write(&replacement, "replacement").expect("write replacement");

        let options = RotateOptions {
            source: replacement,
            destination: destination.clone(),
            backup_dir: None,
            strategy: RotateStrategy::Auto,
            dry_run: false,
            backup_retention: Some(0),
            backup_max_age: None,
            backup_cleanup_retry: Some(BackupRetry::default()),
        };
        let outcome = rotate(&options).expect("rotate a running binary");
        let RotateOutcome::ReplacedAfterRename { backup, .. } = outcome else {
            panic!("expected a rename-aside rotation, got {outcome:?}");
        };
        assert!(
            backup.is_file(),
            "the image of the running process cannot be deleted yet"
        );
        let stored = read_pending_backups(&manifest_path(&destination)).expect("read manifest");
        assert_eq!(stored.len(), 1, "registered: {stored:?}");
        assert_eq!(stored[0].path, backup);
        assert_eq!(stored[0].attempts, 1);

        let _ = child.kill();
        let _ = child.wait();
        // Windows releases the mapping asynchronously; retry briefly before
        // declaring the deferred cleanup broken.
        for _ in 0..50 {
            if cleanup_pending_backups(&options)
                .expect("cleanup")
                .removed
                .is_empty()
            {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            break;
        }

        assert!(!backup.exists(), "the deferred cleanup deletes the backup");
        assert!(
            !manifest_path(&destination).exists(),
            "the empty registry is removed"
        );
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
