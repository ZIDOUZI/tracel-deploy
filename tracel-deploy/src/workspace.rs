//! Where the workspace a checkout deploy works on lives.
//!
//! A hook runs `cargo run --manifest-path <xtask>/Cargo.toml -- deploy
//! --background` from whatever directory the agent happens to be in, so the root
//! cannot simply be read off the current directory. [`WorkspaceRoot::detect`]
//! tries these sources in order, and the explicit one always wins:
//!
//! 1. `--workspace <dir>` (or the API's explicit root),
//! 2. the consumer's manifest directory, which the `deploy_commands` macro bakes
//!    in as `env!("CARGO_MANIFEST_DIR")` - the library cannot know it, because
//!    `env!` inside the library expands to the library's own directory,
//! 3. the `CARGO_MANIFEST_DIR` the environment carries (`cargo run` sets it, and
//!    the queued child inherits it),
//! 4. the current directory and its ancestors: the nearest `Cargo.toml` with a
//!    `[workspace]` section (the queue starts the child in the workspace root),
//! 5. an error that names the flag to pass.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};

/// A resolved workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// Detect the workspace root; see the [module docs](self) for the order.
    pub fn detect(hint: DetectHint<'_>) -> Result<Self> {
        if let Some(explicit) = hint.explicit {
            return Self::from_explicit(explicit);
        }
        for candidate in [
            hint.manifest_dir,
            env::var_os("CARGO_MANIFEST_DIR").as_deref().map(Path::new),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(root) = find_workspace_root(candidate) {
                return Ok(Self(root));
            }
            // A manifest directory with no workspace above it: the convention
            // is that the xtask crate lives in `<root>/xtask`.
            if let Some(parent) = candidate.parent() {
                return Ok(Self(parent.to_path_buf()));
            }
        }
        if let Ok(current) = env::current_dir()
            && let Some(root) = find_workspace_root(&current)
        {
            return Ok(Self(root));
        }
        bail!(
            "could not detect the workspace root (searched from {} for a Cargo.toml with a [workspace] section); pass `--workspace <dir>`",
            env::current_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|_| "the current directory".to_owned())
        )
    }

    /// The root a caller named explicitly; it has to look like a workspace.
    fn from_explicit(dir: &Path) -> Result<Self> {
        if !dir.is_dir() {
            bail!("`--workspace {}` is not a directory", dir.display());
        }
        if !dir.join("Cargo.toml").is_file() {
            bail!("`--workspace {}` has no Cargo.toml", dir.display());
        }
        Ok(Self(dir.to_path_buf()))
    }

    /// The root.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The root, by value.
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for WorkspaceRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// What a caller can tell [`WorkspaceRoot::detect`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DetectHint<'a> {
    explicit: Option<&'a Path>,
    manifest_dir: Option<&'a Path>,
}

impl<'a> DetectHint<'a> {
    /// A hint carrying the explicit root (the `--workspace` flag), when there is
    /// one.
    pub fn new(explicit: Option<&'a Path>) -> Self {
        Self {
            explicit,
            manifest_dir: None,
        }
    }

    /// Add the manifest directory of the crate that owns the deploy command -
    /// what the generated dispatcher passes as `env!("CARGO_MANIFEST_DIR")`.
    pub fn manifest_dir(mut self, manifest_dir: &'a Path) -> Self {
        self.manifest_dir = Some(manifest_dir);
        self
    }
}

/// The nearest ancestor of `start` (itself included) whose `Cargo.toml`
/// declares a `[workspace]`.
fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let start = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    start
        .ancestors()
        .find(|dir| {
            let manifest = dir.join("Cargo.toml");
            manifest.is_file() && declares_workspace(&manifest)
        })
        .map(Path::to_path_buf)
}

/// Whether a manifest declares a `[workspace]` section.
///
/// A line scan is enough, and keeps the crate free of a TOML dependency: the
/// section header is what a workspace manifest has and a package manifest does
/// not.
fn declares_workspace(manifest: &Path) -> bool {
    fs::read_to_string(manifest)
        .map(|text| text.lines().any(|line| line.trim() == "[workspace]"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = env::temp_dir().join(format!("tracel-deploy-workspace-{label}-{unique}"));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_manifest(dir: &Path, body: &str) {
        fs::create_dir_all(dir).expect("dir");
        fs::write(dir.join("Cargo.toml"), body).expect("manifest");
    }

    #[test]
    fn the_explicit_root_wins() {
        let dir = temp_dir("explicit");
        write_manifest(&dir, "[package]\nname = \"app\"\n");
        let root = WorkspaceRoot::detect(DetectHint::new(Some(&dir))).expect("explicit root");
        assert_eq!(root.path(), dir);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_explicit_root_has_to_look_like_a_workspace() {
        let dir = temp_dir("explicit-bad");
        let error = WorkspaceRoot::detect(DetectHint::new(Some(&dir))).expect_err("no manifest");
        assert!(
            format!("{error:#}").contains("has no Cargo.toml"),
            "error: {error:#}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_manifest_dir_hint_walks_up_to_the_workspace() {
        let root = temp_dir("manifest");
        write_manifest(&root, "[workspace]\nmembers = [\"xtask\"]\n");
        let xtask = root.join("xtask");
        write_manifest(&xtask, "[package]\nname = \"xtask\"\n");

        let detected =
            WorkspaceRoot::detect(DetectHint::new(None).manifest_dir(&xtask)).expect("hint");
        assert_eq!(detected.path(), root);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_manifest_dir_without_a_workspace_falls_back_to_its_parent() {
        let root = temp_dir("manifest-parent");
        let xtask = root.join("xtask");
        write_manifest(&xtask, "[package]\nname = \"xtask\"\n");

        let detected =
            WorkspaceRoot::detect(DetectHint::new(None).manifest_dir(&xtask)).expect("hint");
        assert_eq!(detected.path(), root, "the xtask lives in <root>/xtask");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_current_directory_is_the_last_resort() {
        // The test runs inside the crate, so the walk up finds this repository's
        // workspace manifest.
        let detected = WorkspaceRoot::detect(DetectHint::new(None)).expect("detection");
        assert!(
            declares_workspace(&detected.path().join("Cargo.toml")),
            "detected {} is a workspace",
            detected.path().display()
        );
    }
}
