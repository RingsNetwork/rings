//! In-memory packet queue adapting a platform TUN to `smoltcp::phy::Device`.

use std::collections::VecDeque;

use smoltcp::phy::Device;
use smoltcp::phy::DeviceCapabilities;
use smoltcp::phy::Medium;
use smoltcp::phy::RxToken;
use smoltcp::phy::TxToken;
use smoltcp::time::Instant;

pub(super) struct PacketQueueDevice {
    ingress: VecDeque<Vec<u8>>,
    egress: VecDeque<Vec<u8>>,
    mtu: usize,
}

impl PacketQueueDevice {
    pub(super) fn new(mtu: usize) -> Self {
        Self {
            ingress: VecDeque::new(),
            egress: VecDeque::new(),
            mtu,
        }
    }

    pub(super) fn enqueue_ingress(&mut self, packet: Vec<u8>) {
        self.ingress.push_back(packet);
    }

    pub(super) fn drain_egress(&mut self) -> Vec<Vec<u8>> {
        self.egress.drain(..).collect()
    }
}

pub(super) struct QueueRxToken {
    packet: Vec<u8>,
}

impl RxToken for QueueRxToken {
    fn consume<R, F>(self, consume: F) -> R
    where F: FnOnce(&[u8]) -> R {
        consume(&self.packet)
    }
}

pub(super) struct QueueTxToken<'a> {
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
}
