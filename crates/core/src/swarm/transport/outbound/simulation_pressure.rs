//! Outbound pressure for tests: the capacity and credit toward a peer held as transfers stalled
//! behind its withheld credit hold them, and the class-reservation probe of the sync-storm
//! simulation.

use futures::FutureExt;
use rings_transport::core::pool::ChannelLane;
use rings_transport::core::transport::LaneCreditReservation;

use super::TransferClass;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

impl crate::swarm::transport::SwarmTransport {
    /// How many transfers toward `peer` hold outbound admission, if it has a scheduler.
    pub(crate) fn outbound_admitted_transfer_count_for_test(&self, peer: Did) -> Option<usize> {
        self.outbound_schedulers
            .admitted_transfer_count_for_test(peer)
    }

    /// Hold every control-class reservation of this end's capacity toward `peer`, as transfers
    /// stalled behind the peer's withheld credit would: the reservations return when dropped.
    pub(crate) fn hold_control_capacity_for_test(&self, peer: Did) -> Result<Vec<impl Sized>> {
        let handle = self.outbound_schedulers.handle(peer)?;
        let mut held = Vec::new();
        while let Ok(permit) = handle.reserve(peer, TransferClass::DhtControl, 1) {
            held.push(permit);
        }
        Ok(held)
    }

    /// Saturate the live scheduler's lower classes toward `peer`, record a violation if
    /// control is then refused, and return the lower classes' overload error.
    pub(crate) fn exercise_class_reservation_pressure_for_simulation(
        &self,
        peer: Did,
    ) -> Result<Error> {
        let handle = self.outbound_schedulers.handle(peer)?;
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

    /// Hold every credit `peer` has granted this end on `lane`, as a peer that withholds the
    /// lane's credit leaves this end: the reservations give it back when dropped.
    pub(crate) fn hold_lane_credit_for_test(
        &self,
        peer: Did,
        lane: ChannelLane,
    ) -> Result<Vec<LaneCreditReservation>> {
        let connection = self
            .admitted_connection(peer)?
            .ok_or(Error::SwarmMissDidInTable(peer))?;
        let mut held = Vec::new();
        while let Some(credit) = connection.reserve_send_credit(lane).now_or_never() {
            held.push(credit?);
        }
        Ok(held)
    }
}
