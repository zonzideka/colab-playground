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
            "tunnels_list",
            "tunnels_open",
            "tunnels_close",
            "plugins_list",
            "plugins_run",
            "models_search",
            "models_files",
            "secrets_set",
            "secrets_has",
            "secrets_delete",
            "colab_update_check",
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
    assert_eq!(read_only("models_search"), Some(true));
    assert_eq!(read_only("models_files"), Some(true));
    assert_eq!(read_only("secrets_has"), Some(true));
    assert_eq!(read_only("colab_update_check"), Some(true));
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

#[test]
fn the_mock_server_covers_tunnels_plugins_models_and_secrets_over_stdio() {
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

    // Tunnels: open on a demo session of the active account, list, close.
    let opened = client.call_tool_args(
        "tunnels_open",
        json!({
            "account": "mock1",
            "session": "a100-7f3a",
            "local_port": 18099,
            "remote_port": 8099,
            "health_path": "/health",
        }),
    );
    assert_eq!(opened["result"]["isError"], json!(false), "{opened}");
    let opened = &opened["result"]["structuredContent"];
    let tunnel_id = opened["tunnel_id"].as_u64().unwrap();
    assert_eq!(opened["tunnel"]["state"]["state"], json!("up"));
    assert_eq!(opened["local_url"], json!("http://127.0.0.1:18099"));

    let listed = client.call_tool("tunnels_list");
    let tunnels = listed["result"]["structuredContent"]["tunnels"]
        .as_array()
        .unwrap();
    assert!(tunnels.iter().any(|t| t["id"] == json!(tunnel_id)));

    let closed = client.call_tool_args("tunnels_close", json!({ "tunnel_id": tunnel_id }));
    assert_eq!(closed["result"]["structuredContent"]["closed"], json!(true));
    let listed = client.call_tool("tunnels_list");
    let tunnels = listed["result"]["structuredContent"]["tunnels"]
        .as_array()
        .unwrap();
    assert!(tunnels.iter().all(|t| t["id"] != json!(tunnel_id)));

    // The reserved VM port is refused by the core validation.
    let reserved = client.call_tool_args(
        "tunnels_open",
        json!({
            "account": "mock1",
            "session": "a100-7f3a",
            "local_port": 18098,
            "remote_port": 8080,
        }),
    );
    assert_eq!(reserved["result"]["isError"], json!(true));
    assert_eq!(
        reserved["result"]["structuredContent"]["code"],
        json!("port_reserved")
    );

    // Plugins: the bundled llama.cpp deploy is listed and runs on an
    // existing session.
    let plugins = client.call_tool("plugins_list");
    let plugin_list = plugins["result"]["structuredContent"]["plugins"]
        .as_array()
        .unwrap();
    let llama = plugin_list
        .iter()
        .find(|p| p["manifest"]["id"] == json!("llama-cpp"))
        .expect("the bundled plugin is listed");
    assert_eq!(llama["enabled"], json!(true));

    // The plugin's model-repo input is required and has no default (the GUI
    // makes the user pick one), so the caller supplies it.
    let run = client.call_tool_args(
        "plugins_run",
        json!({
            "plugin": "llama-cpp",
            "session": "a100-7f3a",
            "inputs": { "repo": "unsloth/Qwen3-0.6B-GGUF" },
        }),
    );
    assert_eq!(run["result"]["isError"], json!(false), "{run}");
    let outcome = &run["result"]["structuredContent"];
    assert_eq!(outcome["session"], json!("a100-7f3a"));
    assert!(outcome["run_id"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(outcome["endpoints"].is_array());

    let gone = client.call_tool_args(
        "plugins_run",
        json!({
            "plugin": "llama-cpp",
            "session": "no-such-session",
            "inputs": { "repo": "unsloth/Qwen3-0.6B-GGUF" },
        }),
    );
    assert_eq!(gone["result"]["isError"], json!(true));
    assert_eq!(
        gone["result"]["structuredContent"]["code"],
        json!("session_gone")
    );

    // Models: search, then list the files of the first hit.
    let search = client.call_tool_args("models_search", json!({ "query": "qwen" }));
    assert_eq!(search["result"]["isError"], json!(false), "{search}");
    let models = search["result"]["structuredContent"]["models"]
        .as_array()
        .unwrap();
    assert!(!models.is_empty());
    let repo = models[0]["id"].as_str().unwrap();
    let files = client.call_tool_args("models_files", json!({ "repo": repo }));
    assert_eq!(files["result"]["isError"], json!(false), "{files}");
    assert!(
        files["result"]["structuredContent"]["variants"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
    );

    // Secrets: set is write-only (the value appears nowhere in the reply),
    // has is a boolean, delete removes it.
    let set = client.call_tool_args(
        "secrets_set",
        json!({ "name": "hf_token", "value": "smoke-secret-value" }),
    );
    assert_eq!(set["result"]["isError"], json!(false), "{set}");
    assert!(
        !set.to_string().contains("smoke-secret-value"),
        "the secret value must never be echoed"
    );
    let has = client.call_tool_args("secrets_has", json!({ "name": "hf_token" }));
    assert_eq!(has["result"]["structuredContent"]["present"], json!(true));
    let deleted = client.call_tool_args("secrets_delete", json!({ "name": "hf_token" }));
    assert_eq!(
        deleted["result"]["structuredContent"]["deleted"],
        json!(true)
    );
    let has = client.call_tool_args("secrets_has", json!({ "name": "hf_token" }));
    assert_eq!(has["result"]["structuredContent"]["present"], json!(false));

    // Update check is informational and reports the mock's status.
    let update = client.call_tool("colab_update_check");
    assert_eq!(update["result"]["isError"], json!(false), "{update}");
    assert_eq!(
        update["result"]["structuredContent"]["status"],
        json!("up_to_date")
    );
}
