//! Tests for the pure client-side logic of the GUI.
//!
//! The interface is deliberately front-end-light: the browser script only renders the
//! model the Rust core produces, and everything that decides *what* is shown (progress
//! folding, change grouping, conflict guidance, commit readiness) lives in a marked
//! region of `src/gui/static/app.js` that has no DOM dependency.
//!
//! This test extracts that region, concatenates it with `client.test.js` and runs it
//! with Node. That way the browser code and the tested code are the same text, and the
//! behaviour the interface depends on is covered by `cargo test`.
//!
//! When Node is not installed the test is skipped with a notice instead of failing: the
//! GUI itself has no Node dependency, so a Rust-only environment must still be able to
//! run the whole suite.

use std::path::PathBuf;
use std::process::Command;

/// Locate a `node` binary, if there is one.
fn node() -> Option<PathBuf> {
    for candidate in ["node", "nodejs"] {
        let probe = Command::new(candidate).arg("--version").output();
        if let Ok(output) = probe {
            if output.status.success() {
                return Some(PathBuf::from(candidate));
            }
        }
    }
    None
}

#[test]
fn client_logic_passes_its_javascript_tests() {
    let Some(node) = node() else {
        eprintln!("skipping: node is not installed, the GUI client logic was not executed");
        return;
    };
    let script = gitmesh::gui::asset::client_logic().to_string();
    let tests = gitmesh::gui::asset::CLIENT_TESTS;
    let source = format!("{script}\n{tests}\n");

    let temp = std::env::temp_dir().join(format!("gitmesh-client-{}", std::process::id()));
    std::fs::create_dir_all(&temp).expect("create temp directory");
    let file = temp.join("client.test.js");
    std::fs::write(&file, source).expect("write the generated test file");

    let output = Command::new(&node)
        .arg(&file)
        .output()
        .expect("run node on the client tests");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the client-side logic tests failed:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("assertions passed"),
        "the client tests should report their assertions:\n{stdout}"
    );
    eprintln!("{}", stdout.trim());
    let _ = std::fs::remove_dir_all(&temp);
}

#[test]
fn the_client_logic_region_is_well_formed() {
    // A marker typo would silently disable the tests above: fail loudly instead.
    let logic = gitmesh::gui::asset::client_logic();
    assert!(
        logic.contains("var GitMesh = (function ()"),
        "the logic is a single module"
    );
    assert!(
        logic.contains("progressRows"),
        "the progress folding is tested"
    );
    assert!(!logic.trim().is_empty(), "the region is not empty");
    assert!(
        logic.len() < gitmesh::gui::asset::APP_JS.len(),
        "the region is a part of app.js, not the whole file"
    );
}
