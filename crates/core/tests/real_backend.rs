//! `RealBackend` against the fake Colab toolchain (`tests/fake/`), unix only.
//!
//! Every test builds an isolated world in a temporary directory: the fake
//! `colab`, the fake CLI interpreter (which runs the real
//! `account_helper.py` with fake keyring/google-auth modules) and a fake
//! `ssh`, all given by absolute path; `HOME` and the data directories point
//! into the temp dir. No real `colab`, keyring or network is ever used.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use colab_playground_core::accounts::{Account, AccountId, Helper};
use colab_playground_core::backend::{
    Backend, PluginRunEvent, RunTarget, Runtime, RuntimeRef, StopOptions,
};
use colab_playground_core::cli::Cli;
use colab_playground_core::guard::GuardPolicy;
use colab_playground_core::http::NoHttp;
use colab_playground_core::paths::HostPath;
use colab_playground_core::plugins::InputValue;
use colab_playground_core::provision::{ProvisionEvent, ProvisionSpec, WATCHDOG_ATTEMPTS};
use colab_playground_core::real::{RealBackend, RealConfig};
use colab_playground_core::sessions::{Accelerator, Ledger, StopReason, StopRecord, WatchdogInfo};
use colab_playground_core::tunnel::{TunnelEvent, TunnelSpec, TunnelState};
use colab_playground_core::vm::RunState;
use colab_playground_core::{CancelToken, Defaults, ErrorKind, Host, ManualClock, Toolchain};
use serde_json::{Value, json};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    cfg: RealConfig,
    b: RealBackend,
}

fn nc() -> CancelToken {
    CancelToken::new()
}

fn token(name: &str) -> String {
    json!({
        "token": format!("fake-access-{name}"),
        "refresh_token": format!("rt-{name}"),
        "client_id": "fake-client",
        "client_secret": "fake-secret",
        "token_uri": "https://oauth2.googleapis.com/token",
    })
    .to_string()
}

fn install(src: &Path, dst: &Path, exec: bool) {
    std::fs::copy(src, dst).unwrap();
    if exec {
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let fake = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fake");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    install(&fake.join("colab.py"), &bin.join("colab"), true);
    install(&fake.join("python_shim.py"), &bin.join("fake-python"), true);
    install(&fake.join("ssh.py"), &bin.join("ssh"), true);
    install(
        &fake.join("fake_common.py"),
        &bin.join("fake_common.py"),
        false,
    );
    let state = root.join("state");
    let home = root.join("home");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let keyring = json!({ "google-colab-cli\u{0}oauth_token": token("alice") });
    std::fs::write(state.join("keyring.json"), keyring.to_string()).unwrap();
    let server = json!({
        "accounts": {
            "alice": {"balance": 50.0, "assignments": {}},
            "bob": {"balance": 80.0, "assignments": {}}
        },
        "knobs": {}
    });
    std::fs::write(state.join("server.json"), server.to_string()).unwrap();
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let cfg = RealConfig {
        host: Host::Local,
        toolchain: Some(Toolchain {
            colab: s(&bin.join("colab")),
            helper_python: s(&bin.join("fake-python")),
        }),
        local_dir: root.join("local"),
        host_dir: Some(HostPath::new(s(&root.join("hostdata")))),
        base_env: vec![
            ("HOME".into(), s(&home)),
            ("CPG_FAKE_STATE".into(), s(&state)),
            ("CPG_SSH".into(), s(&bin.join("ssh"))),
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
        ],
        clock: Arc::new(ManualClock::fixed()),
        http: Arc::new(NoHttp),
        defaults: Defaults::default(),
        run_poll_interval: Duration::from_secs(1),
    };
    let b = RealBackend::new(cfg.clone()).unwrap();
    Fixture {
        _tmp: tmp,
        root,
        state,
        cfg,
        b,
    }
}

impl Fixture {
    fn server(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.state.join("server.json")).unwrap())
            .unwrap()
    }

    fn edit_server(&self, f: impl FnOnce(&mut Value)) {
        let mut v = self.server();
        f(&mut v);
        std::fs::write(self.state.join("server.json"), v.to_string()).unwrap();
    }

    fn knob(&self, k: &str, v: Value) {
        self.edit_server(|s| s["knobs"][k] = v);
    }

    fn assignments(&self, account: &str) -> serde_json::Map<String, Value> {
        self.server()["accounts"][account]["assignments"]
            .as_object()
            .cloned()
            .unwrap_or_default()
    }

    fn slot(&self) -> String {
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(self.state.join("keyring.json")).unwrap(),
        )
        .unwrap();
        v["google-colab-cli\u{0}oauth_token"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.state.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn history_path(&self, session: &str) -> PathBuf {
        self.root
            .join("home/.config/colab-cli/history")
            .join(format!("{session}.jsonl"))
    }

    fn history(&self, session: &str) -> String {
        std::fs::read_to_string(self.history_path(session)).unwrap_or_default()
    }

    fn helper(&self) -> Helper {
        let tc = self.cfg.toolchain.clone().unwrap();
        Helper::new(Host::Local, tc.helper_python, self.cfg.base_env.clone())
    }

    fn state_file(&self, id: &AccountId) -> PathBuf {
        self.root
            .join("hostdata")
            .join("accounts")
            .join(id.as_str())
            .join("sessions.json")
    }

    fn adopt_alice(&self) -> Account {
        let v = self.b.accounts(&nc()).unwrap();
        assert!(v.unadopted);
        assert_eq!(v.active_email.as_deref(), Some("alice@example.com"));
        self.b.adopt_active_account(&nc()).unwrap()
    }

    fn add_bob(&self) -> Account {
        let flow = self.b.begin_add_account(&nc()).unwrap();
        flow.submit_code("good-code", &nc()).unwrap()
    }

    /// Every `colab exec` that started the watchdog (only the start sends hooks).
    fn watchdog_starts(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter_map(|c| {
                let argv: Vec<String> = serde_json::from_value(c["argv"].clone()).ok()?;
                argv.iter()
                    .any(|a| a.starts_with("CPG_WD_HOOKS="))
                    .then_some(argv)
            })
            .collect()
    }

    fn listed_watchdog(&self, name: &str) -> (Runtime, WatchdogInfo) {
        let rt = self
            .b
            .runtimes(&nc())
            .unwrap()
            .runtimes
            .into_iter()
            .find(|r| r.id.name.as_deref() == Some(name))
            .unwrap();
        let wd = rt.tracked.as_ref().unwrap().watchdog.clone().unwrap();
        (rt, wd)
    }

    fn provision(
        &self,
        name: &str,
        accel: Accelerator,
    ) -> colab_playground_core::provision::ProvisionOutcome {
        let (tx, _rx) = channel();
        self.b
            .provision(
                ProvisionSpec::new(name, accel, &Defaults::default()),
                tx,
                &nc(),
            )
            .unwrap()
    }
}

