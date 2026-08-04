use super::*;

pub(super) fn https_response_body_limit(remaining_policy_bytes: Option<u64>) -> u64 {
    remaining_policy_bytes.map_or(DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES, |remaining| {
        remaining.min(DEFAULT_HTTPS_RESPONSE_BODY_LIMIT_BYTES)
    })
}

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

pub(super) fn usize_to_u64(value: usize) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::InvalidData)
}

pub(super) fn reject_content_length_over_limit(
    headers: &[(String, String)],
    max_body_bytes: u64,
) -> Result<()> {
    if max_body_bytes == 0 {
        return Ok(());
    }
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
