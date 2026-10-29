//! AI access: a read-only MCP server so an agent can read and debug the browser.
//!
//! - Off at every launch; turned on from the menu, and visibly on (a bar with "Turn off").
//! - Listens on 127.0.0.1 only, and every request needs the token, which is new each time
//!   access is turned on. The address and token are written to `ai-access.json` (readable
//!   only by the user) next to settings.json, for MCP clients to use.
//! - Requests from web pages are refused: they carry an `Origin` header, MCP clients don't.
//! - Read-only: tabs, a page's outline, console and errors, the trace, and requests. Form
//!   field values, cookies, headers and URL query values and fragments are never returned.
//! - Every call appears in the trace (pack "ai").
//!
//! The server thread only speaks HTTP and JSON-RPC; each tool call is answered on the UI
//! thread (the `AiAccess` component), where the tabs and their pages live.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use blitz_dom::{BaseDocument, NodeId};
use blitz_traits::net::Url;
use dioxus_native::prelude::*;
use serde_json::{Value, json};

use crate::gate::Gate;
use crate::history::HistoryNav;
use crate::host::{self, TraceEntry, TraceKind};
use crate::settings;
use crate::tab::{Tab, TabId, TabStoreExt, TabStoreImplExt};

pub const PORT: u16 = 7807;

/// The most a page outline returns, in characters.
const MAX_OUTLINE: usize = 60_000;

/// A tool call, sent from the server thread to the UI thread.
struct Query {
    tool: String,
    args: Value,
    reply: std::sync::mpsc::Sender<Result<Value, String>>,
}

struct Running {
    stop: Arc<AtomicBool>,
}

static QUERIES: OnceLock<tokio::sync::mpsc::UnboundedSender<Query>> = OnceLock::new();
static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
/// Why the server couldn't start, shown in the AI access bar.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

pub fn is_on() -> bool {
    RUNNING.lock().unwrap().is_some()
}

pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap().clone()
}

fn connection_file() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "mattsplat", "browser-test").map(|d| d.config_dir().join("ai-access.json"))
}

pub fn url() -> String {
    format!("http://127.0.0.1:{PORT}/mcp")
}

/// Start the server with a new token. Does nothing if it's already on.
pub fn turn_on() {
    let mut running = RUNNING.lock().unwrap();
    if running.is_some() {
        return;
    }
    let result = (|| -> anyhow::Result<Running> {
        let token = new_token()?;
        let listener = TcpListener::bind(("127.0.0.1", PORT))?;
        listener.set_nonblocking(true)?;
        write_connection_file(&token)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::Builder::new().name("ai-access".into()).spawn(move || serve(listener, &token, &stopped))?;
        Ok(Running { stop })
    })();
    match result {
        Ok(r) => {
            *running = Some(r);
            *LAST_ERROR.lock().unwrap() = None;
        }
        Err(e) => *LAST_ERROR.lock().unwrap() = Some(format!("AI access couldn't start: {e:#}")),
    }
}

/// Stop the server and delete the token; connected clients are cut off.
pub fn turn_off() {
    if let Some(r) = RUNNING.lock().unwrap().take() {
        r.stop.store(true, Ordering::Relaxed);
    }
    if let Some(path) = connection_file() {
        let _ = std::fs::remove_file(path);
    }
    *LAST_ERROR.lock().unwrap() = None;
}

fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_connection_file(token: &str) -> anyhow::Result<()> {
    let path = connection_file().ok_or_else(|| anyhow::anyhow!("no settings folder"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_string_pretty(&json!({ "url": url(), "token": token }))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&path)?.write_all(body.as_bytes())?;
    Ok(())
}

// ---- The server thread: HTTP and JSON-RPC (MCP's Streamable HTTP transport, JSON replies).

fn serve(listener: TcpListener, token: &str, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = handle_connection(stream, token);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn read_request(stream: &TcpStream) -> anyhow::Result<HttpRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        anyhow::ensure!(headers.len() < 100, "too many headers");
    }
    let mut request = HttpRequest { method, path, headers, body: Vec::new() };
    let length: usize = request.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    anyhow::ensure!(length <= 1 << 20, "request too large");
    request.body.resize(length, 0);
    reader.read_exact(&mut request.body)?;
    Ok(request)
}

