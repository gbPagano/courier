//! Runtime component registry.
//!
//! Maps short type names (`"kafka"`, `"api_poll"`, …) to factories that
//! build concrete `Source`, `Transform`, and `Sink` instances from a
//! `serde_json::Value` spec. Built-ins register via [`register_builtin`];
//! external crates register more of their own before the `Courier` is
//! constructed.
//!
//! Plugin model (by level):
//! 1. Built-ins — components shipped in this crate, registered via
//!    [`register_builtin`].
//! 2. Statically-linked plugin crates — `my_plugin::register(&mut registry)`
//!    at startup. First-class native plugin mechanism.
//! 3. (Future) Dynamically-loaded plugins — crate-boundary via `libloading`
//!    or an embedded scripting runtime. The factory traits are already
//!    object-safe, so this is additive.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::Courier;
use crate::config::{Config, PipelineSpec, SinkSpec, SourceSpec, TransformSpec};
use crate::pipeline::{ErrorPolicy, Pipeline};
use crate::retry::RetryPolicy;
use crate::sinks::Sink;
use crate::sources::Source;
use crate::transforms::Transform;

/// Builds a `Box<dyn Source>` from a JSON spec. Polling sources receive an
/// optional `RetryPolicy` extracted from `SourceSpec`; push-based sources
/// (kafka, http_webhook) reject it at factory time so users don't think
/// retry is doing something it isn't.
pub trait SourceFactory: Send + Sync {
    fn build(&self, id: &str, config: Value, retry: Option<RetryPolicy>)
    -> Result<Box<dyn Source>>;
}

impl<F> SourceFactory for F
where
    F: Fn(&str, Value, Option<RetryPolicy>) -> Result<Box<dyn Source>> + Send + Sync,
{
    fn build(
        &self,
        id: &str,
        config: Value,
        retry: Option<RetryPolicy>,
    ) -> Result<Box<dyn Source>> {
        (self)(id, config, retry)
    }
}

/// Builds a `Box<dyn Transform>` from a JSON spec. Factories that wrap a
/// `MapOne` in `BasicTransform` are responsible for applying `on_error`.
pub trait TransformFactory: Send + Sync {
    fn build(&self, id: &str, config: Value, on_error: ErrorPolicy) -> Result<Box<dyn Transform>>;
}

impl<F> TransformFactory for F
where
    F: Fn(&str, Value, ErrorPolicy) -> Result<Box<dyn Transform>> + Send + Sync,
{
    fn build(&self, id: &str, config: Value, on_error: ErrorPolicy) -> Result<Box<dyn Transform>> {
        (self)(id, config, on_error)
    }
}

/// Builds a `Box<dyn Sink>` from a JSON spec. Factories that wrap a
/// `WriteOne` in `ManagedSink` are responsible for applying `on_error` and
/// `retry` — both are extracted from `SinkSpec` by the registry and passed
/// directly, so no per-sink config parsing is needed for these policies.
pub trait SinkFactory: Send + Sync {
    fn build(
        &self,
        id: &str,
        config: Value,
        on_error: ErrorPolicy,
        retry: Option<RetryPolicy>,
    ) -> Result<Box<dyn Sink>>;
}

impl<F> SinkFactory for F
where
    F: Fn(&str, Value, ErrorPolicy, Option<RetryPolicy>) -> Result<Box<dyn Sink>> + Send + Sync,
{
    fn build(
        &self,
        id: &str,
        config: Value,
        on_error: ErrorPolicy,
        retry: Option<RetryPolicy>,
    ) -> Result<Box<dyn Sink>> {
        (self)(id, config, on_error, retry)
    }
}

/// Runtime registry of pipeline components. Each category (source,
/// transform, sink) is a separate namespace — `"kafka"` as a source and
/// `"kafka"` as a sink coexist without collision. Duplicate `kind`s
/// within a category are rejected at registration time.
#[derive(Default)]
pub struct Registry {
    source_factories: HashMap<String, Box<dyn SourceFactory>>,
    transform_factories: HashMap<String, Box<dyn TransformFactory>>,
    sink_factories: HashMap<String, Box<dyn SinkFactory>>,
}