fn wait_for<T>(rx: &Receiver<T>, secs: u64, mut pred: impl FnMut(&T) -> bool) -> Option<T> {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if let Ok(v) = rx.recv_timeout(Duration::from_millis(200))
            && pred(&v)
        {
            return Some(v);
        }
    }
    None
}

#[test]
fn cli_version_and_missing_toolchain() {
    let f = fixture();
    let tc = f.cfg.toolchain.clone().unwrap();
    let cli = Cli::new(Host::Local, tc.colab, f.cfg.base_env.clone());
    assert_eq!(cli.version(&nc()).unwrap(), "0.8.0-fake");

    let mut cfg = f.cfg.clone();
    cfg.toolchain = Some(Toolchain {
        colab: "/nonexistent/colab".into(),
        helper_python: "/nonexistent/python".into(),
    });
    cfg.local_dir = f.root.join("local-missing");
    let b = RealBackend::new(cfg).unwrap();
    assert!(b.accounts(&nc()).unwrap_err().is(ErrorKind::NotInstalled));
}

#[test]
fn login_rollback_cancel_drop_and_success() {
    let f = fixture();
    let alice = f.adopt_alice();
    assert_eq!(alice.email.as_deref(), Some("alice@example.com"));

    let flow = f.b.begin_add_account(&nc()).unwrap();
    assert!(flow.auth_url().starts_with("https://accounts.google.com/"));
    let err = flow.submit_code("wrong-code", &nc()).unwrap_err();
    assert!(err.is(ErrorKind::LoginInvalidCode), "{err}");
    assert_eq!(
        f.slot(),
        token("alice"),
        "the previous credential is restored"
    );
    let v = f.b.accounts(&nc()).unwrap();
    assert_eq!(v.active.as_ref(), Some(&alice.id));
    assert!(!v.recovered_pending_add);

    f.b.begin_add_account(&nc()).unwrap().cancel().unwrap();
    assert_eq!(f.slot(), token("alice"));
    {
        let _abandoned = f.b.begin_add_account(&nc()).unwrap();
    }
    assert_eq!(f.slot(), token("alice"));

    let bob = f.add_bob();
    assert_eq!(bob.email.as_deref(), Some("bob@example.com"));
    let v = f.b.accounts(&nc()).unwrap();
    assert_eq!(v.active.as_ref(), Some(&bob.id));
    assert_eq!(v.accounts.len(), 2);
    // No token ever reached the app-side data directory.
    let accounts_json = std::fs::read_to_string(f.cfg.local_dir.join("accounts.json")).unwrap();
    assert!(!accounts_json.contains("fake-access") && !accounts_json.contains("rt-"));
}

#[test]
fn per_account_state_files_switching_and_cross_account_stop() {
    let f = fixture();
    let alice = f.adopt_alice();
    let bob = f.add_bob();
    f.provision("bob-t4", Accelerator::T4);
    let impact = f.b.switch_impact(&alice.id).unwrap();
    assert_eq!(impact.sessions_left_running.len(), 1);
    f.b.switch_account(&alice.id, &nc()).unwrap();
    f.provision("alice-cpu", Accelerator::None);
    // `colab sessions` under alice did not prune bob's state file.
    assert!(
        std::fs::read_to_string(f.state_file(&bob.id))
            .unwrap()
            .contains("bob-t4")
    );
    // Every account-touching call carried the active account's own --config.
    let ids: BTreeMap<&str, &AccountId> = [("alice", &alice.id), ("bob", &bob.id)]
        .into_iter()
        .collect();
    for call in f.calls() {
        let argv: Vec<String> = serde_json::from_value(call["argv"].clone()).unwrap();
        if argv[0] == "whoami" || argv[0] == "version" {
            continue;
        }
        assert_eq!(argv[0], "--config", "{argv:?}");
        let who = call["account"].as_str().unwrap();
        assert_eq!(
            argv[1],
            f.state_file(ids[who]).to_string_lossy(),
            "{argv:?}"
        );
    }
    // Runtimes of every account, managed or not, can be listed and stopped.
    f.edit_server(|s| {
        s["accounts"]["alice"]["assignments"]["browser-1"] =
            json!({"accelerator": "L4", "variant": "GPU", "shape": "STANDARD"});
    });
    let rt = f.b.runtimes(&nc()).unwrap();
    assert!(rt.errors.is_empty(), "{:?}", rt.errors);
    assert_eq!(rt.runtimes.len(), 3);
    let bobs = rt.runtimes.iter().find(|r| r.id.account == bob.id).unwrap();
    assert!(!bobs.on_active_account && bobs.managed);
    let err =
        f.b.extend_session(&bobs.id, Duration::from_secs(3600), &nc())
            .unwrap_err();
    assert!(err.is(ErrorKind::AccountNotActive));
    assert_eq!(err.param("account"), Some(bob.id.as_str()));
    f.b.stop_session(&bobs.id, StopOptions::default(), &nc())
        .unwrap();
    assert!(f.assignments("bob").is_empty());
    let unmanaged = rt
        .runtimes
        .iter()
        .find(|r| r.id.endpoint == "browser-1")
        .unwrap();
    assert!(!unmanaged.managed && unmanaged.id.name.is_none());
    f.b.stop_session(&unmanaged.id, StopOptions::default(), &nc())
        .unwrap();
    assert_eq!(f.assignments("alice").len(), 1);
}

