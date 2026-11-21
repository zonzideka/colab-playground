//! The MCP surface: one tool per `Backend` operation, shaped for an agent
//! caller (see `docs/mcp-design.md`). Business failures come back as tool
//! results with `isError` and a structured body carrying the core error's
//! kind slug, so a caller branches on the slug alone; only routing failures
//! (an unknown tool) are protocol errors.

use std::{path::PathBuf, sync::Arc, time::Duration};

use colab_playground_core::{
    CancelToken, Error as CoreError, ErrorKind,
    accounts::AccountId,
    backend::{Backend, RuntimeRef, StopOptions},
    guard::GuardPolicy,
    provision::ProvisionSpec,
    sessions::Accelerator,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorCode,
        Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
        Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use serde_json::{Map, Value, json};

use crate::ops::Operations;

#[derive(Clone)]
pub struct McpServer {
    backend: Arc<dyn Backend>,
    backend_label: &'static str,
    data_dir: Option<PathBuf>,
    ops: Arc<Operations>,
}

impl McpServer {
    pub fn new(
        backend: Arc<dyn Backend>,
        backend_label: &'static str,
        data_dir: Option<PathBuf>,
    ) -> Self {
        McpServer {
            backend,
            backend_label,
            data_dir,
            ops: Arc::new(Operations::new()),
        }
    }

    /// Runs one backend call on a blocking thread (the real backend shells
    /// out to the CLI) and bridges the request's cancellation token onto the
    /// core token, the same path the GUI's cancel buttons take. A core error
    /// is returned as a value: the caller turns it into a tool result, never
    /// a protocol error.
    async fn run<T>(
        &self,
        context: &RequestContext<RoleServer>,
        call: impl FnOnce(&dyn Backend, &CancelToken) -> Result<T, CoreError> + Send + 'static,
    ) -> Result<T, CoreError>
    where
        T: Send + 'static,
    {
        let token = CancelToken::new();
        let watcher = {
            let token = token.clone();
            let cancelled = context.ct.clone();
            tokio::spawn(async move {
                cancelled.cancelled().await;
                token.cancel();
            })
        };
        let backend = self.backend.clone();
        let joined = tokio::task::spawn_blocking(move || call(backend.as_ref(), &token)).await;
        watcher.abort();
        match joined {
            Ok(result) => result,
            Err(e) => Err(CoreError::new(ErrorKind::Other).detail(format!("backend task: {e}"))),
        }
    }

    fn status(&self) -> Value {
        json!({
            "backend": self.backend_label,
            "server_version": env!("CARGO_PKG_VERSION"),
            "data_dir": self.data_dir.as_ref().map(|p| p.display().to_string()),
        })
    }

    /// `sessions_start`: validate, pin the account, register the operation
    /// and return it at once; provisioning runs on the operation's thread.
    async fn sessions_start(
        &self,
        context: &RequestContext<RoleServer>,
        args: &Map<String, Value>,
    ) -> CallToolResult {
        let request_id = match req_str(args, "request_id") {
            Ok(id) => id.to_string(),
            Err(e) => return failure(&e),
        };
        // An idempotent retry returns the original operation untouched,
        // whatever the rest of the retry carries.
        if let Some(existing) = self.ops.get_by_request(&request_id) {
            return success(existing.view());
        }
        let max_runtime_secs = match validate_policy(args) {
            Ok(secs) => secs,
            Err(e) => return failure(&e),
        };
        let accelerator = match parse_accelerator(args) {
            Ok(a) => a,
            Err(e) => return failure(&e),
        };
        let requested = match get_str(args, "account") {
            Some(text) => match AccountId::new(text) {
                Ok(id) => Some(id),
                Err(e) => return failure(&e),
            },
            None => None,
        };
        // Provisioning runs on the active account only; the resolved
        // account is pinned into the operation so its result is attributed
        // even if the active account changes mid-flight.
        let view = match self
            .run(context, |backend, cancel| backend.accounts(cancel))
            .await
        {
            Ok(view) => view,
            Err(e) => return failure(&e),
        };
        let account = match (requested, view.active) {
            (Some(want), Some(active)) if want == active => active,
            (Some(want), Some(_)) => {
                return failure(
                    &CoreError::new(ErrorKind::AccountNotActive)
                        .with("account", want.as_str())
                        .detail(
                            "sessions start on the active account; switch accounts \
                             in the GUI first",
                        ),
                );
            }
            (_, None) => {
                return failure(
                    &CoreError::new(ErrorKind::NoActiveAccount)
                        .detail("no account is active; add and sign in to one in the GUI first"),
                );
            }
            (None, Some(active)) => active,
        };

        let defaults = self.backend.defaults();
        let name = self.backend.new_session_name(get_str(args, "name_prefix"));
        let mut spec =
            ProvisionSpec::new(name, accelerator.unwrap_or(defaults.accelerator), &defaults);
        if let Some(high_mem) = args.get("high_mem").and_then(Value::as_bool) {
            spec.high_mem = high_mem;
        }
        // v1 policy, replacing whatever the defaults carried: the runtime
        // cap is the only guard, enforced by the VM watchdog itself.
        spec.guard = GuardPolicy {
            max_runtime: Some(Duration::from_secs(max_runtime_secs)),
            max_spend_cu: None,
            min_balance_cu: None,
            idle_timeout: None,
        };
        if let Err(e) = spec.validate() {
            return failure(&e);
        }
        let (op, _created) = self.ops.start(
            self.backend.clone(),
            request_id,
            account,
            spec,
            max_runtime_secs,
        );
        success(op.view())
    }
}

fn empty_schema() -> Arc<serde_json::Map<String, Value>> {
    Arc::new(
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
            .as_object()
            .unwrap()
            .clone(),
    )
}

fn schema(value: Value) -> Arc<Map<String, Value>> {
    Arc::new(value.as_object().unwrap().clone())
}

fn tool(name: &'static str, description: &'static str, read_only: bool) -> Tool {
    tool_full(
        name,
        description,
        read_only,
        false,
        read_only,
        empty_schema(),
    )
}

fn tool_full(
    name: &'static str,
    description: &'static str,
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    input_schema: Arc<Map<String, Value>>,
) -> Tool {
    let mut tool = Tool::new(name, description, input_schema);
    tool.annotations = Some(
        ToolAnnotations::new()
            .read_only(read_only)
            .destructive(destructive)
            .idempotent(idempotent)
            .open_world(false),
    );
    tool
}

fn ref_schema(extra_required: &[&str]) -> Arc<Map<String, Value>> {
    let mut required: Vec<Value> = vec![json!("account"), json!("endpoint")];
    required.extend(extra_required.iter().map(|f| json!(f)));
    schema(json!({
        "type": "object",
        "properties": {
            "account": { "type": "string" },
            "endpoint": { "type": "string" },
            "name": { "type": "string" },
            "additional_secs": { "type": "integer", "minimum": 1 },
        },
        "required": required,
        "additionalProperties": false,
    }))
}

fn tools() -> Vec<Tool> {
    vec![
        tool(
            "colab_status",
            "Environment status: CLI presence and version, backend kind, data directory.",
            true,
        ),
        tool(
            "accounts_list",
            "Saved accounts, which one is active, and the state of the CLI credential slot. Not read-only: if an earlier add-account flow was interrupted, listing first completes its rollback, which can restore or clear the CLI's active credential.",
            false,
        ),
        tool(
            "usage_get",
            "Cached per-account compute-unit balance and burn rate, with sampled-at times. No I/O.",
            true,
        ),
        tool(
            "usage_refresh",
            "Fetch a fresh compute-unit usage sample for every saved account.",
            false,
        ),
        tool(
            "sessions_list",
            "Runtimes on every saved account, managed or not. Listing reconciles against the server and can write the ledger, so it is not read-only.",
            false,
        ),
        tool_full(
            "sessions_start",
            "Start a session asynchronously: returns an operation_id at once; poll operations_get until it succeeds or fails. request_id is required and makes retries safe — the same request_id always returns the same operation and never provisions twice. Provisioning runs on the active account. Guard mode is VM-watchdog-only: policy.max_runtime_secs is required (no default) and is the only policy field accepted; max_spend_cu, min_balance_cu and idle_timeout_secs are rejected by name, never silently dropped.",
            false,
            false,
            false,
            schema(json!({
                "type": "object",
                "properties": {
                    "request_id": { "type": "string" },
                    "account": { "type": "string" },
                    "name_prefix": { "type": "string" },
                    "accelerator": {
                        "type": "string",
                        "enum": ["CPU", "T4", "L4", "A100", "H100", "G4", "V5E1", "V6E1"],
                    },
                    "high_mem": { "type": "boolean" },
                    "policy": {
                        "type": "object",
                        "properties": {
                            "max_runtime_secs": { "type": "integer", "minimum": 1 },
                        },
                        "required": ["max_runtime_secs"],
                        "additionalProperties": true,
                    },
                },
                "required": ["request_id", "policy"],
                "additionalProperties": false,
            })),
        ),
        tool_full(
            "operations_get",
            "Read one operation's state: stage while running, the final result once succeeded, the structured error once failed. Local state only — polling never calls Google. Operations live in this server process; if it restarted, reconcile with sessions_list instead.",
            true,
            false,
            true,
            schema(json!({
                "type": "object",
                "properties": { "operation_id": { "type": "string" } },
                "required": ["operation_id"],
                "additionalProperties": false,
            })),
        ),
        tool_full(
            "operations_cancel",
            "Cancel an in-flight operation (its provisioning is rolled back by the core). A finished operation keeps its final state.",
            false,
            false,
            true,
            schema(json!({
                "type": "object",
                "properties": { "operation_id": { "type": "string" } },
                "required": ["operation_id"],
                "additionalProperties": false,
            })),
        ),
        tool_full(
            "sessions_stop",
            "Stop a runtime and end its billing; the stop is recorded as a user stop. Errors such as stop_unconfirmed and orphan_suspected are returned verbatim — surface them, never retry blindly.",
            false,
            true,
            false,
            ref_schema(&[]),
        ),
        tool_full(
            "sessions_extend",
            "Move a session's auto-stop later by additional_secs (never earlier). Returns the watchdog state now in force.",
            false,
            false,
            false,
            ref_schema(&["additional_secs"]),
        ),
        tool(
            "guard_poll",
            "One host-guard evaluation: decisions for every tracked live session, sessions found gone, fresh usage. In v1 no background loop runs the host guards — this call is the evaluation, and the VM watchdog is the standing guard.",
            false,
        ),
    ]
}

/// The tool call's arguments as an object (absent means empty).
fn args(request: &CallToolRequestParams) -> Map<String, Value> {
    request.arguments.clone().unwrap_or_default()
}

fn get_str<'a>(args: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    args.get(field).and_then(Value::as_str)
}

