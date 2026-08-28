//! Onion byte-stream effect boundary.

use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

use crate::FlowId;
use crate::GatewayError;

/// Async duplex stream accepted by the shared Onion byte pump.
pub trait GatewayDuplex: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> GatewayDuplex for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// Owned, type-erased gateway side of one reconstructed TCP stream.
pub type BoxGatewayDuplex = Box<dyn GatewayDuplex>;

/// Node-owned effect that opens one immutable target over an Onion TCP route.
///
/// The gateway crate never imports Rings directory or circuit types. A native node implements
/// this boundary by building a route for `flow.target`, opening it, and relaying `stream` through
/// the resulting Onion stream. Failure must be fail-closed; implementations must not dial the
/// target directly from the client node.
#[async_trait::async_trait]
pub trait OnionStreamConnector: Send + Sync {
    /// Open one admitted flow and attach the reconstructed local byte stream.
    ///
    /// This method returns after the Onion route and exit stream are ready and the supplied
    /// stream has been handed to its long-running relay task. It must not wait for the entire
    /// stream lifetime before returning.
    async fn open_stream(&self, flow: FlowId, stream: BoxGatewayDuplex)
        -> Result<(), GatewayError>;
}
