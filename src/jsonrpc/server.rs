use std::str::FromStr;

use super::{
    method::Method,
    response::{Peer, TransportAndIce},
};
use crate::{
    error::Error as ServerError, prelude::rings_core::prelude::Address, processor::Processor,
};
use jsonrpc_core::{Error, ErrorCode, MetaIoHandler, Params, Result, Value};

pub async fn build_handler(handler: &mut MetaIoHandler<Processor>) {
    handler.add_method_with_meta(Method::ConnectPeerViaHttp.as_str(), connect_peer_via_http);
    handler.add_method_with_meta(Method::ConnectPeerViaIce.as_str(), connect_peer_via_ice);
    handler.add_method_with_meta(Method::ConnectWithAddress.as_str(), connect_with_address);
    handler.add_method_with_meta(Method::CreateOffer.as_str(), create_offer);
    handler.add_method_with_meta(Method::AcceptAnswer.as_str(), accept_answer);
    handler.add_method_with_meta(Method::ListPeers.as_str(), list_peers);
    handler.add_method_with_meta(Method::Disconnect.as_str(), close_connection);
}

pub async fn connect_peer_via_http(params: Params, processor: Processor) -> Result<Value> {
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

pub async fn connect_peer_via_ice(params: Params, processor: Processor) -> Result<Value> {
    let p: Vec<String> = params.parse()?;
    let ice_info = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    let r = processor
        .connect_peer_via_ice(ice_info)
        .await
        .map_err(Error::from)?;
    TransportAndIce::from(r).to_json_obj().map_err(Error::from)
}

pub async fn connect_with_address(params: Params, processor: Processor) -> Result<Value> {
    let p: Vec<String> = params.parse()?;
    let address_str = p
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    processor
        .connect_with_address(
            &Address::from_str(address_str).map_err(|_| Error::new(ErrorCode::InvalidParams))?,
        )
        .await
        .map_err(Error::from)?;
    Ok(Value::Null)
}

pub async fn create_offer(_params: Params, processor: Processor) -> Result<Value> {
    let r = processor.create_offer().await.map_err(Error::from)?;
    TransportAndIce::from(r).to_json_obj().map_err(Error::from)
}

pub async fn accept_answer(params: Params, processor: Processor) -> Result<Value> {
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

pub async fn list_peers(_params: Params, processor: Processor) -> Result<Value> {
    let r = processor
        .list_peers()
        .await?
        .into_iter()
        .map(|x| x.into())
        .collect::<Vec<Peer>>();
    serde_json::to_value(&r).map_err(|_| Error::from(ServerError::JsonSerializeError))
}

pub async fn close_connection(params: Params, processor: Processor) -> Result<Value> {
    let params: Vec<String> = params.parse()?;
    let address = params
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams))?;
    processor.disconnect(address).await?;
    Ok(serde_json::json!({}))
}
