//! The outbound queue's model: which recipients of a queued message are still pending, and
//! when to try them next.
//!
//! ```text
//!   attempt n fails temporarily  ↦  retry at t + delay(n)        delay = 1m, 5m, 15m, 1h, 4h, 4h, …
//!   a recipient fails permanently, or is still pending past MAX_AGE  ↦  bounced
//!   a recipient is delivered  ↦  dropped
//!   Invariant: a queued message has at least one pending recipient
//! ```

use serde::Deserialize;
use serde::Serialize;

/// The retry delays after each failed attempt, in seconds; the last repeats.
const RETRY_DELAYS_SECS: [i64; 5] = [60, 300, 900, 3_600, 14_400];
/// How long a recipient may stay pending before it is bounced, in seconds: three days.
pub const MAX_AGE_SECS: i64 = 3 * 86_400;

/// What one delivery attempt to one recipient came to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The recipient's server took the message.
    Delivered,
    /// A temporary failure: try again later.
    Deferred(String),
    /// A permanent failure: give up.
    Failed(String),
}

/// One queued message's delivery state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Pending {
    /// The reverse path.
    pub from: String,
    /// The recipients not yet delivered.
    pub recipients: Vec<String>,
    /// When the message was queued, in seconds since the Unix epoch.
    pub queued_at: i64,
    /// How many attempts were made.
    pub attempts: u32,
    /// When to try next, in seconds since the Unix epoch.
    pub next_attempt_at: i64,
    /// The last temporary failure, for the bounce if it comes to that.
    pub last_error: Option<String>,
}

/// What became of a queued message after an attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settled {
    /// The message still queued, if any recipient is pending.
    pub pending: Option<Pending>,
    /// The recipients given up on, with why.
    pub bounced: Vec<(String, String)>,
}

impl Pending {
    /// A message from `from` to `recipients`, queued at `now` and due at once.
    pub fn new(from: String, recipients: Vec<String>, now: i64) -> Self {
        Self {
            from,
            recipients,
            queued_at: now,
            attempts: 0,
            next_attempt_at: now,
            last_error: None,
        }
    }

    /// Whether the message is due at `now`.
    pub fn is_due(&self, now: i64) -> bool {
        self.next_attempt_at <= now
    }

    /// Settle the attempt made at `now` whose outcome for each recipient is `outcomes` (`module
    /// doc`).
    pub fn settle(mut self, outcomes: Vec<(String, Outcome)>, now: i64) -> Settled {
        let expired = now.saturating_sub(self.queued_at) >= MAX_AGE_SECS;
        let mut still = Vec::new();
        let mut bounced = Vec::new();
        for (recipient, outcome) in outcomes {
            match outcome {
                Outcome::Delivered => {}
                Outcome::Failed(reason) => bounced.push((recipient, reason)),
                Outcome::Deferred(reason) if expired => {
                    bounced.push((recipient, format!("gave up after three days: {reason}")));
                }
                Outcome::Deferred(reason) => {
                    self.last_error = Some(reason);
                    still.push(recipient);
                }
            }
        }
        if still.is_empty() {
            return Settled {
                pending: None,
                bounced,
            };
        }
        let delay = usize::try_from(self.attempts)
            .ok()
            .and_then(|attempt| RETRY_DELAYS_SECS.get(attempt))
            .or(RETRY_DELAYS_SECS.last())
            .copied()
            .unwrap_or(MAX_AGE_SECS);
        self.recipients = still;
        self.attempts = self.attempts.saturating_add(1);
        self.next_attempt_at = now.saturating_add(delay);
        Settled {
            pending: Some(self),
            bounced,
        }
    }
}

#[cfg(test)]
mod test_schedule;
