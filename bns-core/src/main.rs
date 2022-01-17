use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Client, Method, Request, Response, Server, StatusCode};
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::math_rand_alpha;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use bns_core::ice_transport::IceTransport;
use bns_core::ice_transport::IceTransportImpl;

pub async fn signal_candidate(addr: &str, c: &RTCIceCandidate) -> Result<()> {
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

        (&Method::POST, "/sdp") => {
            println!("remote_handler receive from /sdp");
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
    let http_addr = "0.0.0.0:60000";
    let remote_addr = "0.0.0.0:50000";

    let ice_transport = IceTransport::new().await?;
    let peer_connection = Arc::downgrade(&ice_transport.get_peer_connection().await.unwrap());
    let pending_candidates = ice_transport.get_pending_candidates().await;
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);

    ice_transport
        .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let peer_connection = peer_connection.clone();
            let pending_candidates = Arc::clone(&pending_candidates);
            Box::pin(async move {
                if let Some(candidate) = c {
                    if let Some(peer_connection) = peer_connection.upgrade() {
                        let desc = peer_connection.remote_description().await;
                        if desc.is_none() {
                            let mut candidates = pending_candidates.lock().await;
                            println!("start answer candidate: {:?}", candidate);
                            candidates.push(candidate.clone());
                        }
                    }
                }
            })
        }))
        .await?;
    ice_transport
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            // Failed to exit dial server
            if s == RTCPeerConnectionState::Failed {
                let _ = done_tx.try_send(());
            }

            Box::pin(async {})
        }))
        .await?;
    ice_transport
        .on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
            let d_label = d.label().to_owned();
            let d_id = d.id();
            println!("New DataChannel {} {}", d_label, d_id);
            Box::pin(async move{
                // Register channel opening handling
                let d2 =  Arc::clone(&d);
                let d_label2 = d_label.clone();
                let d_id2 = d_id;
                d.on_open(Box::new(move || {
                    println!("Data channel '{}'-'{}' open. ", d_label2, d_id2);
                    print!("Random messages will now be sent to any connected DataChannels every 5 seconds");
                    Box::pin(async move {
                        let mut result = Result::<usize>::Ok(0);
                        while result.is_ok() {
                            let timeout = tokio::time::sleep(Duration::from_secs(5));
                            tokio::pin!(timeout);

                            tokio::select! {
                                _ = timeout.as_mut() =>{
                                    let message = math_rand_alpha(15);
                                    println!("Sending '{}'", message);
                                    result = d2.send_text(message).await.map_err(Into::into);
                                }
                            };
                        }
                    })
                })).await;

                // Register text message handling
                d.on_message(Box::new(move |msg: DataChannelMessage| {
                    let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
                    println!("Message from DataChannel '{}': '{}'", d_label, msg_str);
                    Box::pin(async{})
                })).await;
            })
        }))
        .await?;

    tokio::spawn(async move {
        let ice_transport = ice_transport.clone();
        let service = make_service_fn(move |_| {
            let ice_transport = ice_transport.to_owned();
            async move {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    remote_handler(req, remote_addr.to_string(), ice_transport.to_owned())
                }))
            }
        });

        let http_addr = SocketAddr::from_str(http_addr).unwrap();
        let server = Server::bind(&http_addr).serve(service);
        // Run this server for... forever!
        if let Err(e) = server.await {
            eprintln!("server error: {}", e);
        }
    });

    tokio::select! {
        _ = done_rx.recv() => {
            println!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
            println!("");
        }
    };
    Ok(())
}
