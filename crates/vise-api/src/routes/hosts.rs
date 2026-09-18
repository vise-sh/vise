use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, State},
    http::{StatusCode, header, request::Parts},
    routing::post,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::AppState;
use crate::auth::AuthedCaller;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/hosts", post(enroll_host).get(list_hosts))
        .route("/hosts/claim", post(claim))
        .route("/hosts/sessions/{id}/heartbeat", post(heartbeat))
        .route("/hosts/sessions/{id}/events", post(report_events))
        .route("/hosts/sessions/{id}/finish", post(finish))
        .route("/hosts/sessions/{id}/credentials", post(issue_credential))
}

/// Host identity resolved from the `Authorization: Bearer vhost_...` header.
pub struct AuthedHost(pub vise_core::hosts::model::Host);

impl FromRequestParts<AppState> for AuthedHost {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(StatusCode::UNAUTHORIZED)?;

        let host = state
            .hosts
            .authenticate(token)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::UNAUTHORIZED)?;

        Ok(AuthedHost(host))
    }
}

// enrollment & fleet (user-facing: resolved through `AppState::caller`)

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EnrollHostRequest {
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EnrollHostResponse {
    pub host: vise_core::hosts::model::Host,
    /// The bearer token for this host. Shown exactly once; store it safely.
    pub token: String,
}

#[utoipa::path(
    post,
    path = "/hosts",
    operation_id = "enroll_host",
    tag = "hosts",
    security(("api_token" = []), ()),
    request_body = EnrollHostRequest,
    responses(
        (
            status = 201,
            description = "Host enrolled; the token is returned exactly once",
            body = EnrollHostResponse
        ),
        (status = 401, description = "Missing or invalid API token")
    )
)]
pub async fn enroll_host(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
    Json(request): Json<EnrollHostRequest>,
) -> Result<(StatusCode, Json<EnrollHostResponse>), StatusCode> {
    let enrolled = state
        .hosts
        .enroll(caller.workspace.clone(), request.name)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((
        StatusCode::CREATED,
        Json(EnrollHostResponse {
            host: enrolled.host,
            token: enrolled.token,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/hosts",
    operation_id = "list_hosts",
    tag = "hosts",
    security(("api_token" = []), ()),
    responses(
        (
            status = 200,
            description = "List all enrolled hosts",
            body = [vise_core::hosts::model::Host]
        ),
        (status = 401, description = "Missing or invalid API token")
    )
)]
pub async fn list_hosts(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
) -> Result<Json<Vec<vise_core::hosts::model::Host>>, StatusCode> {
    let hosts = state
        .hosts
        .list(&caller.workspace)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(hosts))
}

// host pull protocol (bearer-authenticated)

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ClaimRequest {
    #[serde(default)]
    pub harnesses: Vec<String>,
    #[serde(default)]
    pub environment_types: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ClaimResponse {
    /// The claimed session, or null if there is no pending work.
    pub session: Option<vise_core::sessions::model::Session>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HeartbeatResponse {
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReportEventsRequest {
    pub events: Vec<vise_core::sessions::model::NewSessionEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FinishRequest {
    pub status: vise_core::sessions::model::SessionStatus,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub outcome: Option<vise_core::sessions::model::SessionOutcome>,
}

#[utoipa::path(
    post,
    path = "/hosts/claim",
    operation_id = "claim_session",
    tag = "hosts",
    request_body = ClaimRequest,
    responses(
        (
            status = 200,
            description = "The claimed session, or null if there is no pending work",
            body = ClaimResponse
        ),
        (status = 401, description = "Missing or invalid host token")
    )
)]
pub async fn claim(
    State(state): State<AppState>,
    AuthedHost(host): AuthedHost,
    Json(_request): Json<ClaimRequest>,
) -> Result<Json<ClaimResponse>, StatusCode> {
    // v1 scheduling is FIFO; capabilities in the request body are not yet matched.
    let session = state
        .sessions
        .claim(&host.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ClaimResponse { session }))
}

#[utoipa::path(
    post,
    path = "/hosts/sessions/{id}/heartbeat",
    operation_id = "heartbeat_session",
    tag = "hosts",
    params(
        ("id" = String, Path, description = "Session ID")
    ),
    responses(
        (
            status = 200,
            description = "Lease extended",
            body = HeartbeatResponse
        ),
        (status = 401, description = "Missing or invalid host token"),
        (status = 409, description = "Host no longer holds this session")
    )
)]
pub async fn heartbeat(
    State(state): State<AppState>,
    AuthedHost(host): AuthedHost,
    Path(id): Path<String>,
) -> Result<Json<HeartbeatResponse>, StatusCode> {
    let cancel_requested = state
        .sessions
        .heartbeat(&host.id, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::CONFLICT)?;

    Ok(Json(HeartbeatResponse { cancel_requested }))
}

#[utoipa::path(
    post,
    path = "/hosts/sessions/{id}/events",
    operation_id = "report_session_events",
    tag = "hosts",
    params(
        ("id" = String, Path, description = "Session ID")
    ),
    request_body = ReportEventsRequest,
    responses(
        (status = 204, description = "Events recorded"),
        (status = 401, description = "Missing or invalid host token"),
        (status = 409, description = "Host no longer holds this session")
    )
)]
pub async fn report_events(
    State(state): State<AppState>,
    AuthedHost(host): AuthedHost,
    Path(id): Path<String>,
    Json(request): Json<ReportEventsRequest>,
) -> Result<StatusCode, StatusCode> {
    state
        .sessions
        .append_events(&host.id, &id, &request.events)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::CONFLICT)?;

    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/hosts/sessions/{id}/finish",
    operation_id = "finish_session",
    tag = "hosts",
    params(
        ("id" = String, Path, description = "Session ID")
    ),
    request_body = FinishRequest,
    responses(
        (
            status = 200,
            description = "Session finished",
            body = vise_core::sessions::model::Session
        ),
        (status = 401, description = "Missing or invalid host token"),
        (status = 409, description = "Host no longer holds this session"),
        (status = 422, description = "Status is not terminal")
    )
)]
pub async fn finish(
    State(state): State<AppState>,
    AuthedHost(host): AuthedHost,
    Path(id): Path<String>,
    Json(request): Json<FinishRequest>,
) -> Result<Json<vise_core::sessions::model::Session>, StatusCode> {
    use vise_core::sessions::model::SessionStatus;

    match request.status {
        SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled => {}
        _ => return Err(StatusCode::UNPROCESSABLE_ENTITY),
    }

    let session = state
        .sessions
        .finish(
            &host.id,
            &id,
            request.status,
            request.stop_reason,
            request.error,
            request.outcome,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::CONFLICT)?;

    Ok(Json(session))
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct IssueCredentialRequest {
    /// Provider name, e.g. "github"
    pub provider: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IssueCredentialResponse {
    pub provider: String,
    pub secret: String,
    /// None for non-expiring credentials
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[utoipa::path(
    post,
    path = "/hosts/sessions/{id}/credentials",
    operation_id = "issue_credential",
    tag = "hosts",
    params(("id" = String, Path, description = "Session ID")),
    request_body = IssueCredentialRequest,
    responses(
        (status = 200, description = "Short-lived credential for the session", body = IssueCredentialResponse),
        (status = 401, description = "Missing or invalid host token"),
        (status = 404, description = "Session not found"),
        (status = 409, description = "Host no longer holds this session"),
        (status = 422, description = "Provider not applicable to this session"),
        (status = 502, description = "Upstream credential issuer failed"),
        (status = 503, description = "Provider not configured on this server")
    )
)]
pub async fn issue_credential(
    State(state): State<AppState>,
    AuthedHost(host): AuthedHost,
    Path(id): Path<String>,
    Json(request): Json<IssueCredentialRequest>,
) -> Result<Json<IssueCredentialResponse>, StatusCode> {
    use crate::credentials::IssueError;

    let provider = state
        .credentials
        .get(&request.provider)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    // Scoped to the host's own workspace, like every other host-driven path.
    let session = state
        .sessions
        .get(&host.workspace_id, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Only the host holding the running lease may obtain credentials.
    if session.host_id.as_deref() != Some(host.id.as_str())
        || !matches!(
            session.status,
            vise_core::sessions::model::SessionStatus::Running
        )
    {
        return Err(StatusCode::CONFLICT);
    }

    let issued = provider
        .issue(&session)
        .await
        .map_err(|error| match error {
            IssueError::NotApplicable(reason) => {
                tracing::warn!(%reason, provider = %request.provider, "credential not applicable");
                StatusCode::UNPROCESSABLE_ENTITY
            }
            IssueError::Upstream(error) => {
                tracing::error!(%error, provider = %request.provider, "credential issue failed");
                StatusCode::BAD_GATEWAY
            }
        })?;

    Ok(Json(IssueCredentialResponse {
        provider: request.provider,
        secret: issued.secret,
        expires_at: issued.expires_at,
    }))
}
