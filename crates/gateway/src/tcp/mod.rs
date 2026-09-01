//! Shared IPv4/TCP termination used by every native packet binding.

mod device;

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;

use smoltcp::iface::Config as InterfaceConfig;
use smoltcp::iface::Interface;
use smoltcp::iface::SocketHandle;
use smoltcp::iface::SocketSet;
use smoltcp::socket::tcp;
use smoltcp::time::Duration as SmolDuration;
use smoltcp::time::Instant;
use smoltcp::wire::HardwareAddress;
use smoltcp::wire::IpAddress;
use smoltcp::wire::IpCidr;

use self::device::PacketQueueDevice;
use crate::FlowId;
use crate::FlowRejectReason;
use crate::GatewayConfig;
use crate::GatewayError;
use crate::TcpSegment;
use crate::TcpStackError;

/// Result of attempting to allocate one TCP endpoint for a captured flow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcpFlowAdmission {
    /// The endpoint exists and may consume the captured initial SYN.
    Accepted,
    /// This flow was refused without affecting the gateway or other flows.
    Rejected(FlowRejectReason),
}

struct TcpEndpoint {
    handle: SocketHandle,
    pending_deadline: Option<Duration>,
}

/// Stable projection of the internal TCP endpoint state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcpEndpointState {
    /// The endpoint no longer participates in a connection.
    Closed,
    /// The endpoint is waiting for the captured initial SYN.
    Listening,
    /// The TCP three-way handshake is in progress.
    Handshaking,
    /// Both TCP directions are established.
    Established,
    /// At least one TCP close transition is in progress.
    Closing,
}

impl From<tcp::State> for TcpEndpointState {
    fn from(state: tcp::State) -> Self {
        match state {
            tcp::State::Closed => Self::Closed,
            tcp::State::Listen => Self::Listening,
            tcp::State::SynSent | tcp::State::SynReceived => Self::Handshaking,
            tcp::State::Established => Self::Established,
            tcp::State::FinWait1
            | tcp::State::FinWait2
            | tcp::State::CloseWait
            | tcp::State::Closing
            | tcp::State::LastAck
            | tcp::State::TimeWait => Self::Closing,
        }
    }
}

impl TcpEndpointState {
    /// Return the runtime-owned exit clock for states not timed out by smoltcp itself.
    ///
    /// Established and closing sockets delegate liveness to smoltcp's configured idle,
    /// retransmission, and TIME-WAIT timers. Closed sockets are terminal and are released during
    /// reconciliation. Listening and handshaking have no such socket-owned guarantee, so the
    /// runtime enforces the configured flow timeout from endpoint admission.
    pub const fn exit_clock(self, flow_timeout: Duration) -> Option<Duration> {
        match self {
            Self::Listening | Self::Handshaking => Some(flow_timeout),
            Self::Closed | Self::Established | Self::Closing => None,
        }
    }
}

/// Platform-neutral IPv4/TCP endpoint engine for captured TUN packets.
///
/// The stack never opens an operating-system TCP socket. It consumes complete IPv4 packets and
/// emits complete IPv4 packets for the platform binding to inject through [`crate::PacketIo`].
/// Direct callers own two cross-model invariants: each endpoint must mirror one admitted
/// [`crate::FlowTable`] entry, and each packet passed to [`Self::ingest_segment`] or
/// [`Self::reject_segment`] must have passed [`crate::classify_ipv4_packet`].
pub struct TcpStack {
    interface: Interface,
    device: PacketQueueDevice,
    sockets: SocketSet<'static>,
    endpoints: HashMap<FlowId, TcpEndpoint>,
    owned_handles: HashSet<SocketHandle>,
    tcp_buffer_bytes: usize,
    flow_idle_timeout: Duration,
}

