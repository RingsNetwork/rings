use anyhow;
use hyper::{Body, Client, Method, Request};
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;

pub async fn send_candidate(addr: &str, candidate: &RTCIceCandidate) -> anyhow::Result<()> {
    let payload = candidate.to_json().await?.candidate;
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{}/candidate", addr))
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(payload))
        .unwrap();
    Client::new().request(req).await.unwrap();
    Ok(())
}
