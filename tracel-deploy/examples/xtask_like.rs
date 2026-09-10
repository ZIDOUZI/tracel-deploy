use tracel_deploy::prelude::*;
use tracel_xtask::prelude::*;

#[tracel_deploy::deploy_commands(Deploy, handler = handle_deploy)]
#[macros::base_commands(Build, Check, Test)]
pub enum Command {
    Custom(CustomArgs),
}

#[derive(clap::Args, Clone, Debug)]
pub struct CustomArgs {
    #[arg(long)]
    pub name: Option<String>,
}

fn handle_deploy(
    _args: tracel_deploy::DeployCmdArgs,
    _environment: Environment,
    _context: Context,
) -> anyhow::Result<()> {
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = init_xtask::<Command>(parse_args::<Command>()?)?;
    match args.command {
        Command::Custom(_) => Ok(()),
        Command::Deploy(command) => handle_deploy_command(command, args.environment, args.context),
        _ => dispatch_base_commands(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[test]
    fn deploy_variant_is_generated_and_parses() {
        let cli = Cli::try_parse_from(["xtask", "deploy", "--release", "--dry-run"])
            .expect("deploy subcommand parses");
        match cli.command {
            Command::Deploy(args) => {
                assert!(args.release);
                assert!(args.dry_run);
            }
            _ => panic!("expected generated deploy variant"),
        }
    }
}
