//! The real backend: composes host, CLI, helper, provisioning, guard,
//! tunnels and plugins.
//!
//! Locking model: one *credential slot lock*. CLI operations (`sessions`,
//! `new`, `exec`, `stop`, tunnels) depend on which account is in the CLI's
//! single credential slot, so they take it shared and check that their
//! account is the active one (`AccountNotActive` otherwise). Switching,
//! adopting and adding accounts take it exclusively. Helper operations that
//! use saved (vault) credentials — usage, listing and releasing runtimes of any
//! account — need no lock.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use crate::accounts::{
    Account, AccountId, AccountStore, AccountsView, Helper, RemoteAssignment, auth_url_from_line,
    classify_login_failure,
};
use crate::backend::*;
use crate::cli::{Cli, CliCommand, StopOutcome};
use crate::clock::{CancelToken, Clock};
use crate::defaults::Defaults;
use crate::error::{Error, ErrorKind, Result};
use crate::guard::{Decision, GuardPolicy, SpendMeter, evaluate};
use crate::hf::{HfClient, HfModel};
use crate::host::{
    Host, HostKeepalive, Line, LineAction, ProbeReport, ScriptRequest, Stream, Toolchain, tail,
};
use crate::hostfs::HostFs;
use crate::http::HttpClient;
use crate::paths::{DataLayout, HostPath};
use crate::plugins::{
    PluginInfo, PluginOrigin, PluginPreview, PluginStore, clone_git, contains_secret, redact,
    redact_json, resolve_endpoints, resolve_inputs,
};
use crate::provision::{
    self, Hardware, ProvisionEvent, ProvisionOps, ProvisionOutcome, ProvisionSpec,
};
use crate::quant::GgufListing;
use crate::sessions::{
    Ledger, ServerSession, StopReason, StopRecord, TrackedSession, WatchdogInfo,
};
use crate::tunnel::{TunnelCtx, TunnelEvent, TunnelId, TunnelInfo, TunnelManager, TunnelSpec};
use crate::usage::{AccountUsage, poll_due};
use crate::vm::{self, RunState, RunStatus};
use crate::watchdog;

#[derive(Clone)]
pub struct RealConfig {
    pub host: Host,
    /// `colab` and its interpreter. `None`: discovered on first use (real
    /// app only; tests always pass explicit paths).
    pub toolchain: Option<Toolchain>,
    /// App-side data directory.
    pub local_dir: PathBuf,
    /// Host-side data directory. `None`: `${XDG_DATA_HOME:-~/.local/share}/colab-playground`
    /// on the host, resolved on first use.
    pub host_dir: Option<HostPath>,
    /// Environment added to every host script (tests isolate `HOME` here).
    pub base_env: Vec<(String, String)>,
    pub clock: Arc<dyn Clock>,
    /// HTTP client for Hugging Face.
    pub http: Arc<dyn HttpClient>,
    pub defaults: Defaults,
    pub run_poll_interval: Duration,
}

#[cfg(test)]
mod reconciliation_tests {
    use super::*;
    use crate::sessions::Accelerator;

    #[test]
    fn entries_started_after_the_listing_sample_are_not_marked_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = RealConfig::for_app(Defaults::default()).unwrap();
        config.local_dir = dir.path().to_owned();
        let backend = RealBackend::new(config).unwrap();
        let account = AccountId::new("alice").unwrap();
        let entry = |name: &str| TrackedSession {
            name: name.into(),
            account: account.clone(),
            endpoint: Some(format!("endpoint-{name}")),
            accelerator: Accelerator::T4,
            high_mem: false,
            gpu: None,
            vram_mib: None,
            policy: GuardPolicy::default(),
            spend: SpendMeter::new(SystemTime::UNIX_EPOCH),
            watchdog: None,
            stopped: None,
            stop_pending_confirmation: None,
        };
        backend.inner.st().ledger.upsert(entry("old")).unwrap();
        let sample = backend.inner.reconciliation_sample();
        let listing = Vec::new();
        backend.inner.st().ledger.upsert(entry("new")).unwrap();
        let vanished = backend.inner.reconcile(&account, &listing, None, &sample);
        assert_eq!(vanished.len(), 1);
        assert_eq!(vanished[0].runtime.name.as_deref(), Some("old"));
        assert!(backend.inner.st().ledger.live(&account, "new").is_some());
    }

    #[test]
    fn a_recorded_stop_is_not_overwritten_by_a_later_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = RealConfig::for_app(Defaults::default()).unwrap();
        config.local_dir = dir.path().to_owned();
        let backend = RealBackend::new(config).unwrap();
        let account = AccountId::new("alice").unwrap();
        backend
            .inner
            .st()
            .ledger
            .upsert(TrackedSession {
                name: "stopped".into(),
                account: account.clone(),
                endpoint: Some("endpoint-stopped".into()),
                accelerator: Accelerator::T4,
                high_mem: false,
                gpu: None,
                vram_mib: None,
                policy: GuardPolicy::default(),
                spend: SpendMeter::new(SystemTime::UNIX_EPOCH),
                watchdog: None,
                stopped: Some(StopRecord {
                    at: SystemTime::UNIX_EPOCH,
                    reason: StopReason::User,
                }),
                stop_pending_confirmation: None,
            })
            .unwrap();
        let sample = backend.inner.reconciliation_sample();
        let vanished = backend.inner.reconcile(&account, &[], None, &sample);
        assert!(vanished.is_empty());
        let entry = backend
            .inner
            .st()
            .ledger
            .entries()
            .iter()
            .find(|e| e.name == "stopped")
            .unwrap()
            .clone();
        assert_eq!(entry.stopped.map(|s| s.reason), Some(StopReason::User));
    }

    #[test]
    fn a_pending_stop_survives_a_listing_that_still_shows_the_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = RealConfig::for_app(Defaults::default()).unwrap();
        config.local_dir = dir.path().to_owned();
        let backend = RealBackend::new(config).unwrap();
        let account = AccountId::new("alice").unwrap();
        backend
            .inner
            .st()
            .ledger
            .upsert(TrackedSession {
                name: "pending".into(),
                account: account.clone(),
                endpoint: Some("endpoint-pending".into()),
                accelerator: Accelerator::T4,
                high_mem: false,
                gpu: None,
                vram_mib: None,
                policy: GuardPolicy::default(),
                spend: SpendMeter::new(SystemTime::UNIX_EPOCH),
                watchdog: None,
                stopped: None,
                stop_pending_confirmation: Some(StopReason::User),
            })
            .unwrap();
        let entry = |backend: &RealBackend| {
            backend
                .inner
                .st()
                .ledger
                .entries()
                .iter()
                .find(|e| e.name == "pending")
                .unwrap()
                .clone()
        };
        let sample = backend.inner.reconciliation_sample();

        // The release already succeeded, so a listing that still shows the
        // endpoint is stale: the stop is neither finalised nor dropped.
        let listing = vec![RemoteAssignment {
            endpoint: "endpoint-pending".into(),
            accelerator: Some(Accelerator::T4),
            accelerator_label: "T4".into(),
            variant: "GPU".into(),
            high_mem: false,
        }];
        let vanished = backend.inner.reconcile(&account, &listing, None, &sample);
        assert!(vanished.is_empty());
        let e = entry(&backend);
        assert!(e.stopped.is_none());
        assert_eq!(e.stop_pending_confirmation, Some(StopReason::User));

        // Once a listing confirms the endpoint is gone, the pending stop is
        // finalised with the user's reason, not a vanish.
        let vanished = backend.inner.reconcile(&account, &[], None, &sample);
        assert_eq!(vanished.len(), 1);
        assert_eq!(vanished[0].reason, StopReason::User);
        let e = entry(&backend);
        assert_eq!(e.stopped.map(|s| s.reason), Some(StopReason::User));
        assert!(e.stop_pending_confirmation.is_none());
    }

    #[test]
    fn a_session_that_ended_is_reported_even_when_the_ledger_cannot_be_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = RealConfig::for_app(Defaults::default()).unwrap();
        config.local_dir = dir.path().to_owned();
        let backend = RealBackend::new(config).unwrap();
        let account = AccountId::new("alice").unwrap();
        for name in ["first", "second"] {
            backend
                .inner
                .st()
                .ledger
                .upsert(TrackedSession {
                    name: name.into(),
                    account: account.clone(),
                    endpoint: Some(format!("endpoint-{name}")),
                    accelerator: Accelerator::T4,
                    high_mem: false,
                    gpu: None,
                    vram_mib: None,
                    policy: GuardPolicy::default(),
                    spend: SpendMeter::new(SystemTime::UNIX_EPOCH),
                    watchdog: None,
                    stopped: None,
                    stop_pending_confirmation: None,
                })
                .unwrap();
        }
        let sample = backend.inner.reconciliation_sample();
        // A directory where the file was makes every save fail.
        let ledger = dir.path().join("ledger.json");
        std::fs::remove_file(&ledger).unwrap();
        std::fs::create_dir(&ledger).unwrap();
        assert!(backend.inner.st().ledger.save().is_err());

        let vanished = backend.inner.reconcile(&account, &[], None, &sample);
        let names: Vec<_> = vanished
            .iter()
            .filter_map(|r| r.runtime.name.as_deref())
            .collect();
        assert_eq!(names, ["first", "second"]);
        assert!(backend.inner.st().ledger.live(&account, "first").is_none());
        assert!(backend.inner.st().ledger.live(&account, "second").is_none());
    }

    #[test]
    fn a_session_without_an_endpoint_takes_only_a_listed_one_and_outlasts_a_stale_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = RealConfig::for_app(Defaults::default()).unwrap();
        config.local_dir = dir.path().to_owned();
        let backend = RealBackend::new(config).unwrap();
        let account = AccountId::new("alice").unwrap();
        backend
            .inner
            .st()
            .ledger
            .upsert(TrackedSession {
                name: "reused".into(),
                account: account.clone(),
                endpoint: None,
                accelerator: Accelerator::T4,
                high_mem: false,
                gpu: None,
                vram_mib: None,
                policy: GuardPolicy::default(),
                spend: SpendMeter::new(SystemTime::UNIX_EPOCH),
                watchdog: None,
                stopped: None,
                stop_pending_confirmation: None,
            })
            .unwrap();
        let listing = |endpoint: &str| {
            vec![RemoteAssignment {
                endpoint: endpoint.into(),
                accelerator: Some(Accelerator::T4),
                accelerator_label: "T4".into(),
                variant: "GPU".into(),
                high_mem: false,
            }]
        };
        let endpoint = |backend: &RealBackend| {
            let st = backend.inner.st();
            st.ledger.live(&account, "reused").unwrap().endpoint.clone()
        };

        // Only the record of an earlier session with this name: nothing to go by.
        let stale = BTreeMap::from([("a-earlier".into(), "reused".into())]);
        let sample = backend.inner.reconciliation_sample();
        let vanished =
            backend
                .inner
                .reconcile(&account, &listing("b-current"), Some(&stale), &sample);
        assert!(vanished.is_empty());
        assert_eq!(endpoint(&backend), None);

        // Both records: the listed one is the session's.
        let both = BTreeMap::from([
            ("a-earlier".into(), "reused".into()),
            ("b-current".into(), "reused".into()),
        ]);
        let vanished =
            backend
                .inner
                .reconcile(&account, &listing("b-current"), Some(&both), &sample);
        assert!(vanished.is_empty());
        assert_eq!(endpoint(&backend).as_deref(), Some("b-current"));
    }

    #[test]
    fn a_session_without_an_endpoint_ends_when_no_listed_runtime_could_be_it() {
        let account = AccountId::new("alice").unwrap();
        let entry = |name: &str, endpoint: Option<&str>| TrackedSession {
            name: name.into(),
            account: account.clone(),
            endpoint: endpoint.map(Into::into),
            accelerator: Accelerator::T4,
            high_mem: false,
            gpu: None,
            vram_mib: None,
            policy: GuardPolicy::default(),
            spend: SpendMeter::new(SystemTime::UNIX_EPOCH),
            watchdog: None,
            stopped: None,
            stop_pending_confirmation: None,
        };
        let listing = |endpoints: &[&str]| -> Vec<RemoteAssignment> {
            endpoints
                .iter()
                .map(|endpoint| RemoteAssignment {
                    endpoint: (*endpoint).into(),
                    accelerator: Some(Accelerator::T4),
                    accelerator_label: "T4".into(),
                    variant: "GPU".into(),
                    high_mem: false,
                })
                .collect()
        };
        let sampled = |entries: &[TrackedSession]| {
            let dir = tempfile::tempdir().unwrap();
            let mut config = RealConfig::for_app(Defaults::default()).unwrap();
            config.local_dir = dir.path().to_owned();
            let backend = RealBackend::new(config).unwrap();
            for e in entries {
                backend.inner.st().ledger.upsert(e.clone()).unwrap();
            }
            let sample = backend.inner.reconciliation_sample();
            (dir, backend, sample)
        };
        let live = |backend: &RealBackend, name: &str| {
            backend.inner.st().ledger.live(&account, name).is_some()
        };

        let (_dir, backend, sample) = sampled(&[entry("lost", None)]);
        // Started after the sample, it still accounts for its runtime.
        let known = entry("known", Some("a-known"));
        backend.inner.st().ledger.upsert(known).unwrap();
        let vanished = backend
            .inner
            .reconcile(&account, &listing(&["a-known"]), None, &sample);
        let lost = Vanished {
            runtime: RuntimeRef {
                account: account.clone(),
                endpoint: String::new(),
                name: Some("lost".into()),
            },
            reason: StopReason::Vanished,
        };
        assert_eq!(vanished, [lost]);
        assert!(!live(&backend, "lost"));
        assert!(live(&backend, "known"));

        let (_dir, backend, sample) = sampled(&[entry("first", None), entry("second", None)]);
        let vanished = backend
            .inner
            .reconcile(&account, &listing(&["b-unclaimed"]), None, &sample);
        assert!(vanished.is_empty());
        assert!(live(&backend, "first") && live(&backend, "second"));

        backend
            .inner
            .st()
            .ledger
            .upsert(entry("late", None))
            .unwrap();
        let vanished = backend.inner.reconcile(&account, &[], None, &sample);
        let names: Vec<_> = vanished
            .iter()
            .filter_map(|r| r.runtime.name.as_deref())
            .collect();
        assert_eq!(names, ["first", "second"]);
        assert!(live(&backend, "late"));
    }

    #[test]
    fn reused_session_names_do_not_reconcile_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = RealConfig::for_app(Defaults::default()).unwrap();
        config.local_dir = dir.path().to_owned();
        let backend = RealBackend::new(config).unwrap();
        let account = AccountId::new("alice").unwrap();
        let original = TrackedSession {
            name: "reused".into(),
            account: account.clone(),
            endpoint: Some("old-endpoint".into()),
            accelerator: Accelerator::T4,
            high_mem: false,
            gpu: None,
            vram_mib: None,
            policy: GuardPolicy::default(),
            spend: SpendMeter::new(SystemTime::UNIX_EPOCH),
            watchdog: None,
            stopped: None,
            stop_pending_confirmation: None,
        };
        for endpoint in [
            None,
            Some("old-endpoint".into()),
            Some("new-endpoint".into()),
        ] {
            backend.inner.st().ledger.upsert(original.clone()).unwrap();
            let sample = backend.inner.reconciliation_sample();
            let mut replacement = original.clone();
            replacement.endpoint = endpoint;
            replacement.spend = SpendMeter::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1));
            backend
                .inner
                .st()
                .ledger
                .upsert(replacement.clone())
                .unwrap();
            let names = BTreeMap::from([("listed-endpoint".into(), "reused".into())]);
            assert!(
                backend
                    .inner
                    .reconcile(&account, &[], Some(&names), &sample)
                    .is_empty()
            );
            assert_eq!(
                backend.inner.st().ledger.live(&account, "reused"),
                Some(&replacement)
            );
        }
    }
}

