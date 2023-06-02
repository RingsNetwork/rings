#![warn(missing_docs)]
//! A jsonrpc-server of rings-node
/// [JSON-RPC]: https://www.jsonrpc.org/specification
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use futures::future::join_all;
use tokio::sync::broadcast::Receiver;
use tokio::sync::Mutex;

use crate::backend::types::BackendMessage;
use crate::backend::MessageType;
use crate::error::Error as ServerError;
use crate::prelude::jsonrpc_core::Error;
use crate::prelude::jsonrpc_core::ErrorCode;
use crate::prelude::jsonrpc_core::MetaIoHandler;
use crate::prelude::jsonrpc_core::Metadata;
use crate::prelude::jsonrpc_core::Params;
use crate::prelude::jsonrpc_core::Result;
use crate::prelude::jsonrpc_core::Value;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::message::Encoder;
use crate::prelude::rings_core::prelude::vnode::VirtualNode;
use crate::prelude::rings_core::transports::manager::TransportHandshake;
use crate::prelude::rings_core::transports::manager::TransportManager;
use crate::prelude::rings_core::types::ice_transport::IceTransportInterface;
use crate::prelude::rings_core::utils::from_rtc_ice_connection_state;
use crate::prelude::rings_rpc;
use crate::prelude::rings_rpc::method::Method;
use crate::prelude::rings_rpc::response;
use crate::prelude::rings_rpc::response::CustomBackendMessage;
use crate::prelude::rings_rpc::response::Peer;
use crate::prelude::rings_rpc::types::HttpRequest;
use crate::processor;
use crate::processor::Processor;
use crate::seed::Seed;

/// RpcMeta basic info struct
/// * processor: contain `swarm` instance and `stabilization` instance.
/// * is_auth: is_auth set true after verify.
#[derive(Clone)]
pub struct RpcMeta {
    processor: Arc<Processor>,
    receiver: Arc<Mutex<Receiver<BackendMessage>>>,
    is_auth: bool,
}

impl RpcMeta {
    fn require_authed(&self) -> Result<()> {
        if !self.is_auth {
            return Err(Error::from(ServerError::NoPermission));
        }
        Ok(())
    }
}

/// `MetaIoHandler<T>, T: Metadata`
impl Metadata for RpcMeta {}

impl From<(Arc<Processor>, Arc<Mutex<Receiver<BackendMessage>>>, bool)> for RpcMeta {
    fn from(
        (processor, receiver, is_auth): (
            Arc<Processor>,
            Arc<Mutex<Receiver<BackendMessage>>>,
            bool,
        ),
    ) -> Self {
        Self {
            processor,
            receiver,
            is_auth,
        }
    }
}

/// Build handler add method with metadata.
pub(crate) async fn build_handler(handler: &mut MetaIoHandler<RpcMeta>) {
    handler.add_method_with_meta(Method::ConnectPeerViaHttp.as_str(), connect_peer_via_http);
    handler.add_method_with_meta(Method::ConnectWithSeed.as_str(), connect_with_seed);
    handler.add_method_with_meta(Method::AnswerOffer.as_str(), answer_offer);
    handler.add_method_with_meta(Method::ConnectWithDid.as_str(), connect_with_did);
    handler.add_method_with_meta(Method::CreateOffer.as_str(), create_offer);
    handler.add_method_with_meta(Method::AcceptAnswer.as_str(), accept_answer);
    handler.add_method_with_meta(Method::ListPeers.as_str(), list_peers);
    handler.add_method_with_meta(Method::Disconnect.as_str(), close_connection);
    handler.add_method_with_meta(Method::ListPendings.as_str(), list_pendings);
    handler.add_method_with_meta(
        Method::ClosePendingTransport.as_str(),
        close_pending_transport,
    );
    handler.add_method_with_meta(Method::SendTo.as_str(), send_raw_message);
    handler.add_method_with_meta(
        Method::SendHttpRequestMessage.as_str(),
        send_http_request_message,
    );
    handler.add_method_with_meta(Method::SendSimpleText.as_str(), send_simple_text_message);
    handler.add_method_with_meta(Method::SendCustomMessage.as_str(), send_custom_message);
    handler.add_method_with_meta(
        Method::PublishMessageToTopic.as_str(),
        publish_message_to_topic,
    );
    handler.add_method_with_meta(
        Method::FetchMessagesOfTopic.as_str(),
        fetch_messages_of_topic,
    );
    handler.add_method_with_meta(Method::RegisterService.as_str(), register_service);
    handler.add_method_with_meta(Method::LookupService.as_str(), lookup_service);
    handler.add_method_with_meta(Method::PollMessage.as_str(), poll_message);
    handler.add_method_with_meta(Method::NodeInfo.as_str(), node_info);
}

