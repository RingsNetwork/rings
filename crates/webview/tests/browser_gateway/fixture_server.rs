use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use url::Url;

use super::*;

pub(super) struct BrowserFixtureServer {
    addr: std::net::SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<()>>>,
}

impl BrowserFixtureServer {
    pub(super) fn start(
        prefix: GatewayPrefix,
        gateway: WebviewGateway<BrowserFixtureTransport>,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| WebviewError::transport(error.to_string()))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| WebviewError::transport(error.to_string()))?;
        let addr = listener
            .local_addr()
            .map_err(|error| WebviewError::transport(error.to_string()))?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let gateway = Arc::new(Mutex::new(gateway));
        let handle =
            std::thread::spawn(move || serve_gateway(listener, thread_shutdown, prefix, gateway));
        Ok(Self {
            addr,
            shutdown,
            handle: Some(handle),
        })
    }

    pub(super) fn gateway_url(&self, gateway_path: &str) -> String {
        format!("http://{}{}", self.addr, gateway_path)
    }

    pub(super) fn stop(mut self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            handle.join().map_err(|_| {
                WebviewError::transport("browser fixture server panicked".to_string())
            })??;
        }
        Ok(())
    }
}

fn serve_gateway(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    prefix: GatewayPrefix,
    gateway: Arc<Mutex<WebviewGateway<BrowserFixtureTransport>>>,
) -> Result<()> {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let thread_prefix = prefix.clone();
                let thread_gateway = Arc::clone(&gateway);
                std::thread::spawn(move || {
                    let mut stream = stream;
                    if let Err(error) =
                        handle_connection(&mut stream, &thread_prefix, &thread_gateway)
                    {
                        write_fixture_error(&mut stream, error);
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(WebviewError::transport(error.to_string())),
        }
    }
    Ok(())
}

fn handle_connection(
    stream: &mut TcpStream,
    prefix: &GatewayPrefix,
    gateway: &Arc<Mutex<WebviewGateway<BrowserFixtureTransport>>>,
) -> Result<()> {
    stream
        .set_nonblocking(false)
        .map_err(|error| WebviewError::transport(error.to_string()))?;
    let request = read_http_request(stream)?;
    if request.path == "/assets/webview-overlay.js" {
        return write_http_response(
            stream,
            &GatewayResponse::new(
                200,
                vec![GatewayHeader::new(
                    "Content-Type",
                    "application/javascript; charset=utf-8",
                )?],
                br#"
globalThis.__ringsWebviewDebugOverlay = { installed: true };
document.documentElement.appendChild(Object.assign(document.createElement("div"), {
  id: "rings-webview-debug-overlay",
}));
"#
                .to_vec(),
            )?,
        );
    }
    let runtime_prefix = runtime_prefix(prefix);
    let gateway_path = if request.path.starts_with(prefix.as_str()) {
        request.path.clone()
    } else if request.path.starts_with(runtime_prefix.as_str()) {
        let target = header_value(&request.headers, "x-rings-webview-target")
            .ok_or_else(|| WebviewError::InvalidGatewayUrl(request.path.clone()))?;
        prefix.encode(TargetUrl::parse(target)?.as_url())
    } else {
        return write_http_response(
            stream,
            &GatewayResponse::new(
                404,
                vec![GatewayHeader::new("Content-Type", "text/plain")?],
                b"not found".to_vec(),
            )?,
        );
    };

    let kind = gateway_request_kind(&request);
    let mut gateway = gateway.lock().map_err(|_| {
        WebviewError::transport("browser fixture gateway lock poisoned".to_string())
    })?;
    let mut gateway_request =
        gateway_request_from_http(prefix, gateway_path.as_str(), kind, &request.headers)?;
    gateway_request.method = request.method;
    gateway_request.headers = request.headers;
    gateway_request.body = request.body;
    let response = futures::executor::block_on(gateway.send(gateway_request))?;
    write_http_response(stream, &response)
}

fn gateway_request_from_http(
    prefix: &GatewayPrefix,
    path: &str,
    kind: GatewayRequestKind,
    headers: &[GatewayHeader],
) -> Result<GatewayRequest> {
    let target = prefix.decode_path(path)?.into_url();
    Ok(match kind {
        GatewayRequestKind::Navigation => GatewayRequest::navigation(target),
        GatewayRequestKind::Subresource => GatewayRequest::subresource(target),
        GatewayRequestKind::Fetch => GatewayRequest::fetch(target, "GET")
            .with_source_origin(runtime_source_from_referer(prefix, headers)?),
        GatewayRequestKind::Xhr => GatewayRequest::xhr(target, "GET")
            .with_source_origin(runtime_source_from_referer(prefix, headers)?),
    })
}

fn runtime_source_from_referer(prefix: &GatewayPrefix, headers: &[GatewayHeader]) -> Result<Url> {
    let source = header_value(headers, "referer")
        .or_else(|| header_value(headers, "ping-from"))
        .ok_or(WebviewError::MissingRuntimeSourceOrigin)?;
    let source_url = Url::parse(source)?;
    Ok(prefix.decode_path(source_url.path())?.into_url())
}

fn write_fixture_error(stream: &mut TcpStream, error: WebviewError) {
    let body = format!("gateway fixture error: {error}");
    let Ok(content_type) = GatewayHeader::new("Content-Type", "text/plain") else {
        return;
    };
    let Ok(response) = GatewayResponse::new(500, vec![content_type], body.into_bytes()) else {
        return;
    };
    let _ = write_http_response(stream, &response);
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<GatewayHeader>,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| WebviewError::transport(error.to_string()))?;
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 1024];
        let read = stream
            .read(&mut chunk)
            .map_err(|error| WebviewError::transport(error.to_string()))?;
        if read == 0 {
            return Err(WebviewError::transport(
                "connection closed before request headers".to_string(),
            ));
        }
        let Some(bytes) = chunk.get(..read) else {
            return Err(WebviewError::transport("invalid read size".to_string()));
        };
        buffer.extend_from_slice(bytes);
        if let Some(index) = find_bytes(buffer.as_slice(), b"\r\n\r\n") {
            break index;
        }
    };
    let body_start = header_end
        .checked_add(4)
        .ok_or_else(|| WebviewError::transport("request header offset overflow".to_string()))?;
    let header_bytes = buffer
        .get(..header_end)
        .ok_or_else(|| WebviewError::transport("invalid request header slice".to_string()))?;
    let header_text = std::str::from_utf8(header_bytes)
        .map_err(|error| WebviewError::transport(error.to_string()))?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| WebviewError::transport("missing request line".to_string()))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| WebviewError::transport("missing request method".to_string()))?
        .to_string();
    let path = request_parts
        .next()
        .ok_or_else(|| WebviewError::transport("missing request path".to_string()))?
        .to_string();
    let mut headers = Vec::new();
    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let trimmed_value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = trimmed_value
                .parse::<usize>()
                .map_err(|error| WebviewError::transport(error.to_string()))?;
        }
        headers.push(GatewayHeader::new(name, trimmed_value)?);
    }
    let total_len = body_start
        .checked_add(content_length)
        .ok_or_else(|| WebviewError::transport("request body offset overflow".to_string()))?;
    while buffer.len() < total_len {
        let mut chunk = [0_u8; 1024];
        let read = stream
            .read(&mut chunk)
            .map_err(|error| WebviewError::transport(error.to_string()))?;
        if read == 0 {
            return Err(WebviewError::transport(
                "connection closed before request body".to_string(),
            ));
        }
        let Some(bytes) = chunk.get(..read) else {
            return Err(WebviewError::transport("invalid read size".to_string()));
        };
        buffer.extend_from_slice(bytes);
    }
    let body = buffer
        .get(body_start..total_len)
        .ok_or_else(|| WebviewError::transport("invalid request body slice".to_string()))?
        .to_vec();
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn gateway_request_kind(request: &HttpRequest) -> GatewayRequestKind {
    if header_value(&request.headers, "x-requested-with")
        .is_some_and(|value| value.eq_ignore_ascii_case("XMLHttpRequest"))
    {
        return GatewayRequestKind::Xhr;
    }
    if let Some(kind) = header_value(&request.headers, "x-rings-webview-kind") {
        return match kind {
            "fetch" => GatewayRequestKind::Fetch,
            "xhr" => GatewayRequestKind::Xhr,
            "navigation" => GatewayRequestKind::Navigation,
            _ => GatewayRequestKind::Subresource,
        };
    }
    if request.method != "GET" {
        return GatewayRequestKind::Fetch;
    }
    match header_value(&request.headers, "sec-fetch-dest") {
        Some("document") => GatewayRequestKind::Navigation,
        _ => GatewayRequestKind::Subresource,
    }
}

fn runtime_prefix(prefix: &GatewayPrefix) -> String {
    format!("{}-runtime/", prefix.as_str().trim_end_matches('/'))
}

fn write_http_response(stream: &mut TcpStream, response: &GatewayResponse) -> Result<()> {
    let reason = match response.status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason,
        response.body.len()
    );
    for header in &response.headers {
        head.push_str(header.name.as_str());
        head.push_str(": ");
        head.push_str(header.value.as_str());
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(response.body.as_slice()))
        .map_err(|error| WebviewError::transport(error.to_string()))
}

fn header_value<'a>(headers: &'a [GatewayHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.name_eq(name))
        .map(|header| header.value.as_str())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
