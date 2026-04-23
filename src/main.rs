include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated.rs"));

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let courier: Courier = courier_from_config();
    courier.run().await;
}
