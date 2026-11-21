//! End-to-end test against a real rust-analyzer. Run with:
//! `cargo test -p zenith-lsp -- --ignored`
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use zenith_lsp::{default_servers, LspClient, LspEvent, LspPos};

#[test]
#[ignore]
#[allow(clippy::collapsible_match)]
fn rust_analyzer_diagnostics_completion_definition() {
    let cfg = default_servers().into_iter().find(|s| s.name == "rust-analyzer").unwrap();
    if !cfg.is_installed() {
        eprintln!("rust-analyzer not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("zenith-lsp-e2e-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    let src = "fn helper() -> i32 { 1 }\nfn main() {\n    let v: Vec<i32> = Vec::new();\n    v.\n    let x: i32 = \"no\";\n    helper();\n}\n";
    let file = dir.join("src/main.rs");
    std::fs::write(&file, src).unwrap();

    let (tx, rx) = mpsc::channel();
    let mut client = LspClient::start(cfg, &dir, tx, Arc::new(|| {})).unwrap();
    client.did_open(&file, "rust", 1, src);

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut got_diag = false;
    let mut got_completion = false;
    let mut got_def = false;
    let mut completion_req = None;
    let mut def_req = None;
    while Instant::now() < deadline && !(got_diag && got_completion && got_def) {
        if completion_req.is_none() && !got_completion {
            completion_req = Some(client.completion(&file, LspPos { line: 3, character: 6 }, Some('.')));
        }
        if def_req.is_none() && !got_def {
            def_req = Some(client.definition(&file, LspPos { line: 5, character: 6 }));
        }
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(LspEvent::Diagnostics { diagnostics, .. }) if !diagnostics.is_empty() => got_diag = true,
            Ok(LspEvent::Completion { request, items }) if Some(request) == completion_req => {
                if items.iter().any(|i| i.label.starts_with("push")) {
                    got_completion = true;
                } else {
                    completion_req = None; // index not ready yet, retry
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            Ok(LspEvent::Definition { request, locations }) if Some(request) == def_req => {
                if locations.iter().any(|l| l.range.start.line == 0) {
                    got_def = true;
                } else {
                    def_req = None; // not indexed yet, retry
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            _ => {}
        }
    }
    client.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(got_diag, "no diagnostics");
    assert!(got_completion, "no completion");
    assert!(got_def, "no definition");
}