impl RealConfig {
    /// A configuration for the real app: `Host::detect()`, the OS data
    /// directory, lazy toolchain discovery.
    pub fn for_app(defaults: Defaults) -> Result<RealConfig> {
        let local_dir = crate::paths::default_local_data_dir()
            .ok_or_else(|| Error::new(ErrorKind::Io).detail("no data directory"))?;
        Ok(RealConfig {
            host: Host::detect(),
            toolchain: None,
            local_dir,
            host_dir: None,
            base_env: Vec::new(),
            clock: Arc::new(crate::clock::SystemClock),
            http: crate::http::default_client(),
            defaults,
            run_poll_interval: Duration::from_secs(10),
        })
    }
}

#[derive(Default)]
struct SlotState {
    readers: u32,
    writer: bool,
}

#[derive(Default)]
struct SlotLock {
    state: Mutex<SlotState>,
    cv: Condvar,
}

struct ReadGuard(Arc<SlotLock>);
struct WriteGuard(Arc<SlotLock>);

impl Drop for ReadGuard {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        s.readers -= 1;
        self.0.cv.notify_all();
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        s.writer = false;
        self.0.cv.notify_all();
    }
}

fn slot_read(lock: &Arc<SlotLock>, timeout: Duration) -> Result<ReadGuard> {
    let deadline = Instant::now() + timeout;
    let mut s = lock.state.lock().unwrap_or_else(|e| e.into_inner());
    while s.writer {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::new(ErrorKind::Busy).detail("an account change is in progress"));
        }
        s = lock
            .cv
            .wait_timeout(s, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    s.readers += 1;
    Ok(ReadGuard(lock.clone()))
}

fn slot_write(lock: &Arc<SlotLock>, timeout: Duration) -> Result<WriteGuard> {
    let deadline = Instant::now() + timeout;
    let mut s = lock.state.lock().unwrap_or_else(|e| e.into_inner());
    while s.writer || s.readers > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::new(ErrorKind::Busy)
                .detail("Colab operations are running on the active account"));
        }
        s = lock
            .cv
            .wait_timeout(s, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    s.writer = true;
    Ok(WriteGuard(lock.clone()))
}

struct State {
    accounts: AccountStore,
    ledger: Ledger,
    plugins: PluginStore,
    usage: BTreeMap<AccountId, AccountUsage>,
    defaults: Defaults,
    active: Option<AccountId>,
    active_known: bool,
    reserved: u32,
    toolchain: Option<Toolchain>,
    host_dir: Option<HostPath>,
}

struct Inner {
    cfg: RealConfig,
    slot: Arc<SlotLock>,
    state: Mutex<State>,
    tunnels: TunnelManager,
    login_active: AtomicBool,
    /// Keeps WSL up between commands, so its keyring stays unlocked (see [`HostKeepalive`]).
    keepalive: HostKeepalive,
    /// The one-driver lock for the data directory (see [`crate::driver_lock`]);
    /// held for the backend's lifetime.
    _driver_guard: Arc<crate::driver_lock::DriverGuard>,
}

#[derive(Clone)]
pub struct RealBackend {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for RealBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealBackend")
            .field("host", &self.inner.cfg.host)
            .finish()
    }
}

const HOST_DATA_DIR: &str =
    r#"printf '__CPG_DIR__ %s\n' "${XDG_DATA_HOME:-$HOME/.local/share}/colab-playground""#;
const COPY_DEFAULT_STATE: &str = r#"src="$HOME/.config/colab-cli/sessions.json"
[ -f "$src" ] || exit 0
[ -f "$1" ] && exit 0
umask 077
mkdir -p -- "$(dirname -- "$1")" && cp -- "$src" "$1""#;
const SLOT_TIMEOUT: Duration = Duration::from_secs(5);

impl RealBackend {
    /// Loads app-side files; spawns nothing.
    pub fn new(cfg: RealConfig) -> Result<RealBackend> {
        std::fs::create_dir_all(&cfg.local_dir)?;
        // One driver per data directory: a second process (the GUI next to
        // an MCP server, or two servers) fails here, before any state is
        // loaded, instead of overwriting this backend's saves later.
        let driver_guard = crate::driver_lock::acquire_driver_lock(&cfg.local_dir)?;
        let probe_layout = DataLayout::new(cfg.local_dir.clone(), HostPath::new("/"));
        let state = State {
            accounts: AccountStore::load(&probe_layout.accounts_json())?,
            ledger: Ledger::load(&probe_layout.ledger_json())?,
            plugins: PluginStore::load(
                Some(&probe_layout.plugins_dir()),
                Some(&probe_layout.plugins_json()),
            )?,
            usage: BTreeMap::new(),
            defaults: cfg.defaults.clone(),
            active: None,
            active_known: false,
            reserved: 0,
            toolchain: cfg.toolchain.clone(),
            host_dir: cfg.host_dir.clone(),
        };
        Ok(RealBackend {
            inner: Arc::new(Inner {
                keepalive: HostKeepalive::new(cfg.host.clone()),
                cfg,
                slot: Arc::new(SlotLock::default()),
                state: Mutex::new(state),
                tunnels: TunnelManager::new(),
                login_active: AtomicBool::new(false),
                _driver_guard: driver_guard,
            }),
        })
    }
}

