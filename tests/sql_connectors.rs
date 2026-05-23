//! Integration tests for generic SQL connectors. SQLite runs against a temp
//! database file; Postgres runs in a real container.

use std::time::Duration;

use serde_json::json;
use sqlx::{PgPool, Row, SqlitePool};
use testcontainers_modules::postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use courier::envelope::Envelope;
use courier::sinks::WriteOne;
use courier::sinks::sql::SqlSink;
use courier::sources::Source;
use courier::sources::sql::{SqlDriver, SqlQueryPollSource, sql_query_poll_source_factory};

const POSTGRES_PORT: u16 = 5432;

#[tokio::test]
async fn sqlite_source_polls_rows_and_sink_inserts_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("courier.db");
    let dsn = format!("sqlite://{}?mode=rwc", db_path.display());

    let pool = SqlitePool::connect(&dsn).await?;
    sqlx::query(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            email TEXT NOT NULL,
            active BOOLEAN NOT NULL
        )",
    )
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO users (id, email, active) VALUES (1, 'a@example.com', true)")
        .execute(&pool)
        .await?;

    let source = SqlQueryPollSource::new(
        "sqlite/src",
        SqlDriver::Sqlite,
        &dsn,
        "SELECT id, email, active FROM users ORDER BY id",
        Duration::from_secs(60),
    );
    let (tx, mut rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });

    let env = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await?
        .expect("source closed before emitting");
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;

    assert_eq!(env.payload["id"], json!(1));
    assert_eq!(env.payload["email"], json!("a@example.com"));
    assert_eq!(env.payload["active"], json!(true));

    sqlx::query(
        "CREATE TABLE snapshots (
            id INTEGER PRIMARY KEY,
            email TEXT NOT NULL,
            source TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    let sink = SqlSink::new(
        "sqlite/sink",
        SqlDriver::Sqlite,
        &dsn,
        "snapshots",
        [
            ("id".to_string(), "payload.id".to_string()),
            ("email".to_string(), "payload.email".to_string()),
            ("source".to_string(), "meta.source_id".to_string()),
        ]
        .into_iter()
        .collect(),
    )?;
    sink.write(&env).await?;

    let row = sqlx::query("SELECT id, email, source FROM snapshots")
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("id"), 1);
    assert_eq!(row.get::<String, _>("email"), "a@example.com");
    assert_eq!(row.get::<String, _>("source"), "sqlite/src");
    Ok(())
}

#[tokio::test]
async fn sqlite_source_keeps_text_columns_as_strings() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("courier.db");
    let dsn = format!("sqlite://{}?mode=rwc", db_path.display());

    let pool = SqlitePool::connect(&dsn).await?;
    sqlx::query(
        "CREATE TABLE events (
            id INTEGER PRIMARY KEY,
            bool_text TEXT NOT NULL,
            number_text TEXT NOT NULL,
            null_text TEXT NOT NULL,
            object_text TEXT NOT NULL,
            array_text TEXT NOT NULL,
            varchar_text VARCHAR(255) NOT NULL,
            clob_text CLOB NOT NULL
        )",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO events (
            id,
            bool_text,
            number_text,
            null_text,
            object_text,
            array_text,
            varchar_text,
            clob_text
        ) VALUES (1, 'true', '42', 'null', '{\"ok\":true}', '[1,2]', 'false', '{\"nested\":true}')",
    )
    .execute(&pool)
    .await?;

    let source = SqlQueryPollSource::new(
        "sqlite/src",
        SqlDriver::Sqlite,
        &dsn,
        "SELECT bool_text, number_text, null_text, object_text, array_text, varchar_text, clob_text FROM events",
        Duration::from_secs(60),
    );
    let (tx, mut rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });

    let env = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await?
        .expect("source closed before emitting");
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;

    assert_eq!(env.payload["bool_text"], json!("true"));
    assert_eq!(env.payload["number_text"], json!("42"));
    assert_eq!(env.payload["null_text"], json!("null"));
    assert_eq!(env.payload["object_text"], json!("{\"ok\":true}"));
    assert_eq!(env.payload["array_text"], json!("[1,2]"));
    assert_eq!(env.payload["varchar_text"], json!("false"));
    assert_eq!(env.payload["clob_text"], json!("{\"nested\":true}"));
    Ok(())
}

