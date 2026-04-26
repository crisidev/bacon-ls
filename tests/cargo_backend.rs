//! End-to-end integration tests for the cargo (native) backend. Spawns the
//! real `bacon-ls` binary against a tempdir Cargo project and drives a full
//! LSP session by writing framed JSON-RPC to its stdin / parsing its stdout.
//!
//! Linux-only: same rationale as `tests/lsp_restart.rs` — the harness reads
//! framed bytes off pipes whose timing/buffering varies enough on macOS and
//! Windows to add flake risk that has nothing to do with what's being tested.
//! The cargo backend logic itself is platform-agnostic and exercised by unit
//! tests on every CI runner.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_bacon-ls");

fn write_fixture(dir: &Path, lib_rs: &str) {
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.1\"\nedition = \"2021\"\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("lib.rs"), lib_rs).unwrap();
}

fn frame(body: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

fn send(stdin: &mut ChildStdin, msg: &Value) {
    let body = msg.to_string();
    stdin.write_all(&frame(&body)).unwrap();
    stdin.flush().unwrap();
}

fn spawn_reader(mut stdout: ChildStdout) -> mpsc::Receiver<Value> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            // Drain all complete frames currently in `buf`.
            while let Some(hdr_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = std::str::from_utf8(&buf[..hdr_end]).unwrap_or("");
                let len: usize = header
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let body_start = hdr_end + 4;
                if buf.len() < body_start + len {
                    break;
                }
                if let Ok(s) = std::str::from_utf8(&buf[body_start..body_start + len])
                    && let Ok(v) = serde_json::from_str::<Value>(s)
                    && tx.send(v).is_err()
                {
                    return;
                }
                buf.drain(..body_start + len);
            }
            match stdout.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
    });
    rx
}

/// Auto-respond to server-initiated requests so the server doesn't stall.
/// `workspace/configuration` is answered with a single empty object so the
/// server proceeds with cargo defaults; everything else (e.g.
/// `window/workDoneProgress/create`) gets a `null` result.
fn auto_respond(stdin: &mut ChildStdin, msg: &Value) {
    if msg.get("method").is_none() || msg.get("id").is_none() {
        return;
    }
    let id = &msg["id"];
    let method = msg["method"].as_str().unwrap_or("");
    let result = match method {
        "workspace/configuration" => json!([{}]),
        _ => Value::Null,
    };
    send(stdin, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

/// Read messages off `rx` until `pred` matches, auto-responding to every
/// server-initiated request along the way. Returns the matched message or
/// `None` on timeout.
fn pump<F>(rx: &mpsc::Receiver<Value>, stdin: &mut ChildStdin, timeout: Duration, mut pred: F) -> Option<Value>
where
    F: FnMut(&Value) -> bool,
{
    let deadline = Instant::now() + timeout;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let Ok(msg) = rx.recv_timeout(remaining) else {
            return None;
        };
        auto_respond(stdin, &msg);
        if pred(&msg) {
            return Some(msg);
        }
    }
    None
}

fn root_uri(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

fn spawn_server(workdir: &Path) -> Child {
    Command::new(BIN)
        .current_dir(workdir)
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bacon-ls")
}

fn initialize(stdin: &mut ChildStdin, rx: &mpsc::Receiver<Value>, workdir: &Path, related_info_support: bool) {
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": null,
            "rootUri": root_uri(workdir),
            "workspaceFolders": [{"uri": root_uri(workdir), "name": "fixture"}],
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {
                        "dataSupport": true,
                        "relatedInformation": related_info_support
                    }
                }
            }
        }
    });
    send(stdin, &init);
    pump(rx, stdin, Duration::from_secs(5), |m| m.get("id") == Some(&json!(1))).expect("initialize response");
    send(stdin, &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
}

