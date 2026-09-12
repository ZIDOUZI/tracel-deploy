//! Small, dependency-free filesystem helpers shared by deploy loops: atomic
//! state files and platform-aware path comparison.
//!
//! Consumers of `tracel-deploy` used to copy the same three patterns into every
//! xtask (a `.tmp` + rename state write, a `\\?\`-stripping path comparison and
//! an install path resolution). This module is the single implementation:
//!
//! - [`write_state_file`] stages `<dir>/<name>.tmp` next to the target and
//!   renames it into place, so a reader never observes a half written file and
//!   a failed write leaves the previous contents intact.
//! - [`read_state_file`] reports a missing file as `Ok(None)` instead of an
//!   error, because "no state yet" is a normal condition for a deploy loop.
//! - [`remove_state_file`] is the matching idempotent cleanup.
//! - [`same_file_path`] answers "is this the installed binary?" the way the
//!   platform reports paths (Windows `\\?\` prefixes, `/` vs `\` and case).
//! - [`resolve_install_path`] turns an `--install-to` argument into the final
//!   binary path.
//!
//! The state file format is owned by the caller: these helpers move bytes and
//! never parse them. The module uses `std` only.

use std::{
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};

use crate::context::binary_name_with_extension;

/// Atomically write `contents` to `<dir>/<name>` and return the written path.
///
/// The directory is created when it is missing. The contents are first written
/// to `<dir>/<name>.tmp` and then renamed over `<dir>/<name>`, so a concurrent
/// reader sees either the old file or the complete new one, never a truncated
/// mix of both. On Windows the rename replaces the existing file as well, which
/// is what makes overwriting a state file safe there.
///
/// A failed write removes the temporary file again and reports the underlying
/// IO error with the offending path attached.
pub fn write_state_file(dir: &Path, name: &str, contents: &str) -> Result<PathBuf> {
    fs::create_dir_all(dir)
        .with_context(|| format!("could not create state dir {}", dir.display()))?;
    let path = dir.join(name);
    let temporary = temporary_path(&path);

    if let Err(error) = fs::write(&temporary, contents) {
        return Err(error)
            .with_context(|| format!("could not write state {}", temporary.display()));
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("could not replace state {}", path.display()));
    }
    Ok(path)
}

/// `<path>` with `.tmp` appended to the whole file name, so `router.json` is
/// staged as `router.json.tmp` in the same directory.
fn temporary_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".tmp");
    PathBuf::from(value)
}

/// Read `<dir>/<name>` as UTF-8 text.
///
/// A missing file is not an error: `Ok(None)` is returned when the state file
/// does not exist yet (or was removed again), while other IO errors, including
/// non UTF-8 contents, are reported with the path attached. An existing but
/// empty file yields `Some(String::new())`, so callers that expect JSON should
/// treat an empty string as "no state".
pub fn read_state_file(dir: &Path, name: &str) -> Result<Option<String>> {
    let path = dir.join(name);
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("could not read state {}", path.display()))
        }
    }
}

/// Delete `<dir>/<name>`, ignoring a file that is not there.
///
/// The cleanup is idempotent, so a deploy loop can call it unconditionally on
/// both the success and the failure path.
pub fn remove_state_file(dir: &Path, name: &str) {
    let _ = fs::remove_file(dir.join(name));
}

/// Whether both paths name the same location, judged from the path text alone.
///
/// - Windows: a verbatim `\\?\` prefix is stripped (`\\?\UNC\server\share` is
///   reduced to `\\server\share`, the same reduction `dunce` performs), `/` and
///   `\` are treated as the same separator, and the comparison is
///   case-insensitive, matching the casing the OS reports for a process image.
///   Trailing separators are ignored, except on a drive root so that `C:\` does
///   not compare equal to the drive-relative `C:`.
/// - Other platforms: `\` is an ordinary character, separators are normalized
///   and the comparison stays byte-wise and case-sensitive, so `/tmp/A` and
///   `/tmp/a` are different files.
///
/// The comparison is purely textual: no filesystem access happens, so neither
/// symlinks nor `.`/`..` components are resolved. Callers that need the
/// identity of a file should canonicalize first.
pub fn same_file_path(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

/// Comparison key for a path, per platform. See [`same_file_path`].
#[cfg(windows)]
fn path_key(path: &Path) -> String {
    let value = path.to_string_lossy().replace('/', "\\");
    // `dunce`-style reduction of the verbatim prefix, done on the string only:
    // `\\?\C:\x` becomes `C:\x` and `\\?\UNC\server\share` becomes
    // `\\server\share`.
    let simplified = match value.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(share) => format!("\\\\{share}"),
            None => rest.to_owned(),
        },
        None => value,
    };
    // Trailing separators carry no meaning, except on a drive root: `C:\` has
    // to stay different from the drive-relative `C:`.
    let trimmed = simplified.trim_end_matches('\\');
    let key = if trimmed.is_empty() {
        "\\"
    } else if trimmed.len() < simplified.len() && trimmed.ends_with(':') {
        &simplified[..trimmed.len() + 1]
    } else {
        trimmed
    };
    key.to_ascii_lowercase()
}

