//! A page's `WebSocket`s: each connection runs on a thread of its own, talking to the page
//! only through two queues. What it receives goes to `Objects`' socket events, which the page
//! document delivers on the UI thread (`RunningPack::deliver_sockets`) as `open`, `message`,
//! `error` and `close` events, each a recorded step; what the page sends goes to the thread
//! as `Command`s.
//!
//! **Through the gate:** the URL is checked with the gate's own rules for the page (logged,
//! block-all, third-party default-deny), and a public page can't reach this machine or the
//! local network: IP literals are refused and names resolve only to public addresses, which
//! are the ones connected to (so a name can't be rebound between check and connect), as for
//! `fetch`. The handshake carries the page's `Origin` and the cookies `SameSite` allows for
//! the request (`Strict` and `Lax` ones only same-site). `Set-Cookie` on the handshake
//! response is ignored.
//!
//! **Bounded:** at most `MAX_OPEN` open sockets per page and `MAX_SOCKETS` in all, messages up
//! to `MAX_MESSAGE` bytes either way, and at most `MAX_PENDING` bytes received and not yet
//! delivered per page: past that, a connection stops reading until the page catches up.
//! When the page goes, its command queues go with it, and each thread closes its connection.
//!
//! Text messages only: a binary message is dropped (noted in the console).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use blitz_traits::net::{CookieScope, Url};
use tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tungstenite::{Message, WebSocket};

/// Open (connecting or open) sockets a page may have at once.
pub const MAX_OPEN: usize = 64;
/// Sockets a page may make in all (each closed one leaves a small object behind).
pub const MAX_SOCKETS: usize = 16 * 1024;
/// The largest message, in bytes, either way.
pub const MAX_MESSAGE: usize = 16 << 20;
/// Bytes a page may have received and not yet had delivered.
const MAX_PENDING: usize = 64 << 20;
/// How long connecting and the handshake may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How often an open connection checks for something to send while waiting to read.
const POLL: Duration = Duration::from_millis(20);

/// What happened on a connection, for the page (in order).
#[derive(Clone, Debug, PartialEq)]
pub enum SocketEvent {
    /// Connected; the protocol the server chose.
    Open { protocol: String },
    Message(String),
    /// The connection failed or broke (a `close` follows).
    Error,
    Close { code: u16, reason: String, clean: bool },
}

/// What the page asks of a connection.
pub enum Command {
    Send(String),
    Close { code: u16, reason: String },
}

/// A page's socket, as the host keeps it.
pub struct Socket {
    pub url: String,
    /// `readyState`: 0 connecting, 1 open, 2 closing, 3 closed.
    pub state: u8,
    pub protocol: String,
    /// To the connection's thread; None once closed (or for a socket that never connected).
    pub commands: Option<Sender<Command>>,
}

/// Events from every connection of one page, with the bytes waiting in them.
#[derive(Clone, Default)]
pub struct Inbox {
    pub events: Arc<Mutex<Vec<(u32, SocketEvent)>>>,
    pub pending: Arc<AtomicUsize>,
    pub waker: Arc<Mutex<Option<std::task::Waker>>>,
}

impl Inbox {
    pub fn push(&self, socket: u32, event: SocketEvent) {
        if let SocketEvent::Message(text) = &event {
            self.pending.fetch_add(text.len(), Ordering::Relaxed);
        }
        self.events.lock().unwrap().push((socket, event));
        if let Some(w) = self.waker.lock().unwrap().as_ref() {
            w.wake_by_ref();
        }
    }

    /// Events not yet delivered, oldest first.
    pub fn take(&self) -> Vec<(u32, SocketEvent)> {
        let events = std::mem::take(&mut *self.events.lock().unwrap());
        let bytes: usize = events.iter().map(|(_, e)| if let SocketEvent::Message(t) = e { t.len() } else { 0 }).sum();
        self.pending.fetch_sub(bytes.min(self.pending.load(Ordering::Relaxed)), Ordering::Relaxed);
        events
    }
}

