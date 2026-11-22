use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rings_core::utils::ManualClock;
use rings_test_support::with_hang_guard;
use tokio::net::UdpSocket;

use super::codec::OnionUdpFrameDecoder;
use super::OnionUdpBindRequest;
use super::OnionUdpClientFrame;
use super::OnionUdpConfig;
use super::OnionUdpExitConfig;
use super::OnionUdpExitFrame;
use super::OnionUdpFamily;
use super::OnionUdpPeerPolicy;
use super::OnionUdpPinholes;
use super::OnionUdpSession;
use super::OnionUdpWorld;
use super::DEFAULT_UDP_PINHOLE_TTL_MS;
use crate::error::Result;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::session::dial::OnionClientStream;
use crate::onion::session::dial::OnionStreamEvent;
use crate::onion::session::serve::OnionWorld;
use crate::onion::session::serve::OnionWorldReader;
use crate::onion::session::serve::OnionWorldWriter;
use crate::onion::OnionExitPolicy;

#[test]
fn test_public_only_peer_policy_and_yaml_config_reject_non_public_ips() {
    let policy = OnionUdpPeerPolicy::PublicOnly;
    for rejected in [
        "127.0.0.1:5000",
        "10.0.0.1:5000",
        "172.16.0.1:5000",
        "192.168.1.1:5000",
        "169.254.1.1:5000",
        "100.64.0.1:5000",
        "0.0.0.0:5000",
        "192.0.2.1:5000",
        "224.0.0.1:5000",
        "255.255.255.255:5000",
        "1.1.1.1:0",
        "[::1]:5000",
        "[::]:5000",
        "[fe80::1]:5000",
        "[fc00::1]:5000",
        "[fd12:3456::1]:5000",
        "[::ffff:127.0.0.1]:5000",
        "[2001:db8::1]:5000",
    ] {
        let addr: SocketAddr = rejected.parse().expect("valid socket addr");
        assert!(
            !policy.allows_addr(addr),
            "PublicOnly must reject {rejected}"
        );
    }

    for accepted in ["1.1.1.1:53", "8.8.8.8:3478", "[2606:4700:4700::1111]:53"] {
        let addr: SocketAddr = accepted.parse().expect("valid socket addr");
        assert!(policy.allows_addr(addr), "PublicOnly must allow {accepted}");
    }

    let bad_yaml = OnionUdpExitConfig {
        advertised_ipv4: Some(Ipv4Addr::LOCALHOST),
        ..OnionUdpExitConfig::default()
    };
    assert!(bad_yaml.to_runtime_config().is_err());

    let nat_without_adv = OnionUdpExitConfig {
        bind_ipv4: Some(Ipv4Addr::new(10, 0, 0, 5)),
        advertised_ipv4: None,
        ..OnionUdpExitConfig::default()
    };
    assert!(nat_without_adv.to_runtime_config().is_err());

    let valid_nat = OnionUdpExitConfig {
        bind_ipv4: Some(Ipv4Addr::new(10, 0, 0, 5)),
        advertised_ipv4: Some(Ipv4Addr::new(1, 1, 1, 1)),
        min_port: Some(40_000),
        max_port: Some(40_100),
        ..OnionUdpExitConfig::default()
    };
    let runtime_cfg = valid_nat.to_runtime_config().expect("valid NAT config");
    assert_eq!(runtime_cfg.peer_policy, OnionUdpPeerPolicy::PublicOnly);
    assert_eq!(runtime_cfg.port_range, Some((40_000, 40_100)));
}

