//! Reusable enrollment tokens. A token is a workspace-scoped secret
//! (`venroll_` prefix, shown exactly once at mint) that a booting host
//! exchanges for its own `vhost_` token; the host it creates is ephemeral
//! and reaped once it stops heartbeating. Tokens can be capped to a number
//! of uses and revoked at any time.

pub mod model;
pub mod postgres;
pub mod repository;
pub mod service;
