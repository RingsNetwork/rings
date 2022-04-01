use bns_core::{
    ecc::SecretKey,
    message::Encoded,
    //message::{ConnectNode, Encoded, Message, MessageRelay, MessageRelayMethod},
    swarm::{Swarm, TransportManager},
    transports::default::DefaultTransport,
    types::ice_transport::{IceTransport, IceTrickleScheme},
};
use serde::de::DeserializeOwned;
use std::str::FromStr;
use std::sync::Arc;
use web3::types::Address;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

use crate::grpc::response::{Peer, TransportAndHsInfo};

use super::{
    grpc_client::GrpcClient,
    grpc_server::Grpc,
    request::{
        AcceptAnswer, ConnectWithHandshakeInfo, ConnectWithUrl, Disconnect, ListPeers, RpcMethod,
        SendTo, DEFAULT_VERSION,
    },
    GrpcRequest, GrpcResponse,
};

impl GrpcClient<tonic::transport::Channel> {
    pub async fn connect_with_handshake_info(
        &mut self,
        handshake_info: &str,
    ) -> anyhow::Result<String> {
        let resp = self
            .grpc(ConnectWithHandshakeInfo::new(handshake_info))
            .await
            .map_err(|e| anyhow::anyhow!("connect to node failed, {}", e.message()))?;
        let transport_hs: TransportAndHsInfo = resp
            .get_ref()
            .as_json_result()
            .map_err(|e| anyhow::anyhow!("invalid handshake info: {}", e))?;
        Ok(transport_hs.handshake_info)
    }
}

// #[derive(Clone)]
pub struct BnsGrpc {
    pub swarm: Arc<Swarm>,
    pub key: SecretKey,
}

#[tonic::async_trait]
impl Grpc for BnsGrpc {
    async fn grpc(
        &self,
        request: tonic::Request<GrpcRequest>,
    ) -> Result<tonic::Response<GrpcResponse>, tonic::Status> {
        let method = request.get_ref().method.to_owned();
        let method: RpcMethod = method
            .try_into()
            .map_err(|e| tonic::Status::new(tonic::Code::NotFound, format!("{}", e)))?;
        let result = match method {
            RpcMethod::ConnectWithUrl => {
                let params = ConnectWithUrl::try_from(&request).map_err(|e: anyhow::Error| {
                    tonic::Status::new(tonic::Code::InvalidArgument, e.to_string())
                })?;
                self.trickle_forward(&params).await
            }
            RpcMethod::ListPeers => {
                let params = ListPeers::try_from(&request).map_err(|e: anyhow::Error| {
                    tonic::Status::new(tonic::Code::InvalidArgument, e.to_string())
                })?;
                self.list_peers(&params).await
            }
            RpcMethod::CreateOffer => self.create_offer().await,
            RpcMethod::ConnectWithHandshakeInfo => {
                let params =
                    ConnectWithHandshakeInfo::try_from(&request).map_err(|e: anyhow::Error| {
                        tonic::Status::new(tonic::Code::InvalidArgument, e.to_string())
                    })?;
                self.connect_with_handshake_info(&params).await
            }
            RpcMethod::Disconnect => {
                let params = Disconnect::try_from(&request).map_err(|e: anyhow::Error| {
                    tonic::Status::new(tonic::Code::InvalidArgument, e.to_string())
                })?;
                self.disconnect(&params).await
            }
            RpcMethod::SendTo => {
                let params = SendTo::try_from(&request).map_err(|e: anyhow::Error| {
                    tonic::Status::new(tonic::Code::InvalidArgument, e.to_string())
                })?;
                self.send_to(&params).await
            }
            RpcMethod::AcceptAnswer => {
                let params = AcceptAnswer::try_from(&request).map_err(|e: anyhow::Error| {
                    tonic::Status::new(tonic::Code::InvalidArgument, e.to_string())
                })?;
                self.accept_answer(&params).await
            }
        }
        .map_err(|e| tonic::Status::new(tonic::Code::Internal, format!("{}", e)))?;

        Ok(tonic::Response::new(GrpcResponse {
            version: DEFAULT_VERSION.to_owned(),
            id: request.get_ref().id,
            result: base64::encode(result),
        }))
    }
}

impl BnsGrpc {
    pub async fn create_offer(&self) -> anyhow::Result<Vec<u8>> {
        let transport = self.swarm.new_transport().await?;
        let id = transport.id;
        let task = async move {
            let hs_info = transport
                .get_handshake_info(self.key, RTCSdpType::Offer)
                .await
                .map_err(|e| anyhow::anyhow!(e))?
                .to_string();
            self.swarm.push_pending_transport(&transport)?;
            Ok(hs_info)
        };
        let hs_info = match task.await {
            Ok(hs_info) => TransportAndHsInfo::new(id.to_string().as_str(), hs_info.as_str()),
            Err(e) => return Err(e),
        };
        hs_info.to_json_vec()
    }

    /// trickle_forward
    pub async fn trickle_forward(&self, params: &ConnectWithUrl) -> anyhow::Result<Vec<u8>> {
        // request remote offer and sand answer to remote
        log::debug!("node_url: {}", params.url);
        let transport = self.swarm.new_transport().await?;
        let hs_info = self.do_trickle_foraward(&transport, &params.url).await;
        if let Err(e) = hs_info {
            transport.close().await?;
            return Err(e);
        }
        let hs_info = hs_info.unwrap();
        Ok(hs_info.as_bytes().to_vec())
    }

