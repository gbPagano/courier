use quote::quote;

use super::config::{
    Config, ErrorPolicyConfig, PipelineConfig, SinkConfig, SourceConfig, TransformConfig,
};

pub fn generate(config: &Config) -> proc_macro2::TokenStream {
    let pipelines: Vec<_> = config.pipelines.iter().map(gen_pipeline).collect();

    quote! {
        pub fn courier_config() -> courier::config::Config {
            courier::config::Config {
                pipelines: vec![#(#pipelines),*],
            }
        }
    }
}

fn gen_pipeline(pipeline: &PipelineConfig) -> proc_macro2::TokenStream {
    let name = &pipeline.name;
    let source = gen_source(&pipeline.source);
    let transforms: Vec<_> = pipeline.transforms.iter().map(gen_transform).collect();
    let sinks: Vec<_> = pipeline.sinks.iter().map(gen_sink).collect();

    let channel_capacity = match pipeline.channel_capacity {
        Some(capacity) => quote! { Some(#capacity) },
        None => quote! { None },
    };

    quote! {
        courier::config::PipelineSpec {
            name: #name.into(),
            source: #source,
            transforms: vec![#(#transforms),*],
            sinks: vec![#(#sinks),*],
            channel_capacity: #channel_capacity,
        }
    }
}

fn gen_source(source: &SourceConfig) -> proc_macro2::TokenStream {
    let kind = &source.kind;
    let config = gen_json_value(&source.config);

    quote! {
        courier::config::SourceSpec {
            kind: #kind.into(),
            config: #config,
        }
    }
}

fn gen_transform(transform: &TransformConfig) -> proc_macro2::TokenStream {
    let kind = &transform.kind;
    let config = gen_json_value(&transform.config);
    let on_error = gen_error_policy(&transform.on_error);

    quote! {
        courier::config::TransformSpec {
            kind: #kind.into(),
            config: #config,
            on_error: #on_error,
        }
    }
}

fn gen_sink(sink: &SinkConfig) -> proc_macro2::TokenStream {
    let kind = &sink.kind;
    let config = gen_json_value(&sink.config);
    let on_error = gen_error_policy(&sink.on_error);

    quote! {
        courier::config::SinkSpec {
            kind: #kind.into(),
            config: #config,
            on_error: #on_error,
            retry: None,
        }
    }
}

fn gen_error_policy(policy: &Option<ErrorPolicyConfig>) -> proc_macro2::TokenStream {
    match policy {
        Some(ErrorPolicyConfig::Drop) => {
            quote! { Some(courier::config::ErrorPolicyConfig::Drop) }
        }
        Some(ErrorPolicyConfig::FailPipeline) => {
            quote! { Some(courier::config::ErrorPolicyConfig::FailPipeline) }
        }
        None => quote! { None },
    }
}

fn gen_json_value(value: &serde_json::Value) -> proc_macro2::TokenStream {
    let encoded = serde_json::to_string(value).expect("config JSON should serialize");
    quote! {
        serde_json::from_str::<serde_json::Value>(#encoded)
            .expect("generated config JSON should parse")
    }
}
