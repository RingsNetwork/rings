//! Shared gateway event loop joining packet IO, TCP endpoints, and Onion streams.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant as MonotonicInstant;

use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::bridge::spawn_bridge;
use crate::bridge::BridgeCommand;
use crate::bridge::BridgeEvent;
use crate::bridge::BridgeHandle;
use crate::ExitAvailability;
use crate::FlowEvent;
use crate::FlowId;
use crate::FlowRejectReason;
use crate::FlowState;
use crate::FlowTableError;
use crate::GatewayConfig;
use crate::GatewayError;
use crate::GatewayEvent;
use crate::GatewayServer;
use crate::GatewayState;
use crate::GatewayStatus;
use crate::GatewayStatusHandle;
use crate::OnionStreamConnector;
use crate::PacketDisposition;
use crate::PacketIo;
use crate::PacketIoError;
use crate::PacketOutcome;
use crate::TcpEndpointState;
use crate::TcpFlowAdmission;
use crate::TcpStack;
use crate::TcpStackError;

const TCP_POLL_INTERVAL: Duration = Duration::from_millis(10);
const BRIDGE_IO_CHUNK_BYTES: usize = 16 * 1_024;
const EVENTS_PER_FLOW: usize = 3;
const CONTROL_QUEUE_CAPACITY: usize = 8;

enum RuntimeControl {
    ExitAvailability {
        availability: ExitAvailability,
        reason: Option<String>,
    },
}

/// Cloneable control capability for dependencies observed outside the packet loop.
#[derive(Clone)]
pub struct GatewayControlHandle {
    sender: mpsc::Sender<RuntimeControl>,
}

impl GatewayControlHandle {
    /// Update compatible Onion-exit availability without sharing mutable runtime state.
    pub async fn set_exit_availability(
        &self,
        availability: ExitAvailability,
        reason: Option<String>,
    ) -> Result<(), GatewayError> {
        self.sender
            .send(RuntimeControl::ExitAvailability {
                availability,
                reason,
            })
            .await
            .map_err(|error| GatewayError::Platform {
                operation: "gateway-control",
                message: error.to_string(),
            })
    }
}

struct PendingChunk {
    bytes: Vec<u8>,
    offset: usize,
    consumed: Option<oneshot::Sender<()>>,
}

impl PendingChunk {
    fn new(bytes: Vec<u8>, consumed: oneshot::Sender<()>) -> Self {
        Self {
            bytes,
            offset: 0,
            consumed: Some(consumed),
        }
    }

    fn remaining(&self) -> &[u8] {
        self.bytes.get(self.offset..).unwrap_or_default()
    }

    fn advance(&mut self, count: usize) {
        self.offset = self.offset.saturating_add(count).min(self.bytes.len());
    }

    fn is_empty(&self) -> bool {
        self.offset >= self.bytes.len()
    }

    fn acknowledge(mut self) {
        if let Some(consumed) = self.consumed.take() {
            let _ = consumed.send(());
        }
    }
}

struct RuntimeFlow {
    bridge: Option<BridgeHandle>,
    client_to_onion_buffer: Option<Vec<u8>>,
    pending_to_client: VecDeque<PendingChunk>,
    client_eof_sent: bool,
    onion_eof_seen: bool,
    client_write_closed: bool,
}

impl RuntimeFlow {
    fn captured(chunk_bytes: usize) -> Self {
        Self {
            bridge: None,
            client_to_onion_buffer: Some(vec![0_u8; chunk_bytes]),
            pending_to_client: VecDeque::new(),
            client_eof_sent: false,
            onion_eof_seen: false,
            client_write_closed: false,
        }
    }
}

enum LoopInput {
    Packet(Result<usize, PacketIoError>),
    Bridge(Option<BridgeEvent>),
    Control(Option<RuntimeControl>),
    Tick,
}

enum ReconcileScope {
    None,
    Flow(FlowId),
    All,
}

/// Runtime-loop failures are gateway-scoped by construction. Packet drops and flow refusals
/// never inhabit this type and therefore cannot short-circuit the loop through `?`.
#[derive(Debug)]
struct GatewayFatal(GatewayError);

impl GatewayFatal {
    fn gateway(error: impl Into<GatewayError>) -> Self {
        Self(error.into())
    }

    fn into_gateway(self) -> GatewayError {
        self.0
    }
}

