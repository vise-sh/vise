//! The embedded migrator (`vise_core::MIGRATOR`) and sqlx-cli both read
//! `crates/vise-core/migrations` and record what they applied in
//! `_sqlx_migrations`, so a database prepared by either must be accepted by
//! the other. These tests need `DATABASE_URL`; `sqlx::test` creates a
//! throwaway database per test.

use sqlx::PgPool;

/// `sqlx::test` applies the migrations directory the same way sqlx-cli does;
/// running the embedded migrator afterwards must find nothing to do.
#[sqlx::test(migrations = "./migrations")]
async fn embedded_migrator_is_a_no_op_on_a_migrated_database(pool: PgPool) {
    vise_core::MIGRATOR
        .run(&pool)
        .await
        .expect("embedded migrations match the migrations directory");

    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied as usize, vise_core::MIGRATOR.iter().count());
}

/// On an empty database (what a fresh container sees) the embedded migrator
/// creates the schema on its own, and is idempotent.
#[sqlx::test(migrations = false)]
async fn embedded_migrator_creates_schema_from_scratch(pool: PgPool) {
    vise_core::MIGRATOR.run(&pool).await.unwrap();
    vise_core::MIGRATOR.run(&pool).await.unwrap();

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = 'public' ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for table in ["hosts", "session_events", "sessions", "workspaces"] {
        assert!(
            tables.iter().any(|t| t == table),
            "missing table {table}: {tables:?}"
        );
    }

    let default: Option<String> =
        sqlx::query_scalar("SELECT id FROM workspaces WHERE id = 'default'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(default.as_deref(), Some("default"));
}

/// A database created before workspaces existed (migration 0001 only) with
/// hosts and sessions in it: the workspace migration must move every row
/// into `default`, leave the column NOT NULL, and relax host-name uniqueness
/// to per-workspace.
#[sqlx::test(migrations = false)]
async fn workspace_migration_backfills_existing_rows_to_default(pool: PgPool) {
    vise_core::MIGRATOR.run_to(1, &pool).await.unwrap();

    sqlx::raw_sql(
        r#"
        INSERT INTO hosts (id, name, token_hash, created_at)
        VALUES ('host_1', 'box', 'hash_1', now());
        INSERT INTO sessions (id, agent, environment, input, status, created_at, updated_at)
        VALUES ('ses_1', '{}', '{}', 'hi', 'pending', now(), now());
        INSERT INTO session_events (session_id, seq, payload)
        VALUES ('ses_1', 1, '{}');
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    vise_core::MIGRATOR.run(&pool).await.unwrap();

    let (host_ws, session_ws): (String, String) = sqlx::query_as(
        "SELECT h.workspace_id, s.workspace_id
         FROM hosts h, sessions s
         WHERE h.id = 'host_1' AND s.id = 'ses_1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(host_ws, "default");
    assert_eq!(session_ws, "default");

    let nullable: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name, is_nullable FROM information_schema.columns
         WHERE column_name = 'workspace_id' ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        nullable,
        vec![
            ("hosts".to_string(), "NO".to_string()),
            ("sessions".to_string(), "NO".to_string())
        ]
    );

    // No column default: inserts must name their workspace.
    let defaults: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns
         WHERE column_name = 'workspace_id'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(defaults.iter().all(Option::is_none), "{defaults:?}");

    // session_events stays workspace-free; scope flows through the session.
    let event_columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns
         WHERE table_name = 'session_events'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!event_columns.iter().any(|c| c == "workspace_id"));

    // Host names: globally unique before, per-workspace after.
    sqlx::raw_sql(
        r#"
        INSERT INTO workspaces (id, name) VALUES ('ws_other', 'Other');
        INSERT INTO hosts (id, workspace_id, name, token_hash, created_at)
        VALUES ('host_2', 'ws_other', 'box', 'hash_2', now());
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    let duplicate = sqlx::query(
        "INSERT INTO hosts (id, workspace_id, name, token_hash, created_at)
         VALUES ('host_3', 'default', 'box', 'hash_3', now())",
    )
    .execute(&pool)
    .await;
    assert!(
        duplicate.is_err(),
        "same name in the same workspace must collide"
    );
}
