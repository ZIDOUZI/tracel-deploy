use std::{
    env, fs,
    io::Write,
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
    /// Cargo executable override; falls back to `$CARGO`, then `cargo`.
    pub cargo: Option<PathBuf>,
    /// Custom cargo profile (for example `dist`); overrides `release`.
    pub profile_name: Option<String>,
    /// Pass `--locked` to cargo.
    pub locked: bool,
    /// Cross-compilation target triple (`--target`).
    pub target: Option<String>,
    /// Explicit cargo target directory (`--target-dir`).
    pub target_dir: Option<PathBuf>,
    /// Features to enable (`--features`).
    pub features: Vec<String>,
    /// Pass `--no-default-features` to cargo.
    pub no_default_features: bool,
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
            cargo: None,
            profile_name: None,
            locked: false,
            target: None,
            target_dir: None,
            features: Vec::new(),
            no_default_features: false,
        }
    }

    /// Start building a context with platform defaults:
    ///
    /// ```ignore
    /// let context = DeployContext::builder(workspace, "my-service")
    ///     .release(true)
    ///     .quiet(false)
    ///     .build();
    /// ```
    pub fn builder(
        workspace_root: impl Into<PathBuf>,
        package: impl Into<String>,
    ) -> DeployContextBuilder {
        DeployContextBuilder {
            context: Self::new(workspace_root, package),
            install_path_explicit: false,
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
        context.cargo = args.cargo.clone();
        context.profile_name = args.profile.clone();
        context.locked = args.locked;
        context.target = args.target.clone();
        context.target_dir = args.target_dir.clone();
        context.features = args.features.clone();
        context.no_default_features = args.no_default_features;
        Ok(context)
    }

    pub fn profile(&self) -> Profile {
        if self.release {
            Profile::Release
        } else {
            Profile::Debug
        }
    }

    /// Directory name inside the target directory that holds the binary.
    pub fn profile_dir(&self) -> String {
        self.profile_name
            .clone()
            .unwrap_or_else(|| self.profile().as_str().to_owned())
    }

    /// Cargo target directory: explicit `target_dir`, then `CARGO_TARGET_DIR`
    /// (relative paths resolve against the workspace root), then
    /// `<workspace>/target`.
    pub fn cargo_target_dir(&self) -> PathBuf {
        if let Some(dir) = &self.target_dir {
            return self.resolve_from_workspace(dir);
        }
        if let Some(dir) = env::var_os("CARGO_TARGET_DIR") {
            return self.resolve_from_workspace(Path::new(&dir));
        }
        self.workspace_root.join("target")
    }

    fn resolve_from_workspace(&self, dir: &Path) -> PathBuf {
        if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            self.workspace_root.join(dir)
        }
    }

    /// Effective target triple: explicit `target`, then `CARGO_BUILD_TARGET`.
    pub fn target_triple(&self) -> Option<String> {
        self.target.clone().or_else(|| {
            env::var("CARGO_BUILD_TARGET")
                .ok()
                .filter(|value| !value.is_empty())
        })
    }

    /// Path of the built binary, honouring `source_binary`, `target_dir`,
    /// `CARGO_TARGET_DIR`, `target`, `CARGO_BUILD_TARGET`, and the profile.
    pub fn built_binary_path(&self) -> PathBuf {
        if let Some(source) = &self.source_binary {
            return source.clone();
        }
        let target_dir = self.cargo_target_dir();
        let profile = self.profile_dir();
        let binary = binary_name_with_extension(&self.binary_name);
        match self.target_triple() {
            Some(triple) => target_dir.join(triple).join(profile).join(binary),
            None => target_dir.join(profile).join(binary),
        }
    }

    /// Like [`built_binary_path`](Self::built_binary_path), but fails with the
    /// list of candidate paths when the binary does not exist yet.
    pub fn resolved_binary_path(&self) -> Result<PathBuf> {
        let path = self.built_binary_path();
        if path.is_file() {
            return Ok(path);
        }
        let mut candidates = vec![path];
        let fallback = self
            .workspace_root
            .join("target")
            .join(self.profile_dir())
            .join(binary_name_with_extension(&self.binary_name));
        if !candidates.contains(&fallback) {
            candidates.push(fallback);
        }
        let candidates = candidates
            .iter()
            .map(|candidate| candidate.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let hint = if self.source_binary.is_some() {
            "the configured --source does not exist"
        } else {
            "build the package first, or set --source / CARGO_TARGET_DIR"
        };
        anyhow::bail!(
            "built binary for package {} not found; tried: {candidates} ({hint})",
            self.package
        )
    }
}

