//! `tm acp` end to end, against the real compiled binary: an ACP client sends `initialize`
//! over newline-delimited JSON-RPC on the child's stdio, then closes stdin and expects the
//! process to exit cleanly. `TM_TEST_MOCK_PROVIDER=1` keeps this offline (no network), matching
//! `tests/mcp_serve.rs`'s isolation: a fresh tempdir project via `--project`, `TM_HOME` pointed
//! at another tempdir, never the primary checkout.
//!
//! `CARGO_BIN_EXE_tm` is only defined for integration test targets under `tests/`, not for unit
//! tests inside `src/`, which is why this test lives here rather than in `src/acp.rs` alongside
//! the fast, in-process `EofNotifyingReader` unit test.

use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// A fresh project directory the binary can open, as `tests/mcp_serve.rs` makes one.
fn init_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    tm_core::Store::open(tmp.path()).expect("open (and thereby create) a fresh project");
    tmp
}

struct AcpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl AcpChild {
    fn spawn(project: &std::path::Path, tm_home: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tm"))
            .arg("--project")
            .arg(project)
            .arg("acp")
            .current_dir(tm_home)
            .env("TM_HOME", tm_home)
            .env("TM_NOTIFY", "0")
            .env("TM_TEST_MOCK_PROVIDER", "1")
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn `tm acp`");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        AcpChild {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        let mut line = serde_json::to_vec(message).expect("serialize");
        line.push(b'\n');
        stdin.write_all(&line).expect("write to `tm acp`");
        stdin.flush().expect("flush");
    }

    fn recv(&mut self) -> Value {
        let line = match self.lines.recv_timeout(Duration::from_secs(30)) {
            Ok(line) => line,
            Err(e) => panic!("no reply from `tm acp` ({e}); stderr: {}", self.stderr()),
        };
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line:?}"))
    }

    fn stderr(&mut self) -> String {
        let _ = self.child.kill();
        let mut out = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            let _ = stderr.read_to_string(&mut out);
        }
        out
    }
}

impl Drop for AcpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn tm_acp_answers_initialize_over_stdio() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut acp = AcpChild::spawn(project.path(), tm_home.path());

    acp.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": {"name": "test-client", "version": "0"}
        }
    }));
    let reply = acp.recv();
    assert_eq!(reply["id"], 1, "{reply}");
    assert!(reply.get("error").is_none(), "initialize failed: {reply}");
    let result = &reply["result"];
    assert_eq!(result["protocolVersion"], 1, "{result}");
    assert_eq!(result["agentInfo"]["name"], "tm-acp", "{result}");

    // Closing stdin is how a client ends the session: the process exits on its own.
    drop(acp.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = acp.child.try_wait().expect("poll `tm acp`") {
            break status;
        }
        if Instant::now() > deadline {
            panic!(
                "`tm acp` did not exit after stdin closed; stderr: {}",
                acp.stderr()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        status.success(),
        "exit {status:?}; stderr: {}",
        acp.stderr()
    );
}
