#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ConnectPeerViaHttpRequest {
    #[prost(string, tag = "1")]
    pub url: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ConnectPeerViaHttpResponse {
    #[prost(string, tag = "1")]
    pub did: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ConnectWithDidRequest {
    #[prost(string, tag = "1")]
    pub did: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ConnectWithDidResponse {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SeedPeer {
    #[prost(string, tag = "1")]
    pub did: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub url: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Seed {
    #[prost(message, repeated, tag = "1")]
    pub peers: ::prost::alloc::vec::Vec<SeedPeer>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ConnectWithSeedRequest {
    #[prost(message, optional, tag = "1")]
    pub seed: ::core::option::Option<Seed>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ConnectWithSeedResponse {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Peer {
    #[prost(string, tag = "1")]
    pub did: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub state: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListPeersRequest {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ListPeersResponse {
    #[prost(message, repeated, tag = "1")]
    pub peers: ::prost::alloc::vec::Vec<Peer>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CreateOfferRequest {
    #[prost(string, tag = "1")]
    pub did: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CreateOfferResponse {
    #[prost(string, tag = "1")]
    pub offer: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AnswerOfferRequest {
    #[prost(string, tag = "1")]
    pub offer: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AnswerOfferResponse {
    #[prost(string, tag = "1")]
    pub answer: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AcceptAnswerRequest {
    #[prost(string, tag = "1")]
    pub answer: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AcceptAnswerResponse {
    #[prost(message, optional, tag = "1")]
    pub peer: ::core::option::Option<Peer>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DisconnectRequest {
    #[prost(string, tag = "1")]
    pub did: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DisconnectResponse {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SendMessageRequest {
    #[prost(string, tag = "1")]
    pub destination_did: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub data: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SendMessageResponse {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PublishToTopicRequest {
    #[prost(string, tag = "1")]
    pub topic: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub data: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PublishToTopicResponse {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FetchTopicRequest {
    #[prost(string, tag = "1")]
    pub topic: ::prost::alloc::string::String,
    #[prost(int64, tag = "2")]
    pub skip: i64,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FetchTopicResponse {
    #[prost(string, repeated, tag = "1")]
    pub data: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RegisterServiceRequest {
    #[prost(string, tag = "1")]
    pub name: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RegisterServiceResponse {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LookupServiceRequest {
    #[prost(string, tag = "1")]
    pub name: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LookupServiceResponse {
    #[prost(string, repeated, tag = "1")]
    pub dids: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct NodeInfoRequest {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FingerTableRange {
    #[prost(string, optional, tag = "1")]
    pub did: ::core::option::Option<::prost::alloc::string::String>,
    #[prost(int64, tag = "2")]
    pub start: i64,
    #[prost(int64, tag = "3")]
    pub end: i64,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Dht {
    #[prost(string, tag = "1")]
    pub did: ::prost::alloc::string::String,
    #[prost(string, repeated, tag = "2")]
    pub successors: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
    #[prost(string, optional, tag = "3")]
    pub predecessor: ::core::option::Option<::prost::alloc::string::String>,
    #[prost(message, repeated, tag = "4")]
    pub finger_table_ranges: ::prost::alloc::vec::Vec<FingerTableRange>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StorageValue {
    #[prost(string, tag = "1")]
    pub did: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub kind: ::prost::alloc::string::String,
    #[prost(string, repeated, tag = "3")]
    pub data: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StorageItem {
    #[prost(string, tag = "1")]
    pub key: ::prost::alloc::string::String,
    #[prost(message, optional, tag = "2")]
    pub value: ::core::option::Option<StorageValue>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Storage {
    #[prost(message, repeated, tag = "1")]
    pub items: ::prost::alloc::vec::Vec<StorageItem>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Swarm {
    #[prost(message, repeated, tag = "1")]
    pub peers: ::prost::alloc::vec::Vec<Peer>,
    #[prost(message, optional, tag = "2")]
    pub dht: ::core::option::Option<Dht>,
    #[prost(message, optional, tag = "3")]
    pub persistence_storage: ::core::option::Option<Storage>,
    #[prost(message, optional, tag = "4")]
    pub cache_storage: ::core::option::Option<Storage>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct NodeInfoResponse {
    #[prost(string, tag = "1")]
    pub version: ::prost::alloc::string::String,
    #[prost(message, optional, tag = "2")]
    pub swarm: ::core::option::Option<Swarm>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct NodeDidRequest {}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct NodeDidResponse {
    #[prost(string, tag = "1")]
    pub did: ::prost::alloc::string::String,
}
/// Generated client implementations.
pub mod rings_node_internal_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::http::Uri;
    use tonic::codegen::*;
    /// Rings node internal service
    #[derive(Debug, Clone)]
    pub struct RingsNodeInternalServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl RingsNodeInternalServiceClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> RingsNodeInternalServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> RingsNodeInternalServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<http::Request<tonic::body::BoxBody>>>::Error:
                Into<StdError> + Send + Sync,
        {
            RingsNodeInternalServiceClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        /// Connect peer via remote peer's http endpoint
        pub async fn connect_peer_via_http(
            &mut self,
            request: impl tonic::IntoRequest<super::ConnectPeerViaHttpRequest>,
        ) -> std::result::Result<tonic::Response<super::ConnectPeerViaHttpResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/ConnectPeerViaHttp",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "ConnectPeerViaHttp",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// Connect peer with remote peer's did
        pub async fn connect_with_did(
            &mut self,
            request: impl tonic::IntoRequest<super::ConnectWithDidRequest>,
        ) -> std::result::Result<tonic::Response<super::ConnectWithDidResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/ConnectWithDid",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "ConnectWithDid",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// Connect peers from a seed file
        pub async fn connect_with_seed(
            &mut self,
            request: impl tonic::IntoRequest<super::ConnectWithSeedRequest>,
        ) -> std::result::Result<tonic::Response<super::ConnectWithSeedResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/ConnectWithSeed",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "ConnectWithSeed",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// List all connected peers
        pub async fn list_peers(
            &mut self,
            request: impl tonic::IntoRequest<super::ListPeersRequest>,
        ) -> std::result::Result<tonic::Response<super::ListPeersResponse>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/ListPeers",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "ListPeers",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// Create offer for manually handshake
        pub async fn create_offer(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateOfferRequest>,
        ) -> std::result::Result<tonic::Response<super::CreateOfferResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/CreateOffer",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "CreateOffer",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// Answer offer for manually handshake
        pub async fn answer_offer(
            &mut self,
            request: impl tonic::IntoRequest<super::AnswerOfferRequest>,
        ) -> std::result::Result<tonic::Response<super::AnswerOfferResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/AnswerOffer",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "AnswerOffer",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// Accept Answer for manually handshake
        pub async fn accept_answer(
            &mut self,
            request: impl tonic::IntoRequest<super::AcceptAnswerRequest>,
        ) -> std::result::Result<tonic::Response<super::AcceptAnswerResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/AcceptAnswer",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "AcceptAnswer",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// Disconnect a peer
        pub async fn disconnect(
            &mut self,
            request: impl tonic::IntoRequest<super::DisconnectRequest>,
        ) -> std::result::Result<tonic::Response<super::DisconnectResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/Disconnect",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "Disconnect",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// Send message to peer
        pub async fn send_message(
            &mut self,
            request: impl tonic::IntoRequest<super::SendMessageRequest>,
        ) -> std::result::Result<tonic::Response<super::SendMessageResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/SendMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "SendMessage",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// / Append data to topic
        pub async fn publish_to_topic(
            &mut self,
            request: impl tonic::IntoRequest<super::PublishToTopicRequest>,
        ) -> std::result::Result<tonic::Response<super::PublishToTopicResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/PublishToTopic",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "PublishToTopic",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// / Fetch data of topic
        pub async fn fetch_topic(
            &mut self,
            request: impl tonic::IntoRequest<super::FetchTopicRequest>,
        ) -> std::result::Result<tonic::Response<super::FetchTopicResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/FetchTopic",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "FetchTopic",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// / Register service
        pub async fn register_service(
            &mut self,
            request: impl tonic::IntoRequest<super::RegisterServiceRequest>,
        ) -> std::result::Result<tonic::Response<super::RegisterServiceResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/RegisterService",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "RegisterService",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// / Lookup service
        pub async fn lookup_service(
            &mut self,
            request: impl tonic::IntoRequest<super::LookupServiceRequest>,
        ) -> std::result::Result<tonic::Response<super::LookupServiceResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/LookupService",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "LookupService",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// / Retrieve Node info
        pub async fn node_info(
            &mut self,
            request: impl tonic::IntoRequest<super::NodeInfoRequest>,
        ) -> std::result::Result<tonic::Response<super::NodeInfoResponse>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/NodeInfo",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "NodeInfo",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// / Retrieve Node DID
        pub async fn node_did(
            &mut self,
            request: impl tonic::IntoRequest<super::NodeDidRequest>,
        ) -> std::result::Result<tonic::Response<super::NodeDidResponse>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeInternalService/NodeDid",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeInternalService",
                "NodeDid",
            ));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod rings_node_external_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::http::Uri;
    use tonic::codegen::*;
    /// Rings node external service
    #[derive(Debug, Clone)]
    pub struct RingsNodeExternalServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl RingsNodeExternalServiceClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> RingsNodeExternalServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> RingsNodeExternalServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<http::Request<tonic::body::BoxBody>>>::Error:
                Into<StdError> + Send + Sync,
        {
            RingsNodeExternalServiceClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        /// Answer offer for manually handshake
        pub async fn answer_offer(
            &mut self,
            request: impl tonic::IntoRequest<super::AnswerOfferRequest>,
        ) -> std::result::Result<tonic::Response<super::AnswerOfferResponse>, tonic::Status>
        {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeExternalService/AnswerOffer",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeExternalService",
                "AnswerOffer",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// / Retrieve Node info
        pub async fn node_info(
            &mut self,
            request: impl tonic::IntoRequest<super::NodeInfoRequest>,
        ) -> std::result::Result<tonic::Response<super::NodeInfoResponse>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeExternalService/NodeInfo",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeExternalService",
                "NodeInfo",
            ));
            self.inner.unary(req, path, codec).await
        }
        /// / Retrieve Node DID
        pub async fn node_did(
            &mut self,
            request: impl tonic::IntoRequest<super::NodeDidRequest>,
        ) -> std::result::Result<tonic::Response<super::NodeDidResponse>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/rings_node.RingsNodeExternalService/NodeDid",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "rings_node.RingsNodeExternalService",
                "NodeDid",
            ));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod rings_node_internal_service_server {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with RingsNodeInternalServiceServer.
    #[async_trait]
    pub trait RingsNodeInternalService: Send + Sync + 'static {
        /// Connect peer via remote peer's http endpoint
        async fn connect_peer_via_http(
            &self,
            request: tonic::Request<super::ConnectPeerViaHttpRequest>,
        ) -> std::result::Result<tonic::Response<super::ConnectPeerViaHttpResponse>, tonic::Status>;
        /// Connect peer with remote peer's did
        async fn connect_with_did(
            &self,
            request: tonic::Request<super::ConnectWithDidRequest>,
        ) -> std::result::Result<tonic::Response<super::ConnectWithDidResponse>, tonic::Status>;
        /// Connect peers from a seed file
        async fn connect_with_seed(
            &self,
            request: tonic::Request<super::ConnectWithSeedRequest>,
        ) -> std::result::Result<tonic::Response<super::ConnectWithSeedResponse>, tonic::Status>;
        /// List all connected peers
        async fn list_peers(
            &self,
            request: tonic::Request<super::ListPeersRequest>,
        ) -> std::result::Result<tonic::Response<super::ListPeersResponse>, tonic::Status>;
        /// Create offer for manually handshake
        async fn create_offer(
            &self,
            request: tonic::Request<super::CreateOfferRequest>,
        ) -> std::result::Result<tonic::Response<super::CreateOfferResponse>, tonic::Status>;
        /// Answer offer for manually handshake
        async fn answer_offer(
            &self,
            request: tonic::Request<super::AnswerOfferRequest>,
        ) -> std::result::Result<tonic::Response<super::AnswerOfferResponse>, tonic::Status>;
        /// Accept Answer for manually handshake
        async fn accept_answer(
            &self,
            request: tonic::Request<super::AcceptAnswerRequest>,
        ) -> std::result::Result<tonic::Response<super::AcceptAnswerResponse>, tonic::Status>;
        /// Disconnect a peer
        async fn disconnect(
            &self,
            request: tonic::Request<super::DisconnectRequest>,
        ) -> std::result::Result<tonic::Response<super::DisconnectResponse>, tonic::Status>;
        /// Send message to peer
        async fn send_message(
            &self,
            request: tonic::Request<super::SendMessageRequest>,
        ) -> std::result::Result<tonic::Response<super::SendMessageResponse>, tonic::Status>;
        /// / Append data to topic
        async fn publish_to_topic(
            &self,
            request: tonic::Request<super::PublishToTopicRequest>,
        ) -> std::result::Result<tonic::Response<super::PublishToTopicResponse>, tonic::Status>;
        /// / Fetch data of topic
        async fn fetch_topic(
            &self,
            request: tonic::Request<super::FetchTopicRequest>,
        ) -> std::result::Result<tonic::Response<super::FetchTopicResponse>, tonic::Status>;
        /// / Register service
        async fn register_service(
            &self,
            request: tonic::Request<super::RegisterServiceRequest>,
        ) -> std::result::Result<tonic::Response<super::RegisterServiceResponse>, tonic::Status>;
        /// / Lookup service
        async fn lookup_service(
            &self,
            request: tonic::Request<super::LookupServiceRequest>,
        ) -> std::result::Result<tonic::Response<super::LookupServiceResponse>, tonic::Status>;
        /// / Retrieve Node info
        async fn node_info(
            &self,
            request: tonic::Request<super::NodeInfoRequest>,
        ) -> std::result::Result<tonic::Response<super::NodeInfoResponse>, tonic::Status>;
        /// / Retrieve Node DID
        async fn node_did(
            &self,
            request: tonic::Request<super::NodeDidRequest>,
        ) -> std::result::Result<tonic::Response<super::NodeDidResponse>, tonic::Status>;
    }
    /// Rings node internal service
    #[derive(Debug)]
    pub struct RingsNodeInternalServiceServer<T: RingsNodeInternalService> {
        inner: _Inner<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    struct _Inner<T>(Arc<T>);
    impl<T: RingsNodeInternalService> RingsNodeInternalServiceServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            let inner = _Inner(inner);
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(inner: T, interceptor: F) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for RingsNodeInternalServiceServer<T>
    where
        T: RingsNodeInternalService,
        B: Body + Send + 'static,
        B::Error: Into<StdError> + Send + 'static,
    {
        type Response = http::Response<tonic::body::BoxBody>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            let inner = self.inner.clone();
            match req.uri().path() {
                "/rings_node.RingsNodeInternalService/ConnectPeerViaHttp" => {
                    #[allow(non_camel_case_types)]
                    struct ConnectPeerViaHttpSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::ConnectPeerViaHttpRequest>
                        for ConnectPeerViaHttpSvc<T>
                    {
                        type Response = super::ConnectPeerViaHttpResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ConnectPeerViaHttpRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::connect_peer_via_http(
                                    &inner, request,
                                )
                                .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ConnectPeerViaHttpSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/ConnectWithDid" => {
                    #[allow(non_camel_case_types)]
                    struct ConnectWithDidSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::ConnectWithDidRequest>
                        for ConnectWithDidSvc<T>
                    {
                        type Response = super::ConnectWithDidResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ConnectWithDidRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::connect_with_did(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ConnectWithDidSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/ConnectWithSeed" => {
                    #[allow(non_camel_case_types)]
                    struct ConnectWithSeedSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::ConnectWithSeedRequest>
                        for ConnectWithSeedSvc<T>
                    {
                        type Response = super::ConnectWithSeedResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ConnectWithSeedRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::connect_with_seed(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ConnectWithSeedSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/ListPeers" => {
                    #[allow(non_camel_case_types)]
                    struct ListPeersSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::ListPeersRequest> for ListPeersSvc<T>
                    {
                        type Response = super::ListPeersResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListPeersRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::list_peers(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ListPeersSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/CreateOffer" => {
                    #[allow(non_camel_case_types)]
                    struct CreateOfferSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::CreateOfferRequest>
                        for CreateOfferSvc<T>
                    {
                        type Response = super::CreateOfferResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CreateOfferRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::create_offer(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = CreateOfferSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/AnswerOffer" => {
                    #[allow(non_camel_case_types)]
                    struct AnswerOfferSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::AnswerOfferRequest>
                        for AnswerOfferSvc<T>
                    {
                        type Response = super::AnswerOfferResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::AnswerOfferRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::answer_offer(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = AnswerOfferSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/AcceptAnswer" => {
                    #[allow(non_camel_case_types)]
                    struct AcceptAnswerSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::AcceptAnswerRequest>
                        for AcceptAnswerSvc<T>
                    {
                        type Response = super::AcceptAnswerResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::AcceptAnswerRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::accept_answer(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = AcceptAnswerSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/Disconnect" => {
                    #[allow(non_camel_case_types)]
                    struct DisconnectSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::DisconnectRequest> for DisconnectSvc<T>
                    {
                        type Response = super::DisconnectResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::DisconnectRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::disconnect(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = DisconnectSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/SendMessage" => {
                    #[allow(non_camel_case_types)]
                    struct SendMessageSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::SendMessageRequest>
                        for SendMessageSvc<T>
                    {
                        type Response = super::SendMessageResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::SendMessageRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::send_message(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = SendMessageSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/PublishToTopic" => {
                    #[allow(non_camel_case_types)]
                    struct PublishToTopicSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::PublishToTopicRequest>
                        for PublishToTopicSvc<T>
                    {
                        type Response = super::PublishToTopicResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::PublishToTopicRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::publish_to_topic(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = PublishToTopicSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/FetchTopic" => {
                    #[allow(non_camel_case_types)]
                    struct FetchTopicSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::FetchTopicRequest> for FetchTopicSvc<T>
                    {
                        type Response = super::FetchTopicResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::FetchTopicRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::fetch_topic(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = FetchTopicSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/RegisterService" => {
                    #[allow(non_camel_case_types)]
                    struct RegisterServiceSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::RegisterServiceRequest>
                        for RegisterServiceSvc<T>
                    {
                        type Response = super::RegisterServiceResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RegisterServiceRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::register_service(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = RegisterServiceSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/LookupService" => {
                    #[allow(non_camel_case_types)]
                    struct LookupServiceSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::LookupServiceRequest>
                        for LookupServiceSvc<T>
                    {
                        type Response = super::LookupServiceResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::LookupServiceRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::lookup_service(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = LookupServiceSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/NodeInfo" => {
                    #[allow(non_camel_case_types)]
                    struct NodeInfoSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::NodeInfoRequest> for NodeInfoSvc<T>
                    {
                        type Response = super::NodeInfoResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::NodeInfoRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::node_info(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = NodeInfoSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeInternalService/NodeDid" => {
                    #[allow(non_camel_case_types)]
                    struct NodeDidSvc<T: RingsNodeInternalService>(pub Arc<T>);
                    impl<T: RingsNodeInternalService>
                        tonic::server::UnaryService<super::NodeDidRequest> for NodeDidSvc<T>
                    {
                        type Response = super::NodeDidResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::NodeDidRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeInternalService>::node_did(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = NodeDidSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => Box::pin(async move {
                    Ok(http::Response::builder()
                        .status(200)
                        .header("grpc-status", "12")
                        .header("content-type", "application/grpc")
                        .body(empty_body())
                        .unwrap())
                }),
            }
        }
    }
    impl<T: RingsNodeInternalService> Clone for RingsNodeInternalServiceServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    impl<T: RingsNodeInternalService> Clone for _Inner<T> {
        fn clone(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
    }
    impl<T: std::fmt::Debug> std::fmt::Debug for _Inner<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}", self.0)
        }
    }
    impl<T: RingsNodeInternalService> tonic::server::NamedService
        for RingsNodeInternalServiceServer<T>
    {
        const NAME: &'static str = "rings_node.RingsNodeInternalService";
    }
}
/// Generated server implementations.
pub mod rings_node_external_service_server {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with RingsNodeExternalServiceServer.
    #[async_trait]
    pub trait RingsNodeExternalService: Send + Sync + 'static {
        /// Answer offer for manually handshake
        async fn answer_offer(
            &self,
            request: tonic::Request<super::AnswerOfferRequest>,
        ) -> std::result::Result<tonic::Response<super::AnswerOfferResponse>, tonic::Status>;
        /// / Retrieve Node info
        async fn node_info(
            &self,
            request: tonic::Request<super::NodeInfoRequest>,
        ) -> std::result::Result<tonic::Response<super::NodeInfoResponse>, tonic::Status>;
        /// / Retrieve Node DID
        async fn node_did(
            &self,
            request: tonic::Request<super::NodeDidRequest>,
        ) -> std::result::Result<tonic::Response<super::NodeDidResponse>, tonic::Status>;
    }
    /// Rings node external service
    #[derive(Debug)]
    pub struct RingsNodeExternalServiceServer<T: RingsNodeExternalService> {
        inner: _Inner<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    struct _Inner<T>(Arc<T>);
    impl<T: RingsNodeExternalService> RingsNodeExternalServiceServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            let inner = _Inner(inner);
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(inner: T, interceptor: F) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for RingsNodeExternalServiceServer<T>
    where
        T: RingsNodeExternalService,
        B: Body + Send + 'static,
        B::Error: Into<StdError> + Send + 'static,
    {
        type Response = http::Response<tonic::body::BoxBody>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            let inner = self.inner.clone();
            match req.uri().path() {
                "/rings_node.RingsNodeExternalService/AnswerOffer" => {
                    #[allow(non_camel_case_types)]
                    struct AnswerOfferSvc<T: RingsNodeExternalService>(pub Arc<T>);
                    impl<T: RingsNodeExternalService>
                        tonic::server::UnaryService<super::AnswerOfferRequest>
                        for AnswerOfferSvc<T>
                    {
                        type Response = super::AnswerOfferResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::AnswerOfferRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeExternalService>::answer_offer(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = AnswerOfferSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeExternalService/NodeInfo" => {
                    #[allow(non_camel_case_types)]
                    struct NodeInfoSvc<T: RingsNodeExternalService>(pub Arc<T>);
                    impl<T: RingsNodeExternalService>
                        tonic::server::UnaryService<super::NodeInfoRequest> for NodeInfoSvc<T>
                    {
                        type Response = super::NodeInfoResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::NodeInfoRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeExternalService>::node_info(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = NodeInfoSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/rings_node.RingsNodeExternalService/NodeDid" => {
                    #[allow(non_camel_case_types)]
                    struct NodeDidSvc<T: RingsNodeExternalService>(pub Arc<T>);
                    impl<T: RingsNodeExternalService>
                        tonic::server::UnaryService<super::NodeDidRequest> for NodeDidSvc<T>
                    {
                        type Response = super::NodeDidResponse;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::NodeDidRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RingsNodeExternalService>::node_did(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = NodeDidSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => Box::pin(async move {
                    Ok(http::Response::builder()
                        .status(200)
                        .header("grpc-status", "12")
                        .header("content-type", "application/grpc")
                        .body(empty_body())
                        .unwrap())
                }),
            }
        }
    }
    impl<T: RingsNodeExternalService> Clone for RingsNodeExternalServiceServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    impl<T: RingsNodeExternalService> Clone for _Inner<T> {
        fn clone(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
    }
    impl<T: std::fmt::Debug> std::fmt::Debug for _Inner<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}", self.0)
        }
    }
    impl<T: RingsNodeExternalService> tonic::server::NamedService
        for RingsNodeExternalServiceServer<T>
    {
        const NAME: &'static str = "rings_node.RingsNodeExternalService";
    }
}