fn req_str<'a>(args: &'a Map<String, Value>, field: &str) -> Result<&'a str, CoreError> {
    get_str(args, field)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::invalid(field, format!("{field} is required")))
}

fn get_u64(args: &Map<String, Value>, field: &str) -> Option<u64> {
    args.get(field).and_then(Value::as_u64)
}

/// Policy fields a host-side guard would have to enforce. v1 has no
/// resident driver, so they are refused by name (design section 3, Guard).
const HOST_POLICY_FIELDS: [&str; 3] = ["max_spend_cu", "min_balance_cu", "idle_timeout_secs"];

/// Validates a `sessions_start` policy and returns the runtime cap.
/// `max_runtime_secs` is required — a v1 session without the one guard it
/// can have is refused, not defaulted.
fn validate_policy(args: &Map<String, Value>) -> Result<u64, CoreError> {
    let Some(policy) = args.get("policy").and_then(Value::as_object) else {
        return Err(CoreError::invalid(
            "policy",
            "policy is required: v1 sessions need policy.max_runtime_secs",
        ));
    };
    for field in HOST_POLICY_FIELDS {
        if policy.get(field).is_some_and(|v| !v.is_null()) {
            return Err(CoreError::invalid(
                field,
                format!(
                    "{field} needs a host-side guard, which v1 does not run \
                     (VM-watchdog-only mode); only max_runtime_secs is accepted"
                ),
            ));
        }
    }
    match get_u64(policy, "max_runtime_secs") {
        Some(secs) if (1..=7 * 24 * 3600).contains(&secs) => Ok(secs),
        Some(_) => Err(CoreError::invalid(
            "policy.max_runtime_secs",
            "max_runtime_secs must be between 1 second and 7 days",
        )),
        None => Err(CoreError::invalid(
            "policy.max_runtime_secs",
            "policy.max_runtime_secs is required (no default in v1)",
        )),
    }
}