impl TcpStack {
    /// Create a shared TCP stack from validated gateway configuration.
    pub fn new(config: &GatewayConfig, random_seed: u64) -> Result<Self, GatewayError> {
        config.validate()?;
        let (interface_address, prefix_len) = config.plan.first_ipv4_address()?;
        let mut device = PacketQueueDevice::new(usize::from(config.plan.mtu.get()));
        let mut interface_config = InterfaceConfig::new(HardwareAddress::Ip);
        interface_config.random_seed = random_seed;
        let mut interface = Interface::new(interface_config, &mut device, Instant::ZERO);

        let address = IpAddress::Ipv4(interface_address);
        let cidr = IpCidr::new(address, prefix_len);
        let mut address_inserted = false;
        interface.update_ip_addrs(|addresses| {
            address_inserted = addresses.push(cidr).is_ok();
        });
        if !address_inserted {
            return Err(TcpStackError::InterfaceAddressTableFull.into());
        }
        interface.set_any_ip(true);
        // This is smoltcp's private reply-path route for packets already admitted from the TUN.
        // It does not install or widen an operating-system capture route.
        interface
            .routes_mut()
            .add_default_ipv4_route(interface_address)
            .map_err(|_| TcpStackError::RouteTableFull)?;

        Ok(Self {
            interface,
            device,
            sockets: SocketSet::new(Vec::new()),
            endpoints: HashMap::with_capacity(config.max_flows),
            owned_handles: HashSet::with_capacity(config.max_flows),
            tcp_buffer_bytes: config.tcp_buffer_bytes,
            flow_idle_timeout: config.flow_idle_timeout,
        })
    }

    /// Allocate a listening endpoint for one initial SYN.
    ///
    /// Flow capacity is owned only by [`crate::FlowTable`]. This stack mirrors already-admitted
    /// endpoints but does not maintain a second capacity counter.
    pub fn admit_flow(&mut self, segment: TcpSegment, elapsed: Duration) -> TcpFlowAdmission {
        if self.endpoints.contains_key(&segment.flow) {
            return TcpFlowAdmission::Accepted;
        }
        if !segment.opens_flow() {
            return TcpFlowAdmission::Rejected(FlowRejectReason::MissingInitialSyn);
        }
        let SocketAddr::V4(target) = segment.flow.target else {
            return TcpFlowAdmission::Rejected(FlowRejectReason::ListenRejected);
        };

        let receive = tcp::SocketBuffer::new(vec![0_u8; self.tcp_buffer_bytes]);
        let transmit = tcp::SocketBuffer::new(vec![0_u8; self.tcp_buffer_bytes]);
        let mut socket = tcp::Socket::new(receive, transmit);
        socket.set_timeout(Some(SmolDuration::from(self.flow_idle_timeout)));
        if socket.listen(target).is_err() {
            return TcpFlowAdmission::Rejected(FlowRejectReason::ListenRejected);
        }
        let handle = self.sockets.add(socket);
        self.owned_handles.insert(handle);
        self.endpoints.insert(segment.flow, TcpEndpoint {
            handle,
            pending_deadline: TcpEndpointState::Listening
                .exit_clock(self.flow_idle_timeout)
                .map(|timeout| elapsed.saturating_add(timeout)),
        });
        TcpFlowAdmission::Accepted
    }

    /// Consume one already-classified TCP segment for an admitted flow.
    ///
    /// `packet` must be the same validated packet from which `segment` was classified. The queue
    /// delegates TCP checksum verification to that classifier and performs only TX checksums.
    pub fn ingest_segment(
        &mut self,
        packet: Vec<u8>,
        segment: TcpSegment,
        elapsed: Duration,
    ) -> Result<(), TcpStackError> {
        if !self.endpoints.contains_key(&segment.flow) {
            return Err(TcpStackError::UnknownFlow(segment.flow));
        }
        self.device.enqueue_ingress(packet);
        self.poll(elapsed);
        Ok(())
    }

    /// Feed a classified, refused segment to smoltcp without a listener so it can emit a TCP reset.
    pub fn reject_segment(&mut self, packet: Vec<u8>, elapsed: Duration) {
        self.device.enqueue_ingress(packet);
        self.poll(elapsed);
    }

    /// Advance TCP timers and process all currently queued ingress.
    pub fn poll(&mut self, elapsed: Duration) {
        let timestamp = timestamp(elapsed);
        let _ = self
            .interface
            .poll(timestamp, &mut self.device, &mut self.sockets);
    }

    /// Take all complete IPv4 packets waiting for platform injection.
    pub fn take_egress(&mut self) -> Vec<Vec<u8>> {
        self.device.drain_egress()
    }

    /// Return the current endpoint state for a tracked flow.
    pub fn endpoint_state(&self, flow: FlowId) -> Result<TcpEndpointState, TcpStackError> {
        self.socket(flow)
            .map(|socket| TcpEndpointState::from(socket.state()))
    }

