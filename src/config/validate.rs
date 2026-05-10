use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use tracing_subscriber::EnvFilter;

use crate::retry::RetryPolicy;

use super::observability::ObservabilityConfig;
use super::redact::{redact_secret, redact_secret_path};
use super::types::{Config, FanOutPolicyConfig, HealthConfig, ShutdownConfig};

impl Config {
    pub fn validate(&self) -> Result<()> {
        if let Some(obs) = &self.observability {
            validate_observability(obs)?;
        }

        if let Some(health) = &self.health {
            validate_health(health)?;
        }

        if let Some(shutdown) = &self.shutdown {
            validate_shutdown(shutdown)?;
        }

        let mut seen_names: HashMap<&str, usize> = HashMap::new();

        for (pipeline_index, pipeline) in self.pipelines.iter().enumerate() {
            let pipeline_path = format!("pipelines[{pipeline_index}]");
            let pipeline_label = pipeline_label(pipeline_index, &pipeline.name);

            if pipeline.name.trim().is_empty() {
                bail!("{pipeline_path}.name: pipeline name must not be empty");
            }

            if let Some(previous_index) = seen_names.insert(pipeline.name.as_str(), pipeline_index)
            {
                bail!(
                    "{pipeline_path}.name: duplicate pipeline name '{}' (already defined at pipelines[{previous_index}].name)",
                    redact_secret(&pipeline.name)
                );
            }

            if pipeline.source.kind.trim().is_empty() {
                bail!("{pipeline_label}.source.type: source type must not be empty");
            }

            if let Some(retry) = &pipeline.source.retry {
                validate_retry_policy(retry, &format!("{pipeline_label}.source.retry"), false)?;
            }

            if matches!(pipeline.channel_capacity, Some(0)) {
                bail!("{pipeline_label}.channel_capacity: must be greater than 0");
            }

            if pipeline.sinks.is_empty() {
                bail!("{pipeline_label}.sinks: at least one sink is required");
            }

            if pipeline.sinks.len() == 1 && pipeline.fan_out != FanOutPolicyConfig::Broadcast {
                let value = pipeline.fan_out.as_str();
                bail!(
                    "{pipeline_label}.fan_out = \"{value}\": only meaningful with multiple sinks; remove it or add another sink"
                );
            }

            for (transform_index, transform) in pipeline.transforms.iter().enumerate() {
                if transform.kind.trim().is_empty() {
                    bail!(
                        "{pipeline_label}.transforms[{transform_index}].type: transform type must not be empty"
                    );
                }
            }

            for (sink_index, sink) in pipeline.sinks.iter().enumerate() {
                if sink.kind.trim().is_empty() {
                    bail!("{pipeline_label}.sinks[{sink_index}].type: sink type must not be empty");
                }
                if let Some(retry) = &sink.retry {
                    validate_retry_policy(
                        retry,
                        &format!("{pipeline_label}.sinks[{sink_index}].retry"),
                        true,
                    )?;
                }
            }
        }

        Ok(())
    }
}

fn pipeline_label(index: usize, name: &str) -> String {
    if name.trim().is_empty() {
        format!("pipelines[{index}]")
    } else {
        format!("pipeline '{}'", redact_secret(name))
    }
}

fn validate_observability(obs: &ObservabilityConfig) -> Result<()> {
    if !(0.0..=1.0).contains(&obs.tracing.sample_ratio) || !obs.tracing.sample_ratio.is_finite() {
        bail!(
            "observability.tracing.sample_ratio: must be a finite value in [0.0, 1.0] (got {})",
            obs.tracing.sample_ratio
        );
    }
    if obs.service_name.trim().is_empty() {
        bail!("observability.service_name: must not be empty");
    }
    if obs.metrics.export_interval_ms == 0 {
        bail!("observability.metrics.export_interval_ms: must be greater than 0");
    }
    if let Some(level) = &obs.log_level {
        if level.trim().is_empty() {
            bail!("observability.log_level: must not be empty when set");
        }
        EnvFilter::try_new(level)
            .with_context(|| "observability.log_level: invalid log filter directive")?;
    }
    Ok(())
}

