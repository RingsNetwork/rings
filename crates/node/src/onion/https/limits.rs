use super::*;

pub(super) fn https_response_body_limit(remaining_policy_bytes: Option<u64>) -> u64 {
    remaining_policy_bytes.map_or(DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES, |remaining| {
        remaining.min(DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES)
    })
}

#[cfg(any(test, rings_browser))]
pub(super) fn checked_status_code(status: f64) -> Result<u16> {
    if !status.is_finite() || status.fract() != 0.0 || !(100.0..=999.0).contains(&status) {
        return Err(Error::HttpRequestError(format!(
            "fetch response status is not a valid HTTP status: {status:?}"
        )));
    }
    status
        .to_string()
        .parse::<u16>()
        .map_err(|_| Error::HttpRequestError(format!("invalid fetch response status {status:?}")))
}

/// The bytes of response headers as the exit's byte policy counts them: every name and value.
pub(super) fn headers_bytes(headers: &[(String, String)]) -> Result<u64> {
    usize_to_u64(
        headers
            .iter()
            .map(|(name, value)| name.len().saturating_add(value.len()))
            .fold(0_usize, usize::saturating_add),
    )
}

fn usize_to_u64(value: usize) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::InvalidData)
}

pub(super) fn reject_content_length_over_limit(
    headers: &[(String, String)],
    max_body_bytes: u64,
) -> Result<()> {
    let Some(length) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<u64>().ok())
    else {
        return Ok(());
    };
    if length > max_body_bytes {
        return Err(Error::NoPermission);
    }
    Ok(())
}

/// Admit a response body chunk of `chunk_len` bytes after `body_len` bytes: past
/// `max_body_bytes` it is refused with [`Error::NoPermission`]; otherwise it is recorded
/// against the exit's byte budget.
pub(super) fn admit_body_chunk(
    body_len: usize,
    chunk_len: usize,
    max_body_bytes: u64,
    record_bytes: &impl Fn(u64) -> Result<()>,
) -> Result<()> {
    let chunk_len = usize_to_u64(chunk_len)?;
    if usize_to_u64(body_len)?.saturating_add(chunk_len) > max_body_bytes {
        return Err(Error::NoPermission);
    }
    record_bytes(chunk_len)
}

/// Cancellation target for an in-flight browser HTTPS fetch and its response stream.
#[cfg(any(test, rings_browser))]
pub(super) trait FetchCancelTarget {
    /// Underlying response body handle (`ReadableStream` in browser builds).
    type Body;
    /// Underlying stream reader handle (`ReadableStreamDefaultReader` in browser builds).
    type Reader;

    /// Cancel the unconsumed response body stream before a reader is acquired.
    fn cancel_body(&mut self, body: &Self::Body) -> bool;

    /// Cancel the active response body reader; returns `true` when reader cancellation was
    /// invoked.
    fn cancel_reader(&mut self, reader: &Self::Reader) -> bool;

    /// Abort the `AbortController` associated with the in-flight `fetch`.
    fn abort(&mut self);
}

/// RAII guard that aborts an in-flight fetch (and cancels any pending response body or reader)
/// whenever the owning Rust future is dropped or returns early with an error before [`Self::disarm`].
#[cfg(any(test, rings_browser))]
pub(super) struct FetchCancellationGuard<T: FetchCancelTarget> {
    target: T,
    body: Option<T::Body>,
    reader: Option<T::Reader>,
    armed: bool,
}

#[cfg(any(test, rings_browser))]
impl<T: FetchCancelTarget> FetchCancellationGuard<T> {
    /// Arm a guard owning `target`.
    pub(super) const fn new(target: T) -> Self {
        Self {
            target,
            body: None,
            reader: None,
            armed: true,
        }
    }

    /// Record the response body stream so early header/budget rejection cancels it.
    pub(super) fn set_body(&mut self, body: T::Body) {
        self.body = Some(body);
    }

    /// Record the locked stream reader so mid-stream error or cancellation cancels it.
    pub(super) fn set_reader(&mut self, reader: T::Reader) {
        self.reader = Some(reader);
    }

