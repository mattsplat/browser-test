//! Per-site CSS: a user stylesheet for each site (origin), kept in the profile's settings and
//! applied to that site's pages after their own styles. No script runs; it's just CSS.
//!
//! The sheet hangs off a `<style>` element that is never put in the page's tree, so page
//! code can't find, read or remove it (`querySelector` only searches the tree), the
//! Inspector's tree doesn't show it, and it can't reach any other document: the browser UI
//! and other tabs are separate documents. Being an author sheet created after the page's own
//! were parsed, it comes after them in the cascade, so it wins ties without `!important`.

use blitz_dom::{Attribute, BaseDocument, LocalName, NodeId, QualName, local_name, ns};

use crate::settings::Settings;

/// The attribute that marks the detached `<style>` element carrying the site's CSS. Its name
/// is random per process, so a page can't make an element the browser mistakes for it (and
/// so receive, or read back, the user's CSS): the carrier is never in the tree, so no page
/// ever sees the name.
fn mark() -> &'static str {
    static MARK: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MARK.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        let random = std::collections::hash_map::RandomState::new().build_hasher().finish();
        format!("data-browser-site-css-{random:016x}")
    })
}

/// The CSS to apply on `origin`'s pages, if it has some and it's switched on.
pub fn for_origin<'a>(settings: &'a Settings, origin: &str) -> Option<&'a str> {
    settings.site_css.get(origin).filter(|s| s.enabled && !s.css.trim().is_empty()).map(|s| s.css.as_str())
}

/// Apply `css` to the page (None removes what was applied). Calling it again replaces the
/// sheet in place, so the Site panel can change a loaded page without a reload.
pub fn apply(doc: &mut BaseDocument, css: Option<&str>) {
    let existing = carrier(doc);
    let node = match (existing, css) {
        (Some(node), _) => node,
        (None, None) => return,
        (None, Some(_)) => {
            let mut m = doc.mutate();
            let attr = Attribute { name: QualName::new(None, ns!(), LocalName::from(mark())), value: String::new() };
            let style = m.create_element(QualName::new(None, ns!(html), local_name!("style")), vec![attr]);
            let text = m.create_text_node("");
            m.append_children(style, &[text]);
            style
        }
    };
    let text = doc.get_node(node).and_then(|n| n.children.first().copied());
    if let Some(text) = text {
        doc.mutate().set_node_text(text, css.unwrap_or(""));
    }
    // An author sheet, parsed from the element's text like a page's own `<style>`.
    doc.upsert_stylesheet_for_node(node);
    doc.shell_provider.request_redraw();
}

/// The detached element holding the site's sheet, if this page has one.
fn carrier(doc: &BaseDocument) -> Option<NodeId> {
    doc.stylesheet_owner_nodes().find(|&id| doc.get_node(id).is_some_and(|n| n.parent.is_none() && n.attr(LocalName::from(mark())).is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SiteCss;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    fn page() -> BaseDocument {
        use blitz_traits::shell::{ColorScheme, Viewport};
        let viewport = Some(Viewport::new(800, 600, 1.0, ColorScheme::Light));
        let html = "<html><head><style>p { color: rgb(255, 0, 0); }</style></head><body><p id=a>hi</p></body></html>";
        HtmlDocument::from_html(html, DocumentConfig { viewport, ..Default::default() }).into_inner()
    }

    fn color(doc: &mut BaseDocument) -> String {
        doc.resolve(0.0);
        let p = doc.query_selector("#a").unwrap().unwrap();
        doc.resolved_style_value(p, "color")
    }

    /// The site's CSS beats the page's own at equal specificity, can be replaced in place and
    /// removed, and the page can't see the element that carries it.
    #[test]
    fn site_css_applies_after_the_page_and_stays_out_of_its_dom() {
        let mut doc = page();
        assert!(color(&mut doc).contains("255, 0, 0"));
        apply(&mut doc, Some("p { color: rgb(0, 0, 255); }"));
        assert!(color(&mut doc).contains("0, 0, 255"), "{}", color(&mut doc));
        apply(&mut doc, Some("p { color: rgb(0, 128, 0); }"));
        assert!(color(&mut doc).contains("0, 128, 0"));
        assert_eq!(doc.stylesheet_owner_nodes().count(), 2, "replaced, not added again");
        // Not in the tree: queries (all a page's code can use) don't find it.
        assert_eq!(doc.query_selector_all("style").unwrap().len(), 1);
        assert!(crate::debug::outline(&doc).find(mark()).is_none());
        apply(&mut doc, None);
        assert!(color(&mut doc).contains("255, 0, 0"), "removed");
    }

    /// A detached `<style>` a page makes with a guess at the marker gets nothing.
    #[test]
    fn a_spoofed_carrier_gets_nothing() {
        let mut doc = page();
        let mut m = doc.mutate();
        let guess = Attribute { name: QualName::new(None, ns!(), LocalName::from("data-browser-site-css")), value: String::new() };
        let spoof = m.create_element(QualName::new(None, ns!(html), local_name!("style")), vec![guess]);
        let text = m.create_text_node("p { color: rgb(1, 2, 3); }");
        m.append_children(spoof, &[text]);
        drop(m);
        doc.upsert_stylesheet_for_node(spoof);
        apply(&mut doc, Some("p { color: rgb(0, 0, 255); }"));
        assert_eq!(doc.get_node(spoof).unwrap().text_content(), "p { color: rgb(1, 2, 3); }", "the user's CSS didn't reach it");
        assert!(color(&mut doc).contains("0, 0, 255"), "the real sheet still applies");
    }

    #[test]
    fn stored_per_origin_and_only_when_switched_on() {
        let mut s = Settings::default();
        s.site_css.insert("https://a.test".into(), SiteCss { css: "body { margin: 0 }".into(), enabled: true });
        s.site_css.insert("https://off.test".into(), SiteCss { css: "body { margin: 0 }".into(), enabled: false });
        assert_eq!(for_origin(&s, "https://a.test"), Some("body { margin: 0 }"));
        assert_eq!(for_origin(&s, "https://b.test"), None, "another site gets nothing");
        assert_eq!(for_origin(&s, "https://off.test"), None);
        // Saved with the profile's settings, and older settings files read without it.
        let round: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round.site_css.len(), 2);
        let old: Settings = serde_json::from_str(r#"{"dev_mode":true}"#).unwrap();
        assert!(old.site_css.is_empty());
    }
}
