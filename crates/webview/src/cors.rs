//! Virtual CORS policy for runtime requests that share the controlled gateway origin.

use std::collections::BTreeSet;

use crate::error::Result;
use crate::error::WebviewError;
use crate::types::GatewayCredentials;
use crate::types::GatewayHeader;
use crate::types::GatewayRequest;
use crate::types::GatewayResponse;

/// Build an upstream CORS preflight request when `request` requires one.
pub fn preflight_request(request: &GatewayRequest) -> Result<Option<GatewayRequest>> {
    if !request.is_cross_origin_runtime_request() || !requires_preflight(request) {
        return Ok(None);
    }
    let source_origin = request.source_origin.clone().ok_or_else(|| {
        WebviewError::Cors("cross-origin request has no trusted source origin".to_string())
    })?;
    let mut preflight = GatewayRequest::new(request.target.clone(), "OPTIONS", request.kind)
        .with_source_origin(source_origin)
        .with_credentials(GatewayCredentials::Omit)
        .with_header(GatewayHeader::new(
            "Access-Control-Request-Method",
            request.method.clone(),
        )?);
    let headers = non_safelisted_headers(request);
    if !headers.is_empty() {
        preflight = preflight.with_header(GatewayHeader::new(
            "Access-Control-Request-Headers",
            headers.into_iter().collect::<Vec<_>>().join(", "),
        )?);
    }
    Ok(Some(preflight))
}

/// Validate an upstream runtime response against the virtual source origin.
pub fn validate_response(request: &GatewayRequest, response: &GatewayResponse) -> Result<()> {
    if !request.is_cross_origin_runtime_request() {
        return Ok(());
    }
    let source_origin = source_origin(request)?;
    validate_allowed_origin(response, source_origin, request.credentials)?;
    if request.credentials == GatewayCredentials::Include
        && !header_has_token(response, "access-control-allow-credentials", "true")
    {
        return Err(WebviewError::Cors(
            "credentialed response lacks Access-Control-Allow-Credentials: true".to_string(),
        ));
    }
    Ok(())
}

/// Validate a CORS preflight response before forwarding the actual runtime request.
pub fn validate_preflight_response(
    request: &GatewayRequest,
    response: &GatewayResponse,
) -> Result<()> {
    if !(200..300).contains(&response.status) {
        return Err(WebviewError::Cors(format!(
            "preflight returned HTTP {}",
            response.status
        )));
    }
    let source_origin = source_origin(request)?;
    validate_allowed_origin(response, source_origin, request.credentials)?;
    let method_allowed = header_values(response, "access-control-allow-methods")
        .flat_map(|value| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case(request.method.as_str()));
    if !method_allowed {
        return Err(WebviewError::Cors(format!(
            "preflight does not allow method {}",
            request.method
        )));
    }
    let allowed_headers = header_values(response, "access-control-allow-headers")
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let requested_headers = non_safelisted_headers(request);
    if !requested_headers.is_empty()
        && !allowed_headers.contains("*")
        && !requested_headers
            .iter()
            .all(|header| allowed_headers.contains(header))
    {
        return Err(WebviewError::Cors(
            "preflight does not allow every requested header".to_string(),
        ));
    }
    if request.credentials == GatewayCredentials::Include
        && !header_has_token(response, "access-control-allow-credentials", "true")
    {
        return Err(WebviewError::Cors(
            "credentialed preflight lacks Access-Control-Allow-Credentials: true".to_string(),
        ));
    }
    Ok(())
}

fn requires_preflight(request: &GatewayRequest) -> bool {
    !is_safelisted_method(request.method.as_str()) || !non_safelisted_headers(request).is_empty()
}

fn is_safelisted_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "POST")
}

fn non_safelisted_headers(request: &GatewayRequest) -> BTreeSet<String> {
    request
        .headers
        .iter()
        .filter(|header| !is_gateway_stripped_header(header.name.as_str()))
        .filter(|header| !is_safelisted_header(header))
        .map(|header| header.name.to_ascii_lowercase())
        .collect()
}

