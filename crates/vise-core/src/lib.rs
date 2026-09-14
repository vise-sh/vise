pub mod hosts;
pub mod id;
pub mod sessions;

/// The schema migrations in `crates/vise-core/migrations`, embedded at compile
/// time so a deployed `vise-server` can bring its own database up to date
/// without `sqlx-cli`. The migrator and `sqlx migrate run` share the
/// `_sqlx_migrations` bookkeeping table, so either can be used against the
/// same database.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
