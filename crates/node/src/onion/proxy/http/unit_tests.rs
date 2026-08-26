use super::*;

fn options() -> OnionHttpProxyOptions {
    OnionHttpProxyOptions::new(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        OnionServiceName::tcp(),
        0,
        false,
    )
}

#[test]
fn connect_request_line_parses_target() -> Result<()> {
    let target = parse_connect_request_line("CONNECT Example.COM:443 HTTP/1.1")?;

    assert_eq!(target.authority(), "example.com:443");
    Ok(())
}

#[test]
fn connect_request_line_rejects_plain_http_request() {
    assert!(matches!(
        parse_connect_request_line("GET http://example.com/ HTTP/1.1"),
        Err(Error::HttpRequestError(_))
    ));
}

#[test]
fn proxy_options_build_custom_tcp_service_config() -> Result<()> {
    let mut options = options();
    options.service = OnionServiceName::parse("web")?;

    let proxy = options.proxy_config()?;

    assert_eq!(proxy.exit_service(), "web");
    Ok(())
}

#[test]
fn proxy_options_reject_unbounded_connection_model() {
    let mut zero_connections = options();
    zero_connections.max_connections = 0;
    assert!(matches!(
        zero_connections.validate(),
        Err(Error::InvalidConfig(_))
    ));

    let mut zero_timeout = options();
    zero_timeout.header_timeout = Duration::ZERO;
    assert!(matches!(
        zero_timeout.validate(),
        Err(Error::InvalidConfig(_))
    ));
}
