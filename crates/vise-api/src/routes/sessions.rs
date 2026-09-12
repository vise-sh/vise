use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/events", get(get_events))
        // Not in the OpenAPI doc: progenitor can't model SSE responses.
        .route("/sessions/{id}/events/stream", get(stream_events))
        .route("/sessions/{id}/cancel", post(cancel_session))
        .route("/sessions/{id}/follow-up", post(follow_up_session))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ListSessionsResponse {
    pub sessions: Vec<vise_core::sessions::model::Session>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    pub agent: vise_core::sessions::model::Agent,
    pub environment: vise_core::sessions::model::Environment,
    pub input: String,
}

#[utoipa::path(
    get,
    path = "/sessions",
    operation_id = "list_sessions",
    tag = "sessions",
    responses(
        (
            status = 200,
            description = "List all sessions",
            body = ListSessionsResponse
        )
    )
)]
pub async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Json<ListSessionsResponse>, StatusCode> {
    let sessions = state
        .sessions
        .list()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ListSessionsResponse {
        sessions,
        next_cursor: None,
    }))
}

#[utoipa::path(
    get,
    path = "/sessions/{id}",
    operation_id = "get_session",
    tag = "sessions",
    params(
        ("id" = String, Path, description = "Session ID")
    ),
    responses(
        (
            status = 200,
            description = "Get a session",
            body = vise_core::sessions::model::Session
        )
    )
)]
pub async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<vise_core::sessions::model::Session>, StatusCode> {
    let session = state
        .sessions
        .get(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        .and_then(|s| s.ok_or(StatusCode::NOT_FOUND))?;

    Ok(Json(session))
}

#[utoipa::path(
    post,
    path = "/sessions",
    operation_id = "create_session",
    tag = "sessions",
    request_body = CreateSessionRequest,
    responses(
        (
            status = 201,
            description = "Session created",
            body = vise_core::sessions::model::Session
        ),
        (status = 422, description = "Invalid environment")
    )
)]
pub async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<vise_core::sessions::model::Session>), StatusCode> {
    if let Err(reason) = request.environment.validate() {
        tracing::warn!(%reason, "rejected session create");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let session = state
        .sessions
        .create(request.agent, request.environment, request.input)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(session)))
}

// events routes

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    pub after_seq: Option<i64>,
    pub limit: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/sessions/{id}/events",
    operation_id = "get_events",
    tag = "sessions",
    params(
        ("id" = String, Path, description = "Session ID"),
        ("after_seq" = Option<i64>, Query, description = "Only return events with seq greater than this"),
        ("limit" = Option<i64>, Query, description = "Maximum number of events to return")
    ),
    responses(
        (
            status = 200,
            description = "Events retrieved",
            body = [vise_core::sessions::model::SessionEvent]
        )
    )
)]
pub async fn get_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<vise_core::sessions::model::SessionEvent>>, StatusCode> {
    let events = state
        .sessions
        .get_events(
            &id,
            query.after_seq.unwrap_or(0),
            query.limit.unwrap_or(1000).clamp(1, 10_000),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(events))
}

