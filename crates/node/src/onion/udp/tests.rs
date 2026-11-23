use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
#[cfg(rings_native)]
use std::sync::Arc;
#[cfg(rings_native)]
use std::time::Duration;

use bytes::BytesMut;

use super::*;
#[cfg(rings_native)]
use crate::onion::exit_accounting::OnionExitAccounting;
#[cfg(rings_native)]
use crate::onion::session::dial::OnionClientStream;
#[cfg(rings_native)]
use crate::onion::session::dial::OnionStreamEvent;

#[test]
fn test_udp_frame_codec_chunked_reassembly_and_bounds() -> Result<()> {
    let peer: SocketAddr = "198.51.100.42:54321".parse().unwrap();
    let f1 = OnionUdpClientFrame::AllowPeerIp {
        peer_ip: peer.ip(),
        ttl_ms: 300_000,
    };
    let f2 = OnionUdpClientFrame::SendDatagram {
        peer,
        payload: b"hello-udp".to_vec(),
    };
    let mut wire = encode_client_frame(&f1)?;
    wire.extend_from_slice(&encode_client_frame(&f2)?);

    let mut buf = BytesMut::new();
    buf.extend_from_slice(&wire[..3]);
    assert_eq!(decode_next_client_frame(&mut buf)?, None);
    buf.extend_from_slice(&wire[3..11]);
    assert_eq!(decode_next_client_frame(&mut buf)?, None);
    buf.extend_from_slice(&wire[11..]);
    assert_eq!(decode_next_client_frame(&mut buf)?, Some(f1));
    assert_eq!(decode_next_client_frame(&mut buf)?, Some(f2));
    assert!(buf.is_empty());

    let mut zero_len = BytesMut::from(&[0_u8, 0, 0, 0][..]);
    assert!(decode_next_client_frame(&mut zero_len).is_err());

    let oversized_len = u32::try_from(MAX_ONION_UDP_FRAME_BYTES + 1)
        .unwrap()
        .to_be_bytes();
    let mut oversized = BytesMut::from(&oversized_len[..]);
    assert!(decode_next_server_frame(&mut oversized).is_err());
    Ok(())
}

#[test]
fn test_pinhole_table_ttl_refresh_and_capacity_bound() {
    let mut table = OnionUdpPinholeTable::with_capacity_limit(2);
    let ip1 = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1));
    let ip2 = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 2));
    let ip3 = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 3));

    assert!(!table.allows(ip1, 1_000));
    assert!(table.allow_ip(ip1, 5_000, 1_000));
    assert!(table.allow_ip(ip2, 10_000, 1_000));
    // Capacity limit 2 rejects third distinct live peer IP without evicting existing ones.
    assert!(!table.allow_ip(ip3, 5_000, 1_000));
    assert!(table.allows(ip1, 5_999));
    assert!(!table.allows(ip1, 6_000));

    // Once ip1 expires at t = 6_000, ip3 is admitted after pruning.
    assert!(table.allow_ip(ip3, 5_000, 6_000));
    assert!(table.allows(ip2, 6_000));
    assert!(table.allows(ip3, 10_999));
    assert!(!table.allows(ip3, 11_000));

    // Clamps TTL to MAX_PINHOLE_TTL_MS.
    assert!(table.allow_ip(ip2, u64::MAX, 20_000));
    assert!(table.allows(ip2, 20_000 + u128::from(MAX_PINHOLE_TTL_MS) - 1));
    assert!(!table.allows(ip2, 20_000 + u128::from(MAX_PINHOLE_TTL_MS)));
}

