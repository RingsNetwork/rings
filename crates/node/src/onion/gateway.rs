//! Native node adapter for the runtime-neutral gateway Onion boundary.

use std::sync::Arc;

use rings_gateway::BoxGatewayDuplex;
use rings_gateway::FlowId;
use rings_gateway::GatewayError;
use rings_gateway::OnionStreamConnector;

use crate::onion::proxy::OnionProxyConfig;
use crate::onion::tcp::NativeOnionCircuitHandle;
use crate::onion::OnionProxyTarget;
use crate::processor::Processor;

/// Native connector that maps one captured target to one Rings Onion TCP stream.
pub struct NativeOnionGatewayConnector {
    processor: Arc<Processor>,
    onion: NativeOnionCircuitHandle,
    proxy: OnionProxyConfig,
}

impl NativeOnionGatewayConnector {
    /// Bind a native processor and installed Onion runtime to gateway route options.
    pub fn new(
        processor: Arc<Processor>,
        onion: NativeOnionCircuitHandle,
        proxy: OnionProxyConfig,
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
    ) -> Result<(), GatewayError> {
        let target = OnionProxyTarget::parse_authority(&flow.target.to_string())
            .map_err(|error| Self::onion_error(flow, error))?;
        let route = self
            .processor
            .build_onion_proxy_route(self.proxy.clone(), target)
            .await
            .map_err(|error| Self::onion_error(flow, error))?;
        let opened = self
            .onion
            .open_tcp_stream(route.route, route.target)
            .await
            .map_err(|error| Self::onion_error(flow, error))?;
        opened.relay(stream);
        Ok(())
    }
}