#[test]
fn provision_retries_to_80_gb_and_enforces_entitlement_and_cap() {
    let f = fixture();
    f.adopt_alice();
    let mut spec = ProvisionSpec::new("big", Accelerator::A100, &Defaults::default());
    spec.min_vram_mib = Some(70_000);
    let (tx, rx) = channel();
    let out = f.b.provision(spec, tx, &nc()).unwrap();
    assert_eq!(out.attempts, 2);
    assert_eq!(out.hardware.vram_mib, Some(81_920));
    assert!(out.watchdog.is_some());
    let events: Vec<ProvisionEvent> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Rejected { attempt: 1, .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::WatchdogStarted { .. }))
    );
    let mine = f.assignments("alice");
    assert_eq!(mine.len(), 1, "the 40 GB VM was released");
    let only = mine.values().next().unwrap();
    assert_eq!(only["vram"], 81_920);
    assert_eq!(only["watchdog"]["grace_s"], 300.0);

    f.knob("deny", json!(["H100"]));
    let (tx, _rx) = channel();
    let err =
        f.b.provision(
            ProvisionSpec::new("h", Accelerator::H100, &Defaults::default()),
            tx,
            &nc(),
        )
        .unwrap_err();
    assert!(err.is(ErrorKind::NotEntitled));
    assert_eq!(f.assignments("alice").len(), 1);

    f.provision("t4a", Accelerator::T4);
    let (tx, _rx) = channel();
    let err =
        f.b.provision(
            ProvisionSpec::new("t4b", Accelerator::T4, &Defaults::default()),
            tx,
            &nc(),
        )
        .unwrap_err();
    assert!(err.is(ErrorKind::SessionLimitReached));
}

/// A request Colab never answers fails once (the CLI already retried), and a
/// cancel kills the waiting `colab new` at once and stops the VM the request
/// got anyway.
#[test]
fn a_request_colab_never_answers_fails_once_and_a_cancel_kills_it() {
    let f = fixture();
    f.adopt_alice();
    f.knob("no_answer", json!(["A100"]));
    let spec = |name| ProvisionSpec::new(name, Accelerator::A100, &Defaults::default());
    let (tx, _rx) = channel();
    let err = f.b.provision(spec("na"), tx, &nc()).unwrap_err();
    assert!(err.is(ErrorKind::AllocationTimeout), "{err}");
    let news = f
        .calls()
        .iter()
        .filter(|c| {
            c["argv"]
                .as_array()
                .is_some_and(|a| a.contains(&json!("new")))
        })
        .count();
    assert_eq!(news, 1, "not retried");
    assert!(f.assignments("alice").is_empty());

    f.knob("no_answer_hangs", json!(true));
    let cancel = CancelToken::new();
    let canceller = {
        let cancel = cancel.clone();
        let server = f.state.join("server.json");
        std::thread::spawn(move || {
            // Cancel once the hanging request has got its VM.
            let end = Instant::now() + Duration::from_secs(30);
            while Instant::now() < end {
                let s: Value = std::fs::read_to_string(&server)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default();
                if s["accounts"]["alice"]["assignments"]
                    .as_object()
                    .is_some_and(|a| !a.is_empty())
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            cancel.cancel();
        })
    };
    let started = Instant::now();
    let (tx, rx) = channel();
    let err = f.b.provision(spec("nb"), tx, &cancel).unwrap_err();
    canceller.join().unwrap();
    assert!(err.is(ErrorKind::Cancelled), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the cancel waited for `colab new`: {:?}",
        started.elapsed()
    );
    let events: Vec<ProvisionEvent> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::OrphanDetected { released: true, .. })),
        "{events:?}"
    );
    assert!(
        f.assignments("alice").is_empty(),
        "the VM the request got anyway was left: {:?}",
        f.assignments("alice")
    );
}

/// Setting an auto-stop after a cancel puts it back on the runtime: the
/// watchdog is started again the way provisioning starts it.
#[test]
fn set_auto_stop_after_cancel_starts_the_watchdog_again() {
    let f = fixture();
    f.adopt_alice();
    let mut spec = ProvisionSpec::new("wd", Accelerator::T4, &Defaults::default());
    spec.guard.idle_timeout = Some(Duration::from_secs(900));
    let (tx, _rx) = channel();
    f.b.provision(spec, tx, &nc()).unwrap();
    let vm = |f: &Fixture| f.assignments("alice").values().next().unwrap()["watchdog"].clone();
    assert_eq!(f.watchdog_starts().len(), 1);
    let (rt, wd) = f.listed_watchdog("wd");
    assert!(wd.armed && !wd.cancelled);

    // The cancel reply still shows the watchdog running (it exits at its
    // next poll), yet it is not armed any more.
    f.b.cancel_auto_stop(&rt.id, &nc()).unwrap();
    let (rt, wd) = f.listed_watchdog("wd");
    assert!(!wd.armed && wd.cancelled);
    assert_eq!(rt.auto_stop_at, None);
    assert_eq!(vm(&f)["running"], false);

    let hour = Duration::from_secs(3600);
    let info = f.b.extend_session(&rt.id, hour, &nc()).unwrap();
    assert!(info.armed && !info.cancelled && info.deadline.is_some());
    assert_eq!(info.idle_timeout, Some(Duration::from_secs(900)));
    let starts = f.watchdog_starts();
    assert_eq!(starts.len(), 2, "the watchdog was started again");
    for env in [
        "CPG_WD_DEADLINE_S=3600",
        "CPG_WD_IDLE_S=900",
        "CPG_WD_GRACE_S=300",
    ] {
        assert!(starts[1].contains(&env.to_owned()), "{:?}", starts[1]);
    }
    assert_eq!(vm(&f)["running"], true);
    assert_eq!(vm(&f)["cancelled"], false);
    assert_eq!(vm(&f)["idle_timeout_s"], 900.0);
    let (rt, wd) = f.listed_watchdog("wd");
    assert_eq!(wd, info, "the listing shows the last known state");
    let tracked = rt.tracked.clone().unwrap();
    assert_eq!(
        tracked.started_at() + tracked.policy.max_runtime.unwrap(),
        f.b.now() + hour
    );
    assert!(rt.auto_stop_at.is_some());

    // A running watchdog only gets the new deadline.
    assert!(f.b.extend_session(&rt.id, 2 * hour, &nc()).unwrap().armed);
    assert_eq!(f.watchdog_starts().len(), 2);

    // One that exited without a cancel is started again too.
    f.edit_server(|s| {
        for a in s["accounts"]["alice"]["assignments"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            a["watchdog"]["running"] = json!(false);
        }
    });
    assert!(f.b.extend_session(&rt.id, hour, &nc()).unwrap().armed);
    assert_eq!(f.watchdog_starts().len(), 3);

    // It cannot be started: the call says the stop is not on the runtime and
    // the app-side limit still moves to the new time.
    f.b.cancel_auto_stop(&rt.id, &nc()).unwrap();
    f.knob("watchdog_start_fails", json!(true));
    let half = hour / 2;
    let info = f.b.extend_session(&rt.id, half, &nc()).unwrap();
    assert!(!info.armed && !info.cancelled);
    assert_eq!(info.deadline, None);
    assert_eq!(
        f.watchdog_starts().len(),
        3 + WATCHDOG_ATTEMPTS as usize,
        "tried like provisioning tries"
    );
    let (rt, wd) = f.listed_watchdog("wd");
    assert_eq!(wd, info);
    let tracked = rt.tracked.clone().unwrap();
    assert_eq!(
        tracked.started_at() + tracked.policy.max_runtime.unwrap(),
        f.b.now() + half
    );
    assert_eq!(rt.auto_stop_at, Some(f.b.now() + half));

    // Once it can start, trying again puts the stop back on the runtime.
    f.knob("watchdog_start_fails", json!(false));
    assert!(f.b.extend_session(&rt.id, hour, &nc()).unwrap().armed);
    assert_eq!(vm(&f)["running"], true);
}

