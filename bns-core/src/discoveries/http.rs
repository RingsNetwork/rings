use crate::swarm::Swarm;
use crate::types::ice_transport::IceTransport;
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
use anyhow;
use hyper::Body;
use hyper::{Client, Method, Request, Response, StatusCode};
use serde_json::{self, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

pub async fn send_candidate(addr: &str, candidate: &RTCIceCandidate) -> anyhow::Result<()> {
    let payload = candidate.to_json().await?.candidate;
    log::info!("Addr {:?}, candidate: {:?}", addr, candidate);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{}/candidate", addr))
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(payload))
        .unwrap();
    Client::new().request(req).await.unwrap();
    Ok(())
}

pub async fn sdp_handler(
    req: Request<Body>,
    http_addr: String,
    swarm: Arc<Mutex<Swarm>>,
) -> Result<Response<Body>, hyper::http::Error> {
    let mut swarm = swarm.lock().await;
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/offer") => {
            println!("receive request to POST /offer");
            // receive answer and send answer back to candidated peer
            let args: HashMap<String, String> =
                serde_json::from_slice(&hyper::body::to_bytes(req).await.unwrap()).unwrap();
            let sdp_str = args.get("offer").unwrap();
            let addr = args.get("host_and_ip").unwrap();
            let offer = serde_json::from_str::<RTCSessionDescription>(&sdp_str).unwrap();
            let transport = swarm.get_pending().await.unwrap();
            println!("Remote Offer: {:?}", offer);
            transport.set_remote_description(offer).await.unwrap();

            // create answer and send
            let answer = transport.get_answer().await.unwrap();
            println!("Local Answer: {:?}", answer);
            let mut payload = HashMap::new();
            payload.insert("answer", serde_json::to_string(&answer).unwrap());
            payload.insert("host_and_ip", http_addr);
            let payload = json!(payload);
            let node = format!("http://{}/answer", addr.clone());
            println!("/offer node: {:?}", node);
            println!("/offer payload: {:?}", payload);
            let req = Request::builder()
                .method(Method::POST)
                .uri(node.to_owned())
                .header("content-type", "application/json; charset=utf-8")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap();
            match Client::new().request(req).await {
                Ok(_) => println!("Successful send"),
                Err(e) => panic!("Opps, Send Answer Failed with {:?}", e),
            };
            println!("after sending answer");
            transport
                .set_local_description(answer.clone())
                .await
                .unwrap();
            transport.add_remote_server(addr).await.unwrap();
            println!("after set local answer and add remote server");
            match Response::builder().status(200).body(Body::empty()) {
                Ok(resp) => Ok(resp),
                Err(_) => panic!("Opps, Response Failed"),
            }
        }
        (&Method::POST, "/answer") => {
            println!("receive request to POST /answer");
            // receive answer and send answer back to candidated peer
            let args: HashMap<String, String> =
                serde_json::from_slice(&hyper::body::to_bytes(req).await.unwrap()).unwrap();
            let sdp_str = args.get("answer").unwrap();
            let addr = args.get("host_and_ip").unwrap();
            let answer = serde_json::from_str::<RTCSessionDescription>(&sdp_str).unwrap();
            println!("Remote Answer: {:?}", answer);
            let transport = swarm.get_pending().await.unwrap();
            transport.set_remote_description(answer).await.unwrap();
            transport.add_remote_server(addr).await.unwrap();
            let pending_candidates = transport.get_pending_candidates().await;
            println!("PendingCandidates Numbers: {:?}", pending_candidates.len());
            match Response::builder().status(200).body(Body::empty()) {
                Ok(resp) => Ok(resp),
                Err(_) => panic!("Opps, Response Failed"),
            }
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

pub async fn sync_offer_to_swarm(swarm: Arc<Mutex<Swarm>>, swarm_addr: String, local_addr: String) {
    let mut swarm = swarm.lock().await;
    // get sdp offer from renote peer
    let node = format!("http://{}/offer", swarm_addr);
    println!("Send to {}", node);
    let transport = swarm.get_pending().await.unwrap();
    let offer = transport.get_offer().await.unwrap();
    println!("Local Offer: {:?}", offer);
    transport
        .set_local_description(offer.clone())
        .await
        .unwrap();
    let mut payload = HashMap::new();
    payload.insert("offer", serde_json::to_string(&offer).unwrap());
    payload.insert("host_and_ip", local_addr);
    let payload = json!(payload);
    let req = Request::builder()
        .method(Method::POST)
        .uri(node.to_owned())
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(serde_json::to_string(&payload).unwrap()))
        .unwrap();
    match Client::new().request(req).await {
        Ok(_) => println!("Successful Send Offer"),
        Err(e) => panic!("Opps, Sending Offer Failed with {:?}", e),
    };
}
