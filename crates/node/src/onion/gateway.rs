//! Native node adapter for the runtime-neutral gateway Onion boundary.

use std::sync::Arc;

use rings_gateway::BoxGatewayDuplex;
use rings_gateway::BoxOnionStreamProbe;
use rings_gateway::FlowId;
use rings_gateway::GatewayError;
use rings_gateway::OnionStreamCondition;
use rings_gateway::OnionStreamConnector;
use rings_gateway::OnionStreamProbe;

use crate::onion::native::NativeOnionCircuitHandle;
use crate::onion::session::witness::OnionSessionCondition;
use crate::onion::session::witness::OnionSessionWitness;
use crate::onion::tcp::OnionLocalStream;
use crate::onion::OnionProxyTarget;
use crate::processor::Processor;

/// The diagnostic a captured flow's reset carries when its session failed closed.
const ONION_SESSION_FAILED: &str = "the onion session failed closed";

/// Native connector that maps one captured target to one Rings Onion TCP stream.
pub struct NativeOnionGatewayConnector {
    processor: Arc<Processor>,
    onion: NativeOnionCircuitHandle,
}

impl NativeOnionGatewayConnector {
    /// Bind a native processor and its installed Onion runtime: every captured flow opens a
    /// `tcp` session ([`NativeOnionCircuitHandle::open_tcp_target`]).
    pub fn new(processor: Arc<Processor>, onion: NativeOnionCircuitHandle) -> Self {
        Self { processor, onion }
    }

    fn onion_error(flow: FlowId, error: impl std::fmt::Display) -> GatewayError {
        GatewayError::OnionUnavailable {
            target: flow.target,
            message: error.to_string(),
        }
    }
}

/// The gateway's probe of one captured flow's session: its witness, read at any instant.
struct NativeOnionStreamProbe(OnionSessionWitness);

impl OnionStreamProbe for NativeOnionStreamProbe {
    /// The session witness's condition, in the gateway's terms.
    fn condition(&self) -> OnionStreamCondition {
        match self.0.condition() {
            OnionSessionCondition::Live => OnionStreamCondition::Live,
            OnionSessionCondition::Stalled => OnionStreamCondition::Stalled,
            OnionSessionCondition::Failed => {
                OnionStreamCondition::Failed(ONION_SESSION_FAILED.to_string())
            }
        }
    }
}

#[async_trait::async_trait]
impl OnionStreamConnector for NativeOnionGatewayConnector {
    async fn open_stream(
        &self,
        flow: FlowId,
        stream: BoxGatewayDuplex,
    ) -> Result<BoxOnionStreamProbe, GatewayError> {
        let target = OnionProxyTarget::parse_authority(&flow.target.to_string())
            .map_err(|error| Self::onion_error(flow, error))?;
        let opened = self
            .onion
            .open_tcp_target(&self.processor, target)
            .await
            .map_err(|error| Self::onion_error(flow, error))?;
        Ok(Box::new(NativeOnionStreamProbe(
            opened.relay_witnessed(stream),
        )))
    }
}

impl OnionLocalStream for BoxGatewayDuplex {
    /// A drop: a duplex expresses no abortive close, so the failure crosses the gateway
    /// boundary as the probe, which reads `Failed` before this drop (#902, #950).
    fn reset(self) {}
}
