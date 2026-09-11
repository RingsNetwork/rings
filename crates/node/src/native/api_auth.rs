//! Authentication and browser-origin policy for the native JSON-RPC listeners.
//!
//! A node serves two listeners. The internal listener is the operator's control surface and
//! gates every route behind the Bearer token. The external listener is the surface peers dial to
//! perform the HTTP handshake: its `nodeDid` and `answerOffer` methods are public, while its
//! status and registry reads stay gated. The requirement for one request is the pure function
//! `required(listener, body) = floor(listener) ⊔ class(body)` over the join-semilattice
//! [`rings_rpc::method::AuthorizationClass`]; this module owns the listener floor and that
//! function, and the endpoint layer only applies its verdict.

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use axum::http::header::HeaderName;
use axum::http::header::HeaderValue;
use axum::http::header::ACCEPT;
use axum::http::header::AUTHORIZATION;
use axum::http::header::CONTENT_TYPE;
use axum::http::Method;
use rand::rngs::OsRng;
use rand::RngCore;
use rings_rpc::method::AuthorizationClass;
use subtle::ConstantTimeEq;
use tower_http::cors::AllowOrigin;
use tower_http::cors::CorsLayer;

use crate::util::expand_home;

const API_TOKEN_BYTES: usize = 32;
const MIN_API_TOKEN_LEN: usize = 32;
const DEFAULT_API_TOKEN_FILE: &str = "api-token";

/// Errors raised while configuring API authentication or managing its token file.
#[derive(Debug, thiserror::Error)]
pub enum ApiSecurityError {
    /// A configured path could not be expanded or resolved.
    #[error("invalid API token path: {0}")]
    InvalidPath(String),
    /// The token file could not be read, created, inspected, or written.
    #[error("API token file operation failed for {path}: {source}")]
    TokenFileIo {
        /// Token file involved in the failed operation.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// The token file grants access to group or other users.
    #[error("API token file {path} must not be accessible by group or other users")]
    InsecureTokenPermissions {
        /// Insecure token file path.
        path: PathBuf,
    },
    /// The token is too short or contains whitespace.
    #[error("API token must contain at least {MIN_API_TOKEN_LEN} non-whitespace characters")]
    InvalidToken,
    /// A CORS origin is not one exact HTTP(S) origin.
    #[error("invalid API allowed origin {0:?}")]
    InvalidAllowedOrigin(String),
    /// A non-loopback external listener was requested without explicit opt-in.
    #[error("external API address {0} is non-loopback; set allow_remote_external_api explicitly")]
    RemoteExternalApiDisabled(SocketAddr),
}

/// An API authentication token loaded from its owner-only file.
pub struct LoadedApiToken {
    path: PathBuf,
    secret: String,
}

impl LoadedApiToken {
    /// Return the token file path without exposing the secret.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the token for configuring an authenticated client or server.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// Consume the loaded file and return its token secret.
    pub fn into_secret(self) -> String {
        self.secret
    }
}

/// The two JSON-RPC listeners a native node serves, distinguished by their authorization floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiListener {
    /// Loopback control listener: every route demands the token.
    Internal,
    /// Peer-facing listener: the HTTP handshake is public, everything else demands the token.
    External,
}

impl ApiListener {
    /// Return the least class this listener demands of any request, `floor(self)`.
    pub fn authorization_floor(self) -> AuthorizationClass {
        match self {
            ApiListener::Internal => AuthorizationClass::Gated,
            ApiListener::External => AuthorizationClass::Public,
        }
    }

    /// Return the class this listener demands of one JSON-RPC body:
    /// `floor(self) ⊔ class(body)`.
    ///
    /// `None` stands for a body that did not decode as JSON-RPC. It carries no classifiable call
    /// and is `⊤`, so a decoding failure is reported only to an authenticated caller.
    pub fn required_authorization(
        self,
        body: Option<&jsonrpc_core::Request>,
    ) -> AuthorizationClass {
        let body_class = body.map_or(AuthorizationClass::Gated, AuthorizationClass::of_request);
        self.authorization_floor().join(body_class)
    }
}

/// Immutable authentication and browser-origin policy shared by both listeners of one node.
pub struct ApiSecurity {
    token: Box<str>,
    allowed_origins: Vec<HeaderValue>,
    allow_remote_external_api: bool,
}