#[test]
fn plugin_run_passes_secrets_only_via_env_and_redacts_them() {
    let f = fixture();
    f.adopt_alice();
    f.b.set_secret("hf_token", "hf_SECRET123", &nc()).unwrap();
    assert!(f.b.has_secret("hf_token", &nc()).unwrap());
    let mut spec = f.b.plugin_run_template("llama-cpp").unwrap();
    spec.inputs.insert(
        "repo".into(),
        InputValue::Text("unsloth/Qwen3-0.6B-GGUF".into()),
    );
    spec.inputs
        .insert("api_key".into(), InputValue::Text("sk-SECRET456".into()));
    let (tx, rx) = channel();
    let out = f.b.run_plugin(spec, tx, &nc()).unwrap();
    assert_eq!(out.status.state, RunState::Ready);
    assert_eq!(out.endpoints[0].remote_port, 8000);
    let events: Vec<PluginRunEvent> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PluginRunEvent::Launched { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PluginRunEvent::Log { .. }))
    );
    let text = serde_json::to_string(&events).unwrap();
    assert!(
        !text.contains("hf_SECRET123") && !text.contains("sk-SECRET456"),
        "secrets leaked into events"
    );
    let run = &f.server()["runs"][&out.run_id];
    assert_eq!(run["env"]["PLUGIN_HF_TOKEN"], "hf_SECRET123");
    assert_eq!(run["env"]["PLUGIN_INPUT_API_KEY"], "sk-SECRET456");
    assert_eq!(run["env"]["PLUGIN_INPUT_REPO"], "unsloth/Qwen3-0.6B-GGUF");
    // Check the secret *values*: the env var name CPG_SECRET_KEYS legitimately
    // appears in recorded cells.
    let leaked = |text: &str| text.contains("hf_SECRET123") || text.contains("sk-SECRET456");
    for entry in walk(&f.cfg.local_dir) {
        let content = std::fs::read(&entry).unwrap_or_default();
        let content = String::from_utf8_lossy(&content);
        assert!(!leaked(&content), "secret written to {}", entry.display());
    }
    // The CLI recorded the launch cell with its --env prelude; it was scrubbed.
    let history = f.history(&out.session);
    assert!(
        history.contains("PLUGIN_HF_TOKEN") && history.contains("***"),
        "{history}"
    );
    assert!(
        !leaked(&history),
        "secret left in the CLI history: {history}"
    );
    // The status polls (whose printed log carried the secret) were recorded
    // and scrubbed too, not dropped.
    assert!(history.contains("poll 0 (debug secret=***)"), "{history}");
    assert!(
        history.contains("--query-gpu=name,memory.total"),
        "other cells kept"
    );
    let mode = std::fs::metadata(f.history_path(&out.session))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // A failing stage stops the session provisioned for the run.
    f.knob("plugin_fail_stage", json!("download"));
    let before = f.assignments("alice").len();
    let mut spec = f.b.plugin_run_template("llama-cpp").unwrap();
    spec.inputs.insert(
        "repo".into(),
        InputValue::Text("unsloth/Qwen3-0.6B-GGUF".into()),
    );
    if let RunTarget::Provision(p) = &mut spec.target {
        p.accelerator = Accelerator::T4;
    }
    let (tx, _rx) = channel();
    let err = f.b.run_plugin(spec, tx, &nc()).unwrap_err();
    assert!(err.is(ErrorKind::PluginRunFailed));
    assert_eq!(err.param("stage"), Some("download"));
    assert!(!leaked(err.param("message").unwrap_or_default()));
    let failed_session = f
        .calls()
        .iter()
        .rev()
        .find_map(|c| {
            let argv: Vec<String> = serde_json::from_value(c["argv"].clone()).ok()?;
            let i = argv.iter().position(|a| a == "exec")?;
            (argv.get(i + 1)? == "-s").then(|| argv[i + 2].clone())
        })
        .unwrap();
    assert_ne!(failed_session, out.session);
    let failed_history = f.history(&failed_session);
    assert!(failed_history.contains("download"), "{failed_history}");
    assert!(
        !leaked(&failed_history),
        "secret left after a failed run: {failed_history}"
    );
    assert_eq!(
        f.assignments("alice").len(),
        before,
        "no billed VM left behind"
    );
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p))
            } else {
                out.push(p)
            }
        }
    }
    out
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn tunnel_uses_per_account_proxy_command_and_never_auto_creates() {
    let f = fixture();
    let alice = f.adopt_alice();
    f.provision("tun1", Accelerator::T4);
    let port = free_port();
    let spec = TunnelSpec {
        account: alice.id.clone(),
        session: "tun1".into(),
        local_port: port,
        remote_port: 8000,
        health_path: None,
    };
    let (tx, rx) = channel();
    let id = f.b.open_tunnel(spec.clone(), tx).unwrap();
    assert!(
        wait_for(&rx, 60, |e| matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Up,
                ..
            }
        ))
        .is_some(),
        "tunnel never came up"
    );
    let calls = std::fs::read_to_string(f.state.join("ssh_calls.jsonl")).unwrap();
    let expected = format!(
        "ProxyCommand={} --config {} ssh --proxy-mode -s tun1",
        f.cfg.toolchain.as_ref().unwrap().colab,
        f.state_file(&alice.id).display()
    );
    assert!(
        calls.contains(&serde_json::to_string(&expected).unwrap()),
        "{calls}"
    );
    assert!(calls.contains(&format!("\"127.0.0.1:{port}:127.0.0.1:8000\"")));
    f.b.close_tunnel(id).unwrap();
    assert!(f.b.tunnels().is_empty());

    // After the session is gone, a reconnect must not let --proxy-mode create a VM.
    let rt = f.b.runtimes(&nc()).unwrap();
    f.b.stop_session(&rt.runtimes[0].id, StopOptions::default(), &nc())
        .unwrap();
    let (tx, rx) = channel();
    f.b.open_tunnel(spec, tx).unwrap();
    let failed = wait_for(&rx, 30, |e| {
        matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Failed { .. },
                ..
            }
        )
    });
    match failed {
        Some(TunnelEvent::State {
            state: TunnelState::Failed { error },
            ..
        }) => assert!(error.is(ErrorKind::SessionGone), "{error}"),
        other => panic!("{other:?}"),
    }
    assert!(
        f.server().get("autocreated").is_none(),
        "a runtime was auto-created"
    );
    assert!(f.assignments("alice").is_empty());
}

