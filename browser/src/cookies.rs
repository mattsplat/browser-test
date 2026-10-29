//! The cookie jar: one per profile (a profile is a process), shared by the network stack and
//! page code. Every request's `Cookie` header and every response's `Set-Cookie` go through it
//! (blitz-net is given it with `set_cookie_store`), and so does `document.cookie`.
//!
//! The rules are RFC 6265's, from the `cookie_store` crate (domain and path matching, expiry,
//! `Secure` only sent over https, `HttpOnly` only over HTTP), plus what browsers add:
//! - page code never sees or overwrites an `HttpOnly` cookie, and can't set one;
//! - a `Secure` cookie can only be set from an https URL;
//! - a cookie can't be set for a public suffix (`Domain=co.uk`), using the `psl` list the gate
//!   already uses.
//!
//! **Persistence:** cookies with an expiry are saved in the profile's `cookies.json`, replaced
//! whole (`profile::write_atomic`) whenever one is set, changed or removed. Session cookies
//! stay in memory and are gone after a restart, as in other browsers. With no profile folder
//! (tests), nothing is saved.
//!
//! **Privacy:** cookie values never reach AI access: `debug.rs` hides `Document.cookie` calls
//! in redacted trace views, and the Site panel shows only a count.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use blitz_traits::net::{CookieScope, Url};
use cookie_store::{Cookie, CookieStore, RawCookie};

/// Browsers' usual limits: name plus value, cookies per domain, cookies in all.
const MAX_COOKIE_BYTES: usize = 4096;
const MAX_PER_DOMAIN: usize = 180;
const MAX_TOTAL: usize = 3000;

/// A cookie's identity in the store: (domain, path, name).
type Key = (String, String, String);

pub struct Jar {
    inner: Mutex<Inner>,
    /// Where persistent cookies are saved; None keeps them in memory only.
    path: Option<PathBuf>,
    /// The profile's jar saves on a thread of its own, after changes settle (`Saver`);
    /// a jar opened directly (tests) saves at once.
    saver: Option<Mutex<Sender<Save>>>,
}

struct Inner {
    store: CookieStore,
    /// When each cookie was set, as a counter, so the oldest go first past the limits.
    order: HashMap<Key, u64>,
    next: u64,
}

enum Save {
    Bytes(Vec<u8>),
    /// Write what's pending now, then say so.
    Flush(Sender<()>),
}

/// The profile's jar, loaded from its folder on first use.
pub fn jar() -> Arc<Jar> {
    static JAR: OnceLock<Arc<Jar>> = OnceLock::new();
    JAR.get_or_init(|| {
        let mut jar = Jar::open(crate::profile::dir().map(|d| d.join("cookies.json")));
        if let Some(path) = jar.path.clone() {
            let (tx, rx) = channel();
            std::thread::Builder::new().name("cookie-saver".into()).spawn(move || saver(path, rx)).expect("cookie saver thread");
            jar.saver = Some(Mutex::new(tx));
        }
        Arc::new(jar)
    })
    .clone()
}

/// Give the network stack the profile's jar, so requests and `document.cookie` share it. Call
/// before the first network provider is created (later calls change nothing).
pub fn install() {
    let jar = jar();
    let view = |scope| Arc::new(View { jar: jar.clone(), scope }) as Arc<dyn blitz_net::CookieStore>;
    blitz_net::set_cookie_stores([view(CookieScope::SameSite), view(CookieScope::CrossSiteNavigation), view(CookieScope::CrossSite)]);
}

