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
use crate::auth::AuthedCaller;

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

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct FollowUpRequest {
    /// Extra guidance for the agent, appended after the review feedback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Agent configuration override; defaults to the parent session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<vise_core::sessions::model::Agent>,
}

#[utoipa::path(
    get,
    path = "/sessions",
    operation_id = "list_sessions",
    tag = "sessions",
    security(("api_token" = []), ()),
    responses(
        (
            status = 200,
            description = "List all sessions",
            body = ListSessionsResponse
        ),
        (status = 401, description = "Missing or invalid API token")
    )
)]
pub async fn list_sessions(
    State(state): State<AppState>,
    _caller: AuthedCaller,
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
    security(("api_token" = []), ()),
    params(
        ("id" = String, Path, description = "Session ID")
    ),
    responses(
        (
            status = 200,
            description = "Get a session",
            body = vise_core::sessions::model::Session
        ),
        (status = 401, description = "Missing or invalid API token")
    )
)]
pub async fn get_session(
    State(state): State<AppState>,
    _caller: AuthedCaller,
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
    security(("api_token" = []), ()),
    request_body = CreateSessionRequest,
    responses(
        (
            status = 201,
            description = "Session created",
            body = vise_core::sessions::model::Session
        ),
        (status = 401, description = "Missing or invalid API token"),
        (status = 422, description = "Invalid environment")
    )
)]
pub async fn create_session(
    State(state): State<AppState>,
    _caller: AuthedCaller,
    Json(request): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<vise_core::sessions::model::Session>), StatusCode> {
    if let Err(reason) = request.environment.validate() {
        tracing::warn!(%reason, "rejected session create");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let session = state
        .sessions
        .create(request.agent, request.environment, request.input, None)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(session)))
}

#[utoipa::path(
    post,
    path = "/sessions/{id}/follow-up",
    operation_id = "follow_up_session",
    tag = "sessions",
    security(("api_token" = []), ()),
    params(
        ("id" = String, Path, description = "Session ID of the session (or follow-up) whose PR to address")
    ),
    request_body = FollowUpRequest,
    responses(
        (
            status = 201,
            description = "Follow-up session created, targeting the PR's head branch",
            body = vise_core::sessions::model::Session
        ),
        (status = 401, description = "Missing or invalid API token"),
        (status = 404, description = "Session not found"),
        (status = 409, description = "The PR is already merged or closed"),
        (status = 422, description = "The session did not open a pull request"),
        (status = 502, description = "GitHub could not be reached"),
        (status = 503, description = "Neither the GitHub App nor a PAT is configured")
    )
)]
pub async fn follow_up_session(
    State(state): State<AppState>,
    _caller: AuthedCaller,
    Path(id): Path<String>,
    Json(request): Json<FollowUpRequest>,
) -> Result<(StatusCode, Json<vise_core::sessions::model::Session>), StatusCode> {
    use crate::follow_up::{FollowUpContext, compose_input};
    use crate::github::PullRef;
    use vise_core::sessions::model::Environment;

    let parent = state
        .sessions
        .get(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Tracking lives on the session that opened the PR; follow-ups chain to it.
    let root = state
        .sessions
        .resolve_tracking_root(&id)
        .await
        .map_err(|error| {
            tracing::error!(session_id = %id, %error, "follow-up root resolution failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let pr_url = root
        .outcome
        .as_ref()
        .filter(|outcome| outcome.kind == "pr_opened")
        .and_then(|outcome| outcome.pr_url.clone())
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let pr = PullRef::parse(&pr_url).ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;

    if root
        .pr_status
        .as_ref()
        .is_some_and(|status| status.state.is_terminal())
    {
        return Err(StatusCode::CONFLICT);
    }

    let github = state
        .github
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let upstream = |error: crate::github::GithubError| {
        tracing::error!(session_id = %root.id, %error, "github fetch failed");
        StatusCode::BAD_GATEWAY
    };
    let pull = github.pull(&pr).await.map_err(upstream)?;
    if pull.merged || pull.closed {
        return Err(StatusCode::CONFLICT);
    }
    let review_summaries = github.review_summaries(&pr).await.map_err(upstream)?;
    let review_comments = github.review_comments(&pr).await.map_err(upstream)?;
    let failing_checks: Vec<String> = github
        .check_runs(&pr, &pull.head_sha)
        .await
        .map_err(upstream)?
        .into_iter()
        .filter(|run| run.is_failing())
        .map(|run| run.name)
        .collect();

    let input = compose_input(&FollowUpContext {
        pr_url: &pull.html_url,
        head_ref: &pull.head_ref,
        review_summaries: &review_summaries,
        review_comments: &review_comments,
        failing_checks: &failing_checks,
        instructions: request.instructions.as_deref(),
    });

    let environment = Environment {
        kind: "github_repo".to_string(),
        repo: Some(pr.full_repo()),
        base_branch: Some(pull.head_ref.clone()),
    };
    let agent = request.agent.unwrap_or_else(|| parent.agent.clone());

    let session = state
        .sessions
        .create(agent, environment, input, Some(parent.id.clone()))
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
    security(("api_token" = []), ()),
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
        ),
        (status = 401, description = "Missing or invalid API token")
    )
)]
pub async fn get_events(
    State(state): State<AppState>,
    _caller: AuthedCaller,
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
    security(("api_token" = []), ()),
    params(
        ("id" = String, Path, description = "Session ID")
    ),
    responses(
        (
            status = 200,
            description = "Cancellation requested (pending sessions are cancelled immediately)",
            body = vise_core::sessions::model::Session
        ),
        (status = 401, description = "Missing or invalid API token"),
        (status = 404, description = "Session not found"),
        (status = 409, description = "Session already finished")
    )
)]
pub async fn cancel_session(
    State(state): State<AppState>,
    _caller: AuthedCaller,
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

/// A finished session that opened a PR keeps producing `pr_state_changed` /
/// `checks_state_changed` events until the PR merges or closes, so the tail
/// stays open ("watch this PR to merge") until the snapshot is terminal.
fn is_tracking_pr(session: &vise_core::sessions::model::Session) -> bool {
    session
        .outcome
        .as_ref()
        .is_some_and(|outcome| outcome.kind == "pr_opened")
        && !session
            .pr_status
            .as_ref()
            .is_some_and(|status| status.state.is_terminal())
}

/// SSE tail: replays history from `after_seq`, then polls for new events until
/// the session reaches a terminal status (and, for sessions that opened a PR,
/// until the PR is merged or closed), closing with a `done` event.
pub async fn stream_events(
    State(state): State<AppState>,
    _caller: AuthedCaller,
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
                    Ok(Some(session)) if is_terminal(&session.status) => {
                        if is_tracking_pr(&session) {
                            tokio::time::sleep(Duration::from_millis(1000)).await;
                            continue;
                        }
                        cursor.done = true;
                        let data = match session.pr_status.as_ref() {
                            Some(status) if status.state.is_terminal() => {
                                status.state.as_str().to_string()
                            }
                            _ => format!("{:?}", session.status).to_lowercase(),
                        };
                        let event = Event::default().event("done").data(data);
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
