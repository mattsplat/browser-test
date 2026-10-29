//! AI access: an MCP server so an agent can read and debug the browser, and, with a separate
//! permission, control dev-mode sites.
//!
//! - Off at every launch; turned on from the menu, and visibly on (a bar with "Turn off").
//! - Listens on 127.0.0.1 only, and every request needs the token, which is new each time
//!   access is turned on. The address and token are written to `ai-access.json` (readable
//!   only by the user) next to settings.json, for MCP clients to use.
//! - Requests from web pages are refused: they carry an `Origin` header, MCP clients don't.
//! - Read-only: tabs, a page's outline, console and errors, the trace, and requests. Form
//!   field values, cookies, headers and URL query values and fragments are never returned.
//! - AI control is a second switch (menu, or the bar), off at every launch and whenever access
//!   is turned off. With it on, `eval`, `click` and `reload` work on sites with a dev-mode
//!   grant while dev mode is on (`control_decision`), and nowhere else.
//! - Every call appears in the trace (pack "ai"), refused ones as errors.
//!
//! The server thread only speaks HTTP and JSON-RPC; each tool call is answered on the UI
//! thread (the `AiAccess` component), where the tabs and their pages live.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use dioxus_native::prelude::*;
use serde_json::{Value, json};

use crate::debug::{self, redact};
use crate::gate::Gate;
use crate::host::{self, TraceEntry, TraceKind};
use crate::settings::{self, Grant, Settings};
use crate::tab::{Tab, TabId, TabStoreImplExt};

pub const PORT: u16 = 7807;

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
/// AI control: write tools on dev-mode sites. Off at launch and whenever access turns off.
static CONTROL: AtomicBool = AtomicBool::new(false);
/// Why the server couldn't start, shown in the AI access bar.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

pub fn is_on() -> bool {
    RUNNING.lock().unwrap().is_some()
}

pub fn control_on() -> bool {
    CONTROL.load(Ordering::Relaxed)
}

pub fn set_control(on: bool) {
    CONTROL.store(on, Ordering::Relaxed);
}

/// The tools that change a page; each needs `control_decision`'s approval.
const CONTROL_TOOLS: &[&str] = &["eval", "click", "reload"];