/// Comparison key for a path, per platform. See [`same_file_path`].
#[cfg(not(windows))]
fn path_key(path: &Path) -> PathBuf {
    // Separators are already `/`, duplicate and trailing ones are dropped by
    // the component collection, and the bytes stay case-sensitive.
    path.components().collect()
}

/// Resolve where a deployed binary is installed.
///
/// - `Some(path)` naming a directory -> `<path>/<binary>`. A path counts as a
///   directory when it exists as one, ends with a path separator, or has no
///   extension (`C:\tools\widgets` is a directory even before it exists).
/// - `Some(path)` naming a file -> used as is. A path counts as a file when it
///   exists as one or carries an extension, so an extensionless runner such as
///   `/usr/local/bin/runner` is left alone.
/// - `None` -> `<cargo home>/bin/<binary>`, where the cargo home is
///   `CARGO_HOME` when set and `<USERPROFILE|HOME>/.cargo` otherwise. When
///   neither variable is usable the fallback is
///   `<current working directory>/bin/<binary>`; if even the current directory
///   cannot be read, the relative path `bin/<binary>` is returned.
///
/// The platform binary extension is appended when the caller did not supply
/// one, so `widget` becomes `widget.exe` on Windows while an explicit
/// `widget.exe` is left unchanged.
pub fn resolve_install_path(install_to: Option<&Path>, binary: &str) -> PathBuf {
    let name = binary_name_with_extension(binary);
    match install_to {
        None => default_install_dir().join(name),
        Some(path) if is_directory_target(path) => path.join(name),
        Some(path) => path.to_path_buf(),
    }
}

/// Whether an `--install-to` argument names a directory. See
/// [`resolve_install_path`] for the exact rules.
fn is_directory_target(path: &Path) -> bool {
    if path.is_dir() {
        return true;
    }
    if path.is_file() {
        return false;
    }
    let value = path.to_string_lossy();
    if value.ends_with(std::path::MAIN_SEPARATOR) || (cfg!(windows) && value.ends_with('/')) {
        return true;
    }
    path.extension().is_none()
}

/// `<cargo home>/bin`, see [`resolve_install_path`] for the fallback rule.
fn default_install_dir() -> PathBuf {
    match cargo_home_dir() {
        Some(home) => home.join("bin"),
        None => env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("bin"),
    }
}

/// `CARGO_HOME` when set, else `<USERPROFILE|HOME>/.cargo`, else `None`.
///
/// Mirrors the cargo home part of [`crate::context::default_cargo_bin_dir`];
/// the difference is the documented current-directory fallback instead of a
/// path relative to an empty home directory.
fn cargo_home_dir() -> Option<PathBuf> {
    if let Some(home) = non_empty_env("CARGO_HOME") {
        return Some(PathBuf::from(home));
    }
    non_empty_env("USERPROFILE")
        .or_else(|| non_empty_env("HOME"))
        .map(|home| PathBuf::from(home).join(".cargo"))
}