/// Foreground, platform-neutral gateway data plane.
///
/// One instance owns one packet interface, one bounded userspace TCP stack, and the reconstructed
/// byte bridges for all admitted flows. Platform setup and teardown remain the responsibility of
/// [`crate::bindings::TunnelControl`].
pub struct GatewayRuntime {
    config: GatewayConfig,
    server: GatewayServer,
    tcp: TcpStack,
    connector: Arc<dyn OnionStreamConnector>,
    flows: HashMap<FlowId, RuntimeFlow>,
    bridge_events_tx: mpsc::Sender<BridgeEvent>,
    bridge_events_rx: mpsc::Receiver<BridgeEvent>,
    status_handle: GatewayStatusHandle,
    control_tx: mpsc::Sender<RuntimeControl>,
    control_rx: mpsc::Receiver<RuntimeControl>,
}

impl GatewayRuntime {
    /// Construct a stopped data plane with bounded queues and a caller-provided TCP seed.
    pub fn new(
        config: GatewayConfig,
        connector: Arc<dyn OnionStreamConnector>,
        random_seed: u64,
    ) -> Result<Self, GatewayError> {
        let server = GatewayServer::new(config.clone())?;
        let tcp = TcpStack::new(&config, random_seed)?;
        let event_capacity = config.max_flows.saturating_mul(EVENTS_PER_FLOW).max(1);
        let (bridge_events_tx, bridge_events_rx) = mpsc::channel(event_capacity);
        let status_handle = GatewayStatusHandle::new(server.status());
        let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
        Ok(Self {
            flows: HashMap::with_capacity(config.max_flows),
            config,
            server,
            tcp,
            connector,
            bridge_events_tx,
            bridge_events_rx,
            status_handle,
            control_tx,
            control_rx,
        })
    }

    /// Mark explicit packet ingress setup complete and begin packet admission.
    pub fn activate(&mut self, interface_name: String) -> Result<(), GatewayError> {
        self.server.transition(GatewayEvent::Start)?;
        self.server.set_established_interface(interface_name);
        self.server.transition(GatewayEvent::AdmitPackets)?;
        self.publish_status();
        Ok(())
    }

    /// Update compatible Onion exit availability independently from process health.
    pub fn set_exit_availability(
        &mut self,
        availability: ExitAvailability,
        reason: Option<String>,
    ) {
        self.server.set_exit_availability(availability, reason);
        self.publish_status();
    }

    /// Return a stable inspection snapshot.
    pub fn status(&self) -> GatewayStatus {
        self.server.status()
    }

    /// Return a cloneable inspection capability that remains valid while the runtime is running.
    pub fn status_handle(&self) -> GatewayStatusHandle {
        self.status_handle.clone()
    }

    /// Return a bounded control capability for exit discovery and similar dependencies.
    pub fn control_handle(&self) -> GatewayControlHandle {
        GatewayControlHandle {
            sender: self.control_tx.clone(),
        }
    }

    /// Run packet and stream processing until `should_stop` requests cooperative shutdown.
    ///
    /// The callback is checked before every wait and at least once per TCP poll interval. This
    /// keeps the gateway compatible with the native node's existing cooperative stop token without
    /// introducing a second lifecycle primitive.
    pub async fn run<D, S>(
        &mut self,
        device: &mut D,
        mut should_stop: S,
    ) -> Result<(), GatewayError>
    where
        D: PacketIo,
        S: FnMut() -> bool,
    {
        if !self.server.state().admits_packets() {
            return Err(GatewayError::PacketAdmissionClosed(self.server.state()));
        }
        let started = MonotonicInstant::now();
        let mut packet = vec![0_u8; usize::from(self.config.plan.mtu.get())];
        let mut interval = tokio::time::interval(TCP_POLL_INTERVAL);
        let result = loop {
            if should_stop() {
                break Ok(());
            }
            let input = tokio::select! {
                read = device.read_packet(packet.as_mut_slice()) => LoopInput::Packet(read),
                event = self.bridge_events_rx.recv() => LoopInput::Bridge(event),
                control = self.control_rx.recv() => LoopInput::Control(control),
                _ = interval.tick() => LoopInput::Tick,
            };
            let elapsed = started.elapsed();
            let reconcile = match self.process_input(input, &packet, elapsed) {
                Ok(reconcile) => reconcile,
                Err(error) => break Err(error.into_gateway()),
            };
            let reconciled = match reconcile {
                ReconcileScope::None => Ok(()),
                ReconcileScope::Flow(flow) => self.reconcile_flow(flow, elapsed),
                ReconcileScope::All => self.reconcile_all(elapsed),
            };
            if let Err(error) = reconciled {
                break Err(error.into_gateway());
            }
            if let Err(error) = self.flush_egress(device).await {
                break Err(error);
            }
        };

        match result {
            Ok(()) => {
                let stopped = self.stop(device, started.elapsed()).await;
                self.publish_status();
                stopped
            }
            Err(error) => {
                let mut cleanup_error = None;
                record_first_error(
                    &mut cleanup_error,
                    self.server.transition(GatewayEvent::Fail).map(drop),
                );
                self.publish_status();
                record_first_error(
                    &mut cleanup_error,
                    self.stop(device, started.elapsed()).await,
                );
                self.publish_status();
                Err(with_cleanup_error(error, cleanup_error))
            }
        }
    }

