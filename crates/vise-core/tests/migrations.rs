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
    for table in ["hosts", "session_events", "sessions"] {
        assert!(
            tables.iter().any(|t| t == table),
            "missing table {table}: {tables:?}"
        );
    }
}
