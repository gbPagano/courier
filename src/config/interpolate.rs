use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::redact::register_secret_value;

pub(super) fn interpolate_config_value(value: &mut Value, base_dir: Option<&Path>) -> Result<()> {
    interpolate_value_at_path(value, "", base_dir)
}

fn interpolate_value_at_path(value: &mut Value, path: &str, base_dir: Option<&Path>) -> Result<()> {
    match value {
        Value::String(s) => {
            let resolved = interpolate_string(s, path, base_dir)?;
            *s = resolved;
        }
        Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                let child_path = if path.is_empty() {
                    format!("[{index}]")
                } else {
                    format!("{path}[{index}]")
                };
                interpolate_value_at_path(value, &child_path, base_dir)?;
            }
        }
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                let child_path = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                interpolate_value_at_path(value, &child_path, base_dir)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn interpolate_string(input: &str, path: &str, base_dir: Option<&Path>) -> Result<String> {
    if let Some(file_path) = strip_whole_file_reference(input) {
        if file_path.is_empty() {
            bail!("{path}: file reference path must not be empty");
        }
        let file_path = resolve_interpolated_file_path(file_path, base_dir);
        let mut content = std::fs::read_to_string(&file_path).with_context(|| {
            format!(
                "{path}: failed to read interpolated file {}",
                file_path.display()
            )
        })?;
        if content.ends_with('\n') {
            content.pop();
            if content.ends_with('\r') {
                content.pop();
            }
        }
        register_secret_value(&content);
        return Ok(content);
    }

    let (resolved, contained_secret) = substitute(input, path)?;
    if contained_secret {
        register_secret_value(&resolved);
    }
    Ok(resolved)
}

fn strip_whole_file_reference(input: &str) -> Option<&str> {
    let rest = input.strip_prefix("${file:")?;
    let inner = rest.strip_suffix('}')?;
    if inner.contains("${") || inner.contains('}') {
        return None;
    }
    Some(inner)
}

fn resolve_interpolated_file_path(path: &str, base_dir: Option<&Path>) -> PathBuf {
    let path = Path::new(path);
    if path.is_relative()
        && let Some(base_dir) = base_dir
    {
        return base_dir.join(path);
    }
    path.to_path_buf()
}

fn substitute(input: &str, path: &str) -> Result<(String, bool)> {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut contained_secret = false;

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let mut lookahead = chars.clone();
            if lookahead.next() == Some('$') && lookahead.next() == Some('{') {
                output.push('$');
                chars.next();
            } else {
                output.push(ch);
            }
            continue;
        }

        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut body = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '}' {
                    closed = true;
                    break;
                }
                body.push(c);
            }
            if !closed {
                bail!("{path}: unterminated reference '${{{body}'");
            }
            let (resolved, is_secret) = resolve_reference(&body, path)?;
            output.push_str(&resolved);
            if is_secret {
                contained_secret = true;
            }
            continue;
        }

        output.push(ch);
    }

    Ok((output, contained_secret))
}