    #[cfg(test)]
    fn read_application_data(
        &mut self,
        flow: FlowId,
        output: &mut [u8],
    ) -> Result<usize, TcpStackError> {
        match self.socket_mut(flow)?.recv_slice(output) {
            Ok(length) => Ok(length),
            Err(tcp::RecvError::Finished) => Ok(0),
            Err(tcp::RecvError::InvalidState) => Err(TcpStackError::ReceiveUnavailable(flow)),
        }
    }

    /// Copy, without consuming, application bytes waiting from the captured client.
    pub fn peek_application_data(
        &mut self,
        flow: FlowId,
        output: &mut [u8],
    ) -> Result<usize, TcpStackError> {
        match self.socket_mut(flow)?.peek_slice(output) {
            Ok(length) => Ok(length),
            Err(tcp::RecvError::Finished) => Ok(0),
            Err(tcp::RecvError::InvalidState) => Err(TcpStackError::ReceiveUnavailable(flow)),
        }
    }

    /// Consume exactly the prefix previously returned by [`Self::peek_application_data`].
    pub fn commit_application_read(
        &mut self,
        flow: FlowId,
        length: usize,
    ) -> Result<(), TcpStackError> {
        let actual = match self
            .socket_mut(flow)?
            .recv(|available| (length.min(available.len()), length.min(available.len())))
        {
            Ok(actual) => actual,
            Err(tcp::RecvError::Finished) => 0,
            Err(tcp::RecvError::InvalidState) => {
                return Err(TcpStackError::ReceiveUnavailable(flow));
            }
        };
        if actual != length {
            return Err(TcpStackError::ReceiveCommitMismatch {
                flow,
                expected: length,
                actual,
            });
        }
        Ok(())
    }

    /// Queue application bytes received from the Onion stream for delivery to the client.
    pub fn write_application_data(
        &mut self,
        flow: FlowId,
        input: &[u8],
    ) -> Result<usize, TcpStackError> {
        self.socket_mut(flow)?
            .send_slice(input)
            .map_err(|tcp::SendError::InvalidState| TcpStackError::SendUnavailable(flow))
    }

    /// Gracefully close the application-to-client direction for a tracked flow.
    pub fn close_application_write(&mut self, flow: FlowId) -> Result<(), TcpStackError> {
        self.socket_mut(flow)?.close();
        Ok(())
    }

    /// Return whether the application may still receive bytes from the captured client.
    pub fn client_read_open(&self, flow: FlowId) -> Result<bool, TcpStackError> {
        self.socket(flow).map(tcp::Socket::may_recv)
    }

    /// Return the number of TCP endpoints currently owned by the stack.
    pub fn flow_count(&self) -> usize {
        self.endpoints.len()
    }

    /// Return pending endpoints whose runtime-owned exit clock has elapsed.
    pub fn expired_pending_flows(
        &mut self,
        elapsed: Duration,
    ) -> Result<Vec<FlowId>, TcpStackError> {
        let due = self
            .endpoints
            .iter()
            .filter_map(|(flow, endpoint)| {
                endpoint
                    .pending_deadline
                    .filter(|deadline| elapsed >= *deadline)
                    .map(|_| *flow)
            })
            .collect::<Vec<_>>();
        let mut expired = Vec::new();
        for flow in due {
            let state = self.endpoint_state(flow)?;
            if state.exit_clock(self.flow_idle_timeout).is_some() {
                expired.push(flow);
            } else if let Some(endpoint) = self.endpoints.get_mut(&flow) {
                endpoint.pending_deadline = None;
            }
        }
        Ok(expired)
    }

    /// Abort a flow, emit its terminal TCP packet, and release its endpoint capacity.
    pub fn abort_flow(&mut self, flow: FlowId, elapsed: Duration) -> Result<(), TcpStackError> {
        self.socket_mut(flow)?.abort();
        self.poll(elapsed);
        self.release_socket(flow)
    }

    /// Release a closed flow endpoint after its terminal packets have been injected.
    pub fn release_closed_flow(&mut self, flow: FlowId) -> Result<bool, TcpStackError> {
        let state = self.endpoint_state(flow)?;
        if state != TcpEndpointState::Closed {
            return Ok(false);
        }
        self.release_socket(flow)?;
        Ok(true)
    }

