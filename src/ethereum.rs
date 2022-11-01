//! A tool integrate web3 transports.
use anyhow::anyhow;
use anyhow::Result;

use crate::prelude::rings_core::prelude::web3;

/// Transport wrapper either `Websock` or `Http`.
pub type Transport = web3::transports::Either<web3::transports::WebSocket, web3::transports::Http>;

/// link web3 with endpoint, using prefix `ws` or `http` determine transport protocol.
pub async fn link_web3(endpoint: &str) -> Result<web3::Web3<Transport>> {
    if endpoint.starts_with("ws") {
        let transport = web3::transports::WebSocket::new(endpoint).await?;
        Ok(web3::Web3::new(web3::transports::Either::Left(transport)))
    } else if endpoint.starts_with("http") {
        let transport = web3::transports::Http::new(endpoint)?;
        Ok(web3::Web3::new(web3::transports::Either::Right(transport)))
    } else {
        Err(anyhow!("Failed to parse eth_endpoint {:?}", endpoint))
    }
}
