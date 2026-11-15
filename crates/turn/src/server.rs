use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use rings_core::delegation::DelegateeKey;
use tokio::net::UdpSocket;
use turn::allocation::five_tuple::FiveTuple;
use turn::allocation::AllocationInfo;
use turn::relay::relay_none::RelayAddressGeneratorNone;
use turn::relay::relay_range::RelayAddressGeneratorRanges;
use turn::relay::relay_static::RelayAddressGeneratorStatic;
use turn::relay::RelayAddressGenerator;
use turn::server::config::ConnConfig;
use turn::server::config::ServerConfig;
use turn::server::Server;
use webrtc_util::vnet::net::Net;

use crate::auth::DelegationAuthHandler;
use crate::config::RelayAddressConfig;
use crate::config::TurnServerConfig;
use crate::error::Result;

/// A running Rings TURN server backed by [`DelegationAuthHandler`].
pub struct TurnServer {
    inner: Server,
    local_addr: SocketAddr,
    realm: String,
    auth_handler: Arc<DelegationAuthHandler>,
}

fn build_relay_generator(
    relay: &RelayAddressConfig,
) -> Box<dyn RelayAddressGenerator + Send + Sync> {
    let net = Arc::new(Net::new(None));
    match *relay {
        RelayAddressConfig::None { bind_ip } => Box::new(RelayAddressGeneratorNone {
            address: bind_ip.to_string(),
            net,
        }),
        RelayAddressConfig::Static { relay_ip, bind_ip } => Box::new(RelayAddressGeneratorStatic {
            relay_address: relay_ip,
            address: bind_ip.to_string(),
            net,
        }),
        RelayAddressConfig::Range {
            relay_ip,
            bind_ip,
            min_port,
            max_port,
            max_retries,
        } => Box::new(RelayAddressGeneratorRanges {
            relay_address: relay_ip,
            min_port,
            max_port,
            max_retries,
            address: bind_ip.to_string(),
            net,
        }),
    }
}

impl TurnServer {
    /// Bind the UDP listener socket and start the TURN server with [`DelegationAuthHandler`].
    pub async fn start(config: TurnServerConfig) -> Result<Self> {
        config.relay.validate()?;

        let socket = UdpSocket::bind(config.listen_addr).await?;
        let local_addr = socket.local_addr()?;
        let conn = Arc::new(socket);

        let auth_handler = Arc::new(DelegationAuthHandler::with_clock(
            config.admission,
            config.clock,
        ));
        let relay_addr_generator = build_relay_generator(&config.relay);

        let inner = Server::new(ServerConfig {
            conn_configs: vec![ConnConfig {
                conn,
                relay_addr_generator,
            }],
            realm: config.realm.clone(),
            auth_handler: Arc::clone(&auth_handler)
                as Arc<dyn turn::auth::AuthHandler + Send + Sync>,
            channel_bind_timeout: config.channel_bind_timeout,
            alloc_close_notify: None,
        })
        .await?;

        Ok(Self {
            inner,
            local_addr,
            realm: config.realm,
            auth_handler,
        })
    }

    /// Return the local UDP [`SocketAddr`] this TURN server is bound to.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Return the configured TURN authentication realm.
    pub fn realm(&self) -> &str {
        &self.realm
    }

    /// Return a reference to the server's [`DelegationAuthHandler`].
    pub const fn auth_handler(&self) -> &Arc<DelegationAuthHandler> {
        &self.auth_handler
    }

    /// Build a complete signed `turn://<username>:<credential>@<local_addr>` URL for `delegatee_key`.
    pub fn ice_server_url_for(&self, delegatee_key: &DelegateeKey) -> Result<String> {
        let base_url = format!("turn://{}", self.local_addr);
        Ok(delegatee_key.sign_ice_servers(&base_url)?)
    }

    /// Query active TURN allocations managed by this server.
    pub async fn allocations_info(
        &self,
        five_tuples: Option<Vec<FiveTuple>>,
    ) -> Result<HashMap<FiveTuple, AllocationInfo>> {
        Ok(self.inner.get_allocations_info(five_tuples).await?)
    }

    /// Delete all active TURN allocations associated with `username`.
    pub async fn delete_allocations_by_username(&self, username: String) -> Result<()> {
        Ok(self.inner.delete_allocations_by_username(username).await?)
    }

    /// Gracefully stop the TURN server and close all managed connections and relay allocations.
    pub async fn close(&self) -> Result<()> {
        Ok(self.inner.close().await?)
    }
}