    fn publish_status(&self) {
        self.status_handle.publish(self.server.status());
    }

    fn chunk_bytes(&self) -> usize {
        self.config.tcp_buffer_bytes.min(BRIDGE_IO_CHUNK_BYTES)
    }

    fn runtime_flow(&self, flow: FlowId) -> Result<&RuntimeFlow, TcpStackError> {
        self.flows
            .get(&flow)
            .ok_or(TcpStackError::UnknownFlow(flow))
    }

    fn runtime_flow_mut(&mut self, flow: FlowId) -> Result<&mut RuntimeFlow, TcpStackError> {
        self.flows
            .get_mut(&flow)
            .ok_or(TcpStackError::UnknownFlow(flow))
    }

    fn process_input(
        &mut self,
        input: LoopInput,
        packet_buffer: &[u8],
        elapsed: Duration,
    ) -> Result<ReconcileScope, GatewayFatal> {
        match input {
            LoopInput::Packet(result) => {
                let length = result.map_err(GatewayFatal::gateway)?;
                let packet = packet_buffer
                    .get(..length)
                    .ok_or(PacketIoError::InvalidLength {
                        length,
                        capacity: packet_buffer.len(),
                    })
                    .map_err(GatewayFatal::gateway)?;
                match self.ingest_packet(packet.to_vec(), elapsed)? {
                    PacketOutcome::Consumed(flow) => Ok(ReconcileScope::Flow(flow)),
                    PacketOutcome::Dropped(_) | PacketOutcome::FlowRejected { .. } => {
                        Ok(ReconcileScope::None)
                    }
                }
            }
            LoopInput::Bridge(Some(event)) => self
                .handle_bridge_event(event, elapsed)
                .map_err(GatewayFatal::gateway),
            LoopInput::Bridge(None) => Err(GatewayFatal::gateway(GatewayError::Platform {
                operation: "bridge-events",
                message: "gateway bridge event channel closed".to_string(),
            })),
            LoopInput::Control(Some(RuntimeControl::ExitAvailability {
                availability,
                reason,
            })) => {
                self.server.set_exit_availability(availability, reason);
                self.publish_status();
                Ok(ReconcileScope::None)
            }
            LoopInput::Control(None) => Ok(ReconcileScope::None),
            LoopInput::Tick => {
                self.tcp.poll(elapsed);
                let expired = self
                    .tcp
                    .expired_pending_flows(elapsed)
                    .map_err(GatewayFatal::gateway)?;
                for flow in expired {
                    self.fail_flow(flow, elapsed)
                        .map_err(GatewayFatal::gateway)?;
                }
                Ok(ReconcileScope::All)
            }
        }
    }

    fn ingest_packet(
        &mut self,
        packet: Vec<u8>,
        elapsed: Duration,
    ) -> Result<PacketOutcome, GatewayFatal> {
        let segment = match crate::classify_ipv4_packet(&packet) {
            PacketDisposition::Tcp(segment) => segment,
            PacketDisposition::Drop(reason) => return Ok(PacketOutcome::Dropped(reason)),
        };
        if !self.flows.contains_key(&segment.flow) {
            if !segment.opens_flow() {
                self.tcp.reject_segment(packet, elapsed);
                return Ok(PacketOutcome::FlowRejected {
                    flow: segment.flow,
                    reason: FlowRejectReason::MissingInitialSyn,
                });
            }
            if let Some(reason) = self.admit_flow(segment, elapsed)? {
                self.tcp.reject_segment(packet, elapsed);
                return Ok(PacketOutcome::FlowRejected {
                    flow: segment.flow,
                    reason,
                });
            }
        }
        self.tcp
            .ingest_segment(packet, segment, elapsed)
            .map_err(GatewayFatal::gateway)?;
        Ok(PacketOutcome::Consumed(segment.flow))
    }

