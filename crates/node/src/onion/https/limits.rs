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
