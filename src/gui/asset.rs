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
        assert!(
            client_logic().contains("function managementPlanSummary"),
            "the repositories panel reads the plan the service built"
        );
        assert!(
            client_logic().contains("function repositoryRows"),
            "the repositories panel words the inspection it is given"
        );
        assert!(CLIENT_TESTS.contains("assert("));
    }

    /// Every element the script looks up must exist in the page, or be created by the
    /// script itself — the list below is exactly that second case.
    #[test]
    fn every_element_the_script_uses_exists() {
        const CREATED_BY_THE_SCRIPT: [&str; 4] = [
            "btn-setup-open-existing",
            "setup-confirm-remotes",
            "setup-confirm-remotes-plan",
            "setup-overwrite",
        ];
        let mut checked = 0;
        let mut rest = APP_JS;
        while let Some(start) = rest.find("$('") {
            rest = &rest[start + 3..];
            let end = rest.find('\'').expect("a closing quote");
            let id = &rest[..end];
            rest = &rest[end + 1..];
            // `$('x' + y)` builds an id at run time: nothing static to check.
            if rest.starts_with(" + ") || rest.starts_with('+') {
                continue;
            }
            // Skips ids that are built at run time (`$('panel-' + name)`) and literals the
            // script looks up somewhere else.
            if id.contains(' ') || id.contains('+') || id.is_empty() {
                continue;
            }
            checked += 1;
            if CREATED_BY_THE_SCRIPT.contains(&id) {
                continue;
            }
            assert!(
                INDEX_HTML.contains(&format!("id=\"{id}\"")),
                "the script looks up #{id}, which the page does not define"
            );
        }
        assert!(
            checked > 50,
            "the check itself must stay meaningful: {checked}"
        );
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
            "/api/setup/inspect",
            "/api/setup/plan",
            "/api/setup/apply",
            "/api/repositories",
            "/api/repository/inspect",
            "/api/repository/plan",
            "/api/repository/apply",
        ] {
            assert!(
                APP_JS.contains(endpoint),
                "the script should use {endpoint}"
            );
        }
    }

    /// A handler the page wires up has to exist: an undefined function is a click that
    /// throws. (This is exactly the kind of bug a "write the helper later" leaves behind,
    /// and the browser console is not part of `cargo test`.)
    #[test]
    fn every_wired_handler_is_defined() {
        let mut checked = 0;
        let mut rest = APP_JS;
        while let Some(start) = rest.find("addEventListener(") {
            rest = &rest[start + "addEventListener(".len()..];
            let Some(comma) = rest.find(',') else { break };
            let after = rest[comma + 1..].trim_start();
            // The handler is either an inline function or the name of one.
            let name: String = after
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '$')
                .collect();
            rest = after;
            if name.is_empty() || name == "function" {
                continue; // an inline function, an arrow function, or a variable.
            }
            checked += 1;
            assert!(
                APP_JS.contains(&format!("function {name}("))
                    || APP_JS.contains(&format!("var {name} = function")),
                "the page calls {name} as an event handler, but the script never defines it"
            );
        }
        assert!(
            checked >= 12,
            "the check itself must stay meaningful: {checked}"
        );
    }

    /// The wizard keeps its answers in one object; a key that is written but never
    /// declared is a typo waiting to throw at the worst moment.
    #[test]
    fn the_wizard_state_only_uses_declared_keys() {
        let start = APP_JS.find("var wizard = {").expect("the wizard state");
        let end = APP_JS[start..]
            .find("\n    };")
            .map(|offset| start + offset)
            .expect("the end of the wizard state");
        let declaration = &APP_JS[start..end];
        let mut keys: Vec<String> = Vec::new();
        for line in declaration.lines().skip(1) {
            let trimmed = line.trim();
            if let Some((key, _)) = trimmed.split_once(':') {
                let key = key.trim();
                if !key.is_empty()
                    && key
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                {
                    keys.push(key.to_string());
                }
            }
        }
        assert!(keys.len() >= 5, "the wizard state lost its keys: {keys:?}");

        let mut checked = 0;
        let mut rest = &APP_JS[end..];
        while let Some(start) = rest.find("wizard.") {
            rest = &rest[start + "wizard.".len()..];
            let key: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            if key.is_empty() {
                continue;
            }
            checked += 1;
            assert!(
                keys.iter().any(|known| known == &key),
                "the script uses wizard.{key}, which the wizard state does not declare \
                 (declared: {keys:?})"
            );
        }
        assert!(
            checked > 10,
            "the check itself must stay meaningful: {checked}"
        );
    }

    #[test]
    fn the_client_logic_has_no_dom_dependency() {
        let logic = client_logic();
        assert!(!logic.contains("document."));
        assert!(!logic.contains("window."));
        assert!(!logic.contains("fetch("));
    }
}