#[test]
fn test_bind_config_endpoint_hint_and_public_ip_enforcement() {
    let cfg = OnionUdpBindConfig::new().with_endpoint_hint_fallback(Some("1.1.1.1:8443"));
    assert_eq!(
        cfg.resolve_advertised_ip(false).unwrap(),
        IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))
    );
    assert!(cfg.resolve_advertised_ip(true).is_err());

    // Documentation (RFC 5737) and private endpoint hints are rejected unless allow_private_peers.
    let doc_cfg = OnionUdpBindConfig::new().with_endpoint_hint_fallback(Some("203.0.113.7:8443"));
    assert!(doc_cfg.resolve_advertised_ip(false).is_err());

    // Private endpoint hint is rejected unless allow_private_peers is set.
    let private_cfg =
        OnionUdpBindConfig::new().with_endpoint_hint_fallback(Some("https://192.168.1.9:8443"));
    assert!(private_cfg.resolve_advertised_ip(false).is_err());

    let ipv6_cfg =
        OnionUdpBindConfig::new().with_endpoint_hint_fallback(Some("https://[2001:db8::9]:8443"));
    // 2001:db8::/32 is documentation space (non-public), so public-only rejects it:
    assert!(ipv6_cfg.resolve_advertised_ip(true).is_err());
    let public_v6_cfg = OnionUdpBindConfig::new()
        .with_endpoint_hint_fallback(Some("https://[2606:4700:4700::1111]:443"));
    assert_eq!(
        public_v6_cfg.resolve_advertised_ip(true).unwrap(),
        IpAddr::V6("2606:4700:4700::1111".parse::<Ipv6Addr>().unwrap())
    );
}

#[cfg(rings_native)]
#[tokio::test]
async fn test_onion_udp_world_pinhole_gate_and_even_port_bind() -> Result<()> {
    let policy = OnionExitPolicy::from_target_strings(vec!["*:*".to_string()], Vec::new())?;
    let clock: SharedClock = Arc::new(rings_core::utils::SystemClock);
    let accounting = OnionExitAccounting::new_with_clock(clock.clone());
    let world = OnionUdpWorld::new(
        policy,
        accounting,
        OnionUdpBindConfig::loopback_for_test(),
        clock,
    );

    let (mut reader, mut writer) = world
        .open(&OnionUdpBindMode::Ipv4EvenPort.to_target())
        .await?;
    let mut initial_buf = BytesMut::new();
    let first_chunk = reader.read(1024).await?.expect("initial Bound frame");
    initial_buf.extend_from_slice(&first_chunk);
    let Some(OnionUdpServerFrame::Bound { local_addr }) =
        decode_next_server_frame(&mut initial_buf)?
    else {
        panic!("expected initial Bound frame");
    };
    assert_eq!(local_addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(local_addr.port() % 2, 0);

    // Unpermitted peer sends datagram first: must be dropped before consuming reader return.
    let unpermitted_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    unpermitted_peer
        .send_to(b"unsolicited-scan", local_addr)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(80), reader.read(1024))
            .await
            .is_err(),
        "unpermitted peer datagram must not pass OnionUdpReader"
    );

    // Now install pinhole for 127.0.0.1 and exchange datagrams in both directions.
    let permitted_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = permitted_peer.local_addr().unwrap();
    writer
        .write(Bytes::from(encode_client_frame(
            &OnionUdpClientFrame::AllowPeerIp {
                peer_ip: peer_addr.ip(),
                ttl_ms: 60_000,
            },
        )?))
        .await?;
    writer
        .write(Bytes::from(encode_client_frame(
            &OnionUdpClientFrame::SendDatagram {
                peer: peer_addr,
                payload: b"ping-exit".to_vec(),
            },
        )?))
        .await?;

    let mut peer_rx = [0_u8; 64];
    let (n, observed_exit_addr) = permitted_peer.recv_from(&mut peer_rx).await.unwrap();
    assert_eq!(&peer_rx[..n], b"ping-exit");
    assert_eq!(observed_exit_addr, local_addr);

    permitted_peer
        .send_to(b"pong-exit", local_addr)
        .await
        .unwrap();
    let reply_chunk = reader.read(1024).await?.expect("inbound datagram frame");
    let mut reply_buf = BytesMut::from(reply_chunk.as_ref());
    assert_eq!(
        decode_next_server_frame(&mut reply_buf)?,
        Some(OnionUdpServerFrame::RecvDatagram {
            peer: peer_addr,
            payload: b"pong-exit".to_vec(),
        })
    );
    Ok(())
}

