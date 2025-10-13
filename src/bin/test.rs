include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated.rs"));

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let runner: Courier = courier_from_config();
    runner.run().await;
}
