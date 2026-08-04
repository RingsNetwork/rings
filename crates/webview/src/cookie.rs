use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use url::Host;
use url::Url;

use crate::error::CookieFailure;
use crate::error::Result;
use crate::types::GatewayRequest;
use crate::types::GatewayRequestKind;

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredCookie {
    partition: CookiePartitionKey,
    name: String,
    value: String,
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    expires_at: Option<i64>,
    same_site: SameSite,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CookieKey {
    partition: CookiePartitionKey,
    name: String,
    domain: String,
    path: String,
}

impl StoredCookie {
    fn key(&self) -> CookieKey {
        CookieKey {
            partition: self.partition.clone(),
            name: self.name.clone(),
            domain: self.domain.clone(),
            path: self.path.clone(),
        }
    }
}

impl CookieKey {
    fn matches(&self, cookie: &StoredCookie) -> bool {
        self.partition == cookie.partition
            && self.name == cookie.name
            && self.domain == cookie.domain
            && self.path == cookie.path
    }
}

/// Schemeful top-level site that partitions third-party cookie state.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CookiePartitionKey(String);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SameSite {
    Strict,
    Lax,
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SetCookieOutcome {
    Ignore,
    Remove(CookieKey),
    Upsert(StoredCookie),
}

struct CookieRequestContext<'a> {
    target: &'a Url,
    partition: CookiePartitionKey,
    source: Option<&'a Url>,
    method: &'a str,
    top_level_navigation: bool,
    kind: GatewayRequestKind,
}

/// Virtual cookie jar double-keyed by top-level site and target origin/domain/path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CookieJar {
    cookies: Vec<StoredCookie>,
}

impl CookieJar {
    /// Create an empty cookie jar.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store one upstream `Set-Cookie` header for `origin` at `now` milliseconds since Unix epoch.
    ///
    /// The supplied instant is used for `Max-Age` and `Expires` decisions, so callers that own a
    /// clock can replay cookie transitions without reading ambient time.
    pub fn store_set_cookie_at(&mut self, origin: &Url, set_cookie: &str, now: i64) -> Result<()> {
        let partition = cookie_partition_for_url(origin)?;
        self.store_set_cookie_in_partition_at(partition, origin, set_cookie, now)
    }

    /// Store one upstream `Set-Cookie` under the request's schemeful top-level site.
    pub fn store_set_cookie_for_request_at(
        &mut self,
        request: &GatewayRequest,
        set_cookie: &str,
        now: i64,
    ) -> Result<()> {
        let partition = cookie_partition_for_request(request)?;
        self.store_set_cookie_in_partition_at(partition, &request.target, set_cookie, now)
    }

    fn store_set_cookie_in_partition_at(
        &mut self,
        partition: CookiePartitionKey,
        origin: &Url,
        set_cookie: &str,
        now: i64,
    ) -> Result<()> {
        let outcome = evaluate_set_cookie(partition, origin, set_cookie, now)?;
        match outcome {
            SetCookieOutcome::Ignore => {}
            SetCookieOutcome::Remove(key) => self.remove_cookie(&key),
            SetCookieOutcome::Upsert(cookie) => {
                self.remove_cookie(&cookie.key());
                self.cookies.push(cookie);
            }
        }
        Ok(())
    }

    fn remove_cookie(&mut self, key: &CookieKey) {
        self.cookies.retain(|cookie| !key.matches(cookie));
    }

    /// Build a `Cookie` request header for `target` at `now` milliseconds since Unix epoch.
    pub fn cookie_header_at(&self, target: &Url, now: i64) -> Option<String> {
        self.cookie_header_matching(
            CookieRequestContext {
                target,
                partition: cookie_partition_for_url(target).ok()?,
                source: None,
                method: "GET",
                top_level_navigation: true,
                kind: GatewayRequestKind::Navigation,
            },
            now,
        )
    }

    /// Build a `Cookie` request header for one normalized gateway request at `now` milliseconds
    /// since Unix epoch.
    pub fn cookie_header_for_request_at(
        &self,
        request: &GatewayRequest,
        now: i64,
    ) -> Option<String> {
        let partition = cookie_partition_for_request(request).ok()?;
        self.cookie_header_matching(
            CookieRequestContext {
                target: &request.target,
                partition,
                source: request.source_origin.as_ref(),
                method: request.method.as_str(),
                top_level_navigation: request.top_level_navigation,
                kind: request.kind,
            },
            now,
        )
    }

