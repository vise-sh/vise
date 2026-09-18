use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

/// Name of the bearer scheme user-facing operations reference in their
/// `security` list. Optional on every operation: the requirement is only
/// enforced when the server is configured with `VISE_API_TOKEN`.
pub const API_TOKEN_SCHEME: &str = "api_token";

struct ApiTokenScheme;

impl Modify for ApiTokenScheme {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            API_TOKEN_SCHEME,
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "Static API token for user-facing routes (sessions, host enrollment). \
                         Required only when the server sets VISE_API_TOKEN; host-protocol \
                         routes use the host's own vhost_ token instead.",
                    ))
                    .build(),
            ),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Vise API",
        version = "0.1.0",
        description = "API for managing Vise agent sessions"
    ),
    modifiers(&ApiTokenScheme),
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
        crate::routes::enrollment::mint_enrollment_token,
        crate::routes::enrollment::list_enrollment_tokens,
        crate::routes::enrollment::revoke_enrollment_token,
        crate::routes::enrollment::exchange_enrollment_token,
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
        crate::routes::enrollment::MintEnrollmentTokenRequest,
        crate::routes::enrollment::MintEnrollmentTokenResponse,
        crate::routes::enrollment::ExchangeEnrollmentTokenRequest,
        vise_core::sessions::model::Session,
        vise_core::sessions::model::SessionStatus,
        vise_core::sessions::model::SessionOutcome,
        vise_core::sessions::model::PrStatus,
        vise_core::sessions::model::PrState,
        vise_core::sessions::model::ChecksState,
        vise_core::sessions::model::SessionEvent,
        vise_core::sessions::model::NewSessionEvent,
        vise_core::hosts::model::Host,
        vise_core::workspaces::model::EventFidelity,
        vise_core::enrollment::model::EnrollmentToken,
    ))
)]
pub struct ApiDoc;
