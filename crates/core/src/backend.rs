//! What the UI talks to.
//!
//! [`Backend`] is object-safe and `Send + Sync`; every method is blocking and
//! meant to run on a background executor. Long operations stream typed events
//! through an [`Events`] sender and accept a [`CancelToken`].

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::accounts::{Account, AccountId, AccountsView};
use crate::clock::CancelToken;
use crate::defaults::Defaults;
use crate::error::{Error, ErrorKind, Result};
use crate::guard::Decision;
use crate::hf::HfModel;
use crate::host::{Line, ProbeReport};
use crate::plugins::{InputValue, PluginInfo, PluginPreview, ResolvedEndpoint};
use crate::provision::{ProvisionEvent, ProvisionOutcome, ProvisionSpec};
use crate::quant::GgufListing;
use crate::sessions::{Accelerator, StopReason, TrackedSession, WatchdogInfo};
use crate::tunnel::{TunnelEvent, TunnelId, TunnelInfo, TunnelSpec};
use crate::usage::{BalancePoint, UsageSnapshot};
use crate::vm::RunStatus;

/// Event channel for long operations (send errors are ignored).
pub type Events<T> = Sender<T>;

/// Which implementation is behind the trait (the UI shows "Demo data" for mock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Real,
    Mock,
}

/// How to install the Colab CLI (always an explicit user action).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
    /// `uv tool install google-colab-cli` (needs `uv`).
    Uv,
    /// Install `uv` with its official installer first, then as [`InstallMethod::Uv`].
    InstallUvThenCli,
    /// `python3 -m pip install --user google-colab-cli`.
    PipUser,
}

