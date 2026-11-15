#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

#[cfg(target_family = "wasm")]
compile_error!("rings-turn is a native-only crate: it binds UDP TURN and relay sockets");

mod auth;
mod config;
mod error;
mod server;

pub use auth::DelegationAuthHandler;
pub use auth::TurnAdmissionPolicy;
pub use auth::DEFAULT_VERIFIED_DELEGATION_CACHE_CAPACITY;
pub use config::RelayAddressConfig;
pub use config::RelayPeerPolicy;
pub use config::TurnServerConfig;
pub use config::DEFAULT_MAX_NONCES;
pub use config::DEFAULT_MAX_PERMISSIONS_PER_ALLOCATION;
pub use config::DEFAULT_PORT_RANGE_MAX_RETRIES;
pub use config::DEFAULT_TURN_PORT;
pub use config::DEFAULT_TURN_REALM;
pub use error::Result;
pub use error::TurnError;
pub use rings_core::delegation::extract_turn_server_pubkey;
pub use rings_core::delegation::format_turn_server_url;
pub use rings_core::delegation::pack_turn_account_auth;
pub use rings_core::delegation::pack_turn_auth;
pub use rings_core::delegation::pack_turn_did_auth;
pub use rings_core::delegation::TurnAuthIdentity;
pub use rings_core::delegation::TurnCredentials;
pub use rings_core::delegation::VerifiedTurnAuth;
pub use rings_core::delegation::MAX_STUN_USERNAME_BYTES;
pub use rings_core::delegation::TURN_AUTH_DOMAIN;
pub use server::TurnServer;

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::net::Ipv4Addr;
    use std::net::Ipv6Addr;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    use rings_core::delegation::DelegateeKey;
    use rings_core::dht::Did;
    use rings_core::ecc::SecretKey;
    use rings_core::utils::Clock;
    use rings_core::utils::ManualClock;
    use stun::agent::TransactionId;
    use stun::attributes::ATTR_EVEN_PORT;
    use stun::attributes::ATTR_NONCE;
    use stun::attributes::ATTR_REALM;
    use stun::attributes::ATTR_RESERVATION_TOKEN;
    use stun::attributes::ATTR_USERNAME;
    use stun::error_code::ErrorCodeAttribute;
    use stun::error_code::CODE_ADDR_FAMILY_NOT_SUPPORTED;
    use stun::error_code::CODE_INSUFFICIENT_CAPACITY;
    use stun::fingerprint::FINGERPRINT;
    use stun::integrity::MessageIntegrity;
    use stun::message::Getter;
    use stun::message::Message;
    use stun::message::MessageType;
    use stun::message::Setter;
    use stun::message::CLASS_ERROR_RESPONSE;
    use stun::message::CLASS_INDICATION;
    use stun::message::CLASS_REQUEST;
    use stun::message::CLASS_SUCCESS_RESPONSE;
    use stun::message::METHOD_ALLOCATE;
    use stun::message::METHOD_CHANNEL_BIND;
    use stun::message::METHOD_CREATE_PERMISSION;
    use stun::message::METHOD_DATA;
    use stun::message::METHOD_REFRESH;
    use stun::message::METHOD_SEND;
    use stun::textattrs::Nonce;
    use stun::textattrs::Realm;
    use stun::textattrs::Username;
    use tokio::net::UdpSocket;
    use tokio::sync::mpsc;
    use turn::auth::generate_auth_key;
    use turn::client::Client;
    use turn::client::ClientConfig;
    use turn::proto::channum::ChannelNumber;
    use turn::proto::data::Data;
    use turn::proto::lifetime::Lifetime;
    use turn::proto::peeraddr::PeerAddress;
    use turn::proto::relayaddr::RelayedAddress;
    use turn::proto::reqfamily::REQUESTED_FAMILY_IPV6;
    use turn::proto::reqtrans::RequestedTransport;
    use turn::proto::rsrvtoken::ReservationToken;
    use turn::proto::PROTO_UDP;
    use webrtc_util::Conn;

    use super::*;

    struct TestEvenPort {
        reserve_port: bool,
    }

    impl Setter for TestEvenPort {
        fn add_to(&self, m: &mut Message) -> std::result::Result<(), stun::Error> {
            m.add(ATTR_EVEN_PORT, &[if self.reserve_port {
                0x80
            } else {
                0x00
            }]);
            Ok(())
        }
    }

    #[tokio::test]
    async fn turn_server_relays_udp_traffic_authenticated_by_delegatee_key() -> Result<()> {
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let mut config = TurnServerConfig::loopback_ephemeral();
        config.clock = clock.clone();

        let server = TurnServer::start(config).await?;
        let server_addr = server.local_addr();
        let server_pubkey = server.server_pubkey();

        // Initialize a node's DelegateeKey at the clock's timestamp and derive TURN credentials.
        let account_key = SecretKey::random();
        let delegatee_key =
            DelegateeKey::new_with_seckey_at(&account_key, 60_000, clock.now_ms()).unwrap();
        let creds = delegatee_key.turn_credentials(server_pubkey)?;

        let client_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let client = Client::new(ClientConfig {
            stun_serv_addr: server_addr.to_string(),
            turn_serv_addr: server_addr.to_string(),
            username: creds.username.clone(),
            password: creds.credential.clone(),
            realm: String::new(),
            software: String::new(),
            rto_in_ms: 0,
            conn: client_socket,
            vnet: None,
        })
        .await?;
        client.listen().await?;

        let relay_conn = client.allocate().await?;
        let relay_addr = relay_conn.local_addr()?;

        // Spin up a UDP echo peer to verify end-to-end TURN relay forwarding.
        let echo_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let echo_addr = echo_socket.local_addr()?;
        let (done_tx, mut done_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            loop {
                tokio::select! {
                    _ = done_rx.recv() => break,
                    recv = echo_socket.recv_from(&mut buf) => {
                        let Ok((n, from)) = recv else { break };
                        assert_eq!(from, relay_addr);
                        let _ = echo_socket.send_to(&buf[..n], from).await;
                    }
                }
            }
        });

        let payload = b"hello rings turn signature auth";
        relay_conn.send_to(payload, echo_addr).await?;

        let mut recv_buf = vec![0u8; 1500];
        let (n, from) =
            tokio::time::timeout(Duration::from_secs(5), relay_conn.recv_from(&mut recv_buf))
                .await
                .expect("timed out waiting for TURN relay echo")?;

        assert_eq!(from, echo_addr);
        assert_eq!(&recv_buf[..n], payload);

        drop(done_tx);
        client.close().await?;
        server.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_relays_udp_traffic_authenticated_by_direct_did_signature() -> Result<()> {
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let mut config = TurnServerConfig::loopback_ephemeral();
        config.clock = clock.clone();

        let server = TurnServer::start(config).await?;
        let server_addr = server.local_addr();
        let server_pubkey = server.server_pubkey();

        // Sign TURN credentials directly with the node's DID SecretKey (no Delegation TTL).
        let did_key = SecretKey::random();
        let creds = did_key.turn_credentials(server_pubkey)?;

        // Advance the clock far into the future to prove direct DID credentials never expire.
        clock.set(1_700_000_000_000 + 365 * 86_400_000);

        let client_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let client = Client::new(ClientConfig {
            stun_serv_addr: server_addr.to_string(),
            turn_serv_addr: server_addr.to_string(),
            username: creds.username.clone(),
            password: creds.credential.clone(),
            realm: String::new(),
            software: String::new(),
            rto_in_ms: 0,
            conn: client_socket,
            vnet: None,
        })
        .await?;
        client.listen().await?;

        let relay_conn = client.allocate().await?;
        let relay_addr = relay_conn.local_addr()?;

        let echo_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let echo_addr = echo_socket.local_addr()?;
        let (done_tx, mut done_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            loop {
                tokio::select! {
                    _ = done_rx.recv() => break,
                    recv = echo_socket.recv_from(&mut buf) => {
                        let Ok((n, from)) = recv else { break };
                        assert_eq!(from, relay_addr);
                        let _ = echo_socket.send_to(&buf[..n], from).await;
                    }
                }
            }
        });

        let payload = b"hello rings turn direct did auth";
        relay_conn.send_to(payload, echo_addr).await?;

        let mut recv_buf = vec![0u8; 1500];
        let (n, from) =
            tokio::time::timeout(Duration::from_secs(5), relay_conn.recv_from(&mut recv_buf))
                .await
                .expect("timed out waiting for TURN relay echo")?;

        assert_eq!(from, echo_addr);
        assert_eq!(&recv_buf[..n], payload);

        drop(done_tx);
        client.close().await?;
        server.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_rejects_expired_delegation_and_unlisted_did() -> Result<()> {
        let ts_ms = 1_700_000_000_000_u128;
        let ttl_ms = 30_000_u64;
        let clock = Arc::new(ManualClock::new(ts_ms));

        let allowed_account = SecretKey::random();
        let allowed_did = Did::from(allowed_account.address());
        let allowed_delegatee =
            DelegateeKey::new_with_seckey_at(&allowed_account, ttl_ms, ts_ms).unwrap();

        let unlisted_account = SecretKey::random();
        let unlisted_delegatee =
            DelegateeKey::new_with_seckey_at(&unlisted_account, ttl_ms, ts_ms).unwrap();

        let handler = DelegationAuthHandler::with_clock(
            TurnAdmissionPolicy::allowlist([allowed_did]),
            clock.clone(),
        );
        let server_pubkey = handler.server_pubkey();
        let src_addr = SocketAddr::from_str("127.0.0.1:50000").unwrap();

        // 1. Allowed delegator succeeds while live (and populates the verified digest cache).
        let allowed_creds = allowed_delegatee.turn_credentials(server_pubkey)?;
        assert!(turn::auth::AuthHandler::auth_handle(
            &handler,
            &allowed_creds.username,
            DEFAULT_TURN_REALM,
            src_addr,
        )
        .is_ok());

        // 2. Unlisted delegator or unlisted direct DID is rejected by admission policy.
        let unlisted_creds = unlisted_delegatee.turn_credentials(server_pubkey)?;
        assert!(turn::auth::AuthHandler::auth_handle(
            &handler,
            &unlisted_creds.username,
            DEFAULT_TURN_REALM,
            src_addr,
        )
        .is_err());
        let unlisted_direct_creds = unlisted_account.turn_credentials(server_pubkey)?;
        assert!(turn::auth::AuthHandler::auth_handle(
            &handler,
            &unlisted_direct_creds.username,
            DEFAULT_TURN_REALM,
            src_addr,
        )
        .is_err());

        // 3. Advancing the clock past `ts_ms + ttl_ms` rejects a cached Delegation on new
        //    allocations, while direct DID credentials for the allowed DID continue to succeed.
        clock.set(ts_ms + u128::from(ttl_ms) + 1);
        assert!(turn::auth::AuthHandler::auth_handle(
            &handler,
            &allowed_creds.username,
            DEFAULT_TURN_REALM,
            src_addr,
        )
        .is_err());

        let allowed_direct_creds = allowed_account.turn_credentials(server_pubkey)?;
        assert!(turn::auth::AuthHandler::auth_handle(
            &handler,
            &allowed_direct_creds.username,
            DEFAULT_TURN_REALM,
            src_addr,
        )
        .is_ok());

        Ok(())
    }

    #[tokio::test]
    async fn turn_server_keeps_established_allocation_alive_across_delegation_expiry() -> Result<()>
    {
        let ts_ms = 1_700_000_000_000_u128;
        let ttl_ms = 30_000_u64;
        let clock = Arc::new(ManualClock::new(ts_ms));
        let mut config = TurnServerConfig::loopback_ephemeral();
        config.clock = clock.clone();

        let server = TurnServer::start(config).await?;
        let server_addr = server.local_addr();
        let server_pubkey = server.server_pubkey();

        let account_key = SecretKey::random();
        let delegatee_key = DelegateeKey::new_with_seckey_at(&account_key, ttl_ms, ts_ms).unwrap();
        let creds = delegatee_key.turn_credentials(server_pubkey)?;

        let client_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let client = Client::new(ClientConfig {
            stun_serv_addr: server_addr.to_string(),
            turn_serv_addr: server_addr.to_string(),
            username: creds.username.clone(),
            password: creds.credential.clone(),
            realm: String::new(),
            software: String::new(),
            rto_in_ms: 0,
            conn: client_socket,
            vnet: None,
        })
        .await?;
        client.listen().await?;

        // Establish allocation while delegation is live.
        let relay_conn = client.allocate().await?;
        let relay_addr = relay_conn.local_addr()?;

        // Advance clock past initial delegation expiry.
        clock.set(ts_ms + u128::from(ttl_ms) + 10_000);

        // Creating a brand-new allocation with the expired delegation fails.
        let second_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let second_client = Client::new(ClientConfig {
            stun_serv_addr: server_addr.to_string(),
            turn_serv_addr: server_addr.to_string(),
            username: creds.username.clone(),
            password: creds.credential.clone(),
            realm: String::new(),
            software: String::new(),
            rto_in_ms: 100,
            conn: second_socket,
            vnet: None,
        })
        .await?;
        second_client.listen().await?;
        assert!(second_client.allocate().await.is_err());
        second_client.close().await?;

        // Existing established allocation can still CreatePermission and relay to a new peer.
        let echo_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let echo_addr = echo_socket.local_addr()?;
        let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            loop {
                tokio::select! {
                    _ = done_rx.recv() => break,
                    recv = echo_socket.recv_from(&mut buf) => {
                        let Ok((n, from)) = recv else { break };
                        assert_eq!(from, relay_addr);
                        let _ = echo_socket.send_to(&buf[..n], from).await;
                    }
                }
            }
        });

        let payload = b"established allocation survives delegation rotation";
        relay_conn.send_to(payload, echo_addr).await?;
        let mut recv_buf = vec![0u8; 1500];
        let (n, from) =
            tokio::time::timeout(Duration::from_secs(5), relay_conn.recv_from(&mut recv_buf))
                .await
                .expect("timed out waiting for echo on established allocation")?;
        assert_eq!(from, echo_addr);
        assert_eq!(&recv_buf[..n], payload);

        drop(done_tx);
        client.close().await?;
        server.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_public_only_peer_policy_blocks_loopback_and_private_destinations(
    ) -> Result<()> {
        let mut config = TurnServerConfig::loopback_ephemeral();
        config.peer_policy = RelayPeerPolicy::PublicOnly;

        let server = TurnServer::start(config).await?;
        let server_addr = server.local_addr();
        let server_pubkey = server.server_pubkey();

        let did_key = SecretKey::random();
        let creds = did_key.turn_credentials(server_pubkey)?;

        let client_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let client = Client::new(ClientConfig {
            stun_serv_addr: server_addr.to_string(),
            turn_serv_addr: server_addr.to_string(),
            username: creds.username,
            password: creds.credential,
            realm: String::new(),
            software: String::new(),
            rto_in_ms: 100,
            conn: client_socket,
            vnet: None,
        })
        .await?;
        client.listen().await?;

        let relay_conn = client.allocate().await?;
        let loopback_target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9999);
        assert!(
            relay_conn
                .send_to(b"ssrf probe", loopback_target)
                .await
                .is_err(),
            "PublicOnly peer policy must reject CreatePermission / relay to 127.0.0.1"
        );

        client.close().await?;
        server.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_bounds_unauthenticated_nonce_challenges() -> Result<()> {
        let mut config = TurnServerConfig::loopback_ephemeral();
        config.max_nonces = 8;

        let server = TurnServer::start(config).await?;
        let server_addr = server.local_addr();

        let attacker_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let mut recv_buf = vec![0u8; 1500];

        for _ in 0..32 {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
            ];
            msg.build(&setters).unwrap();
            attacker_socket.send_to(&msg.raw, server_addr).await?;
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                attacker_socket.recv_from(&mut recv_buf),
            )
            .await
            .expect("expected 401 challenge response")?;
        }

        assert!(
            server.active_nonce_count().await <= 8,
            "nonce table exceeded configured max_nonces=8"
        );

        server.close().await?;
        Ok(())
    }

    #[test]
    fn relay_address_config_rejects_singleton_port_range() {
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let singleton = RelayAddressConfig::Range {
            relay_ip: localhost,
            bind_ip: localhost,
            min_port: 49153,
            max_port: 49153,
            max_retries: 8,
        };
        assert!(matches!(
            singleton.validate(),
            Err(TurnError::InvalidPortRange {
                min_port: 49153,
                max_port: 49153,
            })
        ));
    }

    #[test]
    fn relay_peer_policy_rejects_local_use_nat64_and_allows_public_ipv6() {
        let local_use_nat64: IpAddr = "64:ff9b:1:1::a00:1".parse().unwrap();
        let well_known_private_nat64: IpAddr = "64:ff9b::a00:1".parse().unwrap();
        let well_known_public_nat64: IpAddr = "64:ff9b::808:808".parse().unwrap();
        let public_v6: IpAddr = "2606:4700:4700::1111".parse().unwrap();

        assert!(!RelayPeerPolicy::PublicOnly.is_allowed_ip(local_use_nat64));
        assert!(!RelayPeerPolicy::PublicOnly.is_allowed_ip(well_known_private_nat64));
        assert!(RelayPeerPolicy::PublicOnly.is_allowed_ip(well_known_public_nat64));
        assert!(RelayPeerPolicy::PublicOnly.is_allowed_ip(public_v6));
    }

    #[tokio::test]
    async fn turn_server_binds_nonces_to_client_and_replays_idempotent_allocate_retransmission(
    ) -> Result<()> {
        let server = TurnServer::start(TurnServerConfig::loopback_ephemeral()).await?;
        let server_addr = server.local_addr();
        let server_pubkey = server.server_pubkey();

        let client_key = SecretKey::random();
        let creds = client_key.turn_credentials(server_pubkey)?;

        let client_a = UdpSocket::bind("127.0.0.1:0").await?;
        let client_b = UdpSocket::bind("127.0.0.1:0").await?;
        let mut recv_buf = vec![0u8; 1500];

        // 1. Send unauthenticated Allocate from client_a to obtain a nonce bound to client_a.
        let challenge_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_a.send_to(&challenge_raw, server_addr).await?;
        let (n, _) = client_a.recv_from(&mut recv_buf).await?;
        let mut challenge_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        challenge_resp.decode().unwrap();
        let mut nonce_attr = Nonce::new(ATTR_NONCE, String::new());
        nonce_attr.get_from(&challenge_resp).unwrap();
        let mut realm_attr = Realm::new(ATTR_REALM, String::new());
        realm_attr.get_from(&challenge_resp).unwrap();

        // 2. Build an authenticated Allocate request using client_a's nonce.
        let tx_id = TransactionId::new();
        let mi_key = generate_auth_key(&creds.username, &realm_attr.text, &creds.credential);
        let auth_allocate_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: tx_id,
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };

        // 3. Replaying the captured packet from client_b must fail with 438 Stale Nonce and not
        //    create an allocation.
        client_b.send_to(&auth_allocate_raw, server_addr).await?;
        let (n, _) = client_b.recv_from(&mut recv_buf).await?;
        let mut replay_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        replay_resp.decode().unwrap();
        assert_eq!(replay_resp.typ.class, CLASS_ERROR_RESPONSE);
        assert!(server.allocations_info(None).await?.is_empty());

        // 4. Sending the packet from client_a succeeds (200 Allocate Success).
        client_a.send_to(&auth_allocate_raw, server_addr).await?;
        let (n1, _) = client_a.recv_from(&mut recv_buf).await?;
        let first_resp_bytes = recv_buf[..n1].to_vec();
        let mut alloc_resp = Message {
            raw: first_resp_bytes.clone(),
            ..Default::default()
        };
        alloc_resp.decode().unwrap();
        assert_eq!(alloc_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        // 5. Retransmitting the exact same Allocate request from client_a replays the cached
        //    200 Allocate Success response instead of failing with 437 Allocation Mismatch.
        client_a.send_to(&auth_allocate_raw, server_addr).await?;
        let (n2, _) = client_a.recv_from(&mut recv_buf).await?;
        assert_eq!(&recv_buf[..n2], first_resp_bytes.as_slice());

        server.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_enforces_allocation_ownership_and_accepts_port_zero_create_permission(
    ) -> Result<()> {
        let mut config = TurnServerConfig::loopback_ephemeral();
        config.peer_policy = RelayPeerPolicy::PublicOnly;
        let server = TurnServer::start(config).await?;
        let server_addr = server.local_addr();
        let server_pubkey = server.server_pubkey();

        let owner_key = SecretKey::random();
        let owner_creds = owner_key.turn_credentials(server_pubkey)?;
        let attacker_key = SecretKey::random();
        let attacker_creds = attacker_key.turn_credentials(server_pubkey)?;

        let client_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let mut recv_buf = vec![0u8; 1500];

        // Obtain nonce and create allocation as `owner_creds`.
        let challenge_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket.send_to(&challenge_raw, server_addr).await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut challenge_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        challenge_resp.decode().unwrap();
        let mut nonce_attr = Nonce::new(ATTR_NONCE, String::new());
        nonce_attr.get_from(&challenge_resp).unwrap();
        let mut realm_attr = Realm::new(ATTR_REALM, String::new());
        realm_attr.get_from(&challenge_resp).unwrap();

        let owner_mi_key = generate_auth_key(
            &owner_creds.username,
            &realm_attr.text,
            &owner_creds.credential,
        );
        let allocate_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
                Box::new(Username::new(ATTR_USERNAME, owner_creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(owner_mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket.send_to(&allocate_raw, server_addr).await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut alloc_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        alloc_resp.decode().unwrap();
        assert_eq!(alloc_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        // Attacker spoofing the same 5-tuple with `attacker_creds` cannot delete the allocation
        // via Refresh(0).
        let attacker_mi_key = generate_auth_key(
            &attacker_creds.username,
            &realm_attr.text,
            &attacker_creds.credential,
        );
        let spoofed_refresh_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)),
                Box::new(Lifetime(Duration::ZERO)),
                Box::new(Username::new(
                    ATTR_USERNAME,
                    attacker_creds.username.clone(),
                )),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(attacker_mi_key)),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&spoofed_refresh_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut refresh_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        refresh_resp.decode().unwrap();
        assert_eq!(refresh_resp.typ.class, CLASS_ERROR_RESPONSE);
        assert_eq!(server.allocations_info(None).await?.len(), 1);

        // Owner sending CreatePermission with port 0 for a public IPv4 peer succeeds.
        let create_perm_public_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                Box::new(PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                    port: 0,
                }),
                Box::new(Username::new(ATTR_USERNAME, owner_creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(owner_mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&create_perm_public_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut perm_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        perm_resp.decode().unwrap();
        assert_eq!(perm_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        // Owner sending CreatePermission for a forbidden private IP receives a 403 error response
        // protected by a valid MESSAGE-INTEGRITY attribute.
        let create_perm_private_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                Box::new(PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                    port: 0,
                }),
                Box::new(Username::new(ATTR_USERNAME, owner_creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(owner_mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&create_perm_private_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut forbidden_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        forbidden_resp.decode().unwrap();
        assert_eq!(forbidden_resp.typ.class, CLASS_ERROR_RESPONSE);
        assert!(MessageIntegrity(owner_mi_key)
            .check(&mut forbidden_resp)
            .is_ok());

        server.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_close_releases_sockets_and_advertises_public_url() -> Result<()> {
        let mut config = TurnServerConfig::loopback_ephemeral();
        config.listen_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        config.relay = RelayAddressConfig::Static {
            relay_ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)),
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        };

        let server = TurnServer::start(config).await?;
        let bound_port = server.local_addr().port();
        assert_eq!(
            server.advertised_addr(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), bound_port)
        );
        let base_url = server.ice_server_base_url()?;
        let pubkey_str = server.server_pubkey().to_base58_string().unwrap();
        assert_eq!(
            base_url,
            format!("turn://203.0.113.10:{bound_port}?pubkey={pubkey_str}")
        );
        assert_eq!(
            extract_turn_server_pubkey(&base_url),
            Some(server.server_pubkey())
        );

        let local_target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bound_port);
        let client_key = SecretKey::random();
        let creds = client_key.turn_credentials(server.server_pubkey())?;
        let client_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let client = Client::new(ClientConfig {
            stun_serv_addr: local_target.to_string(),
            turn_serv_addr: local_target.to_string(),
            username: creds.username.clone(),
            password: creds.credential.clone(),
            realm: String::new(),
            software: String::new(),
            rto_in_ms: 0,
            conn: client_socket,
            vnet: None,
        })
        .await?;
        client.listen().await?;
        let first_relay = client.allocate().await?;
        let first_relay_port = first_relay.local_addr()?.port();

        // Deleting one allocation must not close the shared listener; a second client can still
        // allocate on the same server.
        server
            .delete_allocations_by_username(creds.username.clone())
            .await?;
        let rebound_first = UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            first_relay_port,
        ))
        .await?;
        drop(rebound_first);

        let second_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let second_client = Client::new(ClientConfig {
            stun_serv_addr: local_target.to_string(),
            turn_serv_addr: local_target.to_string(),
            username: creds.username,
            password: creds.credential,
            realm: String::new(),
            software: String::new(),
            rto_in_ms: 0,
            conn: second_socket,
            vnet: None,
        })
        .await?;
        second_client.listen().await?;
        let relay_conn = second_client.allocate().await?;
        let relay_port = relay_conn.local_addr()?.port();

        // Closing the server while `server` remains in scope must release both the listener
        // socket and the allocated relay socket.
        server.close().await?;
        assert!(server.allocations_info(None).await?.is_empty());

        let rebound_relay =
            UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), relay_port)).await?;
        drop(rebound_relay);
        let rebound_listener = UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            bound_port,
        ))
        .await?;
        drop(rebound_listener);

        let _ = client.close().await;
        let _ = second_client.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_handles_oversized_datagrams_batched_permissions_and_even_port_reservations(
    ) -> Result<()> {
        let config = TurnServerConfig::loopback_ephemeral();
        let server = TurnServer::start(config).await?;
        let server_addr = server.local_addr();
        let server_pubkey = server.server_pubkey();

        // 1. Sending an oversized UDP datagram (> 1500 bytes) must not stop the listener.
        let raw_sender = UdpSocket::bind("127.0.0.1:0").await?;
        let oversized_payload = vec![0xA5u8; 4096];
        raw_sender.send_to(&oversized_payload, server_addr).await?;

        let client_key = SecretKey::random();
        let creds = client_key.turn_credentials(server_pubkey)?;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let mut recv_buf = vec![0u8; 2048];

        // Obtain challenge nonce from the still-running listener.
        let challenge_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket.send_to(&challenge_raw, server_addr).await?;
        let (n, _) = tokio::time::timeout(
            Duration::from_secs(5),
            client_socket.recv_from(&mut recv_buf),
        )
        .await
        .expect("TURN listener should remain responsive after oversized UDP datagram")?;
        let mut challenge_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        challenge_resp.decode().unwrap();
        let mut nonce_attr = Nonce::new(ATTR_NONCE, String::new());
        nonce_attr.get_from(&challenge_resp).unwrap();
        let mut realm_attr = Realm::new(ATTR_REALM, String::new());
        realm_attr.get_from(&challenge_resp).unwrap();

        let mi_key = generate_auth_key(&creds.username, &realm_attr.text, &creds.credential);

        // 2. Allocate with EVEN-PORT (reserve_port = true) must be rejected with 508 Insufficient Capacity.
        let reserve_even_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
                Box::new(TestEvenPort { reserve_port: true }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&reserve_even_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut reserve_even_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        reserve_even_resp.decode().unwrap();
        assert_eq!(reserve_even_resp.typ.class, CLASS_ERROR_RESPONSE);
        let mut err_code = ErrorCodeAttribute::default();
        err_code.get_from(&reserve_even_resp).unwrap();
        assert!(err_code.code == CODE_INSUFFICIENT_CAPACITY);
        assert!(MessageIntegrity(mi_key.clone())
            .check(&mut reserve_even_resp)
            .is_ok());

        // 3. Allocate with RESERVATION-TOKEN must also be rejected with 508 Insufficient Capacity.
        let token_alloc_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
                Box::new(ReservationToken(b"12345678".to_vec())),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket.send_to(&token_alloc_raw, server_addr).await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut token_alloc_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        token_alloc_resp.decode().unwrap();
        assert_eq!(token_alloc_resp.typ.class, CLASS_ERROR_RESPONSE);
        let mut err_code = ErrorCodeAttribute::default();
        err_code.get_from(&token_alloc_resp).unwrap();
        assert!(err_code.code == CODE_INSUFFICIENT_CAPACITY);
        assert!(MessageIntegrity(mi_key.clone())
            .check(&mut token_alloc_resp)
            .is_ok());

        // 4. Allocate with EVEN-PORT (reserve_port = false) succeeds with an even port and no
        //    RESERVATION-TOKEN attribute.
        let even_alloc_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
                Box::new(TestEvenPort {
                    reserve_port: false,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket.send_to(&even_alloc_raw, server_addr).await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut even_alloc_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        even_alloc_resp.decode().unwrap();
        assert_eq!(even_alloc_resp.typ.class, CLASS_SUCCESS_RESPONSE);
        assert!(!even_alloc_resp.contains(ATTR_RESERVATION_TOKEN));
        let mut relayed_attr = RelayedAddress::default();
        relayed_attr.get_from(&even_alloc_resp).unwrap();
        assert_eq!(relayed_attr.port % 2, 0);

        // 5. An on-path attacker appending a trailing XOR-PEER-ADDRESS (127.0.0.1) after
        //    MESSAGE-INTEGRITY on a CreatePermission for 8.8.8.8 must NOT authorize 127.0.0.1.
        let peer_two_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_two_addr = peer_two_socket.local_addr()?;

        let tampered_perm_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                Box::new(PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                    port: 53,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                // Appended after MESSAGE-INTEGRITY without recomputing HMAC:
                Box::new(PeerAddress {
                    ip: peer_two_addr.ip(),
                    port: peer_two_addr.port(),
                }),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&tampered_perm_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut tampered_perm_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        tampered_perm_resp.decode().unwrap();
        assert_eq!(tampered_perm_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        // Sending to `peer_two_addr` must still be dropped because the trailing attribute after
        // MESSAGE-INTEGRITY was ignored.
        let probe_indication_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_SEND, CLASS_INDICATION)),
                Box::new(PeerAddress {
                    ip: peer_two_addr.ip(),
                    port: peer_two_addr.port(),
                }),
                Box::new(Data(b"should be dropped".to_vec())),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&probe_indication_raw, server_addr)
            .await?;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(150),
                peer_two_socket.recv_from(&mut recv_buf),
            )
            .await
            .is_err(),
            "trailing XOR-PEER-ADDRESS after MESSAGE-INTEGRITY must not install permission"
        );

        // 6. Batched CreatePermission with two distinct peer IPs (8.8.8.8 first, 127.0.0.1 second)
        //    inside the authenticated prefix must install permissions for both peers, and >1500-byte
        //    UDP payloads must round-trip without truncation in both directions.
        let batched_perm_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                Box::new(PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                    port: 53,
                }),
                Box::new(PeerAddress {
                    ip: peer_two_addr.ip(),
                    port: peer_two_addr.port(),
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&batched_perm_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut batched_perm_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        batched_perm_resp.decode().unwrap();
        assert_eq!(batched_perm_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        let large_payload: Vec<u8> = (0..3000u16).map(|i| (i % 251) as u8).collect();
        let send_indication_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_SEND, CLASS_INDICATION)),
                Box::new(PeerAddress {
                    ip: peer_two_addr.ip(),
                    port: peer_two_addr.port(),
                }),
                Box::new(Data(large_payload.clone())),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&send_indication_raw, server_addr)
            .await?;
        let mut large_recv_buf = vec![0u8; 8192];
        let (n, from_relay) = tokio::time::timeout(
            Duration::from_secs(5),
            peer_two_socket.recv_from(&mut large_recv_buf),
        )
        .await
        .expect("second peer in batched CreatePermission should receive relayed packet")?;
        assert_eq!(from_relay.port(), relayed_attr.port);
        assert_eq!(&large_recv_buf[..n], large_payload.as_slice());

        // Echo the 3000-byte payload from the peer back to the relay port and verify the client
        // receives the full >1500-byte payload in DataIndication without truncation.
        peer_two_socket.send_to(&large_payload, from_relay).await?;
        let (n_back, _) = tokio::time::timeout(
            Duration::from_secs(5),
            client_socket.recv_from(&mut large_recv_buf),
        )
        .await
        .expect("client should receive DataIndication for >1500-byte peer datagram")?;
        let mut data_ind = Message {
            raw: large_recv_buf[..n_back].to_vec(),
            ..Default::default()
        };
        data_ind.decode().unwrap();
        assert_eq!(data_ind.typ.class, CLASS_INDICATION);
        assert_eq!(data_ind.typ.method, METHOD_DATA);
        let mut echoed_data = Data::default();
        echoed_data.get_from(&data_ind).unwrap();
        assert_eq!(echoed_data.0, large_payload);

        server.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_bounds_permissions_preserves_reallocated_five_tuple_and_honors_default_ipv4_family(
    ) -> Result<()> {
        let mut config = TurnServerConfig::loopback_ephemeral();
        config.max_permissions_per_allocation = 2;
        let server_pubkey = config.server_key.pubkey();
        let server = TurnServer::start(config).await?;
        let server_addr = server.local_addr();

        let client_key = SecretKey::random();
        let creds = client_key.turn_credentials(server_pubkey)?;

        let client_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let mut recv_buf = vec![0u8; 4096];

        // 1. Challenge and initial Allocate.
        let unauth_alloc_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&unauth_alloc_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut challenge = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        challenge.decode().unwrap();
        let mut nonce_attr = Nonce::new(ATTR_NONCE, String::new());
        nonce_attr.get_from(&challenge).unwrap();
        let mut realm_attr = Realm::new(ATTR_REALM, String::new());
        realm_attr.get_from(&challenge).unwrap();
        let mi_key = generate_auth_key(&creds.username, &realm_attr.text, &creds.credential);

        let alloc_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket.send_to(&alloc_raw, server_addr).await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut alloc_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        alloc_resp.decode().unwrap();
        assert_eq!(alloc_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        // 2. Delete the allocation via Refresh(0) and immediately reallocate on the exact same
        // client socket (same 5-tuple). The previous allocation's background reader must NOT
        // asynchronously delete the replacement allocation.
        let refresh_zero_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)),
                Box::new(Lifetime(Duration::ZERO)),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&refresh_zero_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut refresh_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        refresh_resp.decode().unwrap();
        assert_eq!(refresh_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        let realloc_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket.send_to(&realloc_raw, server_addr).await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut realloc_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        realloc_resp.decode().unwrap();
        assert_eq!(realloc_resp.typ.class, CLASS_SUCCESS_RESPONSE);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            server.allocations_info(None).await?.len(),
            1,
            "replacement allocation on the same 5-tuple must remain active"
        );

        // 3. Enforce max_permissions_per_allocation = 2 across CreatePermission and ChannelBind.
        let perm_two_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                Box::new(PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)),
                    port: 4001,
                }),
                Box::new(PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 2)),
                    port: 4002,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket.send_to(&perm_two_raw, server_addr).await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut perm_two_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        perm_two_resp.decode().unwrap();
        assert_eq!(perm_two_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        // Refreshing an existing permitted IP via ChannelBind must succeed even at the quota cap.
        let chbind_existing_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CHANNEL_BIND, CLASS_REQUEST)),
                Box::new(ChannelNumber(0x4000)),
                Box::new(PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)),
                    port: 4001,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&chbind_existing_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut chbind_existing_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        chbind_existing_resp.decode().unwrap();
        assert_eq!(chbind_existing_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        // Adding a 3rd distinct peer IP via CreatePermission must be rejected with 508.
        let perm_third_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                Box::new(PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 3)),
                    port: 4003,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket.send_to(&perm_third_raw, server_addr).await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut perm_third_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        perm_third_resp.decode().unwrap();
        assert_eq!(perm_third_resp.typ.class, CLASS_ERROR_RESPONSE);
        let mut err_code = ErrorCodeAttribute::default();
        err_code.get_from(&perm_third_resp).unwrap();
        assert!(err_code.code == CODE_INSUFFICIENT_CAPACITY);

        // Adding a 3rd distinct peer IP via ChannelBind must also be rejected with 508.
        let chbind_third_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CHANNEL_BIND, CLASS_REQUEST)),
                Box::new(ChannelNumber(0x4001)),
                Box::new(PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4)),
                    port: 4004,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket
            .send_to(&chbind_third_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut chbind_third_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        chbind_third_resp.decode().unwrap();
        assert_eq!(chbind_third_resp.typ.class, CLASS_ERROR_RESPONSE);
        let mut err_code = ErrorCodeAttribute::default();
        err_code.get_from(&chbind_third_resp).unwrap();
        assert!(err_code.code == CODE_INSUFFICIENT_CAPACITY);

        server.close().await?;

        // 4. Verify RFC 6156 §4.1 default IPv4 requested family on an IPv6-configured relay:
        // omitting REQUESTED-ADDRESS-FAMILY must return 440, while requesting IPv6 succeeds.
        let ipv6_localhost = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let mut v6_config = TurnServerConfig::loopback_ephemeral();
        v6_config.relay = RelayAddressConfig::Static {
            relay_ip: ipv6_localhost,
            bind_ip: ipv6_localhost,
        };
        let v6_server_pubkey = v6_config.server_key.pubkey();
        let v6_server = TurnServer::start(v6_config).await?;
        let v6_server_addr = v6_server.local_addr();
        let v6_creds = client_key.turn_credentials(v6_server_pubkey)?;

        let v6_client_socket = UdpSocket::bind("127.0.0.1:0").await?;
        v6_client_socket
            .send_to(&unauth_alloc_raw, v6_server_addr)
            .await?;
        let (n, _) = v6_client_socket.recv_from(&mut recv_buf).await?;
        let mut v6_challenge = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        v6_challenge.decode().unwrap();
        let mut v6_nonce = Nonce::new(ATTR_NONCE, String::new());
        v6_nonce.get_from(&v6_challenge).unwrap();
        let mut v6_realm = Realm::new(ATTR_REALM, String::new());
        v6_realm.get_from(&v6_challenge).unwrap();
        let v6_mi_key = generate_auth_key(&v6_creds.username, &v6_realm.text, &v6_creds.credential);

        let default_v4_alloc_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
                Box::new(Username::new(ATTR_USERNAME, v6_creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, v6_realm.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, v6_nonce.text.clone())),
                Box::new(MessageIntegrity(v6_mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        v6_client_socket
            .send_to(&default_v4_alloc_raw, v6_server_addr)
            .await?;
        let (n, _) = v6_client_socket.recv_from(&mut recv_buf).await?;
        let mut default_v4_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        default_v4_resp.decode().unwrap();
        assert_eq!(default_v4_resp.typ.class, CLASS_ERROR_RESPONSE);
        let mut err_code = ErrorCodeAttribute::default();
        err_code.get_from(&default_v4_resp).unwrap();
        assert!(err_code.code == CODE_ADDR_FAMILY_NOT_SUPPORTED);

        let explicit_v6_alloc_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }),
                Box::new(REQUESTED_FAMILY_IPV6),
                Box::new(Username::new(ATTR_USERNAME, v6_creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, v6_realm.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, v6_nonce.text.clone())),
                Box::new(MessageIntegrity(v6_mi_key)),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        v6_client_socket
            .send_to(&explicit_v6_alloc_raw, v6_server_addr)
            .await?;
        let (n, _) = v6_client_socket.recv_from(&mut recv_buf).await?;
        let mut explicit_v6_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        explicit_v6_resp.decode().unwrap();
        assert_eq!(explicit_v6_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        v6_server.close().await?;
        Ok(())
    }
}
