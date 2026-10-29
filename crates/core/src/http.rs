//! A bounded HTTP/1.1 transport for the control API: one request
//! per connection, small JSON bodies (and one large upload, the host update),
//! `Connection: close`. No framework, no TLS (Tailscale is the transport),
//! no keep-alive.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Larger bodies are refused; nothing Latch sends comes close, except the
/// host executable on [`crate::api::UPDATE_PATH`], which has its own limit.
const MAX_BODY: usize = 64 * 1024;
const MAX_HEADER: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 16;
/// Time to read headers and a small JSON body, and to write the reply.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// Time to read `latch-host.exe` after the headers. Tailscale plus a
/// 10–30 MB body does not fit in [`IDLE_TIMEOUT`].
const UPDATE_BODY_TIMEOUT: Duration = Duration::from_secs(120);

/// macOS reports `SO_RCVTIMEO` / `SO_SNDTIMEO` as EAGAIN (os error 35,
/// `ErrorKind::WouldBlock`) rather than `TimedOut`. Users then see
/// "Resource temporarily unavailable". Treat that as a timeout, and a
/// broken pipe as the peer going away.
pub(crate) fn io_err(e: std::io::Error) -> anyhow::Error {
    if is_timeout(&e) {
        anyhow!("timed out")
    } else if is_closed(&e) {
        anyhow!("connection closed")
    } else {
        e.into()
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
        // EAGAIN: Linux 11, macOS 35. ETIMEDOUT: macOS 60, Linux 110.
        // WSAEWOULDBLOCK 10035, WSAETIMEDOUT 10060.
        || matches!(e.raw_os_error(), Some(11 | 35 | 60 | 110 | 10035 | 10060))
}

fn is_closed(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
    ) || matches!(e.raw_os_error(), Some(32 | 54 | 104 | 10053 | 10054))
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Header names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body decoded as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).context("body is not the expected JSON")
    }
}

/// How large a body `path` may carry.
fn max_body(path: &str) -> usize {
    if path == crate::api::UPDATE_PATH {
        crate::api::UPDATE_MAX_BYTES
    } else {
        MAX_BODY
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
    /// For `101 Switching Protocols`: who takes the connection over.
    pub takeover: Option<Takeover>,
}

/// The protocol a connection switches to after its request, and the code
/// that runs it. It gets the socket once `101 Switching Protocols` is
/// written and keeps the connection's worker thread until it returns.
#[derive(Clone)]
pub struct Takeover {
    pub protocol: String,
    run: std::sync::Arc<std::sync::Mutex<Option<TakeoverFn>>>,
}

type TakeoverFn = Box<dyn FnOnce(TcpStream) + Send>;

impl std::fmt::Debug for Takeover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Takeover")
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Takeover {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.run, &other.run)
    }
}

impl Eq for Takeover {}

impl Response {
    pub fn json<T: serde::Serialize>(status: u16, value: &T) -> Self {
        Self {
            status,
            body: serde_json::to_string(value).unwrap_or_else(|_| "{}".into()),
            takeover: None,
        }
    }

    /// `101 Switching Protocols` to `protocol`; `run` then owns the socket.
    pub fn switching(protocol: &str, run: impl FnOnce(TcpStream) + Send + 'static) -> Self {
        Self {
            status: 101,
            body: String::new(),
            takeover: Some(Takeover {
                protocol: protocol.to_string(),
                run: std::sync::Arc::new(std::sync::Mutex::new(Some(Box::new(run)))),
            }),
        }
    }

    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(&self.body)
            .with_context(|| format!("HTTP {}: {}", self.status, self.body))
    }
}

/// Read one request from `stream`. `None` for an empty connection (a port
/// scan, a health probe) so the caller can drop it quietly.
pub fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut remaining = MAX_HEADER;
    let Some(line) = read_line(&mut reader, &mut remaining)? else {
        return Ok(None);
    };
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let version = parts.next().unwrap_or("");
    validate_target(&method, &path)?;
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") || parts.next().is_some() {
        bail!("bad request line");
    }
    let headers = read_headers(&mut reader, &mut remaining)?;
    if header(&headers, "transfer-encoding").is_some() {
        bail!("request transfer-encoding is not supported");
    }
    let content_length = content_length(&headers)?.unwrap_or(0);
    if content_length > max_body(&path) as u64 {
        bail!("body too large ({content_length} bytes)");
    }
    if content_length > MAX_BODY as u64 {
        reader
            .get_mut()
            .set_read_timeout(Some(UPDATE_BODY_TIMEOUT))
            .map_err(io_err)?;
    }
    // Grow only as bytes arrive; an untrusted size must not reserve the
    // entire update allowance before the sender transmits its body.
    let mut body = Vec::new();
    let received = reader
        .take(content_length)
        .read_to_end(&mut body)
        .map_err(io_err)?;
    if received as u64 != content_length {
        bail!("connection closed inside the body");
    }
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