/// Law (Cancel safety): dropping a pending `recv_from` or `send_to` future (as a `select!` arm
/// that lost its race does) loses neither stream half nor partially buffered frame bytes.
#[cfg(rings_native)]
#[tokio::test]
async fn test_native_onion_udp_session_recv_from_is_cancel_safe() -> Result<()> {
    let (stream, mut driver) = OnionClientStream::driven_by_test();
    let expected_addr: SocketAddr = "203.0.113.10:44444".parse().unwrap();
    driver
        .emit(OnionStreamEvent::Data(Bytes::from(encode_server_frame(
            &OnionUdpServerFrame::Bound {
                local_addr: expected_addr,
            },
        )?)))
        .await;
    let session = NativeOnionUdpSession::from_stream(stream).await?;

    // 1. Cancel `recv_from` on an empty buffer.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), session.recv_from())
            .await
            .is_err()
    );

    // 2. Cancel `recv_from` after consuming the first half of a chunked frame into `state.buffer`.
    let peer: SocketAddr = "198.51.100.7:5000".parse().unwrap();
    let full_frame = encode_server_frame(&OnionUdpServerFrame::RecvDatagram {
        peer,
        payload: b"after-partial-cancel".to_vec(),
    })?;
    driver
        .emit(OnionStreamEvent::Data(Bytes::copy_from_slice(
            &full_frame[..6],
        )))
        .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), session.recv_from())
            .await
            .is_err()
    );
    driver
        .emit(OnionStreamEvent::Data(Bytes::copy_from_slice(
            &full_frame[6..],
        )))
        .await;
    let next = tokio::time::timeout(Duration::from_millis(500), session.recv_from())
        .await
        .expect("recv_from must reassemble partial frame across cancellation")?;
    assert_eq!(
        next,
        Some((Bytes::from_static(b"after-partial-cancel"), peer))
    );

    // 3. Fill the outbound command queue until `send_to` parks on backpressure, cancel `send_to`,
    //    drain one command, and verify `send_to` remains live without corrupting framing.
    let mut filled = 0_usize;
    loop {
        if tokio::time::timeout(Duration::from_millis(15), session.send_to(b"fill", peer))
            .await
            .is_err()
        {
            break;
        }
        filled += 1;
        assert!(filled <= 256, "command channel must be bounded");
    }
    assert!(driver.next_data().await.is_some());
    let sent = tokio::time::timeout(
        Duration::from_millis(500),
        session.send_to(b"resumed", peer),
    )
    .await
    .expect("send_to must remain live after cancelled backpressure wait")?;
    assert_eq!(sent, b"resumed".len());
    Ok(())
}

#[cfg(rings_native)]
#[test]
fn test_outbound_datagram_rate_window_caps_per_second_and_resets_next_second() {
    let mut window = OutboundDatagramWindow::default();
    for _ in 0..MAX_OUTBOUND_DATAGRAMS_PER_SEC {
        assert!(window.try_admit(5_250));
    }
    assert!(!window.try_admit(5_999));
    // Advancing into the next 1-second bucket resets the counter.
    assert!(window.try_admit(6_000));
}

