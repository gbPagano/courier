use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Number, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::types::chrono;
use sqlx::{Column, PgPool, Row, SqlitePool, TypeInfo};
use tokio::sync::mpsc::Sender;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::config::{parse_config, redact_secret};
use crate::envelope::Envelope;
use crate::observability::{NodeCtx, SendStopped, SourceCtx};
use crate::retry::RetryPolicy;
use crate::sources::Source;
use crate::sources::retry::PollScheduler;

/// Polls a SQL query and emits one envelope per returned row.
///
/// This first version is stateless: every poll executes the configured query
/// as-is. Operators who need incremental behavior should express it in SQL
/// until Courier has durable checkpoint storage.
///
/// When `retry` is configured, consecutive query failures schedule the next
/// attempt sooner than the normal cadence — see `PollScheduler` for the rule.
pub struct SqlQueryPollSource {
    id: String,
    driver: SqlDriver,
    dsn: String,
    query: String,
    poll_interval: Duration,
    retry: Option<RetryPolicy>,
    source_ctx: SourceCtx,
}

impl SqlQueryPollSource {
    pub fn new(
        id: impl Into<String>,
        driver: SqlDriver,
        dsn: impl Into<String>,
        query: impl Into<String>,
        poll_interval: Duration,
    ) -> Self {
        let id = id.into();
        Self {
            source_ctx: SourceCtx::new(&id),
            id,
            driver,
            dsn: dsn.into(),
            query: query.into(),
            poll_interval,
            retry: None,
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = Some(retry);
        self
    }
}

#[async_trait]
impl Source for SqlQueryPollSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.source_ctx = SourceCtx::from_node_ctx(ctx);
    }

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
        let mut scheduler = PollScheduler::new(self.poll_interval, self.retry.clone());
        let mut ticker = interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await;
        let source_ctx = self.source_ctx.clone();

        let db = match SourceDb::connect(self.driver, &self.dsn).await {
            Ok(db) => db,
            Err(e) => {
                log::error!(
                    "[{}] failed to connect SQL source: {e}",
                    redact_secret(&self.id)
                );
                return;
            }
        };

        log::info!(
            "[{}] starting SQL poll loop every {:?}",
            redact_secret(&self.id),
            self.poll_interval
        );

        loop {
            let start = Instant::now();
            let rows = tokio::select! {
                _ = cancel.cancelled() => return,
                result = db.fetch_rows(&self.query) => match result {
                    Ok(rows) => rows,
                    Err(e) => {
                        let delay = scheduler.record_failure();
                        log::error!(
                            "[{}] SQL query failed (consecutive failures: {}), next attempt in {:?}: {e}",
                            redact_secret(&self.id),
                            scheduler.consecutive_failures(),
                            delay,
                        );
                        // Backoff bypasses the ticker — sleep an arbitrary
                        // duration, then reset() so normal cadence resumes
                        // `interval` after this point.
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(delay) => {}
                        }
                        ticker.reset();
                        continue;
                    }
                },
            };

            scheduler.record_success();

            for payload in rows {
                let env = Envelope::new(&self.id, payload);
                match source_ctx.send(&tx, env, &cancel).await {
                    Ok(()) => {}
                    Err(SendStopped::Cancelled) => return,
                    Err(SendStopped::DownstreamClosed) => {
                        log::info!("[{}] downstream closed, stopping", redact_secret(&self.id));
                        return;
                    }
                }
            }

            let elapsed = start.elapsed();
            if elapsed > self.poll_interval {
                log::warn!(
                    "[{}] SQL poll took {:?}, exceeding interval {:?}",
                    redact_secret(&self.id),
                    elapsed,
                    self.poll_interval,
                );
            }

            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {}
            }
        }
    }
}

enum SourceDb {
    Postgres(PgPool),
    Sqlite(SqlitePool),
}

impl SourceDb {
    async fn connect(driver: SqlDriver, dsn: &str) -> Result<Self> {
        match driver {
            SqlDriver::Postgres => Ok(Self::Postgres(PgPoolOptions::new().connect(dsn).await?)),
            SqlDriver::Sqlite => Ok(Self::Sqlite(SqlitePoolOptions::new().connect(dsn).await?)),
        }
    }

    async fn fetch_rows(&self, query: &str) -> Result<Vec<Value>> {
        match self {
            Self::Postgres(pool) => {
                let rows = sqlx::query(query).fetch_all(pool).await?;
                rows.into_iter().map(pg_row_to_json).collect()
            }
            Self::Sqlite(pool) => {
                let rows = sqlx::query(query).fetch_all(pool).await?;
                rows.into_iter().map(sqlite_row_to_json).collect()
            }
        }
    }
}

