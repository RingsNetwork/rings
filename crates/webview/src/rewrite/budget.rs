//! Pure output-budget algebra shared by HTML and CSS rewriting.

use crate::error::Result;
use crate::error::TransportFailure;
use crate::error::WebviewError;

/// String accumulator that fails closed once output exceeds a byte budget.
pub(super) struct BoundedString {
    /// Accumulated rewritten UTF-8 output.
    output: String,
    /// Maximum permitted output length in bytes.
    limit: usize,
}

impl BoundedString {
    /// Create a bounded accumulator preallocated up to `capacity.min(limit)`.
    pub(super) fn new(capacity: usize, limit: usize) -> Self {
        Self {
            output: String::with_capacity(capacity.min(limit)),
            limit,
        }
    }

    /// Append `value` if doing so stays within the configured byte budget.
    pub(super) fn push_str(&mut self, value: &str) -> Result<()> {
        let actual = self.output.len().saturating_add(value.len());
        if actual > self.limit {
            return Err(response_body_too_large(actual, self.limit));
        }
        self.output.push_str(value);
        Ok(())
    }

    /// Append a single Unicode scalar value if it fits within the byte budget.
    pub(super) fn push(&mut self, value: char) -> Result<()> {
        let actual = self.output.len().saturating_add(value.len_utf8());
        if actual > self.limit {
            return Err(response_body_too_large(actual, self.limit));
        }
        self.output.push(value);
        Ok(())
    }

    /// Consume the accumulator and return the accumulated string.
    pub(super) fn finish(self) -> String {
        self.output
    }
}

/// Build a typed response-body-too-large error for `actual` bytes exceeding `limit`.
pub(super) fn response_body_too_large(actual: usize, limit: usize) -> WebviewError {
    TransportFailure::ResponseBodyTooLarge { actual, limit }.into()
}

/// Return `value` if its byte length does not exceed `limit`.
pub(super) fn bounded_value(value: String, limit: usize) -> Result<String> {
    if value.len() > limit {
        Err(response_body_too_large(value.len(), limit))
    } else {
        Ok(value)
    }
}
