use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
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

    /// [`from_args`](Self::from_args) plus the conventions a checkout deploy
    /// uses when it deploys what the repository contains:
    ///
    /// - **release build**: unless `--profile <name>` picks another profile, the
    ///   binary is built and installed as a release build (`--release` is
    ///   implied),
    /// - **state next to the logs**: unless `--state-dir` is given, state,
    ///   locks, stamps, and the queued run's logs live in
    ///   [`checkout_state_dir`],
    /// - **rotation sweeps what it replaced**: unless `--backup-retention` or
    ///   `--backup-max-age-days` is given, the rotation keeps no
    ///   `<binary>.old-*` files behind.
    ///
    /// Every convention is only a default: the matching flag, or a field set on
    /// the returned context, wins.
    pub fn for_checkout(
        args: &DeployCmdArgs,
        workspace_root: impl Into<PathBuf>,
        package: impl Into<String>,
    ) -> Result<Self> {
        let workspace_root = workspace_root.into();
        let mut context = Self::from_args(args, workspace_root.clone(), package)?;
        if args.profile.is_none() {
            context.release = true;
        }
        if args.state_dir.is_none() {
            context.state_dir = checkout_state_dir(&workspace_root);
        }
        if args.backup_retention.is_none() && args.backup_max_age_days.is_none() {
            context.backup_retention = Some(0);
        }
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

/// Deployment state, locks, stamps, and the queued run's logs for one checkout:
/// `<workspace_root>/target/codex-deploy`.
///
/// The directory [`DeployContext::for_checkout`] and
/// [`run_deploy`](crate::run_deploy) use unless `--state-dir` says otherwise.
/// It sits inside `target/`, so `cargo clean` removes the state together with
/// the build output.
pub fn checkout_state_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join("target").join("codex-deploy")
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

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Mix `bytes` into an FNV-1a 64 hash. Non-cryptographic change detection only.
fn fnv1a_64(bytes: &[u8], hash: &mut u64) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

/// Fingerprint a file by reading it in chunks.
pub fn file_fingerprint(path: &Path) -> std::io::Result<FileFingerprint> {
    use std::io::Read;

    let mut file = fs::File::open(path)?;
    let mut buffer = [0_u8; 8192];
    let mut hash: u64 = FNV_OFFSET_BASIS;
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        fnv1a_64(&buffer[..read], &mut hash);
    }
    Ok(FileFingerprint { size, hash })
}

/// True when both files exist and have identical size and content hash.
pub fn content_matches(left: &Path, right: &Path) -> std::io::Result<bool> {
    Ok(file_fingerprint(left)? == file_fingerprint(right)?)
}

/// Build inputs of [`build_inputs_fingerprint`], relative to the workspace
/// root. An entry may be a file or a directory; missing entries are skipped.
pub const DEFAULT_BUILD_INPUTS: &[&str] = &[
    "src",
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".cargo/config.toml",
];

/// Fingerprint of everything that can change the built binary: the `src` tree,
/// manifests, the lockfile, the build script, the toolchain pin, and the cargo
/// config ([`DEFAULT_BUILD_INPUTS`]).
///
/// This answers "were the sources touched since the last deploy?", which is a
/// different question from [`file_fingerprint`] on the built binary ("does the
/// build output differ from the installed binary?"). A deploy stamp that
/// records this fingerprint can therefore tell a changed source tree apart
/// from a stale build output.
///
/// Missing inputs are skipped, so a crate without `build.rs`, a lockfile, or a
/// toolchain pin still gets a stable fingerprint.
pub fn build_inputs_fingerprint(context: &DeployContext) -> Result<FileFingerprint> {
    build_inputs_fingerprint_with(context, DEFAULT_BUILD_INPUTS)
}