async fn node_info(_: Params, meta: RpcMeta) -> Result<Value> {
    let node_info = meta
        .processor
        .get_node_info()
        .await
        .map_err(|_| Error::new(ErrorCode::InternalError))?;
    serde_json::to_value(node_info).map_err(|_| Error::new(ErrorCode::ParseError))
}

/// Connect Peer VIA http
async fn connect_peer_via_http(params: Params, meta: RpcMeta) -> Result<Value> {
    let p: Vec<String> = params.parse()?;
    let peer_url = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let peer = meta
        .processor
        .connect_peer_via_http(peer_url)
        .await
        .map_err(Error::from)?;
    Ok(Value::String(peer.transport.id.to_string()))
}

/// Connect Peer with seed
async fn connect_with_seed(params: Params, meta: RpcMeta) -> Result<Value> {
    let p: Vec<Seed> = params.parse()?;
    let seed = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let mut connected_addresses: HashSet<Did> = HashSet::from_iter(meta.processor.swarm.get_dids());
    connected_addresses.insert(meta.processor.swarm.did());

    let tasks = seed
        .peers
        .iter()
        .filter(|&x| !connected_addresses.contains(&x.did))
        .map(|x| meta.processor.connect_peer_via_http(&x.endpoint));

    let results = join_all(tasks).await;

    let first_err = results.into_iter().find(|x| x.is_err());
    if let Some(err) = first_err {
        err.map_err(Error::from)?;
    }

    Ok(Value::Null)
}

/// Handle Connect with DID
async fn connect_with_did(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let p: Vec<String> = params.parse()?;
    let address_str = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    meta.processor
        .connect_with_did(
            Did::from_str(address_str).map_err(|_| Error::new(ErrorCode::InvalidParams))?,
            true,
        )
        .await
        .map_err(Error::from)?;
    Ok(Value::Null)
}

/// Handle create offer
async fn create_offer(_params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let (_, offer_payload) = meta
        .processor
        .swarm
        .create_offer()
        .await
        .map_err(ServerError::CreateOffer)
        .map_err(Error::from)?;

    serde_json::to_value(offer_payload)
        .map_err(ServerError::SerdeJsonError)
        .map_err(Error::from)
}

/// Handle Answer Offer
async fn answer_offer(params: Params, meta: RpcMeta) -> Result<Value> {
    let p: Vec<String> = params.parse()?;
    let offer_payload_str = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let offer_payload = serde_json::from_str(offer_payload_str)
        .map_err(|_| Error::new(ErrorCode::InvalidParams))?;

    let (_, answer_payload) = meta
        .processor
        .swarm
        .answer_offer(offer_payload)
        .await
        .map_err(ServerError::AnswerOffer)
        .map_err(Error::from)?;

    tracing::debug!("connect_peer_via_ice response: {:?}", answer_payload);
    serde_json::to_value(answer_payload)
        .map_err(ServerError::SerdeJsonError)
        .map_err(Error::from)
}

