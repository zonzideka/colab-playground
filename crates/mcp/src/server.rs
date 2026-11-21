//! The MCP surface: one tool per `Backend` operation, shaped for an agent
//! caller (see `docs/mcp-design.md`). Business failures come back as tool
//! results with `isError` and a structured body carrying the core error's
//! kind slug, so a caller branches on the slug alone; only routing failures
//! (an unknown tool) are protocol errors.

use std::{path::PathBuf, sync::Arc};

use colab_playground_core::{CancelToken, Error as CoreError, ErrorKind, backend::Backend};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorCode,
        Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
        Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct McpServer {
    backend: Arc<dyn Backend>,
    backend_label: &'static str,
    data_dir: Option<PathBuf>,
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
}

fn empty_schema() -> Arc<serde_json::Map<String, Value>> {
    Arc::new(
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
            .as_object()
            .unwrap()
            .clone(),
    )
}

fn tool(name: &'static str, description: &'static str, read_only: bool) -> Tool {
    let mut tool = Tool::new(name, description, empty_schema());
    tool.annotations = Some(
        ToolAnnotations::new()
            .read_only(read_only)
            .destructive(false)
            .idempotent(read_only)
            .open_world(false),
    );
    tool
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
            "Saved accounts, which one is active, and the state of the CLI credential slot.",
            true,
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
    ]
}

/// A core success as a tool result: the value as structured content, plus the
/// same JSON as text for hosts that only read text content.
fn success(value: Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(value);
    result
}

/// A core failure as a tool result (never a protocol error): the kind slug
/// is the machine-readable code an agent branches on.
fn failure(error: &CoreError) -> CallToolResult {
    let message = if error.detail.is_empty() {
        error.kind.as_str().to_string()
    } else {
        error.detail.clone()
    };
    let body = json!({
        "code": error.kind.as_str(),
        "message": message,
        "next_step": error.next_step,
        "params": error.params,
    });
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
                 started here are visible in the GUI and vice versa.",
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
}
