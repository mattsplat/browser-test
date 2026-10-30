use std::path::{Path, PathBuf};

use blitz_traits::navigation::NavigationOptions;
use blitz_traits::net::{Method, Request, Url};
use dioxus_native::prelude::{Key, NamedKey};

pub fn req_from_string(url_s: &str) -> Option<Request> {
    // A path that exists opens as a file:// URL (a folder shows as one).
    if let Some(path) = local_path(url_s).filter(|p| p.exists()) {
        let url = if path.is_dir() { Url::from_directory_path(&path) } else { Url::from_file_path(&path) };
        if let Ok(url) = url {
            return Some(Request::get(url));
        }
    }
    if let Ok(url) = Url::parse(url_s) {
        return Some(Request::get(url));
    };

    let contains_space = url_s.contains(' ');
    let contains_dot = url_s.contains('.');
    if contains_dot && !contains_space {
        if let Ok(url) = Url::parse(&format!("https://{}", url_s)) {
            return Some(Request::get(url));
        }
    }

    Some(synthesize_duckduckgo_search_req(url_s))
}

fn synthesize_duckduckgo_search_req(query: &str) -> Request {
    let mut url = Url::parse("https://html.duckduckgo.com/html/").unwrap();
    url.query_pairs_mut().append_pair("q", query);

    NavigationOptions::new(
        url,
        Some(String::from("application/x-www-form-urlencoded")),
        0,
    )
    .set_method(Method::GET)
    .into_request()
}

pub fn open_in_external_browser(req: &Request) {
    if req.method == Method::GET && matches!(req.url.scheme(), "http" | "https" | "mailto") {
        if let Err(err) = webbrowser::open(req.url.as_str()) {
            tracing::error!("Failed to open URL: {}", err);
        }
    }
}

pub fn is_enter_key(key: &Key) -> bool {
    matches!(key, Key::Named(NamedKey::Enter)) || matches!(key, Key::Character(s) if s == "\n")
}

/// The file or folder the address bar's text names, if it's a path: `/...`, `~`, `~/...` or a
/// `file://` URL. Only what the user types reaches this; pages never see the file system.
pub fn local_path(input: &str) -> Option<PathBuf> {
    let input = input.trim();
    if let Some(rest) = input.strip_prefix('~') {
        if !rest.is_empty() && !rest.starts_with('/') {
            return None;
        }
        let home = PathBuf::from(std::env::var_os("HOME")?);
        return Some(home.join(rest.trim_start_matches('/')));
    }
    if input.starts_with('/') {
        return Some(PathBuf::from(input));
    }
    if input.starts_with("file://") {
        return Url::parse(input).ok()?.to_file_path().ok();
    }
    None
}

/// One file or folder Tab completion found: its name, and the address bar text that picks it.
#[derive(Clone, Debug, PartialEq)]
pub struct PathMatch {
    pub name: String,
    pub text: String,
    pub is_dir: bool,
}

/// What Tab does to the address bar's text: `text` is the new text (as far as every match
/// agrees, like a terminal), `matches` what it could be. None when the text isn't a path.
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    pub text: String,
    pub matches: Vec<PathMatch>,
}

/// The most entries of one folder Tab completion reads.
const MAX_LISTED: usize = 2000;

/// Complete the address bar's text as a path, as a terminal's Tab does. Matches keep the text's
/// form (`~/...`, `/...` or `file://...`). Hidden entries only match once the name starts with
/// a dot; a case-sensitive match wins over a case-insensitive one.
pub fn complete_path(input: &str) -> Option<Completion> {
    let input = input.trim();
    if input == "~" {
        return complete_path("~/");
    }
    let cut = input.rfind('/')? + 1;
    let (folder_text, typed) = input.split_at(cut);
    let folder = local_path(folder_text)?;
    let as_url = folder_text.starts_with("file://");
    // In a file:// URL the name is percent-encoded.
    let typed = if as_url { percent_decode(typed) } else { typed.to_string() };
    let text_for = |name: &str, is_dir: bool| -> String {
        if as_url {
            let path = folder.join(name);
            let url = if is_dir { Url::from_directory_path(&path) } else { Url::from_file_path(&path) };
            url.map(String::from).unwrap_or_default()
        } else {
            format!("{folder_text}{name}{}", if is_dir { "/" } else { "" })
        }
    };

    let entries: Vec<(String, bool)> = std::fs::read_dir(&folder)
        .map(|dir| dir.flatten().take(MAX_LISTED).map(|e| (e.file_name().to_string_lossy().into_owned(), is_dir(&e.path()))).collect())
        .unwrap_or_default();
    let visible = |name: &str| typed.starts_with('.') || !name.starts_with('.');
    let mut found: Vec<&(String, bool)> = entries.iter().filter(|(n, _)| visible(n) && n.starts_with(&typed)).collect();
    if found.is_empty() {
        let lower = typed.to_lowercase();
        found = entries.iter().filter(|(n, _)| visible(n) && n.to_lowercase().starts_with(&lower)).collect();
    }
    found.sort_by_key(|(n, _)| n.to_lowercase());
    let matches: Vec<PathMatch> = found.iter().map(|(name, dir)| PathMatch { name: name.clone(), text: text_for(name, *dir), is_dir: *dir }).collect();

    let text = match matches.as_slice() {
        [] => input.to_string(),
        [only] => only.text.clone(),
        many => {
            let common = common_prefix(many.iter().map(|m| m.name.as_str()));
            // A case-insensitive set may share less than was typed: keep what was typed.
            if common.len() > typed.len() { text_for(&common, false) } else { input.to_string() }
        }
    };
    Some(Completion { text, matches })
}

