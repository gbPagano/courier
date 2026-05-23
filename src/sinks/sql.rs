use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{PgPool, SqlitePool};

use crate::config::{parse_config, redact_secret};
use crate::envelope::Envelope;
use crate::pipeline::ErrorPolicy;
use crate::retry::RetryPolicy;
use crate::sinks::{ManagedSink, Sink, WriteOne};
use crate::sources::sql::{SqlDriver, validate_driver_dsn};

/// SQL sink that inserts one row per envelope using explicit column mappings.
///
/// Upsert is intentionally not included in the first version because conflict
/// targets and update column behavior need a more precise cross-driver API.
pub struct SqlSink {
    id: String,
    db: SinkDb,
    insert_sql: String,
    columns: Vec<(String, String)>,
}

impl SqlSink {
    pub fn new(
        id: impl Into<String>,
        driver: SqlDriver,
        dsn: &str,
        table: &str,
        columns: BTreeMap<String, String>,
    ) -> Result<Self> {
        validate_identifier(table, "table")?;
        if columns.is_empty() {
            return Err(anyhow!(
                "invalid config for component type 'sql': columns must not be empty"
            ));
        }
        for column in columns.keys() {
            validate_identifier(column, "column")?;
        }

        let db_columns: Vec<_> = columns.keys().cloned().collect();
        let placeholders = placeholders(driver, db_columns.len());
        let insert_sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            quote_identifier(table),
            db_columns
                .iter()
                .map(|c| quote_identifier(c))
                .collect::<Vec<_>>()
                .join(", "),
            placeholders.join(", ")
        );

        let db = match driver {
            SqlDriver::Postgres => {
                SinkDb::Postgres(PgPoolOptions::new().connect_lazy(dsn).map_err(|e| {
                    anyhow!("invalid config for component type 'sql': invalid postgres dsn: {e}")
                })?)
            }
            SqlDriver::Sqlite => {
                SinkDb::Sqlite(SqlitePoolOptions::new().connect_lazy(dsn).map_err(|e| {
                    anyhow!("invalid config for component type 'sql': invalid sqlite dsn: {e}")
                })?)
            }
        };

        Ok(Self {
            id: id.into(),
            db,
            insert_sql,
            columns: columns.into_iter().collect(),
        })
    }
}

#[async_trait]
impl WriteOne for SqlSink {
    fn id(&self) -> &str {
        &self.id
    }

    async fn write(&self, env: &Envelope) -> Result<()> {
        let env_value = serde_json::to_value(env)?;
        match &self.db {
            SinkDb::Postgres(pool) => {
                let mut query = sqlx::query(&self.insert_sql);
                for (_, path) in &self.columns {
                    query = bind_pg_value(query, extract_path(&env_value, path));
                }
                query.execute(pool).await?;
            }
            SinkDb::Sqlite(pool) => {
                let mut query = sqlx::query(&self.insert_sql);
                for (_, path) in &self.columns {
                    query = bind_sqlite_value(query, extract_path(&env_value, path));
                }
                query.execute(pool).await?;
            }
        }
        Ok(())
    }
}

enum SinkDb {
    Postgres(PgPool),
    Sqlite(SqlitePool),
}

fn bind_pg_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    value: Option<&Value>,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match value {
        None | Some(Value::Null) => query.bind(None::<String>),
        Some(Value::Bool(v)) => query.bind(*v),
        Some(Value::Number(n)) => {
            if let Some(v) = n.as_i64() {
                query.bind(v)
            } else if let Some(v) = n.as_f64() {
                query.bind(v)
            } else {
                query.bind(n.to_string())
            }
        }
        Some(Value::String(v)) => query.bind(v.clone()),
        Some(other) => query.bind(sqlx::types::Json(other.clone())),
    }
}

fn bind_sqlite_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    value: Option<&Value>,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    match value {
        None | Some(Value::Null) => query.bind(None::<String>),
        Some(Value::Bool(v)) => query.bind(*v),
        Some(Value::Number(n)) => {
            if let Some(v) = n.as_i64() {
                query.bind(v)
            } else if let Some(v) = n.as_f64() {
                query.bind(v)
            } else {
                query.bind(n.to_string())
            }
        }
        Some(Value::String(v)) => query.bind(v.clone()),
        Some(other) => query.bind(other.to_string()),
    }
}

