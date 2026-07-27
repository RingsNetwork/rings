use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use url::Host;
use url::Url;

use crate::error::Result;
use crate::error::WebviewError;
use crate::types::GatewayRequest;
use crate::types::GatewayRequestKind;

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredCookie {
    name: String,
    value: String,
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    expires_at: Option<i64>,
    same_site: SameSite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SameSite {
    Strict,
    Lax,
    None,
}

/// Virtual cookie jar keyed by target origin/domain/path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CookieJar {
    cookies: Vec<StoredCookie>,
}

impl CookieJar {
    /// Create an empty cookie jar.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store one upstream `Set-Cookie` header for `origin`.
    pub fn store_set_cookie(&mut self, origin: &Url, set_cookie: &str) -> Result<()> {
        let Some(host) = origin.host_str() else {
            return Err(WebviewError::Cookie(
                "cookie origin host is empty".to_string(),
            ));
        };
        let mut parts = set_cookie.split(';').map(str::trim);
        let Some(name_value) = parts.next() else {
            return Err(WebviewError::Cookie("empty Set-Cookie".to_string()));
        };
        let Some((name, value)) = name_value.split_once('=') else {
            return Err(WebviewError::Cookie(format!(
                "invalid Set-Cookie pair {name_value:?}"
            )));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(WebviewError::Cookie("cookie name is empty".to_string()));
        }

        let mut cookie = StoredCookie {
            name: name.to_string(),
            value: value.trim().to_string(),
            domain: host.to_ascii_lowercase(),
            host_only: true,
            path: default_cookie_path(origin.path()),
            secure: false,
            expires_at: None,
            same_site: SameSite::Lax,
        };
        let mut delete_cookie = false;
        let mut saw_max_age = false;

        for part in parts {
            let lower = part.to_ascii_lowercase();
            if lower == "secure" {
                cookie.secure = true;
                continue;
            }
            if let Some((key, value)) = part.split_once('=') {
                if key.eq_ignore_ascii_case("domain") {
                    return Ok(());
                } else if key.eq_ignore_ascii_case("path") {
                    let path = value.trim();
                    cookie.path = if path.starts_with('/') {
                        path.to_string()
                    } else {
                        "/".to_string()
                    };
                } else if key.eq_ignore_ascii_case("max-age") {
                    saw_max_age = true;
                    if let Ok(seconds) = value.trim().parse::<i64>() {
                        if seconds <= 0 {
                            delete_cookie = true;
                            cookie.expires_at = None;
                        } else {
                            delete_cookie = false;
                            cookie.expires_at =
                                Some(current_time_millis().saturating_add(max_age_millis(seconds)));
                        }
                    }
                } else if key.eq_ignore_ascii_case("expires") && !saw_max_age {
                    if let Ok(expires_at) = httpdate::parse_http_date(value.trim()) {
                        let expires_at = system_time_millis(expires_at);
                        if expires_at <= current_time_millis() {
                            delete_cookie = true;
                            cookie.expires_at = None;
                        } else {
                            cookie.expires_at = Some(expires_at);
                        }
                    }
                } else if key.eq_ignore_ascii_case("samesite") {
                    match value.trim().to_ascii_lowercase().as_str() {
                        "strict" => cookie.same_site = SameSite::Strict,
                        "lax" => cookie.same_site = SameSite::Lax,
                        "none" => cookie.same_site = SameSite::None,
                        _ => {}
                    }
                }
            }
        }

        if !delete_cookie && cookie.same_site == SameSite::None && !cookie.secure {
            return Ok(());
        }
        self.cookies.retain(|existing| {
            !(existing.name == cookie.name
                && existing.domain == cookie.domain
                && existing.path == cookie.path)
        });
        if delete_cookie {
            return Ok(());
        }
        self.cookies.push(cookie);
        Ok(())
    }

    /// Build a `Cookie` request header for `target`, if any jar entries match.
    pub fn cookie_header(&self, target: &Url) -> Option<String> {
        self.cookie_header_matching(target, None, "GET", true, GatewayRequestKind::Navigation)
    }

    /// Build a `Cookie` request header for one normalized gateway request.
    pub fn cookie_header_for_request(&self, request: &GatewayRequest) -> Option<String> {
        self.cookie_header_matching(
            &request.target,
            request.source_origin.as_ref(),
            request.method.as_str(),
            request.top_level_navigation,
            request.kind,
        )
    }

