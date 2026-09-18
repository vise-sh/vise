//! Caller identity for user-facing routes.
//!
//! Sessions CRUD and host enrollment resolve *who is calling* through the
//! [`CallerExtractor`] held in [`AppState::caller`](crate::AppState::caller).
//! The OSS server ships two implementations: [`OpenAccess`], which accepts
//! every request (the behavior when `VISE_API_TOKEN` is unset), and
//! [`StaticToken`], which requires `Authorization: Bearer <token>` with the
//! one token from `VISE_API_TOKEN`. A hosted composition that builds its
//! router with [`app`](crate::app) can plug in its own implementation (API
//! keys, session cookies, ...) without touching the routes.
//!
//! Host-protocol routes (`/hosts/claim`, `/hosts/sessions/*`) are not affected:
//! they authenticate the host itself with its `vhost_` token (the `AuthedHost`
//! extractor in the hosts routes).

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    extract::{FromRef, FromRequestParts},
    http::{HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;

/// Environment variable holding the static API token, read by
/// [`StaticToken::from_env`].
pub const API_TOKEN_ENV: &str = "VISE_API_TOKEN";

/// The resolved identity of whoever is calling a user-facing route.
///
/// `#[non_exhaustive]` so fields can be added (richer workspace context,
/// roles, ...) without breaking external [`CallerExtractor`] implementations;
/// build one with [`Caller::anonymous`] or [`Caller::new`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Caller {
    /// Stable identifier of the principal (user id, API key id, ...), when
    /// the extractor has one. The OSS extractors do not.
    pub subject: Option<String>,
    /// Workspace the caller acts in. `None` is the default workspace, which
    /// is the only one an OSS deployment has.
    pub workspace: Option<String>,
}

impl Caller {
    /// A caller with no identity and the default workspace.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// A caller identified by `subject`, in the default workspace.
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: Some(subject.into()),
            workspace: None,
        }
    }

    /// Set the workspace the caller acts in.
    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }
}

/// Why a caller could not be resolved.
#[derive(Debug)]
pub enum CallerError {
    /// The request carries no credential, or one that does not identify a
    /// caller. Rendered as `401 Unauthorized`.
    Unauthorized,
    /// The credential could not be checked (a lookup failed, ...). Rendered
    /// as `500 Internal Server Error`; the cause is logged, not returned.
    Internal(anyhow::Error),
}

impl IntoResponse for CallerError {
    fn into_response(self) -> Response {
        match self {
            CallerError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
            )
                .into_response(),
            CallerError::Internal(error) => {
                tracing::error!(%error, "caller identity lookup failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

/// Resolves the [`Caller`] behind a request to a user-facing route.
///
/// Implementations only see the request head; the body is untouched. Return
/// [`CallerError::Unauthorized`] to reject the request with `401`.
#[async_trait]
pub trait CallerExtractor: Send + Sync + 'static {
    async fn extract(&self, parts: &mut Parts) -> Result<Caller, CallerError>;
}

/// Accepts every request as an anonymous caller. The default when
/// `VISE_API_TOKEN` is unset.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAccess;

#[async_trait]
impl CallerExtractor for OpenAccess {
    async fn extract(&self, _parts: &mut Parts) -> Result<Caller, CallerError> {
        Ok(Caller::anonymous())
    }
}

/// Requires `Authorization: Bearer <token>` with one shared, static token.
///
/// Every request carrying the token is the same anonymous caller in the
/// default workspace; the token proves possession, not identity.
#[derive(Clone)]
pub struct StaticToken {
    token: String,
}

impl std::fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticToken").finish_non_exhaustive()
    }
}

impl StaticToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }

    /// The token configured in [`API_TOKEN_ENV`], or `None` when the variable
    /// is unset or blank.
    pub fn from_env() -> Option<Self> {
        Self::from_value(std::env::var(API_TOKEN_ENV).ok())
    }

    /// [`from_env`](Self::from_env) on an already-read value: whitespace is
    /// trimmed and a blank value counts as unset.
    pub fn from_value(value: Option<String>) -> Option<Self> {
        value
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(Self::new)
    }
}

#[async_trait]
impl CallerExtractor for StaticToken {
    async fn extract(&self, parts: &mut Parts) -> Result<Caller, CallerError> {
        let presented = bearer_token(parts).ok_or(CallerError::Unauthorized)?;
        if presented.as_bytes().ct_eq(self.token.as_bytes()).into() {
            Ok(Caller::anonymous())
        } else {
            Err(CallerError::Unauthorized)
        }
    }
}

/// The extractor the OSS server runs with: [`StaticToken`] when
/// `VISE_API_TOKEN` is set, otherwise [`OpenAccess`].
pub fn from_env() -> Arc<dyn CallerExtractor> {
    match StaticToken::from_env() {
        Some(token) => Arc::new(token),
        None => Arc::new(OpenAccess),
    }
}

/// The token in an `Authorization: Bearer <token>` header, if any.
pub fn bearer_token(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// Axum extractor: the [`Caller`] resolved by the state's [`CallerExtractor`].
///
/// Add it to a handler's arguments to make the route user-facing; a request
/// the extractor rejects never reaches the handler.
#[derive(Debug, Clone)]
pub struct AuthedCaller(pub Caller);

impl<S> FromRequestParts<S> for AuthedCaller
where
    S: Send + Sync,
    Arc<dyn CallerExtractor>: FromRef<S>,
{
    type Rejection = CallerError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let extractor = Arc::<dyn CallerExtractor>::from_ref(state);
        extractor.extract(parts).await.map(AuthedCaller)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn parts(authorization: Option<&str>) -> Parts {
        let mut request = Request::builder();
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        request.body(()).unwrap().into_parts().0
    }

    #[test]
    fn blank_env_value_means_no_token() {
        assert!(StaticToken::from_value(None).is_none());
        assert!(StaticToken::from_value(Some("".into())).is_none());
        assert!(StaticToken::from_value(Some("   \n".into())).is_none());
        assert_eq!(
            StaticToken::from_value(Some("  s3cret \n".into()))
                .unwrap()
                .token,
            "s3cret"
        );
    }

    #[test]
    fn bearer_token_parses_only_bearer_credentials() {
        assert_eq!(bearer_token(&parts(None)), None);
        assert_eq!(bearer_token(&parts(Some("Basic abc"))), None);
        assert_eq!(bearer_token(&parts(Some("Bearer "))), None);
        assert_eq!(bearer_token(&parts(Some("Bearer abc"))), Some("abc"));
    }

    #[tokio::test]
    async fn static_token_compares_the_whole_token() {
        let extractor = StaticToken::new("s3cret");
        for wrong in [
            None,
            Some("Bearer s3cre"),
            Some("Bearer s3cret1"),
            Some("Bearer S3CRET"),
        ] {
            assert!(matches!(
                extractor.extract(&mut parts(wrong)).await,
                Err(CallerError::Unauthorized)
            ));
        }
        assert_eq!(
            extractor
                .extract(&mut parts(Some("Bearer s3cret")))
                .await
                .unwrap(),
            Caller::anonymous()
        );
    }

    #[tokio::test]
    async fn open_access_accepts_anything() {
        assert_eq!(
            OpenAccess.extract(&mut parts(None)).await.unwrap(),
            Caller::anonymous()
        );
        assert_eq!(
            OpenAccess
                .extract(&mut parts(Some("Bearer whatever")))
                .await
                .unwrap(),
            Caller::anonymous()
        );
    }
}