fn shutdown_and_wait(stdin: &mut ChildStdin, rx: &mpsc::Receiver<Value>, child: &mut Child) {
    send(stdin, &json!({"jsonrpc": "2.0", "id": 999, "method": "shutdown"}));
    let _ = pump(rx, stdin, Duration::from_secs(5), |m| m.get("id") == Some(&json!(999)));
    send(stdin, &json!({"jsonrpc": "2.0", "method": "exit"}));
    // Best-effort wait so the harness doesn't leave zombies behind on
    // assertion failures.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Returns the diagnostics array if `msg` is a `publishDiagnostics`
/// notification for a URI containing `file_uri_substring` and the array is
/// non-empty.
fn diagnostics_for(msg: &Value, file_uri_substring: &str) -> Option<Vec<Value>> {
    if msg.get("method")?.as_str()? != "textDocument/publishDiagnostics" {
        return None;
    }
    let params = msg.get("params")?;
    let uri = params.get("uri")?.as_str()?;
    if !uri.contains(file_uri_substring) {
        return None;
    }
    let diags = params.get("diagnostics")?.as_array()?.clone();
    if diags.is_empty() {
        return None;
    }
    Some(diags)
}

#[test]
fn cargo_backend_publishes_error_diagnostic() {
    let tmp = TempDir::new().expect("tempdir");
    write_fixture(tmp.path(), "pub fn boom() { undefined_symbol; }\n");

    let mut child = spawn_server(tmp.path());
    let stdout = child.stdout.take().expect("stdout");
    let mut stdin = child.stdin.take().expect("stdin");
    let rx = spawn_reader(stdout);

    initialize(&mut stdin, &rx, tmp.path(), true);

    let msg = pump(&rx, &mut stdin, Duration::from_secs(60), |m| {
        diagnostics_for(m, "lib.rs").is_some()
    })
    .expect("publishDiagnostics for src/lib.rs");

    let diags = diagnostics_for(&msg, "lib.rs").unwrap();
    let has_error = diags.iter().any(|d| {
        let severity = d.get("severity").and_then(|s| s.as_i64());
        let message = d.get("message").and_then(|s| s.as_str()).unwrap_or("");
        let source = d.get("source").and_then(|s| s.as_str());
        severity == Some(1)
            && source == Some("bacon-ls")
            && (message.contains("undefined_symbol") || message.contains("cannot find"))
    });
    assert!(
        has_error,
        "expected an ERROR diagnostic mentioning the undefined symbol; got {diags:#?}"
    );

    shutdown_and_wait(&mut stdin, &rx, &mut child);
}

#[test]
fn cargo_backend_code_action_replaces_unused_variable() {
    let tmp = TempDir::new().expect("tempdir");
    // Reading the binding once silences the "unused variable" hint into a
    // pure unused-variable warning whose help-child carries the
    // MachineApplicable replacement we want to surface as a QuickFix.
    write_fixture(
        tmp.path(),
        "pub fn warn_me() -> i32 { let unused_var = 42; 0 }\n",
    );

    let mut child = spawn_server(tmp.path());
    let stdout = child.stdout.take().expect("stdout");
    let mut stdin = child.stdin.take().expect("stdin");
    let rx = spawn_reader(stdout);

    // related_info_support=false forces the server to emit the help-child
    // span as its own diagnostic with a `data` payload (corrections),
    // which is what powers the QuickFix code action.
    initialize(&mut stdin, &rx, tmp.path(), false);

    let msg = pump(&rx, &mut stdin, Duration::from_secs(60), |m| {
        let Some(diags) = diagnostics_for(m, "lib.rs") else {
            return false;
        };
        diags.iter().any(|d| d.get("data").is_some())
    })
    .expect("publishDiagnostics with `data` for src/lib.rs");

    let params = msg.get("params").unwrap();
    let uri = params.get("uri").unwrap().as_str().unwrap().to_string();
    let diags = params.get("diagnostics").unwrap().as_array().unwrap().clone();
    let target = diags
        .iter()
        .find(|d| d.get("data").is_some())
        .expect("a diagnostic with corrections must be present");
    let range = target.get("range").unwrap().clone();

    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/codeAction",
        "params": {
            "textDocument": {"uri": uri},
            "range": range,
            "context": {"diagnostics": [target]}
        }
    });
    send(&mut stdin, &req);

    let resp = pump(&rx, &mut stdin, Duration::from_secs(10), |m| {
        m.get("id") == Some(&json!(2))
    })
    .expect("codeAction response");
    let actions = resp
        .get("result")
        .and_then(|r| r.as_array())
        .expect("codeAction result must be an array");
    assert!(!actions.is_empty(), "code action list must be non-empty");
    let titles: Vec<&str> = actions.iter().filter_map(|a| a.get("title")?.as_str()).collect();
    assert!(
        titles.iter().any(|t| t.starts_with("Replace with: _")),
        "expected a 'Replace with: _<name>' QuickFix; got {titles:?}"
    );

    // Confirm the action carries an actual workspace edit pointing at our URI.
    let action_with_edit = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()).is_some_and(|t| t.starts_with("Replace with: _")))
        .unwrap();
    let edit = action_with_edit
        .get("edit")
        .and_then(|e| e.get("changes"))
        .and_then(|c| c.as_object())
        .expect("workspace edit with changes");
    assert!(edit.contains_key(&uri), "edit must target the diagnostic's URI");

    shutdown_and_wait(&mut stdin, &rx, &mut child);
}
