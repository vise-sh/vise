//! Workspace tenancy. A workspace is the tenant boundary: every host and
//! session belongs to exactly one, and user-facing repository methods take
//! the [`WorkspaceId`](model::WorkspaceId) they operate in. The
//! out-of-the-box server runs single-tenant in
//! [`WorkspaceId::DEFAULT`](model::WorkspaceId::DEFAULT); a hosting layer
//! composing this crate supplies its own workspaces through
//! [`WorkspaceRepository`](repository::WorkspaceRepository).

pub mod model;
pub mod postgres;
pub mod repository;