#[test]
fn test_codec_round_trips_split_and_coalesced_chunks_and_rejects_malformed() -> Result<()> {
    let client_frames = vec![
        OnionUdpClientFrame::AllowPeer {
            ip: IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
        },
        OnionUdpClientFrame::AllowPeer {
            ip: IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 1)),
        },
        OnionUdpClientFrame::Datagram {
            peer: "8.8.8.8:5353".parse().unwrap(),
            payload: Bytes::from_static(b"hello-v4"),
        },
        OnionUdpClientFrame::Datagram {
            peer: "[2606:4700::1111]:9000".parse().unwrap(),
            payload: Bytes::from(vec![0xab; 1200]),
        },
    ];
    let mut wire = Vec::new();
    for frame in &client_frames {
        wire.extend_from_slice(&frame.encode()?);
    }

    let mut decoder = OnionUdpFrameDecoder::default();
    let mut decoded = Vec::new();
    for byte in &wire {
        decoder.push(std::slice::from_ref(byte))?;
        while let Some(frame) = decoder.next_client_frame()? {
            decoded.push(frame);
        }
    }
    assert_eq!(decoded, client_frames);

    // Zero-length body and oversized body headers are rejected immediately.
    let mut zero_dec = OnionUdpFrameDecoder::default();
    assert!(zero_dec.push(&[0, 0, 0, 0]).is_err());
    let mut huge_dec = OnionUdpFrameDecoder::default();
    assert!(huge_dec.push(&0x0100_0000_u32.to_be_bytes()).is_err());

    Ok(())
}

#[test]
fn test_pinhole_table_bounds_entries_and_expires_deterministically() {
    let mut pinholes = OnionUdpPinholes::new(1_000, 2);
    let ip1 = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
    let ip2 = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
    let ip3 = IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9));

    pinholes.allow_ip(ip1, 100);
    pinholes.allow_ip(ip2, 200);
    assert!(pinholes.admits(ip1, 500));
    assert!(pinholes.admits(ip2, 500));
    assert_eq!(pinholes.len(), 2);

    // Adding a 3rd distinct IP evicts the earliest-expiring entry (`ip1`, exp=1100).
    pinholes.allow_ip(ip3, 300);
    assert_eq!(pinholes.len(), 2);
    assert!(!pinholes.admits(ip1, 500));
    assert!(pinholes.admits(ip2, 500));
    assert!(pinholes.admits(ip3, 500));

    // Past `200 + 1_000 = 1_200`, `ip2` expires while `ip3` (`exp = 1_300`) stays live.
    assert!(!pinholes.admits(ip2, 1_201));
    assert!(pinholes.admits(ip3, 1_201));
    assert!(!pinholes.admits(ip3, 1_301));
}