/// Handle accept answer
async fn accept_answer(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;

    let p: Vec<String> = params.parse()?;
    let answer_payload_str = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let answer_payload = serde_json::from_str(answer_payload_str)
        .map_err(|_| Error::new(ErrorCode::InvalidParams))?;

    let p: processor::Peer = meta
        .processor
        .swarm
        .accept_answer(answer_payload)
        .await
        .map_err(ServerError::AcceptAnswer)
        .map_err(Error::from)?
        .into();

    let state = p.transport.ice_connection_state().await;
    let r: Peer = p.into_response_peer(state.map(from_rtc_ice_connection_state));
    r.to_json_obj()
        .map_err(|_| ServerError::EncodeError)
        .map_err(Error::from)
}

/// Handle list peers
async fn list_peers(_params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let peers = meta.processor.list_peers().await?;
    let states_async = peers
        .iter()
        .map(|x| x.transport.ice_connection_state())
        .collect::<Vec<_>>();
    let states = futures::future::join_all(states_async).await;
    let r: Vec<Peer> = peers
        .iter()
        .zip(states.iter())
        .map(|(x, y)| x.into_response_peer(y.map(from_rtc_ice_connection_state)))
        .collect::<Vec<_>>();
    serde_json::to_value(r).map_err(|_| Error::from(ServerError::EncodeError))
}

/// Handle close connection
async fn close_connection(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<String> = params.parse()?;
    let did = params
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let did = Did::from_str(did).map_err(|_| Error::from(ServerError::InvalidDid))?;
    meta.processor.disconnect(did).await?;
    Ok(serde_json::json!({}))
}

/// Handle list pendings
async fn list_pendings(_params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let transports = meta.processor.list_pendings().await?;
    let states_async = transports
        .iter()
        .map(|x| x.ice_connection_state())
        .collect::<Vec<_>>();
    let states = futures::future::join_all(states_async).await;
    let r: Vec<response::TransportInfo> = transports
        .iter()
        .zip(states.iter())
        .map(|(x, y)| response::TransportInfo::from((x, y.map(from_rtc_ice_connection_state))))
        .collect::<Vec<_>>();
    serde_json::to_value(r).map_err(|_| Error::from(ServerError::EncodeError))
}

/// Handle close pending transport
async fn close_pending_transport(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<String> = params.parse()?;
    let transport_id = params
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    meta.processor
        .close_pending_transport(transport_id.as_str())
        .await?;
    Ok(serde_json::json!({}))
}

/// Handle send message
async fn send_raw_message(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: serde_json::Map<String, Value> = params.parse()?;
    let destination = params
        .get("destination")
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let text = params
        .get("text")
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let tx_id = meta
        .processor
        .send_message(destination, text.as_bytes())
        .await?;
    Ok(
        serde_json::to_value(rings_rpc::response::SendMessageResponse::from(
            tx_id.to_string(),
        ))
        .unwrap(),
    )
}

/// send custom message to specifice destination
/// * Params
///   - destination:  destination did
///   - message_type: u16
///   - data: base64 of [u8]
async fn send_custom_message(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<serde_json::Value> = params.parse()?;
    let destination = params
        .get(0)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let message_type: u16 = params
        .get(1)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_u64()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .try_into()
        .map_err(|_| Error::new(ErrorCode::InvalidParams))?;

    let data = params
        .get(2)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let data = base64::decode(data).map_err(|_| Error::new(ErrorCode::InvalidParams))?;

    let msg: BackendMessage = BackendMessage::from((message_type, data.as_ref()));
    let msg: Vec<u8> = msg.into();
    let tx_id = meta.processor.send_message(destination, &msg).await?;

    Ok(
        serde_json::to_value(rings_rpc::response::SendMessageResponse::from(
            tx_id.to_string(),
        ))
        .unwrap(),
    )
}