#[cfg(rings_native)]
#[tokio::test]
async fn test_native_onion_udp_session_close_wakes_parked_reader_immediately() -> Result<()> {
    let (stream, mut driver) = OnionClientStream::driven_by_test();
    let expected_addr: SocketAddr = "203.0.113.10:44444".parse().unwrap();
    let bound_frame = encode_server_frame(&OnionUdpServerFrame::Bound {
        local_addr: expected_addr,
    })?;
    driver
        .emit(OnionStreamEvent::Data(Bytes::from(bound_frame)))
        .await;

    let session = Arc::new(NativeOnionUdpSession::from_stream(stream).await?);
    assert_eq!(session.local_addr(), expected_addr);

    let recv_session = Arc::clone(&session);
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move { recv_session.recv_from().await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Driver remains alive and silent; close() must still wake recv_from() immediately.
    session.close();
    let joined = tokio::time::timeout(Duration::from_millis(100), tasks.join_next())
        .await
        .expect("recv_from must wake promptly on close()")
        .expect("task joined")
        .expect("task did not panic")?;
    assert_eq!(joined, None);
    drop(driver);
    Ok(())
}

#[cfg(rings_native)]
#[tokio::test]
async fn test_onion_udp_world_admits_port_restricted_policy_and_enforces_per_datagram() -> Result<()>
{
    let allowed_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let allowed_addr = allowed_peer.local_addr().unwrap();
    let denied_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let denied_addr = denied_peer.local_addr().unwrap();
    assert_ne!(allowed_addr.port(), denied_addr.port());

    let policy = OnionExitPolicy::from_target_strings(
        vec![format!("*:{}", allowed_addr.port())],
        Vec::new(),
    )?;
    let clock: SharedClock = Arc::new(rings_core::utils::SystemClock);
    let accounting = OnionExitAccounting::new_with_clock(clock.clone());
    let world = OnionUdpWorld::new(
        policy.clone(),
        accounting,
        OnionUdpBindConfig::loopback_for_test(),
        clock,
    );

    // Session-open admission (`admits_open` + `world.open`) must accept `0.0.0.0:1` under `*:port`.
    let bind_target = OnionUdpBindMode::Ipv4AnyPort.to_target();
    assert!(world.admits_open(&policy, &bind_target));
    assert!(!world.admits_open(&policy, &OnionProxyTarget::parse_authority("127.0.0.1:53")?));

    let (mut reader, mut writer) = world.open(&bind_target).await?;
    let first_chunk = reader.read(1024).await?.expect("initial Bound frame");
    let mut initial_buf = BytesMut::from(first_chunk.as_ref());
    let Some(OnionUdpServerFrame::Bound { local_addr }) =
        decode_next_server_frame(&mut initial_buf)?
    else {
        panic!("expected initial Bound frame");
    };

    writer
        .write(Bytes::from(encode_client_frame(
            &OnionUdpClientFrame::AllowPeerIp {
                peer_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
                ttl_ms: 60_000,
            },
        )?))
        .await?;

    // Outbound to disallowed port is dropped by per-datagram policy; outbound to allowed port passes.
    writer
        .write(Bytes::from(encode_client_frame(
            &OnionUdpClientFrame::SendDatagram {
                peer: denied_addr,
                payload: b"blocked".to_vec(),
            },
        )?))
        .await?;
    assert!(tokio::time::timeout(
        Duration::from_millis(50),
        denied_peer.recv_from(&mut [0u8; 16])
    )
    .await
    .is_err());

    writer
        .write(Bytes::from(encode_client_frame(
            &OnionUdpClientFrame::SendDatagram {
                peer: allowed_addr,
                payload: b"allowed".to_vec(),
            },
        )?))
        .await?;
    let mut rx = [0u8; 16];
    let (n, _) = allowed_peer.recv_from(&mut rx).await.unwrap();
    assert_eq!(&rx[..n], b"allowed");

    // Inbound from disallowed port is dropped even though 127.0.0.1 has a pinhole; allowed port passes.
    denied_peer.send_to(b"denied-in", local_addr).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), reader.read(1024))
            .await
            .is_err()
    );
    allowed_peer
        .send_to(b"allowed-in", local_addr)
        .await
        .unwrap();
    let chunk = reader.read(1024).await?.expect("allowed inbound frame");
    let mut buf = BytesMut::from(chunk.as_ref());
    assert_eq!(
        decode_next_server_frame(&mut buf)?,
        Some(OnionUdpServerFrame::RecvDatagram {
            peer: allowed_addr,
            payload: b"allowed-in".to_vec(),
        })
    );
    Ok(())
}
