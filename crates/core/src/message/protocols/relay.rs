#![warn(missing_docs)]

use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

/// Policy used when sending a report for a request payload.
///
/// The default preserves the legacy path-return behavior. Routed returns are
/// opt-in and send the report as a fresh Chord-routed payload to the declared
/// destination.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReportReturnPolicy {
    /// Return the report through the reversed relay path.
    #[default]
    Path,
    /// Route the report normally through Chord to this destination.
    Routed {
        /// DID that should receive the report.
        destination: Did,
    },
}

impl ReportReturnPolicy {
    /// Validate that this policy is authorized by the signed request origin.
    pub fn validate_authorized_by(&self, signer: Did) -> Result<()> {
        match self {
            Self::Path => Ok(()),
            Self::Routed { destination } if *destination == signer => Ok(()),
            Self::Routed { destination } => Err(Error::InvalidMessage(format!(
                "routed report return destination {destination} is not signed by that destination"
            ))),
        }
    }
}

/// MessageRelay guide message passing on rings network by relay.
///
/// All messages should be sent with `MessageRelay`.
/// By calling `relay` method in correct place, `MessageRelay` help to do things:
/// - Record the whole transport path for inspection.
/// - Get the sender of a message.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct MessageRelay {
    /// A push only stack. Record routes when handling messages.
    pub path: Vec<Did>,

    /// The next node to handle the message.
    /// A message handler will pick transport by this field.
    pub next_hop: Did,

    /// The destination of the message.
    /// It may help the handler to find out `next_hop` in some situations.
    pub destination: Did,
}

impl MessageRelay {
    /// Create a new `MessageRelay`.
    pub fn new(path: Vec<Did>, next_hop: Did, destination: Did) -> Self {
        Self {
            path,
            next_hop,
            destination,
        }
    }

    /// Validate relay, then create a new `MessageRelay` that have `current` did in the end of path.
    /// The new relay will use `next_hop` as `next_hop` and `self.destination` as `destination`.
    pub fn forward(&self, current: Did, next_hop: Did) -> Result<Self> {
        self.validate(current)?;

        if self.next_hop != current {
            return Err(Error::InvalidNextHop);
        }

        let mut path = self.path.clone();
        path.push(current);

        Ok(Self {
            path,
            next_hop,
            destination: self.destination,
        })
    }

    /// Validate relay, then create a new `MessageRelay` that used to report the message.
    /// The new relay will use `self.path[self.path.len() - 1]` as `next_hop` and `self.sender()` as `destination`.
    /// In the new relay, the path will be cleared and only have `current` did.
    pub fn path_report(&self, current: Did) -> Result<Self> {
        self.validate(current)?;

        if self.path.is_empty() {
            return Err(Error::CannotInferNextHop);
        }

        Ok(Self {
            path: vec![current],
            next_hop: self.path.last().copied().ok_or(Error::CannotInferNextHop)?,
            destination: self.try_origin_sender()?,
        })
    }

    /// Validate relay, then create a fresh Chord-routed report relay.
    ///
    /// The caller must infer `next_hop` from the destination before invoking
    /// this constructor.
    pub fn routed_report(&self, current: Did, destination: Did, next_hop: Did) -> Result<Self> {
        self.validate(current)?;

        Ok(Self {
            path: vec![current],
            next_hop,
            destination,
        })
    }

    /// Create a report relay with an explicit return policy.
    pub fn report(
        &self,
        current: Did,
        policy: ReportReturnPolicy,
        routed_next_hop: Option<Did>,
    ) -> Result<Self> {
        match policy {
            ReportReturnPolicy::Path => self.path_report(current),
            ReportReturnPolicy::Routed { destination } => self.routed_report(
                current,
                destination,
                routed_next_hop.ok_or(Error::CannotInferNextHop)?,
            ),
        }
    }

    /// Sometime the sender may not know the destination of the message. They just use next_hop as destination.
    /// The next node can find a new next_hop, and may use this function to set that next_hop as destination again.
    pub fn reset_destination(&self, destination: Did) -> Self {
        let mut relay = self.clone();
        relay.destination = destination;
        relay
    }

    /// Check if path and destination is valid.
    pub fn validate(&self, current: Did) -> Result<()> {
        if self.next_hop != current {
            return Err(Error::InvalidNextHop);
        }

        // Adjacent elements in self.path cannot be equal
        if self
            .path
            .windows(2)
            .any(|window| matches!(window, [left, right] if left == right))
        {
            return Err(Error::InvalidRelayPath);
        }

        // Prevent infinite loop
        if has_infinite_loop(&self.path) {
            tracing::error!("Infinite path detected {:?}", self.path);
            return Err(Error::InfiniteRelayPath);
        }

        Ok(())
    }

    /// Get the origin sender of current message.
    /// Should be the first element of path.
    #[deprecated(note = "please use `origin_sender` instead")]
    pub fn sender(&self) -> Did {
        self.origin_sender()
    }

    /// Get the origin sender of current message as a checked relay-path boundary.
    pub fn try_origin_sender(&self) -> Result<Did> {
        self.path.first().copied().ok_or(Error::CannotInferNextHop)
    }

    /// Get the origin sender of current message.
    ///
    /// The origin should be the first element of `path`. Empty relay paths keep
    /// the legacy fallback to `destination`; callers that must distinguish an
    /// invalid relay boundary from a real origin should use
    /// [`try_origin_sender`](Self::try_origin_sender).
    pub fn origin_sender(&self) -> Did {
        self.path.first().copied().unwrap_or(self.destination)
    }
}

// Since rust cannot zip N iterators, when you change this number,
// you should also change the code of `has_infinite_loop` below.
const INFINITE_LOOP_TOLERANCE: usize = 3;

