use std::path::PathBuf;

use crate::background::DeployQueueArgs;

/// Common command line arguments for a deployment command.
///
/// Project-specific commands can flatten this into their own args struct:
///
/// ```ignore
/// #[derive(clap::Args, Clone, Debug)]
/// pub struct MyDeployArgs {
///     #[command(flatten)]
///     pub deploy: tracel_deploy::DeployCmdArgs,
///     #[arg(long)]
///     pub verify_timeout: Option<u64>,
/// }
/// ```
#[derive(Debug, Clone, clap::Args)]
pub struct DeployCmdArgs {
    /// Build in release mode.
    #[arg(long)]
    pub release: bool,
    /// Print the plan without building, installing, or changing a service.
    #[arg(long)]
    pub dry_run: bool,
    /// Run the full plan, including a rotation whose installed binary is
    /// already the built one.
    #[arg(long)]
    pub force: bool,
    /// Suppress normal progress output.
    #[arg(long)]
    pub quiet: bool,
    /// Workspace the deployed packages belong to. Detected from the xtask crate
    /// and the current directory when it is not given.
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,
    /// Crate/package to build.
    #[arg(long, value_name = "CRATE")]
    pub package: Option<String>,
    /// Binary name to install. Defaults to the package name.
    #[arg(long, value_name = "NAME")]
    pub binary: Option<String>,
    /// Override the built source binary instead of `target/<profile>/<binary>`.
    #[arg(long, value_name = "PATH")]
    pub source: Option<PathBuf>,
    /// Install destination. Defaults to `<cargo-home>/bin/<binary>`.
    #[arg(long, value_name = "PATH")]
    pub install_to: Option<PathBuf>,
    /// Directory for rotated backups. Defaults next to the destination.
    #[arg(long, value_name = "DIR")]
    pub backup_dir: Option<PathBuf>,
    /// Keep at most this many rotated backups (`<binary>.old-*`).
    #[arg(long, value_name = "COUNT")]
    pub backup_retention: Option<usize>,
    /// Remove rotated backups older than this many days.
    #[arg(long, value_name = "DAYS")]
    pub backup_max_age_days: Option<u64>,
    /// Directory for state, locks, stamps, and logs.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Cargo executable to run instead of `$CARGO` / `cargo`.
    #[arg(long, value_name = "PATH")]
    pub cargo: Option<PathBuf>,
    /// Custom cargo profile (for example `dist`) instead of `--release`.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// Pass `--locked` to cargo.
    #[arg(long)]
    pub locked: bool,
    /// Cross-compilation target triple passed to cargo.
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,
    /// Explicit cargo target directory.
    #[arg(long, value_name = "DIR")]
    pub target_dir: Option<PathBuf>,
    /// Comma separated cargo features to enable.
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    pub features: Vec<String>,
    /// Pass `--no-default-features` to cargo.
    #[arg(long)]
    pub no_default_features: bool,
    /// Extra arguments forwarded to `cargo build` after `--`.
    #[arg(last = true)]
    pub build_args: Vec<String>,
}

/// The complete `deploy` argument surface: the queue switches a hook-driven
/// deploy needs plus the common deployment flags.
///
/// This is the args type [`deploy_commands`](crate::deploy_commands) uses when
/// it generates the command for you (`packages = [...]`) and the type
/// [`run_deploy`](crate::run_deploy) takes. Flatten it into a
/// project-specific struct when the command needs extra flags:
///
/// ```ignore
/// #[derive(clap::Args, Clone, Debug)]
/// pub struct MyDeployArgs {
///     #[command(flatten)]
///     pub deploy: tracel_deploy::DeployArgs,
///     #[arg(long, value_name = "SECS")]
///     pub verify_timeout: Option<u64>,
/// }
/// ```
#[derive(Debug, Clone, clap::Args)]
pub struct DeployArgs {
    /// `--background` / `--child`: queue the deployment instead of running it.
    #[command(flatten)]
    pub queue: DeployQueueArgs,
    /// The shared build, install, rotation, and state flags.
    #[command(flatten)]
    pub common: DeployCmdArgs,
}