    fn admit_flow(
        &mut self,
        segment: crate::TcpSegment,
        elapsed: Duration,
    ) -> Result<Option<FlowRejectReason>, GatewayFatal> {
        match self.server.capture_flow(segment.flow) {
            Ok(_) => {}
            Err(GatewayError::FlowTable(FlowTableError::CapacityExhausted { limit })) => {
                return Ok(Some(FlowRejectReason::CapacityExhausted { limit }));
            }
            Err(error) => return Err(GatewayFatal::gateway(error)),
        }
        self.server
            .transition_flow(segment.flow, FlowEvent::BindTarget)
            .map_err(GatewayFatal::gateway)?;
        match self.tcp.admit_flow(segment, elapsed) {
            TcpFlowAdmission::Accepted => {
                let chunk_bytes = self.chunk_bytes();
                self.flows
                    .insert(segment.flow, RuntimeFlow::captured(chunk_bytes));
                self.publish_status();
                Ok(None)
            }
            TcpFlowAdmission::Rejected(reason) => {
                self.server
                    .transition_flow(segment.flow, FlowEvent::Fail)
                    .map_err(GatewayFatal::gateway)?;
                self.publish_status();
                Ok(Some(reason))
            }
        }
    }

    fn handle_bridge_event(
        &mut self,
        event: BridgeEvent,
        elapsed: Duration,
    ) -> Result<ReconcileScope, GatewayError> {
        match event {
            BridgeEvent::Opened(flow) => {
                if self.server.flow_state(flow) == Some(FlowState::Opening(flow)) {
                    self.server.transition_flow(flow, FlowEvent::Establish)?;
                    self.publish_status();
                }
                Ok(ReconcileScope::Flow(flow))
            }
            BridgeEvent::Data {
                flow,
                bytes,
                consumed,
            } => {
                let Some(runtime) = self.flows.get_mut(&flow) else {
                    return Ok(ReconcileScope::None);
                };
                runtime
                    .pending_to_client
                    .push_back(PendingChunk::new(bytes, consumed));
                self.flush_pending_to_client(flow, elapsed)?;
                Ok(ReconcileScope::Flow(flow))
            }
            BridgeEvent::ClientBuffer { flow, mut buffer } => {
                let chunk_bytes = self.chunk_bytes();
                let Some(runtime) = self.flows.get_mut(&flow) else {
                    return Ok(ReconcileScope::None);
                };
                buffer.resize(chunk_bytes, 0);
                runtime.client_to_onion_buffer = Some(buffer);
                Ok(ReconcileScope::Flow(flow))
            }
            BridgeEvent::PeerClosed(flow) => {
                let Some(runtime) = self.flows.get_mut(&flow) else {
                    return Ok(ReconcileScope::None);
                };
                runtime.onion_eof_seen = true;
                self.mark_half_closed(flow)?;
                self.publish_status();
                Ok(ReconcileScope::Flow(flow))
            }
            BridgeEvent::Failed { flow, error } => {
                if !self.flows.contains_key(&flow) {
                    return Ok(ReconcileScope::None);
                }
                self.fail_flow(flow, elapsed)?;
                if matches!(error, GatewayError::OnionUnavailable { .. }) {
                    self.server
                        .set_exit_availability(ExitAvailability::Unknown, Some(error.to_string()));
                }
                self.publish_status();
                Ok(ReconcileScope::None)
            }
        }
    }

    fn reconcile_all(&mut self, elapsed: Duration) -> Result<(), GatewayFatal> {
        let flows = self.flows.keys().copied().collect::<Vec<_>>();
        for flow in flows {
            self.reconcile_flow(flow, elapsed)?;
        }
        Ok(())
    }