#[test]
fn tunnel_reports_429_and_port_in_use() {
    let f = fixture();
    let alice = f.adopt_alice();
    f.provision("busy", Accelerator::T4);
    f.knob("ssh_busy", json!(true));
    let (tx, rx) = channel();
    let spec = TunnelSpec {
        account: alice.id.clone(),
        session: "busy".into(),
        local_port: free_port(),
        remote_port: 8000,
        health_path: None,
    };
    f.b.open_tunnel(spec.clone(), tx).unwrap();
    match wait_for(&rx, 90, |e| {
        matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Failed { .. },
                ..
            }
        )
    }) {
        Some(TunnelEvent::State {
            state: TunnelState::Failed { error },
            ..
        }) => assert!(error.is(ErrorKind::SshBusy), "{error}"),
        other => panic!("{other:?}"),
    }
    f.knob("ssh_busy", json!(false));
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (tx, rx) = channel();
    let spec = TunnelSpec {
        local_port: taken.local_addr().unwrap().port(),
        ..spec
    };
    f.b.open_tunnel(spec, tx).unwrap();
    match wait_for(&rx, 30, |e| {
        matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Failed { .. },
                ..
            }
        )
    }) {
        Some(TunnelEvent::State {
            state: TunnelState::Failed { error },
            ..
        }) => assert!(error.is(ErrorKind::PortInUse), "{error}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn usage_guard_stop_and_vanished_sessions() {
    let f = fixture();
    let alice = f.adopt_alice();
    let views = f.b.refresh_usage(&[], &nc()).unwrap();
    let snap = views[0].snapshot.unwrap();
    assert_eq!(snap.balance_cu, 50.0);
    assert!(!snap.stale);

    let mut spec = ProvisionSpec::new("floor", Accelerator::T4, &Defaults::default());
    spec.guard = GuardPolicy {
        min_balance_cu: Some(40.0),
        ..GuardPolicy::unlimited()
    };
    let (tx, _rx) = channel();
    f.b.provision(spec, tx, &nc()).unwrap();
    let r = f.b.guard_poll(&nc()).unwrap();
    assert_eq!(r.decisions.len(), 1);
    assert!(!r.decisions[0].stopped);
    f.edit_server(|s| s["accounts"]["alice"]["balance"] = json!(30.0));
    let r = f.b.guard_poll(&nc()).unwrap();
    assert!(r.decisions[0].stopped, "{:?}", r.decisions);
    assert!(f.assignments("alice").is_empty());
    let rt = f.b.runtimes(&nc()).unwrap();
    assert!(rt.runtimes.is_empty());

    f.provision("vanish", Accelerator::T4);
    f.edit_server(|s| s["accounts"]["alice"]["assignments"] = json!({}));
    let r = f.b.guard_poll(&nc()).unwrap();
    assert_eq!(r.vanished.len(), 1);
    assert_eq!(
        r.vanished[0].runtime,
        RuntimeRef {
            account: alice.id.clone(),
            endpoint: r.vanished[0].runtime.endpoint.clone(),
            name: Some("vanish".into())
        }
    );
    assert_eq!(r.vanished[0].reason, StopReason::Vanished);
}

#[test]
fn usage_reconciles_before_counting_managed_sessions() {
    for guard in [false, true] {
        let f = fixture();
        f.adopt_alice();
        f.provision("gone", Accelerator::T4);
        f.edit_server(|s| {
            s["accounts"]["alice"]["assignments"] = json!({
                "m-browser": {"accelerator": "T4", "high_mem": false}
            })
        });
        let views = if guard {
            let report = f.b.guard_poll(&nc()).unwrap();
            assert_eq!(report.vanished.len(), 1);
            report.usage
        } else {
            f.b.refresh_usage(&[], &nc()).unwrap()
        };
        assert!(views[0].ownership_verified);
        assert_eq!(views[0].unmanaged_runtimes, 1);
    }
}

#[test]
fn ownership_is_unverified_when_listing_fails_and_recovers_on_a_new_reading() {
    let f = fixture();
    f.adopt_alice();
    f.provision("managed", Accelerator::T4);
    f.knob("cli_api_broken", json!(true));
    for views in [
        f.b.refresh_usage(&[], &nc()).unwrap(),
        f.b.guard_poll(&nc()).unwrap().usage,
    ] {
        assert!(views[0].snapshot.is_some_and(|s| !s.stale));
        assert!(!views[0].ownership_verified);
    }
    f.knob("cli_api_broken", json!(false));
    f.b.runtimes(&nc()).unwrap();
    assert!(
        !f.b.usage()[0].ownership_verified,
        "listing does not rewrite a reading"
    );
    assert!(f.b.refresh_usage(&[], &nc()).unwrap()[0].ownership_verified);
}

#[test]
fn ownership_is_verified_without_live_ledger_entries_even_if_listing_fails() {
    for stopped in [false, true] {
        let f = fixture();
        f.adopt_alice();
        if stopped {
            f.provision("ended", Accelerator::T4);
            let target = f.b.runtimes(&nc()).unwrap().runtimes[0].id.clone();
            f.b.stop_session(&target, StopOptions::default(), &nc())
                .unwrap();
        }
        f.edit_server(|s| {
            s["accounts"]["alice"]["assignments"] = json!({
                "m-browser": {"accelerator": "T4"}
            });
        });
        f.knob("cli_api_broken", json!(true));
        let views = f.b.refresh_usage(&[], &nc()).unwrap();
        assert!(views[0].ownership_verified);
        assert_eq!(views[0].unmanaged_runtimes, 1);
        assert!(!views[0].snapshot.unwrap().stale);
    }
}

#[test]
fn runtime_listing_survives_ledger_write_failure() {
    let f = fixture();
    f.adopt_alice();
    f.provision("ended", Accelerator::T4);
    f.provision("running", Accelerator::T4);
    let running_endpoint =
        f.b.runtimes(&nc())
            .unwrap()
            .runtimes
            .into_iter()
            .find(|r| r.id.name.as_deref() == Some("running"))
            .unwrap()
            .id
            .endpoint;
    f.edit_server(|s| {
        s["accounts"]["alice"]["assignments"]
            .as_object_mut()
            .unwrap()
            .retain(|endpoint, _| endpoint == &running_endpoint);
    });
    let ledger_file = f.cfg.local_dir.join("ledger.json");
    std::fs::remove_file(&ledger_file).unwrap();
    std::fs::create_dir(&ledger_file).unwrap();
    let view = f.b.runtimes(&nc()).unwrap();
    assert_eq!(view.runtimes.len(), 1);
    assert_eq!(view.runtimes[0].id.name.as_deref(), Some("running"));
}

#[test]
fn endpoint_backfill_is_persisted_and_missing_names_do_not_end_sessions() {
    let mut f = fixture();
    let account = f.adopt_alice();
    f.provision("managed", Accelerator::T4);
    let state_file = f.state_file(&account.id);
    let state = std::fs::read_to_string(&state_file).unwrap();
    let ledger_file = f.cfg.local_dir.join("ledger.json");
    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_file).unwrap()).unwrap();
    ledger[0]["endpoint"] = Value::Null;
    std::fs::write(&ledger_file, ledger.to_string()).unwrap();
    f.b = RealBackend::new(f.cfg.clone()).unwrap();

    for contents in [None, Some("not json"), Some("{}")] {
        if let Some(contents) = contents {
            std::fs::write(&state_file, contents).unwrap();
        } else {
            std::fs::remove_file(&state_file).unwrap();
        }
        let views = f.b.refresh_usage(&[], &nc()).unwrap();
        assert!(!views[0].ownership_verified);
        let ledger: Value =
            serde_json::from_str(&std::fs::read_to_string(&ledger_file).unwrap()).unwrap();
        assert!(ledger[0]["endpoint"].is_null());
        assert!(ledger[0]["stopped"].is_null());
    }
    std::fs::write(&state_file, state).unwrap();
    assert!(f.b.runtimes(&nc()).unwrap().runtimes[0].managed);
    let ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_file).unwrap()).unwrap();
    assert!(ledger[0]["endpoint"].is_string());
    f.b = RealBackend::new(f.cfg.clone()).unwrap();
    std::fs::remove_file(&state_file).unwrap();
    assert!(f.b.refresh_usage(&[], &nc()).unwrap()[0].ownership_verified);
    f.edit_server(|s| s["accounts"]["alice"]["assignments"] = json!({}));
    assert_eq!(f.b.guard_poll(&nc()).unwrap().vanished.len(), 1);
}

