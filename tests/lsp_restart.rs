//! Regression test for issue #47 (`:LspRestart` hung the old bacon-ls
//! forever). Simulates a client that completes `initialize` properly
//! but never responds to the server's follow-up
//! `workspace/configuration` request, then sends `shutdown` and `exit`.
//! Without the shutdown-watchdog fix, that keeps `Server::serve()`
//! alive indefinitely because the `initialized` future stays blocked
//! waiting on a response that will never arrive.

use std::io::{Read, Write};
use std::process::{ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_bacon-ls");

fn frame(body: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

/// Read LSP-framed messages off a child stdout, forwarding each JSON
/// body onto a channel. Returns when the pipe closes. Swallowing
/// errors is fine; the test decides success by polling the process.
fn spawn_reader(mut stdout: ChildStdout) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        'outer: loop {
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
                let body = String::from_utf8_lossy(&buf[body_start..body_start + len]).into_owned();
                if tx.send(body).is_err() {
                    return;
                }
                buf.drain(..body_start + len);
            }
            match stdout.read(&mut tmp) {
                Ok(0) | Err(_) => break 'outer,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
    });
    rx
}

fn wait_for<F: Fn(&str) -> bool>(rx: &mpsc::Receiver<String>, timeout: Duration, pred: F) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok(msg) if pred(&msg) => return Some(msg),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
    None
}

#[test]
fn unresponsive_client_still_exits_after_shutdown_and_exit() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    let mut child = Command::new(BIN)
        .current_dir(tempdir.path())
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bacon-ls");

    let stdout = child.stdout.take().expect("stdout");
    let rx = spawn_reader(stdout);
    let mut stdin = child.stdin.take().expect("stdin");

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":null,"rootUri":null,"capabilities":{"textDocument":{"publishDiagnostics":{"dataSupport":true,"relatedInformation":true}}}}}"#;
    stdin.write_all(&frame(init)).unwrap();
    stdin.flush().unwrap();

    // Wait for the initialize response so the server's state transitions to
    // Initialized — otherwise the Normal middleware in tower-lsp-server
    // rejects the notification and our handler never runs.
    wait_for(&rx, Duration::from_secs(5), |msg| msg.contains("\"id\":1")).expect("initialize response");

    let initialized = r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
    stdin.write_all(&frame(initialized)).unwrap();
    stdin.flush().unwrap();

    // Wait until the server fires `workspace/configuration`. At that point
    // the `initialized` future is parked on a response that our "broken"
    // client will never send — precisely the state that used to hang
    // `:LspRestart`.
    wait_for(&rx, Duration::from_secs(5), |msg| {
        msg.contains("workspace/configuration")
    })
    .expect("server should issue workspace/configuration");

    let shutdown = r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#;
    stdin.write_all(&frame(shutdown)).unwrap();
    stdin.flush().unwrap();

    wait_for(&rx, Duration::from_secs(5), |msg| msg.contains("\"id\":2")).expect("shutdown response");

    let start = Instant::now();
    let exit = r#"{"jsonrpc":"2.0","method":"exit"}"#;
    stdin.write_all(&frame(exit)).unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    // Watchdog fires ~500ms after shutdown; leave slack for CI.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let elapsed = start.elapsed();
                assert!(status.success(), "bacon-ls exited with {status}");
                assert!(
                    elapsed < Duration::from_secs(3),
                    "process took {elapsed:?} to exit — expected <3s"
                );
                return;
            }
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("bacon-ls did not exit within 5s after shutdown+exit — regression of #47");
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
}