impl Inner {
    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> SystemTime {
        self.cfg.clock.now()
    }

    fn fs(&self) -> HostFs<'_> {
        HostFs::new(&self.cfg.host, &self.cfg.base_env)
    }

    fn toolchain(&self) -> Result<Toolchain> {
        if let Some(t) = self.st().toolchain.clone() {
            if !t.needs_resolution() {
                return Ok(t);
            }
            // Only the colab path was given: derive the interpreter.
            let resolved = t.resolve(&self.cfg.host, &self.cfg.base_env, &CancelToken::new())?;
            self.st().toolchain = Some(resolved.clone());
            return Ok(resolved);
        }
        let report = self.cfg.host.probe();
        match report.toolchain() {
            Some(t) => {
                self.st().toolchain = Some(t.clone());
                Ok(t)
            }
            None if self.cfg.host != Host::Local
                && (!report.wsl_installed || report.wsl_distros.is_empty()) =>
            {
                Err(Error::new(ErrorKind::WslUnavailable))
            }
            None => Err(Error::new(ErrorKind::NotInstalled).with("program", "colab")),
        }
    }

    fn layout(&self, cancel: &CancelToken) -> Result<DataLayout> {
        if let Some(h) = self.st().host_dir.clone() {
            return Ok(DataLayout::new(self.cfg.local_dir.clone(), h));
        }
        let mut req = ScriptRequest::new(HOST_DATA_DIR).timeout(Duration::from_secs(60));
        req.env = self.cfg.base_env.clone();
        let out = self.cfg.host.run_quiet(req, cancel)?;
        let dir = out
            .stdout
            .lines()
            .find_map(|l| l.strip_prefix("__CPG_DIR__ "))
            .map(|d| HostPath::new(d.trim()))
            .ok_or_else(|| Error::new(ErrorKind::Other).detail(tail(&out.stderr, 300)))?;
        self.st().host_dir = Some(dir.clone());
        Ok(DataLayout::new(self.cfg.local_dir.clone(), dir))
    }

    fn local_layout(&self) -> DataLayout {
        DataLayout::new(self.cfg.local_dir.clone(), HostPath::new("/"))
    }

    // Both read the keyring, so both keep the host up first.
    fn cli(&self) -> Result<Cli> {
        let colab = self.toolchain()?.colab;
        self.keepalive.ensure();
        Ok(Cli::new(
            self.cfg.host.clone(),
            colab,
            self.cfg.base_env.clone(),
        ))
    }

    fn helper(&self) -> Result<Helper> {
        let python = self.toolchain()?.helper_python;
        self.keepalive.ensure();
        Ok(Helper::new(
            self.cfg.host.clone(),
            python,
            self.cfg.base_env.clone(),
        ))
    }

    fn known(&self) -> Vec<AccountId> {
        self.st().accounts.ids()
    }

    /// The active account, asking the helper once if unknown.
    fn active(&self, cancel: &CancelToken) -> Result<Option<AccountId>> {
        if self.st().active_known {
            return Ok(self.st().active.clone());
        }
        let status = self.helper()?.status(&self.known(), cancel)?;
        let mut st = self.st();
        st.active = status.matches;
        st.active_known = true;
        Ok(st.active.clone())
    }

    /// Takes the slot shared and checks that `account` is active.
    fn cli_for(
        &self,
        account: &AccountId,
        cancel: &CancelToken,
    ) -> Result<(ReadGuard, Cli, HostPath)> {
        let guard = slot_read(&self.slot, SLOT_TIMEOUT)?;
        match self.active(cancel)? {
            Some(a) if &a == account => {}
            Some(_) | None => {
                return Err(Error::new(ErrorKind::AccountNotActive).with("account", account));
            }
        }
        let state = self.layout(cancel)?.account_state(account);
        // The CLI writes its state file and lock inside this directory but
        // does not create the directory itself; a brand-new account whose
        // first action is a session start fails with FileNotFoundError.
        self.fs().ensure_dir(&state.parent(), cancel)?;
        Ok((guard, self.cli()?, state))
    }

    fn active_cli(&self, cancel: &CancelToken) -> Result<(ReadGuard, Cli, HostPath, AccountId)> {
        let account = self.active(cancel)?.ok_or_else(|| {
            let has_token = self
                .helper()
                .and_then(|h| h.status(&[], cancel))
                .map(|s| s.has_token)
                .unwrap_or(false);
            Error::new(if has_token {
                ErrorKind::ActiveAccountNotAdopted
            } else {
                ErrorKind::NoActiveAccount
            })
        })?;
        let (g, cli, state) = self.cli_for(&account, cancel)?;
        Ok((g, cli, state, account))
    }

    fn live_names(&self, account: &AccountId) -> BTreeSet<String> {
        self.st()
            .ledger
            .entries()
            .iter()
            .filter(|e| &e.account == account && e.stopped.is_none())
            .map(|e| e.name.clone())
            .collect()
    }

    fn accounts_view(&self, cancel: &CancelToken, recover: bool) -> Result<AccountsView> {
        let helper = self.helper()?;
        let known = self.known();
        let mut status = helper.status(&known, cancel)?;
        let mut recovered = false;
        if recover && status.pending_add && !self.login_active.load(Ordering::SeqCst) {
            recovered = helper.rollback_add(cancel)?;
            status = helper.status(&known, cancel)?;
        }
        let now = self.now();
        let mut st = self.st();
        st.active = status.matches.clone();
        st.active_known = true;
        if let (Some(id), Some(email)) = (&status.matches, &status.email)
            && let Some(mut a) = st.accounts.get(id).cloned()
            && a.email.as_deref() != Some(email.as_str())
        {
            a.email = Some(email.clone());
            a.updated_at = now;
            st.accounts.upsert(a)?;
        }
        Ok(AccountsView {
            accounts: st.accounts.all().to_vec(),
            active: status.matches.clone(),
            has_token: status.has_token,
            unadopted: status.has_token && status.matches.is_none(),
            active_email: status.email,
            recovered_pending_add: recovered,
        })
    }

    /// Saves the slot's credential as an account (exclusive slot must be held).
    fn adopt_locked(&self, cancel: &CancelToken) -> Result<Account> {
        let helper = self.helper()?;
        let known = self.known();
        let status = helper.status(&known, cancel)?;
        if !status.has_token {
            return Err(Error::new(ErrorKind::AuthRequired));
        }
        if let Some(id) = status.matches {
            self.st().active = Some(id.clone());
            return self
                .st()
                .accounts
                .get(&id)
                .cloned()
                .ok_or_else(|| Error::new(ErrorKind::AccountNotFound));
        }
        let existing = status
            .email
            .as_deref()
            .and_then(|e| self.st().accounts.by_email(e).cloned());
        let id = existing
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_else(AccountId::generate);
        let email = helper.adopt(&id, cancel)?;
        if existing.is_none() {
            let layout = self.layout(cancel)?;
            let mut req =
                ScriptRequest::new(COPY_DEFAULT_STATE).arg(layout.account_state(&id).as_str());
            req.env = self.cfg.base_env.clone();
            let _ = self
                .cfg
                .host
                .run_quiet(req.timeout(Duration::from_secs(60)), cancel);
        }
        let now = self.now();
        let account = Account {
            id: id.clone(),
            email: email.or(status.email),
            created_at: existing.as_ref().map(|a| a.created_at).unwrap_or(now),
            updated_at: now,
            usage: existing.and_then(|a| a.usage),
        };
        let mut st = self.st();
        st.accounts.upsert(account.clone())?;
        st.active = Some(id);
        st.active_known = true;
        Ok(account)
    }

    fn ops<'a>(
        &'a self,
        cli: &'a Cli,
        account: &AccountId,
        state: &HostPath,
        scratch: HostPath,
    ) -> RealOps<'a> {
        RealOps {
            inner: self,
            cli,
            account: account.clone(),
            state: state.clone(),
            scratch,
        }
    }

    fn mark_stopped(&self, account: &AccountId, name: &str, reason: StopReason) {
        let now = self.now();
        let _ = self.st().ledger.update(account, name, |e| {
            e.stopped = Some(StopRecord { at: now, reason })
        });
    }

    /// Records a stop whose release succeeded but whose confirmation listing
    /// failed: the intended reason is kept on the live entry and reconcile
    /// finalises it once the endpoint is confirmed gone.
    fn mark_stop_pending(&self, account: &AccountId, name: &str, reason: StopReason) {
        let _ = self.st().ledger.update(account, name, |e| {
            e.stop_pending_confirmation = Some(reason);
        });
    }

    /// The name of an unstopped ledger entry on `account` whose endpoint is
    /// `endpoint` (newest first), or `None` when none exists.
    fn ledger_name_by_endpoint(&self, account: &AccountId, endpoint: &str) -> Option<String> {
        self.st()
            .ledger
            .entries()
            .iter()
            .rev()
            .find(|e| {
                &e.account == account
                    && e.endpoint.as_deref() == Some(endpoint)
                    && e.stopped.is_none()
            })
            .map(|e| e.name.clone())
    }

    /// Names recorded in an account's state file, by endpoint.
    fn state_names(
        &self,
        account: &AccountId,
        cancel: &CancelToken,
    ) -> Option<BTreeMap<String, String>> {
        let layout = self.layout(cancel).ok()?;
        let text = self
            .fs()
            .read(&layout.account_state(account), cancel)
            .ok()
            .flatten()?;
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text).ok()?;
        Some(
            map.into_iter()
                .filter_map(|(name, rec)| {
                    rec.get("endpoint")
                        .and_then(|e| e.as_str())
                        .map(|e| (e.to_owned(), name))
                })
                .collect(),
        )
    }

    fn reconciliation_sample(&self) -> BTreeMap<AccountId, Vec<TrackedSession>> {
        let mut sample: BTreeMap<AccountId, Vec<TrackedSession>> = BTreeMap::new();
        for entry in self
            .st()
            .ledger
            .entries()
            .iter()
            .filter(|e| e.stopped.is_none())
        {
            sample
                .entry(entry.account.clone())
                .or_default()
                .push(entry.clone());
        }
        sample
    }

    fn reconcile(
        &self,
        account: &AccountId,
        list: &[RemoteAssignment],
        names: Option<&BTreeMap<String, String>>,
        sample: &BTreeMap<AccountId, Vec<TrackedSession>>,
    ) -> Vec<Vanished> {
        let listed = |endpoint: &str| list.iter().any(|a| a.endpoint == endpoint);
        let mut st = self.st();
        let live: Vec<_> = st
            .ledger
            .entries()
            .iter()
            .filter(|e| {
                &e.account == account
                    && e.stopped.is_none()
                    && sample.get(account).is_some_and(|entries| {
                        entries.iter().any(|sampled| {
                            sampled.name == e.name
                                && sampled.started_at() == e.started_at()
                                && sampled.endpoint == e.endpoint
                        })
                    })
            })
            .cloned()
            .collect();
        let mut vanished = Vec::new();
        let mut unmatched = Vec::new();
        // A failed save keeps the change in memory and the next save writes it, so the
        // round goes on: a session that ended is still reported.
        for e in live {
            // A stop whose release succeeded but whose confirmation listing failed is
            // finalised here: once the endpoint is gone from the server's list the stop
            // is confirmed. A listing that still shows the endpoint is stale — the
            // release already succeeded — so the pending reason is kept until a later
            // listing confirms the endpoint is gone; dropping it here would record
            // the stop as a vanish once the listing catches up.
            if let Some(reason) = e.stop_pending_confirmation.clone() {
                let gone = e
                    .endpoint
                    .as_deref()
                    .is_none_or(|endpoint| !listed(endpoint));
                if gone {
                    let _ = st.ledger.update(account, &e.name, |x| {
                        x.stop_pending_confirmation = None;
                        x.stopped = Some(StopRecord {
                            at: self.now(),
                            reason: reason.clone(),
                        });
                    });
                    vanished.push(Vanished {
                        runtime: RuntimeRef {
                            account: account.clone(),
                            endpoint: e.endpoint.clone().unwrap_or_default(),
                            name: Some(e.name),
                        },
                        reason,
                    });
                }
                continue;
            }
            let Some(endpoint) = e.endpoint.clone() else {
                // The state file may still hold the record of an earlier session with this
                // name, so only an endpoint the listing confirms is taken from it, and an
                // unlisted one never ends the session.
                let confirmed = names.and_then(|names| {
                    names
                        .iter()
                        .find(|(endpoint, name)| *name == &e.name && listed(endpoint))
                        .map(|(endpoint, _)| endpoint.clone())
                });
                match confirmed {
                    Some(endpoint) => {
                        let _ = st
                            .ledger
                            .update(account, &e.name, |e| e.endpoint = Some(endpoint));
                    }
                    None => unmatched.push(e.name),
                }
                continue;
            };
            if !listed(&endpoint) {
                let _ = st.ledger.update(account, &e.name, |e| {
                    e.stopped = Some(StopRecord {
                        at: self.now(),
                        reason: StopReason::Vanished,
                    });
                });
                vanished.push(Vanished {
                    runtime: RuntimeRef {
                        account: account.clone(),
                        endpoint,
                        name: Some(e.name),
                    },
                    reason: StopReason::Vanished,
                });
            }
        }
        // A session without an endpoint can only be a listed runtime that no other live
        // session claims; with none left, its runtime has ended.
        let claimed = |endpoint: &str| {
            st.ledger.entries().iter().any(|e| {
                &e.account == account
                    && e.stopped.is_none()
                    && e.endpoint.as_deref() == Some(endpoint)
            })
        };
        if !unmatched.is_empty() && list.iter().all(|a| claimed(&a.endpoint)) {
            for name in unmatched {
                let _ = st.ledger.update(account, &name, |e| {
                    e.stopped = Some(StopRecord {
                        at: self.now(),
                        reason: StopReason::Vanished,
                    });
                });
                vanished.push(Vanished {
                    runtime: RuntimeRef {
                        account: account.clone(),
                        endpoint: String::new(),
                        name: Some(name),
                    },
                    reason: StopReason::Vanished,
                });
            }
        }
        vanished
    }

    fn reconcile_accounts(
        &self,
        ids: &[AccountId],
        cancel: &CancelToken,
    ) -> (BTreeSet<AccountId>, Vec<Vanished>) {
        let mut verified = BTreeSet::new();
        let mut vanished = Vec::new();
        let sample = self.reconciliation_sample();
        if let Ok(helper) = self.helper()
            && let Ok(listed) = helper.assignments(ids, cancel)
        {
            for (account, result) in listed {
                if let Ok(list) = result {
                    let names = self.state_names(&account, cancel);
                    vanished.extend(self.reconcile(&account, &list, names.as_ref(), &sample));
                    if !self.st().ledger.entries().iter().any(|e| {
                        e.account == account && e.stopped.is_none() && e.endpoint.is_none()
                    }) {
                        verified.insert(account);
                    }
                }
            }
        }
        (verified, vanished)
    }

    fn stop(&self, target: &RuntimeRef, opts: &StopOptions, cancel: &CancelToken) -> Result<()> {
        let nc = CancelToken::new();
        let name = target
            .name
            .clone()
            .or_else(|| {
                // Parity with `runtimes()`: the state file maps endpoints to the
                // names the CLI recorded for them.
                self.state_names(&target.account, &nc)
                    .and_then(|names| names.get(&target.endpoint).cloned())
            })
            .or_else(|| self.ledger_name_by_endpoint(&target.account, &target.endpoint));
        if let Some(n) = &name {
            self.tunnels.close_session(&target.account, n);
        }
        let mut stopped_by_cli = false;
        if let Some(n) = &name
            && let Ok((_g, cli, state)) = self.cli_for(&target.account, cancel)
            && let Ok(StopOutcome::Terminated) = cli.stop(&state, n, &nc)
        {
            stopped_by_cli = true;
        }
        // The CLI did not end it and there is no endpoint to release, so its runtime may still
        // be billing; the session stays live until reconciliation can tell that it has ended.
        if !stopped_by_cli && target.endpoint.is_empty() {
            return Err(
                Error::new(ErrorKind::OrphanSuspected).detail("the session's runtime is not known")
            );
        }
        let helper = self.helper()?;
        if !stopped_by_cli {
            match helper.unassign(&target.account, &target.endpoint, &nc) {
                Ok(()) => {}
                Err(e) if e.kind == ErrorKind::SessionGone => {}
                Err(e) => return Err(e),
            }
        }
        // Verify: the endpoint must be gone from the server's list.
        let listed = helper.assignments(std::slice::from_ref(&target.account), &nc)?;
        let still = listed
            .into_iter()
            .find(|(a, _)| a == &target.account)
            .map(|(_, r)| r.map(|list| list.iter().any(|x| x.endpoint == target.endpoint)));
        match still {
            Some(Ok(false)) => {}
            Some(Ok(true)) => {
                return Err(
                    Error::new(ErrorKind::OrphanSuspected).with("endpoint", &target.endpoint)
                );
            }
            Some(Err(e)) => {
                if let Some(n) = &name {
                    self.mark_stop_pending(&target.account, n, opts.reason.clone());
                }
                return Err(Error::new(ErrorKind::StopUnconfirmed)
                    .with("endpoint", &target.endpoint)
                    .detail(e.to_string()));
            }
            None => {
                if let Some(n) = &name {
                    self.mark_stop_pending(&target.account, n, opts.reason.clone());
                }
                return Err(
                    Error::new(ErrorKind::StopUnconfirmed).with("endpoint", &target.endpoint)
                );
            }
        }
        // A verified-gone release must not leave the record live: if the name
        // could not be resolved, mark whatever unstopped entry owns the
        // endpoint. Only an unmanaged runtime (no matching entry) is returned
        // as stopped with nothing recorded.
        let mark_name = name
            .clone()
            .or_else(|| self.ledger_name_by_endpoint(&target.account, &target.endpoint));
        if let Some(n) = &mark_name {
            self.mark_stopped(&target.account, n, opts.reason.clone());
        }
        Ok(())
    }

    fn record_usage(
        &self,
        results: Vec<(AccountId, Result<crate::usage::Usage>)>,
        verified: &BTreeSet<AccountId>,
    ) {
        let now = self.now();
        for (id, res) in results {
            let live = self.live_names(&id);
            let mut st = self.st();
            let entry = st.usage.entry(id.clone()).or_default();
            match res {
                Ok(u) => {
                    entry.record_success(u, now, &live);
                    entry.ownership_verified = live.is_empty() || verified.contains(&id);
                    let snap = entry.snapshot;
                    if let Some(s) = snap {
                        let _ = st.accounts.set_usage(&id, s);
                    }
                }
                Err(e) => entry.record_failure(e, now),
            }
        }
    }

    fn usage_views(&self) -> Vec<AccountUsageView> {
        let st = self.st();
        st.accounts
            .all()
            .iter()
            .map(|a| {
                let usage = st.usage.get(&a.id).cloned().unwrap_or_default();
                let known = st
                    .ledger
                    .entries()
                    .iter()
                    .filter(|e| e.account == a.id && e.stopped.is_none())
                    .count();
                usage_view(&a.id, a.email.clone(), &usage, known)
            })
            .collect()
    }
}