/// Writes the jar's file off the UI thread: the latest snapshot, half a second after the last
/// change (a page load sets many cookies at once), or at once when asked to flush.
fn saver(path: PathBuf, rx: Receiver<Save>) {
    let mut pending: Option<Vec<u8>> = None;
    loop {
        let message = match pending {
            Some(_) => rx.recv_timeout(Duration::from_millis(500)),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match message {
            Ok(Save::Bytes(bytes)) => pending = Some(bytes),
            Ok(Save::Flush(done)) => {
                if let Some(bytes) = pending.take() {
                    write(&path, &bytes);
                }
                let _ = done.send(());
            }
            Err(RecvTimeoutError::Timeout) => write(&path, &pending.take().unwrap_or_default()),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn write(path: &std::path::Path, bytes: &[u8]) {
    let written = path.parent().map(std::fs::create_dir_all).transpose().and_then(|_| crate::profile::write_atomic(path, bytes));
    if let Err(e) = written {
        tracing::error!("could not save cookies to {}: {e}", path.display());
    }
}

fn key(c: &Cookie<'_>) -> Option<Key> {
    Some((c.domain.as_cow()?.into_owned(), c.path.as_ref().to_string(), c.name().to_string()))
}

/// Whether a cookie for `domain` could be sent where one for `other` is, or the reverse.
fn domains_overlap(domain: &str, other: &str) -> bool {
    domain == other || domain.ends_with(&format!(".{other}")) || other.ends_with(&format!(".{domain}"))
}

impl Jar {
    /// The jar saved at `path` (none yet: empty). A file that won't parse is kept as
    /// `cookies.json.bad` and the jar starts empty, as settings.json does.
    pub fn open(path: Option<PathBuf>) -> Jar {
        let store = match path.as_ref().and_then(|p| std::fs::read(p).ok().map(|bytes| (p, bytes))) {
            Some((p, bytes)) => cookie_store::serde::json::load(&bytes[..]).unwrap_or_else(|e| {
                crate::profile::keep_bad(p, &e.to_string());
                CookieStore::default()
            }),
            None => CookieStore::default(),
        };
        let order: HashMap<Key, u64> = store.iter_any().filter_map(key).zip(0..).collect();
        let next = order.len() as u64;
        Jar { inner: Mutex::new(Inner { store, order, next }), path, saver: None }
    }

    /// Save the persistent, unexpired cookies: on the saver thread, or now.
    fn save(&self, store: &CookieStore) {
        let Some(path) = &self.path else { return };
        let mut bytes = Vec::new();
        if let Err(e) = cookie_store::serde::json::save(store, &mut bytes) {
            tracing::error!("could not save cookies: {e}");
            return;
        }
        match &self.saver {
            Some(saver) => {
                let _ = saver.lock().unwrap().send(Save::Bytes(bytes));
            }
            None => write(path, &bytes),
        }
    }

    /// Write any change not yet saved, and wait for it (at exit).
    pub fn flush(&self) {
        if let Some(saver) = &self.saver {
            let (done, wait) = channel();
            if saver.lock().unwrap().send(Save::Flush(done)).is_ok() {
                let _ = wait.recv_timeout(Duration::from_secs(5));
            }
        }
    }

    /// Store `raw`, received from `url`, if the rules allow it. `from_page` means
    /// `document.cookie` rather than a response header. None if it was refused; otherwise
    /// whether it changes what's saved on disk: it adds, replaces or removes a cookie with
    /// an expiry, or pushes one out past the limits.
    fn store(&self, inner: &mut Inner, raw: &RawCookie<'_>, url: &Url, from_page: bool) -> Option<bool> {
        let secure_url = url.scheme() == "https";
        if raw.secure() == Some(true) && !secure_url {
            return None;
        }
        if raw.name().len() + raw.value().len() > MAX_COOKIE_BYTES {
            return None;
        }
        // No cookies for a whole public suffix (`Domain=co.uk`), unless that's the host itself.
        if let Some(domain) = raw.domain().map(|d| d.trim_start_matches('.').to_ascii_lowercase()) {
            if psl::suffix_str(&domain) == Some(domain.as_str()) && url.host_str() != Some(domain.as_str()) {
                return None;
            }
        }
        if from_page && raw.http_only() == Some(true) {
            return None;
        }
        let cookie = Cookie::try_from_raw_cookie(raw, url).ok()?;
        let key = key(&cookie)?;
        let old = inner.store.get_any(&key.0, &key.1, &key.2).map(|c| (c.http_only() == Some(true), c.is_persistent()));
        // Page code can't overwrite (or delete) an HttpOnly cookie either.
        if from_page && old.is_some_and(|(http_only, _)| http_only) {
            return None;
        }
        // From an insecure URL, nothing may shadow a Secure cookie of the same name that could
        // be sent alongside it (RFC 6265bis 5.7, step 14).
        if !secure_url {
            let shadows = inner.store.iter_unexpired().any(|c| {
                c.secure() == Some(true)
                    && c.name() == raw.name()
                    && c.domain.as_cow().is_some_and(|d| domains_overlap(&d, &key.0))
                    && key.1.starts_with(c.path.as_ref())
            });
            if shadows {
                return None;
            }
        }
        let persistent = cookie.is_persistent();
        inner.store.insert(cookie.into_owned(), url).ok()?;
        inner.order.insert(key.clone(), inner.next);
        inner.next += 1;
        let evicted = Self::evict(inner, &key.0);
        Some(persistent || old.is_some_and(|(_, was_persistent)| was_persistent) || evicted)
    }

    /// Past the limits, the oldest cookies go: of `domain`, then of all. Returns whether any did.
    fn evict(inner: &mut Inner, domain: &str) -> bool {
        if inner.store.iter_any().count() <= MAX_PER_DOMAIN {
            return false;
        }
        // Oldest first.
        let by_age = |of_domain: bool| -> Vec<Key> {
            let mut keys: Vec<(u64, Key)> = inner
                .store
                .iter_any()
                .filter_map(key)
                .filter(|k| !of_domain || k.0 == domain)
                .map(|k| (inner.order.get(&k).copied().unwrap_or(0), k))
                .collect();
            keys.sort();
            keys.into_iter().map(|(_, k)| k).collect()
        };
        let mine = by_age(true);
        let mut doomed: Vec<Key> = mine.iter().take(mine.len().saturating_sub(MAX_PER_DOMAIN)).cloned().collect();
        let rest: Vec<Key> = by_age(false).into_iter().filter(|k| !doomed.contains(k)).collect();
        doomed.extend(rest.iter().take(rest.len().saturating_sub(MAX_TOTAL)).cloned());
        for k in &doomed {
            inner.store.remove(&k.0, &k.1, &k.2);
            inner.order.remove(k);
        }
        !doomed.is_empty()
    }

    /// `document.cookie` for a page at `page`: the cookies it would send there, without
    /// `HttpOnly` ones, longest path first (as browsers order them), as `name=value; ...`.
    pub fn document_cookie(&self, page: &Url) -> String {
        if !matches!(page.scheme(), "http" | "https") {
            return String::new();
        }
        let inner = self.inner.lock().unwrap();
        let mut cookies: Vec<&Cookie<'static>> = inner.store.matches(page).into_iter().filter(|c| c.http_only() != Some(true)).collect();
        cookies.sort_by(|a, b| b.path.as_ref().len().cmp(&a.path.as_ref().len()).then_with(|| a.name().cmp(b.name())));
        cookies.iter().map(|c| format!("{}={}", c.name(), c.value())).collect::<Vec<_>>().join("; ")
    }

    /// `document.cookie = text` on a page at `page`. Ignored where a browser would ignore it
    /// (no host, `HttpOnly`, a bad cookie); returns whether it was stored.
    pub fn set_document_cookie(&self, page: &Url, text: &str) -> bool {
        if !matches!(page.scheme(), "http" | "https") {
            return false;
        }
        let Ok(raw) = RawCookie::parse(text.to_string()) else { return false };
        let mut inner = self.inner.lock().unwrap();
        let Some(saved) = self.store(&mut inner, &raw, page, true) else { return false };
        if saved {
            self.save(&inner.store);
        }
        true
    }

    /// The `Cookie` header for a request to `url`. A cross-site request (`scope`) leaves out
    /// `SameSite=Strict` cookies, and a cross-site subresource or non-GET navigation leaves
    /// out `Lax` ones too; cookies without the attribute are sent, as before browsers made
    /// Lax the default.
    pub fn header(&self, url: &Url, scope: CookieScope) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        let header = inner
            .store
            .matches(url)
            .into_iter()
            .filter(|c| match (scope, c.same_site()) {
                (CookieScope::SameSite, _) => true,
                (_, Some(cookie::SameSite::Strict)) => false,
                (CookieScope::CrossSite, Some(cookie::SameSite::Lax)) => false,
                _ => true,
            })
            .map(|c| format!("{}={}", c.name(), c.value()))
            .collect::<Vec<_>>()
            .join("; ");
        (!header.is_empty()).then_some(header)
    }

    /// How many unexpired cookies belong to `site` (a registrable domain, as `gate::site_of`).
    pub fn count(&self, site: &str) -> usize {
        self.inner.lock().unwrap().store.iter_unexpired().filter(|c| cookie_site(c) == site).count()
    }

    /// Remove every cookie of `site`.
    pub fn clear_site(&self, site: &str) {
        let mut inner = self.inner.lock().unwrap();
        let doomed: Vec<Key> = inner.store.iter_any().filter(|c| cookie_site(c) == site).filter_map(key).collect();
        for k in &doomed {
            inner.store.remove(&k.0, &k.1, &k.2);
            inner.order.remove(k);
        }
        self.save(&inner.store);
    }
}

/// The site (registrable domain) a cookie belongs to.
fn cookie_site(c: &Cookie<'_>) -> String {
    let domain = c.domain.as_cow().map(|d| d.into_owned()).unwrap_or_default();
    // IP addresses are their own site, as in `gate::site_of`.
    if domain.parse::<std::net::IpAddr>().is_ok() {
        return domain;
    }
    psl::domain_str(&domain).unwrap_or(&domain).to_string()
}

/// The jar as the network sees it for one kind of request: reqwest asks for its `Cookie`
/// header and hands over its responses' `Set-Cookie` headers.
pub struct View {
    pub jar: Arc<Jar>,
    pub scope: CookieScope,
}

impl blitz_net::CookieStore for View {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &blitz_net::HeaderValue>, url: &Url) {
        self.jar.set_cookies(headers, url)
    }

    fn cookies(&self, url: &Url) -> Option<blitz_net::HeaderValue> {
        self.jar.header(url, self.scope).and_then(|h| blitz_net::HeaderValue::from_str(&h).ok())
    }
}

/// The jar for same-site requests.
impl blitz_net::CookieStore for Jar {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &blitz_net::HeaderValue>, url: &Url) {
        let mut inner = self.inner.lock().unwrap();
        let mut save = false;
        for header in headers {
            let Ok(text) = std::str::from_utf8(header.as_bytes()) else { continue };
            if let Ok(raw) = RawCookie::parse(text.to_string()) {
                save |= self.store(&mut inner, &raw, url, false) == Some(true);
            }
        }
        if save {
            self.save(&inner.store);
        }
    }

    fn cookies(&self, url: &Url) -> Option<blitz_net::HeaderValue> {
        self.header(url, CookieScope::SameSite).and_then(|h| blitz_net::HeaderValue::from_str(&h).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_net::CookieStore as _;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn header(s: &str) -> blitz_net::HeaderValue {
        blitz_net::HeaderValue::from_str(s).unwrap()
    }

    fn temp_jar(name: &str) -> (PathBuf, Jar) {
        let dir = std::env::temp_dir().join(format!("cookie-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cookies.json");
        (dir, Jar::open(Some(path)))
    }

    /// Page code sees what it would send, minus HttpOnly; it can't set or overwrite an
    /// HttpOnly cookie; Secure needs https; Path and Domain match; expired cookies go.
    #[test]
    fn document_cookie_follows_browser_rules() {
        let jar = Jar::open(None);
        let page = url("https://www.shop.test/cart/view");
        jar.set_cookies(&mut [header("session=s1; HttpOnly; Path=/"), header("theme=dark; Path=/")].iter(), &page);
        assert_eq!(jar.document_cookie(&page), "theme=dark");
        // The network still sends the HttpOnly one.
        assert!(jar.cookies(&page).unwrap().to_str().unwrap().contains("session=s1"));

        assert!(!jar.set_document_cookie(&page, "session=stolen; Path=/"), "an HttpOnly cookie can't be overwritten");
        assert!(!jar.set_document_cookie(&page, "sneaky=1; HttpOnly"));
        assert!(jar.cookies(&page).unwrap().to_str().unwrap().contains("session=s1"));

        assert!(jar.set_document_cookie(&page, "cart=3"), "default path is the page's directory");
        assert!(jar.set_document_cookie(&page, "lang=en; Domain=shop.test; Path=/"));
        assert_eq!(jar.document_cookie(&page), "cart=3; lang=en; theme=dark");
        assert_eq!(jar.document_cookie(&url("https://shop.test/")), "lang=en");
        assert_eq!(jar.document_cookie(&url("https://other.test/")), "");

        let plain = url("http://www.shop.test/");
        assert!(!jar.set_document_cookie(&plain, "token=x; Secure"), "Secure needs https");
        assert!(jar.set_document_cookie(&page, "token=x; Secure; Path=/"));
        assert!(!jar.document_cookie(&plain).contains("token"), "and is only sent over https");

        assert!(!jar.set_document_cookie(&page, "wide=1; Domain=test"), "not for a public suffix");
        jar.set_document_cookie(&page, "theme=gone; Path=/; Max-Age=0");
        assert!(!jar.document_cookie(&page).contains("theme"), "an expired cookie is removed");
        assert_eq!(jar.document_cookie(&url("file:///tmp/a.html")), "");
    }

    /// Persistent cookies survive a restart; session cookies don't; the file is replaced whole.
    #[test]
    fn persistent_cookies_are_saved_and_session_cookies_are_not() {
        let (dir, jar) = temp_jar("persist");
        let page = url("https://keep.test/");
        jar.set_cookies(&mut [header("kept=1; Max-Age=3600"), header("session=2")].iter(), &page);
        jar.set_document_cookie(&page, "also=3; Expires=Wed, 01 Jan 2200 00:00:00 GMT");
        let reopened = Jar::open(Some(dir.join("cookies.json")));
        assert_eq!(reopened.document_cookie(&page), "also=3; kept=1");
        assert_eq!(reopened.count("keep.test"), 2);
        reopened.clear_site("keep.test");
        assert_eq!(Jar::open(Some(dir.join("cookies.json"))).count("keep.test"), 0);
        // A damaged file is kept aside, not overwritten.
        std::fs::write(dir.join("cookies.json"), "not json").unwrap();
        assert_eq!(Jar::open(Some(dir.join("cookies.json"))).count("keep.test"), 0);
        assert!(dir.join("cookies.json.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counting_and_clearing_are_per_site() {
        let jar = Jar::open(None);
        jar.set_cookies(&mut [header("a=1"), header("b=2; Domain=one.test")].iter(), &url("https://www.one.test/"));
        jar.set_cookies(&mut [header("c=3")].iter(), &url("https://two.test/"));
        assert_eq!((jar.count("one.test"), jar.count("two.test")), (2, 1));
        jar.clear_site("one.test");
        assert_eq!((jar.count("one.test"), jar.count("two.test")), (0, 1));
    }

    /// From http, nothing may shadow a Secure cookie of the same name (RFC 6265bis), by
    /// Set-Cookie or by page code; https can still replace it.
    #[test]
    fn an_insecure_url_cant_overwrite_a_secure_cookie() {
        let jar = Jar::open(None);
        let secure = url("https://bank.test/");
        jar.set_cookies(&mut [header("sid=good; Secure; Path=/")].iter(), &secure);
        let plain = url("http://bank.test/account/");
        jar.set_cookies(&mut [header("sid=evil; Path=/")].iter(), &plain);
        assert!(!jar.set_document_cookie(&plain, "sid=evil2; Path=/account"));
        assert!(!jar.set_document_cookie(&url("http://www.bank.test/"), "sid=evil3; Domain=bank.test; Path=/"));
        assert_eq!(jar.document_cookie(&secure), "sid=good");
        assert!(jar.set_document_cookie(&plain, "other=1; Path=/"), "other names are fine");
        assert!(jar.set_document_cookie(&secure, "sid=new; Secure; Path=/"));
        assert_eq!(jar.document_cookie(&secure), "other=1; sid=new");
    }

    /// Cookies over 4 KB are refused; past 180 for a domain (or 3000 in all) the oldest go.
    #[test]
    fn cookie_limits_evict_the_oldest() {
        let jar = Jar::open(None);
        let page = url("https://many.test/");
        assert!(!jar.set_document_cookie(&page, &format!("big={}", "x".repeat(MAX_COOKIE_BYTES))));
        for i in 0..MAX_PER_DOMAIN + 5 {
            assert!(jar.set_document_cookie(&page, &format!("c{i}=1")));
        }
        assert_eq!(jar.count("many.test"), MAX_PER_DOMAIN);
        let cookies = jar.document_cookie(&page);
        assert!(!cookies.contains("c0=") && !cookies.contains("c4=") && cookies.contains("c5=") && cookies.contains(&format!("c{}=", MAX_PER_DOMAIN + 4)));
    }

    /// Replacing a persistent cookie with a session one is saved: the old value doesn't come
    /// back after a restart.
    #[test]
    fn replacing_a_persistent_cookie_with_a_session_one_is_saved() {
        let (dir, jar) = temp_jar("replace");
        let page = url("https://token.test/");
        jar.set_cookies(&mut [header("token=old; Max-Age=3600; Path=/")].iter(), &page);
        jar.set_cookies(&mut [header("token=new; Path=/")].iter(), &page);
        assert_eq!(Jar::open(Some(dir.join("cookies.json"))).document_cookie(&page), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cross-site requests carry only what SameSite allows: a top-level GET navigation no
    /// Strict cookies, anything else neither Strict nor Lax ones.
    #[test]
    fn same_site_cookies_stay_home() {
        let jar = Jar::open(None);
        let site = url("https://shop.test/");
        jar.set_cookies(&mut [header("strict=1; SameSite=Strict"), header("lax=2; SameSite=Lax"), header("none=3; SameSite=None; Secure"), header("plain=4")].iter(), &site);
        let names = |scope| {
            let mut n: Vec<String> = jar.header(&site, scope).unwrap_or_default().split("; ").map(|c| c.split('=').next().unwrap().to_string()).collect();
            n.sort();
            n.join(",")
        };
        assert_eq!(names(CookieScope::SameSite), "lax,none,plain,strict");
        assert_eq!(names(CookieScope::CrossSiteNavigation), "lax,none,plain");
        assert_eq!(names(CookieScope::CrossSite), "none,plain");
        assert!(crate::gate::cross_site(&url("https://evil.test/"), &site));
        assert!(!crate::gate::cross_site(&url("https://www.shop.test/x"), &site));
        assert!(crate::gate::cross_site(&url("file:///tmp/a.html"), &site));
    }
}