/// Like [`build_inputs_fingerprint`], with an explicit input list.
///
/// Entries are resolved against [`DeployContext::workspace_root`] unless they
/// are absolute. Directories are walked recursively and missing entries are
/// skipped, so callers can pass whole source trees next to single files. The
/// result does not depend on the enumeration order, on the absolute location
/// of the checkout, or on an entry being listed twice.
pub fn build_inputs_fingerprint_with<P>(
    context: &DeployContext,
    inputs: &[P],
) -> Result<FileFingerprint>
where
    P: AsRef<Path>,
{
    let root = &context.workspace_root;
    let mut files = Vec::new();
    for input in inputs {
        collect_build_inputs(root, input.as_ref(), &mut files)?;
    }
    let mut entries = files
        .iter()
        .map(|path| (relative_input_name(root, path), path))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries.dedup_by(|left, right| left.0 == right.0);

    let mut hash = FNV_OFFSET_BASIS;
    let mut size = 0_u64;
    for (name, path) in entries {
        let fingerprint = file_fingerprint(path)
            .with_context(|| format!("could not fingerprint build input {}", path.display()))?;
        size += fingerprint.size;
        fnv1a_64(name.as_bytes(), &mut hash);
        fnv1a_64(&[0], &mut hash);
        fnv1a_64(&fingerprint.size.to_le_bytes(), &mut hash);
        fnv1a_64(&fingerprint.hash.to_le_bytes(), &mut hash);
    }
    Ok(FileFingerprint { size, hash })
}

/// Collect the files behind one build input: a file is taken as is, a
/// directory is walked recursively, and a missing entry is skipped.
///
/// Directory symlinks are not followed, so a cycle cannot hang the walk.
fn collect_build_inputs(root: &Path, input: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let path = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not read build input {}", path.display()));
        }
    };
    if !metadata.is_dir() {
        files.push(path);
        return Ok(());
    }
    let entries = fs::read_dir(&path)
        .with_context(|| format!("could not list build input {}", path.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("could not list build input {}", path.display()))?;
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("could not read build input {}", entry_path.display()))?;
        if file_type.is_dir() {
            collect_build_inputs(root, &entry_path, files)?;
        } else if file_type.is_file() || entry_path.is_file() {
            // `is_file` follows symlinks: symlinked files are hashed, symlinked
            // directories are skipped.
            files.push(entry_path);
        }
    }
    Ok(())
}

/// Stable name of a build input: relative to the workspace root where possible
/// and always `/`-separated, so the fingerprint does not depend on where the
/// checkout lives or on the platform path separator.
fn relative_input_name(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.to_string_lossy().replace('\\', "/")
}

/// How old a lock file must be before [`DeployLock::acquire`] treats it as the
/// leftovers of a killed deployment and takes it over.
///
/// This is a heuristic, because nothing refreshes the file while a deployment
/// runs: a build that legitimately takes longer than the threshold can be taken
/// over. That is safe here - the takeover only decides *who* deploys: cargo
/// serializes builds of one target directory, and a rotation that finds the
/// installed binary already equal to the built one is a no-op.
pub const STALE_LOCK_AFTER: Duration = Duration::from_secs(600);

