use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use vise_core::routines::model::{Routine, SessionSpec, UpdateRoutine};
use vise_core::sessions::model::Session;

use crate::AppState;
use crate::auth::AuthedCaller;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/routines", get(list_routines).post(create_routine))
        .route(
            "/routines/{id}",
            get(get_routine).patch(update_routine).delete(delete_routine),
        )
        .route("/routines/{id}/run", post(run_routine))
        .route("/routines/{id}/runs", get(list_routine_runs))
}

/// Create a routine. The schedule is a flat `cron` + `timezone` pair — chosen
/// for request/response symmetry with the flat [`Routine`] response, rather
/// than the design doc's illustrative nested `schedule` object.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateRoutineRequest {
    pub name: String,
    /// Standard 5-field cron expression.
    pub cron: String,
    /// IANA timezone name (e.g. "America/New_York") the cron is interpreted in.
    pub timezone: String,
    pub spec: SessionSpec,
}

/// A partial update: only the `Some` fields change. Mirrors [`UpdateRoutine`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct UpdateRoutineRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<SessionSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ListRoutinesResponse {
    pub routines: Vec<Routine>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ListRunsResponse {
    pub sessions: Vec<Session>,
}

#[utoipa::path(
    post,
    path = "/routines",
    operation_id = "create_routine",
    tag = "routines",
    security(("api_token" = []), ()),
    request_body = CreateRoutineRequest,
    responses(
        (
            status = 201,
            description = "Routine created",
            body = Routine
        ),
        (status = 401, description = "Missing or invalid API token"),
        (status = 422, description = "Invalid environment, cron, or timezone")
    )
)]
pub async fn create_routine(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
    Json(request): Json<CreateRoutineRequest>,
) -> Result<(StatusCode, Json<Routine>), StatusCode> {
    // Validate up front so a bad spec/schedule is a 422, not a 500. The service
    // re-runs these same checks; this mirrors `create_session`'s 422 path.
    if let Err(reason) = request.spec.environment.validate() {
        tracing::warn!(%reason, "rejected routine create: environment");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if let Err(reason) = vise_core::routines::schedule::validate_min_interval(&request.cron) {
        tracing::warn!(%reason, "rejected routine create: cron interval");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if let Err(reason) =
        vise_core::routines::schedule::next_after(&request.cron, &request.timezone, chrono::Utc::now())
    {
        tracing::warn!(%reason, "rejected routine create: cron/timezone");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let routine = state
        .routines
        .create(
            caller.workspace.clone(),
            request.name,
            request.cron,
            request.timezone,
            request.spec,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(routine)))
}

#[utoipa::path(
    get,
    path = "/routines",
    operation_id = "list_routines",
    tag = "routines",
    security(("api_token" = []), ()),
    responses(
        (
            status = 200,
            description = "List all routines",
            body = ListRoutinesResponse
        ),
        (status = 401, description = "Missing or invalid API token")
    )
)]
pub async fn list_routines(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
) -> Result<Json<ListRoutinesResponse>, StatusCode> {
    let routines = state
        .routines
        .list(&caller.workspace)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ListRoutinesResponse { routines }))
}

