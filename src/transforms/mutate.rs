use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::config::parse_config;
use crate::envelope::Envelope;
use crate::pipeline::ErrorPolicy;
use crate::transforms::{BasicTransform, MapOne, Transform};

/// Performs structural changes to an envelope without a scripting runtime.
///
/// Operations are applied in order. Each operation targets a dotted path
/// inside the envelope payload (e.g. `payload.user.id`). The root `payload`
/// is implicit — paths are relative to the payload object.
///
/// Supported operations:
/// - `add_field` — insert or overwrite a field. Creates intermediate objects
///   as needed.
/// - `remove_field` — delete a field. In strict mode the field must exist.
/// - `rename_field` — move a value from one path to another. In strict mode
///   the source must exist.
/// - `cast` — convert a scalar value to another JSON type (`string`, `int`,
///   `float`, `bool`, `json`).
///
/// # Missing-field handling
/// `on_missing` controls behavior when an operation references a path that
/// does not exist:
/// - `strict` (default) — the transform returns an error, which is handled
///   by the configured `ErrorPolicy`.
/// - `lenient` — the operation is silently skipped.
pub struct MutateTransform {
    id: String,
    operations: Vec<Operation>,
    on_missing: MissingMode,
}

impl MutateTransform {
    pub fn new(id: impl Into<String>, operations: Vec<Operation>, on_missing: MissingMode) -> Self {
        Self {
            id: id.into(),
            operations,
            on_missing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingMode {
    #[default]
    Strict,
    Lenient,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Operation {
    AddField {
        path: String,
        value: Value,
    },
    RemoveField {
        path: String,
    },
    RenameField {
        from: String,
        to: String,
    },
    Cast {
        path: String,
        #[serde(rename = "to")]
        to_type: CastType,
    },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CastType {
    String,
    Int,
    Float,
    Bool,
    Json,
}

#[async_trait]
impl MapOne for MutateTransform {
    fn id(&self) -> &str {
        &self.id
    }

    async fn map(&self, mut env: Envelope) -> Result<Option<Envelope>> {
        for op in &self.operations {
            apply_operation(&mut env, op, self.on_missing)?;
        }
        Ok(Some(env))
    }
}

fn apply_operation(env: &mut Envelope, op: &Operation, mode: MissingMode) -> Result<()> {
    match op {
        Operation::AddField { path, value } => {
            if mode == MissingMode::Strict {
                set_path(&mut env.payload, path, value.clone(), true)?;
            } else {
                set_path_lenient(&mut env.payload, path, value.clone())?;
            }
        }
        Operation::RemoveField { path } => match remove_path(&mut env.payload, path)? {
            Some(removed) => {
                if !removed && mode == MissingMode::Strict {
                    bail!("mutate: field '{}' does not exist", path);
                }
            }
            None => {
                if mode == MissingMode::Strict {
                    bail!("mutate: field '{}' does not exist", path);
                }
            }
        },
        Operation::RenameField { from, to } => match remove_path_value(&mut env.payload, from)? {
            Some(Some(v)) => {
                set_path(&mut env.payload, to, v, true)?;
            }
            Some(None) => {
                if mode == MissingMode::Strict {
                    bail!("mutate: field '{}' does not exist", from);
                }
            }
            None => {
                if mode == MissingMode::Strict {
                    bail!("mutate: field '{}' does not exist", from);
                }
            }
        },
        Operation::Cast { path, to_type } => {
            let current = get_path_mut(&mut env.payload, path)?;
            match current {
                Some(v) => {
                    *v = cast_value(v, *to_type)?;
                }
                None => {
                    if mode == MissingMode::Strict {
                        bail!("mutate: field '{}' does not exist", path);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Splits a dotted path into segments.
fn split_path(path: &str) -> Vec<&str> {
    path.split('.').collect()
}

/// Navigate to the parent object of the last segment, optionally creating
/// intermediate objects.
///
/// Returns `Ok(None)` when an intermediate segment is missing and `create` is
/// `false`. Callers in lenient mode should treat `None` as "path does not
/// exist" and skip the operation.
fn navigate_to_parent<'a>(
    root: &'a mut Value,
    path: &str,
    create: bool,
) -> Result<Option<(&'a mut serde_json::Map<String, Value>, String)>> {
    let segments = split_path(path);
    if segments.is_empty() {
        bail!("mutate: empty path");
    }

    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        if create {
            if !current.is_object() {
                bail!("mutate: cannot create field inside non-object");
            }
            let map = current.as_object_mut().unwrap();
            if !map.contains_key(*segment) {
                map.insert(segment.to_string(), Value::Object(serde_json::Map::new()));
            }
            current = map.get_mut(*segment).unwrap();
        } else {
            match current {
                Value::Object(map) => {
                    current = match map.get_mut(*segment) {
                        Some(v) => v,
                        None => return Ok(None),
                    };
                }
                _ => return Ok(None),
            }
        }
    }

    match current {
        Value::Object(map) => Ok(Some((map, segments.last().unwrap().to_string()))),
        _ => {
            if create {
                bail!("mutate: cannot create field inside non-object")
            } else {
                Ok(None)
            }
        }
    }
}

fn set_path(root: &mut Value, path: &str, value: Value, create: bool) -> Result<()> {
    let (map, key) = navigate_to_parent(root, path, create)?
        .ok_or_else(|| anyhow::anyhow!("mutate: path '{}' does not exist", path))?;
    map.insert(key, value);
    Ok(())
}

fn set_path_lenient(root: &mut Value, path: &str, value: Value) -> Result<()> {
    if let Some((map, key)) = navigate_to_parent_lenient_create(root, path)? {
        map.insert(key, value);
    }
    Ok(())
}

fn navigate_to_parent_lenient_create<'a>(
    root: &'a mut Value,
    path: &str,
) -> Result<Option<(&'a mut serde_json::Map<String, Value>, String)>> {
    let segments = split_path(path);
    if segments.is_empty() {
        bail!("mutate: empty path");
    }

    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        if !current.is_object() {
            return Ok(None);
        }

        let map = current.as_object_mut().unwrap();
        if !map.contains_key(*segment) {
            map.insert(segment.to_string(), Value::Object(serde_json::Map::new()));
        }
        current = map.get_mut(*segment).unwrap();
    }

    match current {
        Value::Object(map) => Ok(Some((map, segments.last().unwrap().to_string()))),
        _ => Ok(None),
    }
}

fn remove_path(root: &mut Value, path: &str) -> Result<Option<bool>> {
    Ok(navigate_to_parent(root, path, false)?.map(|(map, key)| map.remove(&key).is_some()))
}

fn remove_path_value(root: &mut Value, path: &str) -> Result<Option<Option<Value>>> {
    Ok(navigate_to_parent(root, path, false)?.map(|(map, key)| map.remove(&key)))
}

fn get_path_mut<'a>(root: &'a mut Value, path: &str) -> Result<Option<&'a mut Value>> {
    let segments = split_path(path);
    if segments.is_empty() {
        bail!("mutate: empty path");
    }

    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        match current {
            Value::Object(map) => {
                current = match map.get_mut(*segment) {
                    Some(v) => v,
                    None => return Ok(None),
                };
            }
            _ => return Ok(None),
        }
    }

    match current {
        Value::Object(map) => {
            let key = segments.last().unwrap();
            Ok(map.get_mut(*key))
        }
        _ => Ok(None),
    }
}

fn cast_value(value: &Value, to: CastType) -> Result<Value> {
    match to {
        CastType::String => Ok(Value::String(match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })),
        CastType::Int => {
            let n = match value {
                Value::Number(n) => n
                    .as_i64()
                    .or_else(|| n.as_f64().map(|f| f as i64))
                    .unwrap_or(0),
                Value::String(s) => s.parse().unwrap_or(0),
                Value::Bool(b) => i64::from(*b),
                Value::Null => 0,
                _ => bail!("mutate: cannot cast {} to int", value),
            };
            Ok(Value::Number(serde_json::Number::from(n)))
        }
        CastType::Float => {
            let f = match value {
                Value::Number(n) => n.as_f64().unwrap_or(0.0),
                Value::String(s) => s.parse().unwrap_or(0.0),
                Value::Bool(b) => f64::from(*b),
                Value::Null => 0.0,
                _ => bail!("mutate: cannot cast {} to float", value),
            };
            Ok(Value::Number(
                serde_json::Number::from_f64(f).unwrap_or_else(|| serde_json::Number::from(0)),
            ))
        }
        CastType::Bool => {
            let b = match value {
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
                Value::String(s) => !s.is_empty() && s != "false" && s != "0",
                Value::Null => false,
                _ => bail!("mutate: cannot cast {} to bool", value),
            };
            Ok(Value::Bool(b))
        }
        CastType::Json => match value {
            Value::String(s) => serde_json::from_str(s)
                .map_err(|err| anyhow::anyhow!("mutate: cannot cast string to json: {err}")),
            other => Ok(other.clone()),
        },
    }
}

// ------------------------------------------------------------------
// Config + Factory
// ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MutateTransformConfig {
    operations: Vec<Operation>,
    #[serde(default)]
    on_missing: MissingMode,
}

/// Registry factory for [`MutateTransform`]. Registered by
/// `courier::registry::register_builtin` under kind `"mutate"`.
pub fn mutate_transform_factory(
    id: &str,
    config: Value,
    on_error: ErrorPolicy,
) -> Result<Box<dyn Transform>> {
    let config: MutateTransformConfig = parse_config("mutate", config)?;
    Ok(Box::new(
        BasicTransform::new(MutateTransform::new(
            id,
            config.operations,
            config.on_missing,
        ))
        .with_error_policy(on_error),
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::Registry;
    use crate::config::{ErrorPolicyConfig, TransformSpec};
    use crate::envelope::Envelope;

    #[tokio::test]
    async fn add_field_creates_value() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::AddField {
                path: "user.name".into(),
                value: json!("alice"),
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({}));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload["user"]["name"], "alice");
    }

    #[tokio::test]
    async fn add_field_overwrites_existing() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::AddField {
                path: "user.name".into(),
                value: json!("bob"),
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({ "user": { "name": "alice" } }));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload["user"]["name"], "bob");
    }

