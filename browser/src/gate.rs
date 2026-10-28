//! The network chokepoint. Every request the browser makes - the page itself, its
//! subresources (CSS, images, fonts) and favicons - goes through `Gate`, which logs it
//! and refuses it when policy says so. Nothing else holds the real provider.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use blitz_net::{Provider, ProviderError};
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request, Url};

/// How many requests the log keeps; older entries are dropped.
const LOG_CAPACITY: usize = 1000;

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub allowed: bool,
    pub url: String,
}

pub struct Gate {
    inner: Arc<Provider>,
    block_all: AtomicBool,
    log: Mutex<VecDeque<LogEntry>>,
    /// Bumped on every log change, so the UI can skip re-rendering when nothing happened.
    version: AtomicU64,
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

    /// Decide whether `url` may be fetched, and record the decision.
    fn allow(&self, url: &Url) -> bool {
        // Only network schemes leave the machine. about:, data: and file: stay local.
        let leaves_machine = matches!(url.scheme(), "http" | "https" | "ws" | "wss");
        let allowed = !(leaves_machine && self.block_all());

        let mut log = self.log.lock().unwrap();
        if log.len() == LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(LogEntry { allowed, url: url.to_string() });
        drop(log);
        self.version.fetch_add(1, Ordering::Relaxed);

        allowed
    }

    /// Fetch a whole resource (used for page loads and favicons).
    pub async fn fetch_async(&self, request: Request) -> Result<(String, Bytes), ProviderError> {
        if !self.allow(&request.url) {
            return Err(blocked_error(&request.url));
        }
        self.inner.fetch_async(request).await
    }

    #[cfg(feature = "cache")]
    pub async fn clear_cache(&self) {
        self.inner.clear_cache().await
    }
}

/// Subresource fetches made by documents (stylesheets, images, fonts).
impl NetProvider for Gate {
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        if !self.allow(&request.url) {
            handler.error(request.url.to_string(), "blocked by policy".into());
            return;
        }
        self.inner.fetch(doc_id, request, handler);
    }
}

fn blocked_error(url: &Url) -> ProviderError {
    ProviderError::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("Blocked by policy (block all requests is on): {url}"),
    ))
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
        assert!(gate.allow(&url("https://example.com/")));
        assert!(gate.allow(&url("http://example.com/a.css")));
    }

    #[test]
    fn block_all_refuses_network_schemes() {
        let gate = gate();
        gate.set_block_all(true);
        for u in ["https://example.com/", "http://example.com/", "wss://example.com/socket"] {
            assert!(!gate.allow(&url(u)), "{u} should be blocked");
        }
    }

    #[test]
    fn block_all_keeps_local_schemes() {
        let gate = gate();
        gate.set_block_all(true);
        for u in ["about:newtab", "data:text/plain,hi", "file:///tmp/page.html"] {
            assert!(gate.allow(&url(u)), "{u} should stay allowed");
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
        gate.allow(&url("https://first.example/"));
        gate.set_block_all(true);
        gate.allow(&url("https://second.example/"));

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
            gate.allow(&url(&format!("https://example.com/{i}")));
        }
        assert_eq!(gate.log().len(), LOG_CAPACITY);
    }
}