impl Registry {
    /// Convenience constructor: empty registry preloaded with every
    /// built-in component from this crate. Equivalent to
    /// `let mut r = Registry::default(); register_builtin(&mut r)?;`.
    pub fn with_builtins() -> Result<Self> {
        let mut registry = Self::default();
        register_builtin(&mut registry)?;
        Ok(registry)
    }

    pub fn register_source(
        &mut self,
        kind: impl Into<String>,
        factory: impl SourceFactory + 'static,
    ) -> Result<()> {
        let kind = kind.into();
        if self.source_factories.contains_key(&kind) {
            bail!("source factory '{kind}' already registered");
        }
        self.source_factories.insert(kind, Box::new(factory));
        Ok(())
    }

    pub fn register_transform(
        &mut self,
        kind: impl Into<String>,
        factory: impl TransformFactory + 'static,
    ) -> Result<()> {
        let kind = kind.into();
        if self.transform_factories.contains_key(&kind) {
            bail!("transform factory '{kind}' already registered");
        }
        self.transform_factories.insert(kind, Box::new(factory));
        Ok(())
    }

    pub fn register_sink(
        &mut self,
        kind: impl Into<String>,
        factory: impl SinkFactory + 'static,
    ) -> Result<()> {
        let kind = kind.into();
        if self.sink_factories.contains_key(&kind) {
            bail!("sink factory '{kind}' already registered");
        }
        self.sink_factories.insert(kind, Box::new(factory));
        Ok(())
    }

    pub fn build_source(&self, id: &str, spec: SourceSpec) -> Result<Box<dyn Source>> {
        let kind = spec.kind;
        let retry = spec.retry;
        let factory = self
            .source_factories
            .get(&kind)
            .with_context(|| format!("unknown source type '{kind}'"))?;
        factory
            .build(id, spec.config, retry)
            .with_context(|| format!("failed to build source '{kind}'"))
    }

    pub fn build_transform(&self, id: &str, spec: TransformSpec) -> Result<Box<dyn Transform>> {
        let kind = spec.kind;
        let on_error = spec.on_error.unwrap_or_default().into();
        let factory = self
            .transform_factories
            .get(&kind)
            .with_context(|| format!("unknown transform type '{kind}'"))?;
        factory
            .build(id, spec.config, on_error)
            .with_context(|| format!("failed to build transform '{kind}'"))
    }

    pub fn build_sink(&self, id: &str, spec: SinkSpec) -> Result<Box<dyn Sink>> {
        let kind = spec.kind;
        let on_error = spec.on_error.unwrap_or_default().into();
        let retry = spec.retry;
        let factory = self
            .sink_factories
            .get(&kind)
            .with_context(|| format!("unknown sink type '{kind}'"))?;
        factory
            .build(id, spec.config, on_error, retry)
            .with_context(|| format!("failed to build sink '{kind}'"))
    }

    /// Builds a whole `Courier` from a `Config`. Mints hierarchical node
    /// ids (`{pipeline}/src`, `{pipeline}/t{i}`, `{pipeline}/sink{i}`) so
    /// logs and metrics can be traced back to the pipeline that owns them.
    pub fn build_courier(&self, config: Config) -> Result<Courier> {
        config.validate()?;

        let observability = config.observability;
        let mut pipelines = Vec::with_capacity(config.pipelines.len());
        for spec in config.pipelines {
            let name = spec.name.clone();
            let pipeline = self
                .build_pipeline(spec)
                .with_context(|| format!("failed to build pipeline '{name}'"))?;
            pipelines.push(pipeline);
        }
        Ok(Courier::new(pipelines).with_observability(observability))
    }

