use anyhow::Result;
use clap::{App, AppSettings, Arg};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Client, Method, Request, Response, Server, StatusCode};
use std::io::Write;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::Duration;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::peer_connection::math_rand_alpha;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use bns_core::ice_transport::IceTransport;

async fn signal_candidate2(addr: String, c: RTCIceCandidate) -> Result<()> {
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

    Ok(())
}

async fn remote_handler2(
    req: Request<Body>,
    addr: String,
    ice_transport: IceTransport,
) -> Result<Response<Body>, hyper::Error> {
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/candidate") => {
            let candidate =
                match std::str::from_utf8(&hyper::body::to_bytes(req.into_body()).await?) {
                    Ok(s) => s.to_owned(),
                    Err(err) => panic!("{}", err),
                };

            ice_transport.add_ice_candidate(candidate).await;
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::OK;
            Ok(response)
        }

        (&Method::POST, "/sdp") => {
            let sdp_str = match std::str::from_utf8(&hyper::body::to_bytes(req.into_body()).await?)
            {
                Ok(s) => s.to_owned(),
                Err(err) => panic!("{}", err),
            };
            let sdp = match serde_json::from_str::<RTCSessionDescription>(&sdp_str) {
                Ok(s) => s,
                Err(err) => panic!("{}", err),
            };

            ice_transport.set_remote_description(sdp).await;

            let answer = match ice_transport
                .conn
                .lock()
                .await
                .clone()
                .unwrap()
                .create_answer(None)
                .await
            {
                Ok(a) => a,
                Err(err) => panic!("{}", err),
            };

            println!("{:?}", answer);

            let payload = match serde_json::to_string(&answer) {
                Ok(p) => p,
                Err(err) => panic!("{}", err),
            };

            let req = match Request::builder()
                .method(Method::POST)
                .uri(format!("http://{}/sdp", addr.clone()))
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

            ice_transport.set_local_description(answer).await;
            {
                let cs = ice_transport.ice_candidates.lock().await;
                for c in &*cs {
                    if let Err(err) = signal_candidate(addr.clone(), c).await {
                        panic!("{}", err);
                    }
                }
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

async fn signal_candidate(addr: String, c: &RTCIceCandidate) -> Result<()> {
    /*println!(
        "signal_candidate Post candidate to {}",
        format!("http://{}/candidate", addr)
    );*/
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

// HTTP Listener to get ICE Credentials/Candidate from remote Peer
async fn remote_handler(req: Request<Body>, ) -> Result<Response<Body>, hyper::Error> {
    let pc = {
        let pcm = PEER_CONNECTION_MUTEX.lock().await;
        pcm.clone().unwrap()
    };
    let addr = {
        let addr = ADDRESS.lock().await;
        addr.clone()
    };

    match (req.method(), req.uri().path()) {
        // A HTTP handler that allows the other WebRTC-rs or Pion instance to send us ICE candidates
        // This allows us to add ICE candidates faster, we don't have to wait for STUN or TURN
        // candidates which may be slower
        (&Method::POST, "/candidate") => {
            //println!("remote_handler receive from /candidate");
            let candidate =
                match std::str::from_utf8(&hyper::body::to_bytes(req.into_body()).await?) {
                    Ok(s) => s.to_owned(),
                    Err(err) => panic!("{}", err),
                };

            if let Err(err) = pc
                .add_ice_candidate(RTCIceCandidateInit {&
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

        // A HTTP handler that processes a SessionDescription given to us from the other WebRTC-rs or Pion process
        (&Method::POST, "/sdp") => {
            //println!("remote_handler receive from /sdp");
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
            println!("{:?}", answer);
            /*println!(
                "remote_handler Post answer to {}",
                format!("http://{}/sdp", addr)
            );*/

            // Send our answer to the HTTP server listening in the other process
            let payload = match serde_json::to_string(&answer) {
                Ok(p) => p,
                Err(err) => panic!("{}", err),
            };

            let req = match Request::builder()
                .method(Method::POST)
                .uri(format!("http://{}/sdp", addr))
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
            //println!("remote_handler Response: {}", resp.status());

            // Sets the LocalDescription, and starts our UDP listeners
            if let Err(err) = pc.set_local_description(answer).await {
                panic!("{}", err);
            }

            {
                let cs = PENDING_CANDIDATES.lock().await;
                for c in &*cs {
                    if let Err(err) = signal_candidate(addr.clone(), c).await {
                        panic!("{}", err);
                    }
                }
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
    let p0_addr = String::from("0.0.0.0:60000");
    let p1_addr = String::from("0.0.0.0:50000");
    let ice_transport = IceTransport::new(vec![]).await?;

    let mut ice_transport_dc = ice_transport.clone();
    let peer_connection = ice_transport_dc.conn.lock().await.clone().unwrap();
    let pending_candidate = ice_transport_dc.clone().ice_candidates;

    let pending_candidates2 = Arc::clone(&pending_candidate);
    let pc = Arc::downgrade(&peer_connection);
    peer_connection
        .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            println!("on_ice_candidate {:?}", c);

            let pc2 = pc.clone();
            let pending_candidates3 = Arc::clone(&pending_candidates2);
            let addr3 = p1_addr.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    if let Some(pc) = pc2.upgrade() {
                        let desc = pc.remote_description().await;
                        if desc.is_none() {
                            let mut cs = pending_candidates3.lock().await;
                            cs.push(c);
                        } else if let Err(err) = signal_candidate(addr3, &c).await {
                            assert!(false, "{}", err);
                        }
                    }
                }
            })
        }))
        .await;
    tokio::spawn(async move {
        let addr = SocketAddr::from_str(&p0_addr).unwrap();
        let service = make_service_fn(|_| {
            let p0_addr2 = p0_addr.clone();
            let ice_transport2 = ice_transport.clone();
            async move { Ok::<_, hyper::Error>(service_fn(move |req| remote_handler(req, addr, ice_transport))) }
        });
        let server = Server::bind(&addr).serve(service);
        println!("Run this server for ... forever!");
        // Run this server for... forever!
        if let Err(e) = server.await {
            println!("server error: {}", e);
        }
    });

    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);

    ice_transport_dc
        .conn
        .lock()
        .await
        .clone()
        .unwrap()
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            println!("Peer Connection State has changed: {}", s);

            if s == RTCPeerConnectionState::Failed {
                println!("Peer Connection has gone to failed exiting");
                let _ = done_tx.try_send(());
            }

            Box::pin(async {})
        }))
        .await;

    ice_transport_dc.clone().on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
        let d_label = d.label().to_owned();
        let d_id = d.id();
        println!("New DataChannel {} {}", d_label, d_id);

        Box::pin(async move{
            // Register channel opening handling
            let d2 =  Arc::clone(&d);
            let d_label2 = d_label.clone();
            let d_id2 = d_id;
            d.on_open(Box::new(move || {
                println!("Data channel '{}'-'{}' open. Random messages will now be sent to any connected DataChannels every 5 seconds", d_label2, d_id2);
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
    })).await;

    println!("Press ctrl-c to stop");
    tokio::select! {
        _ = done_rx.recv() => {
            println!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
            println!("");
        }
    };

    ice_transport_dc
        .conn
        .lock()
        .await
        .clone()
        .unwrap()
        .close()
        .await;
    Ok(())
}