#[utoipa::path(
    get,
    path = "/routines/{id}",
    operation_id = "get_routine",
    tag = "routines",
    security(("api_token" = []), ()),
    params(
        ("id" = String, Path, description = "Routine ID")
    ),
    responses(
        (
            status = 200,
            description = "Get a routine",
            body = Routine
        ),
        (status = 401, description = "Missing or invalid API token"),
        (status = 404, description = "Routine not found")
    )
)]
pub async fn get_routine(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
    Path(id): Path<String>,
) -> Result<Json<Routine>, StatusCode> {
    let routine = state
        .routines
        .get(&caller.workspace, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        .and_then(|r| r.ok_or(StatusCode::NOT_FOUND))?;

    Ok(Json(routine))
}

#[utoipa::path(
    patch,
    path = "/routines/{id}",
    operation_id = "update_routine",
    tag = "routines",
    security(("api_token" = []), ()),
    params(
        ("id" = String, Path, description = "Routine ID")
    ),
    request_body = UpdateRoutineRequest,
    responses(
        (
            status = 200,
            description = "Routine updated",
            body = Routine
        ),
        (status = 401, description = "Missing or invalid API token"),
        (status = 404, description = "Routine not found"),
        (status = 422, description = "Invalid environment, cron, or timezone")
    )
)]
pub async fn update_routine(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
    Path(id): Path<String>,
    Json(request): Json<UpdateRoutineRequest>,
) -> Result<Json<Routine>, StatusCode> {
    // Pre-validate the cheaply-checkable inputs for a clean 422. A changed cron
    // is checked against the min-interval floor; the full cron+timezone parse
    // (which needs the routine's other, possibly-existing field) is left to the
    // service, whose only expected update error is a bad schedule/spec — hence
    // the service Err is mapped to 422 below, not 500.
    if let Some(cron) = request.cron.as_deref()
        && let Err(reason) = vise_core::routines::schedule::validate_min_interval(cron)
    {
        tracing::warn!(%reason, "rejected routine update: cron interval");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if let Some(spec) = request.spec.as_ref()
        && let Err(reason) = spec.environment.validate()
    {
        tracing::warn!(%reason, "rejected routine update: environment");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let patch = UpdateRoutine {
        name: request.name,
        cron: request.cron,
        timezone: request.timezone,
        spec: request.spec,
        enabled: request.enabled,
    };

    // The routine must exist to distinguish 404 from a validation 422. Fetch
    // first so a bad-schedule service Err on an existing routine is a 422.
    let exists = state
        .routines
        .get(&caller.workspace, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_some();
    if !exists {
        return Err(StatusCode::NOT_FOUND);
    }

    // The service re-validates the resulting schedule/spec; on an existing
    // routine its only expected error is a bad cron+timezone combination, so
    // map any Err to 422 (not 500).
    let routine = state
        .routines
        .update(&caller.workspace, &id, patch)
        .await
        .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(routine))
}

#[utoipa::path(
    delete,
    path = "/routines/{id}",
    operation_id = "delete_routine",
    tag = "routines",
    security(("api_token" = []), ()),
    params(
        ("id" = String, Path, description = "Routine ID")
    ),
    responses(
        (status = 204, description = "Routine deleted"),
        (status = 401, description = "Missing or invalid API token")
    )
)]
pub async fn delete_routine(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    state
        .routines
        .delete(&caller.workspace, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/routines/{id}/run",
    operation_id = "run_routine",
    tag = "routines",
    security(("api_token" = []), ()),
    params(
        ("id" = String, Path, description = "Routine ID")
    ),
    responses(
        (
            status = 201,
            description = "Session spawned off-schedule",
            body = Session
        ),
        (status = 401, description = "Missing or invalid API token"),
        (status = 404, description = "Routine not found")
    )
)]
pub async fn run_routine(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Session>), StatusCode> {
    let session = state
        .routines
        .run_now(&caller.workspace, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok((StatusCode::CREATED, Json(session)))
}

#[utoipa::path(
    get,
    path = "/routines/{id}/runs",
    operation_id = "list_routine_runs",
    tag = "routines",
    security(("api_token" = []), ()),
    params(
        ("id" = String, Path, description = "Routine ID")
    ),
    responses(
        (
            status = 200,
            description = "List the routine's runs (sessions it spawned)",
            body = ListRunsResponse
        ),
        (status = 401, description = "Missing or invalid API token"),
        (status = 404, description = "Routine not found")
    )
)]
pub async fn list_routine_runs(
    State(state): State<AppState>,
    AuthedCaller(caller): AuthedCaller,
    Path(id): Path<String>,
) -> Result<Json<ListRunsResponse>, StatusCode> {
    // Distinguish an unknown routine (404) from one with no runs yet (200 []).
    let exists = state
        .routines
        .get(&caller.workspace, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_some();
    if !exists {
        return Err(StatusCode::NOT_FOUND);
    }

    let sessions = state
        .sessions
        .list_by_routine(&caller.workspace, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ListRunsResponse { sessions }))
}