/// The `http(s)` URL whose cookies a `ws(s)` URL gets.
pub fn cookie_url(url: &Url) -> Url {
    let mut http = url.clone();
    let _ = http.set_scheme(if url.scheme() == "wss" { "https" } else { "http" });
    http
}

/// The addresses to connect to for `host:port`. For a public page (`public_only`), only
/// public ones: an IP literal that isn't public is refused, and a name that resolves to no
/// public address fails.
pub fn addresses(host: &str, port: u16, public_only: bool) -> Result<Vec<SocketAddr>, String> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let found: Vec<SocketAddr> = (host, port).to_socket_addrs().map_err(|e| format!("can't resolve {host}: {e}"))?.collect();
    let kept: Vec<SocketAddr> = found.into_iter().filter(|a| !public_only || blitz_net::is_public_ip(a.ip())).collect();
    if kept.is_empty() {
        return Err(match public_only {
            true => "a public page can't reach private or local addresses".into(),
            false => format!("{host} has no addresses"),
        });
    }
    Ok(kept)
}

/// Connect socket `id` to `url` for `page` on a thread of its own, reporting to `inbox`. The
/// gate has already allowed it. Returns the queue for the page's commands.
pub fn connect(id: u32, url: Url, page: &Url, protocols: Vec<String>, inbox: Inbox) -> Sender<Command> {
    let (tx, rx) = std::sync::mpsc::channel();
    let origin = page.origin().ascii_serialization();
    let scope = if crate::gate::cross_site(page, &cookie_url(&url)) { CookieScope::CrossSite } else { CookieScope::SameSite };
    let cookies = crate::cookies::jar().header(&cookie_url(&url), scope);
    let public_only = crate::gate::is_public_page(page);
    let reporter = inbox.clone();
    let spawned = std::thread::Builder::new().name("websocket".into()).spawn(move || {
        let request = Handshake { url, origin, cookies, protocols };
        match open(&request, public_only) {
            Ok((socket, protocol)) => {
                inbox.push(id, SocketEvent::Open { protocol });
                run(id, socket, rx, &inbox);
            }
            Err(why) => {
                tracing::info!("WebSocket {} failed: {why}", request.url);
                inbox.push(id, SocketEvent::Error);
                inbox.push(id, SocketEvent::Close { code: 1006, reason: String::new(), clean: false });
            }
        }
    });
    if spawned.is_err() {
        reporter.push(id, SocketEvent::Error);
        reporter.push(id, SocketEvent::Close { code: 1006, reason: String::new(), clean: false });
    }
    tx
}

struct Handshake {
    url: Url,
    origin: String,
    cookies: Option<String>,
    protocols: Vec<String>,
}

