//! Reusable enrollment tokens. Mint/list/revoke are user-facing routes
//! resolved through the caller-identity seam like the rest of the fleet
//! management; the exchange route is deliberately outside it — the
//! enrollment token *in the request body* is what authenticates a booting
//! host, which has no API token of its own.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::AppState;
use crate::auth::AuthedCaller;
use crate::routes::hosts::EnrollHostResponse;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/hosts/enrollment-tokens",
            post(mint_enrollment_token).get(list_enrollment_tokens),
        )
        .route(
            "/hosts/enrollment-tokens/{id}/revoke",
            post(revoke_enrollment_token),
        )
        .route("/hosts/exchange", post(exchange_enrollment_token))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct MintEnrollmentTokenRequest {
    /// Maximum number of hosts this token may enroll; omit for unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MintEnrollmentTokenResponse {
    pub token: vise_core::enrollment::model::EnrollmentToken,
    /// The enrollment secret (`venroll_...`). Shown exactly once; store it
    /// safely.
    pub secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ExchangeEnrollmentTokenRequest {
    /// The enrollment secret (`venroll_...`) this host was provisioned with.
    pub token: String,
    /// Prefix for the generated host name; defaults to "host".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_prefix: Option<String>,
}

#[utoipa::path(
    post,
    path = "/hosts/enrollment-tokens",
    operation_id = "mint_enrollment_token",
    tag = "hosts",
    security(("api_token" = []), ()),
    request_body = MintEnrollmentTokenRequest,
    responses(
        (
            status = 201,
            description = "Enrollment token minted; the secret is returned exactly once",
            body = MintEnrollmentTokenResponse
        ),
        (status = 401, description = "Missing or invalid API token"),
        (status = 422, description = "max_uses is not a positive number")
    )
)]
pub async fn mint_enrollment_token(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
    Json(request): Json<MintEnrollmentTokenRequest>,
) -> Result<(StatusCode, Json<MintEnrollmentTokenResponse>), StatusCode> {
    if request.max_uses.is_some_and(|max_uses| max_uses < 1) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let minted = state
        .enrollment
        .mint(caller.workspace.clone(), request.max_uses)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((
        StatusCode::CREATED,
        Json(MintEnrollmentTokenResponse {
            token: minted.token,
            secret: minted.secret,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/hosts/enrollment-tokens",
    operation_id = "list_enrollment_tokens",
    tag = "hosts",
    security(("api_token" = []), ()),
    responses(
        (
            status = 200,
            description = "List enrollment tokens (no secrets)",
            body = [vise_core::enrollment::model::EnrollmentToken]
        ),
        (status = 401, description = "Missing or invalid API token")
    )
)]
pub async fn list_enrollment_tokens(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
) -> Result<Json<Vec<vise_core::enrollment::model::EnrollmentToken>>, StatusCode> {
    let tokens = state
        .enrollment
        .list(&caller.workspace)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(tokens))
}

#[utoipa::path(
    post,
    path = "/hosts/enrollment-tokens/{id}/revoke",
    operation_id = "revoke_enrollment_token",
    tag = "hosts",
    security(("api_token" = []), ()),
    params(
        ("id" = String, Path, description = "Enrollment token ID")
    ),
    responses(
        (
            status = 200,
            description = "Token revoked (idempotent)",
            body = vise_core::enrollment::model::EnrollmentToken
        ),
        (status = 401, description = "Missing or invalid API token"),
        (status = 404, description = "No such enrollment token")
    )
)]
pub async fn revoke_enrollment_token(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
    Path(id): Path<String>,
) -> Result<Json<vise_core::enrollment::model::EnrollmentToken>, StatusCode> {
    let token = state
        .enrollment
        .revoke(&caller.workspace, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(token))
}

#[utoipa::path(
    post,
    path = "/hosts/exchange",
    operation_id = "exchange_enrollment_token",
    tag = "hosts",
    request_body = ExchangeEnrollmentTokenRequest,
    responses(
        (
            status = 201,
            description = "Ephemeral host enrolled in the token's workspace; \
                           the vhost_ token is returned exactly once",
            body = EnrollHostResponse
        ),
        (status = 401, description = "Unknown, revoked or exhausted enrollment token")
    )
)]
pub async fn exchange_enrollment_token(
    State(state): State<AppState>,
    Json(request): Json<ExchangeEnrollmentTokenRequest>,
) -> Result<(StatusCode, Json<EnrollHostResponse>), StatusCode> {
    let enrolled = state
        .enrollment
        .exchange(&request.token, request.name_prefix.as_deref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    Ok((
        StatusCode::CREATED,
        Json(EnrollHostResponse {
            host: enrolled.host,
            token: enrolled.token,
        }),
    ))
}