    fn reconcile_flow(&mut self, flow: FlowId, elapsed: Duration) -> Result<(), GatewayFatal> {
        if !self.flows.contains_key(&flow) {
            return Ok(());
        }
        self.maybe_start_bridge(flow)
            .map_err(GatewayFatal::gateway)?;
        self.flush_pending_to_client(flow, elapsed)
            .map_err(GatewayFatal::gateway)?;
        self.drive_client_to_onion(flow, elapsed)
            .map_err(GatewayFatal::gateway)?;
        self.finish_if_closed(flow, elapsed)
            .map_err(GatewayFatal::gateway)
    }

    fn maybe_start_bridge(&mut self, flow: FlowId) -> Result<(), GatewayError> {
        if self.tcp.endpoint_state(flow)? != TcpEndpointState::Established
            || self.server.flow_state(flow) != Some(FlowState::TargetBound(flow))
        {
            return Ok(());
        }
        self.server.transition_flow(flow, FlowEvent::Open)?;
        let chunk_bytes = self.chunk_bytes();
        let bridge = spawn_bridge(
            flow,
            Arc::clone(&self.connector),
            self.bridge_events_tx.clone(),
            self.config.tcp_buffer_bytes,
            chunk_bytes,
        );
        let runtime = self.runtime_flow_mut(flow)?;
        runtime.bridge = Some(bridge);
        Ok(())
    }

    fn drive_client_to_onion(
        &mut self,
        flow: FlowId,
        elapsed: Duration,
    ) -> Result<(), GatewayError> {
        if !matches!(
            self.server.flow_state(flow),
            Some(FlowState::Established(_) | FlowState::HalfClosed(_))
        ) {
            return Ok(());
        }
        let Some(mut buffer) = self
            .flows
            .get_mut(&flow)
            .and_then(|runtime| runtime.client_to_onion_buffer.take())
        else {
            return Ok(());
        };
        let length = match self.tcp.peek_application_data(flow, &mut buffer) {
            Ok(length) => length,
            Err(TcpStackError::ReceiveUnavailable(_)) => 0,
            Err(error) => return Err(error.into()),
        };
        if length > 0 {
            if length > buffer.len() {
                return Err(TcpStackError::ReceiveCommitMismatch {
                    flow,
                    expected: length,
                    actual: buffer.len(),
                }
                .into());
            }
            buffer.truncate(length);
            let send = self
                .runtime_flow(flow)?
                .bridge
                .as_ref()
                .ok_or(TcpStackError::UnknownFlow(flow))?
                .try_send(BridgeCommand::Data(buffer));
            match send {
                Ok(()) => self.tcp.commit_application_read(flow, length)?,
                Err(mpsc::error::TrySendError::Full(command)) => match command {
                    BridgeCommand::Data(mut buffer) => {
                        buffer.resize(self.chunk_bytes(), 0);
                        self.runtime_flow_mut(flow)?.client_to_onion_buffer = Some(buffer);
                        return Ok(());
                    }
                    BridgeCommand::CloseWrite => {
                        return Err(GatewayError::platform(
                            "bridge-client-to-onion",
                            "data send returned an unrelated close-write command",
                        ));
                    }
                },
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.fail_flow(flow, elapsed)?;
                    return Ok(());
                }
            }
        } else {
            self.runtime_flow_mut(flow)?.client_to_onion_buffer = Some(buffer);
        }

