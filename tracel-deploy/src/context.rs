use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result};

use crate::command::DeployCmdArgs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Debug,
    Release,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeployContext {
    pub workspace_root: PathBuf,
    pub package: String,
    pub binary_name: String,
    pub release: bool,
    pub dry_run: bool,
    pub force: bool,
    pub quiet: bool,
    pub source_binary: Option<PathBuf>,
    pub install_path: PathBuf,
    pub backup_dir: Option<PathBuf>,
    /// Keep at most this many `<binary>.old-*` backups (newest first).
    pub backup_retention: Option<usize>,
    /// Remove `<binary>.old-*` backups older than this age.
    pub backup_max_age: Option<Duration>,
    pub state_dir: PathBuf,
    pub build_args: Vec<String>,
}

impl DeployContext {
    /// Build a context with platform defaults.
    pub fn new(workspace_root: impl Into<PathBuf>, package: impl Into<String>) -> Self {
        let workspace_root = workspace_root.into();
        let package = package.into();
        let binary_name = package.clone();
        let install_path = default_cargo_bin_dir().join(binary_name_with_extension(&binary_name));
        Self {
            workspace_root,
            package,
            binary_name,
            release: false,
            dry_run: false,
            force: false,
            quiet: true,
            source_binary: None,
            install_path,
            backup_dir: None,
            backup_retention: None,
            backup_max_age: None,
            state_dir: default_state_dir(),
            build_args: Vec::new(),
        }
    }

    /// Convert common clap args plus the owning workspace into a context.
    pub fn from_args(
        args: &DeployCmdArgs,
        workspace_root: impl Into<PathBuf>,
        package: impl Into<String>,
    ) -> Result<Self> {
        let mut context = Self::new(workspace_root, package);
        if let Some(package) = &args.package {
            context.package = package.clone();
        }
        context.binary_name = args
            .binary
            .clone()
            .unwrap_or_else(|| context.package.clone());
        context.release = args.release;
        context.dry_run = args.dry_run;
        context.force = args.force;
        context.quiet = args.quiet;
        context.source_binary = args.source.clone();
        context.install_path = args.install_to.clone().unwrap_or_else(|| {
            default_cargo_bin_dir().join(binary_name_with_extension(&context.binary_name))
        });
        context.backup_dir = args.backup_dir.clone();
        context.backup_retention = args.backup_retention;
        context.backup_max_age = args
            .backup_max_age_days
            .map(|days| Duration::from_secs(days.saturating_mul(86_400)));
        context.state_dir = args.state_dir.clone().unwrap_or_else(default_state_dir);
        context.build_args = args.build_args.clone();
        Ok(context)
    }

    pub fn profile(&self) -> Profile {
        if self.release {
            Profile::Release
        } else {
            Profile::Debug
        }
    }

    pub fn built_binary_path(&self) -> PathBuf {
        self.source_binary.clone().unwrap_or_else(|| {
            self.workspace_root
                .join("target")
                .join(self.profile().as_str())
                .join(binary_name_with_extension(&self.binary_name))
        })
    }
}

pub fn default_cargo_bin_dir() -> PathBuf {
    if let Some(home) = env::var_os("CARGO_HOME") {
        return PathBuf::from(home).join("bin");
    }
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".cargo").join("bin")
}

pub fn default_state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("tracel-deploy");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join("Library/Application Support/tracel-deploy");
        }
    }
    if let Some(state) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(state).join("tracel-deploy");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/tracel-deploy");
    }
    env::temp_dir().join("tracel-deploy")
}

pub fn binary_name_with_extension(name: &str) -> String {
    #[cfg(windows)]
    {
        if Path::new(name).extension().is_none() {
            return format!("{name}.exe");
        }
    }
    name.to_owned()
}

/// Find the nearest ancestor whose `Cargo.toml` declares a workspace.
pub fn workspace_root_from(start: &Path) -> Option<PathBuf> {
    let mut candidate = Some(start);
    while let Some(path) = candidate {
        let manifest = path.join("Cargo.toml");
        if manifest.is_file()
            && let Ok(text) = fs::read_to_string(&manifest)
            && text.contains("[workspace]")
        {
            return Some(path.to_path_buf());
        }
        candidate = path.parent();
    }
    None
}

pub fn current_workspace_root() -> Result<PathBuf> {
    let cwd = env::current_dir().context("could not read the current directory")?;
    workspace_root_from(&cwd)
        .ok_or_else(|| anyhow::anyhow!("could not find a workspace root above {}", cwd.display()))
}