fn respond(mut stream: &TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let content_type = if body.is_empty() { "" } else { "Content-Type: application/json\r\n" };
    write!(stream, "HTTP/1.1 {status}\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn handle_connection(stream: TcpStream, token: &str) -> anyhow::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let request = read_request(&stream)?;
    let (status, body) = route(&request, token);
    respond(&stream, status, &body)?;
    Ok(())
}

/// Checks, then the JSON-RPC message. Returns the HTTP status and body.
fn route(request: &HttpRequest, token: &str) -> (&'static str, String) {
    // Web pages send an Origin header; refusing it keeps any page (even one that rebinds
    // its DNS name to 127.0.0.1) from reaching the server.
    if request.header("origin").is_some() {
        return ("403 Forbidden", String::new());
    }
    let host_ok = request.header("host").is_some_and(|h| h == format!("127.0.0.1:{PORT}") || h == format!("localhost:{PORT}"));
    if !host_ok {
        return ("403 Forbidden", String::new());
    }
    let presented = request.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or_default();
    if !same(presented.as_bytes(), token.as_bytes()) {
        return ("401 Unauthorized", String::new());
    }
    if request.path != "/mcp" {
        return ("404 Not Found", String::new());
    }
    if request.method != "POST" {
        return ("405 Method Not Allowed", String::new());
    }
    let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
        return ("400 Bad Request", rpc_error(Value::Null, -32700, "parse error").to_string());
    };
    match rpc(&message) {
        Some(reply) => ("200 OK", reply.to_string()),
        // A notification: nothing to say.
        None => ("202 Accepted", String::new()),
    }
}

