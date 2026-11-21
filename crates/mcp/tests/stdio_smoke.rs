//! End-to-end over stdio against the mock backend: the wire shape an agent
//! host actually sees — tool list with annotations, structured content on
//! success, and a protocol error (not a tool result) for an unknown tool.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use serde_json::{Value, json};

struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Client {
    fn spawn() -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_colab-playground-mcp"))
            .arg("--mock")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout: ChildStdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Client {
            child,
            stdin,
            lines: rx,
            next_id: 0,
        }
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .expect("server did not answer in time");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message;
            }
        }
    }

    fn call_tool(&mut self, name: &str) -> Value {
        self.request("tools/call", json!({ "name": name }))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_mock_server_answers_the_v1_read_tools_over_stdio() {
    let mut client = Client::spawn();

    let init = client.request(
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "stdio-smoke", "version": "0" },
        }),
    );
    assert_eq!(
        init["result"]["serverInfo"]["name"],
        json!("colab-playground-mcp")
    );
    client.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    let list = client.request("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(
        names,
        [
            "colab_status",
            "accounts_list",
            "usage_get",
            "usage_refresh",
            "sessions_list"
        ]
    );
    let read_only = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == json!(name))
            .and_then(|t| t["annotations"]["readOnlyHint"].as_bool())
    };
    assert_eq!(read_only("colab_status"), Some(true));
    assert_eq!(read_only("accounts_list"), Some(true));
    assert_eq!(read_only("usage_get"), Some(true));
    // Refreshing fetches, and listing reconciles (it can write the ledger):
    // neither may claim to be read-only.
    assert_eq!(read_only("usage_refresh"), Some(false));
    assert_eq!(read_only("sessions_list"), Some(false));

    let status = client.call_tool("colab_status");
    assert_eq!(status["result"]["isError"], json!(false));
    assert_eq!(
        status["result"]["structuredContent"]["backend"],
        json!("mock")
    );
    assert!(status["result"]["structuredContent"]["probe"].is_object());

    let accounts = client.call_tool("accounts_list");
    assert_eq!(accounts["result"]["isError"], json!(false));
    let accounts = &accounts["result"]["structuredContent"];
    assert!(
        accounts["accounts"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert!(accounts["active"].is_string());

    let usage = client.call_tool("usage_get");
    assert_eq!(usage["result"]["isError"], json!(false));
    assert!(usage["result"]["structuredContent"]["accounts"].is_array());

    let refreshed = client.call_tool("usage_refresh");
    assert_eq!(refreshed["result"]["isError"], json!(false));
    assert!(refreshed["result"]["structuredContent"]["accounts"].is_array());

    let sessions = client.call_tool("sessions_list");
    assert_eq!(sessions["result"]["isError"], json!(false));
    assert!(sessions["result"]["structuredContent"].is_object());

    // An unknown tool is a protocol error, never a tool result.
    let bogus = client.call_tool("bogus");
    assert!(bogus.get("result").is_none());
    assert_eq!(bogus["error"]["code"], json!(-32601));
}
