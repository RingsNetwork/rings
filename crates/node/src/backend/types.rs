#![warn(missing_docs)]

//! Backend Message Types.
use std::io::ErrorKind as IOErrorKind;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::message::MessagePayload;
use rings_rpc::protos::rings_node::SendBackendMessageRequest;
use rings_snark::prelude::nova::provider::hyperkzg::Bn256EngineKZG;
use rings_snark::prelude::nova::provider::GrumpkinEngine;
use rings_snark::prelude::nova::provider::PallasEngine;
use rings_snark::prelude::nova::provider::VestaEngine;
use serde::Deserialize;
use serde::Serialize;

use crate::backend::snark::SNARKGenerator;
use crate::error::Error;
use crate::provider::Provider;

/// TunnelId type, use uuid.
pub type TunnelId = uuid::Uuid;

/// BackendMessage struct for handling CustomMessage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BackendMessage {
    /// extension message
    Extension(Bytes),
    /// server message
    ServiceMessage(ServiceMessage),
    /// Plain text
    PlainText(String),
    /// SNARK with curve pallas and vesta
    SNARKTaskMessage(SNARKTaskMessage),
}

/// Message for snark task
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SNARKTaskMessage {
    /// uuid of task
    pub task_id: uuid::Uuid,
    /// task details
    #[serde(
        serialize_with = "crate::util::serialize_gzip",
        deserialize_with = "crate::util::deserialize_gzip"
    )]
    pub task: SNARKTask,
}

/// Message types for snark task, including proof and verify
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SNARKTask {
    /// Proof task
    SNARKProof(SNARKProofTask),
    /// Verify task
    SNARKVerify(SNARKVerifyTask),
}

/// Message type of snark proof
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SNARKProofTask {
    /// SNARK with curve pallas and vesta
    PallasVasta(SNARKGenerator<PallasEngine, VestaEngine>),
    /// SNARK with curve vesta and pallas
    VastaPallas(SNARKGenerator<VestaEngine, PallasEngine>),
    /// SNARK with curve bn256 whth KZG multi linear commitment and grumpkin
    Bn256KZGGrumpkin(SNARKGenerator<Bn256EngineKZG, GrumpkinEngine>),
}

/// Message type of snark proof
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SNARKVerifyTask {
    /// SNARK with curve pallas and vesta
    PallasVasta(String),
    /// SNARK with curve vesta and pallas
    VastaPallas(String),
    /// SNARK with curve bn256 whth KZG multi linear commitment and grumpkin
    Bn256KZGGrumpkin(String),
}

/// ServiceMessage
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum ServiceMessage {
    /// Tunnel Open
    TcpDial {
        /// Tunnel Id
        tid: TunnelId,
        /// service name
        service: String,
    },
    /// Tunnel Close
    TcpClose {
        /// Tunnel Id
        tid: TunnelId,
        /// The reason of close
        reason: TunnelDefeat,
    },
    /// Send Tcp Package
    TcpPackage {
        /// Tunnel Id
        tid: TunnelId,
        /// Tcp Package
        body: Bytes,
    },
    /// Http Request
    HttpRequest(HttpRequest),
    /// Http Response
    HttpResponse(HttpResponse),
}

/// A list specifying general categories of Tunnel error like [std::io::ErrorKind].
#[derive(Deserialize, Serialize, Debug, Clone, Copy)]
#[repr(u8)]
#[non_exhaustive]
pub enum TunnelDefeat {
    /// Failed to send data to peer by webrtc datachannel.
    WebrtcDatachannelSendFailed = 1,
    /// The connection timed out when dialing.
    ConnectionTimeout = 2,
    /// Got [std::io::ErrorKind::ConnectionRefused] error from local stream.
    ConnectionRefused = 3,
    /// Got [std::io::ErrorKind::ConnectionAborted] error from local stream.
    ConnectionAborted = 4,
    /// Got [std::io::ErrorKind::ConnectionReset] error from local stream.
    ConnectionReset = 5,
    /// Got [std::io::ErrorKind::NotConnected] error from local stream.
    NotConnected = 6,
    /// The connection is closed by peer.
    ConnectionClosed = 7,
    /// Unknown [std::io::ErrorKind] error.
    Unknown = u8::MAX,
}

/// HttpRequest
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpRequest {
    /// Request Id
    pub rid: Option<String>,
    /// Service name
    pub service: String,
    /// Method
    pub method: String,
    /// Path
    pub path: String,
    /// Headers
    pub headers: Vec<(String, String)>,
    /// Body
    pub body: Option<Vec<u8>>,
}

/// HttpResponse
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpResponse {
    /// Request Id
    pub rid: Option<String>,
    /// Status
    pub status: u16,
    /// Headers
    pub headers: Vec<(String, String)>,
    /// Body
    pub body: Option<Bytes>,
}

