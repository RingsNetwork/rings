use super::CookieFailure;
use super::CorsFailure;
use super::GatewayFailureCode;
use super::TransportFailure;

#[test]
fn test_gateway_failure_codes_have_exhaustive_http_ui_projections() {
    let projections = [
        (
            GatewayFailureCode::GatewayTransportFailed,
            502,
            "gateway_transport_failed",
            "Gateway transport failed.",
        ),
        (
            GatewayFailureCode::GatewayOverloaded,
            503,
            "gateway_overloaded",
            "The local gateway is overloaded.",
        ),
        (
            GatewayFailureCode::OnionExitUnavailable,
            503,
            "onion_exit_unavailable",
            "No live HTTPS onion exit is available.",
        ),
        (
            GatewayFailureCode::OnionRouteUnavailable,
            503,
            "onion_route_unavailable",
            "No onion route is currently available for the requested target.",
        ),
        (
            GatewayFailureCode::OnionRequestTimedOut,
            504,
            "onion_request_timed_out",
            "Onion HTTPS proxy request timed out.",
        ),
    ];
    for (code, status, name, summary) in projections {
        assert_eq!(code.status(), status);
        assert_eq!(code.as_str(), name);
        assert_eq!(code.summary(), summary);
    }
}

#[test]
fn test_cors_failure_renders_only_after_the_closed_policy_fact_is_selected() {
    let projections = [
        (
            CorsFailure::MissingTrustedSourceOrigin,
            "cross-origin request has no trusted source origin",
        ),
        (
            CorsFailure::OriginNotAllowed,
            "response does not allow the controlled source origin",
        ),
        (
            CorsFailure::CredentialsNotAllowed,
            "credentialed response lacks Access-Control-Allow-Credentials: true",
        ),
        (
            CorsFailure::PreflightStatus { status: 418 },
            "preflight returned HTTP 418",
        ),
        (
            CorsFailure::PreflightMethodNotAllowed,
            "preflight does not allow the requested method",
        ),
        (
            CorsFailure::PreflightHeadersNotAllowed,
            "preflight does not allow every requested header",
        ),
        (
            CorsFailure::PreflightCredentialsNotAllowed,
            "credentialed preflight lacks Access-Control-Allow-Credentials: true",
        ),
    ];
    for (failure, rendered) in projections {
        assert_eq!(failure.to_string(), rendered);
    }
}

#[test]
fn test_cookie_failure_renders_only_after_the_closed_policy_fact_is_selected() {
    let projections = [
        (
            CookieFailure::MissingOriginHost,
            "cookie origin host is empty",
        ),
        (CookieFailure::EmptySetCookie, "empty Set-Cookie"),
        (CookieFailure::InvalidNameValue, "invalid Set-Cookie pair"),
        (CookieFailure::EmptyName, "cookie name is empty"),
        (
            CookieFailure::JarLockPoisoned,
            "virtual cookie jar lock is poisoned",
        ),
    ];
    for (failure, rendered) in projections {
        assert_eq!(failure.to_string(), rendered);
    }
}

#[test]
fn test_transport_failure_class_is_closed_before_an_adapter_renders_detail() {
    assert_eq!(
        TransportFailure::RequestSessionSentTwice.to_string(),
        "gateway request session was sent twice"
    );
    assert_eq!(
        TransportFailure::adapter("fixture transport stopped").to_string(),
        "fixture transport stopped"
    );
}
