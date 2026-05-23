use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::config::parse_config;
use crate::envelope::Envelope;
use crate::pipeline::ErrorPolicy;
use crate::transforms::{BasicTransform, MapOne, Transform};

/// Populates `meta.key` from a top-level field of the payload. String
/// values are used as-is; other JSON types are stringified via `to_string`.
/// Missing fields leave `meta.key` unchanged.
pub struct SetKeyTransform {
    id: String,
    from_field: String,
}

impl SetKeyTransform {
    pub fn new(id: impl Into<String>, from_field: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            from_field: from_field.into(),
        }
    }
}

#[async_trait]
impl MapOne for SetKeyTransform {
    fn id(&self) -> &str {
        &self.id
    }

    async fn map(&self, mut env: Envelope) -> Result<Option<Envelope>> {
        if let Some(v) = env.payload.get(&self.from_field) {
            env.meta.key = Some(match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
        }
        Ok(Some(env))
    }
}

#[derive(Debug, Deserialize)]
struct SetKeyTransformConfig {
    from_field: String,
}

/// Registry factory for [`SetKeyTransform`]. Registered by
/// `courier::registry::register_builtin` under kind `"set_key"`.
pub fn set_key_transform_factory(
    id: &str,
    config: Value,
    on_error: ErrorPolicy,
) -> Result<Box<dyn Transform>> {
    let config: SetKeyTransformConfig = parse_config("set_key", config)?;
    Ok(Box::new(
        BasicTransform::new(SetKeyTransform::new(id, config.from_field))
            .with_error_policy(on_error),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::Registry;
    use crate::config::{ErrorPolicyConfig, TransformSpec};

    #[tokio::test]
    async fn sets_key_from_string_field() {
        let t = SetKeyTransform::new("t", "user_id");
        let env = Envelope::new("src", json!({ "user_id": "abc" }));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.meta.key.as_deref(), Some("abc"));
    }

    #[tokio::test]
    async fn stringifies_non_string_field() {
        let t = SetKeyTransform::new("t", "id");
        let env = Envelope::new("src", json!({ "id": 42 }));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.meta.key.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn leaves_key_unchanged_when_missing() {
        let t = SetKeyTransform::new("t", "missing");
        let env = Envelope::new("src", json!({ "other": 1 }));
        let out = t.map(env).await.unwrap().unwrap();
        assert!(out.meta.key.is_none());
    }

    #[test]
    fn factory_resolves_through_registry() {
        // Happy path for the factory wired into `with_builtins`.
        let registry = Registry::with_builtins();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "set_key".into(),
                    config: json!({ "from_field": "user_id" }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                },
            )
            .unwrap();
    }

    #[test]
    fn factory_reports_invalid_config() {
        let registry = Registry::with_builtins();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "set_key".into(),
                    config: json!({ "wrong_field": "x" }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected invalid-config error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid config for component type 'set_key'"),
            "{msg}",
        );
    }
}
