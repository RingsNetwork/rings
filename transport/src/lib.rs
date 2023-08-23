mod callback;
pub mod connections;
pub mod core;
pub mod error;
pub mod ice_server;

use std::sync::Arc;

use dashmap::DashMap;

use crate::ice_server::IceServer;

#[derive(Clone)]
pub struct Transport<C> {
    ice_servers: Vec<IceServer>,
    external_address: Option<String>,
    connections: Arc<DashMap<String, C>>,
}

impl<C> Transport<C> {
    pub fn new(ice_servers: &str, external_address: Option<String>) -> Self {
        let ice_servers = IceServer::vec_from_str(ice_servers).unwrap();

        Self {
            ice_servers,
            external_address,
            connections: Arc::new(DashMap::new()),
        }
    }
}