    fn build_pipeline(&self, spec: PipelineSpec) -> Result<Pipeline> {
        let name = spec.name;
        let source = self
            .build_source(&format!("{name}/src"), spec.source)
            .with_context(|| format!("pipeline '{name}' source"))?;

        let mut pipeline = Pipeline::new(&name, source);
        if let Some(capacity) = spec.channel_capacity {
            pipeline = pipeline.with_channel_capacity(capacity);
        }

        for (i, transform) in spec.transforms.into_iter().enumerate() {
            let id = format!("{name}/t{i}");
            pipeline = pipeline.with_transform(
                self.build_transform(&id, transform)
                    .with_context(|| format!("pipeline '{name}' transform[{i}]"))?,
            );
        }

        for (i, sink) in spec.sinks.into_iter().enumerate() {
            let id = format!("{name}/sink{i}");
            pipeline = pipeline.with_sink(
                self.build_sink(&id, sink)
                    .with_context(|| format!("pipeline '{name}' sink[{i}]"))?,
            );
        }

        Ok(pipeline)
    }

    /// Kinds currently registered for each category. Useful for
    /// diagnostics or for a `--list-components` CLI flag.
    pub fn source_kinds(&self) -> impl Iterator<Item = &str> {
        self.source_factories.keys().map(String::as_str)
    }

    pub fn transform_kinds(&self) -> impl Iterator<Item = &str> {
        self.transform_factories.keys().map(String::as_str)
    }

    pub fn sink_kinds(&self) -> impl Iterator<Item = &str> {
        self.sink_factories.keys().map(String::as_str)
    }
}

