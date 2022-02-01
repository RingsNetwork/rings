use bns_core::encoder::decode;
use bns_core::swarm::swarm::Swarm;
/// HTTP services for braowser based P2P initialization
/// Two API *must* provided:
/// 1. GET /sdp
/// Which create offer and send back to candidated peer
/// 2. POST /sdp
/// Which receive offer from peer and send the answer back
use bns_core::transports::default::DefaultTransport;
use bns_core::types::ice_transport::IceTransport;
use hyper::Body;
use hyper::{Method, Request, Response, StatusCode};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

pub async fn sdp_handler(req: Request<Body>, swarm: Swarm) -> Result<Response<Body>, hyper::Error> {
    let mut swarm = swarm.to_owned();
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/sdp") => {
            // create offer and send back to candidated peer
            let offer = swarm.get_pending().await.unwrap().get_offer_str().unwrap();
            match Response::builder().status(200).body(Body::from(offer)) {
                Ok(resp) => Ok(resp),
                Err(_) => panic!("Opps"),
            }
        }
        (&Method::POST, "/sdp") => {
            // create offer and send back to candidated peer
            let sdp_str =
                std::str::from_utf8(&hyper::body::to_bytes(req.into_body()).await.unwrap())
                    .unwrap()
                    .to_owned();
            let sdp_str = decode(sdp_str).unwrap();
            let sdp = serde_json::from_str::<RTCSessionDescription>(&sdp_str).unwrap();
            let transport = swarm.get_pending().await.unwrap();
            transport.set_remote_description(sdp).await.unwrap();
            let answer = transport.get_answer().await.unwrap();
            swarm.upgrade_pending().unwrap();
            match Response::builder().status(200).body(Body::from(answer.sdp)) {
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

pub async fn remote_handler(
    req: Request<Body>,
    _remote_addr: String,
    ice_transport: DefaultTransport,
) -> Result<Response<Body>, hyper::Error> {
    let pc = ice_transport.connection.lock().await.clone().unwrap();
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/candidate") => {
            println!("remote_handler receive from /candidate");
            let candidate =
                match std::str::from_utf8(&hyper::body::to_bytes(req.into_body()).await?) {
                    Ok(s) => s.to_owned(),
                    Err(err) => panic!("{}", err),
                };
            if let Err(err) = pc
                .add_ice_candidate(RTCIceCandidateInit {
                    candidate,
                    ..Default::default()
                })
                .await
            {
                panic!("{}", err);
            }

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
