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

    fn call_tool_args(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
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
            "sessions_list",
            "sessions_start",
            "operations_get",
            "operations_cancel",
            "sessions_stop",
            "sessions_extend",
            "guard_poll",
        ]
    );
    let read_only = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == json!(name))
            .and_then(|t| t["annotations"]["readOnlyHint"].as_bool())
    };
    assert_eq!(read_only("colab_status"), Some(true));
    assert_eq!(read_only("usage_get"), Some(true));
    assert_eq!(read_only("operations_get"), Some(true));
    // Refreshing fetches, listing reconciles (it can write the ledger), and
    // the account query can complete an interrupted add-account rollback:
    // none of them may claim to be read-only.
    assert_eq!(read_only("accounts_list"), Some(false));
    assert_eq!(read_only("usage_refresh"), Some(false));
    assert_eq!(read_only("sessions_list"), Some(false));
    assert_eq!(read_only("sessions_start"), Some(false));
    assert_eq!(read_only("sessions_stop"), Some(false));
    let stop_tool = tools
        .iter()
        .find(|t| t["name"] == json!("sessions_stop"))
        .unwrap();
    assert_eq!(
        stop_tool["annotations"]["destructiveHint"],
        json!(true),
        "stopping a session is destructive"
    );

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

#[test]
fn the_mock_server_runs_a_session_lifecycle_over_stdio() {
    let mut client = Client::spawn();
    client.request(
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "stdio-smoke", "version": "0" },
        }),
    );
    client.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    // The demo scenario fills the active account's session cap (2 managed
    // runtimes); stop one to make room for the session this test starts.
    let listed = client.call_tool("sessions_list");
    let demo = listed["result"]["structuredContent"]["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"]["name"] == json!("t4-91c2"))
        .unwrap()
        .clone();
    let freed = client.call_tool_args(
        "sessions_stop",
        json!({
            "account": demo["id"]["account"],
            "endpoint": demo["id"]["endpoint"],
            "name": demo["id"]["name"],
        }),
    );
    assert_eq!(
        freed["result"]["structuredContent"]["stopped"],
        json!(true),
        "{freed}"
    );

    // Guard validation happens before anything is provisioned: a host-side
    // policy field is refused by name, and a missing cap is not defaulted.
    let refused = client.call_tool_args(
        "sessions_start",
        json!({
            "request_id": "smoke-refused",
            "policy": { "max_runtime_secs": 900, "max_spend_cu": 5 },
        }),
    );
    assert_eq!(refused["result"]["isError"], json!(true));
    assert_eq!(
        refused["result"]["structuredContent"]["code"],
        json!("invalid_input")
    );
    assert_eq!(
        refused["result"]["structuredContent"]["params"]["field"],
        json!("max_spend_cu")
    );
    let no_cap = client.call_tool_args(
        "sessions_start",
        json!({ "request_id": "smoke-nocap", "policy": {} }),
    );
    assert_eq!(no_cap["result"]["isError"], json!(true));
    assert_eq!(
        no_cap["result"]["structuredContent"]["params"]["field"],
        json!("policy.max_runtime_secs")
    );

    // Provisioning runs on the active account only.
    let wrong_account = client.call_tool_args(
        "sessions_start",
        json!({
            "request_id": "smoke-wrong-account",
            "account": "zz9",
            "policy": { "max_runtime_secs": 900 },
        }),
    );
    assert_eq!(wrong_account["result"]["isError"], json!(true));
    assert_eq!(
        wrong_account["result"]["structuredContent"]["code"],
        json!("account_not_active")
    );

    // A real start: operation at once, then poll to the terminal state.
    let start = client.call_tool_args(
        "sessions_start",
        json!({
            "request_id": "smoke-1",
            "accelerator": "CPU",
            "policy": { "max_runtime_secs": 900 },
        }),
    );
    assert_eq!(start["result"]["isError"], json!(false), "{start}");
    let started = &start["result"]["structuredContent"];
    let operation_id = started["operation_id"].as_str().unwrap().to_string();
    assert!(
        started["state"] == json!("running") || started["state"] == json!("succeeded"),
        "{started}"
    );

    // The same request_id returns the same operation, never a second one.
    let retry = client.call_tool_args(
        "sessions_start",
        json!({
            "request_id": "smoke-1",
            "accelerator": "CPU",
            "policy": { "max_runtime_secs": 900 },
        }),
    );
    assert_eq!(
        retry["result"]["structuredContent"]["operation_id"],
        json!(operation_id)
    );

    let mut view = start["result"]["structuredContent"].clone();
    for _ in 0..60 {
        if view["state"] != json!("running") {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
        let got = client.call_tool_args("operations_get", json!({ "operation_id": operation_id }));
        assert_eq!(got["result"]["isError"], json!(false));
        view = got["result"]["structuredContent"].clone();
    }
    assert_eq!(view["state"], json!("succeeded"), "{view}");
    let session = &view["result"]["session"];
    let (account, endpoint, name) = (
        session["account"].as_str().unwrap().to_string(),
        session["endpoint"].as_str().unwrap().to_string(),
        session["name"].as_str().unwrap().to_string(),
    );
    assert!(!endpoint.is_empty());
    assert_eq!(
        view["result"]["policy"]["watcher_mode"],
        json!("vm_watchdog_only")
    );
    assert_eq!(view["result"]["watchdog"]["armed"], json!(true));

    // Cancelling a finished operation leaves its state alone.
    let cancel =
        client.call_tool_args("operations_cancel", json!({ "operation_id": operation_id }));
    assert_eq!(
        cancel["result"]["structuredContent"]["state"],
        json!("succeeded")
    );

    // An unknown operation is a structured error, not a protocol error.
    let unknown = client.call_tool_args("operations_get", json!({ "operation_id": "op-999" }));
    assert_eq!(unknown["result"]["isError"], json!(true));
    assert_eq!(
        unknown["result"]["structuredContent"]["code"],
        json!("invalid_input")
    );

    let extended = client.call_tool_args(
        "sessions_extend",
        json!({
            "account": account,
            "endpoint": endpoint,
            "name": name,
            "additional_secs": 600,
        }),
    );
    assert_eq!(extended["result"]["isError"], json!(false), "{extended}");
    assert_eq!(
        extended["result"]["structuredContent"]["watchdog"]["armed"],
        json!(true)
    );
    // The extension adds to the time left: the new deadline is later than
    // the one provisioning set (900 s cap + 600 s added, roughly).
    let started_deadline = view["result"]["watchdog"]["deadline"].as_u64().unwrap();
    let extended_deadline = extended["result"]["structuredContent"]["watchdog"]["deadline"]
        .as_u64()
        .unwrap();
    assert!(
        extended_deadline > started_deadline,
        "extend moved the deadline earlier: {started_deadline} -> {extended_deadline}"
    );

    let guard = client.call_tool("guard_poll");
    assert_eq!(guard["result"]["isError"], json!(false));
    assert_eq!(
        guard["result"]["structuredContent"]["watcher_mode"],
        json!("vm_watchdog_only")
    );
    assert!(guard["result"]["structuredContent"]["report"].is_object());

    let stopped = client.call_tool_args(
        "sessions_stop",
        json!({
            "account": account,
            "endpoint": endpoint,
            "name": name,
        }),
    );
    assert_eq!(stopped["result"]["isError"], json!(false), "{stopped}");
    assert_eq!(
        stopped["result"]["structuredContent"]["stopped"],
        json!(true)
    );

    let sessions = client.call_tool("sessions_list");
    let runtimes = sessions["result"]["structuredContent"]["runtimes"]
        .as_array()
        .unwrap();
    assert!(
        runtimes.iter().all(|r| r["id"]["name"] != json!(name)),
        "the stopped session is gone from the listing"
    );
}
