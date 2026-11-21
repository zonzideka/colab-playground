# MCP Interface Design

Status: open questions decided by the user on 2026-11-20, adopting the local
agent's (zonzi's) caller-side review on every point (see section 7).
Implemented so far: rollout steps 2–3 (2026-11-21) — the
`colab-playground-mcp` crate over stdio, against the mock and the real
backend, with the section 5 driver lock. Step 2: `colab_status`,
`accounts_list`, `usage_get`, `usage_refresh`, `sessions_list`. Step 3:
the session lifecycle — `sessions_start` as operations with `request_id`
idempotency, `operations_get`, `operations_cancel`, `sessions_stop`,
`sessions_extend`, `guard_poll` — in VM-watchdog-only guard mode, with an
operation reporting success only once the watchdog is confirmed armed.
Operations live in the server process's memory; a restart loses them, and
`sessions_list` is the reconciliation path (the sessions themselves are in
the ledger either way). Tunnels, plugins, models and secrets follow the
rollout in section 8.
Goal: let a local agent (running on the same machine, under the same user
account) drive Colab Playground's core directly — list accounts and balances,
start and stop sessions, run guard checks, open tunnels, run plugins — without
the GUI and without the browser.

Positioning: Google's hosted Colab MCP endpoint (2026-11-16) already offers
bare runtime provisioning and code execution. This server is the layer it
does not provide: policies, cost guards, the ledger, multiple accounts,
plugins and tunnels. Where a tool overlaps a Google tool, ours is the same
action plus an account pinned, a policy enforced and a ledger entry written.

## 1. Form factor

- A new headless binary in this workspace, `colab-playground-mcp`, built on
  `colab-playground-core` only. It constructs the same `Backend` the GUI uses
  and exposes it over MCP. The GUI app is not modified.
- Transport: **stdio** for v1. The agent spawns the binary as a subprocess;
  there is no listening socket, no network surface, and no separate auth —
  the trust boundary is "same OS user", which is exactly the local-agent
  setup this is for. An HTTP transport can be added later behind the same
  tool implementations if a remote agent ever needs it.
- The server shares the app's data directory (accounts, ledger, plugins), so
  state is continuous with the GUI: a session started from the GUI is visible
  to the agent and vice versa. See section 5 for the concurrency rule this
  requires.
- Not to be confused with ROADMAP's "The Colab API as a backend": that item
  is about consuming Google Colab's own hosted MCP endpoint as an
  alternative `Backend` implementation. This document is the opposite
  direction — exposing this app's existing CLI-backed core to a local
  agent. The two are independent and could coexist.

## 2. Design principles

1. **One-to-one with the core, shaped for an agent.** Every tool maps to a
   `Backend` operation. Nothing is reimplemented; cost guards, the ledger,
   and the VM-side watchdog behave identically to the GUI.
2. **No session without an auto-stop.** `sessions_start` requires
   `max_runtime_secs` (the GUI's policy field). There is no way to open an
   unbounded session through MCP.
3. **Secrets are write-only.** A secret value can be set through MCP; no tool
   ever returns one. `secrets_has` returns a boolean only.
4. **Errors are machine-readable.** Core already classifies failures with
   stable kind slugs (`orphan_suspected`, `stop_unconfirmed`,
   `unsupported_cli_version`, ...). MCP errors carry that slug as the error
   code, plus the human-readable detail and the suggested next step when the
   core provides one. An agent must be able to branch on the slug alone.
5. **Long operations are operations.** Provisioning takes longer than
   agent hosts let a tool call run, so it is modelled as an operation the
   caller polls (section 3), fed by the core's existing event streams
   (`ProvisionEvent`). Plugin runs may additionally use MCP progress
   notifications tied to `PluginEvent`.
6. **Read tools are safe to poll.** Listing accounts, usage, and sessions has
   no side effects beyond the core's normal reconciliation (the same thing
   the GUI does on refresh), and results state their freshness (usage views
   carry the time they were sampled).

## 3. Tool catalog (v1)

### Status and accounts