fn resolve_reference(body: &str, path: &str) -> Result<(String, bool)> {
    let (kind, rest) = body.split_once(':').ok_or_else(|| {
        anyhow::anyhow!(
            "{path}: reference '${{{body}}}' must use a 'env:', 'secret:', or 'file:' prefix"
        )
    })?;

    match kind {
        "env" => {
            let (name, default) = match rest.split_once(':') {
                Some((name, default)) => (name, Some(default)),
                None => (rest, None),
            };
            if name.is_empty() {
                bail!("{path}: 'env' reference is missing a variable name");
            }
            match std::env::var(name) {
                Ok(value) => Ok((value, false)),
                Err(_) => match default {
                    Some(default) => Ok((default.to_string(), false)),
                    None => bail!("{path}: environment variable '{name}' is not set"),
                },
            }
        }
        "secret" => {
            if rest.is_empty() {
                bail!("{path}: 'secret' reference is missing a variable name");
            }
            if rest.contains(':') {
                bail!("{path}: '${{secret:...}}' does not support default values");
            }
            match std::env::var(rest) {
                Ok(value) => {
                    register_secret_value(&value);
                    Ok((value, true))
                }
                Err(_) => bail!("{path}: secret environment variable '{rest}' is not set"),
            }
        }
        "file" => {
            bail!("{path}: file references must occupy the whole value");
        }
        other => {
            bail!("{path}: unknown reference kind '{other}' (expected 'env', 'secret', or 'file')")
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{
        Config, ENV_LOCK, REDACTED_SECRET, clear_secret_values_for_test, redact_secret,
    };

    fn set_env_var(key: &str, value: &str) {
        unsafe {
            std::env::set_var(key, value);
        }
    }

    fn remove_env_var(key: &str) {
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn interpolates_env_references_without_treating_them_as_secrets() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();
        set_env_var("COURIER_TEST_PIPELINE_NAME", "env-pipeline");
        set_env_var("COURIER_TEST_API_URL", "https://example.test");

        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "${env:COURIER_TEST_PIPELINE_NAME}"

            [pipelines.source]
            type = "noop"
            url = "${env:COURIER_TEST_API_URL}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        assert_eq!(config.pipelines[0].name, "env-pipeline");
        assert_eq!(
            config.pipelines[0].source.config["url"],
            "https://example.test"
        );

        let debug = format!("{config:?}");
        assert!(debug.contains("env-pipeline"), "{debug}");
        assert!(debug.contains("https://example.test"), "{debug}");
        assert!(!debug.contains(REDACTED_SECRET), "{debug}");
        assert_eq!(
            redact_secret("env-pipeline/src").to_string(),
            "env-pipeline/src"
        );

        remove_env_var("COURIER_TEST_PIPELINE_NAME");
        remove_env_var("COURIER_TEST_API_URL");
    }

    #[test]
    fn interpolates_secret_references_and_redacts_in_debug() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();
        set_env_var("COURIER_TEST_API_TOKEN", "super-secret-token");

        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            token = "Bearer ${secret:COURIER_TEST_API_TOKEN}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.pipelines[0].source.config["token"],
            "Bearer super-secret-token"
        );

        let debug = format!("{config:?}");
        assert!(!debug.contains("super-secret-token"), "{debug}");
        assert!(debug.contains(REDACTED_SECRET), "{debug}");

        assert_eq!(redact_secret("p").to_string(), "p");
        assert_eq!(
            redact_secret("super-secret-token").to_string(),
            REDACTED_SECRET
        );
        assert_eq!(
            redact_secret("Bearer super-secret-token").to_string(),
            REDACTED_SECRET
        );

        remove_env_var("COURIER_TEST_API_TOKEN");
    }

    #[test]
    fn substring_matches_are_not_redacted() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();
        set_env_var("COURIER_TEST_TOKEN_MATCH", "tokenvalue");

        let _config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            token = "${secret:COURIER_TEST_TOKEN_MATCH}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        assert_eq!(redact_secret("tokenvalue").to_string(), REDACTED_SECRET);
        assert_eq!(
            redact_secret("tokenvalue/src").to_string(),
            "tokenvalue/src"
        );
        assert_eq!(
            redact_secret("prefix-tokenvalue").to_string(),
            "prefix-tokenvalue"
        );

        remove_env_var("COURIER_TEST_TOKEN_MATCH");
    }

    #[test]
    fn missing_env_reference_without_default_fails_with_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        remove_env_var("COURIER_TEST_REQUIRED_MISSING");

        let err = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            api_key = "${env:COURIER_TEST_REQUIRED_MISSING}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("pipelines[0].source.api_key"), "{msg}");
        assert!(msg.contains("COURIER_TEST_REQUIRED_MISSING"), "{msg}");
    }

    #[test]
    fn missing_env_reference_with_default_uses_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();
        remove_env_var("COURIER_TEST_OPTIONAL_MISSING");

        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            api_key = "${env:COURIER_TEST_OPTIONAL_MISSING:dev-token}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        assert_eq!(config.pipelines[0].source.config["api_key"], "dev-token");
        let debug = format!("{config:?}");
        assert!(debug.contains("dev-token"), "{debug}");
        assert!(!debug.contains(REDACTED_SECRET), "{debug}");
    }

    #[test]
    fn missing_secret_reference_fails_with_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        remove_env_var("COURIER_TEST_SECRET_MISSING");

        let err = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            token = "${secret:COURIER_TEST_SECRET_MISSING}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("pipelines[0].source.token"), "{msg}");
        assert!(msg.contains("COURIER_TEST_SECRET_MISSING"), "{msg}");
    }

    #[test]
    fn secret_reference_rejects_default_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        remove_env_var("COURIER_TEST_SECRET_NOPE");

        let err = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            token = "${secret:COURIER_TEST_SECRET_NOPE:fallback}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("does not support default"), "{msg}");
    }

    #[test]
    fn unknown_reference_kind_fails_with_path() {
        let err = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            x = "${foo:bar}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("unknown reference kind 'foo'"), "{msg}");
        assert!(msg.contains("pipelines[0].source.x"), "{msg}");
    }

    #[test]
    fn unprefixed_reference_fails_with_clear_error() {
        let err = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            x = "${NAME}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("'env:', 'secret:', or 'file:' prefix"),
            "{msg}"
        );
    }

    #[test]
    fn escaped_interpolation_yields_literal_template() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();
        set_env_var("COURIER_TEST_ESCAPED_TOKEN", "must-not-appear");

        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            literal = '\${env:COURIER_TEST_ESCAPED_TOKEN}'

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.pipelines[0].source.config["literal"],
            "${env:COURIER_TEST_ESCAPED_TOKEN}"
        );

        remove_env_var("COURIER_TEST_ESCAPED_TOKEN");
    }

    #[test]
    fn interpolation_preserves_literal_doubled_backslashes() {
        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            unc = '\\server\\share'
            regex = '\\d+\\w+'
            price = '\$5'

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let cfg = &config.pipelines[0].source.config;
        assert_eq!(cfg["unc"], r"\\server\\share");
        assert_eq!(cfg["regex"], r"\\d+\\w+");
        assert_eq!(cfg["price"], r"\$5");
    }

    #[test]
    fn interpolates_file_reference_relative_to_config_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();

        let dir = tempfile::tempdir().unwrap();
        let secret_dir = dir.path().join("secrets");
        std::fs::create_dir(&secret_dir).unwrap();
        std::fs::write(secret_dir.join("api-token"), "from-file-secret").unwrap();
        let config_path = dir.path().join("courier.toml");
        std::fs::write(
            &config_path,
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            token = "${file:secrets/api-token}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(
            config.pipelines[0].source.config["token"],
            "from-file-secret"
        );

        let debug = format!("{config:?}");
        assert!(!debug.contains("from-file-secret"), "{debug}");
        assert!(debug.contains(REDACTED_SECRET), "{debug}");
    }

    #[test]
    fn file_reference_must_occupy_the_whole_value() {
        let dir = tempfile::tempdir().unwrap();
        let secret_dir = dir.path().join("secrets");
        std::fs::create_dir(&secret_dir).unwrap();
        std::fs::write(secret_dir.join("api-token"), "from-file-secret").unwrap();
        let config_path = dir.path().join("courier.toml");
        std::fs::write(
            &config_path,
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            token = "Bearer ${file:secrets/api-token}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let err = Config::load(&config_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("file references must occupy the whole value"),
            "{msg}"
        );
    }

    #[test]
    fn file_reference_strips_single_trailing_newline() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lf"), "lf-secret\n").unwrap();
        std::fs::write(dir.path().join("crlf"), "crlf-secret\r\n").unwrap();
        std::fs::write(dir.path().join("double"), "double-secret\n\n").unwrap();
        std::fs::write(dir.path().join("plain"), "plain-secret").unwrap();
        let config_path = dir.path().join("courier.toml");
        std::fs::write(
            &config_path,
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            lf = "${file:lf}"
            crlf = "${file:crlf}"
            double = "${file:double}"
            plain = "${file:plain}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        let cfg = &config.pipelines[0].source.config;
        assert_eq!(cfg["lf"], "lf-secret");
        assert_eq!(cfg["crlf"], "crlf-secret");
        assert_eq!(cfg["double"], "double-secret\n");
        assert_eq!(cfg["plain"], "plain-secret");
    }

    #[test]
    fn interpolates_file_reference_with_absolute_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();

        let secret_dir = tempfile::tempdir().unwrap();
        let secret_path = secret_dir.path().join("absolute-secret");
        std::fs::write(&secret_path, "absolute-token").unwrap();

        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("courier.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
                [[pipelines]]
                name = "p"

                [pipelines.source]
                type = "noop"
                token = "${{file:{}}}"

                [[pipelines.sinks]]
                type = "noop"
                "#,
                secret_path.display(),
            ),
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.pipelines[0].source.config["token"], "absolute-token");
        assert_eq!(redact_secret("absolute-token").to_string(), REDACTED_SECRET);
    }

    #[test]
    fn missing_file_reference_fails_with_path_and_filename() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("courier.toml");
        std::fs::write(
            &config_path,
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            token = "${file:secrets/does-not-exist}"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let err = Config::load(&config_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("pipelines[0].source.token"), "{msg}");
        assert!(msg.contains("secrets/does-not-exist"), "{msg}");
    }

    #[test]
    fn substitutes_multiple_references_in_one_string_and_redacts_full_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();
        set_env_var("COURIER_TEST_MULTI_HOST", "db.example.test");
        set_env_var("COURIER_TEST_MULTI_PASSWORD", "p4ssw0rd");

        let config = Config::from_toml_str(
            r#"
            [[pipelines]]
            name = "p"

            [pipelines.source]
            type = "noop"
            dsn = "postgres://app:${secret:COURIER_TEST_MULTI_PASSWORD}@${env:COURIER_TEST_MULTI_HOST}/app"

            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let dsn = "postgres://app:p4ssw0rd@db.example.test/app";
        assert_eq!(config.pipelines[0].source.config["dsn"], dsn);

        assert_eq!(redact_secret(dsn).to_string(), REDACTED_SECRET);
        assert_eq!(redact_secret("p4ssw0rd").to_string(), REDACTED_SECRET);
        assert_eq!(
            redact_secret("db.example.test").to_string(),
            "db.example.test"
        );

        let debug = format!("{config:?}");
        assert!(!debug.contains("p4ssw0rd"), "{debug}");
        assert!(!debug.contains(dsn), "{debug}");
        assert!(debug.contains(REDACTED_SECRET), "{debug}");

        remove_env_var("COURIER_TEST_MULTI_HOST");
        remove_env_var("COURIER_TEST_MULTI_PASSWORD");
    }

    #[test]
    fn interpolates_observability_block_fields() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_secret_values_for_test();
        set_env_var("COURIER_TEST_OBS_SERVICE", "courier-obs");
        set_env_var("COURIER_TEST_OBS_METRICS", "https://otel.example.test:4318");

        let config = Config::from_toml_str(
            r#"
            [observability]
            service_name = "${env:COURIER_TEST_OBS_SERVICE}"

            [observability.metrics]
            otlp_endpoint = "${env:COURIER_TEST_OBS_METRICS}"

            [[pipelines]]
            name = "p"
            [pipelines.source]
            type = "noop"
            [[pipelines.sinks]]
            type = "noop"
            "#,
        )
        .unwrap();

        let obs = config.observability.expect("observability set");
        assert_eq!(obs.service_name, "courier-obs");
        assert_eq!(
            obs.metrics.otlp_endpoint.as_deref(),
            Some("https://otel.example.test:4318")
        );

        remove_env_var("COURIER_TEST_OBS_SERVICE");
        remove_env_var("COURIER_TEST_OBS_METRICS");
    }
}