fn parse_accelerator(args: &Map<String, Value>) -> Result<Option<Accelerator>, CoreError> {
    let Some(text) = get_str(args, "accelerator") else {
        return Ok(None);
    };
    let parsed = match text {
        "CPU" => Accelerator::None,
        "T4" => Accelerator::T4,
        "L4" => Accelerator::L4,
        "A100" => Accelerator::A100,
        "H100" => Accelerator::H100,
        "G4" => Accelerator::G4,
        "V5E1" => Accelerator::TpuV5e1,
        "V6E1" => Accelerator::TpuV6e1,
        _ => {
            return Err(CoreError::invalid(
                "accelerator",
                format!("unknown accelerator {text:?}"),
            ));
        }
    };
    Ok(Some(parsed))
}

fn parse_ref(args: &Map<String, Value>) -> Result<RuntimeRef, CoreError> {
    Ok(RuntimeRef {
        account: AccountId::new(req_str(args, "account")?)?,
        endpoint: req_str(args, "endpoint")?.to_string(),
        name: get_str(args, "name").map(str::to_string),
    })
}

/// A core success as a tool result: the value as structured content, plus the
/// same JSON as text for hosts that only read text content.
fn success(value: Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(value);
    result
}

/// The structured body of a core failure: the kind slug as `code` is the
/// machine-readable part an agent branches on.
pub(crate) fn error_body(error: &CoreError) -> Value {
    let message = if error.detail.is_empty() {
        error.kind.as_str().to_string()
    } else {
        error.detail.clone()
    };
    json!({
        "code": error.kind.as_str(),
        "message": message,
        "next_step": error.next_step,
        "params": error.params,
    })
}

