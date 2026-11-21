//! Long-running operations: the state behind `sessions_start`,
//! `operations_get` and `operations_cancel` (design section 3).
//!
//! Provisioning takes minutes, far past an agent host's tool-call timeout,
//! so a start runs on its own thread and the caller polls. A caller-given
//! `request_id` makes retries safe: the same id always names the same
//! operation, and provisioning is never duplicated. Operations live in
//! this process's memory only — if the server restarts, the sessions
//! themselves are still in the ledger and `sessions_list` is the
//! reconciliation path.
//!
//! An operation reports success only when the provisioned session's
//! watchdog is confirmed armed: in v1 the VM watchdog is the only guard,
//! so a session without it is a failed operation (the session itself is
//! named in the error, so the caller can stop it or extend it to arm one).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use colab_playground_core::accounts::AccountId;
use colab_playground_core::backend::Backend;
use colab_playground_core::provision::{ProvisionEvent, ProvisionOutcome, ProvisionSpec};
use colab_playground_core::{CancelToken, Error as CoreError, ErrorKind};
use serde_json::{Value, json};

use crate::server::error_body;

/// How long `operations_get` suggests waiting between polls.
const POLL_AFTER_MS: u64 = 5_000;

enum OpState {
    Running {
        stage: String,
        last_event: Option<Value>,
    },
    Succeeded(Value),
    Failed(Value),
    Cancelled,
}

pub struct Operation {
    id: String,
    request_id: String,
    account: AccountId,
    name: String,
    max_runtime_secs: u64,
    cancel: CancelToken,
    state: Mutex<OpState>,
}

impl Operation {
    /// The operation as an agent sees it (the `operations_get` result).
    pub fn view(&self) -> Value {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut view = json!({
            "operation_id": self.id,
            "request_id": self.request_id,
            "account": self.account,
            "name": self.name,
        });
        match &*state {
            OpState::Running { stage, last_event } => {
                view["state"] = json!("running");
                view["stage"] = json!(stage);
                view["last_event"] = last_event.clone().unwrap_or(Value::Null);
                view["poll_after_ms"] = json!(POLL_AFTER_MS);
            }
            OpState::Succeeded(result) => {
                view["state"] = json!("succeeded");
                view["result"] = result.clone();
            }
            OpState::Failed(body) => {
                view["state"] = json!("failed");
                view["error"] = body.clone();
            }
            OpState::Cancelled => {
                view["state"] = json!("cancelled");
            }
        }
        view
    }

    fn set_stage(&self, event: &ProvisionEvent) {
        let value = serde_json::to_value(event).unwrap_or(Value::Null);
        let stage = value
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("working")
            .to_string();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let OpState::Running {
            stage: s,
            last_event,
        } = &mut *state
        {
            *s = stage;
            *last_event = Some(value);
        }
    }

    fn finish(&self, result: Result<Value, CoreError>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *state = match result {
            Ok(value) => OpState::Succeeded(value),
            Err(e) if self.cancel.is_cancelled() || e.is(ErrorKind::Cancelled) => {
                OpState::Cancelled
            }
            Err(e) => OpState::Failed(error_body(&e)),
        };
    }
}

#[derive(Default)]
struct Registry {
    by_id: HashMap<String, Arc<Operation>>,
    by_request: HashMap<String, String>,
}

#[derive(Default)]
pub struct Operations {
    registry: Mutex<Registry>,
    counter: AtomicU64,
}

impl Operations {
    pub fn new() -> Operations {
        Operations::default()
    }

    pub fn get(&self, operation_id: &str) -> Option<Arc<Operation>> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_id
            .get(operation_id)
            .cloned()
    }

    pub fn get_by_request(&self, request_id: &str) -> Option<Arc<Operation>> {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        registry
            .by_request
            .get(request_id)
            .and_then(|id| registry.by_id.get(id))
            .cloned()
    }

    /// Starts provisioning in the background, or returns the operation an
    /// earlier call with the same `request_id` already started. The bool
    /// says whether this call created the operation.
    pub fn start(
        &self,
        backend: Arc<dyn Backend>,
        request_id: String,
        account: AccountId,
        spec: ProvisionSpec,
        max_runtime_secs: u64,
    ) -> (Arc<Operation>, bool) {
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let op = Arc::new(Operation {
            id: format!("op-{n}"),
            request_id: request_id.clone(),
            account,
            name: spec.name.clone(),
            max_runtime_secs,
            cancel: CancelToken::new(),
            state: Mutex::new(OpState::Running {
                stage: "queued".to_string(),
                last_event: None,
            }),
        });
        {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            // A racing (or earlier) start with the same request id wins;
            // this operation is dropped before its thread ever spawns.
            if let Some(existing) = registry
                .by_request
                .get(&request_id)
                .and_then(|id| registry.by_id.get(id))
            {
                return (existing.clone(), false);
            }
            registry.by_request.insert(request_id, op.id.clone());
            registry.by_id.insert(op.id.clone(), op.clone());
        }

        let worker = op.clone();
        std::thread::spawn(move || {
            let (tx, rx) = mpsc::channel::<ProvisionEvent>();
            let staging = worker.clone();
            let forwarder = std::thread::spawn(move || {
                while let Ok(event) = rx.recv() {
                    staging.set_stage(&event);
                }
            });
            let result = backend
                .provision(spec, tx, &worker.cancel)
                .and_then(|outcome| ready_result(&worker, &outcome));
            let _ = forwarder.join();
            worker.finish(result);
        });
        (op, true)
    }

    /// Asks an operation to cancel. Returns the operation (its state may
    /// still be `running` until the provisioning thread unwinds).
    pub fn cancel(&self, operation_id: &str) -> Option<Arc<Operation>> {
        let op = self.get(operation_id)?;
        op.cancel.cancel();
        Some(op)
    }
}

