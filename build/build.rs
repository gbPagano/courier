mod codegen;
mod config;

fn main() {
    println!("cargo:rerun-if-changed=build/build.rs");
    println!("cargo:rerun-if-changed=build/config.rs");
    println!("cargo:rerun-if-changed=build/codegen.rs");

    let config_path = "config.toml";
    println!("cargo:rerun-if-changed={config_path}");

    let config_content = std::fs::read_to_string(config_path).expect("Failed to read config file");
    let config = config::parse_config(&config_content).expect("Failed to parse config file");

    let code = codegen::generate(&config);
    std::fs::write("generated.rs", code.to_string()).unwrap();
}
