include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated.rs"));

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let registry =
        courier::Registry::with_builtins().expect("failed to register built-in components");
    let courier = registry
        .build_courier(courier_config())
        .expect("failed to build courier from config");
    courier.run().await;
}
