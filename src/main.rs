use std::process::ExitCode;

use clap::Parser;
use courier::Registry;
use courier::cli::{self, Cli, CliCommand};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };

    let default_log_level = match &cli.command {
        CliCommand::Run { .. } => "info",
        CliCommand::Validate { .. } | CliCommand::ListComponents => "off",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_log_level))
        .init();

    match cli.command {
        CliCommand::Run { config } => {
            let path = cli::resolve_config_path(config);
            let courier = match cli::build_runtime(&path) {
                Ok(courier) => courier,
                Err(err) => {
                    log::error!("failed to start courier: {err:#}");
                    return ExitCode::FAILURE;
                }
            };

            log::info!("loaded pipeline config from {}", path.display());
            courier.run().await;
            ExitCode::SUCCESS
        }
        CliCommand::Validate { config } => {
            let path = cli::resolve_config_path(config);
            match cli::validate_config(&path) {
                Ok(()) => {
                    println!("configuration OK: {}", path.display());
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("configuration invalid: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        CliCommand::ListComponents => match Registry::with_builtins() {
            Ok(registry) => {
                print!("{}", cli::list_components(&registry));
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("failed to initialize registry: {err:#}");
                ExitCode::FAILURE
            }
        },
    }
}