    #[tokio::test]
    async fn add_field_lenient_creates_missing_intermediate_path() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::AddField {
                path: "user.name".into(),
                value: json!("alice"),
            }],
            MissingMode::Lenient,
        );
        let env = Envelope::new("src", json!({}));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload, json!({ "user": { "name": "alice" } }));
    }

    #[tokio::test]
    async fn add_field_lenient_skips_non_object_parent() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::AddField {
                path: "user.name".into(),
                value: json!("alice"),
            }],
            MissingMode::Lenient,
        );
        let env = Envelope::new("src", json!({ "user": 1 }));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload, json!({ "user": 1 }));
    }

    #[tokio::test]
    async fn remove_field_deletes_value() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::RemoveField {
                path: "old_field".into(),
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({ "old_field": 1, "keep": 2 }));
        let out = t.map(env).await.unwrap().unwrap();
        assert!(!out.payload.as_object().unwrap().contains_key("old_field"));
        assert_eq!(out.payload["keep"], 2);
    }

    #[tokio::test]
    async fn remove_field_strict_errors_when_missing() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::RemoveField {
                path: "missing".into(),
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({}));
        assert!(t.map(env).await.is_err());
    }

    #[tokio::test]
    async fn remove_field_lenient_ignores_missing() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::RemoveField {
                path: "missing".into(),
            }],
            MissingMode::Lenient,
        );
        let env = Envelope::new("src", json!({}));
        assert!(t.map(env).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn rename_field_moves_value() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::RenameField {
                from: "old".into(),
                to: "new".into(),
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({ "old": 42 }));
        let out = t.map(env).await.unwrap().unwrap();
        assert!(!out.payload.as_object().unwrap().contains_key("old"));
        assert_eq!(out.payload["new"], 42);
    }

    #[tokio::test]
    async fn rename_field_lenient_ignores_missing() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::RenameField {
                from: "missing".into(),
                to: "new".into(),
            }],
            MissingMode::Lenient,
        );
        let env = Envelope::new("src", json!({}));
        assert!(t.map(env).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn cast_to_string() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::Cast {
                path: "value".into(),
                to_type: CastType::String,
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({ "value": 42 }));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload["value"], "42");
    }

    #[tokio::test]
    async fn cast_string_to_int() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::Cast {
                path: "value".into(),
                to_type: CastType::Int,
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({ "value": "99" }));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload["value"], 99);
    }

    #[tokio::test]
    async fn cast_to_bool() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::Cast {
                path: "value".into(),
                to_type: CastType::Bool,
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({ "value": 1 }));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload["value"], true);
    }

    #[tokio::test]
    async fn cast_string_to_json() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::Cast {
                path: "value".into(),
                to_type: CastType::Json,
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({ "value": "{\"nested\":true}" }));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload["value"], json!({ "nested": true }));
    }

    #[tokio::test]
    async fn cast_string_to_json_errors_when_invalid() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::Cast {
                path: "value".into(),
                to_type: CastType::Json,
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({ "value": "not json" }));
        let err = t.map(env).await.err().expect("expected invalid json error");
        assert!(
            err.to_string().contains("cannot cast string to json"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn multiple_operations_applied_in_order() {
        let t = MutateTransform::new(
            "t",
            vec![
                Operation::AddField {
                    path: "a".into(),
                    value: json!(1),
                },
                Operation::RenameField {
                    from: "a".into(),
                    to: "b".into(),
                },
            ],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({}));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload["b"], 1);
        assert!(!out.payload.as_object().unwrap().contains_key("a"));
    }

    #[test]
    fn factory_resolves_through_registry() {
        let registry = Registry::with_builtins().unwrap();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "mutate".into(),
                    config: json!({
                        "operations": [
                            { "type": "add_field", "path": "x", "value": 1 }
                        ]
                    }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                },
            )
            .unwrap();
    }

    #[test]
    fn factory_reports_invalid_config() {
        let registry = Registry::with_builtins().unwrap();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "mutate".into(),
                    config: json!({ "wrong_field": "x" }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected invalid-config error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid config for component type 'mutate'"),
            "{msg}",
        );
    }

    #[tokio::test]
    async fn cast_string_to_string_preserves_value() {
        let t = MutateTransform::new(
            "t",
            vec![Operation::Cast {
                path: "value".into(),
                to_type: CastType::String,
            }],
            MissingMode::Strict,
        );
        let env = Envelope::new("src", json!({ "value": "hello" }));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload["value"], "hello");
    }

    #[tokio::test]
    async fn lenient_skips_missing_intermediate_path() {
        let ops = vec![
            Operation::RemoveField {
                path: "a.b.c".into(),
            },
            Operation::RenameField {
                from: "x.y.z".into(),
                to: "w".into(),
            },
            Operation::Cast {
                path: "m.n.o".into(),
                to_type: CastType::String,
            },
        ];
        let t = MutateTransform::new("t", ops, MissingMode::Lenient);
        let env = Envelope::new("src", json!({}));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload, json!({}));
    }

    #[tokio::test]
    async fn lenient_skips_non_object_parent() {
        // When an intermediate segment is a scalar (not an object),
        // lenient mode should skip the operation instead of erroring.
        let ops = vec![
            Operation::RemoveField { path: "a.b".into() },
            Operation::RenameField {
                from: "a.c".into(),
                to: "d".into(),
            },
        ];
        let t = MutateTransform::new("t", ops, MissingMode::Lenient);
        let env = Envelope::new("src", json!({ "a": 1 }));
        let out = t.map(env).await.unwrap().unwrap();
        assert_eq!(out.payload, json!({ "a": 1 }));
    }
}
