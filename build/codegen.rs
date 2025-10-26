use quote::{format_ident, quote};

use super::config::{Config, OperationConfig, ReaderConfig, WriterConfig};

pub fn generate(config: &Config) -> proc_macro2::TokenStream {
    let operation_builders: Vec<_> = config.operations.iter().map(gen_operation).collect();

    quote! {
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

fn gen_operation(op: &OperationConfig) -> proc_macro2::TokenStream {
    match op {
        OperationConfig::Interval {
            name,
            reader,
            writer,
            interval_secs,
        } => {
            let reader_expr = gen_reader_expr(reader);
            let writer_expr = gen_writer_expr(writer);

            quote! {
                {
                    let reader = #reader_expr;
                    let writer = #writer_expr;
                    let operation = IntervalOperation::new(
                        #name,
                        reader,
                        writer,
                        Duration::from_secs(#interval_secs)
                    );
                    operations.push(Box::new(operation));
                }
            }
        }
        OperationConfig::Stream {
            name,
            reader,
            writer,
        } => {
            let reader_expr = gen_reader_expr(reader);
            let writer_expr = gen_writer_expr(writer);

            quote! {
                {
                    let reader = #reader_expr;
                    let writer = #writer_expr;
                    let operation = StreamOperation::new(#name, reader, writer);
                    operations.push(Box::new(operation));
                }
            }
        }
        OperationConfig::IntervalFanout {
            name,
            reader,
            writers,
            interval_secs,
        } => {
            let reader_expr = gen_reader_expr(reader);
            let writer_exprs: Vec<_> = writers.iter().map(gen_writer_expr).collect();

            quote! {
                {
                    let reader = #reader_expr;
                    let mut operation = IntervalFanoutOperation::new(
                        #name,
                        reader,
                        Duration::from_secs(#interval_secs)
                    );

                    #(
                        operation.add_writer(#writer_exprs);
                    )*

                    operations.push(Box::new(operation));
                }
            }
        }
    }
}

fn gen_reader_expr(reader: &ReaderConfig) -> proc_macro2::TokenStream {
    match reader {
        ReaderConfig::ApiReader { url, data_type } => {
            let data_type = format_ident!("{data_type}");
            quote! {
                ApiReader::new(#url).with_type::<#data_type>()
            }
        }
        ReaderConfig::KafkaReader {
            brokers,
            group_id,
            topics,
            data_type,
        } => {
            let data_type = format_ident!("{data_type}");
            quote! {
                KafkaReader::<#data_type>::new(
                    #brokers,
                    #group_id,
                    vec![#(#topics),*]
                )
            }
        }
    }
}

fn gen_writer_expr(writer: &WriterConfig) -> proc_macro2::TokenStream {
    match writer {
        WriterConfig::KafkaWriter {
            brokers,
            topic,
            data_type,
        } => {
            let data_type = format_ident!("{data_type}");
            quote! {
                KafkaWriter::<#data_type>::new(#brokers, #topic)
            }
        }
    }
}
