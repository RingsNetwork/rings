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
use crate::signing::verify;
use crate::signing::SigMsg;

#[derive(Deserialize, Serialize, Debug)]
pub struct TricklePayload {
    pub sdp: String,
    pub candidates: Vec<RTCIceCandidateInit>,
}


async fn handle_sdp(
    swarm: Swarm,
    req: Request<Body>,
    key: impl Key
) -> anyhow::Result<String> {
    let mut swarm = swarm.to_owned();
    let transport = swarm.get_pending().await
        .ok_or("cannot get transaction").map_err(|e| anyhow!(e))?;
    let data: SigMsg<TricklePayload> = serde_json::from_slice(
        decode(
            String::from_utf8(
                hyper::body::to_bytes(req).await?.to_vec()
            )?
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
    transport.set_local_description(answer.clone()).await?;
    let local_candidates_json = join_all(
        transport
            .get_pending_candidates()
            .await
            .iter()
            .map(async move |c| c.clone().to_json().await.unwrap()),
    ).await;
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

// pub async fn sdp_handler(
//     req: Request<Body>,
//     swarm: Swarm,
// ) -> Result<Response<Body>, Error> {
//     let mut swarm = swarm.to_owned();
//     match (req.method(), req.uri().path()) {
//         (&Method::POST, "/offer") => {
//             let transport = swarm.get_pending().await.unwrap();
//             let data: TicklePayload =
//                 serde_json::from_slice(
//                     &decode(hyper::body::to_bytes(req).await.unwrap()).unwrap()
//                 ).unwrap();
//             let offer = serde_json::from_str::<RTCSessionDescription>(&data.sdp).unwrap();
//             transport.set_remote_description(offer).await.unwrap();

//             let answer = transport.get_answer().await.unwrap();
//             transport
//                 .set_local_description(answer.clone())
//                 .await
//                 .unwrap();
//             let local_candidates_json = join_all(
//                 transport
//                     .get_pending_candidates()
//                     .await
//                     .iter()
//                     .map(async move |c| c.clone().to_json().await.unwrap()),
//             )
//             .await;
//             for c in data.candidates {
//                 transport
//                     .add_ice_candidate(c.candidate.clone())
//                     .await
//                     .unwrap();
//             }
//             swarm.upgrade_pending().await.unwrap();
//             let resp = TicklePayload {
//                 sdp: serde_json::to_string(&answer).unwrap(),
//                 candidates: local_candidates_json,
//             };
//             match Response::builder()
//                 .status(200)
//                 .body(Body::from(serde_json::to_string(&resp).unwrap()))
//             {
//                 Ok(r) => {
//                     log::debug!("Ok Response, {:?}", r);
//                     Ok(r)
//                 }
//                 Err(_) => panic!("Opps, Response Failed"),
//             }
//         }
//         _ => {
//             let mut not_found = Response::default();
//             *not_found.status_mut() = StatusCode::NOT_FOUND;
//             Ok(not_found)
//         }
//     }
// }

// pub async fn send_to_swarm(transport: &mut DefaultTransport, localhost: &str, swarmhost: &str) {
//     let offer = transport.run_as_node().await.unwrap();
//     let node = format!("http://{}/offer", swarmhost);
//     let local_candidates_json = join_all(
//         transport
//             .get_pending_candidates()
//             .await
//             .iter()
//             .map(async move |c| c.clone().to_json().await.unwrap()),
//     )
//     .await;
//     let payload = TicklePayload {
//         sdp: serde_json::to_string(&offer).unwrap(),
//         host: localhost.to_owned(),
//         candidates: local_candidates_json,
//     };
//     let req = Request::builder()
//         .method(Method::POST)
//         .uri(node.to_owned())
//         .header("content-type", "application/json; charset=utf-8")
//         .body(Body::from(serde_json::to_string(&payload).unwrap()))
//         .unwrap();
//     match Client::new().request(req).await {
//         Ok(resp) => {
//             let data: TicklePayload =
//                 serde_json::from_slice(&hyper::body::to_bytes(resp).await.unwrap()).unwrap();
//             let answer = serde_json::from_str::<RTCSessionDescription>(&data.sdp).unwrap();
//             transport.set_remote_description(answer).await.unwrap();
//             for c in data.candidates {
//                 transport
//                     .add_ice_candidate(c.candidate.clone())
//                     .await
//                     .unwrap();
//             }
//         }
//         Err(e) => panic!("Opps, Sending Offer Failed with {:?}", e),
//     };
// }