struct RealOps<'a> {
    inner: &'a Inner,
    cli: &'a Cli,
    account: AccountId,
    state: HostPath,
    scratch: HostPath,
}

impl ProvisionOps for RealOps<'_> {
    fn sessions(&self, cancel: &CancelToken) -> Result<Vec<ServerSession>> {
        self.cli.sessions(&self.state, cancel)
    }
    fn create(
        &self,
        name: &str,
        accelerator: crate::sessions::Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<()> {
        self.cli
            .new_session(&self.state, name, accelerator, high_mem, cancel)
    }
    fn probe(&self, name: &str, cancel: &CancelToken) -> Result<Hardware> {
        let v = self
            .cli
            .exec_json(&self.state, name, &[], vm::PROBE_HARDWARE, 120, cancel)?;
        Ok(vm::parse_hardware(&v))
    }
    fn stop(&self, name: &str, cancel: &CancelToken) -> Result<StopOutcome> {
        self.cli.stop(&self.state, name, cancel)
    }
    fn reap(&self, endpoint: &str, cancel: &CancelToken) -> Result<bool> {
        match self
            .inner
            .helper()?
            .unassign(&self.account, endpoint, cancel)
        {
            Ok(()) => Ok(true),
            Err(e) if e.kind == ErrorKind::SessionGone => Ok(false),
            Err(e) if e.kind == ErrorKind::UnsupportedCliVersion => {
                self.cli.reap_endpoint(&self.scratch, endpoint, cancel)
            }
            Err(e) => Err(e),
        }
    }
    fn start_watchdog(
        &self,
        name: &str,
        policy: &GuardPolicy,
        cancel: &CancelToken,
    ) -> Result<Option<WatchdogInfo>> {
        // Read before the call: a state guard taken inside the argument list
        // would stay locked for the whole remote execution.
        let grace = self.inner.st().defaults.winddown_grace;
        let v = self.cli.exec_json(
            &self.state,
            name,
            &watchdog::start_env(policy, grace, &[]),
            &watchdog::start_snippet(),
            120,
            cancel,
        )?;
        let reply = watchdog::parse_reply(&v, self.inner.now())?;
        if !reply.alive {
            return Err(Error::new(ErrorKind::Other).detail("the watchdog process is not running"));
        }
        if !reply.can_release {
            return Err(
                Error::new(ErrorKind::Other).detail("TBE_RUNTIME_ADDR is unknown on the VM")
            );
        }
        Ok(Some(reply.info))
    }
}

