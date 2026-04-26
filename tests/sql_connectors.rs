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
            active BOOL NOT NULL
        )",
    )
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO users (id, email, active) VALUES (7, 'pg@example.com', true)")
        .execute(&pool)
        .await?;

    let source = SqlQueryPollSource::new(
        "postgres/src",
        SqlDriver::Postgres,
        &dsn,
        "SELECT id, email, active FROM users ORDER BY id",
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