    fn cookie_header_matching(
        &self,
        context: CookieRequestContext<'_>,
        now: i64,
    ) -> Option<String> {
        let host = context.target.host_str()?.to_ascii_lowercase();
        let path = context.target.path();
        let secure_request = context.target.scheme() == "https";
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .filter(|cookie| {
                !cookie_expired(cookie, now)
                    && cookie.partition == context.partition
                    && (!cookie.secure || secure_request)
                    && domain_matches(cookie, &host)
                    && path_matches(cookie.path.as_str(), path)
                    && same_site_allows_cookie(
                        cookie,
                        context.source,
                        context.target,
                        context.method,
                        context.top_level_navigation,
                        context.kind,
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

    /// Return the number of unexpired cookies at `now` milliseconds since Unix epoch.
    pub fn len_at(&self, now: i64) -> usize {
        self.cookies
            .iter()
            .filter(|cookie| !cookie_expired(cookie, now))
            .count()
    }

    /// Return true when no unexpired cookies exist at `now` milliseconds since Unix epoch.
    pub fn is_empty_at(&self, now: i64) -> bool {
        self.len_at(now) == 0
    }
}

fn evaluate_set_cookie(
    partition: CookiePartitionKey,
    origin: &Url,
    set_cookie: &str,
    now: i64,
) -> Result<SetCookieOutcome> {
    let Some(host) = origin.host_str() else {
        return Err(CookieFailure::MissingOriginHost.into());
    };
    let mut parts = set_cookie.split(';').map(str::trim);
    let Some(name_value) = parts.next() else {
        return Err(CookieFailure::EmptySetCookie.into());
    };
    let Some((name, value)) = name_value.split_once('=') else {
        return Err(CookieFailure::InvalidNameValue.into());
    };
    let name = name.trim();
    if name.is_empty() {
        return Err(CookieFailure::EmptyName.into());
    }

    let mut cookie = StoredCookie {
        partition,
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
                return Ok(SetCookieOutcome::Ignore);
            } else if key.eq_ignore_ascii_case("path") {
                let path = value.trim();
                cookie.path = if path.starts_with('/') {
                    path.to_string()
                } else {
                    "/".to_string()
                };
            } else if key.eq_ignore_ascii_case("max-age") {
                if let Ok(seconds) = value.trim().parse::<i64>() {
                    saw_max_age = true;
                    if seconds <= 0 {
                        delete_cookie = true;
                        cookie.expires_at = None;
                    } else {
                        delete_cookie = false;
                        cookie.expires_at = Some(now.saturating_add(max_age_millis(seconds)));
                    }
                }
            } else if key.eq_ignore_ascii_case("expires") && !saw_max_age {
                if let Ok(expires_at) = httpdate::parse_http_date(value.trim()) {
                    let expires_at = system_time_millis(expires_at);
                    if expires_at <= now {
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
        return Ok(SetCookieOutcome::Ignore);
    }
    Ok(if delete_cookie {
        SetCookieOutcome::Remove(cookie.key())
    } else {
        SetCookieOutcome::Upsert(cookie)
    })
}

#[cfg(test)]
impl CookieJar {
    pub(crate) fn store_set_cookie(&mut self, origin: &Url, set_cookie: &str) -> Result<()> {
        self.store_set_cookie_at(origin, set_cookie, 0)
    }

    pub(crate) fn cookie_header(&self, target: &Url) -> Option<String> {
        self.cookie_header_at(target, 0)
    }

    pub(crate) fn cookie_header_for_request(&self, request: &GatewayRequest) -> Option<String> {
        self.cookie_header_for_request_at(request, 0)
    }

    pub(crate) fn len(&self) -> usize {
        self.len_at(0)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.is_empty_at(0)
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

fn cookie_partition_for_request(request: &GatewayRequest) -> Result<CookiePartitionKey> {
    let top_level =
        if request.kind == GatewayRequestKind::Navigation && request.top_level_navigation {
            &request.target
        } else {
            request.source_origin.as_ref().unwrap_or(&request.target)
        };
    cookie_partition_for_url(top_level)
}

fn cookie_partition_for_url(url: &Url) -> Result<CookiePartitionKey> {
    let site = site_for_same_site(url).ok_or(CookieFailure::MissingOriginHost)?;
    Ok(CookiePartitionKey(format!("{}://{site}", url.scheme())))
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
        prefix.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_cookie_evaluation_is_a_pure_total_decision() -> Result<()> {
        let origin = Url::parse("https://example.com/app/index.html")?;

        assert!(matches!(
            evaluate_set_cookie(
                cookie_partition_for_url(&origin)?,
                &origin,
                "sid=one; Domain=example.com",
                1_000,
            )?,
            SetCookieOutcome::Ignore
        ));
        assert!(matches!(
            evaluate_set_cookie(
                cookie_partition_for_url(&origin)?,
                &origin,
                "sid=one; SameSite=None",
                1_000,
            )?,
            SetCookieOutcome::Ignore
        ));
        assert!(matches!(
            evaluate_set_cookie(
                cookie_partition_for_url(&origin)?,
                &origin,
                "sid=gone; Path=/app; Max-Age=0",
                1_000,
            )?,
            SetCookieOutcome::Remove(key) if key.path == "/app"
        ));
        assert!(matches!(
            evaluate_set_cookie(
                cookie_partition_for_url(&origin)?,
                &origin,
                "sid=one; Path=/app; Max-Age=2",
                1_000,
            )?,
            SetCookieOutcome::Upsert(cookie) if cookie.expires_at == Some(3_000)
        ));
        Ok(())
    }

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
        assert_eq!(jar.cookie_header(&insecure), None);
        Ok(())
    }

    #[test]
    fn third_party_cookies_are_partitioned_by_schemeful_top_level_site() -> Result<()> {
        let mut jar = CookieJar::new();
        let tracker = Url::parse("https://tracker.example/pixel")?;
        let under_news =
            GatewayRequest::new(tracker.clone(), "GET", GatewayRequestKind::Subresource)
                .with_source_origin(Url::parse("https://news.example/")?);
        let under_shop = GatewayRequest::new(tracker, "GET", GatewayRequestKind::Subresource)
            .with_source_origin(Url::parse("https://shop.example/")?);

        jar.store_set_cookie_for_request_at(
            &under_news,
            "id=news; Path=/; SameSite=None; Secure",
            0,
        )?;
        assert_eq!(
            jar.cookie_header_for_request_at(&under_news, 0).as_deref(),
            Some("id=news")
        );
        assert_eq!(jar.cookie_header_for_request_at(&under_shop, 0), None);

        jar.store_set_cookie_for_request_at(
            &under_shop,
            "id=shop; Path=/; SameSite=None; Secure",
            0,
        )?;
        assert_eq!(
            jar.cookie_header_for_request_at(&under_news, 0).as_deref(),
            Some("id=news")
        );
        assert_eq!(
            jar.cookie_header_for_request_at(&under_shop, 0).as_deref(),
            Some("id=shop")
        );
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
    fn invalid_max_age_does_not_suppress_valid_expires() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://example.com/app/index.html")?;

        jar.store_set_cookie_at(
            &origin,
            "sid=one; Max-Age=not-a-number; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            1_000,
        )?;

        assert!(jar.is_empty_at(1_000));
        Ok(())
    }

    #[test]
    fn default_cookie_path_uses_directory_without_trailing_slash() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://example.com/app/index.html")?;
        jar.store_set_cookie(&origin, "sid=one")?;

        assert_eq!(
            jar.cookie_header(&Url::parse("https://example.com/app/page")?)
                .as_deref(),
            Some("sid=one")
        );
        assert_eq!(
            jar.cookie_header(&Url::parse("https://example.com/application")?),
            None
        );
        Ok(())
    }

    #[test]
    fn max_age_expiry_is_visible_until_its_exact_boundary() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://example.com/app/index.html")?;
        let target = Url::parse("https://example.com/app/page")?;

        jar.store_set_cookie_at(&origin, "sid=one; Path=/app; Max-Age=2", 1_000)?;

        assert_eq!(
            jar.cookie_header_at(&target, 2_999).as_deref(),
            Some("sid=one")
        );
        assert_eq!(jar.len_at(2_999), 1);
        assert_eq!(jar.cookie_header_at(&target, 3_000), None);
        assert_eq!(jar.len_at(3_000), 0);
        Ok(())
    }

    #[test]
    fn replacement_and_deletion_are_replayable_at_one_instant() -> Result<()> {
        let mut jar = CookieJar::new();
        let origin = Url::parse("https://example.com/app/index.html")?;
        let target = Url::parse("https://example.com/app/page")?;
        let now = 1_000;

        jar.store_set_cookie_at(&origin, "sid=old; Path=/app", now)?;
        jar.store_set_cookie_at(&origin, "sid=new; Path=/app", now)?;
        assert_eq!(
            jar.cookie_header_at(&target, now).as_deref(),
            Some("sid=new")
        );
        assert_eq!(jar.len_at(now), 1);

        jar.store_set_cookie_at(&origin, "sid=gone; Path=/app; Max-Age=0", now)?;
        assert_eq!(jar.cookie_header_at(&target, now), None);
        assert!(jar.is_empty_at(now));
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

        let request = crate::types::GatewayRequest::subresource(Url::parse(
            "https://auth.example.test/app/script.js",
        )?)
        .with_source_origin(Url::parse("https://shop.example.net/page")?);
        jar.store_set_cookie_for_request_at(
            &request,
            "sid=none; Path=/; SameSite=None; Secure",
            0,
        )?;

        assert_eq!(
            jar.cookie_header_for_request(&request).as_deref(),
            Some("sid=none")
        );
        Ok(())
    }
}
