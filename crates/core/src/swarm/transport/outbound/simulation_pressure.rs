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

    /// Take every control-class reservation this end's capacity toward `peer` admits.
    fn hold_control_capacity(&self, peer: Did) -> Result<Vec<super::TransferCapacityPermit>> {
        let handle = self.handle(peer)?;
        let mut held = Vec::new();
        while let Ok(permit) = handle.reserve(peer, TransferClass::DhtControl, 1) {
            held.push(permit);
        }
        Ok(held)
    }
}

impl crate::swarm::transport::SwarmTransport {
    pub(crate) fn outbound_admitted_transfer_count_for_test(&self, peer: Did) -> Option<usize> {
        self.outbound_schedulers
            .admitted_transfer_count_for_test(peer)
    }

    /// Exercise the live peer scheduler under lower-class saturation and record
    /// whether an actual control reservation is rejected.
    /// Hold every control-class reservation of this end's capacity toward `peer`, as transfers
    /// stalled behind the peer's withheld credit would: the reservations return when dropped.
    pub(crate) fn hold_control_capacity_for_test(&self, peer: Did) -> Result<Vec<impl Sized>> {
        self.outbound_schedulers.hold_control_capacity(peer)
    }

    pub(crate) fn exercise_class_reservation_pressure_for_simulation(
        &self,
        peer: Did,
    ) -> Result<Error> {
        self.outbound_schedulers
            .exercise_class_reservation_pressure(peer)
    }
}