/// Deployment lock stored inside the context `state_dir`.
///
/// The lock is created with `create_new`, so two deployments of the same binary
/// cannot rotate it at the same time, and it is removed on drop, including
/// failure paths. A lock a killed deployment left behind (no `Drop`, no chance
/// to clean up) is taken over once it is older than [`STALE_LOCK_AFTER`]; a
/// fresh lock reports the owning pid and refuses.
#[derive(Debug)]
pub struct DeployLock {
    path: PathBuf,
    _file: fs::File,
    took_over: Option<u32>,
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
                Ok(Self {
                    path,
                    _file: file,
                    took_over: None,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::take_over_if_stale(path)
            }
            Err(error) => Err(error)
                .with_context(|| format!("could not create deployment lock {}", path.display())),
        }
    }

    /// Takes the lock over when the existing one is stale, refuses otherwise.
    fn take_over_if_stale(path: PathBuf) -> Result<Self> {
        let age = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok());
        let owner = lock_owner(&path);
        if age.is_none_or(|age| age < STALE_LOCK_AFTER) {
            let owner = owner.map(|pid| format!(", pid {pid}")).unwrap_or_default();
            anyhow::bail!(
                "deployment already running (lock: {}{owner}); a lock left behind by a killed \
                 deployment is taken over {} minutes after it was written",
                path.display(),
                STALE_LOCK_AFTER.as_secs() / 60
            );
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "could not take over the stale deployment lock {}",
                    path.display()
                )
            })?;
        writeln!(file, "pid={}", std::process::id())
            .with_context(|| format!("could not write the deployment lock {}", path.display()))?;
        Ok(Self {
            path,
            _file: file,
            took_over: owner,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The pid recorded by the lock this deployment took over; `None` when it
    /// created the lock itself.
    pub fn took_over(&self) -> Option<u32> {
        self.took_over
    }
}

/// The `pid=…` a lock file records, when it parses.
fn lock_owner(path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(path).ok()?;
    contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid="))
        .and_then(|pid| pid.trim().parse().ok())
}

impl Drop for DeployLock {
    fn drop(&mut self) {
        // Take the lock away only while it still records this process: a
        // deployment that took the stale lock over owns the file from then on,
        // and its own drop removes it.
        if lock_owner(&self.path) == Some(std::process::id()) {
            let _ = fs::remove_file(&self.path);
        }
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

    use super::{
        DeployContext, binary_name_with_extension, build_inputs_fingerprint,
        build_inputs_fingerprint_with, content_matches, file_fingerprint,
    };

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

    #[test]
    fn build_inputs_fingerprint_is_stable_and_order_independent() {
        let dir = temp_dir("build-inputs");
        fs::create_dir_all(dir.join("src/nested")).expect("src tree");
        fs::write(dir.join("Cargo.toml"), "[package]\n").expect("manifest");
        fs::write(dir.join("Cargo.lock"), "lock").expect("lockfile");
        fs::write(dir.join("build.rs"), "fn main() {}").expect("build script");
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").expect("main");
        fs::write(dir.join("src/nested/mod.rs"), "mod nested;\n").expect("nested");
        let context = DeployContext::new(&dir, "demo");

        let fingerprint = build_inputs_fingerprint(&context).expect("fingerprint");
        let expected_size = ("[package]\n".len()
            + "lock".len()
            + "fn main() {}".len()
            + "fn main() {}\n".len()
            + "mod nested;\n".len()) as u64;
        assert_eq!(fingerprint.size, expected_size);

        // The same inputs in another order, plus inputs that do not exist.
        let reordered = build_inputs_fingerprint_with(
            &context,
            &[
                "src",
                "Cargo.lock",
                "missing",
                "rust-toolchain",
                ".cargo/config.toml",
                "Cargo.toml",
                "build.rs",
                "rust-toolchain.toml",
            ],
        )
        .expect("fingerprint");
        assert_eq!(fingerprint, reordered);

        // Listing a file twice (here through `src` and `src/main.rs`) is stable.
        let duplicated = build_inputs_fingerprint_with(
            &context,
            &["src", "src/main.rs", "Cargo.toml", "Cargo.lock", "build.rs"],
        )
        .expect("fingerprint");
        assert_eq!(fingerprint, duplicated);

        // A workspace without any of the default inputs has an empty fingerprint.
        let empty_dir = temp_dir("build-inputs-empty");
        let empty =
            build_inputs_fingerprint(&DeployContext::new(&empty_dir, "demo")).expect("fingerprint");
        assert_eq!(empty.size, 0);
        assert_ne!(empty.hash, 0);

        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(empty_dir);
    }

    #[test]
    fn build_inputs_fingerprint_tracks_added_removed_and_changed_files() {
        let dir = temp_dir("build-inputs-changes");
        fs::create_dir_all(dir.join("src")).expect("src tree");
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").expect("main");
        fs::write(dir.join("src/lib.rs"), "pub fn lib() {}\n").expect("lib");
        let context = DeployContext::new(&dir, "demo");
        let initial = build_inputs_fingerprint(&context).expect("fingerprint");

        fs::write(dir.join("src/main.rs"), "fn main() { }\n").expect("edit");
        let changed = build_inputs_fingerprint(&context).expect("fingerprint");
        assert_ne!(initial, changed, "a content change must change the hash");

        fs::write(dir.join("src/extra.rs"), "pub fn extra() {}\n").expect("add");
        let added = build_inputs_fingerprint(&context).expect("fingerprint");
        assert_ne!(changed, added, "an added file must change the hash");

        fs::remove_file(dir.join("src/extra.rs")).expect("remove added file");
        let restored = build_inputs_fingerprint(&context).expect("fingerprint");
        assert_eq!(
            changed, restored,
            "removing the added file restores the hash"
        );

        fs::remove_file(dir.join("src/lib.rs")).expect("remove file");
        let removed = build_inputs_fingerprint(&context).expect("fingerprint");
        assert_ne!(changed, removed, "a removed file must change the hash");

        let _ = fs::remove_dir_all(dir);
    }
}
