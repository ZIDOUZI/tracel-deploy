//! The macro's one-line registration, exercised end to end.
//!
//! `packages = [..]` writes the variant and the whole dispatcher, so the test
//! only has to call the generated function: a dry run is enough to prove the
//! generated body reaches the library with the packages it was given, while
//! building, installing, and queueing nothing.

use tracel_deploy::prelude::*;
use tracel_xtask::prelude::*;

use clap::Parser as _;

#[tracel_deploy::deploy_commands(Deploy, packages = ["my-app"])]
#[macros::base_commands(Build, Check, Test)]
pub enum Command {}

#[derive(clap::Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[test]
fn the_generated_dispatcher_deploys_the_listed_packages() {
    let cli = Cli::try_parse_from(["xtask", "deploy", "--dry-run", "--quiet"])
        .expect("the generated deploy subcommand parses");
    let Command::Deploy(deploy) = cli.command else {
        panic!("expected the generated deploy variant");
    };

    // The generated dispatcher derives the workspace root from this crate's
    // `CARGO_MANIFEST_DIR`, so the dry run renders the plan of this workspace.
    handle_deploy_command(deploy, Environment::default(), Context::default())
        .expect("the generated dispatcher deploys its packages");
}
