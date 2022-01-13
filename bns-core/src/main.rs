use anyhow::Result;


use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Client, Method, Request, Response, Server, StatusCode};

use std::net::SocketAddr;
use std::str::FromStr;








use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};





use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;


use bns_core::ice_transport::IceTransport;

async fn signal_candidate(addr: &str, c: &RTCIceCandidate) -> Result<()> {
    let payload = c.to_json().await?.candidate;
    let req = match Request::builder()
        .method(Method::POST)
        .uri(format!("http://{}/candidate", addr))
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(payload))
    {
        Ok(req) => req,
        Err(err) => {
            println!("{}", err);
            return Err(err.into());
        }
    };

    let _resp = match Client::new().request(req).await {
        Ok(resp) => resp,
        Err(err) => {
            println!("{}", err);
            return Err(err.into());
        }
    };
    //println!("signal_candidate Response: {}", resp.status());

    Ok(())
}

async fn remote_handler(
    req: Request<Body>,
    remote_addr: String,
    ice_transport: IceTransport,
) -> Result<Response<Body>, hyper::Error> {
    let pc = ice_transport.connection.lock().await.clone().unwrap();
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/candidate") => {
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

        (&Method::POST, "/sdp") => {
            // maybe can handle json decode sdp and server addr
            let sdp_str = match std::str::from_utf8(&hyper::body::to_bytes(req.into_body()).await?)
            {
                Ok(s) => s.to_owned(),
                Err(err) => panic!("{}", err),
            };
            let sdp = match serde_json::from_str::<RTCSessionDescription>(&sdp_str) {
                Ok(s) => s,
                Err(err) => panic!("{}", err),
            };

            if let Err(err) = pc.set_remote_description(sdp).await {
                panic!("{}", err);
            }

            // Create an answer to send to the other process
            let answer = match pc.create_answer(None).await {
                Ok(a) => a,
                Err(err) => panic!("{}", err),
            };

            let payload = match serde_json::to_string(&answer) {
                Ok(p) => p,
                Err(err) => panic!("{}", err),
            };

            let req = match Request::builder()
                .method(Method::POST)
                .uri(format!("http://{}/sdp", remote_addr))
                .header("content-type", "application/json; charset=utf-8")
                .body(Body::from(payload))
            {
                Ok(req) => req,
                Err(err) => panic!("{}", err),
            };

            let _resp = match Client::new().request(req).await {
                Ok(resp) => resp,
                Err(err) => {
                    println!("{}", err);
                    return Err(err);
                }
            };

            // Sets the LocalDescription, and starts our UDP listeners
            if let Err(err) = pc.set_local_description(answer).await {
                panic!("{}", err);
            }

            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::OK;
            Ok(response)
        }
        // Return the 404 Not Found for other routes.
        _ => {
            let mut not_found = Response::default();
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let server_addr = "0.0.0.0:60000";
    let remote_addr = "0.0.0.0:50000";
    let ice_transport = IceTransport::new().await?;
    let ice_transport_start = ice_transport.clone();
    let (candidate_tx, _candidate_rx) = tokio::sync::mpsc::channel::<RTCIceCandidate>(1);
    tokio::spawn(async move {
        let addr = SocketAddr::from_str(&server_addr).unwrap();
        let ice_transport2 = ice_transport.clone();
        let service = make_service_fn(move |_| {
            let ice_transport3 = ice_transport2.clone();
            async move {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    remote_handler(req, remote_addr.to_string(), ice_transport3.clone())
                }))
            }
        });
        let server = Server::bind(&addr).serve(service);
        // Run this server for... forever!
        if let Err(e) = server.await {
            eprintln!("server error: {}", e);
        }
    });
    ice_transport_start.start_answer(candidate_tx).await;
    Ok(())
}
