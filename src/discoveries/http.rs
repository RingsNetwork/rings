use anyhow::anyhow;
use bns_core::swarm::Swarm;
use bns_core::types::ice_transport::IceTrickleScheme;
use hyper::http::Error;
use hyper::{Body, Method, Request, Response, StatusCode};
use secp256k1::SecretKey;
use std::collections::HashMap;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

async fn handshake(swarm: Swarm, key: SecretKey, data: Vec<u8>) -> anyhow::Result<String> {
    // get offer from remote and send answer back
    let mut swarm = swarm.to_owned();
    let transport = swarm
        .get_pending()
        .await
        .ok_or("cannot get transaction")
        .map_err(|e| anyhow!(e))?;
    let registered = transport
        .register_remote_info(String::from_utf8(data)?)
        .await;
    match registered {
        Ok(_) => {
            let resp = transport.get_handshake_info(key, RTCSdpType::Answer).await;
            match resp {
                Ok(info) => {
                    swarm.upgrade_pending().await?;
                    anyhow::Result::Ok(info)
                }
                Err(e) => {
                    log::error!("failed to get handshake info: {:?}", e);
                    anyhow::Result::Err(e)
                }
            }
        }
        Err(e) => {
            log::error!("failed to register {:?}", e);
            Err(anyhow!(e))
        }
    }
}

pub async fn trickle_scheme_handler(
    swarm: Swarm,
    req: Request<Body>,
    key: SecretKey,
) -> anyhow::Result<String> {
    match hyper::body::to_bytes(req).await {
        Ok(data) => handshake(swarm, key, data.to_vec()).await,
        Err(e) => {
            log::error!("someting wrong {:?}", e);
            anyhow::Result::Err(anyhow!(e))
        }
    }
}

pub async fn trickle_forward(
    swarm: Swarm,
    req: Request<Body>,
    key: SecretKey,
) -> anyhow::Result<String> {
    // request remote offer and sand answer to remote
    let client = reqwest::Client::new();
    let query = req
        .uri()
        .query()
        .ok_or("cannot get query")
        .map_err(|e| anyhow!(e))?;
    let args = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<HashMap<String, String>>();
    let node = args
        .get("node")
        .ok_or("should include node params")
        .map_err(|e| anyhow!(e))?;
    let mut swarm = swarm.to_owned();
    let transport = swarm
        .get_pending()
        .await
        .ok_or("cannot get transaction")
        .map_err(|e| anyhow!(e))?;
    let req = transport.get_handshake_info(key, RTCSdpType::Offer).await?;
    log::debug!(
        "sending offer and candidate {:?} to {:?}",
        req.to_owned(),
        &node
    );
    match client.post(node).body(req).send().await?.text().await {
        Ok(resp) => {
            log::debug!("get answer and candidate from remote");
            let _ = transport
                .register_remote_info(String::from_utf8(resp.as_bytes().to_vec())?)
                .await?;
            Ok("ok".to_string())
        }
        Err(e) => {
            log::error!("someting wrong {:?}", e);
            anyhow::Result::Err(anyhow!(e))
        }
    }
}

pub async fn discoveries_services(
    req: Request<Body>,
    swarm: Swarm,
    key: SecretKey,
) -> Result<Response<Body>, Error> {
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/sdp") => {
            log::info!("incoming req {:?}", req);
            match trickle_scheme_handler(swarm.to_owned(), req, key).await {
                Ok(resp) => Response::builder().status(200).body(Body::from(resp)),
                Err(e) => {
                    log::error!("failed to handle trickle scheme: {:?}", e);
                    let mut swarm = swarm.to_owned();
                    swarm.drop_pending().await;
                    Response::builder()
                        .status(500)
                        .body(Body::from("internal error".to_string()))
                }
            }
        }
        (&Method::GET, "/connect") => match trickle_forward(swarm.to_owned(), req, key).await {
            Ok(resp) => Response::builder().status(200).body(Body::from(resp)),
            Err(e) => {
                log::error!("failed to trickle forward {:?}", e);
                let mut swarm = swarm.to_owned();
                swarm.drop_pending().await;
                Response::builder()
                    .status(500)
                    .body(Body::from("internal error".to_string()))
            }
        },
        _ => {
            let mut not_found = Response::default();
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
        }
    }
}