#[test]
fn a_session_without_an_endpoint_ends_once_nothing_listed_could_be_it() {
    let mut f = fixture();
    let account = f.adopt_alice();
    f.provision("managed", Accelerator::T4);
    let state_file = f.state_file(&account.id);
    let ledger_file = f.cfg.local_dir.join("ledger.json");
    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_file).unwrap()).unwrap();
    ledger[0]["endpoint"] = Value::Null;
    std::fs::write(&ledger_file, ledger.to_string()).unwrap();
    f.b = RealBackend::new(f.cfg.clone()).unwrap();
    std::fs::remove_file(&state_file).unwrap();

    // Its runtime is still listed and unnamed, so it could be the session.
    assert!(!f.b.refresh_usage(&[], &nc()).unwrap()[0].ownership_verified);
    f.edit_server(|s| s["accounts"]["alice"]["assignments"] = json!({}));
    let report = f.b.guard_poll(&nc()).unwrap();
    assert_eq!(report.vanished.len(), 1);
    assert_eq!(report.vanished[0].runtime.name.as_deref(), Some("managed"));
    assert!(report.usage[0].ownership_verified);
}

#[test]
fn a_stop_without_an_endpoint_suspects_an_orphan_and_releases_nothing() {
    let mut f = fixture();
    let account = f.adopt_alice();
    f.provision("managed", Accelerator::T4);
    let state_file = f.state_file(&account.id);
    let ledger_file = f.cfg.local_dir.join("ledger.json");
    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_file).unwrap()).unwrap();
    ledger[0]["endpoint"] = Value::Null;
    std::fs::write(&ledger_file, ledger.to_string()).unwrap();
    f.b = RealBackend::new(f.cfg.clone()).unwrap();
    std::fs::remove_file(&state_file).unwrap();

    let target = RuntimeRef {
        account: account.id.clone(),
        endpoint: String::new(),
        name: Some("managed".into()),
    };
    let err =
        f.b.stop_session(&target, StopOptions::default(), &nc())
            .unwrap_err();
    assert!(err.is(ErrorKind::OrphanSuspected), "{err}");
    assert_eq!(f.assignments("alice").len(), 1);
}

