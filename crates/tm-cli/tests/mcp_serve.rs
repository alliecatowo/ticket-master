//! `tm mcp` end to end, against the real compiled binary: what Claude Code does after `claude mcp
//! add --transport stdio tm -- tm mcp`. `initialize`, `tools/list`, then `tools/call
//! ticket_dispatch`, all as newline-delimited JSON-RPC over the child's stdio, then polling
//! `ticket_show` over the same connection until the in-process scheduler has picked the ticket
//! up. `TM_TEST_MOCK_PROVIDER=1` makes the worker's model a scripted mock, so no network.
//!
//! Every line the child writes to stdout is parsed as JSON-RPC: stdout is the protocol, and a
//! single stray `println!` anywhere on this path would break a real host.
//!
//! Same isolation as `tests/sched_run.rs`: a fresh tempdir project via `--project`, `TM_HOME`
//! pointed at another tempdir, never the primary checkout.

use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// A fresh project directory the binary can open, as `tests/sched_run.rs` makes one.
fn init_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    tm_core::Store::open(tmp.path()).expect("open (and thereby create) a fresh project");
    tmp
}

struct McpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next_id: i64,
}

impl McpChild {
    fn spawn(project: &std::path::Path, tm_home: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tm"))
            .arg("--project")
            .arg(project)
            .arg("mcp")
            .current_dir(project)
            .env("TM_HOME", tm_home)
            .env("TM_NOTIFY", "0")
            .env("TM_TEST_MOCK_PROVIDER", "1")
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn `tm mcp`");
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
        McpChild {
            child,
            stdin,
            lines,
            next_id: 1,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        let mut line = serde_json::to_vec(message).expect("serialize");
        line.push(b'\n');
        stdin.write_all(&line).expect("write to `tm mcp`");
        stdin.flush().expect("flush");
    }

    /// The next stdout line, which must be a JSON-RPC 2.0 message.
    fn recv(&mut self) -> Value {
        let line = match self.lines.recv_timeout(Duration::from_secs(30)) {
            Ok(line) => line,
            Err(e) => panic!("no reply from `tm mcp` ({e}); stderr: {}", self.stderr()),
        };
        let value: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line:?}"));
        assert_eq!(value["jsonrpc"], "2.0", "not JSON-RPC: {line}");
        value
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let reply = self.recv();
        assert_eq!(reply["id"], id, "reply to the wrong request: {reply}");
        assert!(reply.get("error").is_none(), "{method} failed: {reply}");
        reply["result"].clone()
    }

    /// A `tools/call` whose tool succeeded, returning its JSON payload.
    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert_eq!(result["isError"], false, "{name} failed: {result}");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("a text content block");
        serde_json::from_str(text).expect("the tool's text is JSON")
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

impl Drop for McpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is_valid_host_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[test]
fn tm_mcp_dispatches_a_ticket_that_the_in_process_scheduler_works() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut mcp = McpChild::spawn(project.path(), tm_home.path());

    let init = mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "claude-code", "version": "test"}
        }),
    );
    assert_eq!(init["serverInfo"]["name"], "tm-mcp");
    assert!(init["capabilities"]["tools"].is_object(), "{init}");
    mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let list = mcp.request("tools/list", json!({}));
    let names: Vec<String> = list["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("a name").to_string())
        .collect();
    assert!(names.contains(&"ticket_dispatch".to_string()), "{names:?}");
    assert!(names.contains(&"ticket_show".to_string()), "{names:?}");
    for name in &names {
        assert!(
            is_valid_host_tool_name(name),
            "{name} is not a valid tool name"
        );
        for human_only in ["accept", "reject", "retry"] {
            assert!(!name.contains(human_only), "{name} is a human-only action");
        }
    }

    let dispatched = mcp.call_tool(
        "ticket_dispatch",
        json!({"objective": "Write a haiku about schedulers into HAIKU.md"}),
    );
    assert_eq!(dispatched["state"], "ready", "{dispatched}");
    let id = dispatched["id"].as_str().expect("an id").to_string();

    // The ticket exists in the project (its defaults are checked in `tm-mcp`'s unit tests).
    let listed = mcp.call_tool("ticket_list", json!({}));
    assert!(
        listed["tickets"]
            .as_array()
            .expect("tickets")
            .iter()
            .any(|t| t["id"] == id.as_str()),
        "{listed}"
    );

    // The in-process scheduler (a 2s tick) leases it and runs an attempt against the mock.
    let deadline = Instant::now() + Duration::from_secs(45);
    let worked = loop {
        let ticket = mcp.call_tool("ticket_show", json!({"id": id}));
        let attempts = ticket["attempts"].as_u64().unwrap_or(0);
        let failures = ticket["failures"].as_array().map_or(0, Vec::len);
        if attempts > 0 || failures > 0 || ticket["state"] != "ready" {
            break ticket;
        }
        if Instant::now() > deadline {
            panic!(
                "ticket {id} was never picked up by the in-process scheduler: {ticket}; stderr: {}",
                mcp.stderr()
            );
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    assert_eq!(
        worked["objective"],
        "Write a haiku about schedulers into HAIKU.md"
    );

    // Closing stdin is how a host ends the session: the process exits on its own.
    drop(mcp.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = mcp.child.try_wait().expect("poll `tm mcp`") {
            break status;
        }
        if Instant::now() > deadline {
            panic!(
                "`tm mcp` did not exit after stdin closed; stderr: {}",
                mcp.stderr()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        status.success(),
        "exit {status:?}; stderr: {}",
        mcp.stderr()
    );

    // Anything else it wrote to stdout must still be JSON-RPC.
    while let Ok(line) = mcp.lines.recv_timeout(Duration::from_millis(200)) {
        let value: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line:?}"));
        assert_eq!(value["jsonrpc"], "2.0", "{line}");
    }
}
