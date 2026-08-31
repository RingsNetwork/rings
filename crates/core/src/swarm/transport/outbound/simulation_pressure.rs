//! Production scheduler pressure probes for deterministic sync-storm tests.

use super::OutboundSchedulers;
use super::TransferClass;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

impl OutboundSchedulers {
    pub(super) fn exercise_class_reservation_pressure(&self, peer: Did) -> Result<Error> {
        let handle = self.handle(peer)?;
        let mut lower_class_permits = Vec::new();
        let lower_class_overload = loop {
            match handle.reserve(peer, TransferClass::Application, 1) {
                Ok(permit) => lower_class_permits.push(permit),
                Err(error) => break error,
            }
        };
        if handle.reserve(peer, TransferClass::DhtControl, 1).is_err() {
            crate::simulation::record_protection_violation(
                crate::simulation::ProtectionLayer::ClassReservations,
            );
        }
        drop(lower_class_permits);
        Ok(lower_class_overload)
    }
}