fn has_infinite_loop<T>(path: &[T]) -> bool
where T: PartialEq {
    // Invariant: a relay loop is witnessed by a non-empty suffix period P such
    // that the final path segment is P repeated INFINITE_LOOP_TOLERANCE times.
    for period in 1..=path.len() / INFINITE_LOOP_TOLERANCE {
        let repeated_len = period * INFINITE_LOOP_TOLERANCE;
        let start = path.len() - repeated_len;
        let Some(suffix) = path.get(start..) else {
            continue;
        };
        let mut chunks = suffix.chunks_exact(period);
        let Some(first) = chunks.next() else {
            continue;
        };
        if chunks.all(|chunk| chunk == first) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    #[rustfmt::skip]
    fn test_has_infinite_loop() {
        assert!(!has_infinite_loop(&Vec::<u8>::new()));

        assert!(!has_infinite_loop(&[
            1, 2, 3,
        ]));

        assert!(!has_infinite_loop(&[
            1, 2, 3,
            1, 2, 3,
        ]));

        assert!(has_infinite_loop(&[
            1, 2, 3,
            1, 2, 3,
            1, 2, 3,
        ]));

        assert!(has_infinite_loop(&[
            1, 1, 2, 3,
               1, 2, 3,
               1, 2, 3,
        ]));

        assert!(!has_infinite_loop(&[
               1, 2, 3,
            1, 1, 2, 3,
               1, 2, 3,
        ]));

        assert!(has_infinite_loop(&[
            1, 2, 1, 2, 3,
                  1, 2, 3,
                  1, 2, 3,
        ]));

        assert!(has_infinite_loop(&[
            4, 5, 1, 2, 3,
                  1, 2, 3,
                  1, 2, 3,
        ]));

        assert!(!has_infinite_loop(&[
            1, 2, 3,
                  3,
            1, 2, 3,
                  3,
            1, 2, 3,
        ]));

        assert!(!has_infinite_loop(&[
                  1,
            1, 2, 3,
                  3,
            1, 2, 3,
                  3,
            1, 2, 3,
        ]));

        assert!(has_infinite_loop(&[
                  3,
            1, 2, 3,
                  3,
            1, 2, 3,
                  3,
            1, 2, 3,
        ]));

        assert!(has_infinite_loop(&[
            1, 2, 3,
            1, 2, 3,
                  3,
            1, 2, 3,
                  3,
            1, 2, 3,
        ]));

        assert!(has_infinite_loop(&[
                  1, 2,
               3, 1, 2,
            3, 3, 1, 2,
            3, 3, 1, 2,
            3, 3, 1, 2,
        ]));

        assert!(!has_infinite_loop(&[
               2, 3,
               4, 3,
            1, 2, 3,
               4, 3,
            1, 2, 3,
               4, 3,
        ]));

        assert!(has_infinite_loop(&[
            1, 2, 3,
               4, 3,
            1, 2, 3,
               4, 3,
            1, 2, 3,
               4, 3,
        ]));

        assert!(has_infinite_loop(&[
               1, 2, 3, 4,
            3, 1, 2, 3, 4,
            3, 1, 2, 3, 4,
            3, 1, 2, 3, 4,
        ]));
    }

    #[test]
    fn empty_path_origin_sender_is_checked() {
        let fallback_destination = Did::from(2);
        let relay = MessageRelay::new(vec![], Did::from(1), fallback_destination);

        assert!(matches!(
            relay.try_origin_sender(),
            Err(Error::CannotInferNextHop)
        ));
        assert_eq!(relay.origin_sender(), fallback_destination);
    }

    #[test]
    fn path_report_preserves_legacy_reverse_path() -> Result<()> {
        let origin = Did::from(1);
        let hop = Did::from(2);
        let current = Did::from(3);
        let relay = MessageRelay::new(vec![origin, hop], current, current);

        let report = relay.report(current, ReportReturnPolicy::Path, None)?;

        assert_eq!(report.path, vec![current]);
        assert_eq!(report.next_hop, hop);
        assert_eq!(report.destination, origin);
        Ok(())
    }

    #[test]
    fn routed_report_uses_declared_destination_and_next_hop() -> Result<()> {
        let origin = Did::from(1);
        let current = Did::from(2);
        let return_destination = Did::from(3);
        let routed_next_hop = Did::from(4);
        let relay = MessageRelay::new(vec![origin], current, current);

        let report = relay.report(
            current,
            ReportReturnPolicy::Routed {
                destination: return_destination,
            },
            Some(routed_next_hop),
        )?;

        assert_eq!(report.path, vec![current]);
        assert_eq!(report.next_hop, routed_next_hop);
        assert_eq!(report.destination, return_destination);
        Ok(())
    }

    #[test]
    fn routed_report_requires_explicit_next_hop() {
        let origin = Did::from(1);
        let current = Did::from(2);
        let return_destination = Did::from(3);
        let relay = MessageRelay::new(vec![origin], current, current);

        assert!(matches!(
            relay.report(
                current,
                ReportReturnPolicy::Routed {
                    destination: return_destination,
                },
                None,
            ),
            Err(Error::CannotInferNextHop)
        ));
    }

    #[test]
    fn routed_policy_must_be_authorized_by_destination_signer() -> Result<()> {
        let signer = Did::from(1);
        let other = Did::from(2);

        ReportReturnPolicy::Path.validate_authorized_by(signer)?;
        ReportReturnPolicy::Routed {
            destination: signer,
        }
        .validate_authorized_by(signer)?;

        assert!(matches!(
            ReportReturnPolicy::Routed { destination: other }.validate_authorized_by(signer),
            Err(Error::InvalidMessage(_))
        ));
        Ok(())
    }
}