/// Follows symlinks, so a link to a folder completes as one.
fn is_dir(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_dir())
}

fn common_prefix<'a>(mut names: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = names.next() else { return String::new() };
    let mut common: &str = first;
    for name in names {
        let shared = common.char_indices().zip(name.chars()).take_while(|((_, a), b)| a == b).last().map_or(0, |((i, a), _)| i + a.len_utf8());
        common = &common[..shared];
    }
    common.to_string()
}

fn percent_decode(text: &str) -> String {
    let url = Url::parse(&format!("file:///{text}")).ok();
    url.and_then(|u| u.to_file_path().ok()).and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("urlbar-paths-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for folder in ["Documents", "Downloads", "Desktop", ".hidden", "My Files"] {
            std::fs::create_dir_all(dir.join(folder)).unwrap();
        }
        std::fs::write(dir.join("Documents/notes.txt"), "x").unwrap();
        std::fs::write(dir.join("readme.md"), "x").unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn existing_paths_open_as_file_urls() {
        let dir = scratch("open");
        let typed = format!("{}/readme.md", dir.display());
        assert_eq!(req_from_string(&typed).unwrap().url, Url::from_file_path(dir.join("readme.md")).unwrap());
        let folder = req_from_string(&format!("{}/Documents", dir.display())).unwrap().url;
        assert!(folder.as_str().starts_with("file://") && folder.as_str().ends_with("/Documents/"), "{folder}");
        // A path that doesn't exist is searched for, as before.
        assert_eq!(req_from_string("/no/such/place").unwrap().url.host_str(), Some("html.duckduckgo.com"));
        assert_eq!(local_path("~/x"), Some(PathBuf::from(std::env::var_os("HOME").unwrap()).join("x")));
        assert_eq!(local_path("~user/x"), None);
        assert_eq!(local_path("example.com"), None);
    }

    #[test]
    fn tab_completes_like_a_terminal() {
        let dir = scratch("tab");
        let at = |rest: &str| format!("{}/{rest}", dir.display());
        // One match: the whole name, with a slash for a folder.
        let one = complete_path(&at("Doc")).unwrap();
        assert_eq!(one.text, at("Documents/"));
        // Several: as far as they agree, and every match listed.
        let many = complete_path(&at("D")).unwrap();
        assert_eq!(many.text, at("D"));
        assert_eq!(many.matches.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["Desktop", "Documents", "Downloads"]);
        assert_eq!(complete_path(&at("Do")).unwrap().text, at("Do"));
        assert_eq!(complete_path(&at("Dow")).unwrap().text, at("Downloads/"));
        // Into a folder, case-insensitively when nothing matches exactly, files without a slash.
        assert_eq!(complete_path(&at("documents/n")).unwrap().text, at("documents/notes.txt"));
        assert_eq!(complete_path(&at("Documents/no")).unwrap().text, at("Documents/notes.txt"));
        // Hidden entries only once a dot is typed.
        assert!(!complete_path(&at("")).unwrap().matches.iter().any(|m| m.name.starts_with('.')));
        assert_eq!(complete_path(&at(".h")).unwrap().text, at(".hidden/"));
        // Nothing matches: the text stays.
        let none = complete_path(&at("zzz")).unwrap();
        assert_eq!((none.text, none.matches.len()), (at("zzz"), 0));
        // A file:// URL completes as one, percent-encoded.
        let url = Url::from_directory_path(&dir).unwrap();
        assert_eq!(complete_path(&format!("{url}My")).unwrap().text, Url::from_directory_path(dir.join("My Files")).unwrap().to_string());
        // Not a path: Tab isn't ours.
        assert_eq!(complete_path("example.com/do"), None);
    }
}
