use std::process::ExitCode;

use clap::Parser;
use courier::cli::{self, Cli, CliCommand};

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        CliCommand::Run { config } => cli::run_command(config).await,
        CliCommand::Validate { config } => cli::validate_command(config),
        CliCommand::ListComponents => cli::list_components_command(),
        CliCommand::ReplayDlq { dlq_path, config } => {
            cli::replay_dlq_command(dlq_path, config).await
        }
    }
}