/// Registers every built-in source, transform and sink onto `registry`.
/// Each factory lives next to its component in the module tree, so this
/// function is purely orchestration — the concrete construction logic
/// and `*Config` structs are defined alongside their components.
///
/// Errors if any of the built-in `kind`s is already registered — callers
/// that want to override a built-in should do so *after* this call.
pub fn register_builtin(registry: &mut Registry) -> Result<()> {
    registry.register_source("api_poll", crate::sources::api::api_poll_source_factory)?;
    registry.register_source(
        "http_webhook",
        crate::sources::http_webhook::http_webhook_source_factory,
    )?;
    registry.register_source("kafka", crate::sources::kafka::kafka_source_factory)?;
    registry.register_source(
        "sql_query_poll",
        crate::sources::sql::sql_query_poll_source_factory,
    )?;
    registry.register_transform(
        "script",
        crate::transforms::script::script_transform_factory,
    )?;
    registry.register_transform(
        "set_key",
        crate::transforms::set_key::set_key_transform_factory,
    )?;
    registry.register_sink("api", crate::sinks::api::api_sink_factory)?;
    registry.register_sink("file", crate::sinks::file::file_sink_factory)?;
    registry.register_sink("kafka", crate::sinks::kafka::kafka_sink_factory)?;
    registry.register_sink("sql", crate::sinks::sql::sql_sink_factory)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::anyhow;
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tokio::sync::mpsc::{Receiver, Sender};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::config::{ErrorPolicyConfig, PipelineSpec, SinkSpec, SourceSpec, TransformSpec};
    use crate::envelope::Envelope;
    use crate::retry::RetryPolicy;

    struct NoopSource(String);

    #[async_trait]
    impl Source for NoopSource {
        fn id(&self) -> &str {
            &self.0
        }

        async fn run(self: Box<Self>, _tx: Sender<Envelope>, _cancel: CancellationToken) {}
    }

    struct NoopTransform(String);

    #[async_trait]
    impl Transform for NoopTransform {
        fn id(&self) -> &str {
            &self.0
        }

        async fn run(
            self: Box<Self>,
            _rx: Receiver<Envelope>,
            _tx: Sender<Envelope>,
            _cancel: CancellationToken,
        ) {
        }
    }

    struct NoopSink(String);

    #[async_trait]
    impl Sink for NoopSink {
        fn id(&self) -> &str {
            &self.0
        }

        async fn run(self: Box<Self>, _rx: Receiver<Envelope>, _cancel: CancellationToken) {}
    }

    fn noop_source(id: &str, _: Value, _: Option<RetryPolicy>) -> Result<Box<dyn Source>> {
        Ok(Box::new(NoopSource(id.to_string())))
    }

    fn noop_transform(id: &str, _: Value, _: ErrorPolicy) -> Result<Box<dyn Transform>> {
        Ok(Box::new(NoopTransform(id.to_string())))
    }

    fn noop_sink(
        id: &str,
        _: Value,
        _: ErrorPolicy,
        _: Option<RetryPolicy>,
    ) -> Result<Box<dyn Sink>> {
        Ok(Box::new(NoopSink(id.to_string())))
    }

    /// Registry preloaded with `noop` source/transform/sink — used by
    /// tests that care about pipeline wiring, not component behavior.
    fn noop_registry() -> Registry {
        let mut r = Registry::default();
        r.register_source("noop", noop_source).unwrap();
        r.register_transform("noop", noop_transform).unwrap();
        r.register_sink("noop", noop_sink).unwrap();
        r
    }

    fn noop_source_spec() -> SourceSpec {
        SourceSpec {
            kind: "noop".into(),
            config: json!({}),
            retry: None,
        }
    }

    fn noop_transform_spec(on_error: Option<ErrorPolicyConfig>) -> TransformSpec {
        TransformSpec {
            kind: "noop".into(),
            config: json!({}),
            on_error,
        }
    }

    fn noop_sink_spec(on_error: Option<ErrorPolicyConfig>) -> SinkSpec {
        SinkSpec {
            kind: "noop".into(),
            config: json!({}),
            on_error,
            retry: None,
        }
    }

    // -----------------------------------------------------------------
    // Registration
    // -----------------------------------------------------------------

    #[test]
    fn rejects_duplicate_source_registration() {
        let mut registry = Registry::default();
        registry.register_source("dup", noop_source).unwrap();
        let err = registry.register_source("dup", noop_source).unwrap_err();
        assert!(
            err.to_string()
                .contains("source factory 'dup' already registered")
        );
    }

    #[test]
    fn rejects_duplicate_transform_registration() {
        let mut registry = Registry::default();
        registry.register_transform("dup", noop_transform).unwrap();
        let err = registry
            .register_transform("dup", noop_transform)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("transform factory 'dup' already registered")
        );
    }

    #[test]
    fn rejects_duplicate_sink_registration() {
        let mut registry = Registry::default();
        registry.register_sink("dup", noop_sink).unwrap();
        let err = registry.register_sink("dup", noop_sink).unwrap_err();
        assert!(
            err.to_string()
                .contains("sink factory 'dup' already registered")
        );
    }

    #[test]
    fn same_kind_across_categories_does_not_collide() {
        // "kafka" registered as both source and sink is supported — each
        // category has its own namespace.
        let mut registry = Registry::default();
        registry.register_source("kafka", noop_source).unwrap();
        registry.register_sink("kafka", noop_sink).unwrap();
    }

    // -----------------------------------------------------------------
    // Lookup errors
    // -----------------------------------------------------------------

    #[test]
    fn reports_unknown_source_type() {
        let registry = Registry::default();
        let err = registry
            .build_source("p/src", noop_source_spec_with_kind("missing"))
            .err()
            .expect("expected unknown-kind error");
        assert!(err.to_string().contains("unknown source type 'missing'"));
    }

    #[test]
    fn reports_unknown_transform_type() {
        let registry = Registry::default();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "missing".into(),
                    config: json!({}),
                    on_error: None,
                },
            )
            .err()
            .expect("expected unknown-kind error");
        assert!(err.to_string().contains("unknown transform type 'missing'"));
    }

    #[test]
    fn reports_unknown_sink_type() {
        let registry = Registry::default();
        let err = registry
            .build_sink(
                "p/sink0",
                SinkSpec {
                    kind: "missing".into(),
                    config: json!({}),
                    on_error: None,
                    retry: None,
                },
            )
            .err()
            .expect("expected unknown-kind error");
        assert!(err.to_string().contains("unknown sink type 'missing'"));
    }

    fn noop_source_spec_with_kind(kind: &str) -> SourceSpec {
        SourceSpec {
            kind: kind.into(),
            config: json!({}),
            retry: None,
        }
    }

    // -----------------------------------------------------------------
    // Built-ins
    // -----------------------------------------------------------------

    #[test]
    fn with_builtins_registers_every_builtin_kind() {
        let registry = Registry::with_builtins().unwrap();

        let mut sources: Vec<_> = registry.source_kinds().collect();
        sources.sort();
        assert_eq!(
            sources,
            vec!["api_poll", "http_webhook", "kafka", "sql_query_poll"]
        );

        let mut transforms: Vec<_> = registry.transform_kinds().collect();
        transforms.sort();
        assert_eq!(transforms, vec!["script", "set_key"]);

        let mut sinks: Vec<_> = registry.sink_kinds().collect();
        sinks.sort();
        assert_eq!(sinks, vec!["api", "file", "kafka", "sql"]);
    }

    #[test]
    fn register_builtin_fails_on_second_call() {
        let mut registry = Registry::default();
        register_builtin(&mut registry).unwrap();
        let err = register_builtin(&mut registry).unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }

    // Per-factory behavior (spec parsing, error messages) is tested next
    // to each component — e.g. `transforms::set_key::tests`. The registry
    // tests here only cover lookup and wiring concerns.

    // -----------------------------------------------------------------
    // build_courier
    // -----------------------------------------------------------------

    #[test]
    fn build_courier_with_empty_config_yields_zero_pipelines() {
        let registry = noop_registry();
        let courier = registry.build_courier(Config::default()).unwrap();
        // Nothing to assert beyond "no panic" — Courier has private fields.
        // Spawning produces zero handles; run-time behavior tested elsewhere.
        let handles = courier.spawn(CancellationToken::new());
        assert!(handles.is_empty());
    }

    #[test]
    fn build_courier_assigns_hierarchical_node_ids() {
        // Factories record the id they receive so we can verify the scheme.
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));

        let mut registry = Registry::default();
        let rec = recorded.clone();
        registry
            .register_source("rec", move |id: &str, _: Value, _: Option<RetryPolicy>| {
                rec.lock().unwrap().push(id.to_string());
                Ok(Box::new(NoopSource(id.into())) as Box<dyn Source>)
            })
            .unwrap();
        let rec = recorded.clone();
        registry
            .register_transform("rec", move |id: &str, _: Value, _: ErrorPolicy| {
                rec.lock().unwrap().push(id.to_string());
                Ok(Box::new(NoopTransform(id.into())) as Box<dyn Transform>)
            })
            .unwrap();
        let rec = recorded.clone();
        registry
            .register_sink(
                "rec",
                move |id: &str, _: Value, _: ErrorPolicy, _: Option<RetryPolicy>| {
                    rec.lock().unwrap().push(id.to_string());
                    Ok(Box::new(NoopSink(id.into())) as Box<dyn Sink>)
                },
            )
            .unwrap();

        registry
            .build_courier(Config {
                observability: None,
                pipelines: vec![PipelineSpec {
                    name: "my-pipeline".into(),
                    source: SourceSpec {
                        kind: "rec".into(),
                        config: json!({}),
                        retry: None,
                    },
                    transforms: vec![
                        TransformSpec {
                            kind: "rec".into(),
                            config: json!({}),
                            on_error: None,
                        },
                        TransformSpec {
                            kind: "rec".into(),
                            config: json!({}),
                            on_error: None,
                        },
                    ],
                    sinks: vec![
                        SinkSpec {
                            kind: "rec".into(),
                            config: json!({}),
                            on_error: None,
                            retry: None,
                        },
                        SinkSpec {
                            kind: "rec".into(),
                            config: json!({}),
                            on_error: None,
                            retry: None,
                        },
                    ],
                    channel_capacity: None,
                }],
            })
            .unwrap();

        let seen = recorded.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                "my-pipeline/src",
                "my-pipeline/t0",
                "my-pipeline/t1",
                "my-pipeline/sink0",
                "my-pipeline/sink1",
            ],
        );
    }

    #[test]
    fn build_courier_wraps_component_errors_with_pipeline_name() {
        let mut registry = Registry::default();
        registry
            .register_source("boom", |_: &str, _: Value, _: Option<RetryPolicy>| {
                Err(anyhow!("source blew up"))
            })
            .unwrap();
        registry.register_sink("noop", noop_sink).unwrap();

        let err = registry
            .build_courier(Config {
                observability: None,
                pipelines: vec![PipelineSpec {
                    name: "analytics".into(),
                    source: SourceSpec {
                        kind: "boom".into(),
                        config: json!({}),
                        retry: None,
                    },
                    transforms: vec![],
                    sinks: vec![noop_sink_spec(None)],
                    channel_capacity: None,
                }],
            })
            .err()
            .expect("expected factory error to propagate");

        let msg = format!("{err:#}");
        assert!(msg.contains("pipeline 'analytics'"), "{msg}");
        assert!(msg.contains("source 'boom'"), "{msg}");
        assert!(msg.contains("source blew up"), "{msg}");
    }

    #[test]
    fn propagates_on_error_to_factories() {
        let mut registry = Registry::default();
        registry.register_source("noop", noop_source).unwrap();

        let seen_tx = Arc::new(Mutex::new(Vec::new()));
        let seen = seen_tx.clone();
        registry
            .register_transform(
                "tracking",
                move |_: &str, _: Value, on_error: ErrorPolicy| {
                    seen_tx.lock().unwrap().push(on_error);
                    Ok(Box::new(NoopTransform("t".into())) as Box<dyn Transform>)
                },
            )
            .unwrap();

        let seen_sx = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen_sx.clone();
        registry
            .register_sink(
                "tracking",
                move |_: &str, _: Value, on_error: ErrorPolicy, _: Option<RetryPolicy>| {
                    seen_sx.lock().unwrap().push(on_error);
                    Ok(Box::new(NoopSink("s".into())) as Box<dyn Sink>)
                },
            )
            .unwrap();

        registry
            .build_courier(Config {
                observability: None,
                pipelines: vec![PipelineSpec {
                    name: "p".into(),
                    source: noop_source_spec(),
                    transforms: vec![
                        TransformSpec {
                            kind: "tracking".into(),
                            config: json!({}),
                            on_error: Some(ErrorPolicyConfig::FailPipeline),
                        },
                        TransformSpec {
                            kind: "tracking".into(),
                            config: json!({}),
                            on_error: None, // defaults to Drop
                        },
                    ],
                    sinks: vec![SinkSpec {
                        kind: "tracking".into(),
                        config: json!({}),
                        on_error: None,
                        retry: None,
                    }],
                    channel_capacity: Some(32),
                }],
            })
            .unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            vec![ErrorPolicy::FailPipeline, ErrorPolicy::Drop],
        );
        assert_eq!(*seen2.lock().unwrap(), vec![ErrorPolicy::Drop]);
    }

    #[test]
    fn ignored_channel_capacity_falls_back_to_pipeline_default() {
        // Smoke test: build_courier succeeds with capacity=None. The
        // default is enforced inside Pipeline::new; we just verify that
        // omission doesn't cause trouble.
        let registry = noop_registry();
        registry
            .build_courier(Config {
                observability: None,
                pipelines: vec![PipelineSpec {
                    name: "p".into(),
                    source: noop_source_spec(),
                    transforms: vec![noop_transform_spec(None)],
                    sinks: vec![noop_sink_spec(None)],
                    channel_capacity: None,
                }],
            })
            .unwrap();
    }
}
