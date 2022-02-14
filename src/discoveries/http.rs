/// HTTP services for braowser based P2P initialization
/// Two API *must* provided:
/// 1. GET /sdp
/// Which create offer and send back to candidated peer
/// 2. POST /sdp
/// Which receive offer from peer and send the answer back
/// 2. Get /connect/{url}
/// Which request offer from peer and sand the answer back

/// SDP Scheme:
/// Browser -> Get offer from server
/// Server -> Send Offer and set it as local_description (implemented in transport)
/// Browser -> Send answer back to server, Server set it to remote description

/// SDP Forward Scheme:
/// Server A -> Requset offer from Server B, and set it as remote_descriton
/// Server A -> sent local_desc as answer to Server B
use futures::future::join_all;
use hyper::{Body, Client, Method, Request, Response, StatusCode};
use serde::Deserialize;
use serde::Serialize;
use serde_json;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use bns_core::swarm::Swarm;
use bns_core::transports::default::DefaultTransport;
use bns_core::types::ice_transport::IceTransport;

#[derive(Deserialize, Serialize, Debug)]
pub struct Payload {
    pub sdp: String,
    pub host: String,
    pub candidates: Vec<RTCIceCandidateInit>,
}

pub async fn sdp_handler(
    req: Request<Body>,
    host: String,
    swarm: Swarm,
) -> Result<Response<Body>, hyper::http::Error> {
    let mut swarm = swarm.to_owned();
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/offer") => {
            let transport = swarm.get_pending().await.unwrap();
            let data: Payload =
                serde_json::from_slice(&hyper::body::to_bytes(req).await.unwrap()).unwrap();
            let offer = serde_json::from_str::<RTCSessionDescription>(&data.sdp).unwrap();
            transport.set_remote_description(offer).await.unwrap();

            let answer = transport.get_answer().await.unwrap();
            transport
                .set_local_description(answer.clone())
                .await
                .unwrap();
            let local_candidates_json = join_all(
                transport
                    .get_pending_candidates()
                    .await
                    .iter()
                    .map(async move |c| c.clone().to_json().await.unwrap()),
            )
            .await;
            for c in data.candidates {
                transport
                    .add_ice_candidate(c.candidate.clone())
                    .await
                    .unwrap();
            }
            swarm.upgrade_pending().await.unwrap();
            let resp = Payload {
                sdp: serde_json::to_string(&answer).unwrap(),
                host: host.clone(),
                candidates: local_candidates_json,
            };
            match Response::builder()
                .status(200)
                .body(Body::from(serde_json::to_string(&resp).unwrap()))
            {
                Ok(r) => {
                    log::debug!("Ok Response, {:?}", r);
                    Ok(r)
                }
                Err(_) => panic!("Opps, Response Failed"),
            }
        }
        _ => {
            let mut not_found = Response::default();
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
        }
    }
}

pub async fn send_to_swarm(transport: &mut DefaultTransport, localhost: &str, swarmhost: &str) {
    let offer = transport.run_as_node().await.unwrap();
    let node = format!("http://{}/offer", swarmhost);
    let local_candidates_json = join_all(
        transport
            .get_pending_candidates()
            .await
            .iter()
            .map(async move |c| c.clone().to_json().await.unwrap()),
    )
    .await;
    let payload = Payload {
        sdp: serde_json::to_string(&offer).unwrap(),
        host: localhost.to_owned(),
        candidates: local_candidates_json,
    };
    let req = Request::builder()
        .method(Method::POST)
        .uri(node.to_owned())
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(serde_json::to_string(&payload).unwrap()))
        .unwrap();
    match Client::new().request(req).await {
        Ok(resp) => {
            let data: Payload =
                serde_json::from_slice(&hyper::body::to_bytes(resp).await.unwrap()).unwrap();
            let answer = serde_json::from_str::<RTCSessionDescription>(&data.sdp).unwrap();
            transport.set_remote_description(answer).await.unwrap();
            for c in data.candidates {
                transport
                    .add_ice_candidate(c.candidate.clone())
                    .await
                    .unwrap();
            }
        }
        Err(e) => panic!("Opps, Sending Offer Failed with {:?}", e),
    };
}
