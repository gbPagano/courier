use serde::Deserialize;
// use std::env;
use std::fs;
// use std::path::Path;

use quote::{format_ident, quote};

#[derive(Debug, Deserialize)]
struct Config {
    operations: Vec<OperationConfig>,
}

#[derive(Debug, Deserialize)]
struct OperationConfig {
    name: String,
    #[serde(rename = "type")]
    op_type: String,
    reader: ReaderConfig,
    #[serde(default)]
    writer: Option<WriterConfig>,
    #[serde(default)]
    writers: Vec<WriterConfig>,
    #[serde(default)]
    interval_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ReaderConfig {
    #[serde(rename = "type")]
    reader_type: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    brokers: String,
    #[serde(default)]
    group_id: String,
    #[serde(default)]
    topics: Vec<String>,
    data_type: String,
}

#[derive(Debug, Deserialize, Clone)]
struct WriterConfig {
    #[serde(rename = "type")]
    writer_type: String,
    #[serde(default)]
    brokers: String,
    #[serde(default)]
    topic: String,
    data_type: String,
}

fn main() {
    println!("cargo:rerun-if-changed=config.toml");

    let config_path = "config.toml";
    let config_content = fs::read_to_string(config_path).expect("Failed to read config file");
    let config: Config = toml::from_str(&config_content).expect("Failed to parse config file");

    let code = generate_code(&config);
    // std::fs::write(&"generated.rs", code.to_string()).unwrap();
    // // let out_dir = env::var("OUT_DIR").unwrap();
    // // let dest_path = Path::new(&out_dir).join("generated_operations.rs");

    fs::write(&"generated.rs", code.to_string()).expect("Failed to write generated code");
}

fn generate_code(config: &Config) -> proc_macro2::TokenStream {
    let operation_builders: Vec<_> = config
        .operations
        .iter()
        .map(|op| generate_operation(op))
        .collect();

    // let operations = quote! {
    //     pub fn create_operations() -> Vec<Box<dyn std::any::Any>> {
    //         let mut operations: Vec<Box<dyn std::any::Any>> = Vec::new();
    //
    //         #(#operation_builders)*
    //
    //         operations
    //     }
    // };

    quote! {
        // Auto-generated from config file
        use std::time::Duration;

        use serde_json::Value;

        use courier::Courier;
        use courier::readers::api::ApiReader;
        use courier::readers::kafka::KafkaReader;
        use courier::writers::kafka::KafkaWriter;
        use courier::operations::*;

        pub fn courier_from_config() -> Courier {
            let mut operations: Vec<Box<dyn Operation>> = Vec::new();

            #(#operation_builders)*

            Courier::new(operations)
        }
    }
}

fn generate_operation(op: &OperationConfig) -> proc_macro2::TokenStream {
    let name = &op.name;
    let reader = &op.reader;

    match op.op_type.as_str() {
        "stream" => {
            let writer = op.writer.as_ref().expect("Stream requires writer");

            let reader_expr = generate_reader_expr(reader);
            let writer_expr = generate_writer_expr(writer);

            quote! {
                {
                    let reader = #reader_expr;
                    let writer = #writer_expr;
                    let operation = StreamOperation::new(#name, reader, writer);
                    operations.push(Box::new(operation));
                }
            }
        }
        "interval" => {
            let writer = op.writer.as_ref().expect("Interval requires writer");
            let interval = op
                .interval_secs
                .expect("Interval operations requires 'interval_secs'");

            let reader_expr = generate_reader_expr(reader);
            let writer_expr = generate_writer_expr(writer);

            quote! {
                {
                    let reader = #reader_expr;
                    let writer = #writer_expr;
                    let operation = IntervalOperation::new(
                        #name,
                        reader,
                        writer,
                        Duration::from_secs(#interval)
                    );
                    operations.push(Box::new(operation));
                }
            }
        }
        "interval_fanout" => {
            let interval = op
                .interval_secs
                .expect("Interval operations requires 'interval_secs'");

            let reader_expr = generate_reader_expr(reader);
            let writer_exprs: Vec<_> = op.writers.iter().map(generate_writer_expr).collect();

            quote! {
                {
                    let reader = #reader_expr;
                    let mut operation = IntervalFanoutOperation::new(
                        #name,
                        reader,
                        Duration::from_secs(#interval)
                    );

                    #(
                        operation.add_writer(#writer_exprs);
                    )*

                    operations.push(Box::new(operation));
                }
            }
        }
        _ => panic!("Unknown operation type: {}", op.op_type),
    }
}

fn generate_reader_expr(reader: &ReaderConfig) -> proc_macro2::TokenStream {
    let data_type = format_ident!("{}", reader.data_type);

    match reader.reader_type.as_str() {
        "api" => {
            let url = &reader.url;

            quote! {
                ApiReader::new(#url).with_type::<#data_type>()
            }
        }
        "kafka" => {
            let brokers = &reader.brokers;
            let group_id = &reader.group_id;
            let topics = &reader.topics;

            quote! {
                KafkaReader::<#data_type>::new(
                    #brokers,
                    #group_id,
                    vec![#(#topics),*]
                )
            }
        }
        _ => panic!("Unknown reader type: {}", reader.reader_type),
    }
}

fn generate_writer_expr(writer: &WriterConfig) -> proc_macro2::TokenStream {
    let data_type = format_ident!("{}", writer.data_type);

    match writer.writer_type.as_str() {
        "kafka" => {
            let brokers = &writer.brokers;
            let topic = &writer.topic;

            quote! {
                KafkaWriter::<#data_type>::new(#brokers, #topic)
            }
        }
        _ => panic!("Unknown writer type: {}", writer.writer_type),
    }
}