/// Compare without stopping at the first difference, so timing doesn't reveal the token.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn rpc(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or_default();
    let params = &message["params"];
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "browser-test", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Read-only access to the browser: its tabs, page outlines, console and errors, \
                the pack trace, and network requests. Form values, cookies, headers and URL query values are hidden.",
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default().to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match ask_ui(name, args) {
                Ok(value) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }] }),
                Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
            }
        }
        _ => return Some(rpc_error(id, -32601, &format!("unknown method {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn tools() -> Value {
    let tab = json!({ "type": "integer", "description": "Tab id from list_tabs; the active tab if omitted." });
    let limit = json!({ "type": "integer", "description": "Most entries to return, newest first (default 100, at most 500)." });
    json!([
        { "name": "list_tabs", "description": "The open tabs: id, URL, title, and which is active.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "read_page", "description": "An outline of a tab's page: elements with their id, classes and key attributes, and their text. Form field values are never included.",
          "inputSchema": { "type": "object", "properties": { "tab": tab } } },
        { "name": "console", "description": "What a tab's site logged (console.log, Dart print) and the errors it hit.",
          "inputSchema": { "type": "object", "properties": { "tab": tab, "limit": limit, "errors_only": { "type": "boolean" } } } },
        { "name": "trace", "description": "The boundary trace for a tab's site: every call its language pack (Dart or JS) made through the browser, with results and timings, plus loads and errors.",
          "inputSchema": { "type": "object", "properties": { "tab": tab, "limit": limit } } },
        { "name": "requests", "description": "Network requests a tab's site made: allowed or blocked, why, and the third-party site. Query values are hidden.",
          "inputSchema": { "type": "object", "properties": { "tab": tab, "limit": limit } } },
    ])
}

fn ask_ui(tool: String, args: Value) -> Result<Value, String> {
    let queries = QUERIES.get().ok_or("the browser window isn't ready")?;
    let (reply, answer) = std::sync::mpsc::channel();
    queries.send(Query { tool, args, reply }).map_err(|_| "the browser window has closed")?;
    answer.recv_timeout(Duration::from_secs(10)).map_err(|_| "the browser didn't answer in time".to_string())?
}

// ---- The UI thread: answering tool calls, and the bar that shows access is on.

/// Answers AI tool calls, and shows a bar while access is on.
#[component]
pub fn AiAccess(tabs: Store<Vec<Tab>>, active_tab_id: Signal<TabId>) -> Element {
    let gate = use_context::<Arc<Gate>>();
    let mut on = use_signal(is_on);
    let mut error = use_signal(last_error);

    use_hook(move || {
        let (sender, mut queries) = tokio::sync::mpsc::unbounded_channel::<Query>();
        let _ = QUERIES.set(sender);
        spawn(async move {
            while let Some(q) = queries.recv().await {
                let _ = q.reply.send(answer(&q.tool, &q.args, tabs, active_tab_id(), &gate));
            }
        });
        // The menu turns access on and off; follow it.
        spawn(async move {
            loop {
                if on() != is_on() {
                    on.set(is_on());
                }
                if error() != last_error() {
                    error.set(last_error());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
    });

    if let Some(message) = error() {
        return rsx!(
            div { class: "prompt-bar",
                span { class: "prompt-text", "{message}" }
                div { class: "prompt-button", onclick: move |_| turn_off(), "Dismiss" }
            }
        );
    }
    if !on() {
        return rsx!();
    }
    let file = connection_file().map(|p| p.display().to_string()).unwrap_or_default();
    rsx!(
        div { class: "prompt-bar ai-bar",
            span { class: "prompt-text", "AI access is on (read-only) at {url()}. Token in {file}" }
            div { class: "prompt-button", onclick: move |_| turn_off(), "Turn off" }
        }
    )
}

fn answer(tool: &str, args: &Value, tabs: Store<Vec<Tab>>, active: TabId, gate: &Gate) -> Result<Value, String> {
    let limit = args["limit"].as_u64().unwrap_or(100).min(500) as usize;
    if tool == "list_tabs" {
        trace_call(tool, String::new(), "");
        let list: Vec<Value> = tabs
            .iter()
            .map(|tab| {
                let url = tab.nav_history().current_url().read().url.to_string();
                json!({ "id": tab.tab_id(), "url": redact(&url), "title": tab.title().read().clone(), "active": tab.tab_id() == active })
            })
            .collect();
        return Ok(json!(list));
    }
    let wanted = args["tab"].as_u64().unwrap_or(active);
    let tab = tabs.iter().find(|t| t.tab_id() == wanted).ok_or_else(|| format!("no tab {wanted}"))?;
    let page_url = tab.nav_history().current_url().read().url.clone();
    let origin = settings::origin_of(&page_url);
    trace_call(tool, format!("tab {wanted}"), page_url.as_str());
    let same_site = |page: &str| Url::parse(page).map(|u| settings::origin_of(&u) == origin).unwrap_or(false);
    match tool {
        "read_page" => {
            let page = tab.page().read().clone().and_then(|p| p.upgrade()).ok_or("the tab has no page loaded")?;
            let outline = outline(&page.borrow());
            Ok(json!({ "url": redact(page_url.as_str()), "outline": outline }))
        }
        "console" => {
            let errors_only = args["errors_only"].as_bool().unwrap_or(false);
            let entries: Vec<Value> = host::trace()
                .into_iter()
                .filter(|e| same_site(&e.page) && (e.kind == TraceKind::Error || (!errors_only && e.kind == TraceKind::Console)))
                .take(limit)
                .map(|e| match e.kind {
                    TraceKind::Console => json!({ "kind": "log", "pack": e.pack, "text": e.args }),
                    _ => json!({ "kind": "error", "pack": e.pack, "in": format!("{}({})", e.call, e.args), "error": e.result.trim_start_matches("ERROR ") }),
                })
                .collect();
            Ok(json!(entries))
        }
        "trace" => {
            let entries: Vec<Value> = host::trace()
                .into_iter()
                .filter(|e| same_site(&e.page) && e.pack != "ai")
                .take(limit)
                .map(|e| json!({ "kind": format!("{:?}", e.kind).to_lowercase(), "pack": e.pack, "call": e.call, "args": e.args, "result": e.result, "micros": e.micros }))
                .collect();
            Ok(json!(entries))
        }
        "requests" => {
            let entries: Vec<Value> = gate
                .log()
                .into_iter()
                .filter(|e| e.page.as_deref() == Some(origin.as_str()))
                .take(limit)
                .map(|e| json!({ "allowed": e.allowed, "url": redact(&e.url), "reason": e.reason, "third_party": e.third_party }))
                .collect();
            Ok(json!(entries))
        }
        _ => Err(format!("unknown tool {tool}")),
    }
}

/// AI calls show in the trace like any other client's.
fn trace_call(tool: &str, args: String, page: &str) {
    host::record(TraceEntry { pack: "ai", kind: TraceKind::Call, page: page.to_string(), call: tool.to_string(), args, result: String::new(), micros: 0 });
}

/// A URL without the parts that commonly carry secrets: credentials, query values and the
/// fragment (OAuth tokens travel in both).
pub fn redact(url: &str) -> String {
    let Ok(mut u) = Url::parse(url) else { return "[unreadable URL]".into() };
    let _ = u.set_password(None);
    let _ = u.set_username("");
    if u.query().is_some() {
        let keys = u.query_pairs().map(|(k, _)| format!("{k}=REDACTED")).collect::<Vec<_>>().join("&");
        u.set_query(if keys.is_empty() { None } else { Some(&keys) });
    }
    if u.fragment().is_some() {
        u.set_fragment(Some("REDACTED"));
    }
    u.to_string()
}

/// The page as indented lines: `tag#id.class attr="…" "text"`. Leaves out scripts, styles,
/// hidden inputs and every form field's value.
pub fn outline(doc: &BaseDocument) -> String {
    let mut out = String::new();
    write_outline(doc, doc.root_node().id, 0, &mut out);
    if out.len() > MAX_OUTLINE {
        let mut cut = MAX_OUTLINE;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n… (truncated)");
    }
    out
}

fn write_outline(doc: &BaseDocument, id: NodeId, depth: usize, out: &mut String) {
    if out.len() > MAX_OUTLINE {
        return;
    }
    let Some(node) = doc.get_node(id) else { return };
    let mut depth = depth;
    if let Some(el) = node.element_data() {
        let tag = el.name.local.as_ref();
        if matches!(tag, "script" | "style" | "template" | "link" | "meta") {
            return;
        }
        if tag == "input" && el.attr(blitz_dom::local_name!("type")).is_some_and(|t| t.eq_ignore_ascii_case("hidden")) {
            return;
        }
        let mut line = tag.to_string();
        if let Some(id) = el.attr(blitz_dom::local_name!("id")) {
            line.push_str(&format!("#{id}"));
        }
        if let Some(class) = el.attr(blitz_dom::local_name!("class")) {
            for c in class.split_whitespace().take(3) {
                line.push_str(&format!(".{c}"));
            }
        }
        for attr in el.attrs() {
            let name = attr.name.local.as_ref();
            let value = match name {
                "href" | "src" | "action" => redact_relative(doc, &attr.value),
                "alt" | "title" | "role" | "aria-label" | "type" | "name" | "placeholder" | "for" | "method" | "disabled" | "checked" => attr.value.to_string(),
                _ => continue,
            };
            line.push_str(&format!(" {name}={value:?}"));
        }
        // Form fields: never their value, nor a textarea's contents.
        let field = matches!(tag, "input" | "textarea" | "select");
        if field {
            line.push_str(" [value hidden]");
        }
        push_line(out, depth, &line);
        if tag == "textarea" {
            return;
        }
        depth += 1;
    } else if let Some(text) = node.text_data() {
        let text: String = text.content.split_whitespace().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            let short: String = text.chars().take(300).collect();
            push_line(out, depth, &format!("{short:?}"));
        }
        return;
    }
    for &child in &node.children {
        write_outline(doc, child, depth, out);
    }
}

fn push_line(out: &mut String, depth: usize, line: &str) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push_str(line);
    out.push('\n');
}

/// Attribute URLs are often relative; resolve them against the page so they can be redacted.
fn redact_relative(doc: &BaseDocument, value: &str) -> String {
    match doc.url().join(value) {
        Ok(u) => redact(u.as_str()),
        Err(_) => "[unreadable URL]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    fn request(headers: &[(&str, &str)], body: &str) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            path: "/mcp".into(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn requests_need_the_token_and_no_origin() {
        let host = format!("127.0.0.1:{PORT}");
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        assert_eq!(route(&request(&[("Host", &host)], ping), "secret").0, "401 Unauthorized");
        assert_eq!(route(&request(&[("Host", &host), ("Authorization", "Bearer wrong!")], ping), "secret").0, "401 Unauthorized");
        // A web page, even with the token, is refused.
        let from_page = [("Host", host.as_str()), ("Authorization", "Bearer secret"), ("Origin", "https://evil.example")];
        assert_eq!(route(&request(&from_page, ping), "secret").0, "403 Forbidden");
        // A DNS-rebound name isn't accepted as the host.
        assert_eq!(route(&request(&[("Host", "evil.example:7807"), ("Authorization", "Bearer secret")], ping), "secret").0, "403 Forbidden");
        let (status, body) = route(&request(&[("Host", &host), ("Authorization", "Bearer secret")], ping), "secret");
        assert_eq!(status, "200 OK");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["result"], json!({}));
    }

    /// Real HTTP over a socket, as an MCP client sends it.
    #[test]
    fn serves_mcp_over_http() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let server = std::thread::spawn(move || serve(listener, "secret", &stopped));
        let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(stream, "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nAuthorization: Bearer secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        let json: Value = serde_json::from_str(reply.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(json["id"], 7);
        assert_eq!(json["result"]["tools"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn mcp_handshake_and_tool_list() {
        let init = rpc(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(rpc(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        let list = rpc(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["list_tabs", "read_page", "console", "trace", "requests"]);
    }

    #[test]
    fn urls_lose_credentials_query_values_and_fragments() {
        assert_eq!(redact("https://me:pw@example.com/a?token=abc&x=1#access_token=zzz"), "https://example.com/a?token=REDACTED&x=REDACTED#REDACTED");
        assert_eq!(redact("https://example.com/plain"), "https://example.com/plain");
    }

    #[test]
    fn outline_hides_field_values_and_scripts() {
        let html = r#"<html><head><title>Sign in</title><script>var secret = 1</script></head><body>
            <h1 id="t" class="big title">Welcome</h1>
            <a href="/next?session=abc">Next</a>
            <form><input type="password" name="pw" value="hunter2"><input type="hidden" name="csrf" value="tok">
            <textarea name="note">private note</textarea></form></body></html>"#;
        let config = DocumentConfig { base_url: Some("https://example.com/login".into()), ..Default::default() };
        let doc = HtmlDocument::from_html(html, config).into_inner();
        let text = outline(&doc);
        assert!(text.contains("h1#t.big.title"), "{text}");
        assert!(text.contains("\"Welcome\""), "{text}");
        assert!(text.contains(r#"href="https://example.com/next?session=REDACTED""#), "{text}");
        assert!(text.contains(r#"input type="password" name="pw" [value hidden]"#), "{text}");
        for secret in ["hunter2", "csrf", "tok", "private note", "var secret", "abc"] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
    }
}