fn non_empty_env(name: &str) -> Option<OsString> {
    env::var_os(name).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        read_state_file, remove_state_file, resolve_install_path, same_file_path, write_state_file,
    };

    /// Unique temp directory per test, so the tests stay parallel-safe.
    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "tracel-deploy-state-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    /// `binary` with the platform extension, for expectations.
    fn expected_name(binary: &str) -> String {
        if cfg!(windows) {
            format!("{binary}.exe")
        } else {
            binary.to_owned()
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = temp_dir("round-trip");
        let path = write_state_file(&dir, "router.json", "{\"pid\":42}").expect("write");
        assert_eq!(path, dir.join("router.json"));
        assert!(path.is_file(), "state file missing: {}", path.display());
        assert_eq!(
            read_state_file(&dir, "router.json").expect("read"),
            Some("{\"pid\":42}".to_owned())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrite_replaces_contents_without_leaving_a_temporary_file() {
        let dir = temp_dir("overwrite");
        write_state_file(&dir, "router.json", "first").expect("write");
        write_state_file(&dir, "router.json", "second").expect("rewrite");
        assert_eq!(
            read_state_file(&dir, "router.json").expect("read"),
            Some("second".to_owned())
        );
        let temporary = dir.join("router.json.tmp");
        assert!(
            !temporary.exists(),
            "temporary file left behind: {}",
            temporary.display()
        );
        assert_eq!(fs::read_dir(&dir).expect("read dir").count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_reads_as_none() {
        let dir = temp_dir("missing");
        assert_eq!(read_state_file(&dir, "absent.json").expect("read"), None);
        let absent_dir = dir.join("not-created");
        assert_eq!(
            read_state_file(&absent_dir, "absent.json").expect("read"),
            None
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_creates_missing_directories() {
        let root = temp_dir("mkdir");
        let dir = root.join("nested").join("state");
        let path = write_state_file(&dir, "status", "ok").expect("write");
        assert!(path.is_file());
        assert_eq!(
            read_state_file(&dir, "status").expect("read"),
            Some("ok".to_owned())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_replace_cleans_the_temporary_file() {
        let dir = temp_dir("failed-replace");
        // A directory in place of the state file makes the final rename fail.
        fs::create_dir(dir.join("status")).expect("create blocking dir");
        let error = write_state_file(&dir, "status", "ok").expect_err("rename must fail");
        assert!(
            error.to_string().contains("could not replace state"),
            "unexpected error: {error}"
        );
        let temporary = dir.join("status.tmp");
        assert!(
            !temporary.exists(),
            "temporary file left behind: {}",
            temporary.display()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_is_idempotent_and_forgets_the_file() {
        let dir = temp_dir("remove");
        write_state_file(&dir, "status", "ok").expect("write");
        remove_state_file(&dir, "status");
        assert_eq!(read_state_file(&dir, "status").expect("read"), None);
        // A second removal of an already missing file is a no-op.
        remove_state_file(&dir, "status");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(windows)]
    fn same_file_path_matches_windows_case_and_separator_differences() {
        assert!(same_file_path(
            Path::new(r"\\?\C:\A\b.exe"),
            Path::new("c:/a/B.EXE")
        ));
        assert!(same_file_path(
            Path::new(r"\\?\C:\A\b.exe"),
            Path::new(r"C:\A\B.exe\")
        ));
        assert!(same_file_path(
            Path::new(r"\\?\UNC\server\share\tool.exe"),
            Path::new(r"\\server\Share\TOOL.EXE")
        ));
    }

    #[test]
    #[cfg(windows)]
    fn same_file_path_rejects_different_windows_paths() {
        assert!(!same_file_path(
            Path::new(r"C:\A\b.exe"),
            Path::new(r"C:\A\c.exe")
        ));
        assert!(!same_file_path(
            Path::new(r"C:\A\b.exe"),
            Path::new(r"D:\A\b.exe")
        ));
        // A drive root is not the drive-relative path.
        assert!(!same_file_path(Path::new(r"C:\"), Path::new("C:")));
    }

    #[test]
    #[cfg(not(windows))]
    fn same_file_path_is_case_sensitive_off_windows() {
        assert!(!same_file_path(
            Path::new("/tmp/Tracel/tool"),
            Path::new("/tmp/tracel/tool")
        ));
        assert!(same_file_path(
            Path::new("/tmp/tracel/tool"),
            Path::new("/tmp/tracel/tool")
        ));
        // Separator noise is ignored: paths are compared by component.
        assert!(same_file_path(
            Path::new("/tmp/tracel/tool/"),
            Path::new("//tmp/tracel/./tool")
        ));
        // A backslash is an ordinary character, not a separator.
        assert!(!same_file_path(
            Path::new("/tmp/tracel/tool"),
            Path::new(r"/tmp\tracel\tool")
        ));
    }

    #[test]
    fn resolve_install_path_appends_the_binary_to_a_directory() {
        let dir = temp_dir("install-dir");
        assert_eq!(
            resolve_install_path(Some(&dir), "widget"),
            dir.join(expected_name("widget"))
        );
        // A directory that does not exist yet is recognised by the missing
        // extension and by a trailing separator.
        let missing = dir.join("bin").join("widgets");
        assert_eq!(
            resolve_install_path(Some(&missing), "widget"),
            missing.join(expected_name("widget"))
        );
        assert_eq!(
            resolve_install_path(Some(&dir.join("bin").join("widgets/")), "widget"),
            dir.join("bin")
                .join("widgets/")
                .join(expected_name("widget"))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_install_path_keeps_a_file_path() {
        let dir = temp_dir("install-file");
        let file = dir.join("custom-widget");
        fs::write(&file, b"stub").expect("write stub");
        // An existing file without an extension is used verbatim.
        assert_eq!(resolve_install_path(Some(&file), "widget"), file);
        // A path with an extension does not have to exist yet.
        let missing = dir.join("other-widget.exe");
        assert_eq!(resolve_install_path(Some(&missing), "widget"), missing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_install_path_does_not_repeat_the_extension() {
        let dir = temp_dir("install-extension");
        let name = expected_name("widget");
        assert_eq!(resolve_install_path(Some(&dir), &name), dir.join(&name));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_install_path_defaults_to_the_cargo_bin_directory() {
        let name = expected_name("tracel-deploy-state-probe");
        let resolved = resolve_install_path(None, "tracel-deploy-state-probe");
        assert_eq!(
            resolved.file_name().and_then(|value| value.to_str()),
            Some(name.as_str())
        );
        assert_eq!(
            resolved
                .parent()
                .and_then(|value| value.file_name())
                .and_then(|value| value.to_str()),
            Some("bin")
        );
        match env::var_os("CARGO_HOME").filter(|value| !value.is_empty()) {
            // With a cargo home the target is `<CARGO_HOME>/bin/<binary>`.
            Some(home) => assert_eq!(resolved, PathBuf::from(home).join("bin").join(&name)),
            // Without one (and without a home directory) the documented
            // fallback is the current directory, which is still absolute.
            None => assert!(
                resolved.is_absolute(),
                "expected an absolute path, got {}",
                resolved.display()
            ),
        }
    }
}