/// A connection: plain TCP, or TLS for `wss:`.
enum Stream {
    Plain(TcpStream),
    Tls(native_tls::TlsStream<TcpStream>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => s.get_ref(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

fn config() -> WebSocketConfig {
    WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE))
}

/// Connect and shake hands. Returns the socket and the protocol the server chose.
fn open(request: &Handshake, public_only: bool) -> Result<(WebSocket<Stream>, String), String> {
    let url = &request.url;
    let host = url.host_str().ok_or("no host")?;
    let port = url.port_or_known_default().ok_or("no port")?;
    let mut tcp = None;
    for address in addresses(host, port, public_only)? {
        if let Ok(s) = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            tcp = Some(s);
            break;
        }
    }
    let tcp = tcp.ok_or_else(|| format!("can't connect to {host}:{port}"))?;
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
    let stream = match url.scheme() {
        "wss" => {
            let tls = native_tls::TlsConnector::new().map_err(|e| e.to_string())?;
            Stream::Tls(tls.connect(host, tcp).map_err(|e| e.to_string())?)
        }
        _ => Stream::Plain(tcp),
    };
    let mut builder = tungstenite::http::Request::builder()
        .uri(url.as_str())
        .header("Host", match url.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        })
        .header("Origin", &request.origin)
        .header("User-Agent", blitz_net::USER_AGENT)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key());
    if let Some(cookies) = &request.cookies {
        builder = builder.header("Cookie", cookies);
    }
    if !request.protocols.is_empty() {
        builder = builder.header("Sec-WebSocket-Protocol", request.protocols.join(", "));
    }
    let http = builder.body(()).map_err(|e| e.to_string())?;
    let (socket, response) = tungstenite::client::client_with_config(http, stream, Some(config())).map_err(|e| match e {
        tungstenite::HandshakeError::Failure(e) => e.to_string(),
        tungstenite::HandshakeError::Interrupted(_) => "the handshake timed out".into(),
    })?;
    let protocol = response.headers().get("sec-websocket-protocol").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    if !protocol.is_empty() && !request.protocols.contains(&protocol) {
        return Err(format!("the server chose a protocol the page didn't offer ({protocol})"));
    }
    // From now on, reads wait briefly so the page's sends get a turn.
    socket.get_ref().tcp().set_read_timeout(Some(POLL)).map_err(|e| e.to_string())?;
    Ok((socket, protocol))
}

/// An open connection: send what the page asks, deliver what arrives, until either side
/// closes it or the page goes (its command queue disconnects).
fn run(id: u32, mut socket: WebSocket<Stream>, commands: Receiver<Command>, inbox: &Inbox) {
    let mut closing = false;
    loop {
        match commands.try_recv() {
            Ok(Command::Send(text)) => {
                if socket.send(Message::text(text)).is_err() {
                    break;
                }
                continue;
            }
            Ok(Command::Close { code, reason }) => {
                closing = true;
                let _ = socket.close(Some(CloseFrame { code: code.into(), reason: reason.into() }));
            }
            Err(TryRecvError::Disconnected) => {
                // The page went: say goodbye, without waiting for an answer.
                let _ = socket.close(None);
                let _ = socket.flush();
                return;
            }
            Err(TryRecvError::Empty) => {}
        }
        // The page is behind: stop reading until it catches up (the server waits too).
        if inbox.pending.load(Ordering::Relaxed) > MAX_PENDING {
            std::thread::sleep(POLL);
            continue;
        }
        match socket.read() {
            Ok(Message::Text(text)) => inbox.push(id, SocketEvent::Message(text.to_string())),
            Ok(Message::Binary(_)) => tracing::info!("dropped a binary WebSocket message: only text is supported"),
            Ok(Message::Close(frame)) => {
                let _ = socket.flush();
                let (code, reason) = frame.map(|f| (u16::from(f.code), f.reason.to_string())).unwrap_or((1005, String::new()));
                inbox.push(id, SocketEvent::Close { code, reason, clean: true });
                return;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::ConnectionClosed) if closing => {
                inbox.push(id, SocketEvent::Close { code: 1005, reason: String::new(), clean: true });
                return;
            }
            Err(_) => break,
        }
    }
    inbox.push(id, SocketEvent::Error);
    inbox.push(id, SocketEvent::Close { code: 1006, reason: String::new(), clean: false });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A public page's socket never reaches this machine or the local network.
    #[test]
    fn public_pages_get_public_addresses_only() {
        assert!(addresses("127.0.0.1", 80, true).is_err());
        assert!(addresses("[::1]", 80, true).is_err());
        assert!(addresses("localhost", 80, true).is_err());
        assert!(addresses("10.0.0.1", 80, true).is_err());
        assert_eq!(addresses("127.0.0.1", 80, false).unwrap(), ["127.0.0.1:80".parse().unwrap()]);
        assert_eq!(cookie_url(&Url::parse("wss://a.test/x").unwrap()).as_str(), "https://a.test/x");
    }
}
