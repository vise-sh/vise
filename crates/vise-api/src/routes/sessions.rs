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

/// SSE tail: replays history from `after_seq`, then polls for new events until
/// the session reaches a terminal status, closing with a `done` event.
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
                    Ok(Some(session)) if is_terminal(&session.status) => {
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