| Tool | Core call | Notes |
| --- | --- | --- |
| `colab_status` | `probe` | CLI presence/version, backend kind, data-dir state. |
| `accounts_list` | `accounts` | Ids, emails, which account is active. Not annotated read-only: the query can complete an interrupted add-account rollback, which can restore or clear the CLI's active credential (see section 6). |
| `accounts_switch` | `switch_impact` + `switch_account` | Impact is checked first; switching away from an account with live sessions is refused unless `force` is set. |
| `accounts_remove` | `remove_account` | Destructive; see annotations. |

Sign-in is **out of scope for v1**: the OAuth login flow is interactive
(browser round-trip plus a pasted code) and the GUI already does it well.
The agent uses accounts the user has signed in to. If this changes, the
`LoginFlow` maps to a two-step `accounts_add_begin` / `accounts_add_complete`
pair — noted here so the naming stays reserved.

### Usage

| Tool | Core call | Notes |
| --- | --- | --- |
| `usage_get` | `usage` | Per-account balance, burn rate, per-session attribution; includes sampled-at timestamps. |
| `usage_refresh` | `refresh_usage` | Forces a fresh sample instead of returning the cached view. |

### Sessions

| Tool | Core call | Notes |
| --- | --- | --- |
| `sessions_list` | `runtimes` | Managed and unmanaged runtimes: endpoint, accelerator, auto-stop deadline, estimated spend, tunnels. Not annotated strictly read-only: listing reconciles and can write the ledger (section 6). |
| `sessions_start` | `new_session_name` + `provision` | Asynchronous: returns an `operation_id` immediately (see "Operations" below). See schema below. |
| `operations_get` | local operation state | Stage, suggested poll interval, final result. Reads local state only; polling never calls Google. |
| `operations_cancel` | the operation's `CancelToken` | Explicit cancellation of an in-flight operation. |
| `sessions_stop` | `stop_session` | `reason` is always recorded as a user stop. Returns the core outcome verbatim, including `stop_unconfirmed` / `orphan_suspected` errors — the agent must surface those, never retry blindly. |
| `sessions_extend` | `extend_session` | Adds time to the auto-stop; never moves it earlier (core rule). |

`sessions_cancel_auto_stop` is **not exposed in v1**: disarming the only
guard a v1 session has (section 3, Guard) is not something an agent should
be able to do silently.

### Operations

Provisioning takes longer than agent hosts allow a single tool call to run
(measured: 60 s tool timeouts), so `sessions_start` is asynchronous:

- It validates the request, resolves and **pins the account into the
  operation**, records the operation locally, starts provisioning in the
  background, and returns an `operation_id` at once.
- The caller passes a `request_id`. A retry with the same `request_id`
  returns the same operation — provisioning is never duplicated. This
  mirrors the official API's own design (CreateRuntime is an Operation;
  its `requestId` makes retries idempotent).
- `operations_get` reports the stage, a suggested poll interval, and, once
  finished, the final result or error. After a disconnect, the caller
  re-reads the original operation and reconciles instead of starting over.
- An operation reports **ready only after the watchdog is confirmed
  armed** — a session whose auto-stop failed to arm is a failed operation,
  not a ready session.

`sessions_start` input (sketch):

```json
{
  "request_id": "9b1f…",             // REQUIRED, caller-generated, idempotency
  "account": "ad82aeab0f4",          // optional; default: active account
  "name_prefix": "train",            // optional; core generates the name
  "accelerator": "T4",               // CPU | T4 | L4 | A100 | H100 | G4 ...
  "high_mem": false,
  "policy": {
    "max_runtime_secs": 900          // REQUIRED, no default; the only policy
                                     // field v1 accepts (see Guard)
  }
}
```

Final result (via `operations_get`): the session's `RuntimeRef` (account,
endpoint, name), hardware actually allocated (which may differ from the
request — Colab assigns what it assigns), watchdog state, and the policy
actually in force.

### Guard

| Tool | Core call | Notes |
| --- | --- | --- |
| `guard_poll` | `guard_poll` | One guard evaluation: sessions near a limit, sessions the guard stopped, failures it could not act on. |

**v1 runs in an explicitly labelled "VM-watchdog-only" mode** (decided
2026-11-20, section 7). The VM-side watchdog enforces max runtime inside
the runtime itself, independent of any process on this machine. The spend,
balance and idle guards are app-side: they only run while a driver process
is alive, and v1 has no resident driver, so:

