//! A deterministic, clock-driven simulation of the whole backend.
//!
//! The mock keeps a small world model (accounts with balances that burn down
//! while runtimes exist, managed and unmanaged runtimes on several accounts,
//! tunnels, plugin runs) and reuses the real provisioning loop, guard, plugin
//! validation and quantisation fitting. Time comes from the injected clock and
//! every simulated step waits `step_delay` on it:
//!
//! * headless UI tests: [`MockConfig::headless`] uses a [`ManualClock`], so
//!   everything completes instantly while virtual time advances;
//! * interactive `--mock` runs: [`MockConfig::interactive`] uses the system
//!   clock with a human-paced delay.
//!
//! [`MockFailures`] injects each failure the UI must handle.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use crate::accounts::{Account, AccountId, AccountsView};
use crate::backend::*;
use crate::cli::StopOutcome;
use crate::clock::{CancelToken, Clock, ManualClock, SystemClock};
use crate::defaults::Defaults;
use crate::error::{Error, ErrorKind, Result};
use crate::guard::{Decision, GuardPolicy, SpendMeter, evaluate};
use crate::hf::HfModel;
use crate::host::{Host, Line, ProbeReport, Stream};
use crate::plugins::{
    PluginInfo, PluginOrigin, PluginPreview, PluginStore, resolve_endpoints, resolve_inputs,
};
use crate::provision::{
    self, Hardware, ProvisionEvent, ProvisionOps, ProvisionOutcome, ProvisionSpec,
};
use crate::quant::{FitInput, GgufFile, GgufListing, fit_auto, group};
use crate::sessions::{
    Accelerator, Ledger, ServerSession, StopReason, StopRecord, TrackedSession, WatchdogInfo,
};
use crate::tunnel::{TunnelEvent, TunnelId, TunnelInfo, TunnelSpec, TunnelState};
use crate::usage::{AccountUsage, Usage, poll_due};
use crate::vm::{RunState, RunStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockLoginFailure {
    CancelledBeforeUrl,
    InvalidCode,
    VerificationFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockFailures {
    /// `colab` is not installed (until `install_cli` succeeds).
    pub cli_missing: bool,
    pub install_fails: bool,
    pub keyring_unavailable: bool,
    pub login: Option<MockLoginFailure>,
    /// Accelerators the accounts are not entitled to.
    pub not_entitled: Vec<Accelerator>,
    /// Accelerators Colab never answers a request for: `colab new` waits
    /// [`ALLOCATION_HANG`] and fails with `AllocationTimeout`. Cancelled during
    /// that wait, the request still gets an unnamed VM that provisioning must
    /// clean up.
    pub allocation_hangs: Vec<Accelerator>,
    /// GPU memory of successive A100 allocations (the last value repeats).
    pub a100_vram_sequence: Vec<u32>,
    /// The first N tunnel connections fail with HTTP 429 (`u32::MAX`: always).
    pub ssh_busy_attempts: u32,
    /// A plugin run fails at this stage id.
    pub plugin_fail_stage: Option<String>,
    /// Usage fetches fail (snapshots turn stale).
    pub usage_fetch_fails: bool,
    /// The CLI's credential slot holds a signed-in Google account that is not
    /// saved yet, so `accounts()` reports `unadopted` (and no active account)
    /// until `adopt_active_account`. Applied at construction and whenever the
    /// switch is turned on with [`MockBackend::set_failures`]; turning it off
    /// signs that credential out.
    pub unadopted_slot: bool,
    /// An add-account flow was interrupted (as after a crash): the credential
    /// slot is empty with a backup pending, and the next `accounts()` restores
    /// it and reports `recovered_pending_add` once. Applied at construction and
    /// whenever the switch is turned on.
    pub interrupted_add: bool,
    /// The VM watchdog cannot be started again: setting an auto-stop on a
    /// runtime whose watchdog is not running (after a cancel) leaves the stop
    /// app-side only (`WatchdogInfo::armed` is false). Watchdogs started by
    /// provisioning, and extending one that runs, are not affected.
    pub watchdog_restart_fails: bool,
    /// The update check finds a newer release (the next patch version).
    pub update_available: bool,
    /// The update check fails (as if GitHub could not be reached).
    pub update_check_fails: bool,
    /// `switch_impact` cannot list the unmanaged runtimes either.
    pub runtimes_listing_fails: bool,
    /// Accounts (by email) whose runtimes cannot be listed: listings leave their runtimes out
    /// and report each account in `RuntimesView::errors`. Leaving such an account affects
    /// `switch_impact` as `runtimes_listing_fails` does.
    pub unlisted_accounts: Vec<String>,
}

impl Default for MockFailures {
    fn default() -> Self {
        MockFailures {
            cli_missing: false,
            install_fails: false,
            keyring_unavailable: false,
            login: None,
            not_entitled: vec![],
            allocation_hangs: vec![],
            a100_vram_sequence: vec![40_960, 81_920],
            ssh_busy_attempts: 0,
            plugin_fail_stage: None,
            usage_fetch_fails: false,
            unadopted_slot: false,
            interrupted_add: false,
            watchdog_restart_fails: false,
            update_available: false,
            update_check_fails: false,
            runtimes_listing_fails: false,
            unlisted_accounts: vec![],
        }
    }
}

/// How long a simulated `colab new` waits for an answer that never comes
/// ([`MockFailures::allocation_hangs`]): four two-minute tries and the CLI's
/// backoff between them.
pub const ALLOCATION_HANG: Duration = Duration::from_secs(4 * 120 + 70);

#[derive(Debug, Clone, PartialEq)]
pub struct MockAccount {
    pub email: String,
    /// Starting balance in compute units.
    pub balance_cu: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MockRuntime {
    /// Index into [`MockScenario::accounts`].
    pub account: usize,
    /// Local name (`None` = created in the browser, unmanaged).
    pub name: Option<String>,
    pub accelerator: Accelerator,
    /// Tracked by the app (ledger entry with the default guard policy).
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MockScenario {
    /// Saved accounts; the first is active. Empty = signed out.
    pub accounts: Vec<MockAccount>,
    pub runtimes: Vec<MockRuntime>,
}

impl MockScenario {
    /// Two accounts; on the active one two managed sessions (A100 + T4) and a
    /// runtime opened in the browser; one runtime on the second account.
    pub fn demo() -> Self {
        MockScenario {
            accounts: vec![
                MockAccount {
                    email: "ada@example.com".into(),
                    balance_cu: 87.5,
                },
                MockAccount {
                    email: "lin@example.com".into(),
                    balance_cu: 240.0,
                },
            ],
            runtimes: vec![
                MockRuntime {
                    account: 0,
                    name: Some("a100-7f3a".into()),
                    accelerator: Accelerator::A100,
                    managed: true,
                },
                MockRuntime {
                    account: 0,
                    name: Some("t4-91c2".into()),
                    accelerator: Accelerator::T4,
                    managed: true,
                },
                MockRuntime {
                    account: 0,
                    name: None,
                    accelerator: Accelerator::L4,
                    managed: false,
                },
                MockRuntime {
                    account: 1,
                    name: Some("cpu-0b5e".into()),
                    accelerator: Accelerator::None,
                    managed: true,
                },
            ],
        }
    }

    pub fn single() -> Self {
        MockScenario {
            accounts: vec![MockAccount {
                email: "ada@example.com".into(),
                balance_cu: 100.0,
            }],
            runtimes: vec![],
        }
    }

    /// No account yet (first run).
    pub fn signed_out() -> Self {
        MockScenario {
            accounts: vec![],
            runtimes: vec![],
        }
    }
}

#[derive(Clone)]
pub struct MockConfig {
    pub clock: Arc<dyn Clock>,
    /// Wait per simulated step (on `clock`).
    pub step_delay: Duration,
    pub failures: MockFailures,
    pub scenario: MockScenario,
    pub defaults: Defaults,
}

impl MockConfig {
    /// Instant: manual clock (returned for advancing), 5 s of virtual time per step.
    pub fn headless(scenario: MockScenario) -> (MockConfig, ManualClock) {
        let clock = ManualClock::fixed();
        (
            MockConfig {
                clock: Arc::new(clock.clone()),
                step_delay: Duration::from_secs(5),
                failures: MockFailures::default(),
                scenario,
                defaults: Defaults::default(),
            },
            clock,
        )
    }

    /// Human-paced: system clock, `step_delay` per step.
    pub fn interactive(step_delay: Duration) -> MockConfig {
        MockConfig {
            clock: Arc::new(SystemClock),
            step_delay,
            failures: MockFailures::default(),
            scenario: MockScenario::demo(),
            defaults: Defaults::default(),
        }
    }
}

/// Simulated hourly rate of an accelerator (CU/h).
pub fn mock_rate(a: Accelerator) -> f64 {
    match a {
        Accelerator::None => 0.08,
        Accelerator::T4 => 1.76,
        Accelerator::L4 => 4.82,
        Accelerator::G4 => 8.2,
        Accelerator::A100 => 11.77,
        Accelerator::H100 => 18.3,
        Accelerator::TpuV5e1 => 3.9,
        Accelerator::TpuV6e1 => 6.4,
    }
}

fn mock_gpu(a: Accelerator, vram: Option<u32>) -> Option<String> {
    match a {
        Accelerator::T4 => Some("Tesla T4".into()),
        Accelerator::L4 => Some("NVIDIA L4".into()),
        Accelerator::G4 => Some("NVIDIA RTX PRO 6000 Blackwell Server Edition".into()),
        Accelerator::A100 => Some(
            if vram.unwrap_or(0) > 50_000 {
                "NVIDIA A100-SXM4-80GB"
            } else {
                "NVIDIA A100-SXM4-40GB"
            }
            .into(),
        ),
        Accelerator::H100 => Some("NVIDIA H100 80GB HBM3".into()),
        _ => None,
    }
}

fn mock_vram(a: Accelerator) -> Option<u32> {
    match a {
        Accelerator::T4 => Some(15_360),
        Accelerator::L4 => Some(23_034),
        Accelerator::G4 => Some(97_887),
        Accelerator::H100 => Some(81_559),
        Accelerator::A100 => Some(40_960),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct MAccount {
    id: AccountId,
    email: String,
    created_at: SystemTime,
    balance: f64,
}

#[derive(Debug, Clone)]
struct MRuntime {
    account: AccountId,
    endpoint: String,
    name: Option<String>,
    accelerator: Accelerator,
    high_mem: bool,
    vram_mib: Option<u32>,
    /// Its VM watchdog is running (a cancelled one exits).
    watchdog_alive: bool,
}

struct World {
    cli_installed: bool,
    accounts: Vec<MAccount>,
    active: Option<AccountId>,
    runtimes: Vec<MRuntime>,
    balance_at: SystemTime,
    ledger: Ledger,
    usage: BTreeMap<AccountId, AccountUsage>,
    tunnels: BTreeMap<TunnelId, TunnelInfo>,
    next_tunnel: u64,
    ssh_busy_left: u32,
    plugins: PluginStore,
    secrets: BTreeMap<String, String>,
    a100_attempt: usize,
    defaults: Defaults,
    reserved: u32,
    next_id: u32,
    failures: MockFailures,
    /// The slot holds this not-yet-saved credential (email).
    unadopted_email: Option<String>,
    /// An add-account backup is pending: the account that was in the slot.
    pending_add: Option<Option<AccountId>>,
    login_live: bool,
    update_checks: u32,
}

/// Email of the credential `MockFailures::unadopted_slot` puts in the slot.
pub const MOCK_UNADOPTED_EMAIL: &str = "pat@example.com";

impl World {
    /// Applies switches that model state (rather than being read on use)
    /// when they change from `old`.
    fn apply_switches(&mut self, old: &MockFailures) {
        let new = self.failures.clone();
        if new.cli_missing != old.cli_missing {
            self.cli_installed = !new.cli_missing;
        }
        self.ssh_busy_left = new.ssh_busy_attempts;
        if new.a100_vram_sequence != old.a100_vram_sequence {
            self.a100_attempt = 0;
        }
        if new.unadopted_slot && !old.unadopted_slot {
            self.active = None;
            self.unadopted_email = Some(MOCK_UNADOPTED_EMAIL.to_owned());
        } else if !new.unadopted_slot && old.unadopted_slot {
            self.unadopted_email = None;
        }
        if new.interrupted_add && !old.interrupted_add && self.pending_add.is_none() {
            let previous = self.active.take();
            self.pending_add = Some(previous);
            self.tunnels.clear();
        }
    }

    /// Restores the account backed up by a pending add (no-op without one).
    fn rollback_add(&mut self) -> bool {
        match self.pending_add.take() {
            Some(previous) => {
                self.active = previous;
                true
            }
            None => false,
        }
    }

    fn adopt_unadopted(&mut self, now: SystemTime) -> Option<AccountId> {
        let email = self.unadopted_email.take()?;
        let id = match self.accounts.iter().find(|a| a.email == email) {
            Some(a) => a.id.clone(),
            None => {
                let id = self.fresh_account_id();
                self.accounts.push(MAccount {
                    id: id.clone(),
                    email,
                    created_at: now,
                    balance: 100.0,
                });
                id
            }
        };
        self.active = Some(id.clone());
        Some(id)
    }

    fn fresh_account_id(&self) -> AccountId {
        let mut n = self.accounts.len() + 1;
        loop {
            let id = AccountId::new(format!("mock{n}")).unwrap_or_else(|_| AccountId::generate());
            if !self.accounts.iter().any(|a| a.id == id) {
                return id;
            }
            n += 1;
        }
    }

    fn unmanaged_on(&self, account: &AccountId) -> Vec<RuntimeRef> {
        self.runtimes
            .iter()
            .filter(|r| &r.account == account)
            .filter(|r| {
                r.name
                    .as_ref()
                    .is_none_or(|n| self.ledger.live(account, n).is_none())
            })
            .map(|r| RuntimeRef {
                account: r.account.clone(),
                endpoint: r.endpoint.clone(),
                name: r.name.clone(),
            })
            .collect()
    }

    fn listing_fails_for(&self, account: &AccountId) -> bool {
        self.failures.runtimes_listing_fails
            || self
                .email(account)
                .is_some_and(|email| self.failures.unlisted_accounts.contains(&email))
    }

    /// Burns balances up to `now`.
    fn settle(&mut self, now: SystemTime) {
        let hours = now
            .duration_since(self.balance_at)
            .unwrap_or_default()
            .as_secs_f64()
            / 3600.0;
        if hours > 0.0 {
            for a in &mut self.accounts {
                let rate: f64 = self
                    .runtimes
                    .iter()
                    .filter(|r| r.account == a.id)
                    .map(|r| mock_rate(r.accelerator))
                    .sum();
                a.balance = (a.balance - rate * hours).max(0.0);
            }
            self.balance_at = now;
        }
    }

    fn usage_of(&self, id: &AccountId) -> Option<Usage> {
        let a = self.accounts.iter().find(|a| &a.id == id)?;
        let rts: Vec<&MRuntime> = self.runtimes.iter().filter(|r| &r.account == id).collect();
        Some(Usage {
            balance_cu: a.balance,
            rate_cu_per_hour: rts.iter().map(|r| mock_rate(r.accelerator)).sum(),
            assignments: rts.len() as u32,
        })
    }

    fn live_names(&self, id: &AccountId) -> BTreeSet<String> {
        self.ledger
            .entries()
            .iter()
            .filter(|e| &e.account == id && e.stopped.is_none())
            .map(|e| e.name.clone())
            .collect()
    }

    fn endpoint(&mut self) -> String {
        self.next_id += 1;
        format!("m-mock-{:04x}", self.next_id * 7919 % 65536)
    }

    fn email(&self, id: &AccountId) -> Option<String> {
        self.accounts
            .iter()
            .find(|a| &a.id == id)
            .map(|a| a.email.clone())
    }

    fn account_views(&self) -> Vec<Account> {
        self.accounts
            .iter()
            .map(|a| Account {
                id: a.id.clone(),
                email: Some(a.email.clone()),
                created_at: a.created_at,
                updated_at: a.created_at,
                usage: self.usage.get(&a.id).and_then(|u| u.snapshot),
            })
            .collect()
    }

    fn reconcile(&mut self, id: &AccountId, now: SystemTime) -> (bool, Vec<Vanished>) {
        if self.listing_fails_for(id) {
            return (false, Vec::new());
        }
        let live: Vec<_> = self
            .ledger
            .entries()
            .iter()
            .filter(|e| &e.account == id && e.stopped.is_none())
            .cloned()
            .collect();
        let mut vanished = Vec::new();
        let mut unmatched = Vec::new();
        let mut verified = true;
        for e in live {
            let endpoint = e.endpoint.clone().or_else(|| {
                self.runtimes
                    .iter()
                    .find(|r| &r.account == id && r.name.as_deref() == Some(&e.name))
                    .map(|r| r.endpoint.clone())
            });
            let Some(endpoint) = endpoint else {
                unmatched.push(e.name);
                continue;
            };
            if self
                .runtimes
                .iter()
                .any(|r| &r.account == id && r.endpoint == endpoint)
            {
                if e.endpoint.is_none()
                    && self
                        .ledger
                        .update(id, &e.name, |e| e.endpoint = Some(endpoint))
                        .is_err()
                {
                    verified = false;
                }
            } else {
                if self
                    .ledger
                    .update(id, &e.name, |e| {
                        e.stopped = Some(StopRecord {
                            at: now,
                            reason: StopReason::Vanished,
                        })
                    })
                    .is_err()
                {
                    verified = false;
                }
                vanished.push(Vanished {
                    runtime: RuntimeRef {
                        account: id.clone(),
                        endpoint,
                        name: Some(e.name),
                    },
                    reason: StopReason::Vanished,
                });
            }
        }
        // Like the real backend: a session without an endpoint can only be a runtime that
        // no other live session claims.
        let claimed = |endpoint: &str| {
            self.ledger.entries().iter().any(|e| {
                &e.account == id && e.stopped.is_none() && e.endpoint.as_deref() == Some(endpoint)
            })
        };
        let all_claimed = self
            .runtimes
            .iter()
            .filter(|r| &r.account == id)
            .all(|r| claimed(&r.endpoint));
        if all_claimed {
            for name in unmatched {
                if self
                    .ledger
                    .update(id, &name, |e| {
                        e.stopped = Some(StopRecord {
                            at: now,
                            reason: StopReason::Vanished,
                        })
                    })
                    .is_err()
                {
                    verified = false;
                }
                vanished.push(Vanished {
                    runtime: RuntimeRef {
                        account: id.clone(),
                        endpoint: String::new(),
                        name: Some(name),
                    },
                    reason: StopReason::Vanished,
                });
            }
        } else if !unmatched.is_empty() {
            verified = false;
        }
        (verified, vanished)
    }

    fn record_usage(&mut self, id: &AccountId, now: SystemTime, verified: bool) {
        let live = self.live_names(id);
        let fails = self.failures.usage_fetch_fails;
        let reading = self.usage_of(id);
        let entry = self.usage.entry(id.clone()).or_default();
        match (fails, reading) {
            (false, Some(u)) => {
                entry.record_success(u, now, &live);
                entry.ownership_verified = live.is_empty() || verified;
            }
            _ => entry.record_failure(
                Error::new(ErrorKind::Http).detail("simulated usage failure"),
                now,
            ),
        }
    }

    fn usage_views(&self) -> Vec<AccountUsageView> {
        self.accounts
            .iter()
            .map(|a| {
                let u = self.usage.get(&a.id).cloned().unwrap_or_default();
                let known = self
                    .ledger
                    .entries()
                    .iter()
                    .filter(|e| e.account == a.id && e.stopped.is_none())
                    .count();
                usage_view(&a.id, Some(a.email.clone()), &u, known)
            })
            .collect()
    }

    fn set_tunnel(
        &mut self,
        id: TunnelId,
        state: TunnelState,
        events: Option<&Events<TunnelEvent>>,
    ) {
        if let Some(t) = self.tunnels.get_mut(&id) {
            t.state = state.clone();
        }
        if let Some(tx) = events {
            let _ = tx.send(TunnelEvent::State { id, state });
        }
    }

    fn close_session_tunnels(&mut self, account: &AccountId, name: &str) {
        let ids: Vec<TunnelId> = self
            .tunnels
            .values()
            .filter(|t| &t.spec.account == account && t.spec.session == name)
            .map(|t| t.id)
            .collect();
        for id in ids {
            self.tunnels.remove(&id);
        }
    }
}

#[derive(Clone)]
pub struct MockBackend {
    cfg: Arc<MockConfig>,
    world: Arc<Mutex<World>>,
}

impl std::fmt::Debug for MockBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockBackend").finish_non_exhaustive()
    }
}

impl MockBackend {
    pub fn new(cfg: MockConfig) -> MockBackend {
        let now = cfg.clock.now();
        let accounts: Vec<MAccount> = cfg
            .scenario
            .accounts
            .iter()
            .enumerate()
            .map(|(i, a)| MAccount {
                id: AccountId::new(format!("mock{}", i + 1))
                    .unwrap_or_else(|_| AccountId::generate()),
                email: a.email.clone(),
                created_at: now - Duration::from_secs(86_400 * (30 + i as u64)),
                balance: a.balance_cu,
            })
            .collect();
        let mut world = World {
            cli_installed: !cfg.failures.cli_missing,
            active: accounts.first().map(|a| a.id.clone()),
            accounts,
            runtimes: vec![],
            balance_at: now,
            ledger: Ledger::in_memory(),
            usage: BTreeMap::new(),
            tunnels: BTreeMap::new(),
            next_tunnel: 0,
            ssh_busy_left: cfg.failures.ssh_busy_attempts,
            plugins: PluginStore::load(None, None).expect("bundled plugins are valid"),
            secrets: BTreeMap::new(),
            a100_attempt: 0,
            defaults: cfg.defaults.clone(),
            reserved: 0,
            next_id: 0,
            failures: cfg.failures.clone(),
            unadopted_email: None,
            pending_add: None,
            login_live: false,
            update_checks: 0,
        };
        let off = MockFailures {
            cli_missing: cfg.failures.cli_missing,
            ..MockFailures::default()
        };
        world.apply_switches(&off);
        for (i, r) in cfg.scenario.runtimes.iter().enumerate() {
            let Some(account) = world.accounts.get(r.account).map(|a| a.id.clone()) else {
                continue;
            };
            let endpoint = world.endpoint();
            let started = now - Duration::from_secs(600 * (i as u64 + 1));
            let policy = world.defaults.guard_policy();
            let watchdog = match (r.managed, &r.name) {
                (true, Some(_)) => policy.max_runtime.map(|d| WatchdogInfo {
                    deadline: Some(started + d),
                    idle_timeout: policy.idle_timeout,
                    cancelled: false,
                    armed: true,
                }),
                _ => None,
            };
            world.runtimes.push(MRuntime {
                account: account.clone(),
                endpoint: endpoint.clone(),
                name: r.name.clone(),
                accelerator: r.accelerator,
                high_mem: false,
                vram_mib: mock_vram(r.accelerator),
                watchdog_alive: watchdog.is_some(),
            });
            if let (true, Some(name)) = (r.managed, &r.name) {
                let _ = world.ledger.upsert(TrackedSession {
                    name: name.clone(),
                    account: account.clone(),
                    endpoint: Some(endpoint),
                    accelerator: r.accelerator,
                    high_mem: false,
                    gpu: mock_gpu(r.accelerator, mock_vram(r.accelerator)),
                    vram_mib: mock_vram(r.accelerator),
                    watchdog,
                    policy,
                    spend: SpendMeter::new(started),
                    stopped: None,
                    stop_pending_confirmation: None,
                });
            }
        }
        MockBackend {
            cfg: Arc::new(cfg),
            world: Arc::new(Mutex::new(world)),
        }
    }

    /// How many update checks were made (the simulation never contacts GitHub).
    pub fn update_checks(&self) -> u32 {
        self.w().update_checks
    }

    /// Replaces the failure switches at runtime (UI tests flip them mid-test).
    /// Every switch takes effect immediately: switches read on use apply to
    /// the next call; state switches (`cli_missing`, `unadopted_slot`,
    /// `interrupted_add`, `ssh_busy_attempts`, `a100_vram_sequence`) are
    /// applied when they change.
    pub fn set_failures(&self, failures: MockFailures) {
        let mut w = self.w();
        let old = std::mem::replace(&mut w.failures, failures);
        w.apply_switches(&old);
    }

    /// Adds an unmanaged runtime (as if opened in the browser) to `account`.
    pub fn add_unmanaged_runtime(&self, account: &AccountId, accelerator: Accelerator) -> String {
        let now = self.cfg.clock.now();
        let mut w = self.w();
        w.settle(now);
        let endpoint = w.endpoint();
        w.runtimes.push(MRuntime {
            account: account.clone(),
            endpoint: endpoint.clone(),
            name: None,
            accelerator,
            high_mem: false,
            vram_mib: mock_vram(accelerator),
            watchdog_alive: false,
        });
        endpoint
    }

    fn w(&self) -> MutexGuard<'_, World> {
        self.world.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now_(&self) -> SystemTime {
        self.cfg.clock.now()
    }

    fn step(&self, cancel: &CancelToken) -> Result<()> {
        self.cfg.clock.sleep(self.cfg.step_delay, cancel)
    }

    fn ready(&self) -> Result<()> {
        let w = self.w();
        if !w.cli_installed {
            return Err(Error::new(ErrorKind::NotInstalled).with("program", "colab"));
        }
        if w.failures.keyring_unavailable {
            return Err(Error::new(ErrorKind::KeyringUnavailable));
        }
        Ok(())
    }

    fn require_active(&self, account: &AccountId) -> Result<()> {
        self.ready()?;
        if self.w().active.as_ref() != Some(account) {
            return Err(Error::new(ErrorKind::AccountNotActive).with("account", account));
        }
        Ok(())
    }

    fn active(&self) -> Result<AccountId> {
        self.ready()?;
        self.w()
            .active
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::NoActiveAccount))
    }

    fn view(&self) -> AccountsView {
        let w = self.w();
        AccountsView {
            accounts: w.account_views(),
            active: w.active.clone(),
            has_token: w.active.is_some() || w.unadopted_email.is_some(),
            unadopted: w.unadopted_email.is_some(),
            active_email: w
                .active
                .as_ref()
                .and_then(|a| w.email(a))
                .or_else(|| w.unadopted_email.clone()),
            recovered_pending_add: false,
        }
    }

    fn stop_inner(&self, target: &RuntimeRef, reason: StopReason) -> Result<()> {
        // Like the real backend: without an endpoint it cannot tell that the runtime is gone.
        if target.endpoint.is_empty() {
            return Err(
                Error::new(ErrorKind::OrphanSuspected).detail("the session's runtime is not known")
            );
        }
        let now = self.now_();
        let mut w = self.w();
        w.settle(now);
        let before = w.runtimes.len();
        w.runtimes
            .retain(|r| !(r.account == target.account && r.endpoint == target.endpoint));
        if w.runtimes.len() == before {
            return Err(Error::new(ErrorKind::SessionGone).with("endpoint", &target.endpoint));
        }
        let name = target.name.clone().or_else(|| {
            w.ledger
                .entries()
                .iter()
                .find(|e| {
                    e.account == target.account
                        && e.endpoint.as_deref() == Some(&target.endpoint)
                        && e.stopped.is_none()
                })
                .map(|e| e.name.clone())
        });
        if let Some(n) = name {
            w.close_session_tunnels(&target.account, &n);
            let _ = w.ledger.update(&target.account, &n, |e| {
                e.stopped = Some(StopRecord { at: now, reason })
            });
        }
        Ok(())
    }
}

struct MockOps<'a> {
    m: &'a MockBackend,
    account: AccountId,
    /// Watchdog starts are restarts (`MockFailures::watchdog_restart_fails` applies).
    restart: bool,
}

impl ProvisionOps for MockOps<'_> {
    fn sessions(&self, _: &CancelToken) -> Result<Vec<ServerSession>> {
        let w = self.m.w();
        Ok(w.runtimes
            .iter()
            .filter(|r| r.account == self.account)
            .map(|r| ServerSession {
                name: r.name.clone(),
                endpoint: r.endpoint.clone(),
                hardware: if r.accelerator == Accelerator::None {
                    "CPU".into()
                } else {
                    r.accelerator.as_str().into()
                },
                accelerator: Some(r.accelerator),
                shape: if r.high_mem {
                    "High-RAM".into()
                } else {
                    "Standard".into()
                },
                variant: if r.accelerator.is_gpu() {
                    "GPU"
                } else if r.accelerator.is_tpu() {
                    "TPU"
                } else {
                    "DEFAULT"
                }
                .into(),
                status: None,
            })
            .collect())
    }

    fn create(
        &self,
        name: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<()> {
        let hangs = self.m.w().failures.allocation_hangs.contains(&accelerator);
        if hangs {
            if let Err(e) = self.m.cfg.clock.sleep(ALLOCATION_HANG, cancel) {
                // Killed mid-request, which Colab still answered with a VM.
                self.m.add_unmanaged_runtime(&self.account, accelerator);
                return Err(e);
            }
            return Err(Error::new(ErrorKind::AllocationTimeout).detail(
                "ReadTimeout: HTTPSConnectionPool(host='colab.research.google.com', port=443): Read timed out. (read timeout=120)",
            ));
        }
        self.m.step(cancel)?;
        let now = self.m.now_();
        let mut w = self.m.w();
        if w.failures.not_entitled.contains(&accelerator) {
            return Err(
                Error::new(ErrorKind::NotEntitled).with("accelerator", accelerator.as_str())
            );
        }
        w.settle(now);
        let vram = if accelerator == Accelerator::A100 {
            let seq = w.failures.a100_vram_sequence.clone();
            let v = seq
                .get(w.a100_attempt)
                .or(seq.last())
                .copied()
                .unwrap_or(40_960);
            w.a100_attempt += 1;
            Some(v)
        } else {
            mock_vram(accelerator)
        };
        let endpoint = w.endpoint();
        w.runtimes.push(MRuntime {
            account: self.account.clone(),
            endpoint,
            name: Some(name.to_owned()),
            accelerator,
            high_mem,
            vram_mib: vram,
            watchdog_alive: false,
        });
        Ok(())
    }

    fn probe(&self, name: &str, cancel: &CancelToken) -> Result<Hardware> {
        self.m.step(cancel)?;
        let w = self.m.w();
        let r = w
            .runtimes
            .iter()
            .find(|r| r.account == self.account && r.name.as_deref() == Some(name))
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        Ok(Hardware {
            gpu: mock_gpu(r.accelerator, r.vram_mib),
            vram_mib: r.vram_mib,
        })
    }

    fn stop(&self, name: &str, _: &CancelToken) -> Result<StopOutcome> {
        let now = self.m.now_();
        let mut w = self.m.w();
        w.settle(now);
        let before = w.runtimes.len();
        w.runtimes
            .retain(|r| !(r.account == self.account && r.name.as_deref() == Some(name)));
        Ok(if w.runtimes.len() < before {
            StopOutcome::Terminated
        } else {
            StopOutcome::NotFound
        })
    }

    fn reap(&self, endpoint: &str, _: &CancelToken) -> Result<bool> {
        let mut w = self.m.w();
        let before = w.runtimes.len();
        w.runtimes
            .retain(|r| !(r.account == self.account && r.endpoint == endpoint));
        Ok(w.runtimes.len() < before)
    }

    fn start_watchdog(
        &self,
        name: &str,
        policy: &GuardPolicy,
        cancel: &CancelToken,
    ) -> Result<Option<WatchdogInfo>> {
        self.m.step(cancel)?;
        let now = self.m.now_();
        let mut w = self.m.w();
        if self.restart && w.failures.watchdog_restart_fails {
            return Err(Error::new(ErrorKind::Other).detail("the watchdog process is not running"));
        }
        let rt = w
            .runtimes
            .iter_mut()
            .find(|r| r.account == self.account && r.name.as_deref() == Some(name))
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        rt.watchdog_alive = true;
        Ok(Some(WatchdogInfo {
            deadline: policy.max_runtime.map(|d| now + d),
            idle_timeout: policy.idle_timeout,
            cancelled: false,
            armed: true,
        }))
    }
}

struct MockLoginFlow {
    m: MockBackend,
    url: String,
    finished: bool,
}

impl Drop for MockLoginFlow {
    /// Like the real flow: anything but success restores the previous account.
    fn drop(&mut self) {
        if !self.finished {
            let mut w = self.m.w();
            w.rollback_add();
            w.login_live = false;
        }
    }
}

impl LoginFlow for MockLoginFlow {
    fn auth_url(&self) -> &str {
        &self.url
    }

    fn submit_code(mut self: Box<Self>, code: &str, cancel: &CancelToken) -> Result<Account> {
        if code.trim().is_empty() {
            return Err(Error::invalid("code", "paste the code exactly as shown"));
        }
        self.m.step(cancel)?;
        match self.m.w().failures.login {
            Some(MockLoginFailure::InvalidCode) => {
                return Err(Error::new(ErrorKind::LoginInvalidCode));
            }
            Some(MockLoginFailure::VerificationFailed) => {
                return Err(Error::new(ErrorKind::LoginVerificationFailed));
            }
            _ => {}
        }
        let now = self.m.now_();
        let mut w = self.m.w();
        let id = w.fresh_account_id();
        let email = format!("user{}@example.com", w.accounts.len() + 1);
        w.accounts.push(MAccount {
            id: id.clone(),
            email: email.clone(),
            created_at: now,
            balance: 100.0,
        });
        w.active = Some(id.clone());
        w.pending_add = None;
        w.login_live = false;
        w.tunnels.clear();
        drop(w);
        self.finished = true;
        Ok(Account {
            id,
            email: Some(email),
            created_at: now,
            updated_at: now,
            usage: None,
        })
    }

    fn cancel(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

fn catalog() -> Vec<(&'static str, f64)> {
    // (repo, billions of parameters) for synthetic file sizes
    vec![
        ("unsloth/Qwen3-0.6B-GGUF", 0.6),
        ("unsloth/Qwen3-8B-GGUF", 8.2),
        ("bartowski/ScarletKc_ScarletKc-2.0-GGUF", 27.0),
        ("unsloth/gpt-oss-20b-GGUF", 20.9),
        ("bartowski/Llama-3.3-70B-Instruct-GGUF", 70.6),
    ]
}

fn mock_listing(repo: &str) -> GgufListing {
    let params = catalog()
        .into_iter()
        .find(|(r, _)| *r == repo)
        .map(|(_, p)| p)
        .unwrap_or(7.0);
    let name = repo
        .rsplit('/')
        .next()
        .unwrap_or("model")
        .trim_end_matches("-GGUF");
    let bits = [
        ("BF16", 16.0),
        ("Q8_0", 8.5),
        ("Q6_K", 6.56),
        ("Q5_K_M", 5.69),
        ("Q4_K_M", 4.85),
        ("IQ4_XS", 4.25),
        ("Q3_K_M", 3.91),
    ];
    let files: Vec<GgufFile> = bits
        .iter()
        .map(|(q, b)| GgufFile {
            path: format!("{name}-{q}.gguf"),
            size: (params * 1e9 * b / 8.0) as u64,
        })
        .collect();
    group(&files)
}

impl Backend for MockBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Mock
    }

    fn defaults(&self) -> Defaults {
        self.w().defaults.clone()
    }

    fn set_defaults(&self, defaults: Defaults) {
        self.w().defaults = defaults;
    }

    fn now(&self) -> SystemTime {
        self.now_()
    }

    fn probe(&self, cancel: &CancelToken) -> Result<ProbeReport> {
        self.step(cancel)?;
        let w = self.w();
        let colab = w
            .cli_installed
            .then(|| "/home/demo/.local/bin/colab".to_owned());
        let wsl = matches!(Host::detect(), Host::Wsl { .. });
        Ok(ProbeReport {
            wsl_installed: wsl,
            wsl_distros: if wsl { vec!["Ubuntu".into()] } else { vec![] },
            bash: Some("/usr/bin/bash".into()),
            ssh: Some("/usr/bin/ssh".into()),
            ssh_keygen: Some("/usr/bin/ssh-keygen".into()),
            git: Some("/usr/bin/git".into()),
            uv: Some("/home/demo/.local/bin/uv".into()),
            python3: Some("/usr/bin/python3".into()),
            colab_version: colab.as_ref().map(|_| "0.8.0".to_owned()),
            helper_python: colab
                .as_ref()
                .map(|_| "/home/demo/.local/share/uv/tools/google-colab-cli/bin/python".to_owned()),
            keyring_ok: colab.is_some() && !w.failures.keyring_unavailable,
            ssh_key_present: true,
            home: Some("/home/demo".into()),
            colab,
            errors: vec![],
        })
    }

    fn install_cli(
        &self,
        method: InstallMethod,
        log: Events<Line>,
        cancel: &CancelToken,
    ) -> Result<()> {
        let lines = [
            format!(
                "$ {}",
                match method {
                    InstallMethod::Uv => "uv tool install google-colab-cli",
                    InstallMethod::InstallUvThenCli =>
                        "curl -LsSf https://astral.sh/uv/install.sh | sh",
                    InstallMethod::PipUser => "python3 -m pip install --user google-colab-cli",
                }
            ),
            "Resolved 42 packages in 1.2s".into(),
            "Prepared 42 packages in 3.4s".into(),
            "Installed 42 packages in 0.8s".into(),
        ];
        for l in lines {
            self.step(cancel)?;
            let _ = log.send(Line {
                stream: Stream::Stdout,
                text: l,
            });
        }
        if self.w().failures.install_fails {
            let _ = log.send(Line {
                stream: Stream::Stderr,
                text: "error: failed to download google-colab-cli".into(),
            });
            return Err(Error::new(ErrorKind::InstallFailed).with("exit_code", 2));
        }
        let _ = log.send(Line {
            stream: Stream::Stdout,
            text: "Installed 1 executable: colab".into(),
        });
        let mut w = self.w();
        w.cli_installed = true;
        w.failures.cli_missing = false;
        Ok(())
    }

    fn accounts(&self, cancel: &CancelToken) -> Result<AccountsView> {
        self.ready()?;
        self.step(cancel)?;
        // Like the real backend: an interrupted add (no live flow) is rolled back.
        let recovered = {
            let mut w = self.w();
            !w.login_live && w.rollback_add()
        };
        let mut view = self.view();
        view.recovered_pending_add = recovered;
        Ok(view)
    }

    fn adopt_active_account(&self, _: &CancelToken) -> Result<Account> {
        self.ready()?;
        let now = self.now_();
        let mut w = self.w();
        w.adopt_unadopted(now);
        let id = w
            .active
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::AuthRequired))?;
        w.account_views()
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| Error::new(ErrorKind::AccountNotFound))
    }

    fn begin_add_account(&self, cancel: &CancelToken) -> Result<Box<dyn LoginFlow>> {
        self.ready()?;
        self.step(cancel)?;
        let now = self.now_();
        {
            let mut w = self.w();
            if w.pending_add.is_some() {
                return Err(Error::new(ErrorKind::AddInProgress));
            }
            if w.failures.login == Some(MockLoginFailure::CancelledBeforeUrl) {
                return Err(Error::new(ErrorKind::LoginCancelled));
            }
            // Like the real backend: save an unsaved credential first, then
            // back up and clear the slot while the flow is alive.
            w.adopt_unadopted(now);
            let previous = w.active.take();
            w.pending_add = Some(previous);
            w.login_live = true;
            w.tunnels.clear();
        }
        Ok(Box::new(MockLoginFlow {
            finished: false,
            m: self.clone(),
            url: "https://accounts.google.com/o/oauth2/auth?client_id=mock&redirect_uri=https%3A%2F%2Fsdk.cloud.google.com%2Fapplicationdefaultauthcode.html".into(),
        }))
    }

    fn switch_impact(&self, to: &AccountId) -> Result<SwitchImpact> {
        let w = self.w();
        if !w.accounts.iter().any(|a| &a.id == to) {
            return Err(Error::new(ErrorKind::AccountNotFound).with("account", to));
        }
        let from = w.active.clone();
        let switching = from.as_ref() != Some(to);
        let unmanaged_left_running = match (&from, switching) {
            (Some(f), true) if w.listing_fails_for(f) => None,
            (Some(f), true) => Some(w.unmanaged_on(f)),
            _ => Some(vec![]),
        };
        Ok(SwitchImpact {
            from: from.clone(),
            to: to.clone(),
            tunnels_closing: if switching {
                w.tunnels.values().cloned().collect()
            } else {
                vec![]
            },
            sessions_left_running: w
                .ledger
                .entries()
                .iter()
                .filter(|e| switching && e.stopped.is_none() && Some(&e.account) == from.as_ref())
                .cloned()
                .collect(),
            unmanaged_left_running,
        })
    }

    fn switch_account(&self, to: &AccountId, cancel: &CancelToken) -> Result<AccountsView> {
        self.ready()?;
        if !self.w().accounts.iter().any(|a| &a.id == to) {
            return Err(Error::new(ErrorKind::AccountNotFound).with("account", to));
        }
        self.step(cancel)?;
        let mut w = self.w();
        if w.active.as_ref() != Some(to) {
            w.tunnels.clear();
            w.active = Some(to.clone());
        }
        drop(w);
        Ok(self.view())
    }

    fn remove_account(&self, id: &AccountId, _: &CancelToken) -> Result<()> {
        self.ready()?;
        let mut w = self.w();
        if w.active.as_ref() == Some(id) {
            return Err(Error::invalid(
                "account",
                "switch to another account before removing this one",
            ));
        }
        if !w.live_names(id).is_empty() {
            return Err(Error::invalid("account", "stop its sessions first")
                .next(crate::error::NextStep::StopASession));
        }
        w.accounts.retain(|a| &a.id != id);
        w.usage.remove(id);
        Ok(())
    }

    fn refresh_usage(
        &self,
        accounts: &[AccountId],
        cancel: &CancelToken,
    ) -> Result<Vec<AccountUsageView>> {
        self.ready()?;
        self.step(cancel)?;
        let now = self.now_();
        let mut w = self.w();
        w.settle(now);
        let ids: Vec<AccountId> = if accounts.is_empty() {
            w.accounts.iter().map(|a| a.id.clone()).collect()
        } else {
            accounts.to_vec()
        };
        for id in ids {
            let (verified, _) = w.reconcile(&id, now);
            w.record_usage(&id, now, verified);
        }
        Ok(w.usage_views())
    }

    fn usage(&self) -> Vec<AccountUsageView> {
        self.w().usage_views()
    }

    fn usage_poll_due(&self) -> bool {
        let now = self.now_();
        let w = self.w();
        w.accounts.iter().any(|a| {
            let has = w.runtimes.iter().any(|r| r.account == a.id);
            poll_due(
                w.usage.get(&a.id).and_then(|u| u.last_attempt),
                now,
                has,
                &w.defaults,
            )
        })
    }

    fn runtimes(&self, cancel: &CancelToken) -> Result<RuntimesView> {
        self.ready()?;
        self.step(cancel)?;
        let now = self.now_();
        let mut w = self.w();
        if w.failures.runtimes_listing_fails {
            return Err(Error::new(ErrorKind::Http).detail("simulated runtime listing failure"));
        }
        w.settle(now);
        let mut view = RuntimesView::default();
        let ids: Vec<_> = w.accounts.iter().map(|a| a.id.clone()).collect();
        for id in ids {
            w.reconcile(&id, now);
        }
        for account in &w.accounts {
            if w.failures.unlisted_accounts.contains(&account.email) {
                view.errors.push(AccountError {
                    account: account.id.clone(),
                    error: Error::new(ErrorKind::Http).detail("simulated account listing failure"),
                });
            }
        }
        for r in &w.runtimes {
            if view.errors.iter().any(|failed| failed.account == r.account) {
                continue;
            }
            let tracked = r
                .name
                .as_ref()
                .and_then(|n| w.ledger.live(&r.account, n).cloned());
            let rate = r.name.as_ref().and_then(|n| {
                w.usage
                    .get(&r.account)
                    .and_then(|u| u.attribution.rate_of(n))
            });
            view.runtimes.push(Runtime {
                id: RuntimeRef {
                    account: r.account.clone(),
                    endpoint: r.endpoint.clone(),
                    name: r.name.clone(),
                },
                account_email: w.email(&r.account),
                on_active_account: w.active.as_ref() == Some(&r.account),
                accelerator: Some(r.accelerator),
                hardware: if r.accelerator == Accelerator::None {
                    "CPU".into()
                } else {
                    r.accelerator.as_str().into()
                },
                high_mem: r.high_mem,
                managed: tracked.is_some(),
                estimated_rate_cu_per_hour: rate.filter(|_| tracked.is_some()),
                estimated_spend_cu: tracked.as_ref().and_then(|t| t.spend.spend()),
                auto_stop_at: tracked.as_ref().and_then(|t| t.auto_stop_at()),
                tunnels: w
                    .tunnels
                    .values()
                    .filter(|t| {
                        t.spec.account == r.account && Some(&t.spec.session) == r.name.as_ref()
                    })
                    .map(|t| t.id)
                    .collect(),
                tracked,
            });
        }
        view.runtimes.sort_by_key(|r| !r.on_active_account);
        Ok(view)
    }

    fn new_session_name(&self, prefix: Option<&str>) -> String {
        let w = self.w();
        let mut taken: Vec<String> = w.runtimes.iter().filter_map(|r| r.name.clone()).collect();
        taken.extend(w.ledger.entries().iter().map(|e| e.name.clone()));
        let p = prefix
            .map(str::to_owned)
            .unwrap_or_else(|| accelerator_prefix(w.defaults.accelerator));
        generate_session_name(&p, &taken)
    }

    fn provision(
        &self,
        spec: ProvisionSpec,
        events: Events<ProvisionEvent>,
        cancel: &CancelToken,
    ) -> Result<ProvisionOutcome> {
        let account = self.active()?;
        let limit = {
            let mut w = self.w();
            let mut l = w.defaults.session_limit();
            l.reserved = w.reserved;
            w.reserved += 1;
            l
        };
        let ops = MockOps {
            m: self,
            account: account.clone(),
            restart: false,
        };
        let r = provision::provision(&ops, &spec, limit, self.cfg.clock.as_ref(), cancel, &|e| {
            let _ = events.send(e);
        });
        let mut w = self.w();
        w.reserved = w.reserved.saturating_sub(1);
        let outcome = r?;
        let endpoint = w
            .runtimes
            .iter()
            .find(|r| r.account == account && r.name.as_deref() == Some(&outcome.name))
            .map(|r| r.endpoint.clone());
        w.ledger.upsert(TrackedSession {
            name: outcome.name.clone(),
            account,
            endpoint,
            accelerator: spec.accelerator,
            high_mem: spec.high_mem,
            gpu: outcome.hardware.gpu.clone(),
            vram_mib: outcome.hardware.vram_mib,
            policy: spec.guard.clone(),
            spend: SpendMeter::new(outcome.started_at),
            watchdog: outcome.watchdog.clone(),
            stopped: None,
            stop_pending_confirmation: None,
        })?;
        Ok(outcome)
    }

    fn stop_session(
        &self,
        target: &RuntimeRef,
        options: StopOptions,
        cancel: &CancelToken,
    ) -> Result<()> {
        self.ready()?;
        self.step(cancel)?;
        self.stop_inner(target, options.reason)
    }

    fn extend_session(
        &self,
        target: &RuntimeRef,
        runtime: Duration,
        cancel: &CancelToken,
    ) -> Result<WatchdogInfo> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        self.step(cancel)?;
        let gone = || Error::new(ErrorKind::SessionGone).with("session", &name);
        // Like the real backend: the runtime's own state decides between
        // moving the deadline of a running watchdog and starting it again.
        let (alive, tracked, vm_idle) = {
            let w = self.w();
            let rt = w
                .runtimes
                .iter()
                .find(|r| r.account == target.account && r.name.as_deref() == Some(&name))
                .ok_or_else(gone)?;
            let e = w.ledger.live(&target.account, &name).ok_or_else(gone)?;
            (
                rt.watchdog_alive,
                e.policy.clone(),
                e.watchdog.as_ref().and_then(|wd| wd.idle_timeout),
            )
        };
        let info = if alive {
            WatchdogInfo {
                deadline: Some(self.now_() + runtime),
                idle_timeout: vm_idle,
                cancelled: false,
                armed: true,
            }
        } else {
            let policy = crate::watchdog::restart_policy(Some(&tracked), vm_idle, runtime);
            let ops = MockOps {
                m: self,
                account: target.account.clone(),
                restart: true,
            };
            match provision::start_watchdog(&ops, &name, &policy) {
                Ok(Some(info)) if info.armed => info,
                _ => crate::watchdog::start_failed(&policy),
            }
        };
        let now = self.now_();
        let found = self.w().ledger.update(&target.account, &name, |e| {
            let elapsed = now.duration_since(e.spend.started_at).unwrap_or_default();
            e.policy.max_runtime = Some(elapsed + runtime);
            e.watchdog = Some(info.clone());
        })?;
        if !found {
            return Err(gone());
        }
        Ok(info)
    }

    fn cancel_auto_stop(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<()> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        self.step(cancel)?;
        let mut w = self.w();
        // The cancelled watchdog exits without releasing the runtime.
        if let Some(rt) = w
            .runtimes
            .iter_mut()
            .find(|r| r.account == target.account && r.name.as_deref() == Some(&name))
        {
            rt.watchdog_alive = false;
        }
        w.ledger.update(&target.account, &name, |e| {
            e.policy.max_runtime = None;
            if let Some(wd) = e.watchdog.as_mut() {
                wd.cancelled = true;
                wd.armed = false;
            }
        })?;
        Ok(())
    }

    fn guard_poll(&self, cancel: &CancelToken) -> Result<GuardReport> {
        self.ready()?;
        self.step(cancel)?;
        let now = self.now_();
        let mut report = GuardReport::default();
        let live: Vec<TrackedSession> = {
            let mut w = self.w();
            w.settle(now);
            let live: Vec<TrackedSession> = w
                .ledger
                .entries()
                .iter()
                .filter(|e| e.stopped.is_none())
                .cloned()
                .collect();
            let ids: BTreeSet<AccountId> = live.iter().map(|e| e.account.clone()).collect();
            for id in &ids {
                let (verified, vanished) = w.reconcile(id, now);
                report.vanished.extend(vanished);
                w.record_usage(id, now, verified);
            }
            w.ledger
                .entries()
                .iter()
                .filter(|e| e.stopped.is_none() && ids.contains(&e.account))
                .cloned()
                .collect()
        };
        for e in live {
            if report.vanished.iter().any(|v| {
                v.runtime.account == e.account && v.runtime.name.as_deref() == Some(&e.name)
            }) {
                continue;
            }
            let (decision, target) = {
                let mut w = self.w();
                let fresh = !w.failures.usage_fetch_fails;
                let u = w.usage.get(&e.account).cloned().unwrap_or_default();
                let rate = if fresh {
                    u.attribution.rate_of(&e.name)
                } else {
                    None
                };
                let mut meter = e.spend.clone();
                meter.record(now, rate);
                let _ = w
                    .ledger
                    .update(&e.account, &e.name, |x| x.spend = meter.clone());
                let target = RuntimeRef {
                    account: e.account.clone(),
                    endpoint: e.endpoint.clone().unwrap_or_default(),
                    name: Some(e.name.clone()),
                };
                (
                    evaluate(&e.policy, now, u.snapshot.as_ref(), &meter),
                    target,
                )
            };
            let (mut stopped, mut error) = (false, None);
            if let Decision::Stop(concern) = &decision {
                match self.stop_inner(
                    &target,
                    StopReason::Guard {
                        concern: concern.clone(),
                    },
                ) {
                    Ok(()) => stopped = true,
                    Err(err) => error = Some(err),
                }
            }
            report.decisions.push(GuardDecision {
                session: target,
                decision,
                stopped,
                error,
            });
        }
        report.usage = self.w().usage_views();
        Ok(report)
    }

    fn open_tunnel(&self, spec: TunnelSpec, events: Events<TunnelEvent>) -> Result<TunnelId> {
        spec.validate()?;
        self.require_active(&spec.account)?;
        let mut w = self.w();
        if !w
            .runtimes
            .iter()
            .any(|r| r.account == spec.account && r.name.as_deref() == Some(&spec.session))
        {
            return Err(Error::new(ErrorKind::SessionGone).with("session", &spec.session));
        }
        if w.tunnels
            .values()
            .any(|t| t.spec.local_port == spec.local_port && !t.state.is_terminal())
        {
            return Err(Error::new(ErrorKind::PortInUse).with("port", spec.local_port));
        }
        w.next_tunnel += 1;
        let id = TunnelId(w.next_tunnel);
        w.tunnels.insert(
            id,
            TunnelInfo {
                id,
                spec,
                state: TunnelState::Starting,
                healthy: None,
            },
        );
        w.set_tunnel(id, TunnelState::Starting, Some(&events));
        if w.ssh_busy_left > 0 {
            w.ssh_busy_left = w
                .ssh_busy_left
                .saturating_sub(if w.ssh_busy_left == u32::MAX { 0 } else { 1 });
            w.set_tunnel(
                id,
                TunnelState::Failed {
                    error: Error::new(ErrorKind::SshBusy),
                },
                Some(&events),
            );
        } else {
            w.set_tunnel(id, TunnelState::Up, Some(&events));
            if let Some(t) = w.tunnels.get_mut(&id) {
                t.healthy = Some(true);
            }
            let _ = events.send(TunnelEvent::Health { id, healthy: true });
        }
        Ok(id)
    }

    fn close_tunnel(&self, id: TunnelId) -> Result<()> {
        self.w()
            .tunnels
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| Error::invalid("tunnel", "unknown tunnel"))
    }

    fn tunnels(&self) -> Vec<TunnelInfo> {
        self.w().tunnels.values().cloned().collect()
    }

    fn plugins(&self) -> Vec<PluginInfo> {
        self.w().plugins.list()
    }

    fn plugin_run_template(&self, plugin: &str) -> Result<PluginRunSpec> {
        let w = self.w();
        let info = w
            .plugins
            .list()
            .into_iter()
            .find(|p| p.manifest.id == plugin)
            .ok_or_else(|| Error::new(ErrorKind::PluginNotFound).with("plugin", plugin))?;
        let taken: Vec<String> = w.runtimes.iter().filter_map(|r| r.name.clone()).collect();
        Ok(run_template(&info, &w.defaults, &taken))
    }

    fn inspect_plugin_folder(&self, path: &Path) -> Result<PluginPreview> {
        let staging =
            std::env::temp_dir().join(format!("colab-playground-mock-{}", std::process::id()));
        self.w().plugins.inspect_folder(
            path,
            &staging,
            PluginOrigin::Folder {
                path: path.display().to_string(),
            },
        )
    }

    fn inspect_plugin_git(&self, url: &str, _: &CancelToken) -> Result<PluginPreview> {
        crate::plugins::validate_git_url(url)?;
        Err(Error::new(ErrorKind::GitFailed)
            .with("url", url)
            .detail("Git installs are not simulated; use a folder"))
    }

    fn install_plugin(&self, preview: &PluginPreview) -> Result<PluginInfo> {
        self.w().plugins.install(preview)
    }

    fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.w().plugins.set_enabled(id, enabled)
    }

    fn remove_plugin(&self, id: &str) -> Result<()> {
        self.w().plugins.remove(id)
    }

    fn run_plugin(
        &self,
        spec: PluginRunSpec,
        events: Events<PluginRunEvent>,
        cancel: &CancelToken,
    ) -> Result<PluginRunOutcome> {
        let account = self.active()?;
        let (plugin, enabled) = {
            let w = self.w();
            let p = w.plugins.get(&spec.plugin).cloned().ok_or_else(|| {
                Error::new(ErrorKind::PluginNotFound).with("plugin", &spec.plugin)
            })?;
            (p, w.plugins.enabled(&spec.plugin))
        };
        if !enabled {
            return Err(
                Error::invalid("plugin", "the plugin is disabled").with("plugin", &spec.plugin)
            );
        }
        let resolved = resolve_inputs(&plugin.manifest, &spec.inputs)?;
        let endpoints = resolve_endpoints(&plugin.manifest, &resolved.values)?;
        let (session, provisioned) = match &spec.target {
            RunTarget::Existing { name } => {
                if !self
                    .w()
                    .runtimes
                    .iter()
                    .any(|r| r.account == account && r.name.as_deref() == Some(name))
                {
                    return Err(Error::new(ErrorKind::SessionGone).with("session", name));
                }
                (name.clone(), false)
            }
            RunTarget::Provision(p) => {
                let (tx, rx) = std::sync::mpsc::channel();
                let r = self.provision(p.clone(), tx, cancel);
                for e in rx.try_iter() {
                    let _ = events.send(PluginRunEvent::Provision(e));
                }
                (r?.name, true)
            }
        };
        let endpoint_of = |m: &MockBackend| {
            m.w()
                .runtimes
                .iter()
                .find(|r| r.account == account && r.name.as_deref() == Some(&session))
                .map(|r| r.endpoint.clone())
        };
        let stop_it = |m: &MockBackend| {
            if let Some(ep) = endpoint_of(m) {
                let _ = m.stop_inner(
                    &RuntimeRef {
                        account: account.clone(),
                        endpoint: ep,
                        name: Some(session.clone()),
                    },
                    StopReason::Provision,
                );
            }
        };
        let run_id = format!("{}-{}", plugin.manifest.id, &crate::accounts::nonce()[..8]);
        let _ = events.send(PluginRunEvent::Launched {
            session: session.clone(),
            run_id: run_id.clone(),
        });
        let vram = self
            .w()
            .runtimes
            .iter()
            .find(|r| r.account == account && r.name.as_deref() == Some(&session))
            .and_then(|r| r.vram_mib);
        let fail_stage = self.w().failures.plugin_fail_stage.clone();
        let stages: Vec<String> = plugin
            .manifest
            .stages
            .iter()
            .map(|s| s.id.clone())
            .collect();
        for (i, stage) in stages.iter().enumerate() {
            if self.step(cancel).is_err() {
                if provisioned {
                    stop_it(self);
                }
                return Err(Error::cancelled());
            }
            let status = RunStatus {
                state: RunState::Running,
                stage: Some(stage.clone()),
                progress: Some((i as f64 + 0.5) / stages.len() as f64),
                message: Some(format!("simulated {stage}")),
                outputs: Default::default(),
            };
            let _ = events.send(PluginRunEvent::Status(status));
            let _ = events.send(PluginRunEvent::Log {
                lines: vec![format!("[mock] {stage}: working")],
            });
            if fail_stage.as_deref() == Some(stage.as_str()) {
                let failed = RunStatus {
                    state: RunState::Failed,
                    stage: Some(stage.clone()),
                    progress: None,
                    message: Some(format!("simulated failure in {stage}")),
                    outputs: Default::default(),
                };
                let _ = events.send(PluginRunEvent::Status(failed));
                if provisioned && spec.stop_on_failure {
                    stop_it(self);
                }
                return Err(Error::new(ErrorKind::PluginRunFailed)
                    .with("stage", stage)
                    .with("message", format!("simulated failure in {stage}")));
            }
        }
        let mut outputs = serde_json::Map::new();
        if let Some(crate::plugins::InputValue::Text(repo)) = resolved.values.get("repo") {
            let listing = mock_listing(repo);
            let input = FitInput {
                vram_mib: vram.unwrap_or(16_384),
                ctx: 0,
                model_ctx: Some(131_072),
                kv_bytes_per_token: Some(65_536),
                mmproj_bytes: 0,
                kv: None,
            };
            if let Ok(choice) = fit_auto(&listing, &input) {
                outputs.insert("quant".into(), choice.quant.clone().into());
                outputs.insert("n_ctx".into(), choice.ctx.into());
                outputs.insert("kv".into(), choice.kv.as_str().into());
            }
            outputs.insert(
                "model_id".into(),
                repo.rsplit('/')
                    .next()
                    .unwrap_or(repo)
                    .trim_end_matches("-GGUF")
                    .to_ascii_lowercase()
                    .into(),
            );
        }
        if let Some(v) = vram {
            outputs.insert("vram_total_mib".into(), v.into());
        }
        outputs.insert("tok_per_s".into(), 64.2.into());
        let status = RunStatus {
            state: RunState::Ready,
            stage: stages.last().cloned(),
            progress: Some(1.0),
            message: Some("ready (simulated)".into()),
            outputs,
        };
        let _ = events.send(PluginRunEvent::Status(status.clone()));
        Ok(PluginRunOutcome {
            session,
            account,
            run_id,
            status,
            endpoints,
        })
    }

    fn set_secret(&self, name: &str, value: &str, _: &CancelToken) -> Result<()> {
        crate::accounts::validate_secret_name(name)?;
        if value.trim().is_empty() {
            return Err(Error::invalid("secret", "empty secret"));
        }
        self.w().secrets.insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    fn has_secret(&self, name: &str, _: &CancelToken) -> Result<bool> {
        crate::accounts::validate_secret_name(name)?;
        Ok(self.w().secrets.contains_key(name))
    }

    fn delete_secret(&self, name: &str, _: &CancelToken) -> Result<()> {
        crate::accounts::validate_secret_name(name)?;
        self.w().secrets.remove(name);
        Ok(())
    }

    fn hf_search(&self, query: &str, limit: u32, cancel: &CancelToken) -> Result<Vec<HfModel>> {
        self.step(cancel)?;
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return Ok(vec![]);
        }
        Ok(catalog()
            .into_iter()
            .filter(|(r, _)| r.to_ascii_lowercase().contains(&q))
            .take(limit as usize)
            .enumerate()
            .map(|(i, (r, _))| HfModel {
                id: r.into(),
                downloads: 100_000 / (i as u64 + 1),
                likes: 500 / (i as u64 + 1),
                gated: false,
            })
            .collect())
    }

    fn hf_files(&self, repo: &str, cancel: &CancelToken) -> Result<GgufListing> {
        if !crate::plugins::is_repo_id(repo) {
            return Err(Error::invalid("repo", "expected owner/name").with("value", repo));
        }
        self.step(cancel)?;
        Ok(mock_listing(repo))
    }

    fn check_for_update(&self, cancel: &CancelToken) -> Result<crate::update::UpdateStatus> {
        cancel.check()?;
        // Never touches the network: the answer comes from the failure switches.
        let mut w = self.w();
        w.update_checks += 1;
        if w.failures.update_check_fails {
            return Err(Error::new(ErrorKind::Http).detail("simulated update check failure"));
        }
        if !w.failures.update_available {
            return Ok(crate::update::UpdateStatus::UpToDate);
        }
        let current = crate::update::Version::parse(crate::update::CURRENT_VERSION).unwrap_or(
            crate::update::Version {
                major: 0,
                minor: 0,
                patch: 0,
                pre: Vec::new(),
            },
        );
        let next = crate::update::Version {
            major: current.major,
            minor: current.minor,
            patch: current.patch + 1,
            pre: Vec::new(),
        }
        .to_string();
        let page = crate::update::this_app_release_page(&next).unwrap_or_default();
        Ok(crate::update::UpdateStatus::Available(
            crate::update::Release {
                tag: format!("v{next}"),
                version: next,
                page,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn mock(scenario: MockScenario) -> (MockBackend, ManualClock) {
        let (cfg, clock) = MockConfig::headless(scenario);
        (MockBackend::new(cfg), clock)
    }

    fn nc() -> CancelToken {
        CancelToken::new()
    }

    #[test]
    fn reconciliation_precedes_usage_and_respects_listing_failures() {
        for guard in [false, true] {
            for per_account in [false, true] {
                let (m, _) = mock(MockScenario::demo());
                let (account, email) = {
                    let mut w = m.w();
                    let account = w.active.clone().unwrap();
                    let email = w.email(&account).unwrap();
                    let endpoint = w
                        .ledger
                        .entries()
                        .iter()
                        .find(|e| e.account == account)
                        .unwrap()
                        .endpoint
                        .clone()
                        .unwrap();
                    w.runtimes.retain(|r| r.endpoint != endpoint);
                    (account, email)
                };
                m.set_failures(MockFailures {
                    runtimes_listing_fails: !per_account,
                    unlisted_accounts: if per_account { vec![email] } else { vec![] },
                    ..Default::default()
                });
                let refresh = || {
                    if guard {
                        m.guard_poll(&nc()).unwrap().usage
                    } else {
                        m.refresh_usage(&[], &nc()).unwrap()
                    }
                };
                let views = refresh();
                let view = views.iter().find(|v| v.account == account).unwrap();
                assert!(!view.snapshot.unwrap().stale);
                assert!(!view.ownership_verified);
                assert_eq!(m.w().live_names(&account).len(), 2);
                m.set_failures(MockFailures::default());
                let views = refresh();
                let view = views.iter().find(|v| v.account == account).unwrap();
                assert!(view.ownership_verified);
                assert_eq!(view.unmanaged_runtimes, 1);
                assert_eq!(m.w().live_names(&account).len(), 1);
            }
        }
    }

    #[test]
    fn reconciliation_backfills_but_does_not_invent_missing_identities() {
        let (m, _) = mock(MockScenario::demo());
        let mut w = m.w();
        let account = w.active.clone().unwrap();
        let entry = w
            .ledger
            .entries()
            .iter()
            .find(|e| e.account == account)
            .unwrap()
            .clone();
        w.ledger
            .update(&account, &entry.name, |e| e.endpoint = None)
            .unwrap();
        assert!(w.reconcile(&account, m.now_()).0);
        assert_eq!(
            w.ledger.live(&account, &entry.name).unwrap().endpoint,
            entry.endpoint
        );
        w.ledger
            .update(&account, &entry.name, |e| e.endpoint = None)
            .unwrap();
        w.runtimes
            .retain(|r| r.endpoint != entry.endpoint.as_deref().unwrap());
        assert!(!w.reconcile(&account, m.now_()).0);
        assert!(w.ledger.live(&account, &entry.name).is_some());
    }

    #[test]
    fn a_session_without_an_endpoint_ends_once_no_runtime_could_be_it() {
        let (m, _) = mock(MockScenario::demo());
        let (active, other) = {
            let w = m.w();
            (w.accounts[0].id.clone(), w.accounts[1].id.clone())
        };
        let lose = |account: &AccountId, name: &str| {
            let mut w = m.w();
            let endpoint = w
                .ledger
                .live(account, name)
                .unwrap()
                .endpoint
                .clone()
                .unwrap();
            w.ledger
                .update(account, name, |e| e.endpoint = None)
                .unwrap();
            w.runtimes.retain(|r| r.endpoint != endpoint);
        };
        let verified = |views: &[AccountUsageView], account: &AccountId| {
            views
                .iter()
                .find(|v| &v.account == account)
                .unwrap()
                .ownership_verified
        };

        lose(&other, "cpu-0b5e");
        assert!(m.remove_account(&other, &nc()).is_err());
        let report = m.guard_poll(&nc()).unwrap();
        let cpu = RuntimeRef {
            account: other.clone(),
            endpoint: String::new(),
            name: Some("cpu-0b5e".into()),
        };
        assert_eq!(
            report.vanished,
            [Vanished {
                runtime: cpu,
                reason: StopReason::Vanished
            }]
        );
        assert!(verified(&report.usage, &other));
        m.remove_account(&other, &nc()).unwrap();

        lose(&active, "t4-91c2");
        let views = m.refresh_usage(&[], &nc()).unwrap();
        assert!(m.w().ledger.live(&active, "t4-91c2").is_some());
        assert!(!verified(&views, &active), "the browser's L4 could be it");
        let browser = {
            let w = m.w();
            let r = w
                .runtimes
                .iter()
                .find(|r| r.account == active && r.name.is_none())
                .unwrap();
            RuntimeRef {
                account: active.clone(),
                endpoint: r.endpoint.clone(),
                name: None,
            }
        };
        m.stop_session(&browser, StopOptions::default(), &nc())
            .unwrap();
        let views = m.refresh_usage(&[], &nc()).unwrap();
        assert!(m.w().ledger.live(&active, "t4-91c2").is_none());
        assert!(verified(&views, &active));
    }

    #[test]
    fn a_guard_stop_without_an_endpoint_suspects_an_orphan_and_keeps_the_session() {
        let (m, clock) = mock(MockScenario::demo());
        let (account, endpoint, limit) = {
            let mut w = m.w();
            let account = w.active.clone().unwrap();
            let entry = w.ledger.live(&account, "t4-91c2").unwrap().clone();
            let endpoint = entry.endpoint.unwrap();
            w.ledger
                .update(&account, "t4-91c2", |e| e.endpoint = None)
                .unwrap();
            let runtime = w
                .runtimes
                .iter_mut()
                .find(|r| r.endpoint == endpoint)
                .unwrap();
            runtime.name = None;
            (account, endpoint, entry.policy.max_runtime.unwrap())
        };
        clock.advance(limit);
        let report = m.guard_poll(&nc()).unwrap();
        let decision = report
            .decisions
            .iter()
            .find(|d| d.session.name.as_deref() == Some("t4-91c2"))
            .unwrap();
        assert!(matches!(decision.decision, Decision::Stop(_)));
        assert!(!decision.stopped);
        let error = decision.error.as_ref().unwrap();
        assert!(error.is(ErrorKind::OrphanSuspected), "{error:?}");
        assert!(m.w().ledger.live(&account, "t4-91c2").is_some());
        let listed = m.runtimes(&nc()).unwrap().runtimes;
        assert!(listed.iter().any(|r| r.id.endpoint == endpoint));
    }

    #[test]
    fn ownership_without_live_ledger_entries_does_not_need_a_listing() {
        for stopped in [false, true] {
            let mut scenario = if stopped {
                MockScenario::demo()
            } else {
                MockScenario::single()
            };
            if !stopped {
                scenario
                    .runtimes
                    .push(MockScenario::demo().runtimes[2].clone());
            }
            let (m, _) = mock(scenario);
            let account = {
                let mut w = m.w();
                let account = w.active.clone().unwrap();
                let names = w.live_names(&account);
                for name in names {
                    w.ledger
                        .update(&account, &name, |e| {
                            e.stopped = Some(StopRecord {
                                at: m.now_(),
                                reason: StopReason::User,
                            })
                        })
                        .unwrap();
                }
                account
            };
            m.set_failures(MockFailures {
                runtimes_listing_fails: true,
                ..Default::default()
            });
            let views = m.refresh_usage(&[], &nc()).unwrap();
            let view = views.iter().find(|v| v.account == account).unwrap();
            assert!(view.ownership_verified);
            assert_eq!(view.unmanaged_runtimes, view.snapshot.unwrap().assignments);
            assert!(view.unmanaged_runtimes > 0);
        }
    }

    #[test]
    fn demo_lists_two_accounts_managed_unmanaged_and_other_account_runtimes() {
        let (m, _) = mock(MockScenario::demo());
        let v = m.runtimes(&nc()).unwrap();
        assert_eq!(v.runtimes.len(), 4);
        assert_eq!(
            v.runtimes
                .iter()
                .filter(|r| r.on_active_account && r.managed)
                .count(),
            2
        );
        let unmanaged = v.runtimes.iter().find(|r| !r.managed).unwrap();
        assert!(unmanaged.id.name.is_none());
        let other = v.runtimes.iter().find(|r| !r.on_active_account).unwrap();
        // Anything listed can be stopped, on any account.
        m.stop_session(&unmanaged.id, StopOptions::default(), &nc())
            .unwrap();
        m.stop_session(&other.id, StopOptions::default(), &nc())
            .unwrap();
        assert_eq!(m.runtimes(&nc()).unwrap().runtimes.len(), 2);
        // Exec-style operations on another account need a switch.
        let err = m
            .extend_session(&other.id, Duration::from_secs(60), &nc())
            .unwrap_err();
        assert!(err.is(ErrorKind::AccountNotActive));
        assert_eq!(err.next_step, crate::error::NextStep::SwitchAccount);
    }

    #[test]
    fn balance_burns_and_attribution_splits_two_sessions() {
        let (m, clock) = mock(MockScenario::demo());
        let first = m.refresh_usage(&[], &nc()).unwrap();
        let a = &first[0];
        let s = a.snapshot.unwrap();
        assert!((s.rate_cu_per_hour - (11.77 + 1.76 + 4.82)).abs() < 1e-9);
        assert_eq!(a.unmanaged_runtimes, 1, "the browser runtime is flagged");
        assert_eq!(a.session_rates.len(), 2);
        clock.advance(Duration::from_secs(3600));
        let later = m.refresh_usage(&[], &nc()).unwrap();
        assert!(later[0].snapshot.unwrap().balance_cu < s.balance_cu - 18.0);
        assert!(later[0].history.len() >= 2);
        assert!(later[0].eta.is_some());
        let f = MockFailures {
            usage_fetch_fails: true,
            ..Default::default()
        };
        m.set_failures(f);
        let stale = m.refresh_usage(&[], &nc()).unwrap();
        assert!(stale[0].snapshot.unwrap().stale);
    }

    #[test]
    fn provision_retries_to_80_gb_and_respects_the_session_cap() {
        let (m, _) = mock(MockScenario::single());
        let mut spec = m.defaults().provision_spec(m.new_session_name(None));
        spec.min_vram_mib = Some(70_000);
        let (tx, rx) = mpsc::channel();
        let out = m.provision(spec, tx, &nc()).unwrap();
        assert_eq!(out.attempts, 2);
        assert_eq!(out.hardware.vram_mib, Some(81_920));
        assert!(
            rx.try_iter()
                .any(|e| matches!(e, ProvisionEvent::Rejected { .. }))
        );
        let spec2 = m.defaults().provision_spec(m.new_session_name(Some("t4")));
        let (tx, _rx) = mpsc::channel();
        m.provision(
            ProvisionSpec {
                accelerator: Accelerator::T4,
                ..spec2
            },
            tx,
            &nc(),
        )
        .unwrap();
        let spec3 = m.defaults().provision_spec(m.new_session_name(None));
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.provision(spec3, tx, &nc())
                .unwrap_err()
                .is(ErrorKind::SessionLimitReached)
        );
    }

    #[test]
    fn guard_warns_then_stops_as_usage_burns() {
        let (m, clock) = mock(MockScenario::single());
        let mut d = m.defaults();
        d.max_spend_cu = Some(10.0);
        d.max_runtime = None;
        m.set_defaults(d);
        let spec = ProvisionSpec {
            accelerator: Accelerator::H100,
            ..m.defaults().provision_spec("h100-test")
        };
        let (tx, _rx) = mpsc::channel();
        m.provision(spec, tx, &nc()).unwrap();
        m.guard_poll(&nc()).unwrap();
        clock.advance(Duration::from_secs(27 * 60)); // ≈ 8.2 CU at 18.3 CU/h
        let r = m.guard_poll(&nc()).unwrap();
        assert!(
            matches!(r.decisions[0].decision, Decision::Warn(_)),
            "{:?}",
            r.decisions
        );
        clock.advance(Duration::from_secs(10 * 60));
        let r = m.guard_poll(&nc()).unwrap();
        assert!(r.decisions[0].stopped, "{:?}", r.decisions);
        assert!(m.runtimes(&nc()).unwrap().runtimes.is_empty());
    }

    #[test]
    fn login_flows_and_failures() {
        let (m, _) = mock(MockScenario::single());
        let flow = m.begin_add_account(&nc()).unwrap();
        assert!(flow.auth_url().starts_with("https://accounts.google.com/"));
        let acct = flow.submit_code("4/0Abc", &nc()).unwrap();
        assert_eq!(m.accounts(&nc()).unwrap().active, Some(acct.id));
        let mut f = MockFailures {
            login: Some(MockLoginFailure::InvalidCode),
            ..Default::default()
        };
        m.set_failures(f.clone());
        assert!(
            m.begin_add_account(&nc())
                .unwrap()
                .submit_code("bad", &nc())
                .unwrap_err()
                .is(ErrorKind::LoginInvalidCode)
        );
        f.login = Some(MockLoginFailure::CancelledBeforeUrl);
        m.set_failures(f);
        assert!(
            m.begin_add_account(&nc())
                .err()
                .unwrap()
                .is(ErrorKind::LoginCancelled)
        );
    }

    #[test]
    fn cli_missing_install_and_failures() {
        let (cfg, _) = MockConfig::headless(MockScenario::signed_out());
        let mut cfg = cfg;
        cfg.failures.cli_missing = true;
        cfg.failures.install_fails = true;
        let m = MockBackend::new(cfg);
        assert!(m.accounts(&nc()).unwrap_err().is(ErrorKind::NotInstalled));
        assert!(m.probe(&nc()).unwrap().colab.is_none());
        let (tx, rx) = mpsc::channel();
        assert!(
            m.install_cli(InstallMethod::Uv, tx, &nc())
                .unwrap_err()
                .is(ErrorKind::InstallFailed)
        );
        assert!(rx.try_iter().count() >= 4);
        m.set_failures(MockFailures::default());
        let (tx, _rx) = mpsc::channel();
        m.install_cli(InstallMethod::Uv, tx, &nc()).unwrap();
        let v = m.accounts(&nc()).unwrap();
        assert!(v.accounts.is_empty() && v.active.is_none());
    }

    #[test]
    fn quick_deploy_reaches_a_ready_endpoint_and_tunnel() {
        let (m, _) = mock(MockScenario::single());
        let mut spec = m.plugin_run_template("llama-cpp").unwrap();
        spec.inputs.insert(
            "repo".into(),
            crate::plugins::InputValue::Text("bartowski/ScarletKc_ScarletKc-2.0-GGUF".into()),
        );
        if let RunTarget::Provision(p) = &mut spec.target {
            p.min_vram_mib = Some(70_000);
        }
        let (tx, rx) = mpsc::channel();
        let out = m.run_plugin(spec, tx, &nc()).unwrap();
        assert_eq!(out.status.state, RunState::Ready);
        assert_eq!(
            out.status.outputs["quant"], "Q8_0",
            "80 GB A100 after a retry gets Q8_0"
        );
        assert!(rx.try_iter().any(|e| matches!(
            e,
            PluginRunEvent::Provision(ProvisionEvent::Retrying { .. })
        )));
        let ep = &out.endpoints[0];
        let (tx, trx) = mpsc::channel();
        let id = m
            .open_tunnel(
                TunnelSpec {
                    account: out.account.clone(),
                    session: out.session.clone(),
                    local_port: ep.remote_port,
                    remote_port: ep.remote_port,
                    health_path: ep.health.clone(),
                },
                tx,
            )
            .unwrap();
        assert!(trx.try_iter().any(|e| matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Up,
                ..
            }
        )));
        assert_eq!(m.tunnels()[0].id, id);
    }

    #[test]
    fn plugin_failure_at_a_stage_stops_the_fresh_session_and_429_is_reported() {
        let (cfg, _) = MockConfig::headless(MockScenario::single());
        let mut cfg = cfg;
        cfg.failures.plugin_fail_stage = Some("download".into());
        cfg.failures.ssh_busy_attempts = 1;
        let m = MockBackend::new(cfg);
        let mut spec = m.plugin_run_template("llama-cpp").unwrap();
        spec.inputs.insert(
            "repo".into(),
            crate::plugins::InputValue::Text("unsloth/Qwen3-0.6B-GGUF".into()),
        );
        let (tx, _rx) = mpsc::channel();
        let err = m.run_plugin(spec, tx, &nc()).unwrap_err();
        assert!(err.is(ErrorKind::PluginRunFailed));
        assert_eq!(err.param("stage"), Some("download"));
        assert!(
            m.runtimes(&nc()).unwrap().runtimes.is_empty(),
            "no billed VM left behind"
        );
        // 429 on the first tunnel attempt.
        let spec = m.defaults().provision_spec("t");
        let (tx, _rx) = mpsc::channel();
        m.provision(
            ProvisionSpec {
                accelerator: Accelerator::T4,
                ..spec
            },
            tx,
            &nc(),
        )
        .unwrap();
        let acct = m.accounts(&nc()).unwrap().active.unwrap();
        let (tx, trx) = mpsc::channel();
        m.open_tunnel(
            TunnelSpec {
                account: acct,
                session: "t".into(),
                local_port: 18001,
                remote_port: 8000,
                health_path: None,
            },
            tx,
        )
        .unwrap();
        assert!(trx.try_iter().any(|e| matches!(e, TunnelEvent::State { state: TunnelState::Failed { ref error }, .. } if error.is(ErrorKind::SshBusy))));
    }

    #[test]
    fn switch_impact_lists_tunnels_and_sessions() {
        let (m, _) = mock(MockScenario::demo());
        let accts = m.accounts(&nc()).unwrap();
        let other = accts.accounts[1].id.clone();
        let (tx, _rx) = mpsc::channel();
        m.open_tunnel(
            TunnelSpec {
                account: accts.accounts[0].id.clone(),
                session: "a100-7f3a".into(),
                local_port: 18000,
                remote_port: 8000,
                health_path: None,
            },
            tx,
        )
        .unwrap();
        let impact = m.switch_impact(&other).unwrap();
        assert_eq!(impact.tunnels_closing.len(), 1);
        assert_eq!(impact.sessions_left_running.len(), 2);
        m.switch_account(&other, &nc()).unwrap();
        assert!(m.tunnels().is_empty());
    }

    #[test]
    fn pending_add_clears_the_slot_and_every_non_success_restores_it() {
        let (m, _) = mock(MockScenario::demo());
        let original = m.accounts(&nc()).unwrap().active.unwrap();
        // While a flow is alive the slot is empty; cancel restores it.
        let flow = m.begin_add_account(&nc()).unwrap();
        let mid = m.accounts(&nc()).unwrap();
        assert_eq!(mid.active, None);
        assert!(!mid.has_token && !mid.recovered_pending_add);
        assert!(
            m.begin_add_account(&nc())
                .err()
                .unwrap()
                .is(ErrorKind::AddInProgress)
        );
        flow.cancel().unwrap();
        assert_eq!(m.accounts(&nc()).unwrap().active.as_ref(), Some(&original));
        // Dropping the flow restores it too.
        drop(m.begin_add_account(&nc()).unwrap());
        assert_eq!(m.accounts(&nc()).unwrap().active.as_ref(), Some(&original));
        // A rejected code restores it.
        m.set_failures(MockFailures {
            login: Some(MockLoginFailure::InvalidCode),
            ..Default::default()
        });
        let err = m
            .begin_add_account(&nc())
            .unwrap()
            .submit_code("x", &nc())
            .unwrap_err();
        assert!(err.is(ErrorKind::LoginInvalidCode));
        assert_eq!(m.accounts(&nc()).unwrap().active.as_ref(), Some(&original));
        // Success replaces it.
        m.set_failures(MockFailures::default());
        let added = m
            .begin_add_account(&nc())
            .unwrap()
            .submit_code("4/ok", &nc())
            .unwrap();
        let v = m.accounts(&nc()).unwrap();
        assert_eq!(v.active, Some(added.id));
        assert_eq!(v.accounts.len(), 3);
    }

    #[test]
    fn unadopted_slot_and_interrupted_add_scenarios() {
        let (cfg, _) = MockConfig::headless(MockScenario::single());
        let mut cfg = cfg;
        cfg.failures.unadopted_slot = true;
        let m = MockBackend::new(cfg);
        let v = m.accounts(&nc()).unwrap();
        assert!(v.unadopted && v.has_token && v.active.is_none());
        assert_eq!(v.active_email.as_deref(), Some(MOCK_UNADOPTED_EMAIL));
        let adopted = m.adopt_active_account(&nc()).unwrap();
        assert_eq!(adopted.email.as_deref(), Some(MOCK_UNADOPTED_EMAIL));
        let v = m.accounts(&nc()).unwrap();
        assert!(!v.unadopted && v.active == Some(adopted.id.clone()));

        // An interrupted add is rolled back on the next accounts() and reported once.
        m.set_failures(MockFailures {
            interrupted_add: true,
            ..Default::default()
        });
        let v = m.accounts(&nc()).unwrap();
        assert!(v.recovered_pending_add);
        assert_eq!(v.active, Some(adopted.id.clone()));
        assert!(!m.accounts(&nc()).unwrap().recovered_pending_add);

        let (cfg, _) = MockConfig::headless(MockScenario::single());
        let mut cfg = cfg;
        cfg.failures.interrupted_add = true;
        let m = MockBackend::new(cfg);
        let v = m.accounts(&nc()).unwrap();
        assert!(v.recovered_pending_add && v.active.is_some());
    }

    #[test]
    fn every_failure_switch_flips_at_runtime() {
        let (m, _) = mock(MockScenario::single());
        let on = |f: MockFailures| m.set_failures(f);
        let off = || m.set_failures(MockFailures::default());
        let d = MockFailures::default;

        on(MockFailures {
            cli_missing: true,
            ..d()
        });
        assert!(m.accounts(&nc()).unwrap_err().is(ErrorKind::NotInstalled));
        assert!(m.probe(&nc()).unwrap().colab.is_none());
        off();
        assert!(m.accounts(&nc()).is_ok());
        assert!(m.probe(&nc()).unwrap().colab.is_some());

        on(MockFailures {
            install_fails: true,
            ..d()
        });
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.install_cli(InstallMethod::Uv, tx, &nc())
                .unwrap_err()
                .is(ErrorKind::InstallFailed)
        );
        off();

        on(MockFailures {
            keyring_unavailable: true,
            ..d()
        });
        assert!(
            m.accounts(&nc())
                .unwrap_err()
                .is(ErrorKind::KeyringUnavailable)
        );
        assert!(!m.probe(&nc()).unwrap().keyring_ok);
        off();

        on(MockFailures {
            usage_fetch_fails: true,
            ..d()
        });
        m.refresh_usage(&[], &nc()).unwrap();
        off();
        assert!(
            !m.refresh_usage(&[], &nc()).unwrap()[0]
                .snapshot
                .unwrap()
                .stale
        );
        on(MockFailures {
            usage_fetch_fails: true,
            ..d()
        });
        assert!(
            m.refresh_usage(&[], &nc()).unwrap()[0]
                .snapshot
                .unwrap()
                .stale
        );
        off();

        on(MockFailures {
            login: Some(MockLoginFailure::VerificationFailed),
            ..d()
        });
        let err = m
            .begin_add_account(&nc())
            .unwrap()
            .submit_code("c", &nc())
            .unwrap_err();
        assert!(err.is(ErrorKind::LoginVerificationFailed));
        off();

        on(MockFailures {
            not_entitled: vec![Accelerator::T4],
            ..d()
        });
        let spec = ProvisionSpec {
            accelerator: Accelerator::T4,
            ..m.defaults().provision_spec("t4-x")
        };
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.provision(spec.clone(), tx, &nc())
                .unwrap_err()
                .is(ErrorKind::NotEntitled)
        );
        off();

        on(MockFailures {
            allocation_hangs: vec![Accelerator::T4],
            ..d()
        });
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.provision(spec.clone(), tx, &nc())
                .unwrap_err()
                .is(ErrorKind::AllocationTimeout)
        );
        assert!(m.runtimes(&nc()).unwrap().runtimes.is_empty());
        off();
        let (tx, _rx) = mpsc::channel();
        m.provision(spec, tx, &nc()).unwrap();

        let acct = m.accounts(&nc()).unwrap().active.unwrap();
        let tspec = |port| TunnelSpec {
            account: acct.clone(),
            session: "t4-x".into(),
            local_port: port,
            remote_port: 8000,
            health_path: None,
        };
        on(MockFailures {
            ssh_busy_attempts: u32::MAX,
            ..d()
        });
        let (tx, trx) = mpsc::channel();
        m.open_tunnel(tspec(18100), tx).unwrap();
        assert!(trx.try_iter().any(|e| matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Failed { .. },
                ..
            }
        )));
        off();
        let (tx, trx) = mpsc::channel();
        m.open_tunnel(tspec(18101), tx).unwrap();
        assert!(trx.try_iter().any(|e| matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Up,
                ..
            }
        )));

        on(MockFailures {
            plugin_fail_stage: Some("plan".into()),
            ..d()
        });
        let mut run = m.plugin_run_template("llama-cpp").unwrap();
        run.inputs.insert(
            "repo".into(),
            crate::plugins::InputValue::Text("unsloth/Qwen3-0.6B-GGUF".into()),
        );
        run.target = RunTarget::Existing {
            name: "t4-x".into(),
        };
        let (tx, _rx) = mpsc::channel();
        assert_eq!(
            m.run_plugin(run.clone(), tx, &nc())
                .unwrap_err()
                .param("stage"),
            Some("plan")
        );
        off();
        let (tx, _rx) = mpsc::channel();
        assert_eq!(
            m.run_plugin(run, tx, &nc()).unwrap().status.state,
            RunState::Ready
        );

        // The A100 sequence restarts when it changes.
        on(MockFailures {
            a100_vram_sequence: vec![81_920],
            ..d()
        });
        m.stop_session(
            &m.runtimes(&nc()).unwrap().runtimes[0].id,
            StopOptions::default(),
            &nc(),
        )
        .unwrap();
        let (tx, _rx) = mpsc::channel();
        let out = m
            .provision(m.defaults().provision_spec("big"), tx, &nc())
            .unwrap();
        assert_eq!(out.hardware.vram_mib, Some(81_920));

        // A cancelled watchdog cannot be started again while the switch is on.
        let big = m
            .runtimes(&nc())
            .unwrap()
            .runtimes
            .into_iter()
            .find(|r| r.id.name.as_deref() == Some("big"))
            .unwrap()
            .id;
        let hour = Duration::from_secs(3600);
        m.cancel_auto_stop(&big, &nc()).unwrap();
        on(MockFailures {
            watchdog_restart_fails: true,
            ..d()
        });
        assert!(!m.extend_session(&big, hour, &nc()).unwrap().armed);
        off();
        assert!(m.extend_session(&big, hour, &nc()).unwrap().armed);
    }

    fn tracked_watchdog(m: &MockBackend, name: &str) -> (Runtime, WatchdogInfo) {
        let rt = m
            .runtimes(&nc())
            .unwrap()
            .runtimes
            .into_iter()
            .find(|r| r.id.name.as_deref() == Some(name))
            .unwrap();
        let wd = rt.tracked.as_ref().unwrap().watchdog.clone().unwrap();
        (rt, wd)
    }

    #[test]
    fn setting_an_auto_stop_after_a_cancel_starts_the_watchdog_again() {
        let (m, _) = mock(MockScenario::single());
        let mut d = m.defaults();
        d.idle_timeout = Some(Duration::from_secs(900));
        m.set_defaults(d);
        let (tx, _rx) = mpsc::channel();
        let spec = ProvisionSpec {
            accelerator: Accelerator::T4,
            ..m.defaults().provision_spec("t4-wd")
        };
        m.provision(spec, tx, &nc()).unwrap();
        let (rt, wd) = tracked_watchdog(&m, "t4-wd");
        assert!(wd.armed && !wd.cancelled && wd.deadline.is_some());
        assert!(rt.auto_stop_at.is_some());

        m.cancel_auto_stop(&rt.id, &nc()).unwrap();
        let (rt, wd) = tracked_watchdog(&m, "t4-wd");
        assert!(!wd.armed && wd.cancelled);
        assert_eq!(rt.auto_stop_at, None, "it bills until stopped");

        // "Set auto-stop": the watchdog runs on the runtime again.
        let hour = Duration::from_secs(3600);
        let info = m.extend_session(&rt.id, hour, &nc()).unwrap();
        assert!(info.armed && !info.cancelled);
        assert_eq!(info.idle_timeout, Some(Duration::from_secs(900)));
        let (rt, wd) = tracked_watchdog(&m, "t4-wd");
        assert_eq!(wd, info, "the listing shows the last known state");
        assert_eq!(rt.auto_stop_at, info.deadline);

        // The restart fails: the call reports it and keeps the app-side limit.
        m.cancel_auto_stop(&rt.id, &nc()).unwrap();
        m.set_failures(MockFailures {
            watchdog_restart_fails: true,
            ..MockFailures::default()
        });
        let before = m.now();
        let info = m.extend_session(&rt.id, hour, &nc()).unwrap();
        assert!(!info.armed && !info.cancelled);
        assert_eq!(info.deadline, None);
        assert_eq!(info.idle_timeout, Some(Duration::from_secs(900)));
        let (rt, wd) = tracked_watchdog(&m, "t4-wd");
        assert_eq!(wd, info);
        let tracked = rt.tracked.as_ref().unwrap();
        let app_stop = tracked.started_at() + tracked.policy.max_runtime.unwrap();
        assert!(
            app_stop >= before + hour,
            "the app-side limit is the new time"
        );
        assert_eq!(rt.auto_stop_at, Some(app_stop));

        // Trying again once it can start works; after that the running
        // watchdog is only moved, so the switch no longer matters.
        m.set_failures(MockFailures::default());
        assert!(m.extend_session(&rt.id, hour, &nc()).unwrap().armed);
        m.set_failures(MockFailures {
            watchdog_restart_fails: true,
            ..MockFailures::default()
        });
        let info = m.extend_session(&rt.id, hour * 2, &nc()).unwrap();
        assert!(info.armed);
        assert_eq!(info.deadline, Some(m.now() + hour * 2));
    }

    #[test]
    fn switch_impact_reports_unmanaged_runtimes_left_running() {
        let (m, _) = mock(MockScenario::demo());
        let accts = m.accounts(&nc()).unwrap();
        let impact = m.switch_impact(&accts.accounts[1].id).unwrap();
        let unmanaged = impact.unmanaged_left_running.unwrap();
        assert_eq!(unmanaged.len(), 1);
        assert_eq!(unmanaged[0].account, accts.accounts[0].id);
        assert!(unmanaged[0].name.is_none());
        let same = m.switch_impact(&accts.accounts[0].id).unwrap();
        assert_eq!(same.unmanaged_left_running, Some(vec![]));
    }

    #[test]
    fn switch_impact_never_reports_unlisted_runtimes_as_none() {
        let idle = |email: &str| MockAccount {
            email: email.into(),
            balance_cu: 100.0,
        };
        let (m, _) = mock(MockScenario {
            accounts: vec![idle("ada@example.com"), idle("lin@example.com")],
            runtimes: vec![],
        });
        let accts = m.accounts(&nc()).unwrap();
        let (ada, lin) = (&accts.accounts[0].id, &accts.accounts[1].id);
        let left = |to: &AccountId| m.switch_impact(to).unwrap().unmanaged_left_running;
        m.refresh_usage(&[], &nc()).unwrap();
        assert_eq!(left(lin), Some(vec![]), "listed, and nothing runs");

        m.set_failures(MockFailures {
            unlisted_accounts: vec!["ada@example.com".into()],
            ..MockFailures::default()
        });
        assert_eq!(left(lin), None, "a usage reading of zero is not a list");
        assert_eq!(left(ada), Some(vec![]), "staying leaves nothing behind");

        // Usage that shows the runtime does not stand in for the list either.
        let endpoint = m.add_unmanaged_runtime(ada, Accelerator::T4);
        m.refresh_usage(&[], &nc()).unwrap();
        assert_eq!(m.usage()[0].unmanaged_runtimes, 1);
        assert_eq!(left(lin), None);
        m.set_failures(MockFailures {
            runtimes_listing_fails: true,
            ..MockFailures::default()
        });
        assert_eq!(left(lin), None);

        m.set_failures(MockFailures::default());
        let listed = left(lin).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].endpoint, endpoint);
    }

    #[test]
    fn listing_failure_switches() {
        let (m, _) = mock(MockScenario::demo());
        m.set_failures(MockFailures {
            unlisted_accounts: vec!["lin@example.com".into()],
            ..MockFailures::default()
        });
        let v = m.runtimes(&nc()).unwrap();
        assert_eq!(v.errors.len(), 1, "the unlisted account is reported");
        assert!(
            v.runtimes
                .iter()
                .all(|r| r.account_email.as_deref() != Some("lin@example.com")),
            "and its runtimes are left out"
        );
        assert_eq!(v.runtimes.len(), 3);
        m.set_failures(MockFailures {
            runtimes_listing_fails: true,
            ..MockFailures::default()
        });
        assert!(m.runtimes(&nc()).is_err());
    }

    #[test]
    fn cancelling_provisioning_leaves_no_vm() {
        // Real clock with short steps, so the asynchronous cancel lands during
        // the retry wait (a manual clock would finish before it arrives).
        let mut cfg = MockConfig::interactive(Duration::from_millis(20));
        cfg.scenario = MockScenario::single();
        let m = MockBackend::new(cfg);
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        let (tx, rx) = mpsc::channel();
        let spec = ProvisionSpec {
            min_vram_mib: Some(70_000),
            retry_delay: Duration::from_secs(30),
            ..m.defaults().provision_spec("x")
        };
        // Cancel as soon as the first rejection is reported.
        let h = std::thread::spawn(move || {
            for e in rx {
                if matches!(e, ProvisionEvent::Rejected { .. }) {
                    c2.cancel();
                }
            }
        });
        let r = m.provision(spec, tx, &cancel);
        h.join().unwrap();
        assert!(r.unwrap_err().is(ErrorKind::Cancelled));
        assert!(m.runtimes(&nc()).unwrap().runtimes.is_empty());
    }
}