fn extract_path<'a>(env: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut current = env;
    for segment in dotted.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

fn placeholders(driver: SqlDriver, count: usize) -> Vec<String> {
    match driver {
        SqlDriver::Postgres => (1..=count).map(|i| format!("${i}")).collect(),
        SqlDriver::Sqlite => (0..count).map(|_| "?".to_string()).collect(),
    }
}

fn validate_identifier(identifier: &str, label: &str) -> Result<()> {
    if identifier.is_empty() {
        return Err(anyhow!(
            "invalid config for component type 'sql': {label} must not be empty"
        ));
    }
    if identifier
        .split('.')
        .any(|part| part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    {
        return Err(anyhow!(
            "invalid config for component type 'sql': invalid {label} identifier '{}'",
            redact_secret(identifier)
        ));
    }
    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    identifier
        .split('.')
        .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(".")
}

#[derive(Debug, Deserialize)]
struct SqlSinkConfig {
    driver: SqlDriver,
    dsn: String,
    table: String,
    #[serde(default)]
    mode: SqlSinkMode,
    columns: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SqlSinkMode {
    #[default]
    Insert,
}

/// Registry factory for [`SqlSink`]. Registered by
/// `courier::registry::register_builtin` under kind `"sql"`.
pub fn sql_sink_factory(
    id: &str,
    config: Value,
    on_error: ErrorPolicy,
    retry: Option<RetryPolicy>,
) -> Result<Box<dyn Sink>> {
    let config: SqlSinkConfig = parse_config("sql", config)?;
    validate_driver_dsn("sql", config.driver, &config.dsn)?;
    match config.mode {
        SqlSinkMode::Insert => {}
    }

    let sql = SqlSink::new(
        id,
        config.driver,
        &config.dsn,
        &config.table,
        config.columns,
    )?;
    let mut sink = ManagedSink::new(sql).with_error_policy(on_error);
    if let Some(policy) = retry {
        sink = sink.with_retry(policy);
    }
    Ok(Box::new(sink))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Registry;
    use crate::config::{ErrorPolicyConfig, SinkSpec};
    use crate::retry::RetryPolicy;

    fn build(config: serde_json::Value, retry: Option<RetryPolicy>) -> anyhow::Result<()> {
        Registry::with_builtins()
            .build_sink(
                "p/sink0",
                SinkSpec {
                    kind: "sql".into(),
                    config,
                    on_error: Some(ErrorPolicyConfig::Drop),
                    retry,
                },
            )
            .map(|_| ())
    }

    #[tokio::test]
    async fn factory_builds_sqlite_sink_with_retry() {
        build(
            json!({
                "driver": "sqlite",
                "dsn": "sqlite::memory:",
                "table": "items",
                "columns": { "id": "payload.id" }
            }),
            Some(RetryPolicy::default()),
        )
        .expect("sqlite factory");
    }

    #[tokio::test]
    async fn factory_builds_postgres_sink_without_retry() {
        // `connect_lazy` doesn't open a TCP socket but does need a Tokio context.
        build(
            json!({
                "driver": "postgres",
                "dsn": "postgres://user:pw@127.0.0.1:5432/db",
                "table": "items",
                "columns": { "id": "payload.id" }
            }),
            None,
        )
        .expect("postgres factory");
    }

    #[test]
    fn factory_rejects_invalid_config() {
        let err =
            build(json!({ "driver": "sqlite" }), None).expect_err("missing dsn / table / columns");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid config for component type 'sql'"),
            "{msg}",
        );
    }

    #[test]
    fn factory_rejects_driver_dsn_mismatch() {
        let err = build(
            json!({
                "driver": "postgres",
                "dsn": "sqlite::memory:",
                "table": "items",
                "columns": { "id": "payload.id" }
            }),
            None,
        )
        .expect_err("driver/dsn mismatch");
        let msg = format!("{err:#}");
        assert!(msg.contains("dsn does not match driver"), "{msg}");
    }

    #[test]
    fn factory_rejects_empty_columns() {
        let err = build(
            json!({
                "driver": "sqlite",
                "dsn": "sqlite::memory:",
                "table": "items",
                "columns": {}
            }),
            None,
        )
        .expect_err("empty columns");
        let msg = format!("{err:#}");
        assert!(msg.contains("columns must not be empty"), "{msg}");
    }
}