fn is_gateway_stripped_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("sec-")
        || matches!(
            lower.as_str(),
            "accept-encoding"
                | "connection"
                | "content-length"
                | "cookie"
                | "expect"
                | "host"
                | "keep-alive"
                | "origin"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "proxy-connection"
                | "referer"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
                | "via"
        )
}

fn is_safelisted_header(header: &GatewayHeader) -> bool {
    if matches!(
        header.name.to_ascii_lowercase().as_str(),
        "accept" | "accept-language" | "content-language"
    ) {
        return true;
    }
    if !header.name_eq("content-type") {
        return false;
    }
    let content_type = header
        .value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        content_type.as_str(),
        "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
    )
}

fn source_origin(request: &GatewayRequest) -> Result<String> {
    request
        .source_origin
        .as_ref()
        .map(|source| source.origin().ascii_serialization())
        .ok_or_else(|| {
            WebviewError::Cors("cross-origin request has no trusted source origin".to_string())
        })
}

fn validate_allowed_origin(
    response: &GatewayResponse,
    source_origin: String,
    credentials: GatewayCredentials,
) -> Result<()> {
    let allowed = header_values(response, "access-control-allow-origin").any(|value| {
        let value = value.trim();
        value == source_origin || (value == "*" && credentials != GatewayCredentials::Include)
    });
    if allowed {
        Ok(())
    } else {
        Err(WebviewError::Cors(format!(
            "response does not allow origin {source_origin}"
        )))
    }
}

fn header_values<'a>(
    response: &'a GatewayResponse,
    name: &'a str,
) -> impl Iterator<Item = &'a str> {
    response
        .headers
        .iter()
        .filter(move |header| header.name_eq(name))
        .map(|header| header.value.as_str())
}

fn header_has_token(response: &GatewayResponse, name: &str, expected: &str) -> bool {
    header_values(response, name).any(|value| value.trim().eq_ignore_ascii_case(expected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GatewayRequestKind;
    use url::Url;

    fn request(credentials: GatewayCredentials) -> Result<GatewayRequest> {
        Ok(GatewayRequest::new(
            Url::parse("https://api.example.test/data")?,
            "PATCH",
            GatewayRequestKind::Fetch,
        )
        .with_source_origin(Url::parse("https://app.example.test/page")?)
        .with_credentials(credentials)
        .with_header(GatewayHeader::new("Origin", "http://127.0.0.1:8080")?)
        .with_header(GatewayHeader::new("Sec-Fetch-Mode", "cors")?)
        .with_header(GatewayHeader::new("X-Requested-With", "Rings")?))
    }

    #[test]
    fn preflight_contains_virtual_origin_method_and_headers() -> Result<()> {
        let request = request(GatewayCredentials::SameOrigin)?;
        let preflight = preflight_request(&request)?.ok_or_else(|| {
            WebviewError::Cors("expected cross-origin request to require preflight".to_string())
        })?;

        assert_eq!(preflight.method, "OPTIONS");
        assert_eq!(preflight.credentials, GatewayCredentials::Omit);
        assert!(preflight.headers.iter().any(|header| header
            .name_eq("access-control-request-method")
            && header.value == "PATCH"));
        assert!(preflight.headers.iter().any(|header| {
            header.name_eq("access-control-request-headers") && header.value == "x-requested-with"
        }));
        Ok(())
    }

    #[test]
    fn wildcard_response_is_denied_for_credentialed_runtime_requests() -> Result<()> {
        let request = request(GatewayCredentials::Include)?;
        let response = GatewayResponse::new(
            200,
            vec![GatewayHeader::new("Access-Control-Allow-Origin", "*")?],
            Vec::new(),
        )?;

        assert!(matches!(
            validate_response(&request, &response),
            Err(WebviewError::Cors(_))
        ));
        Ok(())
    }

    #[test]
    fn exact_origin_and_credentials_allow_cross_origin_runtime_response() -> Result<()> {
        let request = request(GatewayCredentials::Include)?;
        let response = GatewayResponse::new(
            200,
            vec![
                GatewayHeader::new("Access-Control-Allow-Origin", "https://app.example.test")?,
                GatewayHeader::new("Access-Control-Allow-Credentials", "true")?,
            ],
            Vec::new(),
        )?;

        validate_response(&request, &response)
    }
}
