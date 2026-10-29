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

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use blitz_traits::net::Url;
use cookie_store::{Cookie, CookieStore, RawCookie};

pub struct Jar {
    store: Mutex<CookieStore>,
    /// Where persistent cookies are saved; None keeps them in memory only.
    path: Option<PathBuf>,
}

/// The profile's jar, loaded from its folder on first use.
pub fn jar() -> Arc<Jar> {
    static JAR: OnceLock<Arc<Jar>> = OnceLock::new();
    JAR.get_or_init(|| Arc::new(Jar::open(crate::profile::dir().map(|d| d.join("cookies.json"))))).clone()
}

/// Give the network stack the profile's jar, so requests and `document.cookie` share it. Call
/// before the first network provider is created (later calls change nothing).
pub fn install() {
    blitz_net::set_cookie_store(jar());
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
        Jar { store: Mutex::new(store), path }
    }

    /// Save the persistent, unexpired cookies.
    fn save(&self, store: &CookieStore) {
        let Some(path) = &self.path else { return };
        let mut bytes = Vec::new();
        let saved = cookie_store::serde::json::save(store, &mut bytes)
            .map_err(|e| std::io::Error::other(e.to_string()))
            .and_then(|_| path.parent().map(std::fs::create_dir_all).transpose())
            .and_then(|_| crate::profile::write_atomic(path, &bytes));
        if let Err(e) = saved {
            tracing::error!("could not save cookies to {}: {e}", path.display());
        }
    }

    /// Store `raw`, received from `url`, if the rules allow it. `from_page` means
    /// `document.cookie` rather than a response header. None if it was refused; otherwise
    /// whether it changes what's saved on disk (a cookie with an expiry, including one that
    /// deletes a cookie).
    fn store(&self, store: &mut CookieStore, raw: &RawCookie<'_>, url: &Url, from_page: bool) -> Option<bool> {
        if raw.secure() == Some(true) && url.scheme() != "https" {
            return None;
        }
        // No cookies for a whole public suffix (`Domain=co.uk`), unless that's the host itself.
        if let Some(domain) = raw.domain().map(|d| d.trim_start_matches('.').to_ascii_lowercase()) {
            if psl::suffix_str(&domain) == Some(domain.as_str()) && url.host_str() != Some(domain.as_str()) {
                return None;
            }
        }
        if from_page {
            if raw.http_only() == Some(true) {
                return None;
            }
            // Page code can't overwrite (or delete) an HttpOnly cookie either.
            if let Ok(cookie) = Cookie::try_from_raw_cookie(raw, url) {
                if let Some(domain) = cookie.domain.as_cow() {
                    if store.get_any(&domain, cookie.path.as_ref(), cookie.name()).is_some_and(|old| old.http_only() == Some(true)) {
                        return None;
                    }
                }
            }
        }
        let saved = raw.max_age().is_some() || raw.expires().is_some_and(|e| e.is_datetime());
        store.insert_raw(raw, url).ok().map(|_| saved)
    }

    /// `document.cookie` for a page at `page`: the cookies it would send there, without
    /// `HttpOnly` ones, longest path first (as browsers order them), as `name=value; ...`.
    pub fn document_cookie(&self, page: &Url) -> String {
        if !matches!(page.scheme(), "http" | "https") {
            return String::new();
        }
        let store = self.store.lock().unwrap();
        let mut cookies: Vec<&Cookie<'static>> = store.matches(page).into_iter().filter(|c| c.http_only() != Some(true)).collect();
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
        let mut store = self.store.lock().unwrap();
        let Some(saved) = self.store(&mut store, &raw, page, true) else { return false };
        if saved {
            self.save(&store);
        }
        true
    }

    /// How many unexpired cookies belong to `site` (a registrable domain, as `gate::site_of`).
    pub fn count(&self, site: &str) -> usize {
        self.store.lock().unwrap().iter_unexpired().filter(|c| cookie_site(c) == site).count()
    }

    /// Remove every cookie of `site`.
    pub fn clear_site(&self, site: &str) {
        let mut store = self.store.lock().unwrap();
        let doomed: Vec<(String, String, String)> = store
            .iter_any()
            .filter(|c| cookie_site(c) == site)
            .filter_map(|c| Some((c.domain.as_cow()?.into_owned(), c.path.as_ref().to_string(), c.name().to_string())))
            .collect();
        for (domain, path, name) in &doomed {
            store.remove(domain, path, name);
        }
        self.save(&store);
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

/// The network side: reqwest asks for a request's `Cookie` header and hands over each
/// response's `Set-Cookie` headers.
impl blitz_net::CookieStore for Jar {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &blitz_net::HeaderValue>, url: &Url) {
        let mut store = self.store.lock().unwrap();
        let mut persistent = false;
        for header in headers {
            let Ok(text) = std::str::from_utf8(header.as_bytes()) else { continue };
            if let Ok(raw) = RawCookie::parse(text.to_string()) {
                persistent |= self.store(&mut store, &raw, url, false) == Some(true);
            }
        }
        if persistent {
            self.save(&store);
        }
    }

    fn cookies(&self, url: &Url) -> Option<blitz_net::HeaderValue> {
        let store = self.store.lock().unwrap();
        let header = store.get_request_values(url).map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("; ");
        if header.is_empty() { None } else { blitz_net::HeaderValue::from_str(&header).ok() }
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
}