fn validate_retry_policy(policy: &RetryPolicy, path: &str, allow_dead_letter: bool) -> Result<()> {
    if policy.max_attempts == 0 {
        bail!("{path}.max_attempts: must be greater than or equal to 1");
    }

    if policy.max_attempts > 1 {
        if policy.initial_delay_ms == 0 {
            bail!("{path}.initial_delay_ms: must be greater than 0 when max_attempts > 1");
        }
        if policy.max_delay_ms == 0 {
            bail!("{path}.max_delay_ms: must be greater than 0 when max_attempts > 1");
        }
    }

    if !policy.backoff_multiplier.is_finite() || policy.backoff_multiplier < 1.0 {
        bail!("{path}.backoff_multiplier: must be finite and greater than or equal to 1.0");
    }

    if policy.max_delay_ms < policy.initial_delay_ms {
        bail!("{path}.max_delay_ms: must be greater than or equal to initial_delay_ms");
    }

    if let crate::retry::ExhaustedPolicy::DeadLetter { path: dlq_path } = &policy.on_exhausted {
        if !allow_dead_letter {
            bail!("{path}.on_exhausted: dead_letter is only supported for sink retry policies");
        }

        if dlq_path.as_os_str().is_empty() {
            bail!("{path}.on_exhausted.path: dead-letter path must not be empty");
        }

        if let Some(parent) = dlq_path.parent()
            && !parent.as_os_str().is_empty()
        {
            if !parent.exists() {
                bail!(
                    "{path}.on_exhausted.path: parent directory '{}' does not exist",
                    redact_secret_path(parent)
                );
            }
            if !parent.is_dir() {
                bail!(
                    "{path}.on_exhausted.path: parent '{}' is not a directory",
                    redact_secret_path(parent)
                );
            }
        }
    }

    Ok(())
}

fn validate_health(health: &HealthConfig) -> Result<()> {
    if health.address.trim().is_empty() {
        bail!("health.address: must not be empty");
    }
    if health.address.parse::<std::net::SocketAddr>().is_err() {
        bail!(
            "health.address: invalid socket address '{}'; expected format is HOST:PORT (e.g. \"0.0.0.0:9090\")",
            redact_secret(&health.address)
        );
    }
    Ok(())
}