/// A core failure as a tool result (never a protocol error).
fn failure(error: &CoreError) -> CallToolResult {
    let body = error_body(error);
    let mut result = CallToolResult::error(vec![ContentBlock::text(body.to_string())]);
    result.structured_content = Some(body);
    result
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "colab-playground-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Colab Playground's guard layer over the Colab CLI: accounts, compute-unit \
                 balances, and sessions whose cost guards and ledger the GUI shares. Sessions \
                 started here are visible in the GUI and vice versa. sessions_start is \
                 asynchronous: it returns an operation_id to poll with operations_get.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: tools(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let result = match request.name.as_ref() {
            "colab_status" => match self
                .run(&context, |backend, cancel| backend.probe(cancel))
                .await
            {
                Ok(probe) => {
                    let mut status = self.status();
                    status["probe"] = serde_json::to_value(probe).unwrap_or(Value::Null);
                    success(status)
                }
                Err(e) => failure(&e),
            },
            "accounts_list" => match self
                .run(&context, |backend, cancel| backend.accounts(cancel))
                .await
            {
                Ok(view) => success(serde_json::to_value(view).unwrap_or(Value::Null)),
                Err(e) => failure(&e),
            },
            // Usage views are wrapped in an object: structured content must
            // be a JSON object, and the envelope names what the array is.
            "usage_get" => {
                let views = self.backend.usage();
                success(json!({ "accounts": views }))
            }
            "usage_refresh" => match self
                .run(&context, |backend, cancel| {
                    backend.refresh_usage(&[], cancel)
                })
                .await
            {
                Ok(views) => success(json!({ "accounts": views })),
                Err(e) => failure(&e),
            },
            "sessions_list" => match self
                .run(&context, |backend, cancel| backend.runtimes(cancel))
                .await
            {
                Ok(view) => success(serde_json::to_value(view).unwrap_or(Value::Null)),
                Err(e) => failure(&e),
            },
            "sessions_start" => self.sessions_start(&context, &args(&request)).await,
            "operations_get" => {
                let args = args(&request);
                match req_str(&args, "operation_id").and_then(|id| {
                    self.ops.get(id).ok_or_else(|| {
                        CoreError::invalid(
                            "operation_id",
                            "unknown operation; operations live in this server \
                                 process — if it restarted, reconcile with sessions_list",
                        )
                    })
                }) {
                    Ok(op) => success(op.view()),
                    Err(e) => failure(&e),
                }
            }
            "operations_cancel" => {
                let args = args(&request);
                match req_str(&args, "operation_id").and_then(|id| {
                    self.ops
                        .cancel(id)
                        .ok_or_else(|| CoreError::invalid("operation_id", "unknown operation"))
                }) {
                    Ok(op) => success(op.view()),
                    Err(e) => failure(&e),
                }
            }
            "sessions_stop" => {
                let args = args(&request);
                match parse_ref(&args) {
                    Ok(target) => {
                        let label = json!({
                            "account": target.account,
                            "endpoint": target.endpoint,
                            "name": target.name,
                        });
                        match self
                            .run(&context, move |backend, cancel| {
                                backend.stop_session(&target, StopOptions::default(), cancel)
                            })
                            .await
                        {
                            Ok(()) => success(json!({ "stopped": true, "session": label })),
                            Err(e) => failure(&e),
                        }
                    }
                    Err(e) => failure(&e),
                }
            }
            "sessions_extend" => {
                let args = args(&request);
                match parse_ref(&args).and_then(|target| {
                    get_u64(&args, "additional_secs")
                        .filter(|s| *s >= 1)
                        .map(|secs| (target, secs))
                        .ok_or_else(|| {
                            CoreError::invalid(
                                "additional_secs",
                                "additional_secs is required (at least 1)",
                            )
                        })
                }) {
                    Ok((target, secs)) => {
                        let label = json!({
                            "account": target.account,
                            "endpoint": target.endpoint,
                            "name": target.name,
                        });
                        match self
                            .run(&context, move |backend, cancel| {
                                backend.extend_session(&target, Duration::from_secs(secs), cancel)
                            })
                            .await
                        {
                            Ok(info) => success(json!({
                                "session": label,
                                "watchdog": info,
                                "watcher_mode": "vm_watchdog_only",
                            })),
                            Err(e) => failure(&e),
                        }
                    }
                    Err(e) => failure(&e),
                }
            }
            "guard_poll" => match self
                .run(&context, |backend, cancel| backend.guard_poll(cancel))
                .await
            {
                Ok(report) => success(json!({
                    "watcher_mode": "vm_watchdog_only",
                    "note": "no background loop runs the host-side guards in v1; \
                             this call is one evaluation, and the VM watchdog is \
                             the standing guard",
                    "report": report,
                })),
                Err(e) => failure(&e),
            },
            other => {
                return Err(McpError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("unknown tool: {other}"),
                    None,
                ));
            }
        };
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use colab_playground_core::ErrorKind;

    #[test]
    fn a_core_failure_is_a_tool_result_carrying_the_kind_slug() {
        let error = CoreError::new(ErrorKind::StopUnconfirmed).detail("listing failed");
        let result = failure(&error);
        assert_eq!(result.is_error, Some(true));
        let body = result.structured_content.unwrap();
        assert_eq!(body["code"], json!("stop_unconfirmed"));
        assert_eq!(body["message"], json!("listing failed"));
    }

    #[test]
    fn a_core_success_carries_structured_content() {
        let result = success(json!({ "balance_cu": 12.5 }));
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result.structured_content.unwrap()["balance_cu"],
            json!(12.5)
        );
    }

    fn policy_args(policy: Value) -> Map<String, Value> {
        json!({ "request_id": "r1", "policy": policy })
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn a_policy_with_only_a_runtime_cap_is_accepted() {
        let args = policy_args(json!({ "max_runtime_secs": 900 }));
        assert_eq!(validate_policy(&args).unwrap(), 900);
    }

    #[test]
    fn a_missing_runtime_cap_is_refused_not_defaulted() {
        let args = policy_args(json!({}));
        let error = validate_policy(&args).unwrap_err();
        assert!(error.is(ErrorKind::InvalidInput));
        assert_eq!(error.param("field"), Some("policy.max_runtime_secs"));
    }

    #[test]
    fn host_side_policy_fields_are_refused_by_name() {
        for field in ["max_spend_cu", "min_balance_cu", "idle_timeout_secs"] {
            let args = policy_args(json!({ "max_runtime_secs": 900, field: 5 }));
            let error = validate_policy(&args).unwrap_err();
            assert!(error.is(ErrorKind::InvalidInput));
            assert_eq!(error.param("field"), Some(field), "{field}");
        }
    }
}
