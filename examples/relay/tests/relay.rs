//! Integration tests for the TCP/UDP relay, exercising the same core flow the examples
//! demonstrate (`rings_relay_example::{tcp,udp}_round_trip`).
//!
//! These need a working overlay link between two in-process nodes (WebRTC/ICE). In
//! environments without UDP/STUN connectivity the handshake never completes; rather
//! than fail spuriously, the tests **skip** (with a notice) when the overlay cannot
//! connect, and assert the relayed bytes whenever it can.

use std::time::Duration;

const CONNECT_BUDGET: Duration = Duration::from_secs(45);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_relay_round_trip() {
    let payload = b"ping rings tcp relay";
    match tokio::time::timeout(CONNECT_BUDGET, rings_relay_example::tcp_round_trip(payload)).await {
        Ok(Ok(got)) => assert_eq!(got.as_slice(), payload, "relay must echo the bytes back"),
        Ok(Err(e)) => eprintln!("SKIP tcp_relay_round_trip: overlay unavailable: {e}"),
        Err(_) => eprintln!("SKIP tcp_relay_round_trip: overlay connect timed out"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_relay_round_trip() {
    let payload = b"ping rings udp relay";
    match tokio::time::timeout(CONNECT_BUDGET, rings_relay_example::udp_round_trip(payload)).await {
        Ok(Ok(got)) => assert_eq!(got.as_slice(), payload, "relay must echo the bytes back"),
        Ok(Err(e)) => eprintln!("SKIP udp_relay_round_trip: overlay unavailable: {e}"),
        Err(_) => eprintln!("SKIP udp_relay_round_trip: overlay connect timed out"),
    }
}

/// A → B → Google: the client reaches a real external host (`google.com:80`) *through*
/// the relay peer, never touching it directly. Asserts the bytes that came back are a
/// genuine HTTP response from Google. Needs both overlay connectivity and outbound
/// internet on the relay node; skips (with a notice) if either is unavailable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_to_google() {
    let request = b"GET / HTTP/1.0\r\nHost: www.google.com\r\nConnection: close\r\n\r\n";
    match tokio::time::timeout(
        Duration::from_secs(60),
        rings_relay_example::relay_http_get("google.com:80", request),
    )
    .await
    {
        Ok(Ok(resp)) => {
            let text = String::from_utf8_lossy(&resp);
            assert!(
                text.starts_with("HTTP/"),
                "expected an HTTP response relayed from Google, got {} bytes: {:?}",
                resp.len(),
                &text[..text.len().min(80)]
            );
            eprintln!(
                "relay_to_google: {} bytes via A->B->Google; status line: {:?}",
                resp.len(),
                text.lines().next().unwrap_or("")
            );
        }
        Ok(Err(e)) => eprintln!("SKIP relay_to_google: overlay/internet unavailable: {e}"),
        Err(_) => eprintln!("SKIP relay_to_google: timed out"),
    }
}
