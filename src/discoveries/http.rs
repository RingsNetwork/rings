use bns_core::encoder::decode;
use bns_core::encoder::encode;
use bns_core::swarm::Swarm;
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
/// Server A -> Create answer and send it to Server B

use bns_core::types::ice_transport::IceTransport;
use hyper::Body;
use hyper::{Method, Request, Response, StatusCode};
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use reqwest;
use std::collections::HashMap;

pub async fn sdp_handler(req: Request<Body>, swarm: Swarm) -> Result<Response<Body>, hyper::http::Error> {
    let mut swarm = swarm.to_owned();
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/sdp") => {
            // create offer and send back to candidated peer
            let transport = swarm.get_pending().await;
            match transport {
                Some(trans) => {
                    let offer = encode(trans.get_offer_str().unwrap());
                    return Response::builder().status(200).body(Body::from(offer));
                },
                None => panic!("Cannot get transport")
            }
        }
        (&Method::POST, "/sdp") => {
            // receive answer and send answer back to candidated peer
            let sdp_str =
                std::str::from_utf8(&hyper::body::to_bytes(req.into_body()).await.unwrap())
                    .unwrap()
                    .to_owned();
            let sdp_str = decode(sdp_str).unwrap();
            let sdp = serde_json::from_str::<RTCSessionDescription>(&sdp_str).unwrap();
            let transport = swarm.get_pending().await.unwrap();
            transport.set_remote_description(sdp).await.unwrap();
            swarm.upgrade_pending().unwrap();
            match Response::builder().status(200).body(Body::empty()) {
                Ok(resp) => Ok(resp),
                Err(_) => panic!("Opps"),
            }
        }
        (&Method::GET, "/connect") => {
            let client = reqwest::Client::new();
            let query = req.uri().query().unwrap() ;
            let args = form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect::<HashMap<String, String>>();
            let node = args.get("node").unwrap();
            let offer = decode(
                reqwest::get(node.to_owned()).await.unwrap().text().await.unwrap()
            ).unwrap();
            let transport = swarm.get_pending().await.unwrap();
            let sdp = serde_json::from_str::<RTCSessionDescription>(&offer).unwrap();
            transport.set_remote_description(sdp).await.unwrap();
            let answer = transport.get_answer().await.unwrap();
            client.post(node).body(answer.sdp).send().await.unwrap();
             match Response::builder().status(200).body(Body::empty()) {
                Ok(resp) => Ok(resp),
                Err(_) => panic!("Opps"),
            }
        }
        _ => {
            let mut not_found = Response::default();
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
        }
    }
}