fn validate_shutdown(shutdown: &ShutdownConfig) -> Result<()> {
    if shutdown.timeout_secs == 0 {
        bail!("shutdown.timeout_secs: must be greater than 0");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    fn minimal_pipeline_block() -> &'static str {
        r#"
            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
        "#
    }

    #[test]
    fn validate_rejects_empty_pipeline_name() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "  "
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("pipelines[0].name"), "{msg}");
        assert!(msg.contains("must not be empty"), "{msg}");
    }

    #[test]
    fn validate_rejects_duplicate_pipeline_names_in_single_config() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "dup"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"

            [[pipelines]]
            name = "dup"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("pipelines[1].name"), "{msg}");
        assert!(msg.contains("duplicate pipeline name 'dup'"), "{msg}");
    }

    #[test]
    fn validate_rejects_zero_channel_capacity() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"
            channel_capacity = 0
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("pipeline 'p'.channel_capacity"), "{msg}");
        assert!(msg.contains("greater than 0"), "{msg}");
    }

    #[test]
    fn validate_rejects_missing_sinks() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("pipeline 'p'.sinks"), "{msg}");
        assert!(msg.contains("at least one sink is required"), "{msg}");
    }

    #[test]
    fn validate_rejects_fan_out_with_single_sink() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"
            fan_out = "broadcast_with_drop"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(
            msg.contains("pipeline 'p'.fan_out = \"broadcast_with_drop\""),
            "{msg}"
        );
        assert!(msg.contains("multiple sinks"), "{msg}");
    }

    #[test]
    fn validate_rejects_invalid_source_retry_policy() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "api_poll"

            [pipelines.source.retry]
            max_attempts = 0
            initial_delay_ms = 1
            backoff_multiplier = 1.0
            max_delay_ms = 1

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(
            msg.contains("pipeline 'p'.source.retry.max_attempts"),
            "{msg}"
        );
    }

    #[test]
    fn validate_rejects_source_retry_dead_letter() {
        let dir = tempfile::tempdir().unwrap();
        let dlq_path = dir.path().join("source-dlq.jsonl");
        let config = Config::from_toml_str(&format!(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "api_poll"

            [pipelines.source.retry]
            max_attempts = 3
            initial_delay_ms = 1
            backoff_multiplier = 1.0
            max_delay_ms = 1
            on_exhausted = {{ kind = "dead_letter", path = "{}" }}

            [[pipelines.sinks]]
            type = "noop"
            "#,
            dlq_path.display()
        ))
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(
            msg.contains("pipeline 'p'.source.retry.on_exhausted"),
            "{msg}"
        );
        assert!(
            msg.contains("dead_letter is only supported for sink retry policies"),
            "{msg}"
        );
    }

    #[test]
    fn validate_rejects_invalid_retry_policy() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            [pipelines.sinks.retry]
            max_attempts = 0
            initial_delay_ms = 1
            backoff_multiplier = 1.0
            max_delay_ms = 1
            "#,
        )
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(
            msg.contains("pipeline 'p'.sinks[0].retry.max_attempts"),
            "{msg}"
        );
    }

    #[test]
    fn validate_rejects_dead_letter_parent_that_is_not_directory() {
        let dir = tempfile::tempdir().unwrap();
        let parent_file = dir.path().join("not-a-dir");
        std::fs::write(&parent_file, "").unwrap();
        let dlq_path = parent_file.join("dlq.jsonl");
        let config = Config::from_toml_str(&format!(
            r#"
            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            [pipelines.sinks.retry]
            max_attempts = 1
            initial_delay_ms = 0
            backoff_multiplier = 1.0
            max_delay_ms = 0
            on_exhausted = {{ kind = "dead_letter", path = "{}" }}
            "#,
            dlq_path.display()
        ))
        .unwrap();

        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(
            msg.contains("pipeline 'p'.sinks[0].retry.on_exhausted.path"),
            "{msg}"
        );
        assert!(msg.contains("is not a directory"), "{msg}");
    }

    #[test]
    fn validate_accepts_valid_health_address() {
        let config = Config::from_toml_str(&format!(
            r#"
            [health]
            address = "127.0.0.1:9090"
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.health.unwrap().address, "127.0.0.1:9090");
    }

    #[test]
    fn validate_rejects_empty_health_address() {
        let config = Config::from_toml_str(&format!(
            r#"
            [health]
            address = ""
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("health.address"), "{msg}");
        assert!(msg.contains("must not be empty"), "{msg}");
    }

    #[test]
    fn validate_rejects_invalid_health_address() {
        let config = Config::from_toml_str(&format!(
            r#"
            [health]
            address = "not-a-socket-addr"
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("health.address"), "{msg}");
        assert!(msg.contains("invalid socket address"), "{msg}");
    }

    #[test]
    fn validate_accepts_valid_shutdown_timeout() {
        let config = Config::from_toml_str(&format!(
            r#"
            [shutdown]
            timeout_secs = 60
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.shutdown.unwrap().timeout_secs, 60);
    }

    #[test]
    fn validate_rejects_zero_shutdown_timeout() {
        let config = Config::from_toml_str(&format!(
            r#"
            [shutdown]
            timeout_secs = 0
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("shutdown.timeout_secs"), "{msg}");
        assert!(msg.contains("greater than 0"), "{msg}");
    }

    #[test]
    fn validate_rejects_sample_ratio_above_one() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability.tracing]
            sample_ratio = 1.5
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("observability.tracing.sample_ratio"), "{msg}");
    }

    #[test]
    fn validate_rejects_negative_sample_ratio() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability.tracing]
            sample_ratio = -0.1
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        config.validate().unwrap_err();
    }

    #[test]
    fn validate_rejects_empty_service_name() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability]
            service_name = ""
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("observability.service_name"), "{msg}");
    }

    #[test]
    fn validate_rejects_zero_export_interval() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability.metrics]
            export_interval_ms = 0
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(
            msg.contains("observability.metrics.export_interval_ms"),
            "{msg}"
        );
    }

    #[test]
    fn validate_rejects_empty_log_level() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability]
            log_level = "   "
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("observability.log_level"), "{msg}");
    }

    #[test]
    fn validate_rejects_invalid_log_level() {
        let config = Config::from_toml_str(&format!(
            r#"
            [observability]
            log_level = "courier==debug"
            {}"#,
            minimal_pipeline_block()
        ))
        .unwrap();
        let msg = format!("{:#}", config.validate().unwrap_err());
        assert!(msg.contains("observability.log_level"), "{msg}");
        assert!(msg.contains("invalid log filter directive"), "{msg}");
    }
}
