//! In-memory packet queue adapting a platform TUN to `smoltcp::phy::Device`.

use std::collections::VecDeque;

use smoltcp::phy::Checksum;
use smoltcp::phy::Device;
use smoltcp::phy::DeviceCapabilities;
use smoltcp::phy::Medium;
use smoltcp::phy::RxToken;
use smoltcp::phy::TxToken;
use smoltcp::time::Instant;

/// In-memory IP packet queue implementing `smoltcp::phy::Device`.
pub(super) struct PacketQueueDevice {
    /// Queued inbound IP packets waiting for `smoltcp` to consume.
    ingress: VecDeque<Vec<u8>>,
    /// Outbound IP packets emitted by `smoltcp` waiting to be written to the tunnel.
    egress: VecDeque<Vec<u8>>,
    /// Maximum transmission unit reported to `smoltcp`.
    mtu: usize,
}

impl PacketQueueDevice {
    /// Create an empty packet queue device with the given `mtu`.
    pub(super) fn new(mtu: usize) -> Self {
        Self {
            ingress: VecDeque::new(),
            egress: VecDeque::new(),
            mtu,
        }
    }

    /// Push one inbound IP packet onto the ingress queue.
    pub(super) fn enqueue_ingress(&mut self, packet: Vec<u8>) {
        self.ingress.push_back(packet);
    }

    /// Drain all outbound IP packets emitted by `smoltcp` since the last poll.
    pub(super) fn drain_egress(&mut self) -> Vec<Vec<u8>> {
        self.egress.drain(..).collect()
    }
}

/// Receive token delivering one queued ingress IP packet to `smoltcp`.
pub(super) struct QueueRxToken {
    /// Raw IP packet bytes.
    packet: Vec<u8>,
}

impl RxToken for QueueRxToken {
    fn consume<R, F>(self, consume: F) -> R
    where F: FnOnce(&[u8]) -> R {
        consume(&self.packet)
    }
}

/// Transmit token appending one serialized egress IP packet onto the device queue.
pub(super) struct QueueTxToken<'a> {
    /// Reference to the device's outbound packet queue.
    egress: &'a mut VecDeque<Vec<u8>>,
}

impl TxToken for QueueTxToken<'_> {
    fn consume<R, F>(self, length: usize, consume: F) -> R
    where F: FnOnce(&mut [u8]) -> R {
        let mut packet = vec![0_u8; length];
        let result = consume(&mut packet);
        self.egress.push_back(packet);
        result
    }
}

impl Device for PacketQueueDevice {
    type RxToken<'a> = QueueRxToken;
    type TxToken<'a> = QueueTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let packet = self.ingress.pop_front()?;
        Some((QueueRxToken { packet }, QueueTxToken {
            egress: &mut self.egress,
        }))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(QueueTxToken {
            egress: &mut self.egress,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ip;
        capabilities.max_transmission_unit = self.mtu;
        // The packet classifier has already verified every TCP packet queued for RX. Keep TX
        // generation enabled while avoiding duplicate RX work inside smoltcp. IPv4 remains at its
        // default because the classifier deliberately leaves header-checksum validation to smoltcp.
        capabilities.checksum.tcp = Checksum::Tx;
        capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_device_preserves_ip_packet_boundaries() {
        let mut device = PacketQueueDevice::new(1_280);
        device.enqueue_ingress(vec![1, 2, 3]);
        let (rx, tx) = device
            .receive(Instant::from_millis(0))
            .expect("queued packet");
        assert_eq!(rx.consume(<[u8]>::to_vec), vec![1, 2, 3]);
        tx.consume(3, |packet| packet.copy_from_slice(&[4, 5, 6]));
        assert_eq!(device.drain_egress(), vec![vec![4, 5, 6]]);
    }

    #[test]
    fn queue_device_delegates_only_unverified_checksums_to_smoltcp() {
        let capabilities = PacketQueueDevice::new(1_280).capabilities();

        assert!(!capabilities.checksum.tcp.rx());
        assert!(capabilities.checksum.tcp.tx());
        assert!(capabilities.checksum.ipv4.rx());
        assert!(capabilities.checksum.ipv4.tx());
    }
}