/// A reply with an arbitrary body: a file, a script.
pub fn write_bytes(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    validate_header("Content-Type", content_type)?;
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes()).map_err(io_err)?;
    stream.write_all(body).map_err(io_err)?;
    stream.flush().map_err(io_err)?;
    Ok(())
}

fn reason(status: u16) -> &'static str {
    match status {
        101 => "Switching Protocols",
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Status",
    }
}

pub fn write_response(stream: &mut TcpStream, resp: &Response) -> Result<()> {
    write_bytes(
        stream,
        resp.status,
        "application/json",
        resp.body.as_bytes(),
    )
}

/// [`serve_with_peer_check`] for everyone, for the tests.
#[cfg(test)]
fn serve<F>(listener: TcpListener, handler: F)
where
    F: Fn(SocketAddr, &Request) -> Response + Send + Sync + 'static,
{
    serve_with_peer_check(listener, |_| true, handler);
}

/// Accept forever, with a bounded number of active connections. `authorize`
/// sees the peer before its headers are read or a body is allocated; it
/// runs in the connection's worker, so it may do I/O. `handler` sees the
/// peer too, so it can decide per route.
pub fn serve_with_peer_check<A, F>(listener: TcpListener, authorize: A, handler: F)
where
    A: Fn(SocketAddr) -> bool + Send + Sync + 'static,
    F: Fn(SocketAddr, &Request) -> Response + Send + Sync + 'static,
{
    let handler = std::sync::Arc::new(handler);
    let authorize = std::sync::Arc::new(authorize);
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        let Some(slot) = ConnectionSlot::acquire(&active) else {
            // Closing immediately also bounds work on the accept thread.
            continue;
        };
        let handler = handler.clone();
        let authorize = authorize.clone();
        std::thread::spawn(move || {
            let _slot = slot;
            let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
            let _ = stream.set_write_timeout(Some(IDLE_TIMEOUT));
            let peer = match stream.peer_addr() {
                Ok(p) => p,
                Err(_) => return,
            };
            if !authorize(peer) {
                let _ = write_response(
                    &mut stream,
                    &Response::json(403, &crate::api::Ack::err("peer is not authorized")),
                );
                return;
            }
            let resp = match read_request(&mut stream) {
                Ok(Some(req)) => handler(peer, &req),
                Ok(None) => return,
                Err(e) => Response::json(
                    400,
                    &serde_json::json!({ "ok": false, "error": e.to_string() }),
                ),
            };
            if let (101, Some(takeover)) = (resp.status, &resp.takeover) {
                switch(stream, takeover);
                return;
            }
            let _ = write_response(&mut stream, &resp);
        });
    }
}

/// Writes `101 Switching Protocols` and hands the socket over, with the
/// HTTP timeouts lifted: the new protocol keeps its own.
fn switch(mut stream: TcpStream, takeover: &Takeover) {
    if validate_header("Upgrade", &takeover.protocol).is_err() {
        return;
    }
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: {}\r\n\r\n",
        takeover.protocol
    );
    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(None);
    let run = takeover.run.lock().ok().and_then(|mut run| run.take());
    if let Some(run) = run {
        run(stream);
    }
}