#[utoipa::path(
    post,
    path = "/sessions/{id}/cancel",
    operation_id = "cancel_session",
    tag = "sessions",
    params(
        ("id" = String, Path, description = "Session ID")
    ),
    responses(
        (
            status = 200,
            description = "Cancellation requested (pending sessions are cancelled immediately)",
            body = vise_core::sessions::model::Session
        ),
        (status = 404, description = "Session not found"),
        (status = 409, description = "Session already finished")
    )
)]
pub async fn cancel_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<vise_core::sessions::model::Session>, StatusCode> {
    let session = state
        .sessions
        .request_cancel(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match session {
        Some(session) => Ok(Json(session)),
        None => {
            let exists = state
                .sessions
                .get(&id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .is_some();

            Err(if exists {
                StatusCode::CONFLICT
            } else {
                StatusCode::NOT_FOUND
            })
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct FollowUpRequest {
    /// Free-form guidance inlined with the PR's review feedback.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Overrides the agent config inherited from the parent session.
    #[serde(default)]
    pub agent: Option<vise_core::sessions::model::Agent>,
}

#[utoipa::path(
    post,
    path = "/sessions/{id}/follow-up",
    operation_id = "follow_up_session",
    tag = "sessions",
    params(
        ("id" = String, Path, description = "Parent session ID (a follow-up may itself be the parent)")
    ),
    request_body = FollowUpRequest,
    responses(
        (
            status = 201,
            description = "Follow-up session created; its input contains the PR's current review feedback",
            body = vise_core::sessions::model::Session
        ),
        (status = 404, description = "Session not found"),
        (status = 409, description = "The PR is merged or closed"),
        (status = 422, description = "The session (or its root) did not open a PR"),
        (status = 502, description = "GitHub could not be reached"),
        (status = 503, description = "No GitHub credential configured on this server")
    )
)]
pub async fn follow_up_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<FollowUpRequest>,
) -> Result<(StatusCode, Json<vise_core::sessions::model::Session>), StatusCode> {
    use crate::credentials::IssueError;
    use vise_core::sessions::model::Environment;
    use vise_core::sessions::pr_tracking::parse_pr_url;

    let internal = |error: anyhow::Error| {
        tracing::error!(%error, "follow-up failed");
        StatusCode::INTERNAL_SERVER_ERROR
    };

    let parent = state
        .sessions
        .get(&id)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Tracking lives on the session that opened the PR; follow-ups chain to it.
    let root = state
        .sessions
        .resolve_root(&id)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let pr_url = root
        .outcome
        .as_ref()
        .filter(|outcome| outcome.opened_pr())
        .and_then(|outcome| outcome.pr_url.clone())
        .ok_or_else(|| {
            tracing::warn!(session_id = %id, root_id = %root.id, "follow-up: root did not open a PR");
            StatusCode::UNPROCESSABLE_ENTITY
        })?;

    if root
        .pr_status
        .as_ref()
        .is_some_and(|status| status.state.is_terminal())
    {
        return Err(StatusCode::CONFLICT);
    }

    let repo = root
        .environment
        .repo
        .clone()
        .filter(|_| root.environment.kind == "github_repo")
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;

    let (pr_repo, number) = parse_pr_url(&pr_url).ok_or_else(|| {
        tracing::warn!(%pr_url, "follow-up: unparseable pr_url");
        StatusCode::UNPROCESSABLE_ENTITY
    })?;

    let provider = state
        .github_credentials()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let token = provider.issue(&root).await.map_err(|error| match error {
        IssueError::NotApplicable(reason) => {
            tracing::warn!(%reason, "follow-up: credential not applicable");
            StatusCode::UNPROCESSABLE_ENTITY
        }
        IssueError::Upstream(error) => {
            tracing::error!(%error, "follow-up: credential issue failed");
            StatusCode::BAD_GATEWAY
        }
    })?;

    let context = crate::follow_up::fetch_context(&state.github, &token.secret, &pr_repo, number)
        .await
        .map_err(|error| {
            tracing::error!(%error, %pr_url, "follow-up: github fetch failed");
            StatusCode::BAD_GATEWAY
        })?;

    // The snapshot may lag by one poll; GitHub's own answer is authoritative.
    if !context.pr.is_open() {
        return Err(StatusCode::CONFLICT);
    }

    let input = crate::follow_up::compose_input(&context, request.instructions.as_deref());
    let agent = request.agent.unwrap_or_else(|| parent.agent.clone());
    let environment = Environment {
        kind: "github_repo".into(),
        repo: Some(repo),
        // The PR's head branch, so the agent pushes to it and the same PR updates.
        base_branch: Some(context.pr.head.name.clone()),
    };

    let session = state
        .sessions
        .create_with_parent(agent, environment, input, Some(parent.id.clone()))
        .await
        .map_err(internal)?;

    Ok((StatusCode::CREATED, Json(session)))
}

struct EventCursor {
    state: AppState,
    id: String,
    after_seq: i64,
    buffer: VecDeque<vise_core::sessions::model::SessionEvent>,
    done: bool,
}

fn is_terminal(status: &vise_core::sessions::model::SessionStatus) -> bool {
    use vise_core::sessions::model::SessionStatus;
    matches!(
        status,
        SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
    )
}

/// The stream ends once the session is terminal, unless the session opened a
/// PR that the poller is still tracking: then it keeps tailing PR-state
/// events until the PR merges or closes.
fn stream_done(session: &vise_core::sessions::model::Session, pr_tracking_enabled: bool) -> bool {
    if !is_terminal(&session.status) {
        return false;
    }
    let tracking_pr = pr_tracking_enabled
        && session
            .outcome
            .as_ref()
            .is_some_and(|outcome| outcome.opened_pr());
    if !tracking_pr {
        return true;
    }
    session
        .pr_status
        .as_ref()
        .is_some_and(|status| status.state.is_terminal())
}

/// SSE tail: replays history from `after_seq`, then polls for new events until
/// the session reaches a terminal status (and, for sessions that opened a PR
/// while tracking is enabled, until that PR merges or closes), closing with a
/// `done` event.
pub async fn stream_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let cursor = EventCursor {
        state,
        id,
        after_seq: query.after_seq.unwrap_or(0),
        buffer: VecDeque::new(),
        done: false,
    };

    let stream = futures::stream::unfold(cursor, |mut cursor| async move {
        loop {
            if let Some(event) = cursor.buffer.pop_front() {
                cursor.after_seq = event.seq;
                match Event::default().id(event.seq.to_string()).json_data(&event) {
                    Ok(event) => return Some((Ok(event), cursor)),
                    Err(_) => continue,
                }
            }

            if cursor.done {
                return None;
            }

            match cursor
                .state
                .sessions
                .get_events(&cursor.id, cursor.after_seq, 256)
                .await
            {
                Ok(events) if !events.is_empty() => {
                    cursor.buffer.extend(events);
                }

                Ok(_) => match cursor.state.sessions.get(&cursor.id).await {
                    Ok(Some(session))
                        if stream_done(&session, cursor.state.pr_tracking_enabled) =>
                    {
                        cursor.done = true;
                        let event = Event::default()
                            .event("done")
                            .data(format!("{:?}", session.status).to_lowercase());
                        return Some((Ok(event), cursor));
                    }

                    Ok(Some(_)) => tokio::time::sleep(Duration::from_millis(1000)).await,

                    // missing session or db error: end the stream
                    _ => return None,
                },

                Err(_) => tokio::time::sleep(Duration::from_millis(1000)).await,
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
