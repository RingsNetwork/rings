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

/// What a connector knows, at one instant, of the circuit of an open Onion stream (#950).
///
/// ```text
/// Live ⇄ Stalled             a frame becomes known missing (a later one arrived) / it arrives
/// Live | Stalled → Failed(c)  the stream failed closed; Failed is absorbing
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnionStreamCondition {
    /// No failure was observed and no frame is known missing.
    Live,
    /// A frame of the stream is known missing: a later one arrived and is held behind it.
    Stalled,
    /// The stream failed closed, at its far side or on a link it rides; carries the node-layer
    /// diagnostic.
    Failed(String),
}

/// Synchronous view of the circuit of one open Onion stream, read by the runtime whenever it
/// names how the captured flow ended.
///
/// A duplex expresses only an orderly end (EOF); the probe tells it apart from an abortive one.
///
/// Law (report before end): a connector whose stream fails makes its probe read `Failed` before
/// it ends the duplex, so an EOF read while the probe reads `Failed` is that failure, never an
/// orderly end (#902).
pub trait OnionStreamProbe: Send + Sync {
    /// The condition of the stream's circuit now.
    fn condition(&self) -> OnionStreamCondition;
}

/// Owned, type-erased probe of one open Onion stream.
pub type BoxOnionStreamProbe = Box<dyn OnionStreamProbe>;

/// The probe of a connector that observes nothing of its circuit.
struct UnobservedStream;

impl OnionStreamProbe for UnobservedStream {
    /// Always `Live`: the duplex alone ends the stream.
    fn condition(&self) -> OnionStreamCondition {
        OnionStreamCondition::Live
    }
}

/// The probe of a connector that observes nothing of its circuit: it always reads `Live`, so
/// the duplex alone ends the stream.
pub fn unobserved_stream() -> BoxOnionStreamProbe {
    Box::new(UnobservedStream)
}

/// Node-owned effect that opens one immutable target over an Onion TCP route.
///
/// The gateway crate never imports Rings directory or onion types. A native node implements
/// this boundary by building a route for `flow.target`, opening it, and relaying `stream` through
/// the resulting Onion stream. Failure must be fail-closed; implementations must not dial the
/// target directly from the client node.
#[async_trait::async_trait]
pub trait OnionStreamConnector: Send + Sync {
    /// Open one admitted flow and attach the reconstructed local byte stream.
    ///
    /// This method returns after the Onion route and exit stream are ready and the supplied
    /// stream has been handed to its long-running relay task. It must not wait for the entire
    /// stream lifetime before returning; the returned [`OnionStreamProbe`] reports the circuit's
    /// condition for the rest of it.
    async fn open_stream(
        &self,
        flow: FlowId,
        stream: BoxGatewayDuplex,
    ) -> Result<BoxOnionStreamProbe, GatewayError>;
}