/// Asks the server at `addr` to switch the connection to `protocol` with a
/// POST of `body` (JSON) to `path`. `Ok(Ok(socket))` once it has; the socket
/// holds nothing past the server's `101` head. `Ok(Err(reply))` when the
/// server answered with anything else.
pub fn upgrade(
    addr: SocketAddr,
    path: &str,
    protocol: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<std::result::Result<TcpStream, Response>> {
    validate_target("POST", path)?;
    validate_header("Upgrade", protocol)?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(io_err)?;
    stream.set_read_timeout(Some(timeout)).map_err(io_err)?;
    stream.set_write_timeout(Some(timeout)).map_err(io_err)?;
    let _ = stream.set_nodelay(true);
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: {protocol}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).map_err(io_err)?;
    stream.write_all(body).map_err(io_err)?;
    stream.flush().map_err(io_err)?;
    // Byte by byte up to the blank line, so nothing the new protocol sends
    // right behind the head is swallowed by a buffer.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEADER {
            bail!("reply head too large");
        }
        match stream.read(&mut byte).map_err(io_err)? {
            0 => bail!("connection closed before the reply"),
            _ => head.push(byte[0]),
        }
    }
    let mut reader = BufReader::new(&head[..]);
    let mut remaining = MAX_HEADER;
    let line = read_line(&mut reader, &mut remaining)?.context("no HTTP status")?;
    let mut parts = line.splitn(3, ' ');
    if !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
        bail!("invalid HTTP version");
    }
    let status: u16 = parts
        .next()
        .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_digit()))
        .context("invalid HTTP status")?
        .parse()?;
    let headers = read_headers(&mut reader, &mut remaining)?;
    if status == 101 {
        if !header(&headers, "upgrade").is_some_and(|u| u.eq_ignore_ascii_case(protocol)) {
            bail!("the server switched to another protocol");
        }
        return Ok(Ok(stream));
    }
    let mut body = Vec::new();
    let mut rest = BufReader::new(&mut stream);
    read_response_body(&mut rest, &headers, &mut body, MAX_BODY as u64, false)?;
    Ok(Err(Response {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
        takeover: None,
    }))
}