    async fn do_trickle_foraward(
        &self,
        transport: &Arc<DefaultTransport>,
        node_url: &str,
    ) -> anyhow::Result<String> {
        let channel =
            tonic::transport::Channel::builder(node_url.parse::<tonic::transport::Uri>()?);
        let mut client = GrpcClient::new(channel.connect().await?);
        let hs_info = transport
            .get_handshake_info(self.key, RTCSdpType::Offer)
            .await?
            .to_string();
        log::debug!(
            "sending offer and candidate {:?} to {:?}",
            hs_info.to_owned(),
            node_url,
        );
        let remote_hs_info = client.connect_with_handshake_info(hs_info.as_str()).await?;
        let addr = transport
            .register_remote_info(Encoded::from_encoded_str(remote_hs_info.as_str()))
            .await?;
        self.swarm.register(&addr, Arc::clone(transport)).await?;
        Ok(addr.to_string())
    }

    async fn connect_with_handshake_info(
        &self,
        params: &ConnectWithHandshakeInfo,
    ) -> anyhow::Result<Vec<u8>> {
        log::debug!("handshake: {}", params.handshake_info);
        let transport = self.swarm.new_transport().await.map_err(|e| {
            log::error!("new_transport failed: {}", e);
            anyhow::anyhow!(e)
        })?;
        match self
            .handshake(&transport, params.handshake_info.as_str())
            .await
        {
            Ok(v) => Ok(
                TransportAndHsInfo::new(transport.id.to_string().as_str(), v.as_str())
                    .to_json_vec()?,
            ),
            Err(e) => {
                transport.close().await?;
                Err(e)
            }
        }
    }

    async fn handshake(
        &self,
        transport: &Arc<DefaultTransport>,
        data: &str,
    ) -> anyhow::Result<String> {
        // get offer from remote and send answer back
        let hs_info = Encoded::from_encoded_str(data);
        let addr = transport
            .register_remote_info(hs_info.to_owned())
            .await
            .map_err(|e| anyhow::anyhow!("failed to register {}", e))?;

        log::debug!("register: {}", addr);
        self.swarm.register(&addr, Arc::clone(transport)).await?;

        let hs_info = transport
            .get_handshake_info(self.key, RTCSdpType::Answer)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get handshake info: {}", e))?
            .to_string();
        log::debug!("answer hs_info: {}", hs_info);
        Ok(hs_info)
    }

    async fn accept_answer(&self, params: &AcceptAnswer) -> anyhow::Result<Vec<u8>> {
        let hs_info = Encoded::from_encoded_str(params.handshake_info.as_str());
        log::debug!(
            "accept_answer/hs_info: {:?}, uuid: {}",
            hs_info,
            params.transport_id
        );
        let transport_id = uuid::Uuid::from_str(params.transport_id.as_str())
            .map_err(|_| anyhow::anyhow!("invalid transport_id"))?;
        let transport = self
            .swarm
            .find_pending_transport(transport_id)?
            .ok_or_else(|| anyhow::anyhow!("Pending transport not found"))?;
        let addr = transport.register_remote_info(hs_info).await?;
        self.swarm.register(&addr, transport.clone()).await?;
        if let Err(e) = self.swarm.pop_pending_transport(transport.id) {
            log::warn!("pop_pending_transport err: {}", e)
        };
        Peer::from((addr, transport)).to_json_vec()
    }

    async fn list_peers(&self, _params: &ListPeers) -> anyhow::Result<Vec<u8>> {
        let transports = self.swarm.get_transports();
        log::debug!(
            "addresses: {:?}",
            transports.iter().map(|(a, _b)| a).collect::<Vec<_>>()
        );
        let data = transports.iter().map(|x| x.into()).collect::<Vec<Peer>>();
        serde_json::to_vec(&data).map_err(|e| anyhow::anyhow!(e))
    }

    async fn send_to(&self, params: &SendTo) -> anyhow::Result<Vec<u8>> {
        let to_address = Address::from_str(params.to_address.as_str())?;
        //let transport = self.swarm.get_transport(&to_address).ok_or_else(|| anyhow::anyhow!("Transport not found"))?;
        log::debug!("to_address: {}", to_address);
        // TODO support send message to address
        // let payload = MessageRelay::new(Message::None, &self.key, None, None, None, MessageRelayMethod::SEND)?;
        //self.swarm.send_message(&to_address, payload).await?;
        Ok(vec![])
    }

    async fn disconnect(&self, params: &Disconnect) -> anyhow::Result<Vec<u8>> {
        let address = Address::from_str(params.address.as_str())?;
        let transport = self
            .swarm
            .get_transport(&address)
            .ok_or_else(|| anyhow::anyhow!("Transport not found"))?;
        transport.close().await?;
        Ok(vec![])
    }
}

impl GrpcResponse {
    pub fn as_text_result(&self) -> anyhow::Result<String> {
        if self.result.is_empty() {
            return Ok("".to_owned());
        }
        String::from_utf8(base64::decode(self.result.as_bytes()).map_err(|e| anyhow::anyhow!(e))?)
            .map_err(|e| e.into())
    }

    pub fn as_json_result<T>(&self) -> anyhow::Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_slice(&base64::decode(&self.result).map_err(|e| anyhow::anyhow!(e))?)
            .map_err(|e| {
                log::error!("decode json failed: {}", e);
                e.into()
            })
    }
}

impl From<GrpcRequest> for tonic::Request<GrpcRequest> {
    fn from(val: GrpcRequest) -> Self {
        tonic::Request::new(val)
    }
}