impl ApiSecurity {
    /// Validate and construct an API policy from a token and exact allowed origins.
    pub fn new(
        token: String,
        allowed_origins: &[String],
        allow_remote_external_api: bool,
    ) -> Result<Self, ApiSecurityError> {
        validate_token(&token)?;
        let allowed_origins = allowed_origins
            .iter()
            .map(|origin| parse_allowed_origin(origin))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            token: token.into_boxed_str(),
            allowed_origins,
            allow_remote_external_api,
        })
    }

    /// Return whether the request contains exactly one valid Bearer credential.
    pub fn authorizes(&self, headers: &axum::http::HeaderMap) -> bool {
        let mut values = headers.get_all(AUTHORIZATION).iter();
        let Some(value) = values.next() else {
            return false;
        };
        if values.next().is_some() {
            return false;
        }
        let Ok(value) = value.to_str() else {
            return false;
        };
        let Some((scheme, provided)) = value.split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("Bearer")
            && provided.as_bytes().ct_eq(self.token.as_bytes()).into()
    }

    /// Build the explicit CORS layer for this policy.
    pub fn cors_layer(&self) -> CorsLayer {
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(self.allowed_origins.clone()))
            .allow_methods([Method::GET, Method::POST])
            .allow_headers([AUTHORIZATION, CONTENT_TYPE, ACCEPT])
            .expose_headers([HeaderName::from_static("x-node-version")])
    }

    /// Validate an external listener address against the explicit remote-bind policy.
    pub fn validate_external_listener(&self, address: SocketAddr) -> Result<(), ApiSecurityError> {
        if address.ip().is_loopback() || self.allow_remote_external_api {
            Ok(())
        } else {
            Err(ApiSecurityError::RemoteExternalApiDisabled(address))
        }
    }
}

/// Resolve and load the configured API token without creating a missing file.
pub fn load_api_token(
    config_path: impl AsRef<Path>,
    configured_path: Option<&str>,
) -> Result<LoadedApiToken, ApiSecurityError> {
    let path = resolve_api_token_path(config_path, configured_path)?;
    read_api_token(path)
}