- `sessions_start` accepts `max_runtime_secs` only. A request carrying
  `max_spend_cu`, `min_balance_cu` or `idle_timeout_secs` is **rejected**
  with a structured error naming the unsupported field — never silently
  accepted and downgraded.
- `sessions_start` results and `guard_poll` reports state the watcher
  mode and the policy actually in force, so a caller can see that only
  the runtime cap is armed.
- `guard_poll` in v1 is a diagnostic: it reports what a guard evaluation
  finds, but no background loop acts on it.

The long-term direction is a resident driver on the same machine holding
the data directory exclusively and running provision / reconcile / guard,
with stdio as a frontend only. That is a later version, not v1.

### Tunnels

| Tool | Core call | Notes |
| --- | --- | --- |
| `tunnels_list` | `tunnels` | |
| `tunnels_open` | `open_tunnel` | Session name + local/remote ports; progress events until the tunnel is up. |
| `tunnels_close` | `close_tunnel` | By tunnel id. |

### Plugins and models

| Tool | Core call | Notes |
| --- | --- | --- |
| `plugins_list` | `plugins` | Installed plugins and enabled state. |
| `plugins_run` | `plugin_run_template` + `run_plugin` | Runs a plugin (e.g. the bundled llama.cpp deploy) against a session; progress notifications carry the run's log lines; result carries the run outcome and any service endpoint the plugin reports. |
| `models_search` | `hf_search` | Hugging Face model search for plugin runs. |
| `models_files` | `hf_files` | GGUF file listing for a repo. |

Plugin installation (`inspect_plugin_folder` / `inspect_plugin_git` /
`install_plugin`) and plugin management (`set_plugin_enabled`,
`remove_plugin`) are **v2**: installation runs third-party code and should
stay a deliberate GUI action until the agent story needs it.

### Secrets

| Tool | Core call | Notes |
| --- | --- | --- |
| `secrets_set` | `set_secret` | Write-only. For tokens a plugin run needs (e.g. `HF_TOKEN`). |
| `secrets_has` | `has_secret` | Boolean only. |
| `secrets_delete` | `delete_secret` | Destructive annotation. |

### Updates

`colab_update_check` (`check_for_update`) is informational only; the server
never updates itself on an agent's say-so.

## 4. Error model

- Business errors are tool results with `isError: true` and a structured
  body: the core error's kind slug as `code`, the detail string as the
  message, and — when present — the core's structured fields (e.g.
  `endpoint`) and a `next_step` hint. Protocol-level failures (bad request
  shape, unknown tool) stay JSON-RPC errors. An agent branches on `code`
  alone; the two channels are never mixed.
- Cancellation: an MCP request cancellation maps to the core's
  `CancelToken`, the same path the GUI's cancel buttons use.
- A tool that partially succeeded before failing (the stop flow is the known
  example) reports the failure kind the core returned; side effects already
  performed are described in the detail. Agents must treat
  `stop_unconfirmed` as "probably stopped, verify with `sessions_list`",
  which is exactly what the core's own bookkeeping does on the next
  reconcile.

## 5. Concurrency: one driver at a time

Core's state lock is in-process. The GUI and the MCP server are separate
processes sharing one data directory, and both write the ledger. v1 rule:
**a cross-process lock file in the data directory**, held for the lifetime
of whichever driver is serving; a second driver fails fast with a clear
`driver_already_running` error naming the holder. This matches how the
state is actually used (one human, one machine) and avoids split-brain
ledger writes. Revisit only if a real need for simultaneous drivers shows
up. The per-account CLI state file keeps its own locking, unchanged.

