use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};

/// How a running binary should be rotated out of the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RotateStrategy {
    /// Best platform-specific behaviour: atomic rename on Unix, copy with a
    /// rename-aside fallback on Windows.
    #[default]
    Auto,
    /// Try an in-place copy first.
    Replace,
    /// Use a sibling rename on Unix; on Windows fall back to rename-aside.
    Atomic,
    /// Rename the destination aside first, then copy the replacement.
    RenameAside,
}

#[derive(Debug, Clone)]
pub struct RotateOptions {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub backup_dir: Option<PathBuf>,
    pub strategy: RotateStrategy,
    pub dry_run: bool,
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
        return Ok(RotateOutcome::Replaced { destination });
    }

    match options.strategy {
        RotateStrategy::Replace => replace_in_place(options, &destination),
        RotateStrategy::Atomic => atomic_replace(options, &destination),
        RotateStrategy::RenameAside => rename_aside_replace(options, &destination),
        RotateStrategy::Auto => {
            #[cfg(windows)]
            {
                replace_in_place(options, &destination)
            }
            #[cfg(not(windows))]
            {
                atomic_replace(options, &destination)
            }
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
    match copy_binary(&options.source, destination) {
        Ok(()) => Ok(RotateOutcome::ReplacedAfterRename {
            destination: destination.to_path_buf(),
            backup,
        }),
        Err(error) => {
            let _ = fs::rename(&backup, destination);
            Err(error).with_context(|| {
                format!(
                    "failed to install {} after renaming the old binary aside",
                    destination.display()
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
        matches!(error.raw_os_error(), Some(5 | 32 | 33))
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

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
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{RotateOptions, RotateOutcome, RotateStrategy, rotate};

    #[cfg(unix)]
    use std::io::Read;

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
}