    fn socket(&self, flow: FlowId) -> Result<&tcp::Socket<'static>, TcpStackError> {
        let handle = self
            .endpoints
            .get(&flow)
            .map(|endpoint| endpoint.handle)
            .ok_or(TcpStackError::UnknownFlow(flow))?;
        if !self.owned_handles.contains(&handle) {
            return Err(TcpStackError::UnknownFlow(flow));
        }
        // Ownership invariant: every handle is inserted into `SocketSet`, `endpoints`, and
        // `owned_handles` together, and `release_socket` removes it from all three together. This
        // crate inserts only TCP sockets. The O(1) membership proof therefore satisfies both
        // preconditions of smoltcp's O(1) typed accessor.
        Ok(self.sockets.get::<tcp::Socket<'static>>(handle))
    }

    fn socket_mut(&mut self, flow: FlowId) -> Result<&mut tcp::Socket<'static>, TcpStackError> {
        let handle = self
            .endpoints
            .get(&flow)
            .map(|endpoint| endpoint.handle)
            .ok_or(TcpStackError::UnknownFlow(flow))?;
        if !self.owned_handles.contains(&handle) {
            return Err(TcpStackError::UnknownFlow(flow));
        }
        // See `socket`: the three owned indexes share one private insertion/removal boundary.
        Ok(self.sockets.get_mut::<tcp::Socket<'static>>(handle))
    }

    fn release_socket(&mut self, flow: FlowId) -> Result<(), TcpStackError> {
        let handle = self
            .endpoints
            .get(&flow)
            .map(|endpoint| endpoint.handle)
            .ok_or(TcpStackError::UnknownFlow(flow))?;
        if !self.owned_handles.contains(&handle) {
            return Err(TcpStackError::UnknownFlow(flow));
        }
        // Membership proves that this handle was returned by this `SocketSet` and has not been
        // removed. Remove the smoltcp entry before dropping the two ownership witnesses.
        let _ = self.sockets.remove(handle);
        self.owned_handles.remove(&handle);
        self.endpoints.remove(&flow);
        Ok(())
    }
}

