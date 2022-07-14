#![warn(missing_docs)]
use std::str::FromStr;

use jsonrpc_core::Error;
use jsonrpc_core::ErrorCode;
use jsonrpc_core::MetaIoHandler;
use jsonrpc_core::Params;
use jsonrpc_core::Result;
use jsonrpc_core::Value;

use super::method::Method;
use super::response::Peer;
use super::response::TransportAndIce;
use crate::error::Error as ServerError;
use crate::prelude::rings_core::prelude::Address;
use crate::processor::Processor;

/// authority_info for request authority info check
#[derive(Debug, Clone)]
pub struct AuthorityInfo(bool);

impl From<bool> for AuthorityInfo {
    fn from(v: bool) -> Self {
        Self(v)
    }
}

pub(crate) async fn build_handler(handler: &mut MetaIoHandler<Processor>) {
    handler.add_method_with_meta(Method::ConnectPeerViaHttp.as_str(), connect_peer_via_http);
    handler.add_method_with_meta(Method::AnswerOffer.as_str(), answer_offer);
    handler.add_method_with_meta(Method::ConnectWithAddress.as_str(), connect_with_address);
    handler.add_method_with_meta(Method::CreateOffer.as_str(), create_offer);
    handler.add_method_with_meta(Method::AcceptAnswer.as_str(), accept_answer);
    handler.add_method_with_meta(Method::ListPeers.as_str(), list_peers);
    handler.add_method_with_meta(Method::Disconnect.as_str(), close_connection);
    handler.add_method_with_meta(Method::SendTo.as_str(), send_message)
}

async fn connect_peer_via_http(params: Params, processor: Processor) -> Result<Value> {
    let p: Vec<String> = params.parse()?;
    let peer_url = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let transport = processor
        .connect_peer_via_http(peer_url)
        .await
        .map_err(Error::from)?;
    Ok(Value::String(transport.id.to_string()))
}

async fn answer_offer(params: Params, processor: Processor) -> Result<Value> {
    let p: Vec<String> = params.parse()?;
    let ice_info = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let r = processor
        .answer_offer(ice_info)
        .await
        .map_err(Error::from)?;
    log::debug!("connect_peer_via_ice response: {:?}", r.1);
    TransportAndIce::from(r).to_json_obj().map_err(Error::from)
}

async fn connect_with_address(params: Params, processor: Processor) -> Result<Value> {
    let p: Vec<String> = params.parse()?;
    let address_str = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    processor
        .connect_with_address(
            &Address::from_str(address_str).map_err(|_| Error::new(ErrorCode::InvalidParams))?,
            true,
        )
        .await
        .map_err(Error::from)?;
    Ok(Value::Null)
}

async fn create_offer(_params: Params, processor: Processor) -> Result<Value> {
    let r = processor.create_offer().await.map_err(Error::from)?;
    TransportAndIce::from(r).to_json_obj().map_err(Error::from)
}

async fn accept_answer(params: Params, processor: Processor) -> Result<Value> {
    let params: Vec<String> = params.parse()?;
    if let ([transport_id, ice], _) = params.split_at(2) {
        let r: Peer = processor
            .accept_answer(transport_id.as_str(), ice.as_str())
            .await?
            .into();
        return r.to_json_obj().map_err(Error::from);
    };
    Err(Error::new(ErrorCode::InvalidParams))
}

async fn list_peers(_params: Params, processor: Processor) -> Result<Value> {
    let r = processor
        .list_peers()
        .await?
        .into_iter()
        .map(|x| x.into())
        .collect::<Vec<Peer>>();
    serde_json::to_value(&r).map_err(|_| Error::from(ServerError::JsonSerializeError))
}

async fn close_connection(params: Params, processor: Processor) -> Result<Value> {
    let params: Vec<String> = params.parse()?;
    let address = params
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    processor.disconnect(address).await?;
    Ok(serde_json::json!({}))
}

async fn send_message(params: Params, processor: Processor) -> Result<Value> {
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
    processor.send_message(destination, text.as_bytes()).await?;
    Ok(serde_json::json!({}))
}
