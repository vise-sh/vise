pub mod credentials;
pub mod follow_up;
pub mod github;
pub mod openapi;
pub mod pr_tracking;
mod routes;
mod state;

pub use state::AppState;

use axum::Router;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

pub fn app(state: AppState) -> Router {
    Router::<AppState>::new()
        .merge(routes::sessions())
        .merge(routes::hosts())
        .merge(SwaggerUi::new("/docs").url("/api-docs/openapi.json", openapi::ApiDoc::openapi()))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(DefaultOnRequest::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state)
}
