use quote::quote;

use super::config::{
    Config, ErrorPolicyConfig, PipelineConfig, SinkConfig, SourceConfig, TransformConfig,
};

pub fn generate(config: &Config) -> proc_macro2::TokenStream {
    let pipeline_builders: Vec<_> = config.pipelines.iter().map(gen_pipeline).collect();

    // Emit all imports inside the function body; #[allow(unused_imports)]
    // on the function covers the case where a given config does not use
    // every node type.
    quote! {
        use courier::Courier;

        #[allow(unused_imports)]
        pub fn courier_from_config() -> Courier {
            use std::time::Duration;

            use courier::pipeline::{ErrorPolicy, Pipeline};
            use courier::sinks::{BasicSink, Sink};
            use courier::sinks::kafka::KafkaSink;
            use courier::sources::Source;
            use courier::sources::api::ApiPollSource;
            use courier::sources::kafka::KafkaSource;
            use courier::transforms::{BasicTransform, Transform};
            use courier::transforms::set_key::SetKeyTransform;

            let mut pipelines: Vec<Pipeline> = Vec::new();

            #(#pipeline_builders)*

            Courier::new(pipelines)
        }
    }
}

fn gen_pipeline(p: &PipelineConfig) -> proc_macro2::TokenStream {
    let name = &p.name;
    let source_expr = gen_source(&p.source, name);

    let transforms: Vec<_> = p
        .transforms
        .iter()
        .enumerate()
        .map(|(i, t)| gen_transform(t, name, i))
        .collect();

    let sinks: Vec<_> = p
        .sinks
        .iter()
        .enumerate()
        .map(|(i, s)| gen_sink(s, name, i))
        .collect();

    let capacity_stmt = match p.channel_capacity {
        Some(c) => quote! { let pipeline = pipeline.with_channel_capacity(#c); },
        None => quote! {},
    };

    quote! {
        {
            let pipeline = Pipeline::new(
                #name,
                Box::new(#source_expr) as Box<dyn Source>,
            );
            #capacity_stmt
            let mut pipeline = pipeline;
            #(
                pipeline = pipeline.with_transform(Box::new(#transforms) as Box<dyn Transform>);
            )*
            #(
                pipeline = pipeline.with_sink(Box::new(#sinks) as Box<dyn Sink>);
            )*
            pipelines.push(pipeline);
        }
    }
}

fn gen_source(s: &SourceConfig, pipeline: &str) -> proc_macro2::TokenStream {
    let id = format!("{pipeline}/src");
    match s {
        SourceConfig::ApiPoll { url, interval_secs } => quote! {
            ApiPollSource::new(#id, #url, Duration::from_secs(#interval_secs))
        },
        SourceConfig::Kafka {
            brokers,
            group_id,
            topics,
        } => quote! {
            KafkaSource::new(#id, #brokers, #group_id, vec![#(#topics),*])
        },
    }
}

fn gen_transform(t: &TransformConfig, pipeline: &str, i: usize) -> proc_macro2::TokenStream {
    let id = format!("{pipeline}/t{i}");
    match t {
        TransformConfig::SetKey {
            from_field,
            on_error,
        } => {
            let policy = gen_error_policy(on_error);
            quote! {
                BasicTransform::new(SetKeyTransform::new(#id, #from_field))
                    .with_error_policy(#policy)
            }
        }
    }
}

fn gen_sink(s: &SinkConfig, pipeline: &str, i: usize) -> proc_macro2::TokenStream {
    let id = format!("{pipeline}/sink{i}");
    match s {
        SinkConfig::Kafka {
            brokers,
            topic,
            on_error,
        } => {
            let policy = gen_error_policy(on_error);
            quote! {
                BasicSink::new(KafkaSink::new(#id, #brokers, #topic))
                    .with_error_policy(#policy)
            }
        }
    }
}

fn gen_error_policy(e: &Option<ErrorPolicyConfig>) -> proc_macro2::TokenStream {
    match e {
        None | Some(ErrorPolicyConfig::Drop) => quote! { ErrorPolicy::Drop },
        Some(ErrorPolicyConfig::FailPipeline) => quote! { ErrorPolicy::FailPipeline },
    }
}
