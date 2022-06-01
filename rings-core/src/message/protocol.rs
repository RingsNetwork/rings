#![warn(missing_docs)]

use super::payload::MessageRelay;
use super::payload::MessageRelayMethod;
use crate::dht::Did;
use crate::err::{Error, Result};

/// MessageSessionRelayProtocol guide message passing on rings network by relay.
pub trait MessageSessionRelayProtocol {
    /// Check current did, update path and its end cursor, then infer next_hop.
    fn relay(&mut self, current: Did, next_hop: Option<Did>) -> Result<()>;

    /// A SEND message can change its destination.
    /// Call with REPORT method will get an error imeediately.
    fn reset_destination(&mut self, destination: Did) -> Result<()>;

    /// Check if path and destination is valid.
    /// It will be automatically called at relay started.
    fn validate(&self) -> Result<()>;

    /// Get sender of current message.
    /// With SEND method, it will be the `origin()` of the message.
    /// With REPORT method, it will be the last element of path.
    fn sender(&self) -> Did;

    /// Get the original sender of current message.
    /// Should always be the first element of path.
    fn origin(&self) -> Did;

    /// Get the previous element of the element pointed by path_end_cursor.
    fn path_prev(&self) -> Option<Did>;
}

impl<T> MessageSessionRelayProtocol for MessageRelay<T> {
    fn relay(&mut self, current: Did, next_hop: Option<Did>) -> Result<()> {
        self.validate()?;

        // If self.next_hop is setted, it should be current
        if self.next_hop.is_some() && self.next_hop.unwrap() != current {
            return Err(Error::InvalidNextHop);
        }

        match self.method {
            MessageRelayMethod::SEND => {
                self.path.push(current);
                self.next_hop = next_hop;
                Ok(())
            }

            MessageRelayMethod::REPORT => {
                // The final hop
                if self.next_hop == Some(self.destination) {
                    self.path_end_cursor = self.path.len() - 1;
                    self.next_hop = None;
                    return Ok(());
                }

                let pos = self
                    .path
                    .iter()
                    .rev()
                    .skip(self.path_end_cursor)
                    .position(|&x| x == current);

                if let (None, None) = (pos, next_hop) {
                    return Err(Error::CannotInferNextHop);
                }

                if let Some(pos) = pos {
                    self.path_end_cursor += pos;
                }

                // `self.path_prev()` should never return None here, because we have handled final hop
                self.next_hop = next_hop.or_else(|| self.path_prev());

                Ok(())
            }
        }
    }

    fn reset_destination(&mut self, destination: Did) -> Result<()> {
        if self.method == MessageRelayMethod::SEND {
            self.destination = destination;
            Ok(())
        } else {
            Err(Error::ResetDestinationNeedSend)
        }
    }

    fn validate(&self) -> Result<()> {
        // Adjacent elements in self.path cannot be equal
        if self.path.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::InvalidRelayPath);
        }

        // The destination of report message should always be the first element of path
        if self.method == MessageRelayMethod::REPORT && self.path[0] != self.destination {
            return Err(Error::InvalidRelayDestination);
        }

        Ok(())
    }

    fn origin(&self) -> Did {
        *self.path.first().unwrap()
    }

    fn sender(&self) -> Did {
        match self.method {
            MessageRelayMethod::SEND => self.origin(),
            MessageRelayMethod::REPORT => *self.path.last().unwrap(),
        }
    }

    fn path_prev(&self) -> Option<Did> {
        self.path
            .get(self.path.len() - 1 - self.path_end_cursor)
            .copied()
    }
}