/// Interactive add-account flow. Dropping it without finishing rolls back,
/// restoring the previously active credential.
pub trait LoginFlow: Send {
    fn auth_url(&self) -> &str;
    /// On any failure the previous credential is restored and the flow must be
    /// started again.
    fn submit_code(self: Box<Self>, code: &str, cancel: &CancelToken) -> Result<Account>;
    /// Aborts and restores the previous credential.
    fn cancel(self: Box<Self>) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeRef {
    pub account: AccountId,
    pub endpoint: String,
    /// Local session name, if known.
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Runtime {
    pub id: RuntimeRef,
    pub account_email: Option<String>,
    /// The owner is the active account (exec, tunnels and plugin runs possible).
    pub on_active_account: bool,
    /// Parsed accelerator.
    pub accelerator: Option<Accelerator>,
    /// Raw hardware label.
    pub hardware: String,
    pub high_mem: bool,
    /// Created and tracked by this app.
    pub managed: bool,
    /// Ledger entry (start time, policy, spend, watchdog) when managed.
    pub tracked: Option<TrackedSession>,
    /// Estimated burn rate (≈).
    pub estimated_rate_cu_per_hour: Option<f64>,
    /// Estimated spend so far (≈).
    pub estimated_spend_cu: Option<f64>,
    /// When it will be stopped automatically (earliest of guard and watchdog).
    #[serde(default, with = "crate::clock::serde_secs::option")]
    pub auto_stop_at: Option<SystemTime>,
    pub tunnels: Vec<TunnelId>,
}

/// A per-account failure while listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountError {
    pub account: AccountId,
    pub error: Error,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimesView {
    /// Active account first.
    pub runtimes: Vec<Runtime>,
    /// Accounts that could not be listed.
    pub errors: Vec<AccountError>,
}

/// Options for stopping a runtime (more pre-stop steps will be added here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopOptions {
    /// Why it is stopped (recorded in the ledger).
    pub reason: StopReason,
}

impl Default for StopOptions {
    fn default() -> Self {
        StopOptions {
            reason: StopReason::User,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountUsageView {
    pub account: AccountId,
    pub email: Option<String>,
    /// Last snapshot (possibly stale).
    pub snapshot: Option<UsageSnapshot>,
    /// Time left at the current rate.
    #[serde(default, with = "crate::clock::serde_dur::option")]
    pub eta: Option<Duration>,
    /// Recent balances, oldest first.
    pub history: Vec<BalancePoint>,
    /// Runtimes on the account the app does not manage.
    pub unmanaged_runtimes: u32,
    #[serde(default)]
    pub ownership_verified: bool,
    /// Estimated rate per managed session name (≈).
    pub session_rates: BTreeMap<String, f64>,
    pub last_error: Option<Error>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuardDecision {
    pub session: RuntimeRef,
    pub decision: Decision,
    /// The guard stopped it in this poll.
    pub stopped: bool,
    /// The stop failed.
    pub error: Option<Error>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuardReport {
    /// Decisions for every tracked live session.
    pub decisions: Vec<GuardDecision>,
    /// Sessions found gone since the last poll, with why they ended.
    pub vanished: Vec<Vanished>,
    pub usage: Vec<AccountUsageView>,
}

/// A session found gone since the last poll, and why it ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vanished {
    pub runtime: RuntimeRef,
    /// Why it ended: confirmed as the user's stop, or it simply disappeared.
    pub reason: StopReason,
}

/// What switching to another account affects (shown before confirming).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwitchImpact {
    pub from: Option<AccountId>,
    pub to: AccountId,
    pub tunnels_closing: Vec<TunnelInfo>,
    /// Managed sessions of the current account that keep running (and
    /// billing) under their watchdog.
    pub sessions_left_running: Vec<TrackedSession>,
    /// Runtimes on the current account the app does not manage (e.g. opened
    /// in the browser). They keep billing too and have no watchdog. `None`
    /// when they could not be listed: unknown, never none.
    #[serde(default)]
    pub unmanaged_left_running: Option<Vec<RuntimeRef>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunTarget {
    /// An existing session on the active account.
    Existing {
        name: String,
    },
    Provision(ProvisionSpec),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginRunSpec {
    pub plugin: String,
    /// Input values by id (missing ones take manifest defaults).
    pub inputs: BTreeMap<String, InputValue>,
    pub target: RunTarget,
    /// Stop a session provisioned for this run when the run fails. (A
    /// cancelled run always stops the session it provisioned.)
    pub stop_on_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PluginRunEvent {
    /// Provisioning progress (fresh sessions only).
    Provision(ProvisionEvent),
    Launched {
        session: String,
        /// Run id (run directory name on the VM).
        run_id: String,
    },
    /// `status.json` changed.
    Status(RunStatus),
    /// New log lines (secrets redacted).
    Log {
        /// Lines.
        lines: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginRunOutcome {
    pub session: String,
    pub account: AccountId,
    pub run_id: String,
    /// Final status (`ready`).
    pub status: RunStatus,
    pub endpoints: Vec<ResolvedEndpoint>,
}

pub trait Backend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn defaults(&self) -> Defaults;
    /// Replaces the defaults (the app persists them in its settings file).
    fn set_defaults(&self, defaults: Defaults);
    /// Current time of the backend's clock.
    fn now(&self) -> SystemTime;

    // ---- environment
    /// Reports WSL, tools, `colab` and its helper interpreter.
    fn probe(&self, cancel: &CancelToken) -> Result<ProbeReport>;
    fn install_cli(
        &self,
        method: InstallMethod,
        log: Events<Line>,
        cancel: &CancelToken,
    ) -> Result<()>;

    // ---- accounts
    /// Saved accounts and the CLI slot (rolls back an interrupted add).
    fn accounts(&self, cancel: &CancelToken) -> Result<AccountsView>;
    /// Saves the credential currently in the CLI slot as an account.
    fn adopt_active_account(&self, cancel: &CancelToken) -> Result<Account>;
    fn begin_add_account(&self, cancel: &CancelToken) -> Result<Box<dyn LoginFlow>>;
    /// What switching to `to` affects (no side effects).
    fn switch_impact(&self, to: &AccountId) -> Result<SwitchImpact>;
    /// Makes `to` the active account (closes tunnels first).
    fn switch_account(&self, to: &AccountId, cancel: &CancelToken) -> Result<AccountsView>;
    /// Removes a saved, non-active account.
    fn remove_account(&self, id: &AccountId, cancel: &CancelToken) -> Result<()>;

    // ---- usage
    /// Fetches usage for `accounts` (all saved accounts if empty), without switching.
    fn refresh_usage(
        &self,
        accounts: &[AccountId],
        cancel: &CancelToken,
    ) -> Result<Vec<AccountUsageView>>;
    /// Cached usage (no I/O).
    fn usage(&self) -> Vec<AccountUsageView>;
    /// True when a background usage refresh is due (cadence from [`Defaults`]).
    fn usage_poll_due(&self) -> bool;

    // ---- runtimes and sessions
    /// Runtimes on every saved account, managed or not.
    fn runtimes(&self, cancel: &CancelToken) -> Result<RuntimesView>;
    /// A fresh readable name such as `a100-3f9c` (`prefix` defaults to the
    /// default accelerator).
    fn new_session_name(&self, prefix: Option<&str>) -> String;
    /// Provisions a session on the active account with its guard.
    fn provision(
        &self,
        spec: ProvisionSpec,
        events: Events<ProvisionEvent>,
        cancel: &CancelToken,
    ) -> Result<ProvisionOutcome>;
    /// Stops any runtime on any saved account and ends its billing.
    fn stop_session(
        &self,
        target: &RuntimeRef,
        options: StopOptions,
        cancel: &CancelToken,
    ) -> Result<()>;
    /// Moves the auto-stop to `runtime` from now (active account only); also
    /// sets one again after [`Backend::cancel_auto_stop`].
    ///
    /// The stop is put on the runtime itself, so it holds with the app
    /// closed. A running watchdog gets the new deadline. A watchdog that is
    /// not running (cancelled, exited, or never started) is started again
    /// the way provisioning starts it, with the new deadline and the
    /// session's tracked idle rule, and the cancel is cleared. The returned
    /// [`WatchdogInfo`] is also recorded in the ledger.
    ///
    /// If that start fails, the result is what provisioning reports when it
    /// cannot start the watchdog: the call still succeeds, and the returned
    /// info has `armed == false` and no deadline, meaning the stop is not on
    /// the runtime. The app-side runtime limit is recorded in either case, so
    /// the guard still stops the runtime at the new time while the app runs;
    /// recording it never makes a runtime bill longer than not recording it.
    ///
    /// Errors (with nothing recorded) are those of reaching the runtime, e.g.
    /// `AccountNotActive` or `SessionGone`.
    fn extend_session(
        &self,
        target: &RuntimeRef,
        runtime: Duration,
        cancel: &CancelToken,
    ) -> Result<WatchdogInfo>;
    /// Cancels the VM watchdog's deadline (active account only). The watchdog
    /// exits without releasing the runtime; the ledger records it as
    /// cancelled and not armed, and drops the app-side runtime limit.
    fn cancel_auto_stop(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<()>;
    /// One app-side guard round: usage, spend, decisions, automatic stops.
    fn guard_poll(&self, cancel: &CancelToken) -> Result<GuardReport>;

    // ---- tunnels
    /// Opens a port forward (the session's account must be active).
    fn open_tunnel(&self, spec: TunnelSpec, events: Events<TunnelEvent>) -> Result<TunnelId>;
    /// Closes a tunnel (billing continues).
    fn close_tunnel(&self, id: TunnelId) -> Result<()>;
    fn tunnels(&self) -> Vec<TunnelInfo>;

    // ---- plugins
    fn plugins(&self) -> Vec<PluginInfo>;
    /// A run request pre-filled from the manifest and [`Defaults`] (fresh
    /// session with a generated name).
    fn plugin_run_template(&self, plugin: &str) -> Result<PluginRunSpec>;
    /// Validates a local plugin folder for the install confirmation.
    fn inspect_plugin_folder(&self, path: &Path) -> Result<PluginPreview>;
    /// Clones a Git URL with the local `git` and validates it.
    fn inspect_plugin_git(&self, url: &str, cancel: &CancelToken) -> Result<PluginPreview>;
    fn install_plugin(&self, preview: &PluginPreview) -> Result<PluginInfo>;
    fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<()>;
    /// Removes a user plugin.
    fn remove_plugin(&self, id: &str) -> Result<()>;
    /// Runs a plugin until it reports `ready` (or fails / is cancelled).
    fn run_plugin(
        &self,
        spec: PluginRunSpec,
        events: Events<PluginRunEvent>,
        cancel: &CancelToken,
    ) -> Result<PluginRunOutcome>;

    // ---- app secrets (stored in the keyring vault; never returned to the UI)
    /// Stores a secret such as `hf_token`.
    fn set_secret(&self, name: &str, value: &str, cancel: &CancelToken) -> Result<()>;
    fn has_secret(&self, name: &str, cancel: &CancelToken) -> Result<bool>;
    fn delete_secret(&self, name: &str, cancel: &CancelToken) -> Result<()>;

    // ---- Hugging Face
    fn hf_search(&self, query: &str, limit: u32, cancel: &CancelToken) -> Result<Vec<HfModel>>;
    fn hf_files(&self, repo: &str, cancel: &CancelToken) -> Result<GgufListing>;
    // ---- updates
    /// Whether a newer release of this app is published (one request to GitHub; nothing is
    /// downloaded). Backends that cannot check say so.
    fn check_for_update(&self, cancel: &CancelToken) -> Result<crate::update::UpdateStatus> {
        let _ = cancel;
        Err(Error::new(ErrorKind::Unsupported).detail("this backend cannot check for updates"))
    }
}

/// `<prefix>-<4 hex>`, unique among `taken`. The prefix is reduced to
/// `[a-z0-9-]` (at most 20 chars, `session` if empty).
pub fn generate_session_name(prefix: &str, taken: &[String]) -> String {
    let mut p: String = prefix
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    p = p
        .trim_matches('-')
        .chars()
        .take(20)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned();
    if p.is_empty() {
        p = "session".into();
    }
    loop {
        let name = format!("{p}-{}", &crate::accounts::nonce()[..4]);
        if !taken.contains(&name) {
            return name;
        }
    }
}

/// Default name prefix for an accelerator (`a100`, `cpu`, `v5e1`).
pub fn accelerator_prefix(a: Accelerator) -> String {
    if a == Accelerator::None {
        "cpu".into()
    } else {
        a.as_str().to_ascii_lowercase()
    }
}

pub fn run_template(info: &PluginInfo, defaults: &Defaults, taken: &[String]) -> PluginRunSpec {
    let m = &info.manifest;
    let mut spec = defaults.provision_spec(generate_session_name(&m.id, taken));
    if let Some(a) = m.provision.accelerator {
        spec.accelerator = a;
    }
    spec.high_mem = m.provision.high_mem || spec.high_mem;
    spec.min_vram_mib = (m.provision.min_vram_mib > 0)
        .then_some(m.provision.min_vram_mib)
        .or(spec.min_vram_mib);
    if !spec.accelerator.is_gpu() {
        spec.min_vram_mib = None;
    }
    let inputs = m
        .inputs
        .iter()
        .filter_map(|i| i.default.clone().map(|d| (i.id.clone(), d)))
        .collect();
    PluginRunSpec {
        plugin: m.id.clone(),
        inputs,
        target: RunTarget::Provision(spec),
        stop_on_failure: true,
    }
}

pub fn usage_view(
    account: &AccountId,
    email: Option<String>,
    usage: &crate::usage::AccountUsage,
    known_sessions: usize,
) -> AccountUsageView {
    AccountUsageView {
        account: account.clone(),
        email,
        snapshot: usage.snapshot,
        eta: usage.snapshot.and_then(|s| s.eta()),
        history: usage.history.points(),
        unmanaged_runtimes: usage.unmanaged_runtimes(known_sessions),
        ownership_verified: usage.ownership_verified,
        session_rates: usage.attribution.rates(),
        last_error: usage.last_error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_short_valid_and_unique() {
        let taken = vec![];
        let n = generate_session_name("llama-cpp", &taken);
        assert!(
            n.starts_with("llama-cpp-") && n.len() == "llama-cpp-".len() + 4,
            "{n}"
        );
        crate::sessions::validate_session_name(&n).unwrap();
        let n = generate_session_name("  Weird Name!! ", &taken);
        assert!(n.starts_with("weird-name-"), "{n}");
        assert!(generate_session_name("", &taken).starts_with("session-"));
        assert_eq!(accelerator_prefix(Accelerator::None), "cpu");
        assert_eq!(accelerator_prefix(Accelerator::TpuV5e1), "v5e1");
        let a = generate_session_name("x", &taken);
        let b = generate_session_name("x", std::slice::from_ref(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn the_trait_is_object_safe() {
        fn takes(_: Option<&dyn Backend>) {}
        takes(None);
        fn send_sync<T: Send + Sync + ?Sized>() {}
        send_sync::<dyn Backend>();
    }
}
