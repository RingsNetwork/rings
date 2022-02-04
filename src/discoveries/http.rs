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
use bns_core::swarm::Swarm;
use bns_core::types::ice_transport::IceTransport;
use hyper::Body;
use hyper::{Method, Request, Response, StatusCode};
use reqwest;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

pub async fn sdp_handler(
    req: Request<Body>,
    http_addr: String,
    swarm: Arc<Mutex<Swarm>>,
) -> Result<Response<Body>, hyper::http::Error> {
    let mut swarm = swarm.lock().await;
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/sdp") => {
            log::debug!("receive request to POST /sdp");
            // receive answer and send answer back to candidated peer
            let args: HashMap<String, String> =
                serde_json::from_slice(&hyper::body::to_bytes(req).await.unwrap()).unwrap();
            let sdp_str = args.get("offer").unwrap();
            let host_and_ip = args.get("host_and_ip").unwrap();
            let sdp = serde_json::from_str::<RTCSessionDescription>(&sdp_str).unwrap();
            let transport = swarm.get_pending().await.unwrap();
            transport.set_remote_description(sdp).await.unwrap();

            // create answer and response
            let answer = transport.get_answer().await.unwrap();
            transport
                .set_local_description(answer.clone())
                .await
                .unwrap();
            transport.add_remote_server(host_and_ip).await.unwrap();
            let payload = serde_json::to_string(&answer).unwrap();
            match Response::builder().status(200).body(Body::from(payload)) {
                Ok(resp) => Ok(resp),
                Err(_) => panic!("Opps, Response Failed"),
            }
        }
        (&Method::POST, "/connect") => {
            log::debug!("receive request to POST /connect");
            let client = reqwest::Client::new();
            // get sdp offer from renote peer
            let args: HashMap<String, String> =
                serde_json::from_slice(&hyper::body::to_bytes(req).await.unwrap()).unwrap();
            let node = args.get("node").unwrap();
            let uri = hyper::Uri::from_str(&node.clone()).unwrap();
            let host_and_ip = format!("{}:{}", uri.host().unwrap(), uri.port().unwrap());
            let transport = swarm.get_pending().await.unwrap();
            let offer = transport.get_offer().await.unwrap();
            transport
                .set_local_description(offer.clone())
                .await
                .unwrap();
            let mut payload = HashMap::new();
            payload.insert("offer", serde_json::to_string(&offer).unwrap());
            payload.insert("host_and_ip", http_addr);
            let resp = client
                .post(node.to_owned())
                .json(&payload)
                .send()
                .await
                .unwrap();
            let sdp_str = resp.text().await.unwrap();
            let answer = serde_json::from_str::<RTCSessionDescription>(&sdp_str).unwrap();
            transport.set_remote_description(answer).await.unwrap();
            transport.add_remote_server(&host_and_ip).await.unwrap();
            Response::builder().status(200).body(Body::empty())
        }
        (&Method::POST, "/candidate") => {
            println!("remote_handler receive from /candidate");
            let candidate =
                std::str::from_utf8(&hyper::body::to_bytes(req.into_body()).await.unwrap())
                    .unwrap()
                    .to_owned();
            let transport = swarm.get_pending().await.unwrap();
            transport.add_ice_candidate(candidate).await.unwrap();
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::OK;
            Ok(response)
        }
        _ => {
            let mut not_found = Response::default();
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
        }
    }
}
