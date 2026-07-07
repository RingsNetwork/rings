use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::net::TcpStream;

use super::super::read_connect_target;
use crate::error::Error;
use crate::error::Result;

#[tokio::test]
async fn connect_header_read_times_out() -> Result<()> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .map_err(|error| Error::HttpRequestError(error.to_string()))?;
    let addr = listener
        .local_addr()
        .map_err(|error| Error::HttpRequestError(error.to_string()))?;
    let client = TcpStream::connect(addr);
    let accepted = listener.accept();
    let (client, accepted) = tokio::join!(client, accepted);
    let _client = client.map_err(|error| Error::HttpRequestError(error.to_string()))?;
    let (mut server, _) = accepted.map_err(|error| Error::HttpRequestError(error.to_string()))?;

    let result = read_connect_target(&mut server, Duration::from_millis(10)).await;

    assert!(matches!(result, Err(Error::HttpRequestError(_))));
    Ok(())
}