#[test]
fn a_stop_whose_confirmation_listing_fails_is_finalised_as_user_stopped() {
    let f = fixture();
    f.adopt_alice();
    f.provision("stop-pending", Accelerator::T4);
    let target = f.b.runtimes(&nc()).unwrap().runtimes[0].id.clone();

    // The stop releases the runtime, then the one confirmation listing errors.
    f.knob("list_assignments_fail_once", json!(true));
    let err =
        f.b.stop_session(&target, StopOptions::default(), &nc())
            .unwrap_err();
    assert!(err.is(ErrorKind::StopUnconfirmed), "{err}");
    assert!(!err.is(ErrorKind::OrphanSuspected));

    // The next listing sees the (now empty) server list, so reconcile confirms
    // the stop and records the user's reason rather than a vanish.
    let report = f.b.guard_poll(&nc()).unwrap();
    assert_eq!(report.vanished.len(), 1);
    assert_eq!(
        report.vanished[0].runtime.name.as_deref(),
        Some("stop-pending")
    );
    assert_eq!(report.vanished[0].reason, StopReason::User);

    let ledger = Ledger::load(&f.cfg.local_dir.join("ledger.json")).unwrap();
    let entry = ledger
        .entries()
        .iter()
        .find(|e| e.name == "stop-pending")
        .unwrap();
    assert!(matches!(
        entry.stopped,
        Some(StopRecord {
            reason: StopReason::User,
            ..
        })
    ));
    assert!(entry.stop_pending_confirmation.is_none());
    assert!(
        ledger.entries().iter().all(|e| e.stopped.is_some()),
        "no session is left live"
    );
}

#[test]
fn keyring_and_cli_api_failures_are_typed() {
    let f = fixture();
    f.adopt_alice();
    f.knob("cli_api_broken", json!(true));
    let rt = f.b.runtimes(&nc()).unwrap();
    assert!(rt.errors[0].error.is(ErrorKind::UnsupportedCliVersion));
    f.knob("cli_api_broken", json!(false));
    f.knob("keyring_locked", json!(true));
    assert!(
        f.b.accounts(&nc())
            .unwrap_err()
            .is(ErrorKind::KeyringUnavailable)
    );
}

#[test]
fn history_redaction_scrubs_only_secret_values() {
    let f = fixture();
    let helper = f.helper();
    let secret = "hf_Ab'c\"d\\é";
    let missing = helper
        .history_redact("nothing-here", &[secret.to_owned()], &nc())
        .unwrap();
    assert!(!missing.present);
    assert_eq!(missing.redacted_lines, 0);

    let path = f.history_path("sess-1");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Line 1: the CLI's own encoding (repr inside the prelude, then JSON).
    let prelude = format!(
        "import os\nos.environ['PLUGIN_HF_TOKEN'] = {}\nprint('ok')",
        python_repr(secret)
    );
    let cell = json!({"event_type": "execution", "code": prelude, "outputs": [{"text": format!("token {secret}\n")}]});
    let unrelated =
        r#"{"event_type": "session_created", "endpoint": "gpu-t4-s-1",   "spacing": "kept"}"#;
    let raw = format!("not json at all {secret} tail");
    std::fs::write(&path, format!("{cell}\n{unrelated}\n{raw}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let r = helper
        .history_redact("sess-1", &[secret.to_owned(), "abc".to_owned()], &nc())
        .unwrap();
    assert!(r.present);
    assert_eq!(r.redacted_lines, 2);
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(!text.contains("hf_Ab"), "{text}");
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(
        first["code"],
        "import os\nos.environ['PLUGIN_HF_TOKEN'] = '***'\nprint('ok')"
    );
    assert_eq!(first["outputs"][0]["text"], "token ***\n");
    assert_eq!(
        lines[1], unrelated,
        "lines without secrets are left byte-for-byte"
    );
    assert_eq!(lines[2], "not json at all *** tail");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(text.ends_with('\n'));
}

/// Python's `repr()` of a str, for the characters used in the test above.
fn python_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[test]
fn switch_impact_lists_unmanaged_runtimes_via_the_helper() {
    let f = fixture();
    let alice = f.adopt_alice();
    let bob = f.add_bob();
    f.provision("bob-t4", Accelerator::T4);
    f.edit_server(|s| {
        s["accounts"]["bob"]["assignments"]["browser-9"] =
            json!({"accelerator": "L4", "variant": "GPU", "shape": "STANDARD"});
    });
    let impact = f.b.switch_impact(&alice.id).unwrap();
    assert_eq!(impact.from.as_ref(), Some(&bob.id));
    assert_eq!(impact.sessions_left_running.len(), 1);
    let unmanaged = impact.unmanaged_left_running.unwrap();
    assert_eq!(unmanaged.len(), 1);
    assert_eq!(unmanaged[0].endpoint, "browser-9");
    assert_eq!(unmanaged[0].account, bob.id);
    let same = f.b.switch_impact(&bob.id).unwrap();
    assert_eq!(same.unmanaged_left_running, Some(vec![]));
}

#[test]
fn switch_impact_never_reports_unlisted_runtimes_as_none() {
    let f = fixture();
    let alice = f.adopt_alice();
    let bob = f.add_bob();
    let left = |to: &AccountId| f.b.switch_impact(to).unwrap().unmanaged_left_running;
    // The helper answers, but reports that bob's runtimes could not be listed.
    f.knob("cli_api_broken", json!(true));
    assert_eq!(left(&alice.id), None, "no list and no usage");
    assert_eq!(left(&bob.id), Some(vec![]), "staying leaves nothing behind");

    f.b.refresh_usage(&[], &nc()).unwrap();
    assert_eq!(
        left(&alice.id),
        None,
        "a usage reading of zero is not a list"
    );

    // Usage that shows the runtime does not stand in for the list either.
    f.edit_server(|s| {
        s["accounts"]["bob"]["assignments"]["browser-9"] =
            json!({"accelerator": "L4", "variant": "GPU", "shape": "STANDARD"});
    });
    let usage = f.b.refresh_usage(&[], &nc()).unwrap();
    let bobs = usage.iter().find(|u| u.account == bob.id).unwrap();
    assert_eq!(bobs.unmanaged_runtimes, 1);
    assert_eq!(left(&alice.id), None);

    // The helper failing outright, before it answers for any account.
    f.knob("cli_api_broken", json!(false));
    f.knob("keyring_locked", json!(true));
    assert_eq!(left(&alice.id), None);

    f.knob("keyring_locked", json!(false));
    let listed = left(&alice.id).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].endpoint, "browser-9");
}

