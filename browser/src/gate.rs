//! The network chokepoint. Every request the browser makes - page loads, their subresources
//! (CSS, images, fonts), scripts, wasm and favicons - goes through `Gate`, which logs it and
//! refuses it when policy says so. Nothing else holds the real provider.
//!
//! Policy (`decide`): only http(s)/ws(s) leave the machine. "Block all" refuses all of them.
//! Requests a page makes to another site (a different registrable domain) are refused unless
//! that site is allowed for the page's origin, while "Block third-party requests" is on.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use blitz_net::{Provider, ProviderError};
use blitz_traits::net::{Body, Bytes, NetHandler, NetProvider, Request, Url, http};

use crate::settings::{self, Settings};

/// How many requests the log keeps; older entries are dropped.
const LOG_CAPACITY: usize = 1000;

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub allowed: bool,
    pub url: String,
    /// Why it was refused.
    pub reason: Option<String>,
    /// The origin of the page that asked; None for navigations and browser UI.
    pub page: Option<String>,
    /// The other site's registrable domain, when the request was third-party.
    pub third_party: Option<String>,
}

pub struct Gate {
    inner: Arc<Provider>,
    block_all: AtomicBool,
    log: Mutex<VecDeque<LogEntry>>,
    /// Bumped on every log change, so the UI can skip re-rendering when nothing happened.
    version: AtomicU64,
}

/// A site, for third-party checks: the registrable domain (`news.bbc.co.uk` → `bbc.co.uk`).
/// None for URLs without a host, such as `file:`.
pub fn site_of(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    if url.host().is_some_and(|h| !matches!(h, url::Host::Domain(_))) {
        return Some(host.to_string()); // IP addresses are their own site.
    }
    Some(psl::domain_str(host).unwrap_or(host).to_string())
}

/// Whether `page` is a public site, whose fetches may not reach private addresses. Local
/// pages aren't: files, IP addresses that are themselves private, `localhost` and the
/// reserved local names (`.test`, `.localhost`, `.local`, `.internal`), as Herd uses.
pub fn is_public_page(page: &Url) -> bool {
    match page.host() {
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name != "localhost" && ![".localhost", ".test", ".local", ".internal", ".home.arpa"].iter().any(|s| name.ends_with(s))
        }
        Some(url::Host::Ipv4(ip)) => blitz_net::is_public_ip(ip.into()),
        Some(url::Host::Ipv6(ip)) => blitz_net::is_public_ip(ip.into()),
        None => false,
    }
}

/// Whether `url`'s host is a private or local IP address (names are checked as they resolve).
fn private_address(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !blitz_net::is_public_ip(ip.into()),
        Some(url::Host::Ipv6(ip)) => !blitz_net::is_public_ip(ip.into()),
        _ => false,
    }
}

/// Whether a request from `from` to `to` is cross-site, as `SameSite` cookies see it: their
/// sites differ, or `from` has none (a file or about page).
pub fn cross_site(from: &Url, to: &Url) -> bool {
    match (site_of(from), site_of(to)) {
        (Some(a), Some(b)) => a != b || (from.scheme() == "https") != (to.scheme() == "https"),
        _ => true,
    }
}

/// The policy: `Ok(third_party_site)` to allow, `Err(reason)` to refuse.
pub fn decide(settings: &Settings, block_all: bool, url: &Url, page: Option<&Url>) -> Result<Option<String>, String> {
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
        return Ok(None); // about:, data:, file: stay on the machine.
    }
    if block_all {
        return Err("block all requests is on".into());
    }
    let (Some(page), Some(target)) = (page, site_of(url)) else { return Ok(None) };
    if site_of(page).as_ref() == Some(&target) || site_of(page).is_none() {
        return Ok(None);
    }
    let allowed = settings.allowed_third_party.get(&settings::origin_of(page)).is_some_and(|s| s.contains(&target));
    if settings.block_third_party && !allowed {
        return Err(format!("third-party site {target} is not allowed on this page"));
    }
    Ok(Some(target))
}

impl Gate {
    pub fn new(inner: Arc<Provider>) -> Self {
        Self {
            inner,
            block_all: AtomicBool::new(false),
            log: Mutex::new(VecDeque::new()),
            version: AtomicU64::new(0),
        }
    }

    pub fn block_all(&self) -> bool {
        self.block_all.load(Ordering::Relaxed)
    }