#[tokio::test]
async fn test_udp_world_drops_unsolicited_packets_without_charging_bytes_and_relays_permitted(
) -> Result<()> {
    let t0: u128 = 1_700_000_000_000;
    let clock = Arc::new(ManualClock::new(t0));
    // Budget of 64 bytes/minute: if unsolicited packets were charged, 5 unsolicited 40-byte scan
    // packets (200 bytes) would exhaust the budget and abort the session!
    let mut exit_policy =
        OnionExitPolicy::from_target_strings(vec!["*:*".to_string()], Vec::new())?;
    exit_policy.max_bytes_per_minute = 64;
    let accounting = OnionExitAccounting::new_with_clock(clock.clone());
    let world = OnionUdpWorld::with_config_and_clock(
        exit_policy,
        accounting,
        OnionUdpConfig::loopback(),
        clock.clone(),
    );

    let bind_req = OnionUdpBindRequest {
        family: OnionUdpFamily::Ipv4,
        even_port: true,
    };
    let (mut reader, mut writer) = world.open(&bind_req.to_proxy_target()?).await?;

    // First read delivers `Bound { local_addr }` with an even port (`12` bytes total).
    let first_bytes = reader.read(1024).await?.expect("Bound frame");
    let mut exit_dec = OnionUdpFrameDecoder::default();
    exit_dec.push(&first_bytes)?;
    let Some(OnionUdpExitFrame::Bound { local_addr: e_exit }) = exit_dec.next_exit_frame()? else {
        panic!("expected initial Bound frame");
    };
    assert_eq!(e_exit.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(e_exit.port() % 2, 0);

    // Unsolicited internet scanner blasts 5 packets (200 bytes) at `e_exit` before any pinhole
    // exists, followed by an authorized peer after `AllowPeer`.
    let scanner = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for _ in 0..5 {
        scanner.send_to(&[0xde; 40], e_exit).await.unwrap();
    }
    // Give the kernel loopback queue a moment so the unsolicited packets sit in `e_exit`'s socket
    // receive buffer when `reader.read` runs.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Install pinhole for `127.0.0.1` via `AllowPeer`: `reader` must skip/drop the 5 unsolicited
    // packets if they arrived before pinhole, wait — here scanner is ALSO `127.0.0.1`, so let's
    // verify `reader.read` BEFORE `AllowPeer` drops all 5 packets without returning!
    let read_before_pinhole =
        tokio::time::timeout(Duration::from_millis(60), reader.read(1024)).await;
    assert!(
        read_before_pinhole.is_err(),
        "unsolicited packets without a pinhole must be dropped inside read() without returning bytes"
    );

    // Now install pinhole via `AllowPeer` -> confirm `PeerAllowed` arrives and subsequent peer
    // datagrams pass within the tight 64-byte accounting budget!
    writer
        .write(
            OnionUdpClientFrame::AllowPeer {
                ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            }
            .encode()?,
        )
        .await?;
    let ack_bytes = with_hang_guard("PeerAllowed frame", Duration::from_secs(2), reader.read(64))
        .await?
        .expect("PeerAllowed bytes");
    exit_dec.push(&ack_bytes)?;
    assert_eq!(
        exit_dec.next_exit_frame()?,
        Some(OnionUdpExitFrame::PeerAllowed {
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST)
        })
    );

    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer.local_addr().unwrap();
    peer.send_to(b"ping-udp", e_exit).await.unwrap();

    let inbound_bytes = with_hang_guard(
        "inbound permitted datagram",
        Duration::from_secs(2),
        reader.read(1024),
    )
    .await?
    .expect("Datagram bytes");
    exit_dec.push(&inbound_bytes)?;
    assert_eq!(
        exit_dec.next_exit_frame()?,
        Some(OnionUdpExitFrame::Datagram {
            peer: peer_addr,
            payload: Bytes::from_static(b"ping-udp"),
        })
    );

    // Advance clock past `DEFAULT_UDP_PINHOLE_TTL_MS` so the pinhole expires, and verify inbound
    // packets are dropped again!
    clock.set(t0 + DEFAULT_UDP_PINHOLE_TTL_MS + 1);
    peer.send_to(b"after-expiry", e_exit).await.unwrap();
    let after_expiry = tokio::time::timeout(Duration::from_millis(60), reader.read(1024)).await;
    assert!(
        after_expiry.is_err(),
        "expired pinhole must drop inbound datagrams"
    );

    // Outbound datagram refreshes the pinhole and reaches `peer` from `e_exit`!
    writer
        .write(
            OnionUdpClientFrame::Datagram {
                peer: peer_addr,
                payload: Bytes::from_static(b"outbound-pong"),
            }
            .encode()?,
        )
        .await?;
    let mut recv_buf = [0_u8; 64];
    let (n, observed_src) = with_hang_guard(
        "peer recv outbound",
        Duration::from_secs(2),
        peer.recv_from(&mut recv_buf),
    )
    .await
    .unwrap();
    assert_eq!(observed_src, e_exit);
    assert_eq!(&recv_buf[..n], b"outbound-pong");

    writer.shutdown().await?;
    assert_eq!(reader.read(64).await?, None);
    Ok(())
}

#[tokio::test]
async fn test_session_close_never_blocks_on_dead_reader() -> Result<()> {
    let (stream, mut driver) = OnionClientStream::driven_by_test();
    let bind_req = OnionUdpBindRequest {
        family: OnionUdpFamily::Ipv4,
        even_port: false,
    };
    let bound_addr: SocketAddr = "1.1.1.1:45000".parse().unwrap();
    driver
        .emit(OnionStreamEvent::Data(
            OnionUdpExitFrame::Bound {
                local_addr: bound_addr,
            }
            .encode()?,
        ))
        .await;

    let session = Arc::new(OnionUdpSession::from_stream(stream, bind_req).await?);
    assert_eq!(session.local_addr(), bound_addr);

    // Park a reader on `recv_from` AND a caller on `allow_peer_ip` while `driver` stays completely
    // silent (simulating a dead upstream circuit), and trigger `close()` after 15 ms.
    let s_recv = Arc::clone(&session);
    let s_allow = Arc::clone(&session);
    let s_close = Arc::clone(&session);
    with_hang_guard(
        "non-blocking close on dead reader",
        Duration::from_millis(200),
        async move {
            let recv_fut = async move { s_recv.recv_from().await };
            let allow_fut = async move { s_allow.allow_peer_ip("8.8.8.8".parse().unwrap()).await };
            let close_fut = async move {
                tokio::time::sleep(Duration::from_millis(15)).await;
                s_close.close().await;
            };
            let (recv_res, allow_res, ()) = futures::join!(recv_fut, allow_fut, close_fut);
            assert!(recv_res.is_err());
            assert!(allow_res.is_err());
        },
    )
    .await;
    Ok(())
}

