use std::process::ExitCode;

use clap::Parser;
use courier::Registry;
use courier::cli::{self, Cli, CliCommand};
use courier::config::Config;

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

    match cli.command {
        CliCommand::Run { config } => {
            let path = cli::resolve_config_path(config);
            // Load the config before installing the subscriber so the
            // user's `[observability]` block can drive logging from the
            // first emitted event. Config-load errors happen pre-logger
            // and go straight to stderr — same surface as the previous
            // `eprintln!` in the validate path.
            let cfg = match Config::load(&path) {
                Ok(cfg) => cfg,
                Err(err) => {
                    eprintln!("failed to load config from {}: {err:#}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            if let Err(err) = cfg.validate() {
                eprintln!("configuration invalid: {err:#}");
                return ExitCode::FAILURE;
            }
            if let Err(err) = courier::observability::init_from_config(
                cfg.observability.as_ref(),
                default_log_level,
            ) {
                eprintln!("failed to initialize observability: {err:#}");
                return ExitCode::FAILURE;
            }

            let courier = match cli::build_runtime_from_config(cfg, &path) {
                Ok(courier) => courier,
                Err(err) => {
                    tracing::error!("failed to start courier: {err:#}");
                    return ExitCode::FAILURE;
                }
            };

            tracing::info!("loaded pipeline config from {}", path.display());
            courier.run().await;
            ExitCode::SUCCESS
        }
        CliCommand::Validate { config } => {
            if let Err(err) = courier::observability::init_default_logging(default_log_level) {
                eprintln!("failed to initialize logging: {err:#}");
                return ExitCode::FAILURE;
            }
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
        CliCommand::ListComponents => {
            if let Err(err) = courier::observability::init_default_logging(default_log_level) {
                eprintln!("failed to initialize logging: {err:#}");
                return ExitCode::FAILURE;
            }
            match Registry::with_builtins() {
                Ok(registry) => {
                    print!("{}", cli::list_components(&registry));
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("failed to initialize registry: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