    /// Disarm the guard once the fetch and response body have been completely consumed.
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(any(test, rings_browser))]
impl<T: FetchCancelTarget> Drop for FetchCancellationGuard<T> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let cancelled_reader = self
            .reader
            .take()
            .is_some_and(|reader| self.target.cancel_reader(&reader));
        if !cancelled_reader {
            if let Some(body) = self.body.take() {
                let _ = self.target.cancel_body(&body);
            }
        }
        self.target.abort();
    }
}

/// Abstracted step interface for an in-flight browser HTTPS fetch so its cancellation and budget
/// state machine is exercised identically in browser builds and deterministic unit tests.
#[cfg(any(test, rings_browser))]
pub(super) trait CancellableHttpsFetch: Sized {
    /// Raw response handle returned by `fetch`.
    type Response;
    /// Stream body handle extracted from `Self::Response`.
    type Body: Clone;
    /// Stream reader handle acquired from `Self::Body`.
    type Reader: Clone;
    /// Cancellation target paired with this fetch.
    type Cancel: FetchCancelTarget<Body = Self::Body, Reader = Self::Reader>;

    /// Wait for the initial fetch promise to resolve to a response.
    async fn await_response(&mut self) -> Result<Self::Response>;

    /// Extract the numeric HTTP status from `response`.
    fn response_status(&self, response: &Self::Response) -> Result<f64>;

    /// Extract the optional body stream from `response`.
    fn response_body(&self, response: &Self::Response) -> Result<Option<Self::Body>>;

    /// Collect response headers from `response`.
    fn response_headers(&self, response: &Self::Response) -> Result<Vec<(String, String)>>;

    /// Acquire a stream reader for `body`.
    fn body_reader(&self, body: &Self::Body) -> Result<Self::Reader>;

    /// Read the next non-empty body chunk, or `None` once the stream is done.
    async fn read_chunk(&self, reader: &Self::Reader) -> Result<Option<Vec<u8>>>;
}

/// Drive an in-flight HTTPS fetch with `cancel_target` armed across response wait, status/header
/// validation, content-length and byte-budget admission, body streaming, and request timeout.
#[cfg(any(test, rings_browser))]
pub(super) async fn drive_cancellable_https_fetch<F>(
    mut fetch: F,
    cancel_target: F::Cancel,
    max_body_bytes: u64,
    timeout_duration: std::time::Duration,
    record_bytes: impl Fn(u64) -> Result<()>,
) -> Result<OnionHttpsResponse>
where
    F: CancellableHttpsFetch,
{
    use futures::future::Either;
    use futures::FutureExt;

    let mut cancel_guard = FetchCancellationGuard::new(cancel_target);
    let fetch_task = async {
        let response = fetch.await_response().await?;
        let body = fetch.response_body(&response)?;
        if let Some(body) = &body {
            cancel_guard.set_body(body.clone());
        }
        let status = checked_status_code(fetch.response_status(&response)?)?;
        let headers = fetch.response_headers(&response)?;
        reject_content_length_over_limit(&headers, max_body_bytes)?;
        record_bytes(headers_bytes(&headers)?)?;
        let collected = match body {
            None => Vec::new(),
            Some(body) => {
                let reader = fetch.body_reader(&body)?;
                cancel_guard.set_reader(reader.clone());
                let mut collected = Vec::new();
                while let Some(bytes) = fetch.read_chunk(&reader).await? {
                    admit_body_chunk(collected.len(), bytes.len(), max_body_bytes, &record_bytes)?;
                    collected.extend_from_slice(bytes.as_slice());
                }
                collected
            }
        };
        cancel_guard.disarm();
        Ok::<OnionHttpsResponse, Error>(OnionHttpsResponse {
            status,
            headers,
            body: collected,
        })
    };
    let timeout = rings_runtime::sleep(timeout_duration).fuse();
    futures::pin_mut!(fetch_task, timeout);
    match futures::future::select(fetch_task, timeout).await {
        Either::Left((result, _)) => result,
        Either::Right((_, _)) => Err(Error::HttpRequestError(
            "browser HTTPS proxy request timed out".to_string(),
        )),
    }
}
