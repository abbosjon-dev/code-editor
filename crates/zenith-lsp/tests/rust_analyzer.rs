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

#[test]
#[ignore]
fn rust_analyzer_references_rename_formatting() {
    use zenith_lsp::apply_text_edits;
    let cfg = default_servers().into_iter().find(|s| s.name == "rust-analyzer").unwrap();
    if !cfg.is_installed() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("zenith-lsp-e2e2-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo2\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    let src = "fn helper() -> i32 { 1 }\nfn main() {\n    let a = helper();\n  let b=helper() ;\n    println!(\"{}\", a + b);\n}\n";
    let file = dir.join("src/main.rs");
    std::fs::write(&file, src).unwrap();
    let (tx, rx) = mpsc::channel();
    let mut client = LspClient::start(cfg, &dir, tx, Arc::new(|| {})).unwrap();
    client.did_open(&file, "rust", 1, src);

    let deadline = Instant::now() + Duration::from_secs(120);
    let decl = LspPos { line: 0, character: 4 };
    let (mut refs, mut renamed, mut formatted) = (None, None, None);
    let (mut rq, mut nq, mut fq) = (None, None, None);
    while Instant::now() < deadline && (refs.is_none() || renamed.is_none() || formatted.is_none()) {
        if rq.is_none() && refs.is_none() {
            rq = Some(client.references(&file, decl));
        }
        if nq.is_none() && renamed.is_none() {
            nq = Some(client.rename(&file, decl, "yordamchi"));
        }
        if fq.is_none() && formatted.is_none() {
            fq = Some(client.formatting(&file, 4, true));
        }
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(LspEvent::References { request, locations }) if Some(request) == rq => {
                if locations.len() >= 3 {
                    refs = Some(locations.len());
                } else {
                    rq = None;
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            Ok(LspEvent::WorkspaceEdit { request, edits, .. }) if Some(request) == nq => {
                match edits.iter().find(|(p, _)| p.ends_with("main.rs")) {
                    Some((_, e)) if !e.is_empty() => renamed = Some(apply_text_edits(src, e)),
                    _ => {
                        nq = None;
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
            Ok(LspEvent::Formatting { request, edits }) if Some(request) == fq => {
                if edits.is_empty() {
                    fq = None;
                    std::thread::sleep(Duration::from_millis(500));
                } else {
                    formatted = Some(apply_text_edits(src, &edits));
                }
            }
            _ => {}
        }
    }
    client.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(refs, Some(3), "references");
    let renamed = renamed.expect("rename");
    assert_eq!(renamed.matches("yordamchi").count(), 3, "{renamed}");
    assert!(!renamed.contains("helper"));
    let formatted = formatted.expect("formatting");
    assert!(formatted.contains("    let b = helper();"), "{formatted}");
}