/// The success value of a start operation — or the watchdog failure that
/// turns a provisioned-but-unguarded session into a failed operation.
fn ready_result(op: &Operation, outcome: &ProvisionOutcome) -> Result<Value, CoreError> {
    let armed = outcome.watchdog.as_ref().is_some_and(|w| w.armed);
    if !armed {
        return Err(CoreError::new(ErrorKind::Other)
            .with("session", &outcome.name)
            .with("endpoint", &outcome.endpoint)
            .detail(
                "the runtime started but its auto-stop watchdog is not armed, \
                     so the session is running without the v1 guard; stop it, \
                     or extend it to arm a watchdog",
            ));
    }
    Ok(json!({
        "session": {
            "account": op.account,
            "endpoint": outcome.endpoint,
            "name": outcome.name,
        },
        "hardware": outcome.hardware,
        "attempts": outcome.attempts,
        "below_spec": outcome.below_spec,
        "watchdog": outcome.watchdog,
        "started_at": outcome.started_at,
        "policy": {
            "max_runtime_secs": op.max_runtime_secs,
            "watcher_mode": "vm_watchdog_only",
        },
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc as StdArc;
    use std::time::{Duration as StdDuration, SystemTime};

    use colab_playground_core::Defaults;
    use colab_playground_core::backend::Backend;
    use colab_playground_core::mock::{MockBackend, MockConfig, MockFailures, MockScenario};
    use colab_playground_core::provision::ProvisionSpec;
    use colab_playground_core::sessions::Accelerator;
    use colab_playground_core::{SystemClock, guard::GuardPolicy};

    use super::*;

    fn mock_backend(step_delay: StdDuration) -> StdArc<dyn Backend> {
        // The demo scenario already runs sessions on the active account;
        // raise the concurrency cap so a test provision has headroom.
        let defaults = Defaults {
            max_concurrent_sessions: 8,
            ..Defaults::default()
        };
        StdArc::new(MockBackend::new(MockConfig {
            clock: StdArc::new(SystemClock),
            step_delay,
            failures: MockFailures::default(),
            scenario: MockScenario::demo(),
            defaults,
        }))
    }

    fn cpu_spec(backend: &dyn Backend, max_runtime_secs: u64) -> ProvisionSpec {
        let name = backend.new_session_name(Some("test"));
        let mut spec = ProvisionSpec::new(name, Accelerator::None, &Defaults::default());
        spec.guard = GuardPolicy {
            max_runtime: Some(StdDuration::from_secs(max_runtime_secs)),
            max_spend_cu: None,
            min_balance_cu: None,
            idle_timeout: None,
        };
        spec
    }

    fn wait_terminal(op: &Operation) -> Value {
        let deadline = SystemTime::now() + StdDuration::from_secs(30);
        loop {
            let view = op.view();
            if view["state"] != json!("running") {
                return view;
            }
            assert!(SystemTime::now() < deadline, "operation did not finish");
            std::thread::sleep(StdDuration::from_millis(10));
        }
    }

    #[test]
    fn a_start_provisions_and_reports_the_session() {
        let backend = mock_backend(StdDuration::ZERO);
        let ops = Operations::new();
        let account = backend
            .accounts(&CancelToken::new())
            .unwrap()
            .active
            .unwrap();
        let (op, created) = ops.start(
            backend.clone(),
            "req-1".into(),
            account,
            cpu_spec(&*backend, 900),
            900,
        );
        assert!(created);
        let view = wait_terminal(&op);
        assert_eq!(view["state"], json!("succeeded"), "{view}");
        assert_eq!(
            view["result"]["policy"]["watcher_mode"],
            json!("vm_watchdog_only")
        );
        assert!(
            view["result"]["session"]["endpoint"]
                .as_str()
                .is_some_and(|e| !e.is_empty())
        );
    }

    #[test]
    fn a_retried_request_id_never_provisions_twice() {
        let backend = mock_backend(StdDuration::ZERO);
        let ops = Operations::new();
        let account = backend
            .accounts(&CancelToken::new())
            .unwrap()
            .active
            .unwrap();
        let (first, created) = ops.start(
            backend.clone(),
            "req-dup".into(),
            account.clone(),
            cpu_spec(&*backend, 900),
            900,
        );
        assert!(created);
        let (second, created) = ops.start(
            backend.clone(),
            "req-dup".into(),
            account,
            cpu_spec(&*backend, 900),
            900,
        );
        assert!(!created);
        assert_eq!(first.id, second.id);
        wait_terminal(&first);
        let runtimes = backend.runtimes(&CancelToken::new()).unwrap();
        let managed = runtimes
            .runtimes
            .iter()
            .filter(|r| r.managed && r.id.name.as_deref() == Some(first.name.as_str()))
            .count();
        assert_eq!(managed, 1);
    }

    #[test]
    fn a_cancelled_start_ends_cancelled() {
        let backend = mock_backend(StdDuration::from_millis(250));
        let ops = Operations::new();
        let account = backend
            .accounts(&CancelToken::new())
            .unwrap()
            .active
            .unwrap();
        let (op, _) = ops.start(
            backend.clone(),
            "req-cancel".into(),
            account,
            cpu_spec(&*backend, 900),
            900,
        );
        ops.cancel(&op.id).unwrap();
        let view = wait_terminal(&op);
        assert_eq!(view["state"], json!("cancelled"), "{view}");
    }
}