As implemented (2026-11-21): the lock lives in core — `RealBackend::new`
acquires it through the `driver_lock` registry, which shares it within a
process (so the GUI's in-place backend rebuild never fights itself) and
fails a second process with the `driver_already_running` kind; the GUI
surfaces that as its own error message, the server prints it and exits.
Known gap: the server does not yet read the GUI's saved host configuration
(a non-default WSL distribution or explicit CLI/interpreter paths chosen
in Settings); it uses automatic discovery and the default distribution.
That parity belongs in core, next to the settings store, and is a
follow-up — it only affects Windows setups that changed those settings.

## 6. Annotations and safety

- Read-only tools (`colab_status`, `usage_get`,
  `tunnels_list`, `plugins_list`, `models_*`, `secrets_has`,
  `colab_update_check`) are annotated read-only.
- `sessions_list` is **not** annotated read-only: the core's listing path
  reconciles against the server and can write the ledger (recording a
  session that ended, finalising a pending stop). The same applies to any
  future tool whose core call reconciles.
- `accounts_list` is **not** annotated read-only either: when a previous
  add-account flow was interrupted, the accounts query completes its
  rollback before answering, and that rollback can restore or clear the
  CLI's active credential. (A code review caught the first implementation
  claiming otherwise; the tool description says this out loud.)
- Destructive tools (`accounts_remove`, `sessions_stop`,
  `secrets_delete`) are annotated destructive so agent hosts can ask the
  user first.
- The server binds nothing, opens no ports, and serves only the process
  that spawned it (stdio).

## 7. Decisions (2026-11-20)

All four questions were decided by the user on 2026-11-20, adopting the
local agent's (zonzi's) caller-side review on every point:

1. **Guard loop ownership → VM-watchdog-only v1.** v1 ships the explicitly
   labelled mode described in section 3 (Guard): only max runtime is
   enforced, host-side policies are rejected rather than downgraded, and
   results report the policy actually in force. A resident driver is the
   long-term direction, not a v1 requirement.
2. **Login flow → out of v1.** Sign-in stays in the GUI; the agent uses
   accounts the user has already added. The reserved tool names stand.
3. **Provision → operation id + polling.** `sessions_start` returns an
   operation id immediately; `operations_get` reads local state; caller
   `request_id` makes retries idempotent; cancellation is explicit;
   reconnects reconcile against the original operation; ready is reported
   only once the watchdog is confirmed armed. (Section 3, Operations.)
4. **SDK → the official Rust SDK (`rmcp`)**, with protocol dependencies
   confined to the `colab-playground-mcp` crate; core stays MCP-free.
   stdio plus the generic tool surface is the compatibility baseline —
   hosts are not required to support MCP Tasks.

Two verification principles are acceptance criteria, not aspirations:

- **Kill the host mid-provision, reconnect, and no second runtime may be
  created** — the original operation is re-read and reconciled.
- **Interrupt a run mid-flight, and a guard that was promised must still
  hold** — in v1 terms: a session reported ready has its VM-side watchdog
  armed, and nothing in the operation layer can leave it unarmed.

## 8. Rollout

1. This spec, decided (section 7).
2. ~~`colab-playground-mcp` crate with the read-only tools first
   (status / accounts / usage / sessions list) against the mock backend,
   then the real backend.~~ Done 2026-11-21. The driver lock (section 5)
   landed with this step rather than step 3, because `sessions_list`
   already reconciles and writes the ledger on the real backend.
3. ~~Session lifecycle tools (start as operations / stop / extend / guard)
   with the concurrency lock, including the two kill-and-reconnect
   verification principles from section 7 as scripted tests.~~ Done
   2026-11-21. The principles are covered as scripted tests against the
   mock backend: a retried `request_id` never provisions twice, and a
   cancelled start ends cancelled with the core rolling the allocation
   back. (The lock landed with step 2, in core.)
4. ~~Tunnels, plugins, models, secrets.~~ Done 2026-11-21: `tunnels_list`,
   `tunnels_open`, `tunnels_close`, `plugins_list`, `plugins_run`,
   `models_search`, `models_files`, `secrets_set`, `secrets_has`,
   `secrets_delete`, `colab_update_check`. Two v1 scopings: `plugins_run`
   targets an existing session only (a plugin-driven provision would bypass
   the sessions_start guard validation), and `tunnels_open` / `plugins_run`
   forward their core events as MCP progress notifications when the caller
   supplies a progress token. Plugin installation and management stay
   GUI-only, as section 3 states.
5. Manual smoke: the local agent drives a full cycle — status, start a
   15-minute CPU session, list, stop — and the ledger records a user stop.
   (This is also the standing retest for the stop-reason bookkeeping.)
