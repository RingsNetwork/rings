use serde::Deserialize;
use serde::Serialize;

use crate::util::loader::ResourceLoader;

#[derive(Deserialize, Serialize, Debug)]
pub struct Backend {
    pub tcp_proxy: Option<TcpProxyBackend>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct TcpProxyBackend {
    pub port: u16,
}

impl ResourceLoader for Backend {}
