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

#[cfg(all(feature = "server", not(target_family = "wasm")))]
mod auth;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
mod config;
mod credentials;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
mod error;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
mod relay_backend;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
mod server;

#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use auth::DelegationAuthHandler;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use auth::TurnAdmissionPolicy;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use auth::DEFAULT_VERIFIED_DELEGATION_CACHE_CAPACITY;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::RelayAddressConfig;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::RelayPeerPolicy;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::TurnServerConfig;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::DEFAULT_MAX_ALLOCATIONS;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::DEFAULT_MAX_NONCES;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::DEFAULT_MAX_PERMISSIONS_PER_ALLOCATION;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::DEFAULT_MAX_RESERVATIONS;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::DEFAULT_MAX_RESERVATION_TTL;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::DEFAULT_PORT_RANGE_MAX_RETRIES;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::DEFAULT_TURN_PORT;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use config::DEFAULT_TURN_REALM;
pub use credentials::extract_turn_server_pubkey;
pub use credentials::format_turn_server_url;
pub use credentials::has_turn_pubkey_query_param;
pub use credentials::pack_turn_account_auth;
pub use credentials::pack_turn_auth;
pub use credentials::pack_turn_did_auth;
pub use credentials::AccountSessionClaim;
pub use credentials::DecodedTurnClaim;
pub use credentials::DecodedTurnUsername;
pub use credentials::SignedTurnSessionTicket;
pub use credentials::TurnAuthIdentity;
pub use credentials::TurnCredentialSigner;
pub use credentials::TurnCredentials;
pub use credentials::TurnSessionTicket;
pub use credentials::VerifiedTurnAuth;
pub use credentials::MAX_STUN_USERNAME_BYTES;
pub use credentials::TURN_AUTH_DOMAIN;
pub use credentials::TURN_SESSION_TICKET_DOMAIN;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use error::Result;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use error::TurnError;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::ActiveAllocationSpec;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::BeginAllocateOutcome;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::BeginAllocateStep;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::CompleteAllocateDecision;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::CorePermissionError;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::CoreReserveError;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::PendingAllocateSpec;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::RefreshDecision;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::TurnAllocationCore;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::TurnDatagramRelay;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::TurnDatagramRelayFactory;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::TurnFiveTupleKey;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use relay_backend::TurnRelayCloseHandle;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use server::TurnServer;
#[cfg(all(feature = "server", not(target_family = "wasm")))]
pub use server::TurnSessionReservation;