/// MessageHandler trait
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait MessageHandler<T> {
    /// handle_message
    async fn handle_message(
        &self,
        provider: Arc<Provider>,
        ctx: &MessagePayload,
        data: &T,
    ) -> Result<(), Box<dyn std::error::Error>>;
}

impl From<ServiceMessage> for BackendMessage {
    fn from(val: ServiceMessage) -> Self {
        BackendMessage::ServiceMessage(val)
    }
}

impl From<SNARKTaskMessage> for BackendMessage {
    fn from(val: SNARKTaskMessage) -> Self {
        BackendMessage::SNARKTaskMessage(val)
    }
}

impl From<IOErrorKind> for TunnelDefeat {
    fn from(kind: IOErrorKind) -> TunnelDefeat {
        match kind {
            IOErrorKind::ConnectionRefused => TunnelDefeat::ConnectionRefused,
            IOErrorKind::ConnectionAborted => TunnelDefeat::ConnectionAborted,
            IOErrorKind::ConnectionReset => TunnelDefeat::ConnectionReset,
            IOErrorKind::NotConnected => TunnelDefeat::NotConnected,
            _ => TunnelDefeat::Unknown,
        }
    }
}

/// This macro is aims to generate code like
/// '''
/// impl <T1, T2, T3> MessageHandler<BackendMessage> for (T1, T2, T3)
/// where
///     T1: MessageHandler<BackendMessage> + Send + Sync + Sized,
///     T2: MessageHandler<BackendMessage> + Send + Sync + Sized,
///     T3: MessageHandler<BackendMessage> + Send + Sync + Sized,
/// {
///     async fn handle_message(
///         &self,
///         provider: Arc<Provider>,
///         ctx: &MessagePayload,
///         msg: &BackendMessage,
///     ) -> std::result::Result<(), Box<dyn std::error::Error>> {
///         self.0.handle_message(provider.clone(), ctx, msg).await?;
///         self.1.handle_message(provider.clone(), ctx, msg).await?;
///         self.2.handle_message(provider.clone(), ctx, msg).await?;
///         Ok(())
///     }
/// }
/// '''ignore
macro_rules! impl_message_handler_for_tuple {
    // Case for WebAssembly target (`wasm`)
    ($($T:ident),+; $($n: tt),+; wasm) => {
        #[async_trait::async_trait(?Send)]
        impl<$($T: MessageHandler<BackendMessage>),+> MessageHandler<BackendMessage> for ($($T),+)
        {
            async fn handle_message(
                &self,
                provider: Arc<Provider>,
                ctx: &MessagePayload,
                msg: &BackendMessage,
            ) -> std::result::Result<(), Box<dyn std::error::Error>> {
                $(
                    self.$n.handle_message(provider.clone(), ctx, msg).await?;
                )+
                Ok(())
            }
        }
    };

    // Case for non-WebAssembly targets
    ($($T:ident),+; $($n: tt),+; non_wasm) => {
        #[async_trait::async_trait]
        impl<$($T: MessageHandler<BackendMessage> + Send + Sync),+> MessageHandler<BackendMessage> for ($($T),+)
        {
            async fn handle_message(
                &self,
                provider: Arc<Provider>,
                ctx: &MessagePayload,
                msg: &BackendMessage,
            ) -> std::result::Result<(), Box<dyn std::error::Error>> {
                $(
                    self.$n.handle_message(provider.clone(), ctx, msg).await?;
                )+
                Ok(())
            }
        }
    };
}

#[cfg(not(target_family = "wasm"))]
impl_message_handler_for_tuple!(T1, T2; 0, 1; non_wasm);
#[cfg(not(target_family = "wasm"))]
impl_message_handler_for_tuple!(T1, T2, T3; 0, 1, 2; non_wasm);
#[cfg(not(target_family = "wasm"))]
impl_message_handler_for_tuple!(T1, T2, T3, T4; 0, 1, 2, 3; non_wasm);
#[cfg(not(target_family = "wasm"))]
impl_message_handler_for_tuple!(T1, T2, T3, T4, T5; 0, 1, 2, 3, 4; non_wasm);

#[cfg(target_family = "wasm")]
impl_message_handler_for_tuple!(T1, T2; 0, 1; wasm);
#[cfg(target_family = "wasm")]
impl_message_handler_for_tuple!(T1, T2, T3; 0, 1, 2; wasm);
#[cfg(target_family = "wasm")]
impl_message_handler_for_tuple!(T1, T2, T3, T4; 0, 1, 2, 3; wasm);
#[cfg(target_family = "wasm")]
impl_message_handler_for_tuple!(T1, T2, T3, T4, T5; 0, 1, 2, 3, 4; wasm);

impl BackendMessage {
    /// Convert to SendBackendMessageRequest
    pub fn into_send_backend_message_request(
        self,
        destination_did: impl ToString,
    ) -> Result<SendBackendMessageRequest, Error> {
        Ok(SendBackendMessageRequest {
            destination_did: destination_did.to_string(),
            data: serde_json::to_string(&self)?,
        })
    }
}