fn timestamp(elapsed: Duration) -> Instant {
    let micros = elapsed.as_micros().min(i64::MAX as u128) as i64;
    Instant::from_micros(micros)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use ipnet::IpNet;
    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::phy::Device as _;
    use smoltcp::phy::Medium;
    use smoltcp::wire::IpProtocol;
    use smoltcp::wire::Ipv4Packet;
    use smoltcp::wire::Ipv4Repr;
    use smoltcp::wire::TcpControl;
    use smoltcp::wire::TcpPacket;
    use smoltcp::wire::TcpRepr;
    use smoltcp::wire::TcpSeqNumber;

    use super::*;
    use crate::classify_ipv4_packet;
    use crate::GatewayPlan;
    use crate::Mtu;
    use crate::PacketDisposition;
    use crate::PacketDropReason;

    fn route(address: Ipv4Addr, prefix: u8) -> IpNet {
        IpNet::new(address.into(), prefix).expect("valid test network")
    }

    fn config() -> GatewayConfig {
        GatewayConfig {
            plan: GatewayPlan {
                addresses: vec![route(Ipv4Addr::new(100, 64, 0, 1), 32)],
                included_routes: vec![route(Ipv4Addr::new(198, 18, 0, 0), 15)],
                mtu: Mtu::try_from(1_280).expect("valid test MTU"),
            },
            max_flows: 8,
            flow_idle_timeout: Duration::from_secs(30),
            tcp_buffer_bytes: 16 * 1_024,
        }
    }

    fn tcp_packet(control: TcpControl, acknowledgment: Option<TcpSeqNumber>) -> Vec<u8> {
        tcp_packet_with_payload(control, TcpSeqNumber(7), acknowledgment, &[])
    }

    fn tcp_packet_with_payload(
        control: TcpControl,
        sequence: TcpSeqNumber,
        acknowledgment: Option<TcpSeqNumber>,
        payload: &[u8],
    ) -> Vec<u8> {
        let source = Ipv4Addr::new(100, 64, 0, 2);
        let target = Ipv4Addr::new(93, 184, 216, 34);
        let tcp = TcpRepr {
            src_port: 41_000,
            dst_port: 443,
            control,
            seq_number: sequence,
            ack_number: acknowledgment,
            window_len: 8_192,
            window_scale: None,
            max_seg_size: Some(1_200),
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload,
        };
        let ipv4 = Ipv4Repr {
            src_addr: source,
            dst_addr: target,
            next_header: IpProtocol::Tcp,
            payload_len: tcp.buffer_len(),
            hop_limit: 64,
        };
        let mut packet = vec![0_u8; ipv4.buffer_len() + tcp.buffer_len()];
        ipv4.emit(
            &mut Ipv4Packet::new_unchecked(&mut packet),
            &ChecksumCapabilities::default(),
        );
        tcp.emit(
            &mut TcpPacket::new_unchecked(&mut packet[ipv4.buffer_len()..]),
            &source.into(),
            &target.into(),
            &ChecksumCapabilities::default(),
        );
        packet
    }

    fn ingest_tcp(stack: &mut TcpStack, packet: Vec<u8>, elapsed: Duration) -> TcpSegment {
        let segment = match classify_ipv4_packet(&packet) {
            PacketDisposition::Tcp(segment) => segment,
            PacketDisposition::Drop(reason) => panic!("test TCP packet was dropped: {reason:?}"),
        };
        if !stack.endpoints.contains_key(&segment.flow) {
            assert_eq!(
                stack.admit_flow(segment, elapsed),
                TcpFlowAdmission::Accepted
            );
        }
        stack
            .ingest_segment(packet, segment, elapsed)
            .expect("test TCP packet is admitted");
        segment
    }

    fn establish(stack: &mut TcpStack) -> (FlowId, TcpSeqNumber) {
        let segment = ingest_tcp(
            stack,
            tcp_packet(TcpControl::Syn, None),
            Duration::from_millis(1),
        );
        let syn_ack = stack
            .take_egress()
            .into_iter()
            .next()
            .expect("SYN-ACK packet");
        let ipv4 = Ipv4Packet::new_checked(syn_ack.as_slice()).expect("valid SYN-ACK IPv4");
        let tcp = TcpPacket::new_checked(ipv4.payload()).expect("valid SYN-ACK TCP");
        let server_sequence = tcp.seq_number();
        ingest_tcp(
            stack,
            tcp_packet_with_payload(
                TcpControl::None,
                TcpSeqNumber(8),
                Some(server_sequence + 1),
                &[],
            ),
            Duration::from_millis(2),
        );
        assert_eq!(
            stack.endpoint_state(segment.flow),
            Ok(TcpEndpointState::Established)
        );
        (segment.flow, server_sequence)
    }

    #[test]
    fn initial_syn_creates_one_endpoint_and_emits_syn_ack() {
        let mut stack = TcpStack::new(&config(), 7).expect("valid TCP stack");
        let segment = ingest_tcp(
            &mut stack,
            tcp_packet(TcpControl::Syn, None),
            Duration::from_millis(1),
        );

        assert_eq!(stack.flow_count(), 1);
        assert_eq!(
            stack.endpoint_state(segment.flow),
            Ok(TcpEndpointState::Handshaking)
        );
        let egress = stack.take_egress();
        assert_eq!(egress.len(), 1);
        assert!(matches!(
            classify_ipv4_packet(&egress[0]),
            PacketDisposition::Tcp(TcpSegment {
                syn: true,
                ack: true,
                ..
            })
        ));
    }

    #[test]
    fn unknown_flow_must_begin_with_initial_syn() {
        let mut stack = TcpStack::new(&config(), 7).expect("valid TCP stack");
        let packet = tcp_packet(TcpControl::None, Some(TcpSeqNumber(8)));
        let segment = match classify_ipv4_packet(&packet) {
            PacketDisposition::Tcp(segment) => segment,
            _ => panic!("ACK must be TCP"),
        };
        assert_eq!(
            stack.admit_flow(segment, Duration::from_millis(1)),
            TcpFlowAdmission::Rejected(FlowRejectReason::MissingInitialSyn)
        );
        assert_eq!(stack.flow_count(), 0);
        stack.reject_segment(packet, Duration::from_millis(1));
        assert!(stack.take_egress().iter().any(|packet| matches!(
            classify_ipv4_packet(packet),
            PacketDisposition::Tcp(TcpSegment { flow: reply, rst: true, .. })
                if reply.source == segment.flow.target && reply.target == segment.flow.source
        )));
    }

    #[test]
    fn captured_udp_is_not_fed_into_the_tcp_stack() {
        let mut packet = tcp_packet(TcpControl::Syn, None);
        packet[9] = 17;
        assert_eq!(
            classify_ipv4_packet(&packet),
            PacketDisposition::Drop(PacketDropReason::Udp)
        );
    }

    #[test]
    fn empty_tcp_buffer_is_rejected_before_stack_construction() {
        let mut candidate = config();
        candidate.tcp_buffer_bytes = 0;
        assert!(matches!(
            TcpStack::new(&candidate, 7),
            Err(GatewayError::Config(crate::ConfigError::ZeroTcpBuffer))
        ));
    }

    #[test]
    fn every_pending_tcp_state_has_a_runtime_exit_clock() {
        let timeout = Duration::from_secs(30);
        assert_eq!(
            TcpEndpointState::Listening.exit_clock(timeout),
            Some(timeout)
        );
        assert_eq!(
            TcpEndpointState::Handshaking.exit_clock(timeout),
            Some(timeout)
        );
        assert_eq!(TcpEndpointState::Established.exit_clock(timeout), None);
        assert_eq!(TcpEndpointState::Closing.exit_clock(timeout), None);
        assert_eq!(TcpEndpointState::Closed.exit_clock(timeout), None);
    }

    #[test]
    fn stack_does_not_duplicate_flow_table_capacity_accounting() {
        let mut candidate = config();
        candidate.max_flows = 1;
        let mut stack = TcpStack::new(&candidate, 7).expect("valid TCP stack");
        let first = ingest_tcp(
            &mut stack,
            tcp_packet(TcpControl::Syn, None),
            Duration::from_millis(1),
        );

        let mut second_packet = tcp_packet(TcpControl::Syn, None);
        second_packet[0x14..0x16].copy_from_slice(&41_001_u16.to_be_bytes());
        TcpPacket::new_unchecked(&mut second_packet[20..]).fill_checksum(
            &Ipv4Addr::new(100, 64, 0, 2).into(),
            &Ipv4Addr::new(93, 184, 216, 34).into(),
        );
        let second = match classify_ipv4_packet(&second_packet) {
            PacketDisposition::Tcp(segment) => segment.flow,
            _ => panic!("second SYN must be TCP"),
        };
        ingest_tcp(&mut stack, second_packet, Duration::from_millis(2));
        assert_eq!(stack.flow_count(), 2);
        assert!(stack.endpoint_state(first.flow).is_ok());
        assert!(stack.endpoint_state(second).is_ok());
    }

    #[test]
    fn queue_device_uses_ip_medium_for_tun_packets() {
        let stack = TcpStack::new(&config(), 7).expect("valid TCP stack");
        let capabilities = stack.device.capabilities();
        assert_eq!(capabilities.medium, Medium::Ip);
        assert_eq!(capabilities.max_transmission_unit, 1_280);
    }

    #[test]
    fn out_of_order_and_retransmitted_payload_is_delivered_exactly_once() {
        let mut stack = TcpStack::new(&config(), 7).expect("valid TCP stack");
        let (flow, server_sequence) = establish(&mut stack);
        let acknowledgment = Some(server_sequence + 1);

        ingest_tcp(
            &mut stack,
            tcp_packet_with_payload(TcpControl::None, TcpSeqNumber(12), acknowledgment, b"efgh"),
            Duration::from_millis(3),
        );
        let mut received = [0_u8; 16];
        assert_eq!(
            stack
                .read_application_data(flow, &mut received)
                .expect("read before gap closes"),
            0
        );

        let prefix =
            tcp_packet_with_payload(TcpControl::None, TcpSeqNumber(8), acknowledgment, b"abcd");
        ingest_tcp(&mut stack, prefix.clone(), Duration::from_millis(4));
        let length = stack
            .read_application_data(flow, &mut received)
            .expect("read reassembled payload");
        assert_eq!(received.get(..length), Some(b"abcdefgh".as_slice()));

        ingest_tcp(&mut stack, prefix, Duration::from_millis(5));
        assert_eq!(
            stack
                .read_application_data(flow, &mut received)
                .expect("read after retransmission"),
            0,
            "a retransmission must not duplicate application bytes"
        );
    }

    #[test]
    fn idle_timeout_closes_and_releases_endpoint_capacity() {
        let mut candidate = config();
        candidate.flow_idle_timeout = Duration::from_secs(1);
        let mut stack = TcpStack::new(&candidate, 7).expect("valid TCP stack");
        let (flow, _) = establish(&mut stack);

        stack.poll(Duration::from_secs(2));
        assert_eq!(stack.endpoint_state(flow), Ok(TcpEndpointState::Closed));
        assert_eq!(stack.release_closed_flow(flow), Ok(true));
        assert_eq!(stack.flow_count(), 0);
    }
}
