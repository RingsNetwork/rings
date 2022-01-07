use crate::ice_transport::IceTransport;
use hyper::{Body, Request, Response};
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

pub struct HttpTransport {
    pub address: String,
    pub ice_provider: IceTransport,
}

impl HttpTransport {
    pub async fn new(addr: &str, ice_provider: IceTransport) -> Self {
        Self {
            address: addr.to_string(),
            ice_provider: ice_provider,
        }
    }

    pub async fn handler(&mut self, _req: Request<Body>) -> Result<Response<Body>, Infallible> {
        match self.ice_provider.candidate().await {
            Some(c) => Ok(Response::new(c.into())),
            None => Ok(Response::new("404".into())),
        }
    }
}
