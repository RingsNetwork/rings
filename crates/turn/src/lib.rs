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
    use stun::message::Message;
    use stun::message::MessageType;
    use stun::message::Setter;
    use stun::message::CLASS_REQUEST;
    use stun::message::METHOD_ALLOCATE;
    use tokio::net::UdpSocket;
    use tokio::sync::mpsc;
    use turn::client::Client;
    use turn::client::ClientConfig;
    use turn::proto::reqtrans::RequestedTransport;
    use turn::proto::PROTO_UDP;
    use webrtc_util::Conn;

    use super::*;

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
}