/// Resolve and load the configured API token, creating a random owner-only token if absent.
pub fn load_or_create_api_token(
    config_path: impl AsRef<Path>,
    configured_path: Option<&str>,
) -> Result<LoadedApiToken, ApiSecurityError> {
    let path = resolve_api_token_path(config_path, configured_path)?;
    match create_api_token(&path) {
        Ok(token) => Ok(LoadedApiToken {
            path,
            secret: token,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_api_token(path),
        Err(source) => Err(ApiSecurityError::TokenFileIo { path, source }),
    }
}

/// Load and validate a token from an explicit file path.
pub fn load_api_token_file(path: impl AsRef<Path>) -> Result<LoadedApiToken, ApiSecurityError> {
    let path = expand_home(path.as_ref()).map_err(|error| {
        ApiSecurityError::InvalidPath(format!("{}: {error}", path.as_ref().display()))
    })?;
    read_api_token(path)
}

fn resolve_api_token_path(
    config_path: impl AsRef<Path>,
    configured_path: Option<&str>,
) -> Result<PathBuf, ApiSecurityError> {
    let config_path = expand_home(config_path.as_ref()).map_err(|error| {
        ApiSecurityError::InvalidPath(format!("{}: {error}", config_path.as_ref().display()))
    })?;
    let parent = config_path.parent().ok_or_else(|| {
        ApiSecurityError::InvalidPath(format!("{} has no parent", config_path.display()))
    })?;
    let Some(configured_path) = configured_path else {
        return Ok(parent.join(DEFAULT_API_TOKEN_FILE));
    };
    let expanded = expand_home(configured_path)
        .map_err(|error| ApiSecurityError::InvalidPath(format!("{configured_path}: {error}")))?;
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(parent.join(expanded))
    }
}

fn create_api_token(path: &Path) -> std::io::Result<String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    let mut bytes = [0_u8; API_TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let token = base64::encode_config(bytes, base64::URL_SAFE_NO_PAD);
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(token)
}

fn read_api_token(path: PathBuf) -> Result<LoadedApiToken, ApiSecurityError> {
    ensure_private_permissions(&path)?;
    let secret = fs::read_to_string(&path)
        .map_err(|source| ApiSecurityError::TokenFileIo {
            path: path.clone(),
            source,
        })?
        .trim()
        .to_string();
    validate_token(&secret)?;
    Ok(LoadedApiToken { path, secret })
}

#[cfg(unix)]
fn ensure_private_permissions(path: &Path) -> Result<(), ApiSecurityError> {
    let metadata = fs::metadata(path).map_err(|source| ApiSecurityError::TokenFileIo {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.permissions().mode() & 0o077 == 0 {
        Ok(())
    } else {
        Err(ApiSecurityError::InsecureTokenPermissions {
            path: path.to_path_buf(),
        })
    }
}

#[cfg(not(unix))]
fn ensure_private_permissions(_path: &Path) -> Result<(), ApiSecurityError> {
    Ok(())
}

fn validate_token(token: &str) -> Result<(), ApiSecurityError> {
    if token.len() < MIN_API_TOKEN_LEN || token.chars().any(char::is_whitespace) {
        Err(ApiSecurityError::InvalidToken)
    } else {
        Ok(())
    }
}

fn parse_allowed_origin(origin: &str) -> Result<HeaderValue, ApiSecurityError> {
    let parsed = reqwest::Url::parse(origin)
        .map_err(|_| ApiSecurityError::InvalidAllowedOrigin(origin.to_string()))?;
    let exact_origin = matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.path() == "/"
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    if !exact_origin || origin == "*" {
        return Err(ApiSecurityError::InvalidAllowedOrigin(origin.to_string()));
    }
    HeaderValue::from_str(parsed.origin().ascii_serialization().as_str())
        .map_err(|_| ApiSecurityError::InvalidAllowedOrigin(origin.to_string()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use rings_rpc::method::Method as RpcMethod;

    use super::*;

    const SEED_ENV: &str = "RINGS_DECODE_BOUNDARY_SEED";
    const CASES_ENV: &str = "RINGS_DECODE_BOUNDARY_CASES";
    const DEFAULT_CASES: usize = 128;

    fn token() -> String {
        "0123456789abcdef0123456789abcdef".to_string()
    }

    #[test]
    fn bearer_auth_requires_exactly_one_constant_time_secret_match() {
        let security = ApiSecurity::new(token(), &[], false);
        assert!(security.is_ok());
        let security = security.ok();
        assert!(security.is_some());
        let Some(security) = security else {
            return;
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer 0123456789abcdef0123456789abcdef"),
        );
        assert!(security.authorizes(&headers));

        headers.append(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer 0123456789abcdef0123456789abcdef"),
        );
        assert!(!security.authorizes(&headers));

        headers.remove(AUTHORIZATION);
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer wrong"));
        assert!(!security.authorizes(&headers));
    }

    #[test]
    fn the_listener_floor_joins_with_the_body_class() {
        let handshake = jsonrpc_core::Request::Batch(vec![call("nodeDid"), call("answerOffer")]);
        let status = jsonrpc_core::Request::Single(call("nodeInfo"));
        assert_eq!(
            ApiListener::External.required_authorization(Some(&handshake)),
            AuthorizationClass::Public
        );
        assert_eq!(
            ApiListener::External.required_authorization(Some(&status)),
            AuthorizationClass::Gated
        );
        assert_eq!(
            ApiListener::External.required_authorization(None),
            AuthorizationClass::Gated
        );
        assert_eq!(
            ApiListener::Internal.required_authorization(Some(&handshake)),
            AuthorizationClass::Gated
        );
    }

    fn call(method: &str) -> jsonrpc_core::Call {
        jsonrpc_core::Call::MethodCall(jsonrpc_core::MethodCall {
            jsonrpc: Some(jsonrpc_core::Version::V2),
            method: method.to_string(),
            params: jsonrpc_core::Params::None,
            id: jsonrpc_core::Id::Num(1),
        })
    }

    #[test]
    fn generated_jsonrpc_decode_boundary_bodies_classify_without_public_fail_open() {
        let mut generator = DecodeBoundaryGenerator::named("node-jsonrpc");
        let cases = generated_case_count();
        eprintln!(
            "{SEED_ENV}={} {CASES_ENV}={cases} target=node-jsonrpc",
            generator.seed()
        );

        for _ in 0..cases {
            let body = generated_jsonrpc_body(&mut generator);
            let decoded = crate::native::endpoint::DecodedJsonRpc::decode(&body);
            let external = ApiListener::External.required_authorization(decoded.request());
            let internal = ApiListener::Internal.required_authorization(decoded.request());
            assert_eq!(internal, AuthorizationClass::Gated);
            match decoded.request() {
                None => assert_eq!(external, AuthorizationClass::Gated),
                Some(request) => assert_eq!(
                    external == AuthorizationClass::Public,
                    every_call_is_public(request)
                ),
            }
        }
    }

    fn generated_jsonrpc_body(generator: &mut DecodeBoundaryGenerator) -> Vec<u8> {
        match generator.usize(6) {
            0 => generator.bytes(2048),
            1 => method_call(generator, true).into_bytes(),
            2 => method_call(generator, false).into_bytes(),
            3 => batch_call(generator).into_bytes(),
            4 => br#"{"jsonrpc":"2.0","id":4}"#.to_vec(),
            _ => br#"[{"jsonrpc":"2.0","id":1}]"#.to_vec(),
        }
    }

    fn method_call(generator: &mut DecodeBoundaryGenerator, include_id: bool) -> String {
        let method = generator.method();
        if include_id {
            format!(
                r#"{{"jsonrpc":"2.0","id":{},"method":"{method}","params":[]}}"#,
                generator.next_u64()
            )
        } else {
            format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":[]}}"#)
        }
    }

    fn batch_call(generator: &mut DecodeBoundaryGenerator) -> String {
        format!(
            "[{},{}]",
            method_call(generator, true),
            method_call(generator, true)
        )
    }

    fn every_call_is_public(request: &jsonrpc_core::Request) -> bool {
        match request {
            jsonrpc_core::Request::Single(call) => call_is_public(call),
            jsonrpc_core::Request::Batch(calls) => calls.iter().all(call_is_public),
        }
    }

    fn call_is_public(call: &jsonrpc_core::Call) -> bool {
        match call {
            jsonrpc_core::Call::MethodCall(call) => method_is_public(call.method.as_str()),
            jsonrpc_core::Call::Notification(notification) => {
                method_is_public(notification.method.as_str())
            }
            jsonrpc_core::Call::Invalid { .. } => false,
        }
    }

    fn method_is_public(name: &str) -> bool {
        matches!(
            RpcMethod::try_from(name),
            Ok(RpcMethod::NodeDid | RpcMethod::AnswerOffer)
        )
    }

    fn generated_case_count() -> usize {
        std::env::var(CASES_ENV)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .map(|cases| cases.clamp(1, 50_000))
            .unwrap_or(DEFAULT_CASES)
    }

    struct DecodeBoundaryGenerator {
        seed: u64,
        state: u64,
    }

    impl DecodeBoundaryGenerator {
        fn named(label: &str) -> Self {
            let seed = std::env::var(SEED_ENV)
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_else(time_seed);
            let state = fold_label(seed, label);
            Self { seed, state }
        }

        fn seed(&self) -> u64 {
            self.seed
        }

        fn next_u64(&mut self) -> u64 {
            self.state = splitmix64(self.state);
            self.state
        }

        fn bytes(&mut self, max_len: usize) -> Vec<u8> {
            let len = self.usize(max_len.saturating_add(1));
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                bytes.push(self.byte());
            }
            bytes
        }

        fn byte(&mut self) -> u8 {
            u8::try_from(self.next_u64() & 0xff).unwrap_or(0)
        }

        fn usize(&mut self, upper: usize) -> usize {
            let upper = u64::try_from(upper).unwrap_or(u64::MAX).max(1);
            usize::try_from(self.next_u64() % upper).unwrap_or(0)
        }

        fn method(&mut self) -> &'static str {
            match self.usize(5) {
                0 => "nodeDid",
                1 => "answerOffer",
                2 => "nodeInfo",
                3 => "disconnect",
                _ => "generatedUnknown",
            }
        }
    }

    fn time_seed() -> u64 {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(duration.as_secs());
        nanos ^ u64::from(std::process::id())
    }

    fn fold_label(seed: u64, label: &str) -> u64 {
        label
            .bytes()
            .fold(seed, |state, byte| splitmix64(state ^ u64::from(byte)))
    }

    fn splitmix64(mut state: u64) -> u64 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    }

    #[test]
    fn wildcard_and_non_origin_cors_values_are_rejected() {
        for origin in ["*", "https://example.com/path", "file:///tmp/page"] {
            assert!(matches!(
                ApiSecurity::new(token(), &[origin.to_string()], false),
                Err(ApiSecurityError::InvalidAllowedOrigin(_))
            ));
        }
        assert!(ApiSecurity::new(token(), &["https://example.com".to_string()], false).is_ok());
    }

    #[test]
    fn remote_external_bind_requires_explicit_opt_in() {
        let local = "127.0.0.1:50001".parse::<SocketAddr>();
        let remote = "0.0.0.0:50001".parse::<SocketAddr>();
        let security = ApiSecurity::new(token(), &[], false);
        assert!(matches!(
            (security, local, remote),
            (Ok(security), Ok(local), Ok(remote))
                if security.validate_external_listener(local).is_ok()
                    && matches!(
                        security.validate_external_listener(remote),
                        Err(ApiSecurityError::RemoteExternalApiDisabled(_))
                    )
        ));
    }

    #[test]
    fn generated_token_is_reused_from_an_owner_only_file() {
        let root = std::env::temp_dir().join(format!("rings-api-auth-{}", uuid::Uuid::new_v4()));
        let config = root.join("config.yaml");
        let first = load_or_create_api_token(&config, None);
        let second = load_or_create_api_token(&config, None);
        assert!(matches!(
            (&first, &second),
            (Ok(first), Ok(second))
                if first.secret() == second.secret()
                    && first.path() == root.join(DEFAULT_API_TOKEN_FILE)
        ));
        let _ = fs::remove_file(root.join(DEFAULT_API_TOKEN_FILE));
        let _ = fs::remove_dir(root);
    }
}