struct ConnectionSlot(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl ConnectionSlot {
    fn acquire(active: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Option<Self> {
        use std::sync::atomic::Ordering::Relaxed;
        // A compare-and-swap loop rather than `fetch_update`, which Rust 1.99
        // renamed `try_update`: this builds on both.
        let mut n = active.load(Relaxed);
        loop {
            if n >= MAX_CONNECTIONS {
                return None;
            }
            match active.compare_exchange_weak(n, n + 1, Relaxed, Relaxed) {
                Ok(_) => return Some(Self(active.clone())),
                Err(seen) => n = seen,
            }
        }
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A connection to the first of `addr`'s addresses that accepts one, each
/// given `timeout`. A name can resolve to an IPv6 address first that this
/// network has no route to, ahead of an IPv4 one that works.
pub(crate) fn connect(addr: impl ToSocketAddrs, timeout: Duration) -> Result<TcpStream> {
    let mut last = None;
    for a in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.map(io_err).unwrap_or_else(|| anyhow!("no address")))
}

/// One request, one reply, within `timeout` for connect and for the read.
pub fn request(
    addr: impl ToSocketAddrs,
    method: &str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<Response> {
    request_with(
        addr,
        method,
        path,
        &[("Content-Type", "application/json")],
        body.unwrap_or("").as_bytes(),
        timeout,
    )
}

/// Like [`request`], with the caller's own headers and a binary body.
pub fn request_with(
    addr: impl ToSocketAddrs,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> Result<Response> {
    let mut stream = connect(addr, timeout)?;
    stream.set_read_timeout(Some(timeout)).map_err(io_err)?;
    stream.set_write_timeout(Some(timeout)).map_err(io_err)?;
    let host = stream.peer_addr().map_err(io_err)?.to_string();
    exchange_with(&mut stream, method, path, &host, headers, body)
}

/// One HTTP/1.1 request with a JSON body and its reply over any stream (TCP,
/// or TLS on top of it). The body is read to `Content-Length`, or to the end
/// of the stream.
pub fn exchange<S: Read + Write>(
    stream: &mut S,
    method: &str,
    path: &str,
    host: &str,
    body: &str,
) -> Result<Response> {
    exchange_with(
        stream,
        method,
        path,
        host,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    )
}

pub fn exchange_with<S: Read + Write>(
    stream: &mut S,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Response> {
    validate_target(method, path)?;
    validate_header("Host", host)?;
    if host.is_empty() || host.bytes().any(|b| b.is_ascii_whitespace()) {
        bail!("invalid host");
    }
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n");
    for (k, v) in headers {
        validate_header(k, v)?;
        if ["host", "content-length", "transfer-encoding", "connection"]
            .iter()
            .any(|reserved| k.eq_ignore_ascii_case(reserved))
        {
            bail!("reserved request header {k}");
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    stream.write_all(head.as_bytes()).map_err(io_err)?;
    stream.write_all(body).map_err(io_err)?;
    stream.flush().map_err(io_err)?;

    let mut reader = BufReader::new(stream);
    let (status, headers) = read_response_head(&mut reader)?;
    let mut body = Vec::new();
    if method != "HEAD" && !matches!(status, 204 | 304) {
        read_response_body(&mut reader, &headers, &mut body, MAX_BODY as u64, true)?;
    }
    Ok(Response {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
        takeover: None,
    })
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

pub(crate) fn validate_header(name: &str, value: &str) -> Result<()> {
    if !token(name) || value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
        bail!("invalid HTTP header");
    }
    Ok(())
}

fn validate_target(method: &str, path: &str) -> Result<()> {
    if !token(method)
        || !path.starts_with('/')
        || path
            .bytes()
            .any(|b| !b.is_ascii() || b.is_ascii_whitespace() || b.is_ascii_control())
    {
        bail!("invalid HTTP method or request target");
    }
    Ok(())
}

/// Read only within the remaining metadata budget, including before a
/// newline arrives. `BufRead::read_line` alone can allocate without limit.
fn read_line<R: BufRead>(reader: &mut R, remaining: &mut usize) -> Result<Option<String>> {
    let mut line = String::new();
    let n = reader
        .take(*remaining as u64 + 1)
        .read_line(&mut line)
        .map_err(io_err)?;
    if n > *remaining {
        bail!("HTTP metadata too large");
    }
    *remaining -= n;
    if n == 0 {
        return Ok(None);
    }
    if !line.ends_with("\r\n") {
        bail!("incomplete HTTP line");
    }
    line.truncate(line.len() - 2);
    Ok(Some(line))
}

fn read_headers<R: BufRead>(r: &mut R, remaining: &mut usize) -> Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    loop {
        let line = read_line(r, remaining)?.context("connection closed inside headers")?;
        if line.is_empty() {
            return Ok(headers);
        }
        let (name, value) = line.split_once(':').context("invalid HTTP header")?;
        let value = value.trim_matches([' ', '\t']);
        validate_header(name, value)?;
        let name = name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "content-length" | "transfer-encoding" | "host" | "location"
        ) && header(&headers, &name).is_some()
        {
            bail!("duplicate {name} header");
        }
        headers.push((name, value.to_string()));
    }
}

pub(crate) fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn content_length(headers: &[(String, String)]) -> Result<Option<u64>> {
    header(headers, "content-length")
        .map(|value| {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                bail!("invalid content-length");
            }
            value.parse().context("invalid content-length")
        })
        .transpose()
}

pub(crate) fn read_response_head<R: BufRead>(r: &mut R) -> Result<(u16, Vec<(String, String)>)> {
    let mut remaining = MAX_HEADER;
    // Bound interim replies with the same metadata budget.
    loop {
        let line = read_line(r, &mut remaining)?.context("connection closed before HTTP status")?;
        let mut parts = line.splitn(3, ' ');
        if !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
            bail!("invalid HTTP version");
        }
        let code = parts.next().context("missing HTTP status")?;
        if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
            bail!("invalid HTTP status");
        }
        let status: u16 = code.parse()?;
        if !(100..=599).contains(&status) || status == 101 {
            bail!("unsupported HTTP status");
        }
        let headers = read_headers(r, &mut remaining)?;
        if status >= 200 {
            return Ok((status, headers));
        }
    }
}

pub(crate) fn read_response_body<R: BufRead>(
    r: &mut R,
    headers: &[(String, String)],
    sink: &mut dyn Write,
    limit: u64,
    allow_tls_eof: bool,
) -> Result<()> {
    let length = content_length(headers)?;
    if let Some(encoding) = header(headers, "transfer-encoding") {
        if length.is_some() || !encoding.eq_ignore_ascii_case("chunked") {
            bail!("ambiguous or unsupported HTTP body framing");
        }
        let mut total = 0u64;
        let mut metadata = MAX_HEADER;
        loop {
            let line =
                read_line(r, &mut metadata)?.context("connection closed inside a chunked body")?;
            let size_hex = line.split(';').next().unwrap_or("");
            if size_hex.is_empty() || !size_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("invalid HTTP chunk size");
            }
            let size = u64::from_str_radix(size_hex, 16).context("invalid HTTP chunk size")?;
            total = total
                .checked_add(size)
                .filter(|n| *n <= limit)
                .context("reply too large")?;
            if size == 0 {
                read_headers(r, &mut metadata)?;
                return Ok(());
            }
            copy_exact(r, sink, size)?;
            let mut ending = [0; 2];
            r.read_exact(&mut ending).map_err(io_err)?;
            if ending != *b"\r\n" {
                bail!("invalid HTTP chunk terminator");
            }
            // Chunk headers need their own bounded budget, but valid large
            // downloads can contain more than 16 KiB of chunk sizes overall.
            metadata = MAX_HEADER;
        }
    }
    if let Some(length) = length {
        if length > limit {
            bail!("reply too large ({length} bytes)");
        }
        return copy_exact(r, sink, length);
    }
    let mut remaining = limit;
    let mut buffer = [0; 8192];
    loop {
        // Read one beyond the limit to distinguish a complete body from
        // silent truncation, without writing that extra byte to the sink.
        let available = buffer.len().min(remaining.saturating_add(1) as usize);
        let n = match r.read(&mut buffer[..available]) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if allow_tls_eof && e.kind() == ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(io_err(e)),
        };
        if n as u64 > remaining {
            bail!("reply too large");
        }
        sink.write_all(&buffer[..n]).map_err(io_err)?;
        remaining -= n as u64;
    }
}

fn copy_exact<R: Read>(r: &mut R, sink: &mut dyn Write, length: u64) -> Result<()> {
    let copied = std::io::copy(&mut r.take(length), sink).map_err(io_err)?;
    if copied != length {
        bail!("connection closed after {copied} of {length} bytes");
    }
    Ok(())
}

/// GET `path` and decode the JSON reply.
pub fn get_json<T: serde::de::DeserializeOwned>(
    addr: impl ToSocketAddrs,
    path: &str,
    timeout: Duration,
) -> Result<T> {
    let r = request(addr, "GET", path, None, timeout)?;
    if r.status != 200 {
        bail!("HTTP {}: {}", r.status, r.body);
    }
    r.parse()
}

/// POST `value` as JSON to `path` and decode the reply.
pub fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
    addr: impl ToSocketAddrs,
    path: &str,
    value: &B,
    timeout: Duration,
) -> Result<T> {
    let body = serde_json::to_string(value)?;
    let r = request(addr, "POST", path, Some(&body), timeout)?;
    if r.status != 200 {
        bail!("HTTP {}: {}", r.status, r.body);
    }
    r.parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    #[test]
    fn client_and_server_agree_over_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            serve(listener, |peer, req| {
                assert!(peer.ip().is_loopback());
                match (req.method.as_str(), req.path.as_str()) {
                    ("GET", "/v1/status") => {
                        Response::json(200, &serde_json::json!({ "name": "PC" }))
                    }
                    ("POST", "/v1/echo") => Response {
                        status: 200,
                        body: String::from_utf8_lossy(&req.body).into_owned(),
                        takeover: None,
                    },
                    _ => Response::json(404, &serde_json::json!({ "ok": false })),
                }
            });
        });
        let t = Duration::from_secs(2);
        let v: serde_json::Value = get_json(addr, "/v1/status", t).unwrap();
        assert_eq!(v["name"], "PC");
        let echoed: serde_json::Value =
            post_json(addr, "/v1/echo", &serde_json::json!({"pin": "1234"}), t).unwrap();
        assert_eq!(echoed["pin"], "1234");
        let err = get_json::<serde_json::Value>(addr, "/nope", t).unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[test]
    fn headers_and_binary_bodies_reach_the_handler_and_big_ones_only_on_update() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            serve(listener, |_, req| {
                Response::json(
                    200,
                    &serde_json::json!({
                        "len": req.body.len(),
                        "ver": req.header("X-Latch-Version"),
                        "first": req.body.first(),
                    }),
                )
            });
        });
        let t = Duration::from_secs(5);
        let body = vec![0x4du8; 100 * 1024];
        let r = request_with(
            addr,
            "POST",
            crate::api::UPDATE_PATH,
            &[("X-Latch-Version", "3.1.0")],
            &body,
            t,
        )
        .unwrap();
        let v: serde_json::Value = r.parse().unwrap();
        assert_eq!(v["len"], 100 * 1024);
        assert_eq!(v["ver"], "3.1.0");
        assert_eq!(v["first"], 0x4d);
        // The same body on an ordinary route is over the limit. The server
        // closes without reading it, so the client may see HTTP 400 or a
        // closed connection.
        match request_with(addr, "POST", "/v1/pin", &[], &body, t) {
            Ok(r) => assert_eq!(r.status, 400, "{}", r.body),
            Err(e) => {
                let s = e.to_string();
                assert!(
                    s.contains("connection closed") || s.contains("timed out") || s.contains("400"),
                    "{e:#}"
                );
            }
        }
    }

    #[test]
    fn an_address_that_refuses_is_skipped_for_the_next_one() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap();
        // Port 0 can never be connected to. A port freed by dropping a
        // listener could be: the tests run in parallel, and another one
        // binds the next free port, often that very one.
        let closed: SocketAddr = "127.0.0.1:0".parse().unwrap();
        std::thread::spawn(move || serve(listener, |_, _| Response::json(200, &Ack::ok())));
        let t = Duration::from_secs(2);
        let r = request(&[closed, open][..], "GET", "/", None, t).unwrap();
        assert_eq!(r.status, 200, "{}", r.body);
        let e = request(closed, "GET", "/", None, t).unwrap_err();
        assert!(!e.to_string().is_empty());
        assert!(connect(&[][..] as &[SocketAddr], t).is_err());
    }

    #[test]
    fn garbage_is_refused_not_panicked() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || serve(listener, |_, _| Response::json(200, &Ack::ok())));
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"not http at all\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    }

    use crate::api::Ack;

    #[test]
    fn mac_socket_timeouts_and_broken_pipes_read_as_english() {
        let e = io_err(std::io::Error::new(
            ErrorKind::WouldBlock,
            "Resource temporarily unavailable",
        ));
        assert_eq!(e.to_string(), "timed out");
        let e = io_err(std::io::Error::from_raw_os_error(35));
        assert_eq!(e.to_string(), "timed out", "{e:#}");
        let e = io_err(std::io::Error::new(ErrorKind::TimedOut, "timed out"));
        assert_eq!(e.to_string(), "timed out");
        let e = io_err(std::io::Error::new(ErrorKind::BrokenPipe, "Broken pipe"));
        assert_eq!(e.to_string(), "connection closed");
        let e = io_err(std::io::Error::from_raw_os_error(32));
        assert_eq!(e.to_string(), "connection closed", "{e:#}");
        let e = io_err(std::io::Error::new(
            ErrorKind::ConnectionReset,
            "Connection reset by peer",
        ));
        assert_eq!(e.to_string(), "connection closed");
        let e = io_err(std::io::Error::other("disk full"));
        assert!(e.to_string().contains("disk full"), "{e}");
    }

    #[test]
    fn an_upgraded_connection_carries_the_new_protocol_both_ways() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            serve(listener, |_, req| {
                if req.path != "/v1/stream" {
                    return Response::json(404, &serde_json::json!({ "ok": false }));
                }
                let body = req.body.clone();
                Response::switching("test-proto/1", move |mut s| {
                    // Sent at once: must not be lost in the client's reader.
                    s.write_all(&body).unwrap();
                    let mut b = [0u8; 4];
                    s.read_exact(&mut b).unwrap();
                    s.write_all(&b).unwrap();
                })
            })
        });
        let mut s = upgrade(
            addr,
            "/v1/stream",
            "test-proto/1",
            b"hello",
            Duration::from_secs(2),
        )
        .unwrap()
        .unwrap();
        let mut got = [0u8; 5];
        s.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");
        s.write_all(b"ping").unwrap();
        let mut back = [0u8; 4];
        s.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"ping");
        let refused = upgrade(
            addr,
            "/v1/other",
            "test-proto/1",
            b"{}",
            Duration::from_secs(2),
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(refused.status, 404);
    }
}