fn pg_row_to_json(row: sqlx::postgres::PgRow) -> Result<Value> {
    let mut object = Map::new();
    for column in row.columns() {
        let name = column.name();
        let type_name = column.type_info().name();
        let value = match type_name {
            "BOOL" => row
                .try_get::<Option<bool>, _>(name)?
                .map(Value::Bool)
                .unwrap_or(Value::Null),
            "INT2" => row
                .try_get::<Option<i16>, _>(name)?
                .map(|v| Value::Number(Number::from(v)))
                .unwrap_or(Value::Null),
            "INT4" => row
                .try_get::<Option<i32>, _>(name)?
                .map(|v| Value::Number(Number::from(v)))
                .unwrap_or(Value::Null),
            "INT8" => row
                .try_get::<Option<i64>, _>(name)?
                .map(|v| Value::Number(Number::from(v)))
                .unwrap_or(Value::Null),
            "FLOAT4" => row
                .try_get::<Option<f32>, _>(name)?
                .and_then(|v| Number::from_f64(v as f64))
                .map(Value::Number)
                .unwrap_or(Value::Null),
            "FLOAT8" => row
                .try_get::<Option<f64>, _>(name)?
                .and_then(Number::from_f64)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            "JSON" | "JSONB" => row
                .try_get::<Option<Value>, _>(name)?
                .unwrap_or(Value::Null),
            "TIMESTAMP" => row
                .try_get::<Option<chrono::NaiveDateTime>, _>(name)?
                .map(|v| Value::String(v.to_string()))
                .unwrap_or(Value::Null),
            "TIMESTAMPTZ" => row
                .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(name)?
                .map(|v| Value::String(v.to_rfc3339()))
                .unwrap_or(Value::Null),
            _ => row
                .try_get::<Option<String>, _>(name)?
                .map(Value::String)
                .unwrap_or(Value::Null),
        };
        object.insert(name.to_string(), value);
    }
    Ok(Value::Object(object))
}

fn sqlite_row_to_json(row: sqlx::sqlite::SqliteRow) -> Result<Value> {
    let mut object = Map::new();
    for column in row.columns() {
        let name = column.name();
        let type_name = column.type_info().name().to_ascii_uppercase();
        let value = match type_name.as_str() {
            "BOOLEAN" | "BOOL" => row
                .try_get::<Option<bool>, _>(name)?
                .map(Value::Bool)
                .unwrap_or(Value::Null),
            "INTEGER" | "INT" | "BIGINT" => row
                .try_get::<Option<i64>, _>(name)?
                .map(|v| Value::Number(Number::from(v)))
                .unwrap_or(Value::Null),
            "REAL" | "DOUBLE" | "FLOAT" => row
                .try_get::<Option<f64>, _>(name)?
                .and_then(Number::from_f64)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            _ => row
                .try_get::<Option<String>, _>(name)?
                .map(|s| serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)))
                .unwrap_or(Value::Null),
        };
        object.insert(name.to_string(), value);
    }
    Ok(Value::Object(object))
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlDriver {
    Postgres,
    Sqlite,
}

pub(crate) fn validate_driver_dsn(kind: &str, driver: SqlDriver, dsn: &str) -> Result<()> {
    let valid = match driver {
        SqlDriver::Postgres => dsn.starts_with("postgres://") || dsn.starts_with("postgresql://"),
        SqlDriver::Sqlite => dsn.starts_with("sqlite:"),
    };
    if !valid {
        return Err(anyhow!(
            "invalid config for component type '{kind}': dsn does not match driver {driver:?}"
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct SqlQueryPollSourceConfig {
    driver: SqlDriver,
    dsn: String,
    query: String,
    poll_interval_secs: u64,
}

/// Registry factory for [`SqlQueryPollSource`]. Registered by
/// `courier::registry::register_builtin` under kind `"sql_query_poll"`.
/// The optional `retry` policy is extracted by the registry and threaded
/// into the source's `PollScheduler`.
pub fn sql_query_poll_source_factory(
    id: &str,
    config: Value,
    retry: Option<RetryPolicy>,
) -> Result<Box<dyn Source>> {
    let config: SqlQueryPollSourceConfig = parse_config("sql_query_poll", config)?;
    validate_driver_dsn("sql_query_poll", config.driver, &config.dsn)?;
    if config.query.trim().is_empty() {
        return Err(anyhow!(
            "invalid config for component type 'sql_query_poll': query must not be empty"
        ));
    }
    if config.poll_interval_secs == 0 {
        return Err(anyhow!(
            "invalid config for component type 'sql_query_poll': poll_interval_secs must be greater than 0"
        ));
    }

    let mut source = SqlQueryPollSource::new(
        id,
        config.driver,
        config.dsn,
        config.query,
        Duration::from_secs(config.poll_interval_secs),
    );
    if let Some(policy) = retry {
        source = source.with_retry(policy);
    }
    Ok(Box::new(source))
}
