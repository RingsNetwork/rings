//! Pure output-budget algebra shared by HTML and CSS rewriting.

use crate::error::Result;
use crate::error::TransportFailure;
use crate::error::WebviewError;

pub(super) struct BoundedString {
    output: String,
    limit: usize,
}

impl BoundedString {
    pub(super) fn new(capacity: usize, limit: usize) -> Self {
        Self {
            output: String::with_capacity(capacity.min(limit)),
            limit,
        }
    }

    pub(super) fn push_str(&mut self, value: &str) -> Result<()> {
        let actual = self.output.len().saturating_add(value.len());
        if actual > self.limit {
            return Err(response_body_too_large(actual, self.limit));
        }
        self.output.push_str(value);
        Ok(())
    }

    pub(super) fn push(&mut self, value: char) -> Result<()> {
        let actual = self.output.len().saturating_add(value.len_utf8());
        if actual > self.limit {
            return Err(response_body_too_large(actual, self.limit));
        }
        self.output.push(value);
        Ok(())
    }

    pub(super) fn finish(self) -> String {
        self.output
    }
}

pub(super) fn response_body_too_large(actual: usize, limit: usize) -> WebviewError {
    TransportFailure::ResponseBodyTooLarge { actual, limit }.into()
}

pub(super) fn bounded_value(value: String, limit: usize) -> Result<String> {
    if value.len() > limit {
        Err(response_body_too_large(value.len(), limit))
    } else {
        Ok(value)
    }
}