    fn cookie_header_matching(
        &self,
        target: &Url,
        source: Option<&Url>,
        method: &str,
        top_level_navigation: bool,
        kind: GatewayRequestKind,
    ) -> Option<String> {
        let host = target.host_str()?.to_ascii_lowercase();
        let path = target.path();
        let secure_request = target.scheme() == "https";
        let now = current_time_millis();
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .filter(|cookie| {
                !cookie_expired(cookie, now)
                    && (!cookie.secure || secure_request)
                    && domain_matches(cookie, &host)
                    && path_matches(cookie.path.as_str(), path)
                    && same_site_allows_cookie(
                        cookie,
                        source,
                        target,
                        method,
                        top_level_navigation,
                        kind,
                    )
            })
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }

    /// Return the number of cookies currently stored.
    pub fn len(&self) -> usize {
        let now = current_time_millis();
        self.cookies
            .iter()
            .filter(|cookie| !cookie_expired(cookie, now))
            .count()
    }

    /// Return true when no cookies are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn domain_matches(cookie: &StoredCookie, host: &str) -> bool {
    if cookie.host_only {
        return cookie.domain == host;
    }
    host == cookie.domain || host.ends_with(format!(".{}", cookie.domain).as_str())
}

fn cookie_expired(cookie: &StoredCookie, now: i64) -> bool {
    cookie
        .expires_at
        .is_some_and(|expires_at| expires_at <= now)
}

fn same_site_allows_cookie(
    cookie: &StoredCookie,
    source: Option<&Url>,
    target: &Url,
    method: &str,
    top_level_navigation: bool,
    kind: GatewayRequestKind,
) -> bool {
    if cookie.same_site == SameSite::None {
        return true;
    }
    if let Some(source) = source {
        if same_site(source, target) {
            return true;
        }
    } else {
        return kind == GatewayRequestKind::Navigation && top_level_navigation;
    }
    cookie.same_site == SameSite::Lax
        && top_level_navigation
        && kind == GatewayRequestKind::Navigation
        && is_safe_navigation_method(method)
}

fn same_site(source: &Url, target: &Url) -> bool {
    source.scheme() == target.scheme()
        && site_for_same_site(source)
            .zip(site_for_same_site(target))
            .is_some_and(|(source, target)| source.eq_ignore_ascii_case(target.as_str()))
}

fn site_for_same_site(url: &Url) -> Option<String> {
    match url.host()? {
        Host::Domain(host) => domain_site_for_same_site(host),
        Host::Ipv4(host) => Some(host.to_string()),
        Host::Ipv6(host) => Some(host.to_string()),
    }
}

fn domain_site_for_same_site(host: &str) -> Option<String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let registrable_domain = psl::domain_str(host.as_str()).unwrap_or(host.as_str());
    Some(
        registrable_domain
            .trim_end_matches('.')
            .to_ascii_lowercase(),
    )
}

fn is_safe_navigation_method(method: &str) -> bool {
    matches!(method.to_ascii_uppercase().as_str(), "GET" | "HEAD")
}

fn max_age_millis(seconds: i64) -> i64 {
    seconds.saturating_mul(1_000)
}

#[cfg(all(target_family = "wasm", feature = "browser"))]
fn current_time_millis() -> i64 {
    js_sys::Date::now() as i64
}

#[cfg(all(target_family = "wasm", not(feature = "browser")))]
fn current_time_millis() -> i64 {
    0
}

#[cfg(not(target_family = "wasm"))]
fn current_time_millis() -> i64 {
    system_time_millis(SystemTime::now())
}

fn system_time_millis(time: SystemTime) -> i64 {
    let Ok(duration) = time.duration_since(UNIX_EPOCH) else {
        return 0;
    };
    duration_millis(duration)
}

fn duration_millis(duration: Duration) -> i64 {
    let millis = duration.as_millis();
    if millis > i64::MAX as u128 {
        i64::MAX
    } else {
        millis as i64
    }
}

fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    let Some(rest) = request_path.strip_prefix(cookie_path) else {
        return false;
    };
    cookie_path.ends_with('/') || rest.starts_with('/')
}