/// AI control's rules, apart from any I/O so they can be tested: a separate switch from
/// read-only access, and only on sites with a dev-mode grant while dev mode is on (turning
/// dev mode off disables grants without deleting them).
fn control_decision(settings: &Settings, control_on: bool, origin: &str) -> Result<(), String> {
    if !control_on {
        return Err("AI control is off. The user can turn it on in the browser's menu (AI control).".into());
    }
    if !settings.dev_mode {
        return Err("AI control works only on sites with a dev-mode grant, and dev mode is off.".into());
    }
    match settings.dev_grants.get(origin) {
        Some(Grant::Allow) => Ok(()),
        _ => Err(format!("AI control works only on sites with a dev-mode grant, and {origin} has none.")),
    }
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
    set_control(false);
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
            "instructions": "Read access to the browser: its tabs, page outlines, console and errors, \
                the pack trace, and network requests. Form values, cookies, headers and URL query values are hidden. \
                eval, click and reload need AI control, which the user turns on separately, and work only on sites \
                the user has granted dev mode.",
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
        { "name": "eval", "description": "Needs AI control and a dev-mode site. Evaluate JavaScript in a tab's page (JS pages only; Dart can't evaluate) and return the result. It reaches the page only through the browser's host, like the page's own code.",
          "inputSchema": { "type": "object", "properties": { "tab": tab, "expression": { "type": "string" } }, "required": ["expression"] } },
        { "name": "click", "description": "Needs AI control and a dev-mode site. Click the element a CSS selector matches: the page's click listeners run, as for element.click() (links aren't followed).",
          "inputSchema": { "type": "object", "properties": { "tab": tab, "selector": { "type": "string" } }, "required": ["selector"] } },
        { "name": "reload", "description": "Needs AI control and a dev-mode site. Reload a tab (dev-mode Dart source is recompiled).",
          "inputSchema": { "type": "object", "properties": { "tab": tab } } },
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
    let mut control = use_signal(control_on);

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
                if control() != control_on() {
                    control.set(control_on());
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
    let control = control();
    rsx!(
        div { class: "prompt-bar ai-bar",
            span { class: "prompt-text",
                if control { "AI access is on, with control of dev-mode sites (eval, click, reload)" } else { "AI access is on (read-only)" }
                " at {url()}. Token in {file}"
            }
            div {
                class: if control { "prompt-button primary" } else { "prompt-button" },
                onclick: move |_| set_control(!control),
                if control { "Turn off control" } else { "Allow control" }
            }
            div { class: "prompt-button", onclick: move |_| turn_off(), "Turn off" }
        }
    )
}

fn answer(tool: &str, args: &Value, tabs: Store<Vec<Tab>>, active: TabId, gate: &Gate) -> Result<Value, String> {
    let limit = args["limit"].as_u64().unwrap_or(100).min(500) as usize;
    if tool == "list_tabs" {
        trace_call(tool, String::new(), "");
        let list: Vec<Value> = debug::tabs(tabs, active)
            .iter()
            .map(|t| json!({ "id": t.id, "url": redact(t.url.as_str()), "title": t.title, "active": t.active }))
            .collect();
        return Ok(json!(list));
    }
    let wanted = args["tab"].as_u64();
    let tab = debug::tab(tabs, active, wanted).ok_or_else(|| format!("no tab {}", wanted.unwrap_or(active)))?;
    let origin = tab.origin();
    let detail = [&args["expression"], &args["selector"]].iter().find_map(|v| v.as_str()).map(|v| format!(": {v}")).unwrap_or_default();
    trace_call(tool, format!("tab {}{detail}", tab.id), tab.url.as_str());
    if CONTROL_TOOLS.contains(&tool) {
        if let Err(refused) = control_decision(&settings::get(), control_on(), &origin) {
            host::record(TraceEntry { pack: "ai", kind: TraceKind::Error, page: tab.url.to_string(), call: tool.to_string(), args: format!("tab {}{detail}", tab.id), result: format!("ERROR refused: {refused}"), micros: 0 });
            return Err(refused);
        }
    }
    let pack = || tab.page.as_ref().and_then(|p| p.pack()).ok_or("no script runs on this page");
    match tool {
        "eval" => {
            let expression = args["expression"].as_str().ok_or("eval needs an expression")?;
            pack()?.eval(expression, "ai").map(|result| json!({ "result": result })).map_err(|e| format!("{e:#}"))
        }
        "click" => {
            let selector = args["selector"].as_str().ok_or("click needs a selector")?;
            pack()?.click(selector).map(|clicked| json!({ "clicked": clicked })).map_err(|e| format!("{e:#}"))
        }
        "reload" => {
            let store = tabs.iter().find(|t| t.tab_id() == tab.id).ok_or("the tab has closed")?;
            store.reload();
            Ok(json!({ "reloading": redact(tab.url.as_str()) }))
        }
        "read_page" => {
            let page = tab.page.and_then(|p| p.doc()).ok_or("the tab has no page loaded")?;
            let outline = debug::outline(&page.borrow());
            Ok(json!({ "url": redact(tab.url.as_str()), "outline": outline }))
        }
        "console" => {
            let errors_only = args["errors_only"].as_bool().unwrap_or(false);
            let entries: Vec<Value> = debug::console(&origin, debug::Privacy::Redacted, usize::MAX)
                .into_iter()
                .filter(|e| !errors_only || e.level == debug::Level::Error)
                .take(limit)
                .map(|e| match (&e.input, e.level) {
                    (Some(input), _) => json!({ "kind": "input", "by": e.pack, "input": input, "result": e.text }),
                    (None, debug::Level::Error) if !e.context.is_empty() => json!({ "kind": "error", "pack": e.pack, "in": e.context, "error": e.text }),
                    (None, level) => json!({ "kind": "log", "level": level.name(), "pack": e.pack, "text": e.text }),
                })
                .collect();
            Ok(json!(entries))
        }
        "trace" => {
            let entries: Vec<Value> = debug::trace(&origin, debug::Privacy::Redacted, limit)
                .into_iter()
                .map(|e| json!({ "kind": format!("{:?}", e.kind).to_lowercase(), "pack": e.pack, "call": e.call, "args": e.args, "result": e.result, "micros": e.micros }))
                .collect();
            Ok(json!(entries))
        }
        "requests" => {
            let entries: Vec<Value> = debug::requests(gate, &origin, limit)
                .into_iter()
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(json["result"]["tools"].as_array().unwrap().len(), 8);
    }

    #[test]
    fn mcp_handshake_and_tool_list() {
        let init = rpc(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(rpc(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        let list = rpc(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["list_tabs", "read_page", "console", "trace", "requests", "eval", "click", "reload"]);
    }

    /// Control is its own switch, needs dev mode, and works only where dev mode was granted.
    #[test]
    fn control_needs_its_own_switch_dev_mode_and_a_grant() {
        let mut s = Settings { dev_mode: true, ..Default::default() };
        s.dev_grants.insert("https://mine.test".into(), Grant::Allow);
        s.dev_grants.insert("https://blocked.test".into(), Grant::Block);
        assert!(control_decision(&s, true, "https://mine.test").is_ok());
        // Off by default: read-only access alone never allows control.
        assert!(control_decision(&s, false, "https://mine.test").unwrap_err().contains("AI control is off"));
        // Production sites, blocked sites and sites never asked about are refused.
        assert!(control_decision(&s, true, "https://bank.example").unwrap_err().contains("has none"));
        assert!(control_decision(&s, true, "https://blocked.test").is_err());
        // Dev mode off disables every grant.
        s.dev_mode = false;
        assert!(control_decision(&s, true, "https://mine.test").unwrap_err().contains("dev mode is off"));
    }

}
