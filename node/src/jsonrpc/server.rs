#![warn(missing_docs)]
//! A jsonrpc-server of rings-node
/// [JSON-RPC]: https://www.jsonrpc.org/specification
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use futures::future::join_all;
use rings_core::swarm::impls::ConnectionHandshake;
use rings_transport::core::transport::ConnectionInterface;
use serde_json::Value;

use crate::backend::types::BackendMessage;
use crate::error::Error as ServerError;
use crate::prelude::jsonrpc_core::Error;
use crate::prelude::jsonrpc_core::ErrorCode;
use crate::prelude::jsonrpc_core::Params;
use crate::prelude::jsonrpc_core::Result;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::message::Decoder;
use crate::prelude::rings_core::message::Encoded;
use crate::prelude::rings_core::message::Encoder;
use crate::prelude::rings_core::message::MessagePayload;
use crate::prelude::rings_core::prelude::vnode::VirtualNode;
use crate::prelude::rings_rpc;
use crate::prelude::rings_rpc::response::Peer;
use crate::processor::Processor;
use crate::seed::Seed;

/// RpcMeta basic info struct
/// * processor: contain `swarm` instance and `stabilization` instance.
/// * is_auth: is_auth set true after verify.
#[derive(Clone)]
pub struct RpcMeta {
    processor: Arc<Processor>,
    /// if is_auth set to true, rpc server of *native node* will check signature from
    /// HEAD['X-SIGNATURE']
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

impl From<(Arc<Processor>, bool)> for RpcMeta {
    fn from((processor, is_auth): (Arc<Processor>, bool)) -> Self {
        Self { processor, is_auth }
    }
}

impl From<Arc<Processor>> for RpcMeta {
    fn from(processor: Arc<Processor>) -> Self {
        Self {
            processor,
            is_auth: true,
        }
    }
}

/// Params for method `BackendMessage`
pub struct BackendMessageParams {
    /// destination did
    pub did: Did,
    /// data of backend message
    pub data: BackendMessage,
}

impl TryFrom<Params> for BackendMessageParams {
    type Error = Error;
    fn try_from(params: Params) -> Result<Self> {
        let params: Vec<serde_json::Value> = params.parse()?;
        let did = params
            .get(0)
            .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
            .as_str()
            .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
        let did = Did::from_str(did).map_err(|_| Error::new(ErrorCode::InvalidParams))?;

        let data = params
            .get(1)
            .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
            .as_str()
            .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
        let data: BackendMessage =
            serde_json::from_str(data).map_err(|_| Error::new(ErrorCode::InvalidParams))?;
        Ok(Self { did, data })
    }
}

impl TryInto<Params> for BackendMessageParams {
    type Error = Error;
    fn try_into(self) -> Result<Params> {
        let data: String =
            serde_json::to_string(&self.data).map_err(|_| Error::new(ErrorCode::InvalidParams))?;
        Ok(Params::Array(vec![
            serde_json::Value::String(self.did.to_string()),
            serde_json::Value::String(data),
        ]))
    }
}

pub(crate) async fn node_info(_: Params, meta: RpcMeta) -> Result<Value> {
    let node_info = meta
        .processor
        .get_node_info()
        .await
        .map_err(|_| Error::new(ErrorCode::InternalError))?;
    serde_json::to_value(node_info).map_err(|_| Error::new(ErrorCode::ParseError))
}

pub(crate) async fn node_did(_: Params, meta: RpcMeta) -> Result<Value> {
    let did = meta.processor.did();
    serde_json::to_value(did).map_err(|_| Error::new(ErrorCode::ParseError))
}

/// Connect Peer VIA http
pub(crate) async fn connect_peer_via_http(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let p: Vec<String> = params.parse()?;
    let peer_url = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let did = meta
        .processor
        .connect_peer_via_http(peer_url)
        .await
        .map_err(Error::from)?;
    Ok(Value::String(did.to_string()))
}

/// Connect Peer with seed
pub(crate) async fn connect_with_seed(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let p: Vec<Seed> = params.parse()?;
    let seed = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let mut connected_addresses: HashSet<Did> =
        HashSet::from_iter(meta.processor.swarm.get_connection_ids());
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
pub(crate) async fn connect_with_did(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let p: Vec<String> = params.parse()?;

    let address_str = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let did = Did::from_str(address_str).map_err(|_| Error::new(ErrorCode::InvalidParams))?;

    meta.processor
        .connect_with_did(did, true)
        .await
        .map_err(Error::from)?;

    Ok(Value::Null)
}

/// Handle create offer
pub(crate) async fn create_offer(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let p: Vec<String> = params.parse()?;

    let address_str = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let did = Did::from_str(address_str).map_err(|_| Error::new(ErrorCode::InvalidParams))?;

    let (_, offer_payload) = meta
        .processor
        .swarm
        .create_offer(did)
        .await
        .map_err(ServerError::CreateOffer)
        .map_err(Error::from)?;

    let encoded = offer_payload
        .encode()
        .map_err(|_| ServerError::EncodeError)?;
    serde_json::to_value(encoded)
        .map_err(ServerError::SerdeJsonError)
        .map_err(Error::from)
}

/// Handle Answer Offer
pub(crate) async fn answer_offer(params: Params, meta: RpcMeta) -> Result<Value> {
    let p: Vec<String> = params.parse()?;
    let offer_payload_str = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let encoded: Encoded = <Encoded as From<&str>>::from(offer_payload_str);
    let offer_payload =
        MessagePayload::from_encoded(&encoded).map_err(|_| ServerError::DecodeError)?;

    let (_, answer_payload) = meta
        .processor
        .swarm
        .answer_offer(offer_payload)
        .await
        .map_err(ServerError::AnswerOffer)
        .map_err(Error::from)?;

    tracing::debug!("connect_peer_via_ice response: {:?}", answer_payload);
    let encoded = answer_payload
        .encode()
        .map_err(|_| ServerError::EncodeError)?;
    serde_json::to_value(encoded)
        .map_err(ServerError::SerdeJsonError)
        .map_err(Error::from)
}

/// Handle accept answer
pub(crate) async fn accept_answer(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;

    let p: Vec<String> = params.parse()?;
    let answer_payload_str = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let encoded: Encoded = <Encoded as From<&str>>::from(answer_payload_str);
    let answer_payload =
        MessagePayload::from_encoded(&encoded).map_err(|_| ServerError::DecodeError)?;

    let dc = meta
        .processor
        .swarm
        .accept_answer(answer_payload)
        .await
        .map_err(ServerError::AcceptAnswer)?;

    dc2p(dc)
        .to_json_obj()
        .map_err(|_| ServerError::EncodeError)
        .map_err(Error::from)
}

/// Handle list peers
pub(crate) async fn list_peers(_params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let peers = meta.processor.swarm.get_connections();
    let r: Vec<Peer> = peers.into_iter().map(dc2p).collect();
    serde_json::to_value(r).map_err(|_| Error::from(ServerError::EncodeError))
}

/// Handle close connection
pub(crate) async fn close_connection(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<String> = params.parse()?;
    let did = params
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let did = Did::from_str(did).map_err(|_| Error::from(ServerError::InvalidDid))?;
    meta.processor.disconnect(did).await?;
    Ok(serde_json::json!({}))
}

/// Handle send message
pub(crate) async fn send_raw_message(params: Params, meta: RpcMeta) -> Result<Value> {
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
///   - data: base64 of [u8]
pub(crate) async fn send_custom_message(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<serde_json::Value> = params.parse()?;
    let destination = params
        .get(0)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let data = params
        .get(1)
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?
        .as_str()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;

    let data = base64::decode(data).map_err(|_| Error::new(ErrorCode::InvalidParams))?;
    let tx_id = meta.processor.send_message(destination, &data).await?;

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
///   - data: base64 of [u8]
pub(crate) async fn send_backend_message(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let bm_params: BackendMessageParams = params.try_into()?;
    let tx_id = meta
        .processor
        .send_backend_message(bm_params.did, bm_params.data)
        .await?;
    tracing::info!("Send Response message");
    Ok(
        serde_json::to_value(rings_rpc::response::SendMessageResponse::from(
            tx_id.to_string(),
        ))
        .unwrap(),
    )
}

pub(crate) async fn publish_message_to_topic(params: Params, meta: RpcMeta) -> Result<Value> {
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

pub(crate) async fn fetch_messages_of_topic(params: Params, meta: RpcMeta) -> Result<Value> {
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

pub(crate) async fn register_service(params: Params, meta: RpcMeta) -> Result<Value> {
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

pub(crate) async fn lookup_service(params: Params, meta: RpcMeta) -> Result<Value> {
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

fn dc2p((did, conn): (Did, impl ConnectionInterface)) -> Peer {
    Peer {
        did: did.to_string(),
        cid: did.to_string(),
        state: format!("{:?}", conn.webrtc_connection_state()),
    }
}

#[cfg(feature = "node")]
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use jsonrpc_core::types::params::Params;

    use super::*;
    use crate::prelude::*;
    use crate::tests::native::prepare_processor;

    async fn new_rnd_meta() -> RpcMeta {
        let (processor, _) = prepare_processor().await;
        Arc::new(processor).into()
    }

    #[tokio::test]
    async fn test_maually_handshake() {
        let meta1 = new_rnd_meta().await;
        let meta2 = new_rnd_meta().await;
        let offer = create_offer(
            Params::Array(vec![meta2.processor.did().to_string().into()]),
            meta1.clone(),
        )
        .await
        .unwrap();
        let answer = answer_offer(Params::Array(vec![offer]), meta2)
            .await
            .unwrap();
        accept_answer(Params::Array(vec![answer]), meta1)
            .await
            .unwrap();
    }
}