fn default_cookie_path(path: &str) -> String {
    if !path.starts_with('/') {
        return "/".to_string();
    }
    let mut segments = path.rsplitn(2, '/');
    let _last = segments.next();
    let Some(prefix) = segments.next() else {
        return "/".to_string();
    };
    if prefix.is_empty() {
        "/".to_string()
    } else {
        format!("{prefix}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_matching_respects_host_only_path_and_secure() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://example.com/app/index.html")?;
        jar.store_set_cookie(&origin, "sid=one; Path=/app; Secure; HttpOnly")?;
        jar.store_set_cookie(&origin, "theme=dark; Path=/")?;

        let target = Url::parse("https://sub.example.com/app/page")?;
        assert_eq!(jar.cookie_header(&target), None);

        let host_target = Url::parse("https://example.com/app/page")?;
        assert_eq!(
            jar.cookie_header(&host_target).as_deref(),
            Some("sid=one; theme=dark")
        );

        let insecure = Url::parse("http://example.com/app/page")?;
        assert_eq!(jar.cookie_header(&insecure).as_deref(), Some("theme=dark"));
        Ok(())
    }

    #[test]
    fn cookie_ignores_domain_attributes() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://evil.example/set-cookie")?;

        jar.store_set_cookie(&origin, "sid=stolen; Domain=example.com; Path=/")?;
        assert!(jar.is_empty());
        Ok(())
    }

    #[test]
    fn cookie_ignores_registry_controlled_and_platform_domain_attributes() -> Result<()> {
        let mut jar = CookieJar::new();
        let dot_com_origin = Url::parse("https://evil.com/set-cookie")?;
        let dot_uk_origin = Url::parse("https://service.co.uk/set-cookie")?;
        let github_origin = Url::parse("https://evil.github.io/set-cookie")?;

        jar.store_set_cookie(&dot_com_origin, "sid=stolen; Domain=com; Path=/")?;
        jar.store_set_cookie(&dot_uk_origin, "sid=stolen; Domain=co.uk; Path=/")?;
        jar.store_set_cookie(&github_origin, "sid=stolen; Domain=github.io; Path=/")?;
        assert!(jar.is_empty());
        Ok(())
    }

    #[test]
    fn cookie_ignores_registrable_domain_attributes_by_default() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://app.example.co.uk/set-cookie")?;

        jar.store_set_cookie(&origin, "sid=one; Domain=example.co.uk; Path=/")?;
        assert!(jar.is_empty());
        Ok(())
    }

    #[test]
    fn cookie_path_matching_respects_segment_boundary() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://example.com/app/index.html")?;
        jar.store_set_cookie(&origin, "sid=one; Path=/app")?;

        let inside = Url::parse("https://example.com/app/page")?;
        let sibling = Url::parse("https://example.com/application")?;

        assert_eq!(jar.cookie_header(&inside).as_deref(), Some("sid=one"));
        assert_eq!(jar.cookie_header(&sibling), None);
        Ok(())
    }

    #[test]
    fn cookie_max_age_zero_deletes_existing_cookie() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://example.com/app/index.html")?;

        jar.store_set_cookie(&origin, "sid=one; Path=/app")?;
        jar.store_set_cookie(&origin, "sid=gone; Path=/app; Max-Age=0")?;

        let target = Url::parse("https://example.com/app/page")?;
        assert_eq!(jar.cookie_header(&target), None);
        assert!(jar.is_empty());
        Ok(())
    }

    #[test]
    fn cookie_past_expires_deletes_existing_cookie() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://example.com/app/index.html")?;

        jar.store_set_cookie(&origin, "sid=one; Path=/app")?;
        jar.store_set_cookie(
            &origin,
            "sid=gone; Path=/app; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
        )?;

        let target = Url::parse("https://example.com/app/page")?;
        assert_eq!(jar.cookie_header(&target), None);
        assert!(jar.is_empty());
        Ok(())
    }

    #[test]
    fn strict_cookie_is_not_sent_for_cross_site_subresource() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://auth.example.test/app/index.html")?;
        jar.store_set_cookie(&origin, "sid=strict; Path=/; SameSite=Strict; Secure")?;

        let request = crate::types::GatewayRequest::subresource(Url::parse(
            "https://auth.example.test/app/script.js",
        )?)
        .with_source_origin(Url::parse("https://shop.example.net/page")?);

        assert_eq!(jar.cookie_header_for_request(&request), None);
        Ok(())
    }

    #[test]
    fn strict_cookie_is_sent_for_same_site_subdomain_subresource() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://auth.example.com/app/index.html")?;
        jar.store_set_cookie(&origin, "sid=strict; Path=/; SameSite=Strict; Secure")?;

        let request = crate::types::GatewayRequest::subresource(Url::parse(
            "https://auth.example.com/app/script.js",
        )?)
        .with_source_origin(Url::parse("https://shop.example.com/page")?);

        assert_eq!(
            jar.cookie_header_for_request(&request).as_deref(),
            Some("sid=strict")
        );
        Ok(())
    }

    #[test]
    fn strict_cookie_is_not_sent_between_public_suffix_siblings() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://owner.github.io/app/index.html")?;
        jar.store_set_cookie(&origin, "sid=strict; Path=/; SameSite=Strict; Secure")?;

        let request = crate::types::GatewayRequest::subresource(Url::parse(
            "https://owner.github.io/app/script.js",
        )?)
        .with_source_origin(Url::parse("https://attacker.github.io/page")?);

        assert_eq!(jar.cookie_header_for_request(&request), None);
        Ok(())
    }

    #[test]
    fn strict_cookie_ip_sources_use_exact_host_site() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://127.0.0.1/app/index.html")?;
        jar.store_set_cookie(&origin, "sid=strict; Path=/; SameSite=Strict; Secure")?;
        let same_ip = crate::types::GatewayRequest::subresource(Url::parse(
            "https://127.0.0.1/app/script.js",
        )?)
        .with_source_origin(Url::parse("https://127.0.0.1/page")?);
        let cross_ip = crate::types::GatewayRequest::subresource(Url::parse(
            "https://127.0.0.1/app/script.js",
        )?)
        .with_source_origin(Url::parse("https://127.0.0.2/page")?);

        assert_eq!(
            jar.cookie_header_for_request(&same_ip).as_deref(),
            Some("sid=strict")
        );
        assert_eq!(jar.cookie_header_for_request(&cross_ip), None);
        Ok(())
    }

    #[test]
    fn strict_and_lax_cookies_are_not_sent_without_subresource_source_context() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://auth.example.test/app/index.html")?;
        jar.store_set_cookie(&origin, "strict=one; Path=/; SameSite=Strict; Secure")?;
        jar.store_set_cookie(&origin, "lax=one; Path=/; SameSite=Lax; Secure")?;
        jar.store_set_cookie(&origin, "none=one; Path=/; SameSite=None; Secure")?;

        let request = crate::types::GatewayRequest::subresource(Url::parse(
            "https://auth.example.test/app/script.js",
        )?);

        assert_eq!(
            jar.cookie_header_for_request(&request).as_deref(),
            Some("none=one")
        );
        Ok(())
    }

    #[test]
    fn lax_cookie_is_only_sent_on_safe_cross_site_top_level_navigation() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://auth.example.test/app/index.html")?;
        jar.store_set_cookie(&origin, "sid=lax; Path=/; SameSite=Lax; Secure")?;

        let source = Url::parse("https://shop.example.net/page")?;
        let safe_top_level = crate::types::GatewayRequest::navigation(Url::parse(
            "https://auth.example.test/app/page",
        )?)
        .with_source_origin(source.clone())
        .with_top_level_navigation(true);
        let unsafe_top_level = crate::types::GatewayRequest::navigation(Url::parse(
            "https://auth.example.test/app/delete",
        )?)
        .with_source_origin(source.clone())
        .with_top_level_navigation(true)
        .with_body("delete")
        .with_header(crate::types::GatewayHeader::new(
            "Content-Type",
            "text/plain",
        )?);
        let unsafe_top_level = crate::types::GatewayRequest {
            method: "POST".to_string(),
            ..unsafe_top_level
        };
        let iframe_navigation = crate::types::GatewayRequest::navigation(Url::parse(
            "https://auth.example.test/app/frame",
        )?)
        .with_source_origin(source)
        .with_top_level_navigation(false);

        assert_eq!(
            jar.cookie_header_for_request(&safe_top_level).as_deref(),
            Some("sid=lax")
        );
        assert_eq!(jar.cookie_header_for_request(&unsafe_top_level), None);
        assert_eq!(jar.cookie_header_for_request(&iframe_navigation), None);
        Ok(())
    }

    #[test]
    fn samesite_none_requires_secure_and_allows_cross_site_subresource() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://auth.example.test/app/index.html")?;

        jar.store_set_cookie(&origin, "keep=old; Path=/")?;
        jar.store_set_cookie(&origin, "keep=new; Path=/; SameSite=None")?;
        assert_eq!(
            jar.cookie_header(&Url::parse("https://auth.example.test/app/page")?)
                .as_deref(),
            Some("keep=old")
        );

        jar.store_set_cookie(&origin, "bad=none; Path=/; SameSite=None")?;
        assert_eq!(jar.len(), 1);

        jar.store_set_cookie(&origin, "sid=none; Path=/; SameSite=None; Secure")?;
        let request = crate::types::GatewayRequest::subresource(Url::parse(
            "https://auth.example.test/app/script.js",
        )?)
        .with_source_origin(Url::parse("https://shop.example.net/page")?);

        assert_eq!(
            jar.cookie_header_for_request(&request).as_deref(),
            Some("sid=none")
        );
        Ok(())
    }
}