#[tokio::test]
async fn test_udp_writer_rejects_unbounded_allow_peer_ack_flood_and_coalesces_batches() -> Result<()>
{
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let exit_policy = OnionExitPolicy::from_target_strings(vec!["*:*".to_string()], Vec::new())?;
    let accounting = OnionExitAccounting::new_with_clock(clock.clone());
    let world = OnionUdpWorld::with_config_and_clock(
        exit_policy,
        accounting,
        OnionUdpConfig::loopback(),
        clock,
    );
    let bind_req = OnionUdpBindRequest {
        family: OnionUdpFamily::Ipv4,
        even_port: false,
    };
    let (mut reader, mut writer) = world.open(&bind_req.to_proxy_target()?).await?;
    // Drain initial `Bound` frame.
    let _ = reader.read(1024).await?.expect("initial Bound frame");

    // 16 batched `AllowPeer` frames coalesce into a single `reader.read(4096)` chunk.
    let mut batch_16 = Vec::new();
    for idx in 1..=16_u8 {
        batch_16.extend_from_slice(
            &OnionUdpClientFrame::AllowPeer {
                ip: IpAddr::V4(Ipv4Addr::new(8, 8, 8, idx)),
            }
            .encode()?,
        );
    }
    writer.write(Bytes::from(batch_16)).await?;
    let coalesced = reader.read(4096).await?.expect("coalesced PeerAllowed");
    let mut dec = OnionUdpFrameDecoder::default();
    dec.push(&coalesced)?;
    let mut count = 0_usize;
    while let Some(OnionUdpExitFrame::PeerAllowed { .. }) = dec.next_exit_frame()? {
        count += 1;
    }
    assert_eq!(count, 16);

    // Flooding `> DEFAULT_MAX_UDP_PINHOLES` (`257`) unconsumed `AllowPeer` frames without draining
    // `reader` must fail closed (`Err`) instead of growing `control_out` without bound.
    let mut flood = Vec::new();
    for _ in 0..=super::DEFAULT_MAX_UDP_PINHOLES {
        flood.extend_from_slice(
            &OnionUdpClientFrame::AllowPeer {
                ip: IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            }
            .encode()?,
        );
    }
    assert!(
        writer.write(Bytes::from(flood)).await.is_err(),
        "AllowPeer ack flood exceeding MAX_PENDING_CONTROL_ACKS must fail the session"
    );
    Ok(())
}

#[tokio::test]
async fn test_allow_peer_ip_cancellation_cleans_up_pending_pinhole_waiter() -> Result<()> {
    let (stream, mut driver) = OnionClientStream::driven_by_test();
    let bind_req = OnionUdpBindRequest {
        family: OnionUdpFamily::Ipv4,
        even_port: false,
    };
    let bound_addr: SocketAddr = "1.1.1.1:45002".parse().unwrap();
    driver
        .emit(OnionStreamEvent::Data(
            OnionUdpExitFrame::Bound {
                local_addr: bound_addr,
            }
            .encode()?,
        ))
        .await;

    let session = Arc::new(OnionUdpSession::from_stream(stream, bind_req).await?);
    for idx in 1..=8_u8 {
        let ip = IpAddr::V4(Ipv4Addr::new(9, 9, 9, idx));
        let timed_out =
            tokio::time::timeout(Duration::from_millis(10), session.allow_peer_ip(ip)).await;
        assert!(
            timed_out.is_err(),
            "silent driver must time out outer caller"
        );
        assert_eq!(
            session.pending_pinhole_count(),
            0,
            "cancelled allow_peer_ip must remove its pending waiter immediately"
        );
    }
    Ok(())
}