async fn send_simple_text_message(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<serde_json::Value> = params.parse()?;
    let destination = params
        .get(0)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let text = params
        .get(1)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let msg: BackendMessage =
        BackendMessage::from((MessageType::SimpleText.into(), text.as_bytes()));
    let msg: Vec<u8> = msg.into();
    // TODO chunk message flag
    let tx_id = meta.processor.send_message(destination, &msg).await?;

    Ok(
        serde_json::to_value(rings_rpc::response::SendMessageResponse::from(
            tx_id.to_string(),
        ))
        .unwrap(),
    )
}

/// handle send http request message
async fn send_http_request_message(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<serde_json::Value> = params.parse()?;
    let destination = params
        .get(0)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let p2 = params
        .get(1)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .to_owned();
    let http_request: HttpRequest =
        serde_json::from_value(p2).map_err(|_| Error::new(ErrorCode::InvalidParams))?;

    let msg: BackendMessage = (MessageType::HttpRequest, &http_request).try_into()?;
    let msg: Vec<u8> = msg.into();
    // TODO chunk message flag
    let tx_id = meta.processor.send_message(destination, &msg).await?;

    Ok(
        serde_json::to_value(rings_rpc::response::SendMessageResponse::from(
            tx_id.to_string(),
        ))
        .unwrap(),
    )
}

async fn publish_message_to_topic(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<serde_json::Value> = params.parse()?;
    let topic = params
        .get(0)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let data = params
        .get(1)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .to_string()
        .encode()
        .map_err(|_| Error::new(ErrorCode::InvalidParams))?;

    meta.processor.storage_append_data(topic, data).await?;

    Ok(serde_json::json!({}))
}

async fn fetch_messages_of_topic(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<serde_json::Value> = params.parse()?;
    let topic = params
        .get(0)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let index = params
        .get(1)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_i64()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let vid = VirtualNode::gen_did(topic).map_err(|_| Error::new(ErrorCode::InvalidParams))?;

    meta.processor.storage_fetch(vid).await?;
    let result = meta.processor.storage_check_cache(vid).await;

    if let Some(vnode) = result {
        let messages = vnode
            .data
            .iter()
            .skip(index as usize)
            .map(|v| v.decode())
            .filter_map(|v| v.ok())
            .collect::<Vec<String>>();
        Ok(serde_json::json!(messages))
    } else {
        Ok(serde_json::json!(Vec::<String>::new()))
    }
}

async fn register_service(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<serde_json::Value> = params.parse()?;
    let name = params
        .get(0)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    meta.processor.register_service(name).await?;
    Ok(serde_json::json!({}))
}

async fn lookup_service(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<serde_json::Value> = params.parse()?;
    let name = params
        .get(0)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let rid = VirtualNode::gen_did(name).map_err(|_| Error::new(ErrorCode::InvalidParams))?;

    meta.processor.storage_fetch(rid).await?;
    let result = meta.processor.storage_check_cache(rid).await;

    if let Some(vnode) = result {
        let dids = vnode
            .data
            .iter()
            .map(|v| v.decode())
            .filter_map(|v| v.ok())
            .collect::<Vec<String>>();
        Ok(serde_json::json!(dids))
    } else {
        Ok(serde_json::json!(Vec::<String>::new()))
    }
}

async fn poll_message(params: Params, meta: RpcMeta) -> Result<Value> {
    let params: Vec<serde_json::Value> = params.parse()?;
    let wait_recv = params
        .get(0)
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false);
    let message = if wait_recv {
        let mut recv = meta.receiver.lock().await;
        recv.recv().await.ok()
    } else {
        let mut recv = meta.receiver.lock().await;
        recv.try_recv().ok()
    };

    let message = if let Some(msg) = message {
        serde_json::to_value(CustomBackendMessage::from(msg))
            .map_err(|_| Error::from(ServerError::EncodeError))?
    } else {
        serde_json::Value::Null
    };
    Ok(serde_json::json!({
      "message": message,
    }))
}