    pub fn set_block_all(&self, on: bool) {
        self.block_all.store(on, Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// The request log, newest first.
    pub fn log(&self) -> Vec<LogEntry> {
        self.log.lock().unwrap().iter().rev().cloned().collect()
    }

    pub fn clear_log(&self) {
        self.log.lock().unwrap().clear();
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Decide whether `url` may be fetched for `page` (None: a navigation or browser UI),
    /// and record the decision.
    pub(crate) fn allow(&self, url: &Url, page: Option<&Url>) -> Result<(), String> {
        // Settings only matter for page-initiated requests; don't read them otherwise.
        let verdict = match page {
            Some(_) => decide(&settings::get(), self.block_all(), url, page),
            None => decide(&Settings::default(), self.block_all(), url, None),
        };
        let entry = LogEntry {
            allowed: verdict.is_ok(),
            url: url.to_string(),
            reason: verdict.as_ref().err().cloned(),
            page: page.map(settings::origin_of),
            third_party: match &verdict {
                Ok(site) => site.clone(),
                Err(_) => page.and(site_of(url)),
            },
        };

        let mut log = self.log.lock().unwrap();
        if log.len() == LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(entry);
        drop(log);
        self.version.fetch_add(1, Ordering::Relaxed);

        verdict.map(drop)
    }

    /// Fetch a whole resource for a navigation or the browser UI (a page load, a favicon).
    pub async fn fetch_async(&self, request: Request) -> Result<(String, Bytes), ProviderError> {
        self.allow(&request.url, None).map_err(|reason| blocked_error(&request.url, &reason))?;
        self.inner.fetch_async(request).await
    }

    /// Fetch a whole resource on behalf of `page` (its scripts, wasm, dev-mode source).
    pub async fn fetch_for_page(&self, request: Request, page: &Url) -> Result<(String, Bytes), ProviderError> {
        self.allow(&request.url, Some(page)).map_err(|reason| blocked_error(&request.url, &reason))?;
        self.inner.fetch_async(request).await
    }

    /// A page's `fetch()`: `request` on behalf of `page`, through the same policy as
    /// everything else, every redirect hop included (each is checked and logged). Rejections
    /// are `TypeError: ...` messages, as `fetch` rejects. See `PageFetch` for the rules.
    pub async fn fetch_from_page(&self, mut request: PageFetch, page: &Url) -> Result<FetchResponse, String> {
        // The URL is redacted (no credentials, query values or fragment): the message can reach
        // the trace, which AI access reads.
        let failed = |url: &Url, why: String| format!("TypeError: Failed to fetch {}: {why}", crate::debug::redact(url.as_str()));
        // Forbidden methods: TRACE and TRACK echo the request back, HttpOnly cookies included.
        if matches!(request.method.as_str(), "CONNECT" | "TRACE" | "TRACK") {
            return Err(failed(&request.url, format!("{} requests can't be made by a page", request.method)));
        }
        let page_origin = page.origin();
        let mut redirected = false;
        for _ in 0..MAX_REDIRECTS {
            let url = request.url.clone();
            match url.scheme() {
                "http" | "https" => {}
                // Local schemes never leave the machine.
                "data" => {
                    self.allow(&url, Some(page)).map_err(|reason| failed(&url, format!("blocked ({reason})")))?;
                    let (_, body) = self.inner.fetch_async(Request::get(url.clone())).await.map_err(|e| failed(&url, e.to_string()))?;
                    return Ok(FetchResponse { url: url.to_string(), status: 200, status_text: "OK".into(), headers: Vec::new(), body: String::from_utf8_lossy(&body).into_owned(), redirected, cross_origin: false });
                }
                // A file is readable only by a file page, in its folder or below it (as Firefox
                // once allowed), not anywhere on disk. The checked path is the one read.
                "file" => {
                    let path = file_beside(&url, page).ok_or_else(|| failed(&url, "a page can read only files in its own folder".into()))?;
                    self.allow(&url, Some(page)).map_err(|reason| failed(&url, format!("blocked ({reason})")))?;
                    let meta = std::fs::metadata(&path).map_err(|e| failed(&url, e.to_string()))?;
                    if meta.len() as usize > MAX_BODY {
                        return Err(failed(&url, format!("the file is over {} MB", MAX_BODY >> 20)));
                    }
                    let body = std::fs::read(&path).map_err(|e| failed(&url, e.to_string()))?;
                    return Ok(FetchResponse { url: url.to_string(), status: 200, status_text: "OK".into(), headers: Vec::new(), body: String::from_utf8_lossy(&body).into_owned(), redirected, cross_origin: false });
                }
                other => return Err(failed(&url, format!("{other}: URLs can't be fetched by this page"))),
            }
            self.allow(&url, Some(page)).map_err(|reason| failed(&url, format!("blocked ({reason})")))?;
            let same_origin = url.origin() == page_origin;
            if !same_origin && !request.is_simple() {
                return Err(failed(&url, "a cross-origin request with this method or these headers needs a CORS preflight, which isn't supported yet".into()));
            }
            // Cookies go only to the page's own origin (fetch's default, "same-origin").
            let credentials = same_origin && request.credentials != "omit";
            let mut sent = Request::get(url.clone());
            sent.method = request.method.parse().map_err(|_| failed(&url, format!("bad method {}", request.method)))?;
            for (name, value) in &request.headers {
                if let (Ok(name), Ok(value)) = (name.parse::<http::HeaderName>(), value.parse::<http::HeaderValue>()) {
                    sent.headers.append(name, value);
                }
            }
            if !same_origin {
                if let Ok(origin) = page_origin.ascii_serialization().parse() {
                    sent.headers.insert(http::header::ORIGIN, origin);
                }
            }
            if let Some(body) = &request.body {
                sent.body = Body::Bytes(Bytes::from(body.clone()));
            }
            sent.signal = request.signal.clone();
            // A public site's page can't reach this machine or the local network, by an
            // address or by a name that resolves to one (DNS rebinding).
            let public_only = is_public_page(page);
            if public_only && private_address(&url) {
                return Err(failed(&url, "a public page can't reach private or local addresses".into()));
            }
            let response = self.inner.fetch_once(sent, credentials, public_only, MAX_BODY, FETCH_TIMEOUT).await.map_err(|e| failed(&url, e.to_string()))?;
            let header = |name: &str| response.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                if let Some(next) = header("location").and_then(|l| url.join(&l).ok()) {
                    // 303, and 301/302 after a POST, continue as a GET without a body.
                    if response.status == 303 || (matches!(response.status, 301 | 302) && request.method == "POST") {
                        request.method = "GET".into();
                        request.body = None;
                    }
                    request.url = next;
                    redirected = true;
                    continue;
                }
            }
            if response.body.len() > MAX_BODY {
                return Err(failed(&url, format!("the response is over {} MB", MAX_BODY >> 20)));
            }
            let mut headers: Vec<(String, String)> = response.headers.iter().filter(|(n, _)| n != "set-cookie" && n != "set-cookie2").cloned().collect();
            if !same_origin {
                // Basic CORS: readable only if the server says this origin may read it.
                let allowed = header("access-control-allow-origin").is_some_and(|a| a.trim() == "*" || a.trim() == page_origin.ascii_serialization());
                if !allowed {
                    return Err(failed(&url, format!("CORS: the response doesn't allow {} to read it (no matching Access-Control-Allow-Origin)", page_origin.ascii_serialization())));
                }
                let exposed: Vec<String> = header("access-control-expose-headers").unwrap_or_default().split(',').map(|h| h.trim().to_ascii_lowercase()).collect();
                headers.retain(|(n, _)| CORS_SAFELISTED_RESPONSE.contains(&n.as_str()) || exposed.contains(n));
            }
            return Ok(FetchResponse {
                url: url.to_string(),
                status: response.status,
                status_text: response.status_text,
                headers,
                body: String::from_utf8_lossy(&response.body).into_owned(),
                redirected,
                cross_origin: !same_origin,
            });
        }
        Err(failed(&request.url, "too many redirects".into()))
    }

    #[cfg(feature = "cache")]
    pub async fn clear_cache(&self) {
        self.inner.clear_cache().await
    }
}

/// The file `url` names, if it's in file page `page`'s folder or below it. Both are decoded
/// to paths and resolved on disk (`..`, percent-encoded or not, and symlinks), so neither can
/// lead out of the folder.
fn file_beside(url: &Url, page: &Url) -> Option<std::path::PathBuf> {
    if page.scheme() != "file" || url.scheme() != "file" {
        return None;
    }
    let folder = page.to_file_path().ok()?.parent()?.canonicalize().ok()?;
    let target = url.to_file_path().ok()?.canonicalize().ok()?;
    target.starts_with(&folder).then_some(target)
}

const MAX_REDIRECTS: usize = 20;
/// How long one hop of a page's `fetch` may take.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// The biggest response body a page's `fetch` takes, so a page can't fill the browser's
/// memory (host memory isn't under the pack's cap).
const MAX_BODY: usize = 64 << 20;
/// Response headers any origin may read (the fetch spec's CORS-safelisted names).
const CORS_SAFELISTED_RESPONSE: &[&str] = &["cache-control", "content-language", "content-length", "content-type", "expires", "last-modified", "pragma"];
/// Request headers page code may not set; they're dropped, as browsers drop them.
const FORBIDDEN_REQUEST: &[&str] = &[
    "accept-charset", "accept-encoding", "access-control-request-headers", "access-control-request-method", "connection",
    "content-length", "cookie", "cookie2", "date", "dnt", "expect", "host", "keep-alive", "origin", "referer", "te",
    "trailer", "transfer-encoding", "upgrade", "user-agent", "via",
];

/// A page's `fetch(url, init)`, as the host read it.
#[derive(Clone, Debug)]
pub struct PageFetch {
    pub url: Url,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// `init.credentials`: "omit", "same-origin" (the default) or "include" (treated as
    /// same-origin: cookies never go to another origin).
    pub credentials: String,
    /// The page's: aborted when the page goes, which stops the request.
    pub signal: Option<blitz_traits::net::AbortSignal>,
}

impl PageFetch {
    pub fn new(url: Url) -> Self {
        Self { url, method: "GET".into(), headers: Vec::new(), body: None, credentials: "same-origin".into(), signal: None }
    }

    /// Add a request header, unless it's one page code may not set.
    pub fn header(&mut self, name: &str, value: &str) {
        let lower = name.to_ascii_lowercase();
        if !FORBIDDEN_REQUEST.contains(&lower.as_str()) && !lower.starts_with("proxy-") && !lower.starts_with("sec-") {
            self.headers.push((lower, value.to_string()));
        }
    }

    /// A "simple" request: one a browser sends cross-origin without a preflight.
    fn is_simple(&self) -> bool {
        matches!(self.method.as_str(), "GET" | "HEAD" | "POST")
            && self.headers.iter().all(|(name, value)| match name.as_str() {
                "accept" | "accept-language" | "content-language" => true,
                "content-type" => {
                    let essence = value.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
                    matches!(essence.as_str(), "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain")
                }
                _ => false,
            })
    }
}

/// What a page's `fetch` resolves with. Headers have lowercase names and never include
/// `Set-Cookie`; a cross-origin response has only the CORS-safelisted and exposed ones.
#[derive(Clone, Debug, PartialEq)]
pub struct FetchResponse {
    pub url: String,
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    /// The body as text (UTF-8, invalid bytes replaced): `text()` and `json()` are all
    /// that's supported.
    pub body: String,
    pub redirected: bool,
    pub cross_origin: bool,
}

/// Subresource fetches made by documents with no page context.
impl NetProvider for Gate {
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        if let Err(reason) = self.allow(&request.url, None) {
            handler.error(request.url.to_string(), format!("blocked: {reason}"));
            return;
        }
        self.inner.fetch(doc_id, request, handler);
    }
}

/// The gate as one page sees it: its subresource requests carry the page's URL, so
/// third-party rules apply.
pub struct PageGate {
    pub gate: Arc<Gate>,
    pub page: Option<Url>,
}

impl NetProvider for PageGate {
    fn fetch(&self, doc_id: usize, mut request: Request, handler: Box<dyn NetHandler>) {
        if let Err(reason) = self.gate.allow(&request.url, self.page.as_ref()) {
            handler.error(request.url.to_string(), format!("blocked: {reason}"));
            return;
        }
        // A page's request to another site gets only cookies SameSite allows across sites.
        if self.page.as_ref().is_some_and(|page| cross_site(page, &request.url)) {
            request.cookies = blitz_traits::net::CookieScope::CrossSite;
        }
        self.gate.inner.fetch(doc_id, request, handler);
    }
}

fn blocked_error(url: &Url, reason: &str) -> ProviderError {
    ProviderError::Io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("Blocked by policy ({reason}): {url}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn gate() -> Gate {
        Gate::new(Arc::new(Provider::new(None)))
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// Counts how a blocked subresource fetch was answered.
    struct CountingHandler(Arc<AtomicUsize>);
    impl NetHandler for CountingHandler {
        fn bytes(self: Box<Self>, _: String, _: Bytes) {
            panic!("a blocked request must never deliver bytes");
        }
        fn error(self: Box<Self>, _: String, _: String) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn allows_everything_by_default() {
        let gate = gate();
        assert!(gate.allow(&url("https://example.com/"), None).is_ok());
        assert!(gate.allow(&url("http://example.com/a.css"), None).is_ok());
    }

    #[test]
    fn block_all_refuses_network_schemes() {
        let gate = gate();
        gate.set_block_all(true);
        for u in ["https://example.com/", "http://example.com/", "wss://example.com/socket"] {
            assert!(!gate.allow(&url(u), None).is_ok(), "{u} should be blocked");
        }
    }

    #[test]
    fn block_all_keeps_local_schemes() {
        let gate = gate();
        gate.set_block_all(true);
        for u in ["about:newtab", "data:text/plain,hi", "file:///tmp/page.html"] {
            assert!(gate.allow(&url(u), None).is_ok(), "{u} should stay allowed");
        }
    }

    #[test]
    fn blocked_subresource_gets_an_error() {
        let gate = gate();
        gate.set_block_all(true);
        let errors = Arc::new(AtomicUsize::new(0));
        let request = Request::get(url("https://example.com/style.css"));
        gate.fetch(0, request, Box::new(CountingHandler(errors.clone())));
        assert_eq!(errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn logs_every_decision_newest_first() {
        let gate = gate();
        let _ = gate.allow(&url("https://first.example/"), None);
        gate.set_block_all(true);
        let _ = gate.allow(&url("https://second.example/"), None);

        let log = gate.log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].url, "https://second.example/");
        assert!(!log[0].allowed);
        assert!(log[1].allowed);
    }

    #[test]
    fn log_is_capped() {
        let gate = gate();
        for i in 0..LOG_CAPACITY + 10 {
            let _ = gate.allow(&url(&format!("https://example.com/{i}")), None);
        }
        assert_eq!(gate.log().len(), LOG_CAPACITY);
    }

    fn page(s: &str) -> Option<Url> {
        Some(url(s))
    }

    #[test]
    fn same_site_includes_subdomains_and_public_suffixes() {
        assert_eq!(site_of(&url("https://cdn.example.com/a.css")).as_deref(), Some("example.com"));
        assert_eq!(site_of(&url("https://news.bbc.co.uk/")).as_deref(), Some("bbc.co.uk"));
        assert_eq!(site_of(&url("https://api.myapp.test/")).as_deref(), Some("myapp.test"));
        assert_eq!(site_of(&url("http://127.0.0.1:8080/")).as_deref(), Some("127.0.0.1"));
        assert_eq!(site_of(&url("file:///tmp/a.html")), None);

        let s = Settings::default();
        assert_eq!(decide(&s, false, &url("https://cdn.example.com/a.css"), page("https://www.example.com/").as_ref()), Ok(None));
        assert_eq!(decide(&s, false, &url("https://www.bbc.co.uk/x.png"), page("https://news.bbc.co.uk/").as_ref()), Ok(None));
    }

    #[test]
    fn third_party_is_blocked_by_default() {
        let s = Settings::default();
        let verdict = decide(&s, false, &url("https://tracker.io/p.gif"), page("https://news.example/").as_ref());
        assert!(verdict.unwrap_err().contains("tracker.io"));
    }

    #[test]
    fn allowlist_is_per_origin() {
        let mut s = Settings::default();
        s.allowed_third_party.entry("https://news.example".into()).or_default().insert("cdn-images.net".into());
        let img = url("https://img.cdn-images.net/1.jpg");
        assert_eq!(decide(&s, false, &img, page("https://news.example/story").as_ref()), Ok(Some("cdn-images.net".into())));
        assert!(decide(&s, false, &img, page("https://other.example/").as_ref()).is_err());
    }

    #[test]
    fn third_party_rule_can_be_turned_off() {
        let s = Settings { block_third_party: false, ..Settings::default() };
        assert_eq!(decide(&s, false, &url("https://tracker.io/p.gif"), page("https://news.example/").as_ref()), Ok(Some("tracker.io".into())));
    }

    #[test]
    fn local_pages_and_schemes_are_not_third_party() {
        let s = Settings::default();
        // A file:// page has no site, so its web requests aren't third-party.
        assert_eq!(decide(&s, false, &url("https://example.com/a.css"), page("file:///tmp/a.html").as_ref()), Ok(None));
        // Local schemes never leave the machine, even with block-all on.
        assert_eq!(decide(&s, true, &url("data:text/plain,hi"), page("https://news.example/").as_ref()), Ok(None));
        // Navigations (no page) aren't third-party; block-all still applies.
        assert_eq!(decide(&s, false, &url("https://anything.example/"), None), Ok(None));
        assert!(decide(&s, true, &url("https://anything.example/"), None).is_err());
    }

    /// A file page reads files beside it, never elsewhere on disk; only simple requests go
    /// cross-origin (there's no preflight); page code can't set forbidden headers.
    #[test]
    fn page_fetch_rules() {
        // A site folder with a file, a secret beside the folder, and a symlink out of it.
        let root = std::env::temp_dir().join(format!("file-beside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("site/data")).unwrap();
        std::fs::write(root.join("site/data/a.json"), "{}").unwrap();
        std::fs::write(root.join("secret.txt"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("secret.txt"), root.join("site/link.txt")).unwrap();
        let page = Url::from_file_path(root.join("site/index.html")).unwrap();
        let at = |rel: &str| page.join(rel).unwrap();
        assert!(file_beside(&at("data/a.json"), &page).is_some());
        assert!(file_beside(&at("..%2Fsecret.txt"), &page).is_none(), "encoded traversal");
        assert!(file_beside(&at("data/..%2F..%2Fsecret.txt"), &page).is_none(), "encoded traversal");
        assert!(file_beside(&at("../secret.txt"), &page).is_none());
        assert!(file_beside(&at("link.txt"), &page).is_none(), "a symlink out of the folder");
        assert!(file_beside(&url("file:///etc/passwd"), &page).is_none());
        assert!(file_beside(&at("data/a.json"), &url("https://x.test/")).is_none());
        let _ = std::fs::remove_dir_all(&root);

        let mut f = PageFetch::new(url("https://a.test/"));
        f.header("Cookie", "stolen=1");
        f.header("Content-Type", "text/plain; charset=utf-8");
        assert_eq!(f.headers, [("content-type".to_string(), "text/plain; charset=utf-8".to_string())]);
        assert!(f.is_simple());
        f.header("X-Custom", "1");
        assert!(!f.is_simple());
        let mut trace = PageFetch::new(url("https://a.test/"));
        trace.method = "TRACE".into();
        let refused = tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(gate().fetch_from_page(trace, &url("https://a.test/")));
        assert!(refused.unwrap_err().contains("TRACE requests can't be made"));
        let mut put = PageFetch::new(url("https://a.test/"));
        put.method = "PUT".into();
        assert!(!put.is_simple());
    }

    /// A public site's page can't reach this machine: not by address, not by a name that
    /// resolves to it. Local pages can.
    #[tokio::test]
    async fn public_pages_cant_reach_private_addresses() {
        assert!(is_public_page(&url("https://news.example/")));
        assert!(!is_public_page(&url("http://myapp.test/")) && !is_public_page(&url("http://127.0.0.1:8000/")) && !is_public_page(&url("file:///a.html")));
        assert!(private_address(&url("http://192.168.1.1/")) && private_address(&url("http://[::1]/")) && private_address(&url("http://10.0.0.8/")));
        assert!(!private_address(&url("http://93.184.216.34/")) && !private_address(&url("http://example.com/")));
        // A name that resolves only to this machine is refused by the resolver.
        assert!(blitz_net::resolve_public("localhost").await.is_err());
    }

    /// A blocked request is refused before anything is sent, and logged as the page's.
    #[tokio::test]
    async fn a_pages_fetch_obeys_the_third_party_rule() {
        let gate = gate();
        let err = gate.fetch_from_page(PageFetch::new(url("https://tracker.io/p")), &url("https://news.example/")).await.unwrap_err();
        assert!(err.starts_with("TypeError: Failed to fetch") && err.contains("tracker.io"), "{err}");
        assert!(!gate.log()[0].allowed);
        let err = gate.fetch_from_page(PageFetch::new(url("file:///etc/passwd")), &url("https://news.example/")).await.unwrap_err();
        assert!(err.contains("only files in its own folder"), "{err}");
    }
}
