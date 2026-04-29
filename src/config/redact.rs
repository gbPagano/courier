use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::{OnceLock, RwLock};

use serde_json::Value;

use crate::retry::RetryPolicy;

pub(crate) const REDACTED_SECRET: &str = "[redacted]";

static SECRET_VALUES: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

fn secret_values() -> &'static RwLock<HashSet<String>> {
    SECRET_VALUES.get_or_init(|| RwLock::new(HashSet::new()))
}

pub(crate) fn register_secret_value(value: &str) {
    if value.is_empty() {
        return;
    }
    if let Ok(mut values) = secret_values().write() {
        values.insert(value.to_string());
    }
}

fn is_secret_value(value: &str) -> bool {
    secret_values()
        .read()
        .map(|values| values.contains(value))
        .unwrap_or(true)
}

pub(super) fn redact_secret_values_in_text(text: &str) -> String {
    let Ok(values) = secret_values().read() else {
        return REDACTED_SECRET.to_string();
    };

    let mut secrets = values
        .iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));

    let mut redacted = text.to_string();
    for secret in secrets {
        redacted = redacted.replace(secret.as_str(), REDACTED_SECRET);
        let serialized = serde_json::to_string(secret.as_str()).unwrap_or_default();
        if serialized.len() >= 2 {
            let escaped = &serialized[1..serialized.len() - 1];
            if escaped != secret.as_str() {
                redacted = redacted.replace(escaped, REDACTED_SECRET);
            }
        }
    }
    redacted
}

pub fn redact_secret(value: &str) -> RedactedDisplay<'_> {
    RedactedDisplay(value)
}

pub fn redact_secret_path(path: &Path) -> RedactedPathDisplay<'_> {
    RedactedPathDisplay(path)
}

pub struct RedactedDisplay<'a>(&'a str);

impl fmt::Display for RedactedDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if is_secret_value(self.0) {
            f.write_str(REDACTED_SECRET)
        } else {
            f.write_str(self.0)
        }
    }
}

pub struct RedactedPathDisplay<'a>(&'a Path);

impl fmt::Display for RedactedPathDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.0.to_string_lossy();
        if is_secret_value(&value) {
            f.write_str(REDACTED_SECRET)
        } else {
            fmt::Display::fmt(&self.0.display(), f)
        }
    }
}

pub(super) struct RedactedStr<'a>(pub &'a str);

impl fmt::Debug for RedactedStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if is_secret_value(self.0) {
            f.write_str("\"")?;
            f.write_str(REDACTED_SECRET)?;
            f.write_str("\"")
        } else {
            fmt::Debug::fmt(self.0, f)
        }
    }
}

pub(super) struct RedactedOptionStr<'a>(pub Option<&'a str>);

impl fmt::Debug for RedactedOptionStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(value) => f.debug_tuple("Some").field(&RedactedStr(value)).finish(),
            None => f.write_str("None"),
        }
    }
}

pub(super) struct RedactedJsonValue<'a>(pub &'a Value);

impl fmt::Debug for RedactedJsonValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&redacted_json_value(self.0), f)
    }
}

fn redacted_json_value(value: &Value) -> Value {
    match value {
        Value::String(value) if is_secret_value(value) => {
            Value::String(REDACTED_SECRET.to_string())
        }
        Value::Array(values) => Value::Array(values.iter().map(redacted_json_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), redacted_json_value(value)))
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

pub(super) struct RedactedOptionRetryPolicy<'a>(pub Option<&'a RetryPolicy>);

impl fmt::Debug for RedactedOptionRetryPolicy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(value) => f
                .debug_tuple("Some")
                .field(&RedactedRetryPolicy(value))
                .finish(),
            None => f.write_str("None"),
        }
    }
}

struct RedactedRetryPolicy<'a>(&'a RetryPolicy);

impl fmt::Debug for RedactedRetryPolicy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let policy = self.0;
        f.debug_struct("RetryPolicy")
            .field("max_attempts", &policy.max_attempts)
            .field("initial_delay_ms", &policy.initial_delay_ms)
            .field("backoff_multiplier", &policy.backoff_multiplier)
            .field("max_delay_ms", &policy.max_delay_ms)
            .field(
                "on_exhausted",
                &RedactedExhaustedPolicy(&policy.on_exhausted),
            )
            .finish()
    }
}

struct RedactedExhaustedPolicy<'a>(&'a crate::retry::ExhaustedPolicy);

impl fmt::Debug for RedactedExhaustedPolicy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            crate::retry::ExhaustedPolicy::Propagate => f.write_str("Propagate"),
            crate::retry::ExhaustedPolicy::DeadLetter { path } => f
                .debug_struct("DeadLetter")
                .field("path", &RedactedPath(path))
                .finish(),
        }
    }
}

struct RedactedPath<'a>(&'a Path);

impl fmt::Debug for RedactedPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.0.to_string_lossy();
        if is_secret_value(&value) {
            f.write_str("\"")?;
            f.write_str(REDACTED_SECRET)?;
            f.write_str("\"")
        } else {
            fmt::Debug::fmt(self.0, f)
        }
    }
}

#[cfg(test)]
pub(crate) fn clear_secret_values_for_test() {
    if let Ok(mut values) = secret_values().write() {
        values.clear();
    }
}
