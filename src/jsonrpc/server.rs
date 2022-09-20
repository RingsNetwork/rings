#![warn(missing_docs)]
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use futures::future::join_all;
use jsonrpc_core::Error;
use jsonrpc_core::ErrorCode;
use jsonrpc_core::MetaIoHandler;
use jsonrpc_core::Metadata;
use jsonrpc_core::Params;
use jsonrpc_core::Result;
use jsonrpc_core::Value;
use rings_core::types::ice_transport::IceTransportInterface;

use super::method::Method;
use super::response;
use super::response::Peer;
use super::response::TransportAndIce;
use crate::error::Error as ServerError;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::transports::manager::TransportManager;
use crate::processor;
use crate::processor::Processor;
use crate::seed::Seed;

/// RpcMeta basic info struct
#[derive(Clone)]
pub struct RpcMeta {
    processor: Arc<Processor>,
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

impl Metadata for RpcMeta {}

impl From<(Arc<Processor>, bool)> for RpcMeta {
    fn from((processor, is_auth): (Arc<Processor>, bool)) -> Self {
        Self { processor, is_auth }
    }
}

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
    handler.add_method_with_meta(Method::SendTo.as_str(), send_message);
    handler.add_method_with_meta(Method::RequestService.as_str(), request_service);
}

async fn connect_peer_via_http(params: Params, meta: RpcMeta) -> Result<Value> {
    let p: Vec<String> = params.parse()?;
    let peer_url = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let transport = meta
        .processor
        .connect_peer_via_http(peer_url)
        .await
        .map_err(Error::from)?;
    Ok(Value::String(transport.id.to_string()))
}

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

async fn answer_offer(params: Params, meta: RpcMeta) -> Result<Value> {
    let p: Vec<String> = params.parse()?;
    let ice_info = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let r = meta
        .processor
        .answer_offer(ice_info)
        .await
        .map_err(Error::from)?;
    log::debug!("connect_peer_via_ice response: {:?}", r.1);
    TransportAndIce::from(r).to_json_obj().map_err(Error::from)
}

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

async fn create_offer(_params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let r = meta.processor.create_offer().await.map_err(Error::from)?;
    TransportAndIce::from(r).to_json_obj().map_err(Error::from)
}

async fn accept_answer(params: Params, meta: RpcMeta) -> Result<Value> {
    meta.require_authed()?;
    let params: Vec<String> = params.parse()?;
    if let ([transport_id, ice], _) = params.split_at(2) {
        let p: processor::Peer = meta
            .processor
            .accept_answer(transport_id.as_str(), ice.as_str())
            .await?;
        let state = p.transport.ice_connection_state().await;
        let r: Peer = (&p, state.map(|x| x.to_string())).into();
        return r.to_json_obj().map_err(Error::from);
    };
    Err(Error::new(ErrorCode::InvalidParams))
}

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
        .map(|(x, y)| Peer::from((x, y.map(|s| s.to_string()))))
        .collect::<Vec<_>>();
    serde_json::to_value(&r).map_err(|_| Error::from(ServerError::JsonSerializeError))
}

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
        .map(|(x, y)| response::TransportInfo::from((x, y.map(|s| s.to_string()))))
        .collect::<Vec<_>>();
    serde_json::to_value(&r).map_err(|_| Error::from(ServerError::JsonSerializeError))
}

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

async fn send_message(params: Params, meta: RpcMeta) -> Result<Value> {
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
    meta.processor
        .send_message(destination, text.as_bytes())
        .await?;
    Ok(serde_json::json!({}))
}


async fn request_service(params: Params, meta: RpcMeta) -> Result<Value> {
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
    meta.processor
        .request_service(destination, text.as_bytes())
        .await?;
    Ok(serde_json::json!({}))
}
