use futures::future::join_all;
use hyper::{Body, Client, Method, Request, Response, StatusCode};
use serde::Deserialize;
use serde::Serialize;
use serde_json;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use bns_core::swarm::Swarm;
use bns_core::transports::default::DefaultTransport;
use bns_core::encoder::{encode, decode};
use bns_core::types::ice_transport::IceTransport;
use web3::types::Address;
use web3::types::SignedData;
use web3::signing::Key;

use hyper::http::Error;
use anyhow::anyhow;
use std::collections::HashMap;
use crate::signing::SigMsg;


#[derive(Deserialize, Serialize, Debug)]
pub struct TricklePayload {
    pub sdp: String,
    pub candidates: Vec<RTCIceCandidateInit>,
}

async fn ser_pending_candidate(t: &DefaultTransport) -> Vec<RTCIceCandidateInit> {
    join_all(
        t
            .get_pending_candidates()
            .await
            .iter()
            .map(async move |c| c.clone().to_json().await.unwrap()),
    ).await
}

async fn handshake(swarm: Swarm, key: impl key, data: vec[u8]) -> Result<String> {
    let mut swarm = swarm.to_owned();
    let transport = swarm.get_pending().await
        .ok_or("cannot get transaction").map_err(|e| anyhow!(e))?;
    let data: SigMsg<TricklePayload> = serde_json::from_slice(
        decode(
            String::from_utf8(data)?
        ).map_err(|e| anyhow!(e))?.as_bytes()
    ).map_err(|e| anyhow!(e))?;
        match data.verify() {
        Ok(true) => (),
        _ => {
            return Err(anyhow!("failed on verify message sigature"));
        }
    }

    let offer = serde_json::from_str::<RTCSessionDescription>(&data.data.sdp)?;
    transport.set_remote_description(offer).await?;
    let answer = transport.get_answer().await?;
    let local_candidates_json = ser_pending_candidate(&transport).await;
    for c in data.data.candidates {
        transport
            .add_ice_candidate(c.candidate.clone())
            .await
            .unwrap();
    }
    swarm.upgrade_pending().await?;

    let data = TricklePayload {
        sdp: serde_json::to_string(&answer).unwrap(),
        candidates: local_candidates_json,
    };
    let resp = SigMsg::new(data, key)?;
    Ok(serde_json::to_string(&resp)?)


}

async fn handle_sdp(
    swarm: Swarm,
    req: Request<Body>,
    key: impl Key
) -> anyhow::Result<String> {
    let data = hyper::body::to_bytes(req).await?.to_vec();
    handshake(swarm, key, data)
}

pub async fn discoveries_services(
    req: Request<Body>,
    swarm: Swarm,
    key: impl Key
) -> Result<Response<Body>, Error> {
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/offer") => {
            match handle_sdp(swarm, req, key).await {
                Ok(resp) => {
                    Response::builder()
                        .status(200)
                        .body(Body::from(resp))
                },
                Err(_) => {
                    Response::builder()
                        .status(500)
                        .body(Body::from("internal error".to_string()))
                }
            }
        },
        _ => {
            let mut not_found = Response::default();
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
        }

    }

}

pub async fn connect(
    swarm: Swarm,
    req: Request<Body>,
    key: impl Key
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let query = req.uri().query().ok_or("cannot get query").map_err(|e| anyhow!(e))?;
    let args = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<HashMap<String, String>>();
    let mut swarm = swarm.to_owned();

    let transport = swarm.get_pending().await
        .ok_or("cannot get transaction").map_err(|e| anyhow!(e))?;
    let offer = transport.get_offer_str();
    let data = TricklePayload {
        sdp: serde_json::to_string(&answer).unwrap(),
        candidates: ser_pending_candidate(&transport).await
    };
    let msg = SigMsg::new(data, key)?;
    let req = encode(serde_json::to_string(&msg)?);
    let resp = client.post(node).body(&req).send().await?;

}
