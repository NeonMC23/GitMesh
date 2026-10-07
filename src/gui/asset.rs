//! Embedded front-end assets.
//!
//! The interface is three files: one page, one stylesheet, one script. They are
//! compiled into the binary, so `gitmesh gui` needs no asset directory, no build step
//! and no CDN — which also means the interface cannot silently start fetching something
//! from the internet.
//!
//! The pure part of the script sits between two markers and is executed by Node in
//! `tests/client.rs`, so the logic the interface relies on (progress folding, change
//! grouping, conflict guidance) is covered by `cargo test`.

/// The page.
pub const INDEX_HTML: &str = include_str!("static/index.html");
/// The stylesheet.
pub const APP_CSS: &str = include_str!("static/app.css");
/// The script (pure logic + DOM layer).
pub const APP_JS: &str = include_str!("static/app.js");

const LOGIC_START: &str = "/* ==== clientLogic:start ==== */";
const LOGIC_END: &str = "/* ==== clientLogic:end ==== */";

/// Everything between the client-logic markers: the part that has no DOM dependency
/// and is therefore unit-testable.
pub fn client_logic() -> &'static str {
    let start = APP_JS
        .find(LOGIC_START)
        .expect("the client logic markers must stay in app.js");
    let end = APP_JS
        .find(LOGIC_END)
        .expect("the client logic markers must stay in app.js");
    &APP_JS[start..end]
}

/// The JavaScript tests that run against [`client_logic`].
pub const CLIENT_TESTS: &str = include_str!("static/client.test.js");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_are_embedded_and_complete() {
        assert!(INDEX_HTML.contains("<!doctype html>"));
        assert!(INDEX_HTML.contains("id=\"workspace\""));
        assert!(INDEX_HTML.contains("id=\"welcome\""));
        assert!(APP_CSS.contains("--accent"));
        assert!(APP_JS.contains("clientLogic:start"));
        assert!(client_logic().contains("function progressRows"));
        assert!(CLIENT_TESTS.contains("assert("));
    }

    #[test]
    fn the_interface_loads_nothing_from_the_network() {
        // The preview environment and normal use both require self-contained assets.
        let html = INDEX_HTML.to_ascii_lowercase();
        assert!(
            !html.contains("http://"),
            "the page must not reference remote URLs"
        );
        assert!(
            !html.contains("https://"),
            "the page must not reference remote URLs"
        );
        assert!(!html.contains("cdn."), "no CDN");
        assert!(!html.contains("googleapis"), "no external fonts or APIs");
        let css = APP_CSS.to_ascii_lowercase();
        assert!(!css.contains("@import"), "no imported stylesheets");
        assert!(
            !css.contains("url(http"),
            "no remote assets in the stylesheet"
        );
        let js = APP_JS.to_ascii_lowercase();
        assert!(!js.contains("http://"), "no hard-coded remote endpoints");
        assert!(!js.contains("https://"), "no hard-coded remote endpoints");
        assert!(!js.contains("analytics"), "no analytics");
    }

    #[test]
    fn the_page_only_calls_local_endpoints() {
        // The page loads the two local assets...
        for asset in ["/app.css", "/app.js"] {
            assert!(INDEX_HTML.contains(asset), "the page should load {asset}");
        }
        // ...and the script only ever talks to its own server through /api.
        for endpoint in [
            "/api/model",
            "/api/refresh",
            "/api/open",
            "/api/commit",
            "/api/branch",
            "/api/sync",
            "/api/push",
            "/api/dry-run",
            "/api/events/",
        ] {
            assert!(
                APP_JS.contains(endpoint),
                "the script should use {endpoint}"
            );
        }
    }

    #[test]
    fn the_client_logic_has_no_dom_dependency() {
        let logic = client_logic();
        assert!(!logic.contains("document."));
        assert!(!logic.contains("window."));
        assert!(!logic.contains("fetch("));
    }
}