        if !self.tcp.client_read_open(flow)? {
            let should_close = self
                .flows
                .get(&flow)
                .is_some_and(|runtime| !runtime.client_eof_sent);
            if should_close {
                let send = self
                    .runtime_flow(flow)?
                    .bridge
                    .as_ref()
                    .ok_or(TcpStackError::UnknownFlow(flow))?
                    .try_send(BridgeCommand::CloseWrite);
                match send {
                    Ok(()) => {
                        let runtime = self.runtime_flow_mut(flow)?;
                        runtime.client_eof_sent = true;
                        self.mark_half_closed(flow)?;
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        self.fail_flow(flow, elapsed)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn flush_pending_to_client(
        &mut self,
        flow: FlowId,
        elapsed: Duration,
    ) -> Result<(), GatewayError> {
        loop {
            let written = {
                let Some(runtime) = self.flows.get(&flow) else {
                    return Ok(());
                };
                let Some(chunk) = runtime.pending_to_client.front() else {
                    break;
                };
                if chunk.is_empty() {
                    0
                } else {
                    match self.tcp.write_application_data(flow, chunk.remaining()) {
                        Ok(written) => written,
                        Err(TcpStackError::SendUnavailable(_)) => break,
                        Err(error) => return Err(error.into()),
                    }
                }
            };
            let runtime = self.runtime_flow_mut(flow)?;
            let Some(chunk) = runtime.pending_to_client.front_mut() else {
                break;
            };
            chunk.advance(written);
            let completed = chunk
                .is_empty()
                .then(|| runtime.pending_to_client.pop_front())
                .flatten();
            if let Some(completed) = completed {
                completed.acknowledge();
            }
            if written == 0 {
                break;
            }
            self.tcp.poll(elapsed);
        }

        let should_close = self.flows.get(&flow).is_some_and(|runtime| {
            runtime.onion_eof_seen
                && runtime.pending_to_client.is_empty()
                && !runtime.client_write_closed
        });
        if should_close {
            self.tcp.close_application_write(flow)?;
            let runtime = self.runtime_flow_mut(flow)?;
            runtime.client_write_closed = true;
            self.tcp.poll(elapsed);
        }
        Ok(())
    }

    fn mark_half_closed(&mut self, flow: FlowId) -> Result<(), GatewayError> {
        if self.server.flow_state(flow) == Some(FlowState::Established(flow)) {
            self.server.transition_flow(flow, FlowEvent::HalfClose)?;
        }
        Ok(())
    }

    fn finish_if_closed(&mut self, flow: FlowId, elapsed: Duration) -> Result<(), GatewayError> {
        if self.tcp.endpoint_state(flow)? != TcpEndpointState::Closed {
            return Ok(());
        }
        let graceful = self
            .flows
            .get(&flow)
            .is_some_and(|runtime| runtime.client_eof_sent && runtime.onion_eof_seen);
        if !graceful {
            return self.fail_flow(flow, elapsed);
        }
        if let Some(runtime) = self.flows.remove(&flow) {
            if let Some(bridge) = runtime.bridge {
                bridge.abort();
            }
        }
        self.server.transition_flow(flow, FlowEvent::Close)?;
        let _ = self.tcp.release_closed_flow(flow)?;
        self.publish_status();
        Ok(())
    }

    fn fail_flow(&mut self, flow: FlowId, elapsed: Duration) -> Result<(), GatewayError> {
        if let Some(runtime) = self.flows.remove(&flow) {
            if let Some(bridge) = runtime.bridge {
                bridge.abort();
            }
        }
        if self.server.flow_state(flow).is_some() {
            self.server.transition_flow(flow, FlowEvent::Fail)?;
        }
        match self.tcp.endpoint_state(flow) {
            Ok(_) => self.tcp.abort_flow(flow, elapsed)?,
            Err(TcpStackError::UnknownFlow(_)) => {}
            Err(error) => return Err(error.into()),
        }
        self.publish_status();
        Ok(())
    }

    async fn flush_egress<D: PacketIo>(&mut self, device: &mut D) -> Result<(), GatewayError> {
        for packet in self.tcp.take_egress() {
            device.write_packet(&packet).await?;
        }
        Ok(())
    }

    async fn stop<D: PacketIo>(
        &mut self,
        device: &mut D,
        elapsed: Duration,
    ) -> Result<(), GatewayError> {
        let mut first_error = None;
        if matches!(
            self.server.state(),
            GatewayState::Starting
                | GatewayState::Active
                | GatewayState::Degraded
                | GatewayState::Failed
        ) {
            record_first_error(
                &mut first_error,
                self.server.transition(GatewayEvent::Stop).map(drop),
            );
        }
        let flows = self.flows.keys().copied().collect::<Vec<_>>();
        for flow in flows {
            record_first_error(&mut first_error, self.fail_flow(flow, elapsed));
        }
        record_first_error(&mut first_error, self.flush_egress(device).await);
        if self.server.state() == GatewayState::Stopping {
            record_first_error(
                &mut first_error,
                self.server.transition(GatewayEvent::FinishStop).map(drop),
            );
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn record_first_error(first_error: &mut Option<GatewayError>, result: Result<(), GatewayError>) {
    if first_error.is_none() {
        *first_error = result.err();
    }
}

fn with_cleanup_error(runtime: GatewayError, cleanup: Option<GatewayError>) -> GatewayError {
    match cleanup {
        Some(cleanup) => GatewayError::RuntimeCleanup {
            runtime: Box::new(runtime),
            cleanup: Box::new(cleanup),
        },
        None => runtime,
    }
}

#[cfg(test)]
mod tests;
