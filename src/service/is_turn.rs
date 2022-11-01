//! A turn server and builder.
use std::collections::HashMap;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::time::Duration;
use turn::auth::*;
use turn::relay::relay_static::*;
use turn::server::config::*;
use turn::server::*;
use turn::Error;
use webrtc_util::vnet::net::*;

struct AuthBuilder {
    cred_map: HashMap<String, Vec<u8>>,
}

impl AuthBuilder {
    fn new(cred_map: HashMap<String, Vec<u8>>) -> Self {
        Self { cred_map }
    }
}

impl AuthHandler for AuthBuilder {
    fn auth_handle(
        &self,
        username: &str,
        _realm: &str,
        _src_addr: SocketAddr,
    ) -> Result<Vec<u8>, Error> {
        if let Some(pw) = self.cred_map.get(username) {
            Ok(pw.to_vec())
        } else {
            Err(Error::ErrFakeErr)
        }
    }
}

/// Run a udp turn server.
/// more about turn server: https://docs.rs/turn/latest/turn/server/struct.Server.html
pub async fn run_udp_turn(
    public_ip: &str,
    port: u16,
    username: &str,
    password: &str,
    realm: &str,
) -> Result<Server, Error> {
    let conn = Arc::new(UdpSocket::bind(format!("0.0.0.0:{}", port)).await?);
    let mut cred_map = HashMap::new();
    let auth = generate_auth_key(username, realm, password);
    cred_map.insert(username.to_owned(), auth);

    let server = Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn,
            relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                relay_address: IpAddr::from_str(public_ip)?,
                address: "0.0.0.0".to_owned(),
                net: Arc::new(Net::new(None)),
            }),
        }],
        realm: realm.to_owned(),
        auth_handler: Arc::new(AuthBuilder::new(cred_map)),
        channel_bind_timeout: Duration::from_secs(0),
    })
    .await?;
    Ok(server)
}
