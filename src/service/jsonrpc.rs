use super::request::Method;
use crate::{error::Error as ServerError, service::processor::Processor};
use jsonrpc_core::{Error, ErrorCode, MetaIoHandler, Params, Result, Value};

pub async fn build_handler(handler: &mut MetaIoHandler<Processor>) {
    handler.add_method_with_meta(Method::ConnectPeerViaHttp.as_str(), connect_peer_via_http);
    handler.add_method_with_meta(Method::ConnectPeerViaIce.as_str(), connect_peer_via_ice);
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
    let id = processor
        .connect_peer_via_http(peer_url)
        .await
        .map_err(Error::from)?;
    Ok(Value::String(id))
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
    r.to_json_obj().map_err(Error::from)
}

pub async fn create_offer(_params: Params, processor: Processor) -> Result<Value> {
    let r = processor.create_offer().await.map_err(Error::from)?;
    r.to_json_obj().map_err(Error::from)
}

pub async fn accept_answer(params: Params, processor: Processor) -> Result<Value> {
    let params: Vec<String> = params.parse()?;
    if let ([transport_id, ice], _) = params.split_at(2) {
        let r = processor
            .accept_answer(transport_id.as_str(), ice.as_str())
            .await?;
        return r.to_json_obj().map_err(Error::from);
    };
    Err(Error::new(ErrorCode::InvalidParams))
}

pub async fn list_peers(_params: Params, processor: Processor) -> Result<Value> {
    let r = processor.list_peers().await?;
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
