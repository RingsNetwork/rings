//! Native node adapter for the runtime-neutral gateway Onion boundary.

use std::sync::Arc;

use rings_gateway::BoxGatewayDuplex;
use rings_gateway::FlowId;
use rings_gateway::GatewayError;
use rings_gateway::OnionStreamConnector;
use rings_gateway::OnionStreamEnd;

use crate::onion::native::NativeOnionCircuitHandle;
use crate::onion::proxy::OnionProxyProtocol;
use crate::onion::tcp::OnionLocalStream;
use crate::onion::tcp::OnionPumpEnd;
use crate::onion::OnionProxyTarget;
use crate::processor::Processor;

/// Native connector that maps one captured target to one Rings Onion TCP stream.
pub struct NativeOnionGatewayConnector {
    processor: Arc<Processor>,
    onion: NativeOnionCircuitHandle,
    proxy: OnionProxyProtocol,
}

impl NativeOnionGatewayConnector {
    /// Bind a native processor and installed Onion runtime to gateway route options.
    pub fn new(
        processor: Arc<Processor>,
        onion: NativeOnionCircuitHandle,
        proxy: OnionProxyProtocol,
    ) -> Self {
        Self {
            processor,
            onion,
            proxy,
        }
    }

    fn onion_error(flow: FlowId, error: impl std::fmt::Display) -> GatewayError {
        GatewayError::OnionUnavailable {
            target: flow.target,
            message: error.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl OnionStreamConnector for NativeOnionGatewayConnector {
    async fn open_stream(
        &self,
        flow: FlowId,
        stream: BoxGatewayDuplex,
    ) -> Result<OnionStreamEnd, GatewayError> {
        let target = OnionProxyTarget::parse_authority(&flow.target.to_string())
            .map_err(|error| Self::onion_error(flow, error))?;
        let route = self
            .processor
            .build_onion_proxy_route(self.proxy, target)
            .await
            .map_err(|error| Self::onion_error(flow, error))?;
        let opened = self
            .onion
            .open_tcp_stream(route.route, route.target)
            .await
            .map_err(|error| Self::onion_error(flow, error))?;
        let (end, report) = tokio::sync::oneshot::channel();
        opened.relay_reporting(stream, move |ended| {
            if ended == OnionPumpEnd::Failed {
                // A dropped report means the gateway already released the flow.
                let _ = end.send(Err(GatewayError::OnionStreamReset {
                    target: flow.target,
                    message: "the onion session failed closed".to_string(),
                }));
            }
        });
        Ok(report)
    }
}

impl OnionLocalStream for BoxGatewayDuplex {
    /// A drop: a duplex expresses no abortive close, so the failure crosses the gateway
    /// boundary as the connector's [`OnionStreamEnd`], which resets the captured flow (#955).
    fn reset(self) {}
}
