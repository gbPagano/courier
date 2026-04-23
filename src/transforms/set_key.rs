use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::envelope::Envelope;
use crate::transforms::MapOne;

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