struct RealLoginFlow {
    inner: Arc<Inner>,
    child: Option<crate::host::HostChild>,
    url: String,
    new_id: AccountId,
    finished: bool,
    _guard: WriteGuard,
}

impl RealLoginFlow {
    fn rollback(&mut self) -> Result<()> {
        if let Some(mut c) = self.child.take() {
            c.kill();
        }
        self.finished = true;
        let r = self
            .inner
            .helper()
            .and_then(|h| h.rollback_add(&CancelToken::new()))
            .map(|_| ());
        self.inner.login_active.store(false, Ordering::SeqCst);
        self.inner.st().active_known = false;
        r
    }
}

impl Drop for RealLoginFlow {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.rollback();
        }
    }
}

impl LoginFlow for RealLoginFlow {
    fn auth_url(&self) -> &str {
        &self.url
    }

    fn submit_code(mut self: Box<Self>, code: &str, cancel: &CancelToken) -> Result<Account> {
        let code = code.trim();
        if code.is_empty() || code.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::invalid("code", "paste the code exactly as shown"));
        }
        let mut child = self
            .child
            .take()
            .ok_or_else(|| Error::new(ErrorKind::LoginCancelled))?;
        child.write_stdin(format!("{code}\n").as_bytes())?;
        child.close_stdin();
        let deadline = Instant::now() + Duration::from_secs(180);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        loop {
            if cancel.is_cancelled() {
                self.child = Some(child);
                let _ = self.rollback();
                return Err(Error::cancelled());
            }
            match child.recv_line(Duration::from_millis(100)) {
                Ok(l) if l.stream == Stream::Stdout => out.push(l.text),
                Ok(l) => err.push(l.text),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() > deadline {
                self.child = Some(child);
                let _ = self.rollback();
                return Err(Error::new(ErrorKind::Timeout));
            }
        }
        let code = child.wait();
        if code != Some(0) {
            let e = classify_login_failure(&out.join("\n"), &err.join("\n"));
            let _ = self.rollback();
            return Err(e);
        }
        let helper = self.inner.helper()?;
        let nc = CancelToken::new();
        let email = match helper.finish_add(&self.new_id, &nc) {
            Ok(e) => e,
            Err(e) => {
                let _ = self.rollback();
                return Err(e);
            }
        };
        self.finished = true;
        self.inner.login_active.store(false, Ordering::SeqCst);
        let now = self.inner.now();
        let existing = self.inner.st().accounts.by_email(&email).cloned();
        let account = match existing {
            // Signed in again to a saved account: keep its id (and state file).
            Some(mut a) => {
                helper.adopt(&a.id, &nc)?;
                let mut known = self.inner.known();
                known.push(self.new_id.clone());
                let _ = helper.remove(&self.new_id, &known, &nc);
                a.updated_at = now;
                a
            }
            None => Account {
                id: self.new_id.clone(),
                email: Some(email),
                created_at: now,
                updated_at: now,
                usage: None,
            },
        };
        let mut st = self.inner.st();
        st.accounts.upsert(account.clone())?;
        st.active = Some(account.id.clone());
        st.active_known = true;
        Ok(account)
    }

    fn cancel(mut self: Box<Self>) -> Result<()> {
        self.rollback()
    }
}

fn install_script(method: InstallMethod) -> &'static str {
    match method {
        InstallMethod::Uv => {
            "command -v uv >/dev/null 2>&1 || { echo 'uv: command not found' >&2; exit 127; }\nuv tool install google-colab-cli"
        }
        InstallMethod::InstallUvThenCli => {
            "curl -LsSf https://astral.sh/uv/install.sh | sh || exit 1\n\"$HOME/.local/bin/uv\" tool install google-colab-cli"
        }
        InstallMethod::PipUser => "python3 -m pip install --user google-colab-cli",
    }
}