/// Overriding only the `colab` path derives the interpreter from its
/// shebang (no Python is run), and bad paths say what to fix.
#[test]
fn colab_path_override_derives_the_interpreter() {
    let tmp = tempfile::tempdir().unwrap();
    // Canonical, because the launcher form resolves symlinks and macOS keeps its temp
    // directories behind one (/var -> /private/var).
    let root = tmp.path().canonicalize().unwrap();
    let root = root.as_path();
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let exe = |p: &Path, text: &str| {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let resolve = |p: &Path| Toolchain::from_colab_path(s(p)).resolve(&Host::Local, &[], &nc());
    let reason = |p: &Path| resolve(p).unwrap_err().param("reason").map(str::to_owned);

    assert_eq!(
        reason(&root.join("missing/colab")).as_deref(),
        Some("not_found")
    );
    let plain = root.join("plain/colab");
    std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
    std::fs::write(&plain, "#!/usr/bin/python3\n").unwrap();
    assert_eq!(reason(&plain).as_deref(), Some("not_executable"));
    let shell = root.join("shell/colab");
    exe(&shell, "#!/bin/sh\necho hi\n");
    assert_eq!(reason(&shell).as_deref(), Some("no_python_shebang"));

    // A uv/pip entry script whose interpreter is missing, then present.
    let venv_python = root.join("venv/bin/python");
    let entry = root.join("venv/bin/colab");
    exe(&entry, &format!("#!{}\nimport sys\n", s(&venv_python)));
    let err = resolve(&entry).unwrap_err();
    assert_eq!(err.param("reason"), Some("interpreter_missing"));
    assert_eq!(err.param("interpreter"), Some(s(&venv_python).as_str()));
    exe(&venv_python, "#!/bin/sh\nexit 0\n");
    assert_eq!(resolve(&entry).unwrap().helper_python, s(&venv_python));

    // Through a symlink (as ~/.local/bin/colab is), and the /bin/sh launcher form.
    let link = root.join("bin/colab");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&entry, &link).unwrap();
    assert_eq!(resolve(&link).unwrap().helper_python, s(&venv_python));
    let launcher = root.join("venv/bin/colab-launcher");
    exe(
        &launcher,
        "#!/bin/sh\n'''exec' \"$(dirname -- \"$(realpath -- \"$0\")\")\"/'python' \"$0\" \"$@\"\n' '''\n",
    );
    let t = resolve(&launcher).unwrap();
    assert_eq!(t.helper_python, s(&root.join("venv/bin").join("python")));
    assert_eq!(t.colab, s(&launcher));
}

/// The real launch and poll snippets, run in one Python process standing in
/// for the kernel (globals persist between cells, as in Colab): a secret the
/// script prints or puts into status.json never appears in the poll output.
#[test]
fn poll_snippet_scrubs_secrets_kept_in_kernel_memory() {
    use colab_playground_core::{vm, watchdog};
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("vm").to_string_lossy().into_owned();
    let script = br#"import json, os, time
secret = os.environ["PLUGIN_INPUT_API_KEY"]
print("leaking " + secret, flush=True)
st = os.environ["PLUGIN_STATUS_FILE"]
with open(st + ".tmp", "w") as fh:
    json.dump({"state": "running", "stage": "s", "message": "m " + secret, "outputs": {"k": [secret]}}, fh)
os.replace(st + ".tmp", st)
time.sleep(30)
"#;
    let write = |name: &str, text: String| {
        let p = tmp.path().join(name);
        std::fs::write(&p, text.replace(watchdog::VM_BASE, &base)).unwrap();
        p.to_string_lossy().into_owned()
    };
    let launch = write("launch.py", vm::plugin_launch_snippet(script));
    let poll = write("poll.py", vm::plugin_poll_snippet());
    let kill = write("kill.py", vm::plugin_kill_snippet());
    let driver = write(
        "driver.py",
        r#"import contextlib, io, json, os, sys, time
g = {"__name__": "__main__"}
os.environ.update({"PLUGIN_INPUT_API_KEY": "sk-VMSECRET789", "PLUGIN_INPUT_REPO": "a/b",
                   "CPG_RUN_ID": "r1", "CPG_SCRIPT_NAME": "w.py", "CPG_SECRET_KEYS": "PLUGIN_INPUT_API_KEY"})
exec(open(sys.argv[1]).read(), g)
assert "PLUGIN_INPUT_API_KEY" not in os.environ, "secret left in the kernel env"
result = None
for _ in range(100):
    time.sleep(0.1)
    os.environ.update({"CPG_RUN_ID": "r1", "CPG_LOG_OFFSET": "0"})
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        exec(open(sys.argv[2]).read(), g)
    result = buf.getvalue()
    if "leaking" in result and '"message"' in result:
        break
os.environ["CPG_RUN_ID"] = "r1"
with contextlib.redirect_stdout(io.StringIO()):
    exec(open(sys.argv[3]).read(), g)
print("POLL " + result.strip())
"#
        .to_owned(),
    );
    let out = Host::Local
        .run_program(
            "python3",
            &["-B".to_owned(), driver, launch, poll, kill],
            &[("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned())],
            &nc(),
            Some(Duration::from_secs(60)),
        )
        .unwrap();
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    let line = out.stdout.lines().find(|l| l.starts_with("POLL ")).unwrap();
    assert!(!out.stdout.contains("sk-VMSECRET789"), "{}", out.stdout);
    assert!(
        line.contains("leaking ***") && line.contains("m ***"),
        "{line}"
    );
    let v: Value = serde_json::from_str(line.trim_start_matches("POLL __CPG_RESULT__ ")).unwrap();
    assert_eq!(v["status"]["outputs"]["k"][0], "***");
}

#[test]
fn worker_quant_fitting_matches_the_rust_vectors() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let worker = root.join("../../plugins/llama-cpp/worker.py");
    let vectors = root.join("tests/data/quant_fit_vectors.json");
    // -B / PYTHONDONTWRITEBYTECODE: never leave __pycache__ inside the repo.
    let args = vec![
        "-B".to_owned(),
        worker.to_string_lossy().into_owned(),
        "--fit-self-test".to_owned(),
        vectors.to_string_lossy().into_owned(),
    ];
    let env = vec![("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned())];
    let out = Host::Local
        .run_program("python3", &args, &env, &nc(), Some(Duration::from_secs(60)))
        .unwrap();
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    assert!(!out.stdout.contains("FAIL"), "{}", out.stdout);
}
