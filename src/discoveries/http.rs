use hyper::{Body, Method, Request, Response, StatusCode};
use bns_core::swarm::Swarm;
use bns_core::types::ice_transport::IceTrickleScheme;
use hyper::http::Error;
use anyhow::anyhow;
use std::collections::HashMap;
use secp256k1::SecretKey;


async fn handshake(swarm: Swarm, key: SecretKey, data: Vec<u8>) -> anyhow::Result<String> {
    let mut swarm = swarm.to_owned();
    let transport = swarm.get_pending().await
        .ok_or("cannot get transaction").map_err(|e| anyhow!(e))?;
    let resp = transport.prepare_local_info(key).await;
    let _ = transport.register_remote_info(
        String::from_utf8(data)?
    ).await;
    swarm.upgrade_pending().await?;
    resp
}

pub async fn trickle_scheme_handler(
    swarm: Swarm,
    req: Request<Body>,
    key: SecretKey
) -> anyhow::Result<String> {
    let data = hyper::body::to_bytes(req).await?.to_vec();
    handshake(swarm, key, data).await
}


pub async fn trickle_forward(
    swarm: Swarm,
    req: Request<Body>,
    key: SecretKey
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let query = req.uri().query().ok_or("cannot get query").map_err(|e| anyhow!(e))?;
    let args = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<HashMap<String, String>>();
    let node = args.get("node").ok_or("should include node params").map_err(|e| anyhow!(e))?;
    let mut swarm = swarm.to_owned();

    let transport = swarm.get_pending().await
        .ok_or("cannot get transaction").map_err(|e| anyhow!(e))?;
    let req = transport.prepare_local_info(key).await?;
    log::trace!("sending info to {:?}", &node);
    let resp = client.post(node).body(req).send().await?.text().await?;
    let _ = transport.register_remote_info(String::from_utf8(resp.as_bytes().to_vec())?).await?;
    Ok("ok".to_string())
}


pub async fn discoveries_services(
    req: Request<Body>,
    swarm: Swarm,
    key: SecretKey
) -> Result<Response<Body>, Error> {
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/sdp") => {
            match trickle_scheme_handler(swarm.to_owned(), req, key).await {
                Ok(resp) => {
                    Response::builder()
                        .status(200)
                        .body(Body::from(resp))
                },
                Err(e) => {
                    log::error!("{:?}", e);
                    let mut swarm = swarm.to_owned();
                    swarm.drop_pending().await;
                    Response::builder()
                        .status(500)
                        .body(Body::from("internal error".to_string()))
                }
            }
        },
        (&Method::GET, "/connect") => {
            match trickle_forward(swarm.to_owned(), req, key).await {
                 Ok(resp) => {
                    Response::builder()
                        .status(200)
                        .body(Body::from(resp))
                },
                Err(e) => {
                    log::error!("{:?}", e);
                    let mut swarm = swarm.to_owned();
                    swarm.drop_pending().await;
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
