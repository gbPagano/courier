use std::process::ExitCode;

use courier::Registry;
use courier::config::Config;

const DEFAULT_CONFIG_PATH: &str = "config.toml";
const CONFIG_ENV_VAR: &str = "COURIER_CONFIG";

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let path = std::env::var(CONFIG_ENV_VAR).unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());

    let courier = match Config::load(&path).and_then(|config| {
        let registry = Registry::with_builtins()?;
        registry.build_courier(config)
    }) {
        Ok(courier) => courier,
        Err(err) => {
            log::error!("failed to start courier: {err:#}");
            return ExitCode::FAILURE;
        }
    };

    log::info!("loaded pipeline config from {path}");
    courier.run().await;
    ExitCode::SUCCESS
}