#[cfg(all(test, feature = "server", not(target_family = "wasm")))]
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
    use stun::error_code::CODE_BAD_REQUEST;
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
        let active_infos = server.allocations_info(None).await?;
        assert_eq!(active_infos.len(), 1);
        let recorded_tuple = active_infos.keys().next().copied().unwrap();
        assert_eq!(
            recorded_tuple.dst_addr, local_target,
            "wildcard listener must record concrete packet destination address in FiveTuple"
        );

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
        config.max_allocations = 1;
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
                Box::new(Lifetime(Duration::from_secs(7200))),
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
        let mut realloc_lt = Lifetime::default();
        realloc_lt.get_from(&realloc_resp).unwrap();
        assert_eq!(realloc_lt.0, Duration::from_secs(3600));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            server.allocations_info(None).await?.len(),
            1,
            "replacement allocation on the same 5-tuple must remain active"
        );

        // Nonzero Refresh below 600s must clamp up to the 600s default lifetime (RFC 5766 §7.2).
        let short_refresh_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)),
                Box::new(Lifetime(Duration::from_secs(60))),
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
            .send_to(&short_refresh_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut short_refresh_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        short_refresh_resp.decode().unwrap();
        assert_eq!(short_refresh_resp.typ.class, CLASS_SUCCESS_RESPONSE);
        let mut short_refresh_lt = Lifetime::default();
        short_refresh_lt.get_from(&short_refresh_resp).unwrap();
        assert_eq!(short_refresh_lt.0, Duration::from_secs(600));

        // A delayed duplicate of the earlier Refresh(0) must replay its 200 OK response
        // without deleting the replacement allocation.
        client_socket
            .send_to(&refresh_zero_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut dup_refresh_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        dup_refresh_resp.decode().unwrap();
        assert_eq!(dup_refresh_resp.typ.class, CLASS_SUCCESS_RESPONSE);
        assert_eq!(
            server.allocations_info(None).await?.len(),
            1,
            "replayed Refresh(0) must not delete the replacement allocation"
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

        // Repeatedly refreshing the same ChannelBind and sending a CreatePermission with
        // many duplicate XOR-PEER-ADDRESS attributes must complete immediately without blocking.
        for _ in 0..8 {
            client_socket
                .send_to(&chbind_existing_raw, server_addr)
                .await?;
            let (n, _) = tokio::time::timeout(
                Duration::from_secs(2),
                client_socket.recv_from(&mut recv_buf),
            )
            .await
            .expect("ChannelBind refresh should not block")?;
            let mut resp = Message {
                raw: recv_buf[..n].to_vec(),
                ..Default::default()
            };
            resp.decode().unwrap();
            assert_eq!(resp.typ.class, CLASS_SUCCESS_RESPONSE);
        }
        let dup_perm_raw = {
            let mut msg = Message::new();
            let mut setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
            ];
            for _ in 0..16 {
                setters.push(Box::new(PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)),
                    port: 4001,
                }));
            }
            setters.push(Box::new(Username::new(
                ATTR_USERNAME,
                creds.username.clone(),
            )));
            setters.push(Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())));
            setters.push(Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())));
            setters.push(Box::new(MessageIntegrity(mi_key.clone())));
            setters.push(Box::new(FINGERPRINT));
            msg.build(&setters).unwrap();
            msg.raw
        };
        client_socket.send_to(&dup_perm_raw, server_addr).await?;
        let (n, _) = tokio::time::timeout(
            Duration::from_secs(2),
            client_socket.recv_from(&mut recv_buf),
        )
        .await
        .expect("batched duplicate CreatePermission should not deadlock")?;
        let mut dup_perm_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        dup_perm_resp.decode().unwrap();
        assert_eq!(dup_perm_resp.typ.class, CLASS_SUCCESS_RESPONSE);

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

        // An out-of-range channel number (0x0001) must be rejected with 400 Bad Request.
        let chbind_invalid_num_raw = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CHANNEL_BIND, CLASS_REQUEST)),
                Box::new(ChannelNumber(0x0001)),
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
            .send_to(&chbind_invalid_num_raw, server_addr)
            .await?;
        let (n, _) = client_socket.recv_from(&mut recv_buf).await?;
        let mut chbind_invalid_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        chbind_invalid_resp.decode().unwrap();
        assert_eq!(chbind_invalid_resp.typ.class, CLASS_ERROR_RESPONSE);
        let mut err_code = ErrorCodeAttribute::default();
        err_code.get_from(&chbind_invalid_resp).unwrap();
        assert!(err_code.code == CODE_BAD_REQUEST);

        // A second concurrent client socket must be rejected with 508 when max_allocations = 1.
        let second_socket = UdpSocket::bind("127.0.0.1:0").await?;
        second_socket
            .send_to(&unauth_alloc_raw, server_addr)
            .await?;
        let (n, _) = second_socket.recv_from(&mut recv_buf).await?;
        let mut second_challenge = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        second_challenge.decode().unwrap();
        let mut second_nonce = Nonce::new(ATTR_NONCE, String::new());
        second_nonce.get_from(&second_challenge).unwrap();
        let second_alloc_raw = {
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
                Box::new(Nonce::new(ATTR_NONCE, second_nonce.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        second_socket
            .send_to(&second_alloc_raw, server_addr)
            .await?;
        let (n, _) = second_socket.recv_from(&mut recv_buf).await?;
        let mut second_alloc_resp = Message {
            raw: recv_buf[..n].to_vec(),
            ..Default::default()
        };
        second_alloc_resp.decode().unwrap();
        assert_eq!(second_alloc_resp.typ.class, CLASS_ERROR_RESPONSE);
        let mut err_code = ErrorCodeAttribute::default();
        err_code.get_from(&second_alloc_resp).unwrap();
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
        assert_eq!(
            v6_server.advertised_addr(),
            v6_server_addr,
            "listener bound to 127.0.0.1 must advertise 127.0.0.1 even when relay is IPv6"
        );
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

    #[tokio::test]
    async fn turn_server_relays_udp_traffic_for_tcp_turn_client() -> Result<()> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;
        use turn::proto::chandata::ChannelData;

        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let mut config = TurnServerConfig::loopback_ephemeral();
        config.clock = clock.clone();

        let server = TurnServer::start(config).await?;
        let server_addr = server.local_addr();
        let server_pubkey = server.server_pubkey();

        let account_key = SecretKey::random();
        let delegatee_key =
            DelegateeKey::new_with_seckey_at(&account_key, 60_000, clock.now_ms()).unwrap();
        let signed_tcp_url = server.tcp_ice_server_url_for(&delegatee_key)?;
        assert!(signed_tcp_url.ends_with("?transport=tcp"));
        let creds = delegatee_key.turn_credentials(server_pubkey)?;

        let mut tcp_client = TcpStream::connect(server_addr).await?;
        tcp_client.set_nodelay(true)?;

        // 1. Send unauthenticated Allocate over TCP to get 401 challenge.
        let unauth_alloc = {
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
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        tcp_client.write_all(&unauth_alloc).await?;
        tcp_client.flush().await?;

        async fn read_stun_over_tcp(stream: &mut tokio::net::TcpStream) -> Message {
            use tokio::io::AsyncReadExt;

            let mut hdr = [0u8; 20];
            stream.read_exact(&mut hdr).await.unwrap();
            let body_len = usize::from(u16::from_be_bytes([hdr[2], hdr[3]]));
            let mut raw = vec![0u8; 20 + body_len];
            raw[..20].copy_from_slice(&hdr);
            stream.read_exact(&mut raw[20..]).await.unwrap();
            let mut msg = Message {
                raw,
                ..Default::default()
            };
            msg.decode().unwrap();
            msg
        }

        let challenge_resp = read_stun_over_tcp(&mut tcp_client).await;
        assert_eq!(challenge_resp.typ.class, CLASS_ERROR_RESPONSE);
        let nonce_attr = Nonce::get_from_as(&challenge_resp, ATTR_NONCE).unwrap();
        let realm_attr = Realm::get_from_as(&challenge_resp, ATTR_REALM).unwrap();
        let mi_key = generate_auth_key(&creds.username, &realm_attr.text, &creds.credential);

        // 2. Send authenticated Allocate over TCP.
        let auth_alloc = {
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
        tcp_client.write_all(&auth_alloc).await?;
        tcp_client.flush().await?;

        let alloc_resp = read_stun_over_tcp(&mut tcp_client).await;
        assert_eq!(alloc_resp.typ.class, CLASS_SUCCESS_RESPONSE);
        let mut relayed = RelayedAddress::default();
        relayed.get_from(&alloc_resp).unwrap();
        let relay_addr = SocketAddr::new(relayed.ip, relayed.port);

        // 3. Spin up a UDP echo peer and bind Channel 0x4000 to it over TCP.
        let echo_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let echo_addr = echo_socket.local_addr()?;

        let channel_bind = {
            let mut msg = Message::new();
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(Message {
                    transaction_id: TransactionId::new(),
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_CHANNEL_BIND, CLASS_REQUEST)),
                Box::new(ChannelNumber(0x4000)),
                Box::new(PeerAddress {
                    ip: echo_addr.ip(),
                    port: echo_addr.port(),
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username)),
                Box::new(Realm::new(ATTR_REALM, realm_attr.text)),
                Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text)),
                Box::new(MessageIntegrity(mi_key)),
                Box::new(FINGERPRINT),
            ];
            msg.build(&setters).unwrap();
            msg.raw
        };
        tcp_client.write_all(&channel_bind).await?;
        tcp_client.flush().await?;
        let bind_resp = read_stun_over_tcp(&mut tcp_client).await;
        assert_eq!(bind_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        // 4. Send unaligned 5-byte ChannelData frame over TCP (padded to 8-byte body = 12 bytes total)
        //    and verify UDP relay delivery and TCP ChannelData reply.
        let mut cd = ChannelData {
            number: ChannelNumber(0x4000),
            data: b"hello".to_vec(),
            raw: Vec::new(),
        };
        cd.encode();
        assert_eq!(cd.raw.len(), 12);
        tcp_client.write_all(&cd.raw).await?;
        tcp_client.flush().await?;

        let mut udp_buf = [0u8; 64];
        let (n, from) =
            tokio::time::timeout(Duration::from_secs(5), echo_socket.recv_from(&mut udp_buf))
                .await
                .expect("UDP peer should receive ChannelData payload")?;
        assert_eq!(from, relay_addr);
        assert_eq!(&udp_buf[..n], b"hello");

        echo_socket.send_to(b"world", from).await?;

        let mut tcp_cd_buf = [0u8; 12];
        tokio::time::timeout(
            Duration::from_secs(5),
            tcp_client.read_exact(&mut tcp_cd_buf),
        )
        .await
        .expect("TCP TURN client should receive padded ChannelData frame")?;
        let mut inbound_cd = ChannelData {
            raw: tcp_cd_buf.to_vec(),
            ..Default::default()
        };
        inbound_cd.decode().unwrap();
        assert_eq!(inbound_cd.number, ChannelNumber(0x4000));
        assert_eq!(inbound_cd.data, b"world");

        assert_eq!(server.allocations_info(None).await?.len(), 1);
        drop(tcp_client);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if server.allocations_info(None).await.unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("TCP disconnect should retire the associated TURN allocation");

        // 5. Verify unauthenticated TCP connections cannot send unlimited dummy frames to hold a
        //    connection slot without authenticating an allocation.
        let mut unauth_spammer = tokio::net::TcpStream::connect(server_addr).await?;
        for _ in 0..8 {
            unauth_spammer.write_all(&[0x40, 0x00, 0x00, 0x00]).await?;
            unauth_spammer.flush().await?;
        }
        let mut eof_buf = [0u8; 4];
        let eof_res =
            tokio::time::timeout(Duration::from_secs(2), unauth_spammer.read(&mut eof_buf))
                .await
                .expect("unauthenticated TCP spammer must be disconnected promptly");
        assert!(
            matches!(eof_res, Ok(0))
                || matches!(eof_res, Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionReset),
            "server must close unauthenticated TCP connection after exceeding unauth frame cap: {eof_res:?}"
        );

        server.close().await?;
        let mut restart_cfg = TurnServerConfig::loopback_ephemeral();
        restart_cfg.listen_addr = server_addr;
        let restarted = TurnServer::start(restart_cfg).await?;
        restarted.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_relays_udp_with_unlinkable_session_ticket_and_rejects_replay() -> Result<()>
    {
        // Configure an empty allowlist (`TurnAdmissionPolicy::allowlist([])`) to prove that
        // server-issued unlinkable `SessionTicket` credentials authenticate without exposing or
        // requiring any client DID.
        let mut cfg = TurnServerConfig::loopback_ephemeral();
        cfg.admission = TurnAdmissionPolicy::allowlist(std::iter::empty());
        let server = TurnServer::start(cfg).await?;

        let eph_key = SecretKey::random();
        let ttl = Duration::from_secs(60);
        let reservation = server.reserve_exit_session(eph_key.pubkey(), ttl).await?;
        let creds = reservation
            .ticket
            .to_turn_credentials(&eph_key, server.server_pubkey())?;

        // Connect an unmodified standard TURN client using the unlinkable ticket credentials.
        let client_conn = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let turn_client = Client::new(ClientConfig {
            stun_serv_addr: server.local_addr().to_string(),
            turn_serv_addr: server.local_addr().to_string(),
            conn: client_conn,
            username: creds.username.clone(),
            password: creds.credential.clone(),
            realm: DEFAULT_TURN_REALM.to_string(),
            software: String::new(),
            rto_in_ms: 0,
            vnet: None,
        })
        .await?;
        turn_client.listen().await?;

        let allocated_conn = turn_client.allocate().await?;
        let allocated_relay_addr = allocated_conn.local_addr()?;
        assert_eq!(
            server.active_reservation_count().await,
            0,
            "claiming a ticket must release its unclaimed reservation slot"
        );

        let peer_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let peer_addr = peer_socket.local_addr()?;

        let outbound_msg = b"single-hop-session-ticket-outbound";
        allocated_conn.send_to(outbound_msg, peer_addr).await?;

        let mut peer_buf = [0u8; 256];
        let (n, observed_relay_src) =
            tokio::time::timeout(Duration::from_secs(5), peer_socket.recv_from(&mut peer_buf))
                .await
                .expect("peer should receive datagram from TURN relay")?;
        assert_eq!(observed_relay_src, allocated_relay_addr);
        assert_eq!(&peer_buf[..n], outbound_msg);

        let inbound_msg = b"single-hop-session-ticket-inbound";
        peer_socket
            .send_to(inbound_msg, allocated_relay_addr)
            .await?;

        let mut client_buf = [0u8; 256];
        let (n_in, observed_peer_src) = tokio::time::timeout(
            Duration::from_secs(5),
            allocated_conn.recv_from(&mut client_buf),
        )
        .await
        .expect("client should receive inbound datagram from TURN relay")?;
        assert_eq!(observed_peer_src, peer_addr);
        assert_eq!(&client_buf[..n_in], inbound_msg);

        // Re-using the single-use `creds` session ticket on a second client must be rejected.
        let replay_conn = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let replay_client = Client::new(ClientConfig {
            stun_serv_addr: server.local_addr().to_string(),
            turn_serv_addr: server.local_addr().to_string(),
            conn: replay_conn,
            username: creds.username,
            password: creds.credential,
            realm: DEFAULT_TURN_REALM.to_string(),
            software: String::new(),
            rto_in_ms: 0,
            vnet: None,
        })
        .await?;
        replay_client.listen().await?;
        assert!(
            replay_client.allocate().await.is_err(),
            "single-use SessionTicket must reject a second Allocate attempt"
        );
        let _ = replay_client.close().await;

        allocated_conn.close().await?;
        turn_client.close().await?;
        server.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_reservations_honor_injected_clock_and_reservation_flood_cap() -> Result<()>
    {
        let t0: u128 = rings_core::utils::get_epoch_ms();
        let clock = Arc::new(ManualClock::new(t0));
        let mut cfg = TurnServerConfig::loopback_ephemeral();
        cfg.clock = clock.clone();
        // Set `max_allocations = 1` and `max_reservations = 2` to verify that unclaimed
        // reservations hold neither OS sockets nor `max_allocations` permits before `Allocate`,
        // and that requested TTLs exceeding `max_reservation_ttl` (`60s` default) are clamped.
        cfg.max_allocations = 1;
        cfg.max_reservations = 2;
        let server = TurnServer::start(cfg).await?;

        let key1 = SecretKey::random();
        let res1 = server
            .reserve_exit_session(key1.pubkey(), Duration::from_secs(3_600))
            .await?;
        assert_eq!(
            res1.ticket.ticket.expires_at_ms,
            t0 + DEFAULT_MAX_RESERVATION_TTL.as_millis(),
            "excessive reservation TTL must be clamped to max_reservation_ttl (60s)"
        );

        let key2 = SecretKey::random();
        let _res2 = server
            .reserve_exit_session(key2.pubkey(), Duration::from_secs(30))
            .await?;
        assert_eq!(server.active_reservation_count().await, 2);
        assert_eq!(
            server.allocations_info(None).await?.len(),
            0,
            "unclaimed reservations must not consume active allocation slots"
        );

        // Unauthenticated reservation flood cannot exceed `max_reservations = 2`.
        for _ in 0..8 {
            let flood_key = SecretKey::random();
            let err = server
                .reserve_exit_session(flood_key.pubkey(), Duration::from_secs(30))
                .await
                .expect_err("reservation flood must fail once max_reservations == 2 is reached");
            assert!(matches!(err, TurnError::ReservationCapacityReached {
                max_reservations: 2
            }));
        }
        assert_eq!(server.active_reservation_count().await, 2);

        // Normal authenticated client allocation still succeeds using the single `max_allocations = 1`
        // slot while 2 unclaimed reservations are active.
        let did_key = SecretKey::random();
        let creds = did_key.turn_credentials(server.server_pubkey())?;
        let client_conn = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let client = Client::new(ClientConfig {
            stun_serv_addr: server.local_addr().to_string(),
            turn_serv_addr: server.local_addr().to_string(),
            conn: client_conn,
            username: creds.username,
            password: creds.credential,
            realm: DEFAULT_TURN_REALM.to_string(),
            software: String::new(),
            rto_in_ms: 0,
            vnet: None,
        })
        .await?;
        client.listen().await?;
        let alloc_conn = client.allocate().await?;

        // Advancing the injected ManualClock past 60s expires both unclaimed reservations.
        clock.set(t0 + 60_001);
        assert_eq!(server.active_reservation_count().await, 0);
        let key3 = SecretKey::random();
        let res3 = server
            .reserve_exit_session(key3.pubkey(), Duration::from_secs(30))
            .await?;
        assert_eq!(res3.ticket.ticket.expires_at_ms, t0 + 90_001);

        alloc_conn.close().await?;
        client.close().await?;
        server.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn turn_server_coalesces_inflight_async_allocate_and_aborts_on_shutdown() -> Result<()> {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        use stun::message::METHOD_BINDING;
        use stun::xoraddr::XorMappedAddress;
        use tokio::sync::Notify;
        use turn::proto::relayaddr::RelayedAddress;

        struct GatedRelay {
            addr: SocketAddr,
            closed: Notify,
            closed_count: Arc<AtomicUsize>,
            permitted_ips: std::sync::Mutex<Vec<IpAddr>>,
        }

        #[async_trait::async_trait]
        impl TurnDatagramRelay for GatedRelay {
            fn relayed_addr(&self) -> SocketAddr {
                self.addr
            }

            async fn recv_datagram(&self, _buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
                self.closed.notified().await;
                Err(std::io::Error::other("closed"))
            }

            async fn send_to(&self, buf: &[u8], _target: SocketAddr) -> std::io::Result<usize> {
                Ok(buf.len())
            }

            async fn allow_peer_ips(
                &self,
                peer_ips: &[IpAddr],
                _ttl: Duration,
            ) -> std::io::Result<()> {
                self.permitted_ips
                    .lock()
                    .unwrap()
                    .extend_from_slice(peer_ips);
                Ok(())
            }

            fn begin_close(&self) -> TurnRelayCloseHandle {
                self.closed.notify_waiters();
                None
            }

            async fn finish_close(&self, _handle: TurnRelayCloseHandle) {
                self.closed_count.fetch_add(1, Ordering::SeqCst);
            }
        }

        struct GatedRelayFactory {
            started: Notify,
            release: Notify,
            calls: AtomicUsize,
            closed_count: Arc<AtomicUsize>,
            last_relay: std::sync::Mutex<Option<Arc<GatedRelay>>>,
        }

        #[async_trait::async_trait]
        impl TurnDatagramRelayFactory for GatedRelayFactory {
            fn supports_family(&self, use_ipv4: bool) -> bool {
                use_ipv4
            }

            async fn allocate_relay(
                &self,
                _use_ipv4: bool,
                _need_even_port: bool,
            ) -> std::io::Result<Arc<dyn TurnDatagramRelay>> {
                let idx = self.calls.fetch_add(1, Ordering::SeqCst);
                self.started.notify_waiters();
                self.release.notified().await;
                let addr = SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(198, 51, 100, 99)),
                    50_000 + u16::try_from(idx).unwrap(),
                );
                let relay = Arc::new(GatedRelay {
                    addr,
                    closed: Notify::new(),
                    closed_count: Arc::clone(&self.closed_count),
                    permitted_ips: std::sync::Mutex::new(Vec::new()),
                });
                *self.last_relay.lock().unwrap() = Some(Arc::clone(&relay));
                Ok(relay)
            }
        }

        let closed_count = Arc::new(AtomicUsize::new(0));
        let factory = Arc::new(GatedRelayFactory {
            started: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
            closed_count: Arc::clone(&closed_count),
            last_relay: std::sync::Mutex::new(None),
        });

        let mut cfg = TurnServerConfig::loopback_ephemeral();
        cfg.max_allocations = 1;
        cfg.custom_relay_factory = Some(Arc::clone(&factory) as Arc<dyn TurnDatagramRelayFactory>);
        let server = TurnServer::start(cfg).await?;
        let server_addr = server.local_addr();

        // Issue a single-use session ticket and get a nonce challenge.
        let eph_key = SecretKey::random();
        let res = server
            .reserve_exit_session(eph_key.pubkey(), Duration::from_secs(30))
            .await?;
        let creds = TurnCredentials::from_session_ticket(&res.ticket, &eph_key, res.server_pubkey)?;

        let client_sock = UdpSocket::bind("127.0.0.1:0").await?;
        let mut unauth = Message::new();
        unauth
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(turn::proto::reqtrans::RequestedTransport {
                    protocol: turn::proto::PROTO_UDP,
                }),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        client_sock.send_to(&unauth.raw, server_addr).await?;
        let mut buf = vec![0_u8; 1500];
        let (n, _) = client_sock.recv_from(&mut buf).await?;
        let mut challenge = Message {
            raw: buf[..n].to_vec(),
            ..Default::default()
        };
        challenge.decode().unwrap();
        let mut nonce = Nonce::new(ATTR_NONCE, String::new());
        let mut realm = Realm::new(ATTR_REALM, String::new());
        nonce.get_from(&challenge).unwrap();
        realm.get_from(&challenge).unwrap();
        let mi_key = generate_auth_key(&creds.username, &realm.text, &creds.credential);

        let tx_id = TransactionId::new();
        let mut auth_alloc = Message::new();
        auth_alloc
            .build(&[
                Box::new(Message {
                    transaction_id: tx_id,
                    ..Default::default()
                }),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(turn::proto::reqtrans::RequestedTransport {
                    protocol: turn::proto::PROTO_UDP,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ])
            .unwrap();

        let started_fut = factory.started.notified();
        client_sock.send_to(&auth_alloc.raw, server_addr).await?;
        started_fut.await;
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);

        // Retransmit the exact same single-use ticket Allocate while the first open is suspended:
        // must coalesce (no 401 error, no second factory call), AND read_loop must remain
        // non-blocking for unrelated STUN Binding requests!
        client_sock.send_to(&auth_alloc.raw, server_addr).await?;
        let observer_sock = UdpSocket::bind("127.0.0.1:0").await?;
        let mut binding_req = Message::new();
        binding_req
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_BINDING, CLASS_REQUEST)),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        observer_sock.send_to(&binding_req.raw, server_addr).await?;
        let (bn, _) = tokio::time::timeout(
            Duration::from_millis(200),
            observer_sock.recv_from(&mut buf),
        )
        .await
        .expect("read_loop must not block on slow async Allocate")?;
        let mut binding_resp = Message {
            raw: buf[..bn].to_vec(),
            ..Default::default()
        };
        binding_resp.decode().unwrap();
        assert_eq!(binding_resp.typ.class, CLASS_SUCCESS_RESPONSE);
        let mut xma = XorMappedAddress::default();
        xma.get_from(&binding_resp).unwrap();
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);

        // Release the open: client receives 200 Allocate Success with exact E_exit relayed_addr.
        factory.release.notify_waiters();
        let (an, _) =
            tokio::time::timeout(Duration::from_millis(500), client_sock.recv_from(&mut buf))
                .await
                .expect("coalesced Allocate must succeed")?;
        let mut alloc_resp = Message {
            raw: buf[..an].to_vec(),
            ..Default::default()
        };
        alloc_resp.decode().unwrap();
        assert_eq!(alloc_resp.typ.class, CLASS_SUCCESS_RESPONSE);
        let mut relayed = RelayedAddress::default();
        relayed.get_from(&alloc_resp).unwrap();
        assert_eq!(
            SocketAddr::new(relayed.ip, relayed.port),
            "198.51.100.99:50000".parse::<SocketAddr>().unwrap()
        );

        // CreatePermission must synchronize peer IP to TurnDatagramRelay::allow_peer_ips before 200.
        let mut perm_req = Message::new();
        perm_req
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                Box::new(turn::proto::peeraddr::PeerAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 77)),
                    port: 0,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce.text.clone())),
                Box::new(MessageIntegrity(mi_key)),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        client_sock.send_to(&perm_req.raw, server_addr).await?;
        let (pkt_len, _) = client_sock.recv_from(&mut buf).await?;
        let mut perm_resp = Message {
            raw: buf[..pkt_len].to_vec(),
            ..Default::default()
        };
        perm_resp.decode().unwrap();
        assert_eq!(perm_resp.typ.class, CLASS_SUCCESS_RESPONSE);
        let installed_relay = factory.last_relay.lock().unwrap().clone().unwrap();
        assert_eq!(*installed_relay.permitted_ips.lock().unwrap(), vec![
            IpAddr::V4(Ipv4Addr::new(198, 51, 100, 77))
        ]);

        // Closing server must tear down the active relay cleanly without blocking on recv_datagram.
        tokio::time::timeout(Duration::from_millis(500), server.close())
            .await
            .expect("server.close() must finish promptly")?;
        assert_eq!(closed_count.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_stalled_relay_backpressure_does_not_block_global_turn_read_loop() -> Result<()> {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        use stun::message::METHOD_REFRESH;
        use stun::message::METHOD_SEND;
        use tokio::sync::Notify;

        struct StalledFirstRelay {
            addr: SocketAddr,
            stall_sends: bool,
            closed: Notify,
            delivered: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl TurnDatagramRelay for StalledFirstRelay {
            fn relayed_addr(&self) -> SocketAddr {
                self.addr
            }

            async fn recv_datagram(&self, _buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
                self.closed.notified().await;
                Err(std::io::Error::other("closed"))
            }

            async fn send_to(&self, buf: &[u8], _target: SocketAddr) -> std::io::Result<usize> {
                if self.stall_sends {
                    self.closed.notified().await;
                    return Err(std::io::Error::other("closed"));
                }
                self.delivered.fetch_add(1, Ordering::SeqCst);
                Ok(buf.len())
            }

            fn begin_close(&self) -> TurnRelayCloseHandle {
                self.closed.notify_waiters();
                None
            }

            async fn finish_close(&self, _handle: TurnRelayCloseHandle) {
                self.closed.notify_waiters();
            }
        }

        struct StalledFirstFactory {
            calls: AtomicUsize,
            healthy_delivered: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl TurnDatagramRelayFactory for StalledFirstFactory {
            fn supports_family(&self, use_ipv4: bool) -> bool {
                use_ipv4
            }

            async fn allocate_relay(
                &self,
                _use_ipv4: bool,
                _need_even_port: bool,
            ) -> std::io::Result<Arc<dyn TurnDatagramRelay>> {
                let idx = self.calls.fetch_add(1, Ordering::SeqCst);
                let addr = SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(198, 51, 100, 90)),
                    51_000 + u16::try_from(idx).unwrap(),
                );
                Ok(Arc::new(StalledFirstRelay {
                    addr,
                    stall_sends: idx == 0,
                    closed: Notify::new(),
                    delivered: Arc::clone(&self.healthy_delivered),
                }))
            }
        }

        let healthy_delivered = Arc::new(AtomicUsize::new(0));
        let factory = Arc::new(StalledFirstFactory {
            calls: AtomicUsize::new(0),
            healthy_delivered: Arc::clone(&healthy_delivered),
        });

        let mut cfg = TurnServerConfig::loopback_ephemeral();
        cfg.max_allocations = 4;
        cfg.custom_relay_factory = Some(Arc::clone(&factory) as Arc<dyn TurnDatagramRelayFactory>);
        let server = TurnServer::start(cfg).await?;
        let server_addr = server.local_addr();

        async fn auth_and_allocate(
            server: &TurnServer,
            server_addr: SocketAddr,
        ) -> Result<(UdpSocket, TurnCredentials, String, String, Vec<u8>)> {
            let eph_key = SecretKey::random();
            let res = server
                .reserve_exit_session(eph_key.pubkey(), Duration::from_secs(30))
                .await?;
            let creds =
                TurnCredentials::from_session_ticket(&res.ticket, &eph_key, res.server_pubkey)?;
            let sock = UdpSocket::bind("127.0.0.1:0").await?;
            let mut unauth = Message::new();
            unauth
                .build(&[
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                    Box::new(turn::proto::reqtrans::RequestedTransport {
                        protocol: turn::proto::PROTO_UDP,
                    }),
                    Box::new(FINGERPRINT),
                ])
                .unwrap();
            sock.send_to(&unauth.raw, server_addr).await?;
            let mut buf = vec![0_u8; 1500];
            let (n, _) = sock.recv_from(&mut buf).await?;
            let mut challenge = Message {
                raw: buf[..n].to_vec(),
                ..Default::default()
            };
            challenge.decode().unwrap();
            let mut nonce = Nonce::new(ATTR_NONCE, String::new());
            let mut realm = Realm::new(ATTR_REALM, String::new());
            nonce.get_from(&challenge).unwrap();
            realm.get_from(&challenge).unwrap();
            let mi_key = generate_auth_key(&creds.username, &realm.text, &creds.credential);

            let mut alloc_req = Message::new();
            alloc_req
                .build(&[
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                    Box::new(turn::proto::reqtrans::RequestedTransport {
                        protocol: turn::proto::PROTO_UDP,
                    }),
                    Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                    Box::new(Realm::new(ATTR_REALM, realm.text.clone())),
                    Box::new(Nonce::new(ATTR_NONCE, nonce.text.clone())),
                    Box::new(MessageIntegrity(mi_key.clone())),
                    Box::new(FINGERPRINT),
                ])
                .unwrap();
            sock.send_to(&alloc_req.raw, server_addr).await?;
            let (an, _) =
                tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut buf))
                    .await
                    .expect("Allocate must respond promptly")?;
            let mut alloc_resp = Message {
                raw: buf[..an].to_vec(),
                ..Default::default()
            };
            alloc_resp.decode().unwrap();
            assert_eq!(alloc_resp.typ.class, CLASS_SUCCESS_RESPONSE);
            Ok((sock, creds, realm.text, nonce.text, mi_key))
        }

        let (c1_sock, c1_creds, c1_realm, c1_nonce, c1_mi) =
            auth_and_allocate(&server, server_addr).await?;
        let peer_ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 50));
        let peer_addr = SocketAddr::new(peer_ip, 9999);

        let mut c1_perm = Message::new();
        c1_perm
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                Box::new(turn::proto::peeraddr::PeerAddress {
                    ip: peer_ip,
                    port: 0,
                }),
                Box::new(Username::new(ATTR_USERNAME, c1_creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, c1_realm.clone())),
                Box::new(Nonce::new(ATTR_NONCE, c1_nonce.clone())),
                Box::new(MessageIntegrity(c1_mi.clone())),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        c1_sock.send_to(&c1_perm.raw, server_addr).await?;
        let mut buf = vec![0_u8; 1500];
        let _ = c1_sock.recv_from(&mut buf).await?;

        // Flood 128 Send indications on Client 1 whose relay blocks indefinitely in `send_to`.
        for _ in 0..128 {
            let mut send_ind = Message::new();
            send_ind
                .build(&[
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(
                        METHOD_SEND,
                        stun::message::CLASS_INDICATION,
                    )),
                    Box::new(turn::proto::peeraddr::PeerAddress {
                        ip: peer_addr.ip(),
                        port: peer_addr.port(),
                    }),
                    Box::new(turn::proto::data::Data(b"congested".to_vec())),
                    Box::new(FINGERPRINT),
                ])
                .unwrap();
            c1_sock.send_to(&send_ind.raw, server_addr).await?;
        }

        // Client 1's own Refresh AND a brand-new Client 2's Allocate + CreatePermission + Send
        // must still succeed immediately without stalling on Client 1's relay backpressure.
        let mut refresh_req = Message::new();
        refresh_req
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)),
                Box::new(Username::new(ATTR_USERNAME, c1_creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, c1_realm)),
                Box::new(Nonce::new(ATTR_NONCE, c1_nonce)),
                Box::new(MessageIntegrity(c1_mi)),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        c1_sock.send_to(&refresh_req.raw, server_addr).await?;
        let (rn, _) = tokio::time::timeout(Duration::from_millis(300), c1_sock.recv_from(&mut buf))
            .await
            .expect("Refresh must not stall behind congested relay sends")?;
        let mut refresh_resp = Message {
            raw: buf[..rn].to_vec(),
            ..Default::default()
        };
        refresh_resp.decode().unwrap();
        assert_eq!(refresh_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        let (c2_sock, c2_creds, c2_realm, c2_nonce, c2_mi) =
            auth_and_allocate(&server, server_addr).await?;
        let mut c2_perm = Message::new();
        c2_perm
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                Box::new(turn::proto::peeraddr::PeerAddress {
                    ip: peer_ip,
                    port: 0,
                }),
                Box::new(Username::new(ATTR_USERNAME, c2_creds.username)),
                Box::new(Realm::new(ATTR_REALM, c2_realm)),
                Box::new(Nonce::new(ATTR_NONCE, c2_nonce)),
                Box::new(MessageIntegrity(c2_mi)),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        c2_sock.send_to(&c2_perm.raw, server_addr).await?;
        let _ = tokio::time::timeout(Duration::from_millis(300), c2_sock.recv_from(&mut buf))
            .await
            .expect("Client 2 CreatePermission must succeed")?;

        let mut c2_send = Message::new();
        c2_send
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(
                    METHOD_SEND,
                    stun::message::CLASS_INDICATION,
                )),
                Box::new(turn::proto::peeraddr::PeerAddress {
                    ip: peer_addr.ip(),
                    port: peer_addr.port(),
                }),
                Box::new(turn::proto::data::Data(b"healthy".to_vec())),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        c2_sock.send_to(&c2_send.raw, server_addr).await?;
        tokio::time::timeout(Duration::from_millis(300), async {
            while healthy_delivered.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Client 2 datagram must be forwarded independently");

        tokio::time::timeout(Duration::from_millis(500), server.close())
            .await
            .expect("server.close() must finish promptly")?;
        Ok(())
    }

    #[tokio::test]
    async fn test_tcp_disconnect_cancels_pending_relay_open_and_caps_concurrency() -> Result<()> {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;
        use tokio::sync::Notify;

        struct InFlightGuard(Arc<AtomicUsize>);
        impl Drop for InFlightGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        struct ConcurrencyBoundFactory {
            in_flight: Arc<AtomicUsize>,
            peak_in_flight: AtomicUsize,
            started: Notify,
            release: Notify,
        }

        #[async_trait::async_trait]
        impl TurnDatagramRelayFactory for ConcurrencyBoundFactory {
            fn supports_family(&self, use_ipv4: bool) -> bool {
                use_ipv4
            }

            async fn allocate_relay(
                &self,
                _use_ipv4: bool,
                _need_even_port: bool,
            ) -> std::io::Result<Arc<dyn TurnDatagramRelay>> {
                let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                let _guard = InFlightGuard(Arc::clone(&self.in_flight));
                self.peak_in_flight.fetch_max(cur, Ordering::SeqCst);
                self.started.notify_waiters();
                self.release.notified().await;
                Ok(Arc::new(DummyClosedRelay {
                    addr: "198.51.100.88:52000".parse().unwrap(),
                    closed: Notify::new(),
                }))
            }
        }

        struct DummyClosedRelay {
            addr: SocketAddr,
            closed: Notify,
        }

        #[async_trait::async_trait]
        impl TurnDatagramRelay for DummyClosedRelay {
            fn relayed_addr(&self) -> SocketAddr {
                self.addr
            }

            async fn recv_datagram(&self, _buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
                self.closed.notified().await;
                Err(std::io::Error::other("closed"))
            }

            async fn send_to(&self, buf: &[u8], _target: SocketAddr) -> std::io::Result<usize> {
                Ok(buf.len())
            }

            fn begin_close(&self) -> TurnRelayCloseHandle {
                self.closed.notify_waiters();
                None
            }

            async fn finish_close(&self, _handle: TurnRelayCloseHandle) {}
        }

        let factory = Arc::new(ConcurrencyBoundFactory {
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak_in_flight: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
        });

        let mut cfg = TurnServerConfig::loopback_ephemeral();
        cfg.max_allocations = 1;
        cfg.max_reservations = 16;
        cfg.custom_relay_factory = Some(Arc::clone(&factory) as Arc<dyn TurnDatagramRelayFactory>);
        let server = TurnServer::start(cfg).await?;
        let server_addr = server.local_addr();

        for _ in 0..4 {
            let eph_key = SecretKey::random();
            let res = server
                .reserve_exit_session(eph_key.pubkey(), Duration::from_secs(30))
                .await?;
            let creds =
                TurnCredentials::from_session_ticket(&res.ticket, &eph_key, res.server_pubkey)?;

            let mut tcp = TcpStream::connect(server_addr).await?;
            let mut unauth = Message::new();
            unauth
                .build(&[
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                    Box::new(turn::proto::reqtrans::RequestedTransport {
                        protocol: turn::proto::PROTO_UDP,
                    }),
                    Box::new(FINGERPRINT),
                ])
                .unwrap();
            tcp.write_all(&unauth.raw).await?;

            let mut hdr = [0_u8; 20];
            tcp.read_exact(&mut hdr).await?;
            let body_len = usize::from(u16::from_be_bytes([hdr[2], hdr[3]]));
            let mut raw_chal = Vec::with_capacity(20 + body_len);
            raw_chal.extend_from_slice(&hdr);
            raw_chal.resize(20 + body_len, 0);
            tcp.read_exact(&mut raw_chal[20..]).await?;

            let mut challenge = Message {
                raw: raw_chal,
                ..Default::default()
            };
            challenge.decode().unwrap();
            let mut nonce = Nonce::new(ATTR_NONCE, String::new());
            let mut realm = Realm::new(ATTR_REALM, String::new());
            nonce.get_from(&challenge).unwrap();
            realm.get_from(&challenge).unwrap();
            let mi_key = generate_auth_key(&creds.username, &realm.text, &creds.credential);

            let mut auth_alloc = Message::new();
            auth_alloc
                .build(&[
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                    Box::new(turn::proto::reqtrans::RequestedTransport {
                        protocol: turn::proto::PROTO_UDP,
                    }),
                    Box::new(Username::new(ATTR_USERNAME, creds.username)),
                    Box::new(Realm::new(ATTR_REALM, realm.text)),
                    Box::new(Nonce::new(ATTR_NONCE, nonce.text)),
                    Box::new(MessageIntegrity(mi_key)),
                    Box::new(FINGERPRINT),
                ])
                .unwrap();

            let started_fut = factory.started.notified();
            tcp.write_all(&auth_alloc.raw).await?;
            started_fut.await;
            assert_eq!(factory.in_flight.load(Ordering::SeqCst), 1);

            // Disconnect TCP client mid-open: must cancel the in-flight `allocate_relay` promptly!
            drop(tcp);
            tokio::time::timeout(Duration::from_millis(300), async {
                while factory.in_flight.load(Ordering::SeqCst) != 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("TCP disconnect must cancel in-flight allocate_relay without waiting for server shutdown");
        }

        assert_eq!(factory.peak_in_flight.load(Ordering::SeqCst), 1);

        // And capacity is cleanly available for a subsequent completing client:
        let eph_key = SecretKey::random();
        let res = server
            .reserve_exit_session(eph_key.pubkey(), Duration::from_secs(30))
            .await?;
        let creds = TurnCredentials::from_session_ticket(&res.ticket, &eph_key, res.server_pubkey)?;
        let sock = UdpSocket::bind("127.0.0.1:0").await?;
        let mut unauth = Message::new();
        unauth
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(turn::proto::reqtrans::RequestedTransport {
                    protocol: turn::proto::PROTO_UDP,
                }),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        sock.send_to(&unauth.raw, server_addr).await?;
        let mut buf = vec![0_u8; 1500];
        let (n, _) = sock.recv_from(&mut buf).await?;
        let mut challenge = Message {
            raw: buf[..n].to_vec(),
            ..Default::default()
        };
        challenge.decode().unwrap();
        let mut nonce = Nonce::new(ATTR_NONCE, String::new());
        let mut realm = Realm::new(ATTR_REALM, String::new());
        nonce.get_from(&challenge).unwrap();
        realm.get_from(&challenge).unwrap();
        let mi_key = generate_auth_key(&creds.username, &realm.text, &creds.credential);
        let mut auth_alloc = Message::new();
        auth_alloc
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(turn::proto::reqtrans::RequestedTransport {
                    protocol: turn::proto::PROTO_UDP,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username)),
                Box::new(Realm::new(ATTR_REALM, realm.text)),
                Box::new(Nonce::new(ATTR_NONCE, nonce.text)),
                Box::new(MessageIntegrity(mi_key)),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        let started_fut = factory.started.notified();
        sock.send_to(&auth_alloc.raw, server_addr).await?;
        started_fut.await;
        factory.release.notify_waiters();
        let (an, _) = tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut buf))
            .await
            .expect("subsequent Allocate must succeed")?;
        let mut alloc_resp = Message {
            raw: buf[..an].to_vec(),
            ..Default::default()
        };
        alloc_resp.decode().unwrap();
        assert_eq!(alloc_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        server.close().await?;
        Ok(())
    }

    #[test]
    fn turn_allocation_core_install_permissions_returns_expired_open_id() {
        use crate::relay_backend::BeginAllocateOutcome;
        use crate::relay_backend::CompleteAllocateDecision;
        use crate::relay_backend::CorePermissionError;
        use crate::relay_backend::TurnFiveTupleKey;

        let mut core = TurnAllocationCore::new(2, 4, 4);
        let tuple = TurnFiveTupleKey {
            src_addr: "127.0.0.1:41000".parse().unwrap(),
            dst_addr: "127.0.0.1:3478".parse().unwrap(),
            protocol: 17,
        };
        let BeginAllocateOutcome::StartOpen { open_id } = core
            .begin_allocate(tuple, [7u8; 12], "u".to_string(), None, 1_000, 100)
            .outcome
        else {
            panic!("expected StartOpen");
        };
        assert!(matches!(
            core.complete_allocate(
                tuple,
                open_id,
                Ok("198.51.100.10:50000".parse().unwrap()),
                100
            ),
            CompleteAllocateDecision::Installed { .. }
        ));

        // At t = 1_100 (expiry boundary), install_permissions must surface `Some(open_id)` for cleanup.
        assert_eq!(
            core.install_permissions(
                &tuple,
                &[IpAddr::V4(Ipv4Addr::new(198, 51, 100, 20))],
                300_000,
                1_100
            ),
            Err(CorePermissionError::AllocationMismatch {
                expired_open_id: Some(open_id),
            })
        );
        // Subsequent call on already-evicted tuple reports `None`.
        assert_eq!(
            core.install_permissions(
                &tuple,
                &[IpAddr::V4(Ipv4Addr::new(198, 51, 100, 20))],
                300_000,
                1_100
            ),
            Err(CorePermissionError::AllocationMismatch {
                expired_open_id: None,
            })
        );
    }

    #[tokio::test]
    async fn test_full_control_queue_rejects_create_permission_without_authorizing_peer(
    ) -> Result<()> {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        use stun::error_code::ErrorCodeAttribute;
        use stun::error_code::CODE_INSUFFICIENT_CAPACITY;
        use tokio::sync::Notify;

        struct StalledPinholeRelay {
            addr: SocketAddr,
            closed: Notify,
            entered_pinhole: Notify,
            release_pinhole: Notify,
            stall: AtomicBool,
            sent_targets: std::sync::Mutex<Vec<SocketAddr>>,
        }

        #[async_trait::async_trait]
        impl TurnDatagramRelay for StalledPinholeRelay {
            fn relayed_addr(&self) -> SocketAddr {
                self.addr
            }

            async fn recv_datagram(&self, _buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
                self.closed.notified().await;
                Err(std::io::Error::other("closed"))
            }

            async fn send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
                self.sent_targets.lock().unwrap().push(target);
                Ok(buf.len())
            }

            async fn allow_peer_ips(
                &self,
                _peer_ips: &[IpAddr],
                _ttl: Duration,
            ) -> std::io::Result<()> {
                if self.stall.load(Ordering::SeqCst) {
                    self.entered_pinhole.notify_waiters();
                    self.release_pinhole.notified().await;
                }
                Ok(())
            }

            fn begin_close(&self) -> TurnRelayCloseHandle {
                self.closed.notify_waiters();
                None
            }

            async fn finish_close(&self, _handle: TurnRelayCloseHandle) {}
        }

        struct SingleRelayFactory(Arc<StalledPinholeRelay>);

        #[async_trait::async_trait]
        impl TurnDatagramRelayFactory for SingleRelayFactory {
            fn supports_family(&self, use_ipv4: bool) -> bool {
                use_ipv4
            }

            async fn allocate_relay(
                &self,
                _use_ipv4: bool,
                _need_even_port: bool,
            ) -> std::io::Result<Arc<dyn TurnDatagramRelay>> {
                Ok(Arc::clone(&self.0) as Arc<dyn TurnDatagramRelay>)
            }
        }

        let relay = Arc::new(StalledPinholeRelay {
            addr: "198.51.100.77:53000".parse().unwrap(),
            closed: Notify::new(),
            entered_pinhole: Notify::new(),
            release_pinhole: Notify::new(),
            stall: AtomicBool::new(true),
            sent_targets: std::sync::Mutex::new(Vec::new()),
        });
        let mut cfg = TurnServerConfig::loopback_ephemeral();
        cfg.custom_relay_factory =
            Some(Arc::new(SingleRelayFactory(Arc::clone(&relay)))
                as Arc<dyn TurnDatagramRelayFactory>);
        let server = TurnServer::start(cfg).await?;
        let server_addr = server.local_addr();

        let eph_key = SecretKey::random();
        let res = server
            .reserve_exit_session(eph_key.pubkey(), Duration::from_secs(30))
            .await?;
        let creds = TurnCredentials::from_session_ticket(&res.ticket, &eph_key, res.server_pubkey)?;

        let sock = UdpSocket::bind("127.0.0.1:0").await?;
        let mut unauth = Message::new();
        unauth
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(turn::proto::reqtrans::RequestedTransport {
                    protocol: turn::proto::PROTO_UDP,
                }),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        sock.send_to(&unauth.raw, server_addr).await?;
        let mut buf = vec![0_u8; 1500];
        let (n, _) = sock.recv_from(&mut buf).await?;
        let mut challenge = Message {
            raw: buf[..n].to_vec(),
            ..Default::default()
        };
        challenge.decode().unwrap();
        let mut nonce = Nonce::new(ATTR_NONCE, String::new());
        let mut realm = Realm::new(ATTR_REALM, String::new());
        nonce.get_from(&challenge).unwrap();
        realm.get_from(&challenge).unwrap();
        let mi_key = generate_auth_key(&creds.username, &realm.text, &creds.credential);

        let mut auth_alloc = Message::new();
        auth_alloc
            .build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                Box::new(turn::proto::reqtrans::RequestedTransport {
                    protocol: turn::proto::PROTO_UDP,
                }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        sock.send_to(&auth_alloc.raw, server_addr).await?;
        let (an, _) = sock.recv_from(&mut buf).await?;
        let mut alloc_resp = Message {
            raw: buf[..an].to_vec(),
            ..Default::default()
        };
        alloc_resp.decode().unwrap();
        assert_eq!(alloc_resp.typ.class, CLASS_SUCCESS_RESPONSE);

        let build_perm = |ip: IpAddr| {
            let mut msg = Message::new();
            msg.build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                Box::new(PeerAddress { ip, port: 0 }),
                Box::new(Username::new(ATTR_USERNAME, creds.username.clone())),
                Box::new(Realm::new(ATTR_REALM, realm.text.clone())),
                Box::new(Nonce::new(ATTR_NONCE, nonce.text.clone())),
                Box::new(MessageIntegrity(mi_key.clone())),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
            msg.raw
        };

        // First CreatePermission enters `allow_peer_ips` and stalls the allocation worker.
        let entered = relay.entered_pinhole.notified();
        sock.send_to(
            &build_perm(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1))),
            server_addr,
        )
        .await?;
        entered.await;

        // Fill all 16 queued slots (`MAX_ALLOCATION_CONTROL_QUEUE = 16`).
        for idx in 2_u8..=17_u8 {
            sock.send_to(
                &build_perm(IpAddr::V4(Ipv4Addr::new(198, 51, 100, idx))),
                server_addr,
            )
            .await?;
        }
        // Give `read_loop` a brief moment to enqueue all 16 before sending the 18th.
        tokio::time::sleep(Duration::from_millis(30)).await;

        let rejected_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 99));
        sock.send_to(&build_perm(rejected_ip), server_addr).await?;
        let (rn, _) = tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut buf))
            .await
            .expect("overflow CreatePermission must immediately return 508")?;
        let mut err_resp = Message {
            raw: buf[..rn].to_vec(),
            ..Default::default()
        };
        err_resp.decode().unwrap();
        assert_eq!(err_resp.typ.class, CLASS_ERROR_RESPONSE);
        let mut err_code = ErrorCodeAttribute::default();
        err_code.get_from(&err_resp).unwrap();
        assert_eq!(err_code.code.0, CODE_INSUFFICIENT_CAPACITY.0);

        // Unblock the worker and drain the 17 queued 200 responses.
        relay.stall.store(false, Ordering::SeqCst);
        relay.release_pinhole.notify_waiters();
        for _ in 0..17 {
            let (sn, _) =
                tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut buf))
                    .await
                    .expect("queued CreatePermission must complete")?;
            let mut ok_resp = Message {
                raw: buf[..sn].to_vec(),
                ..Default::default()
            };
            ok_resp.decode().unwrap();
            assert_eq!(ok_resp.typ.class, CLASS_SUCCESS_RESPONSE);
        }

        // Send indications to `rejected_ip` (must NOT be authorized) and `198.51.100.1` (authorized).
        for ip in [rejected_ip, IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1))] {
            let mut send_ind = Message::new();
            send_ind
                .build(&[
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(
                        METHOD_SEND,
                        stun::message::CLASS_INDICATION,
                    )),
                    Box::new(PeerAddress { ip, port: 9000 }),
                    Box::new(turn::proto::data::Data(b"probe".to_vec())),
                    Box::new(FINGERPRINT),
                ])
                .unwrap();
            sock.send_to(&send_ind.raw, server_addr).await?;
        }
        tokio::time::timeout(Duration::from_millis(300), async {
            while relay.sent_targets.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("authorized Send indication must reach relay");
        assert_eq!(*relay.sent_targets.lock().unwrap(), vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)),
            9000
        )]);

        server.close().await?;

        // `max_allocations = usize::MAX` must clamp to `Semaphore::MAX_PERMITS` without panicking.
        let mut unbounded_cfg = TurnServerConfig::loopback_ephemeral();
        unbounded_cfg.max_allocations = usize::MAX;
        let unbounded_server = TurnServer::start(unbounded_cfg).await?;
        unbounded_server.close().await?;
        Ok(())
    }

    #[test]
    fn test_begin_allocate_distinguishes_expired_vs_unknown_ticket() {
        let mut core = TurnAllocationCore::new(2, 4, 4);
        let tuple = TurnFiveTupleKey {
            src_addr: "127.0.0.1:42000".parse().unwrap(),
            dst_addr: "127.0.0.1:3478".parse().unwrap(),
            protocol: 17,
        };
        let expired_sid = [1u8; 16];
        core.reserve_ticket(expired_sid, 1_500, 1_000).unwrap();

        // Unknown ticket is rejected as RejectTicketUnknown.
        assert_eq!(
            core.begin_allocate(
                tuple,
                [2u8; 12],
                "u-unknown".to_string(),
                Some([9u8; 16]),
                1_000,
                1_200,
            )
            .outcome,
            BeginAllocateOutcome::RejectTicketUnknown
        );

        // Expired reserved ticket returns RejectTicketExpired on first Allocate, then prunes it.
        assert_eq!(
            core.begin_allocate(
                tuple,
                [3u8; 12],
                "u-expired".to_string(),
                Some(expired_sid),
                1_000,
                1_501,
            )
            .outcome,
            BeginAllocateOutcome::RejectTicketExpired
        );
        assert_eq!(
            core.begin_allocate(
                tuple,
                [4u8; 12],
                "u-expired".to_string(),
                Some(expired_sid),
                1_000,
                1_502,
            )
            .outcome,
            BeginAllocateOutcome::RejectTicketUnknown
        );

        // Valid reserved ticket within window enters StartOpen.
        let valid_sid = [5u8; 16];
        core.reserve_ticket(valid_sid, 2_000, 1_600).unwrap();
        assert!(matches!(
            core.begin_allocate(
                tuple,
                [5u8; 12],
                "u-valid".to_string(),
                Some(valid_sid),
                1_000,
                2_000,
            )
            .outcome,
            BeginAllocateOutcome::StartOpen { .. }
        ));
    }

    #[test]
    fn test_delegation_auth_handler_authenticate_enforces_expected_owner_statelessly() {
        let now_ms = 1_700_000_000_000_u128;
        let clock: rings_core::utils::SharedClock = Arc::new(ManualClock::new(now_ms));
        let server_key = SecretKey::random();
        let handler = DelegationAuthHandler::with_server_key_and_clock(
            TurnAdmissionPolicy::AllowAnyDelegation,
            server_key,
            clock,
        );

        let key1 = DelegateeKey::new_with_seckey_at(&SecretKey::random(), 60_000, now_ms).unwrap();
        let key2 = DelegateeKey::new_with_seckey_at(&SecretKey::random(), 60_000, now_ms).unwrap();
        let creds1 = TurnCredentials::from_delegatee_key(&key1, handler.server_pubkey()).unwrap();
        let creds2 = TurnCredentials::from_delegatee_key(&key2, handler.server_pubkey()).unwrap();

        let (verified1, _) = handler
            .authenticate(&creds1.username, "rings", Some(&creds1.username))
            .unwrap();
        assert_eq!(verified1.did(), key1.delegator_did());

        let err = handler
            .authenticate(&creds2.username, "rings", Some(&creds1.username))
            .unwrap_err();
        assert!(matches!(err, TurnError::AllocationOwnerMismatch));

        // Simultaneous independent request without expected_owner succeeds without interference.
        let (verified2, _) = handler
            .authenticate(&creds2.username, "rings", None)
            .unwrap();
        assert_eq!(verified2.did(), key2.delegator_did());
    }

    #[tokio::test]
    async fn test_turn_server_start_paused_holds_packets_until_resume_and_closes_cleanly(
    ) -> Result<()> {
        let paused_only = TurnServer::start_paused(TurnServerConfig::loopback_ephemeral()).await?;
        paused_only.close().await?;

        let server = TurnServer::start_paused(TurnServerConfig::loopback_ephemeral()).await?;
        let sock = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?;
        let tx_id = TransactionId::new();
        let mut binding_req = Message::new();
        binding_req
            .build(&[
                Box::new(tx_id),
                Box::new(MessageType::new(
                    stun::message::METHOD_BINDING,
                    CLASS_REQUEST,
                )),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
        sock.send_to(&binding_req.raw, server.local_addr()).await?;

        let mut buf = [0u8; 1500];
        let early = tokio::time::timeout(Duration::from_millis(60), sock.recv_from(&mut buf)).await;
        assert!(
            early.is_err(),
            "paused TurnServer must not process incoming packets before resume()"
        );

        server.resume();
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), sock.recv_from(&mut buf))
            .await
            .expect("resumed TurnServer must answer buffered STUN Binding request")?;
        let mut resp = Message {
            raw: buf[..n].to_vec(),
            ..Default::default()
        };
        resp.decode().unwrap();
        assert_eq!(resp.transaction_id, tx_id);
        assert_eq!(resp.typ.class, CLASS_SUCCESS_RESPONSE);

        server.close().await?;
        Ok(())
    }
}