/// Fluent builder for [`DeployContext`].
pub struct DeployContextBuilder {
    context: DeployContext,
    install_path_explicit: bool,
}

impl DeployContextBuilder {
    pub fn binary(mut self, name: impl Into<String>) -> Self {
        self.context.binary_name = name.into();
        if !self.install_path_explicit {
            self.context.install_path =
                default_cargo_bin_dir().join(binary_name_with_extension(&self.context.binary_name));
        }
        self
    }

    pub fn release(mut self, release: bool) -> Self {
        self.context.release = release;
        self
    }

    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.context.dry_run = dry_run;
        self
    }

    pub fn force(mut self, force: bool) -> Self {
        self.context.force = force;
        self
    }

    pub fn quiet(mut self, quiet: bool) -> Self {
        self.context.quiet = quiet;
        self
    }

    pub fn source(mut self, path: impl Into<PathBuf>) -> Self {
        self.context.source_binary = Some(path.into());
        self
    }

    pub fn install_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.context.install_path = path.into();
        self.install_path_explicit = true;
        self
    }

    pub fn backup_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.context.backup_dir = Some(path.into());
        self
    }

    pub fn backup_retention(mut self, keep: usize) -> Self {
        self.context.backup_retention = Some(keep);
        self
    }

    pub fn backup_max_age(mut self, max_age: Duration) -> Self {
        self.context.backup_max_age = Some(max_age);
        self
    }

    pub fn state_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.context.state_dir = path.into();
        self
    }

    pub fn build_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.context.build_args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn cargo(mut self, path: impl Into<PathBuf>) -> Self {
        self.context.cargo = Some(path.into());
        self
    }

    pub fn profile(mut self, name: impl Into<String>) -> Self {
        self.context.profile_name = Some(name.into());
        self
    }

    pub fn locked(mut self, locked: bool) -> Self {
        self.context.locked = locked;
        self
    }

    pub fn target(mut self, triple: impl Into<String>) -> Self {
        self.context.target = Some(triple.into());
        self
    }

    pub fn target_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.context.target_dir = Some(dir.into());
        self
    }

    pub fn features<I, S>(mut self, features: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.context.features = features.into_iter().map(Into::into).collect();
        self
    }

    pub fn no_default_features(mut self, value: bool) -> Self {
        self.context.no_default_features = value;
        self
    }

    pub fn build(self) -> DeployContext {
        self.context
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

/// Content fingerprint of a file: size plus a 64-bit FNV-1a hash.
///
/// The hash is used for change detection, not for security.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFingerprint {
    pub size: u64,
    pub hash: u64,
}

impl std::fmt::Display for FileFingerprint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "size={} hash=0x{:016x}", self.size, self.hash)
    }
}