impl Backend for RealBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Real
    }

    fn defaults(&self) -> Defaults {
        self.inner.st().defaults.clone()
    }

    fn set_defaults(&self, defaults: Defaults) {
        self.inner.st().defaults = defaults;
    }

    fn now(&self) -> SystemTime {
        self.inner.now()
    }

    fn probe(&self, _cancel: &CancelToken) -> Result<ProbeReport> {
        let report = self.inner.cfg.host.probe();
        if let Some(t) = report.toolchain() {
            self.inner.st().toolchain = Some(t);
        }
        Ok(report)
    }

    fn install_cli(
        &self,
        method: InstallMethod,
        log: Events<Line>,
        cancel: &CancelToken,
    ) -> Result<()> {
        let mut req = ScriptRequest::new(install_script(method)).timeout(Duration::from_secs(1800));
        req.env = self.inner.cfg.base_env.clone();
        let out = self.inner.cfg.host.run(req, cancel, &mut |l| {
            let _ = log.send(l.clone());
            LineAction::Continue
        })?;
        if !out.success() {
            return Err(Error::new(ErrorKind::InstallFailed)
                .with(
                    "exit_code",
                    out.code.map(|c| c.to_string()).unwrap_or_default(),
                )
                .detail(tail(&out.stderr, 600)));
        }
        if self.inner.cfg.toolchain.is_none() {
            self.inner.st().toolchain = None;
            let _ = self.inner.toolchain();
        }
        Ok(())
    }

    fn accounts(&self, cancel: &CancelToken) -> Result<AccountsView> {
        self.inner.accounts_view(cancel, true)
    }

    fn adopt_active_account(&self, cancel: &CancelToken) -> Result<Account> {
        let _g = slot_write(&self.inner.slot, SLOT_TIMEOUT)?;
        self.inner.adopt_locked(cancel)
    }

    fn begin_add_account(&self, cancel: &CancelToken) -> Result<Box<dyn LoginFlow>> {
        if self.inner.login_active.swap(true, Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::AddInProgress));
        }
        let start = || -> Result<Box<dyn LoginFlow>> {
            let guard = slot_write(&self.inner.slot, SLOT_TIMEOUT)?;
            for t in self.inner.tunnels.list() {
                let _ = self.inner.tunnels.close(t.id);
            }
            let helper = self.inner.helper()?;
            let status = helper.status(&self.inner.known(), cancel)?;
            if status.has_token && status.matches.is_none() {
                self.inner.adopt_locked(cancel)?;
            }
            helper.begin_add(&self.inner.known(), cancel)?;
            self.inner.st().active_known = false;
            let cli = self.inner.cli()?;
            let req = cli.request(None, &CliCommand::Whoami)?.stdin_pipe();
            let mut flow = RealLoginFlow {
                inner: self.inner.clone(),
                child: None,
                url: String::new(),
                new_id: AccountId::generate(),
                finished: false,
                _guard: guard,
            };
            let mut child = match self.inner.cfg.host.spawn(req) {
                Ok(c) => c,
                Err(e) => {
                    let _ = flow.rollback();
                    return Err(e);
                }
            };
            let deadline = Instant::now() + Duration::from_secs(90);
            let mut err = Vec::new();
            loop {
                if cancel.is_cancelled() {
                    flow.child = Some(child);
                    let _ = flow.rollback();
                    return Err(Error::cancelled());
                }
                match child.recv_line(Duration::from_millis(100)) {
                    Ok(l) => {
                        if let Some(url) = (l.stream == Stream::Stderr)
                            .then(|| auth_url_from_line(&l.text))
                            .flatten()
                        {
                            flow.url = url;
                            flow.child = Some(child);
                            return Ok(Box::new(flow));
                        }
                        err.push(l.text);
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                if Instant::now() > deadline {
                    break;
                }
            }
            child.kill();
            let _ = flow.rollback();
            Err(classify_login_failure("", &err.join("\n")))
        };
        let r = start();
        if r.is_err() {
            self.inner.login_active.store(false, Ordering::SeqCst);
        }
        r
    }

    fn switch_impact(&self, to: &AccountId) -> Result<SwitchImpact> {
        let st = self.inner.st();
        if st.accounts.get(to).is_none() {
            return Err(Error::new(ErrorKind::AccountNotFound).with("account", to));
        }
        let from = st.active.clone();
        let sessions_left_running = st
            .ledger
            .entries()
            .iter()
            .filter(|e| {
                e.stopped.is_none()
                    && Some(&e.account) == from.as_ref()
                    && from.as_ref() != Some(to)
            })
            .cloned()
            .collect();
        drop(st);
        let tunnels_closing = if self.inner.st().active.as_ref() == Some(to) {
            vec![]
        } else {
            self.inner
                .tunnels
                .list()
                .into_iter()
                .filter(|t| !t.state.is_terminal())
                .collect()
        };
        // Unmanaged runtimes on the account being left, listed through the
        // helper (no switch needed). The usage reading cannot stand in for
        // the list: it is older than the question, and the sessions to
        // subtract from it may have ended.
        let mut unmanaged_left_running = Some(Vec::new());
        if let Some(f) = from.as_ref().filter(|f| *f != to) {
            let live = self.inner.live_names(f);
            let tracked: BTreeSet<String> = self
                .inner
                .st()
                .ledger
                .entries()
                .iter()
                .filter(|e| &e.account == f && e.stopped.is_none())
                .filter_map(|e| e.endpoint.clone())
                .collect();
            // The helper reports a failure for one account inside an `Ok`, and
            // an account missing from its answer was not listed either.
            let listed = self
                .inner
                .helper()
                .and_then(|h| h.assignments(std::slice::from_ref(f), &CancelToken::new()))
                .ok()
                .and_then(|results| results.into_iter().find(|(a, _)| a == f))
                .and_then(|(_, res)| res.ok());
            unmanaged_left_running = listed.map(|assignments| {
                let names = self
                    .inner
                    .state_names(f, &CancelToken::new())
                    .unwrap_or_default();
                assignments
                    .into_iter()
                    .filter_map(|a| {
                        let name = names.get(&a.endpoint).cloned();
                        let managed = tracked.contains(&a.endpoint)
                            || name.as_ref().is_some_and(|n| live.contains(n));
                        (!managed).then(|| RuntimeRef {
                            account: f.clone(),
                            endpoint: a.endpoint,
                            name,
                        })
                    })
                    .collect()
            });
        }
        Ok(SwitchImpact {
            from,
            to: to.clone(),
            tunnels_closing,
            sessions_left_running,
            unmanaged_left_running,
        })
    }

    fn switch_account(&self, to: &AccountId, cancel: &CancelToken) -> Result<AccountsView> {
        if self.inner.st().accounts.get(to).is_none() {
            return Err(Error::new(ErrorKind::AccountNotFound).with("account", to));
        }
        if self.inner.active(cancel)?.as_ref() != Some(to) {
            for t in self.inner.tunnels.list() {
                let _ = self.inner.tunnels.close(t.id);
            }
            let _g = slot_write(&self.inner.slot, SLOT_TIMEOUT)?;
            self.inner
                .helper()?
                .use_account(to, &self.inner.known(), cancel)?;
            let mut st = self.inner.st();
            st.active = Some(to.clone());
            st.active_known = true;
        }
        self.inner.accounts_view(cancel, false)
    }

    fn remove_account(&self, id: &AccountId, cancel: &CancelToken) -> Result<()> {
        if self.inner.active(cancel)?.as_ref() == Some(id) {
            return Err(Error::invalid(
                "account",
                "switch to another account before removing this one",
            ));
        }
        if !self.inner.live_names(id).is_empty() {
            return Err(Error::invalid("account", "stop its sessions first")
                .next(crate::error::NextStep::StopASession));
        }
        let _g = slot_write(&self.inner.slot, SLOT_TIMEOUT)?;
        self.inner
            .helper()?
            .remove(id, &self.inner.known(), cancel)?;
        let mut st = self.inner.st();
        st.accounts.remove(id)?;
        st.usage.remove(id);
        Ok(())
    }

    fn refresh_usage(
        &self,
        accounts: &[AccountId],
        cancel: &CancelToken,
    ) -> Result<Vec<AccountUsageView>> {
        let ids = if accounts.is_empty() {
            self.inner.known()
        } else {
            accounts.to_vec()
        };
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let (verified, _) = self.inner.reconcile_accounts(&ids, cancel);
        match self.inner.helper()?.usage_many(&ids, cancel) {
            Ok(results) => self.inner.record_usage(results, &verified),
            Err(e) => self.inner.record_usage(
                ids.iter().map(|i| (i.clone(), Err(e.clone()))).collect(),
                &verified,
            ),
        }
        Ok(self.inner.usage_views())
    }

    fn usage(&self) -> Vec<AccountUsageView> {
        self.inner.usage_views()
    }

    fn usage_poll_due(&self) -> bool {
        let now = self.inner.now();
        let st = self.inner.st();
        st.accounts.all().iter().any(|a| {
            let has_sessions = st
                .ledger
                .entries()
                .iter()
                .any(|e| e.account == a.id && e.stopped.is_none());
            let last = st.usage.get(&a.id).and_then(|u| u.last_attempt);
            poll_due(last, now, has_sessions, &st.defaults)
        })
    }

    fn runtimes(&self, cancel: &CancelToken) -> Result<RuntimesView> {
        let ids = self.inner.known();
        let mut view = RuntimesView::default();
        if ids.is_empty() {
            return Ok(view);
        }
        let active = self.inner.active(cancel).ok().flatten();
        let sample = self.inner.reconciliation_sample();
        let listed = self.inner.helper()?.assignments(&ids, cancel)?;
        let tunnels = self.inner.tunnels.list();
        for (account, res) in listed {
            let list: Vec<RemoteAssignment> = match res {
                Ok(l) => l,
                Err(error) => {
                    view.errors.push(AccountError { account, error });
                    continue;
                }
            };
            let names = self.inner.state_names(&account, cancel);
            self.inner
                .reconcile(&account, &list, names.as_ref(), &sample);
            let names = names.unwrap_or_default();
            let st = self.inner.st();
            let email = st.accounts.get(&account).and_then(|a| a.email.clone());
            let usage = st.usage.get(&account);
            for a in list {
                let name = names.get(&a.endpoint).cloned().or_else(|| {
                    st.ledger
                        .entries()
                        .iter()
                        .rev()
                        .find(|e| {
                            e.account == account
                                && e.endpoint.as_deref() == Some(&a.endpoint)
                                && e.stopped.is_none()
                        })
                        .map(|e| e.name.clone())
                });
                let tracked = name
                    .as_ref()
                    .and_then(|n| st.ledger.live(&account, n).cloned());
                let rate = name
                    .as_ref()
                    .and_then(|n| usage.and_then(|u| u.attribution.rate_of(n)));
                view.runtimes.push(Runtime {
                    on_active_account: active.as_ref() == Some(&account),
                    account_email: email.clone(),
                    accelerator: a.accelerator,
                    hardware: if a.accelerator_label == "NONE" {
                        "CPU".into()
                    } else {
                        a.accelerator_label.clone()
                    },
                    high_mem: a.high_mem,
                    managed: tracked.is_some(),
                    estimated_rate_cu_per_hour: rate.filter(|_| tracked.is_some()),
                    estimated_spend_cu: tracked.as_ref().and_then(|t| t.spend.spend()),
                    auto_stop_at: tracked.as_ref().and_then(|t| t.auto_stop_at()),
                    tunnels: tunnels
                        .iter()
                        .filter(|t| {
                            t.spec.account == account && Some(&t.spec.session) == name.as_ref()
                        })
                        .map(|t| t.id)
                        .collect(),
                    tracked,
                    id: RuntimeRef {
                        account: account.clone(),
                        endpoint: a.endpoint.clone(),
                        name,
                    },
                });
            }
        }
        view.runtimes.sort_by_key(|r| !r.on_active_account);
        Ok(view)
    }

    fn new_session_name(&self, prefix: Option<&str>) -> String {
        let st = self.inner.st();
        let taken: Vec<String> = st.ledger.entries().iter().map(|e| e.name.clone()).collect();
        let p = prefix
            .map(str::to_owned)
            .unwrap_or_else(|| accelerator_prefix(st.defaults.accelerator));
        generate_session_name(&p, &taken)
    }

    fn provision(
        &self,
        spec: ProvisionSpec,
        events: Events<ProvisionEvent>,
        cancel: &CancelToken,
    ) -> Result<ProvisionOutcome> {
        let (_g, cli, state, account) = self.inner.active_cli(cancel)?;
        let layout = self.inner.layout(cancel)?;
        let limit = {
            let mut st = self.inner.st();
            let mut l = st.defaults.session_limit();
            l.reserved = st.reserved;
            st.reserved += 1;
            l
        };
        struct Release<'a>(&'a Inner);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                let mut st = self.0.st();
                st.reserved = st.reserved.saturating_sub(1);
            }
        }
        let _release = Release(&self.inner);
        let ops = self
            .inner
            .ops(&cli, &account, &state, layout.host_scratch());
        // Set when this call's `Allocated` handler has written the early
        // ledger entry. The failure path below may only close that entry,
        // never a pre-existing one that happens to share the name.
        let registered = std::cell::Cell::new(false);
        let outcome = provision::provision(
            &ops,
            &spec,
            limit,
            self.inner.cfg.clock.as_ref(),
            cancel,
            &|e| {
                // Register the VM as soon as Colab answers, so a host that
                // dies mid-provision still leaves a managed (reconcilable)
                // entry. The event channel is fire-and-forget: never fail
                // provisioning because the ledger could not be written.
                if let ProvisionEvent::Allocated { .. } = &e {
                    let upserted = self.inner.st().ledger.upsert(TrackedSession {
                        name: spec.name.clone(),
                        account: account.clone(),
                        endpoint: None,
                        accelerator: spec.accelerator,
                        high_mem: spec.high_mem,
                        gpu: None,
                        vram_mib: None,
                        policy: spec.guard.clone(),
                        spend: SpendMeter::new(self.inner.now()),
                        watchdog: None,
                        stopped: None,
                        stop_pending_confirmation: None,
                    });
                    if upserted.is_ok() {
                        registered.set(true);
                    }
                }
                let _ = events.send(e);
            },
        );
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => {
                // Close only the entry this call registered, and only when
                // the VM is known to be gone. A failure before `Allocated`
                // (a duplicate name, the session limit) registered nothing,
                // and a same-named entry already in the ledger belongs to a
                // session that is still running. When cleanup could not
                // confirm the release (`OrphanSuspected`) the VM may still
                // be running and billed, so the entry stays live and
                // reconciliation settles whether it actually disappeared.
                if registered.get() && !e.is(ErrorKind::OrphanSuspected) {
                    let _ = self
                        .inner
                        .st()
                        .ledger
                        .update(&account, &spec.name, |entry| {
                            entry.stopped = Some(StopRecord {
                                at: self.inner.now(),
                                reason: StopReason::Provision,
                            });
                        });
                }
                return Err(e);
            }
        };
        let endpoint = if outcome.endpoint.is_empty() {
            // The kept session's endpoint could not be confirmed during
            // provisioning; fall back to the state file's name mapping.
            self.inner
                .state_names(&account, &CancelToken::new())
                .unwrap_or_default()
                .into_iter()
                .find(|(_, n)| n == &outcome.name)
                .map(|(e, _)| e)
        } else {
            Some(outcome.endpoint.clone())
        };
        self.inner.st().ledger.upsert(TrackedSession {
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
        self.inner.stop(target, &options, cancel)
    }

    fn extend_session(
        &self,
        target: &RuntimeRef,
        runtime: Duration,
        cancel: &CancelToken,
    ) -> Result<WatchdogInfo> {
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        let (_g, cli, state) = self.inner.cli_for(&target.account, cancel)?;
        // The VM decides: a running watchdog only gets the new deadline; one
        // that is not running (cancelled, exited, never started) is started
        // the way provisioning starts it.
        let v = cli.exec_json(&state, &name, &[], &watchdog::status_snippet(), 60, cancel)?;
        let mut reply = watchdog::parse_reply(&v, self.inner.now())?;
        if reply.info.armed {
            let env = vec![(
                watchdog::ENV_DEADLINE.to_owned(),
                runtime.as_secs().to_string(),
            )];
            let v = cli.exec_json(&state, &name, &env, &watchdog::extend_snippet(), 60, cancel)?;
            reply = watchdog::parse_reply(&v, self.inner.now())?;
        }
        let info = if reply.info.armed {
            reply.info
        } else {
            let tracked = self
                .inner
                .st()
                .ledger
                .live(&target.account, &name)
                .map(|e| e.policy.clone());
            let policy =
                watchdog::restart_policy(tracked.as_ref(), reply.info.idle_timeout, runtime);
            let started = self.inner.layout(cancel).and_then(|layout| {
                let ops = self
                    .inner
                    .ops(&cli, &target.account, &state, layout.host_scratch());
                provision::start_watchdog(&ops, &name, &policy, &|_| {})
            });
            match started {
                Ok(Some(info)) if info.armed => info,
                _ => watchdog::start_failed(&policy),
            }
        };
        let now = self.inner.now();
        let _ = self.inner.st().ledger.update(&target.account, &name, |e| {
            e.watchdog = Some(info.clone());
            let elapsed = now.duration_since(e.spend.started_at).unwrap_or_default();
            e.policy.max_runtime = Some(elapsed + runtime);
        });
        Ok(info)
    }

    fn cancel_auto_stop(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<()> {
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        let (_g, cli, state) = self.inner.cli_for(&target.account, cancel)?;
        let v = cli.exec_json(&state, &name, &[], &watchdog::cancel_snippet(), 60, cancel)?;
        let reply = watchdog::parse_reply(&v, self.inner.now())?;
        let _ = self.inner.st().ledger.update(&target.account, &name, |e| {
            e.watchdog = Some(reply.info.clone());
            e.policy.max_runtime = None;
        });
        Ok(())
    }

    fn guard_poll(&self, cancel: &CancelToken) -> Result<GuardReport> {
        let mut report = GuardReport::default();
        let live: Vec<TrackedSession> = self
            .inner
            .st()
            .ledger
            .entries()
            .iter()
            .filter(|e| e.stopped.is_none())
            .cloned()
            .collect();
        let ids: Vec<AccountId> = live
            .iter()
            .map(|e| e.account.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if ids.is_empty() {
            report.usage = self.inner.usage_views();
            return Ok(report);
        }
        let helper = self.inner.helper()?;
        let (verified, vanished) = self.inner.reconcile_accounts(&ids, cancel);
        report.vanished = vanished;
        let live: Vec<TrackedSession> = self
            .inner
            .st()
            .ledger
            .entries()
            .iter()
            .filter(|e| e.stopped.is_none() && ids.contains(&e.account))
            .cloned()
            .collect();
        let fresh: BTreeSet<AccountId> = match helper.usage_many(&ids, cancel) {
            Ok(results) => {
                let ok = results
                    .iter()
                    .filter(|(_, r)| r.is_ok())
                    .map(|(i, _)| i.clone())
                    .collect();
                self.inner.record_usage(results, &verified);
                ok
            }
            Err(e) => {
                self.inner.record_usage(
                    ids.iter().map(|i| (i.clone(), Err(e.clone()))).collect(),
                    &verified,
                );
                BTreeSet::new()
            }
        };
        let now = self.inner.now();
        let vanished: BTreeSet<(AccountId, String)> = report
            .vanished
            .iter()
            .filter_map(|r| {
                r.runtime
                    .name
                    .clone()
                    .map(|n| (r.runtime.account.clone(), n))
            })
            .collect();
        for e in live {
            if vanished.contains(&(e.account.clone(), e.name.clone())) {
                continue;
            }
            let (snapshot, rate) = {
                let st = self.inner.st();
                let u = st.usage.get(&e.account);
                (
                    u.and_then(|u| u.snapshot),
                    u.and_then(|u| u.attribution.rate_of(&e.name)),
                )
            };
            let rate = if fresh.contains(&e.account) {
                rate
            } else {
                None
            };
            let mut meter = e.spend.clone();
            meter.record(now, rate);
            let _ = self
                .inner
                .st()
                .ledger
                .update(&e.account, &e.name, |x| x.spend = meter.clone());
            let decision = evaluate(&e.policy, now, snapshot.as_ref(), &meter);
            let target = RuntimeRef {
                account: e.account.clone(),
                endpoint: e.endpoint.clone().unwrap_or_default(),
                name: Some(e.name.clone()),
            };
            let (mut stopped, mut error) = (false, None);
            if let Decision::Stop(concern) = &decision {
                match self.inner.stop(
                    &target,
                    &StopOptions {
                        reason: StopReason::Guard {
                            concern: concern.clone(),
                        },
                    },
                    cancel,
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
        report.usage = self.inner.usage_views();
        Ok(report)
    }

    fn open_tunnel(&self, spec: TunnelSpec, events: Events<TunnelEvent>) -> Result<TunnelId> {
        let cancel = CancelToken::new();
        let (_g, cli, state) = self.inner.cli_for(&spec.account, &cancel)?;
        let ctx = TunnelCtx {
            host: self.inner.cfg.host.clone(),
            colab: cli.colab.clone(),
            base_env: self.inner.cfg.base_env.clone(),
            state_file: state,
            clock: self.inner.cfg.clock.clone(),
        };
        self.inner.tunnels.open(ctx, spec, Some(events))
    }

    fn close_tunnel(&self, id: TunnelId) -> Result<()> {
        self.inner.tunnels.close(id)
    }

    fn tunnels(&self) -> Vec<TunnelInfo> {
        self.inner.tunnels.list()
    }

    fn plugins(&self) -> Vec<PluginInfo> {
        self.inner.st().plugins.list()
    }

    fn plugin_run_template(&self, plugin: &str) -> Result<PluginRunSpec> {
        let st = self.inner.st();
        let info = st
            .plugins
            .list()
            .into_iter()
            .find(|p| p.manifest.id == plugin)
            .ok_or_else(|| Error::new(ErrorKind::PluginNotFound).with("plugin", plugin))?;
        let taken: Vec<String> = st.ledger.entries().iter().map(|e| e.name.clone()).collect();
        Ok(run_template(&info, &st.defaults, &taken))
    }

    fn inspect_plugin_folder(&self, path: &Path) -> Result<PluginPreview> {
        let staging = self.inner.local_layout().staging_dir();
        self.inner.st().plugins.inspect_folder(
            path,
            &staging,
            PluginOrigin::Folder {
                path: path.display().to_string(),
            },
        )
    }

    fn inspect_plugin_git(&self, url: &str, cancel: &CancelToken) -> Result<PluginPreview> {
        let staging = self.inner.local_layout().staging_dir();
        let dir = clone_git(url, &staging, cancel)?;
        let r = self.inner.st().plugins.inspect_folder(
            &dir,
            &staging,
            PluginOrigin::Git {
                url: url.to_owned(),
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    fn install_plugin(&self, preview: &PluginPreview) -> Result<PluginInfo> {
        self.inner.st().plugins.install(preview)
    }

    fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.inner.st().plugins.set_enabled(id, enabled)
    }

    fn remove_plugin(&self, id: &str) -> Result<()> {
        self.inner.st().plugins.remove(id)
    }

    fn run_plugin(
        &self,
        spec: PluginRunSpec,
        events: Events<PluginRunEvent>,
        cancel: &CancelToken,
    ) -> Result<PluginRunOutcome> {
        let (plugin, enabled) = {
            let st = self.inner.st();
            let p = st.plugins.get(&spec.plugin).cloned().ok_or_else(|| {
                Error::new(ErrorKind::PluginNotFound).with("plugin", &spec.plugin)
            })?;
            (p, st.plugins.enabled(&spec.plugin))
        };
        if !enabled {
            return Err(
                Error::invalid("plugin", "the plugin is disabled").with("plugin", &spec.plugin)
            );
        }
        let manifest = &plugin.manifest;
        let resolved = resolve_inputs(manifest, &spec.inputs)?;
        let endpoints = resolve_endpoints(manifest, &resolved.values)?;
        let mut env = resolved.env.clone();
        let mut secrets = resolved.secrets.clone();
        let mut secret_keys: Vec<String> = manifest
            .inputs
            .iter()
            .filter(|i| i.kind == crate::plugins::InputKind::Secret)
            .map(|i| i.env_key())
            .collect();
        let helper = self.inner.helper()?;
        for need in &manifest.needs {
            if let Ok(Some(v)) = helper.secret_get(need.secret_name(), cancel) {
                env.push((need.env_key().to_owned(), v.clone()));
                secret_keys.push(need.env_key().to_owned());
                secrets.push(v);
            }
        }
        // Names only (no values): lets the VM-side poll scrub these values.
        env.push((vm::ENV_SECRET_KEYS.to_owned(), secret_keys.join(",")));
        let account = self
            .inner
            .active(cancel)?
            .ok_or_else(|| Error::new(ErrorKind::NoActiveAccount))?;
        let (session, provisioned) = match &spec.target {
            RunTarget::Existing { name } => (name.clone(), false),
            RunTarget::Provision(p) => {
                let (tx, rx) = std::sync::mpsc::channel();
                let ev = events.clone();
                let fwd = std::thread::spawn(move || {
                    for e in rx {
                        let _ = ev.send(PluginRunEvent::Provision(e));
                    }
                });
                let r = self.provision(p.clone(), tx, cancel);
                let _ = fwd.join();
                (r?.name, true)
            }
        };
        let target = |inner: &Inner| -> RuntimeRef {
            let endpoint = inner
                .st()
                .ledger
                .live(&account, &session)
                .and_then(|e| e.endpoint.clone())
                .unwrap_or_default();
            RuntimeRef {
                account: account.clone(),
                endpoint,
                name: Some(session.clone()),
            }
        };
        let result = self.run_on(
            &plugin, &account, &session, env, &secrets, &endpoints, &events, cancel,
        );
        match &result {
            Err(e) if e.kind == ErrorKind::Cancelled && provisioned => {
                let _ = self.inner.stop(
                    &target(&self.inner),
                    &StopOptions {
                        reason: StopReason::Provision,
                    },
                    &CancelToken::new(),
                );
            }
            Err(e) if e.kind != ErrorKind::Cancelled && provisioned && spec.stop_on_failure => {
                let _ = self.inner.stop(
                    &target(&self.inner),
                    &StopOptions {
                        reason: StopReason::Provision,
                    },
                    &CancelToken::new(),
                );
            }
            _ => {}
        }
        result
    }

    fn set_secret(&self, name: &str, value: &str, cancel: &CancelToken) -> Result<()> {
        self.inner.helper()?.secret_set(name, value, cancel)
    }

    fn has_secret(&self, name: &str, cancel: &CancelToken) -> Result<bool> {
        self.inner.helper()?.secret_has(name, cancel)
    }

    fn delete_secret(&self, name: &str, cancel: &CancelToken) -> Result<()> {
        self.inner.helper()?.secret_delete(name, cancel)
    }

    fn hf_search(&self, query: &str, limit: u32, cancel: &CancelToken) -> Result<Vec<HfModel>> {
        HfClient::new(self.inner.cfg.http.clone(), self.hf_token(cancel)).search(query, limit)
    }

    fn hf_files(&self, repo: &str, cancel: &CancelToken) -> Result<GgufListing> {
        HfClient::new(self.inner.cfg.http.clone(), self.hf_token(cancel)).gguf_files(repo)
    }

    fn check_for_update(&self, cancel: &CancelToken) -> Result<crate::update::UpdateStatus> {
        crate::update::check_this_app(&*self.inner.cfg.http, cancel)
    }

    fn stage_update(
        &self,
        release: &crate::update::Release,
        events: std::sync::mpsc::Sender<(u64, Option<u64>)>,
        cancel: &CancelToken,
    ) -> Result<crate::self_update::StagedUpdate> {
        let mut progress = |written: u64, total: Option<u64>| {
            let _ = events.send((written, total));
        };
        crate::self_update::stage(
            &*self.inner.cfg.http,
            release,
            &self.inner.cfg.local_dir,
            cancel,
            &mut progress,
        )
    }
}

impl RealBackend {
    /// Best effort: removes `secrets` from the CLI's history file for
    /// `session` (the CLI appends every executed cell there, including the
    /// prelude built from `--env`). Never fails a run; callers only warn.
    pub fn redact_cli_history(
        &self,
        session: &str,
        secrets: &[String],
    ) -> Result<crate::accounts::HistoryRedaction> {
        if secrets.is_empty() {
            return Ok(crate::accounts::HistoryRedaction {
                present: false,
                redacted_lines: 0,
            });
        }
        self.inner
            .helper()?
            .history_redact(session, secrets, &CancelToken::new())
    }

    fn hf_token(&self, cancel: &CancelToken) -> Option<String> {
        self.inner
            .helper()
            .ok()?
            .secret_get("hf_token", cancel)
            .ok()
            .flatten()
    }

    /// [`RealBackend::redact_cli_history`], reporting a failure as a warning
    /// line in the run log (never fatal).
    fn scrub_history(&self, session: &str, secrets: &[String], events: &Events<PluginRunEvent>) {
        if let Err(e) = self.redact_cli_history(session, secrets) {
            let _ = events.send(PluginRunEvent::Log {
                lines: vec![format!(
                    "[colab-playground] warning: could not redact secrets from the Colab CLI history: {}",
                    redact(&e.to_string(), secrets)
                )],
            });
        }
    }

    /// Runs the plugin on `session`, then scrubs the CLI history once more,
    /// however the run ended (ready, failed, cancelled or error).
    #[allow(clippy::too_many_arguments)]
    fn run_on(
        &self,
        plugin: &crate::plugins::LoadedPlugin,
        account: &AccountId,
        session: &str,
        env: Vec<(String, String)>,
        secrets: &[String],
        endpoints: &[crate::plugins::ResolvedEndpoint],
        events: &Events<PluginRunEvent>,
        cancel: &CancelToken,
    ) -> Result<PluginRunOutcome> {
        let result = self.run_on_inner(
            plugin, account, session, env, secrets, endpoints, events, cancel,
        );
        if !secrets.is_empty() {
            self.scrub_history(session, secrets, events);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn run_on_inner(
        &self,
        plugin: &crate::plugins::LoadedPlugin,
        account: &AccountId,
        session: &str,
        mut env: Vec<(String, String)>,
        secrets: &[String],
        endpoints: &[crate::plugins::ResolvedEndpoint],
        events: &Events<PluginRunEvent>,
        cancel: &CancelToken,
    ) -> Result<PluginRunOutcome> {
        let (_g, cli, state) = self.inner.cli_for(account, cancel)?;
        let run_id = format!("{}-{}", plugin.manifest.id, &crate::accounts::nonce()[..8]);
        let script_name = plugin
            .manifest
            .run
            .script
            .rsplit('/')
            .next()
            .unwrap_or("script.py")
            .to_owned();
        env.push(("PLUGIN_ID".into(), plugin.manifest.id.clone()));
        env.push(("PLUGIN_RUN_ID".into(), run_id.clone()));
        env.push((vm::ENV_RUN_ID.into(), run_id.clone()));
        env.push((vm::ENV_SCRIPT_NAME.into(), script_name));
        let launched = cli
            .exec_json(
                &state,
                session,
                &env,
                &vm::plugin_launch_snippet(&plugin.script),
                120,
                cancel,
            )
            .map_err(|e| Error {
                detail: redact(&e.detail, secrets),
                ..e
            });
        // The CLI recorded the cell (with its --env prelude) in its local
        // history; scrub the secrets from it right away, success or not.
        self.scrub_history(session, secrets, events);
        launched?;
        let _ = events.send(PluginRunEvent::Launched {
            session: session.to_owned(),
            run_id: run_id.clone(),
        });
        let nc = CancelToken::new();
        let kill = |cli: &Cli| {
            let _ = cli.exec_json(
                &state,
                session,
                &[(vm::ENV_RUN_ID.into(), run_id.clone())],
                &vm::plugin_kill_snippet(),
                60,
                &nc,
            );
        };
        let mut offset = 0u64;
        let mut last: Option<RunStatus> = None;
        let mut transient = 0u32;
        let mut first = true;
        loop {
            let wait = if first {
                Duration::from_secs(3).min(self.inner.cfg.run_poll_interval)
            } else {
                self.inner.cfg.run_poll_interval
            };
            first = false;
            if self.inner.cfg.clock.sleep(wait, cancel).is_err() {
                kill(&cli);
                return Err(Error::cancelled());
            }
            let env = vec![
                (vm::ENV_RUN_ID.into(), run_id.clone()),
                (vm::ENV_LOG_OFFSET.into(), offset.to_string()),
            ];
            let poll = match cli
                .exec_json(&state, session, &env, &vm::plugin_poll_snippet(), 60, &nc)
                .and_then(|v| vm::parse_poll(&v))
            {
                Ok(p) => {
                    transient = 0;
                    p
                }
                Err(e)
                    if matches!(
                        e.kind,
                        ErrorKind::ConnectionLost | ErrorKind::ExecTimeout | ErrorKind::Parse
                    ) && transient < 5 =>
                {
                    transient += 1;
                    continue;
                }
                Err(e) => return Err(e),
            };
            offset = poll.offset;
            // The poll's printed output (log chunk, status) is recorded in the
            // CLI's history too. The VM side normally scrubs it already; if a
            // secret still came through, scrub the history again now.
            let status_text = poll
                .status
                .as_ref()
                .and_then(|s| serde_json::to_string(s).ok())
                .unwrap_or_default();
            if contains_secret(&poll.log, secrets) || contains_secret(&status_text, secrets) {
                self.scrub_history(session, secrets, events);
            }
            if !poll.log.is_empty() {
                let lines: Vec<String> = redact(&poll.log, secrets)
                    .lines()
                    .map(str::to_owned)
                    .collect();
                let _ = events.send(PluginRunEvent::Log { lines });
            }
            if let Some(mut s) = poll.status.clone() {
                if let Some(m) = s.message.as_mut() {
                    *m = redact(m, secrets);
                }
                if let Some(st) = s.stage.as_mut() {
                    *st = redact(st, secrets);
                }
                for v in s.outputs.values_mut() {
                    redact_json(v, secrets);
                }
                if last.as_ref() != Some(&s) {
                    let _ = events.send(PluginRunEvent::Status(s.clone()));
                    last = Some(s.clone());
                }
                match s.state {
                    RunState::Ready => {
                        return Ok(PluginRunOutcome {
                            session: session.to_owned(),
                            account: account.clone(),
                            run_id,
                            status: s,
                            endpoints: endpoints.to_vec(),
                        });
                    }
                    RunState::Failed => {
                        return Err(Error::new(ErrorKind::PluginRunFailed)
                            .with("stage", s.stage.clone().unwrap_or_default())
                            .with("message", s.message.clone().unwrap_or_default()));
                    }
                    _ => {}
                }
            }
            if !poll.alive {
                let stage = last
                    .as_ref()
                    .and_then(|s| s.stage.clone())
                    .unwrap_or_default();
                return Err(Error::new(ErrorKind::PluginRunFailed)
                    .with("stage", stage)
                    .with("message", "the script exited without reporting"));
            }
        }
    }
}
