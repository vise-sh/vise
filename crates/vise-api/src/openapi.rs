use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Vise API",
        version = "0.1.0",
        description = "API for managing Vise agent sessions"
    ),
    paths(
        crate::routes::sessions::list_sessions,
        crate::routes::sessions::get_session,
        crate::routes::sessions::create_session,
        crate::routes::sessions::get_events,
        crate::routes::sessions::cancel_session,
        crate::routes::sessions::follow_up_session,
        crate::routes::hosts::enroll_host,
        crate::routes::hosts::list_hosts,
        crate::routes::hosts::claim,
        crate::routes::hosts::heartbeat,
        crate::routes::hosts::report_events,
        crate::routes::hosts::finish,
        crate::routes::hosts::issue_credential,
    ),
    components(schemas(
        crate::routes::sessions::CreateSessionRequest,
        crate::routes::sessions::ListSessionsResponse,
        crate::routes::sessions::FollowUpRequest,
        crate::routes::hosts::EnrollHostRequest,
        crate::routes::hosts::EnrollHostResponse,
        crate::routes::hosts::ClaimRequest,
        crate::routes::hosts::ClaimResponse,
        crate::routes::hosts::HeartbeatResponse,
        crate::routes::hosts::ReportEventsRequest,
        crate::routes::hosts::FinishRequest,
        crate::routes::hosts::IssueCredentialRequest,
        crate::routes::hosts::IssueCredentialResponse,
        vise_core::sessions::model::Session,
        vise_core::sessions::model::SessionStatus,
        vise_core::sessions::model::SessionOutcome,
        vise_core::sessions::model::PrStatus,
        vise_core::sessions::model::PrState,
        vise_core::sessions::model::ChecksState,
        vise_core::sessions::model::SessionEvent,
        vise_core::sessions::model::NewSessionEvent,
        vise_core::hosts::model::Host,
    ))
)]
pub struct ApiDoc;