#[tokio::test]
async fn postgres_source_polls_rows_and_sink_inserts_rows() -> anyhow::Result<()> {
    let node = match postgres::Postgres::default().start().await {
        Ok(node) => node,
        Err(e) => {
            eprintln!("skipping Postgres test because testcontainer failed to start: {e}");
            return Ok(());
        }
    };
    let host_port = node.get_host_port_ipv4(POSTGRES_PORT).await?;
    let dsn = format!("postgres://postgres:postgres@127.0.0.1:{host_port}/postgres");

    let pool = PgPool::connect(&dsn).await?;
    sqlx::query(
        "CREATE TABLE users (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL,
            active BOOL NOT NULL,
            age SMALLINT NOT NULL,
            score INTEGER NOT NULL,
            ratio REAL NOT NULL,
            weight DOUBLE PRECISION NOT NULL,
            profile JSON NOT NULL,
            tags JSONB NOT NULL,
            created_at TIMESTAMP NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL,
            null_field TEXT
        )",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO users (
            id, email, active, age, score, ratio, weight,
            profile, tags, created_at, updated_at, null_field
        ) VALUES (
            7, 'pg@example.com', true, 30, 1000, 1.5::real, 2.5::double precision,
            '{\"plan\":\"pro\"}'::json, '[\"a\",\"b\"]'::jsonb,
            '2026-01-02 03:04:05'::timestamp,
            '2026-01-02 03:04:05+00'::timestamptz,
            NULL
        )",
    )
    .execute(&pool)
    .await?;

    let source = SqlQueryPollSource::new(
        "postgres/src",
        SqlDriver::Postgres,
        &dsn,
        "SELECT id, email, active, age, score, ratio, weight, \
         profile, tags, created_at, updated_at, null_field \
         FROM users ORDER BY id",
        Duration::from_secs(60),
    );
    let (tx, mut rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });

    let env = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await?
        .expect("source closed before emitting");
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    assert_eq!(env.payload["id"], json!(7));
    assert_eq!(env.payload["email"], json!("pg@example.com"));
    assert_eq!(env.payload["active"], json!(true));
    assert_eq!(env.payload["age"], json!(30));
    assert_eq!(env.payload["score"], json!(1000));
    assert_eq!(env.payload["ratio"], json!(1.5));
    assert_eq!(env.payload["weight"], json!(2.5));
    assert_eq!(env.payload["profile"], json!({"plan": "pro"}));
    assert_eq!(env.payload["tags"], json!(["a", "b"]));
    // NaiveDateTime renders as "YYYY-MM-DD HH:MM:SS".
    assert_eq!(env.payload["created_at"], json!("2026-01-02 03:04:05"));
    // DateTime<Utc>::to_rfc3339 keeps the UTC offset.
    assert_eq!(
        env.payload["updated_at"],
        json!("2026-01-02T03:04:05+00:00")
    );
    assert_eq!(env.payload["null_field"], json!(null));

    sqlx::query(
        "CREATE TABLE snapshots (
            id BIGINT PRIMARY KEY,
            email TEXT NOT NULL,
            source TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    let sink = SqlSink::new(
        "postgres/sink",
        SqlDriver::Postgres,
        &dsn,
        "snapshots",
        [
            ("id".to_string(), "payload.id".to_string()),
            ("email".to_string(), "payload.email".to_string()),
            ("source".to_string(), "meta.source_id".to_string()),
        ]
        .into_iter()
        .collect(),
    )?;
    sink.write(&env).await?;

    let row = sqlx::query("SELECT id, email, source FROM snapshots")
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("id"), 7);
    assert_eq!(row.get::<String, _>("email"), "pg@example.com");
    assert_eq!(row.get::<String, _>("source"), "postgres/src");
    Ok(())
}

#[test]
fn sql_sink_rejects_empty_columns() {
    let err = match SqlSink::new(
        "sql/sink",
        SqlDriver::Sqlite,
        "sqlite::memory:",
        "items",
        Default::default(),
    ) {
        Ok(_) => panic!("expected empty columns error"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("columns must not be empty"));
}

#[test]
fn sql_sink_rejects_invalid_identifier() {
    let err = match SqlSink::new(
        "sql/sink",
        SqlDriver::Sqlite,
        "sqlite::memory:",
        "items; drop table items",
        [("id".to_string(), "payload.id".to_string())]
            .into_iter()
            .collect(),
    ) {
        Ok(_) => panic!("expected invalid identifier error"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("invalid table identifier"));
}

#[test]
fn sql_source_rejects_driver_dsn_mismatch() {
    let err = match sql_query_poll_source_factory(
        "sql/src",
        json!({
            "driver": "postgres",
            "dsn": "sqlite::memory:",
            "query": "SELECT 1",
            "poll_interval_secs": 1
        }),
        None,
    ) {
        Ok(_) => panic!("expected driver/dsn mismatch error"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("dsn does not match driver"));
}

#[allow(dead_code)]
fn envelope(payload: serde_json::Value) -> Envelope {
    Envelope::new("test/src", payload)
}