/// Fingerprint a file by reading it in chunks.
pub fn file_fingerprint(path: &Path) -> std::io::Result<FileFingerprint> {
    use std::io::Read;

    let mut file = fs::File::open(path)?;
    let mut buffer = [0_u8; 8192];
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        for byte in &buffer[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Ok(FileFingerprint { size, hash })
}

/// True when both files exist and have identical size and content hash.
pub fn content_matches(left: &Path, right: &Path) -> std::io::Result<bool> {
    Ok(file_fingerprint(left)? == file_fingerprint(right)?)
}

/// Deployment lock stored inside the context `state_dir`.
///
/// The lock is created with `create_new` so two deployments of the same
/// binary cannot rotate it at the same time. It is removed on drop, including
/// failure paths; if a previous deployment crashed, delete the lock file
/// manually (its path is part of the error message).
#[derive(Debug)]
pub struct DeployLock {
    path: PathBuf,
    _file: fs::File,
}

impl DeployLock {
    pub fn acquire(state_dir: &Path, name: &str) -> Result<Self> {
        fs::create_dir_all(state_dir)
            .with_context(|| format!("could not create state dir {}", state_dir.display()))?;
        let path = state_dir.join(format!("{name}.deploy.lock"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                Ok(Self { path, _file: file })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => anyhow::bail!(
                "deployment already running (lock: {}); delete the file if the previous deploy crashed",
                path.display()
            ),
            Err(error) => Err(error)
                .with_context(|| format!("could not create deployment lock {}", path.display())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DeployLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{DeployContext, binary_name_with_extension, content_matches, file_fingerprint};

    /// Serializes the tests that mutate process-wide cargo environment
    /// variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tracel-deploy-context-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn restore_env(key: &str, previous: Option<std::ffi::OsString>) {
        match previous {
            Some(value) => unsafe { std::env::set_var(key, value) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[test]
    fn builder_sets_binary_and_install_path() {
        let dir = temp_dir("builder");
        let context = DeployContext::builder(&dir, "my-service")
            .release(true)
            .quiet(false)
            .build();
        assert_eq!(context.binary_name, "my-service");
        assert!(
            context
                .install_path
                .ends_with(binary_name_with_extension("my-service"))
        );
        assert!(context.release);
        assert!(!context.quiet);

        let context = DeployContext::builder(&dir, "my-service")
            .binary("other")
            .install_to(dir.join("custom"))
            .build();
        assert_eq!(context.binary_name, "other");
        assert_eq!(context.install_path, dir.join("custom"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cargo_target_dir_follows_env_and_overrides() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let previous = std::env::var_os("CARGO_TARGET_DIR");
        let dir = temp_dir("target-dir");
        let binary = binary_name_with_extension("my-service");

        let context = DeployContext::new(&dir, "my-service");
        assert_eq!(
            context.built_binary_path(),
            dir.join("target").join("debug").join(&binary)
        );

        let custom = dir.join("custom-target");
        unsafe { std::env::set_var("CARGO_TARGET_DIR", &custom) };
        let context = DeployContext::new(&dir, "my-service");
        assert_eq!(
            context.built_binary_path(),
            custom.join("debug").join(&binary)
        );

        unsafe { std::env::set_var("CARGO_TARGET_DIR", "rel-target") };
        let context = DeployContext::new(&dir, "my-service");
        assert_eq!(
            context.built_binary_path(),
            dir.join("rel-target").join("debug").join(&binary)
        );

        let context = DeployContext::builder(&dir, "my-service")
            .release(true)
            .target_dir("explicit")
            .build();
        assert_eq!(
            context.built_binary_path(),
            dir.join("explicit").join("release").join(&binary)
        );

        restore_env("CARGO_TARGET_DIR", previous);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cargo_build_target_adds_the_triple() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let previous = std::env::var_os("CARGO_BUILD_TARGET");
        let dir = temp_dir("triple");

        unsafe { std::env::set_var("CARGO_BUILD_TARGET", "x86_64-pc-windows-msvc") };
        let context = DeployContext::new(&dir, "my-service");
        assert!(
            context.built_binary_path().ends_with(
                PathBuf::from("x86_64-pc-windows-msvc")
                    .join("debug")
                    .join(binary_name_with_extension("my-service"))
            )
        );

        restore_env("CARGO_BUILD_TARGET", previous);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolved_binary_path_lists_candidates_when_missing() {
        let dir = temp_dir("resolve");
        let context = DeployContext::builder(&dir, "my-service")
            .release(true)
            .build();

        let error = context
            .resolved_binary_path()
            .expect_err("binary is missing");
        let text = format!("{error:#}");
        assert!(text.contains("not found"), "text: {text}");
        assert!(
            text.contains(&dir.join("target").join("release").display().to_string()),
            "text: {text}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fingerprints_detect_content_changes() {
        let dir = temp_dir("fingerprint");
        let left = dir.join("left");
        let right = dir.join("right");
        fs::write(&left, "same").expect("left");
        fs::write(&right, "same").expect("right");
        assert!(content_matches(&left, &right).expect("compare"));
        assert_eq!(file_fingerprint(&left).expect("fingerprint").size, 4);

        fs::write(&right, "different").expect("right");
        assert!(!content_matches(&left, &right).expect("compare"));
        let _ = fs::remove_dir_all(dir);
    }
}
