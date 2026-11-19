//! Session types and the app-side session ledger.
//!
//! The Colab CLI knows which runtimes exist; the ledger remembers what the app
//! itself provisioned (start time, guard policy, probed hardware, watchdog
//! deadline, integrated spend, why it was stopped) so the UI can show uptime,
//! estimated spend and time to auto-stop.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::accounts::AccountId;
use crate::clock::serde_secs;
use crate::error::{Error, Result};
use crate::guard::{Concern, GuardPolicy, SpendMeter};

/// Hardware that can be requested from `colab new`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Accelerator {
    #[default]
    None,
    /// NVIDIA T4 (16 GB).
    T4,
    /// NVIDIA L4 (24 GB).
    L4,
    /// NVIDIA G4 (RTX PRO 6000 class).
    G4,
    /// NVIDIA A100 — Colab assigns 40 GB or 80 GB; it cannot be requested.
    A100,
    /// NVIDIA H100 (80 GB).
    H100,
    /// TPU v5e-1.
    #[serde(rename = "V5E1")]
    TpuV5e1,
    /// TPU v6e-1.
    #[serde(rename = "V6E1")]
    TpuV6e1,
}

impl Accelerator {
    /// Every accelerator, in UI order.
    pub const ALL: [Accelerator; 8] = [
        Accelerator::None,
        Accelerator::T4,
        Accelerator::L4,
        Accelerator::G4,
        Accelerator::A100,
        Accelerator::H100,
        Accelerator::TpuV5e1,
        Accelerator::TpuV6e1,
    ];

    /// The CLI's accelerator value (`NONE`, `A100`, `V5E1`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Accelerator::None => "NONE",
            Accelerator::T4 => "T4",
            Accelerator::L4 => "L4",
            Accelerator::G4 => "G4",
            Accelerator::A100 => "A100",
            Accelerator::H100 => "H100",
            Accelerator::TpuV5e1 => "V5E1",
            Accelerator::TpuV6e1 => "V6E1",
        }
    }

    /// Parses the `Hardware:` label of `colab sessions` (`CPU` means none).
    pub fn from_label(label: &str) -> Option<Accelerator> {
        let l = label.trim().to_ascii_uppercase();
        if l == "CPU" {
            return Some(Accelerator::None);
        }
        Accelerator::ALL.into_iter().find(|a| a.as_str() == l)
    }

    /// Flags for `colab new` (validated locally; never pass unknown values).
    pub fn new_flags(self) -> Vec<String> {
        match self {
            Accelerator::None => vec![],
            Accelerator::TpuV5e1 => vec!["--tpu".into(), "v5e1".into()],
            Accelerator::TpuV6e1 => vec!["--tpu".into(), "v6e1".into()],
            gpu => vec!["--gpu".into(), gpu.as_str().into()],
        }
    }

    pub fn is_gpu(self) -> bool {
        matches!(
            self,
            Accelerator::T4
                | Accelerator::L4
                | Accelerator::G4
                | Accelerator::A100
                | Accelerator::H100
        )
    }

    pub fn is_tpu(self) -> bool {
        matches!(self, Accelerator::TpuV5e1 | Accelerator::TpuV6e1)
    }

    /// False for accelerators with a single machine shape (`--high-mem` is ignored).
    pub fn supports_high_mem(self) -> bool {
        !matches!(
            self,
            Accelerator::L4 | Accelerator::TpuV5e1 | Accelerator::TpuV6e1
        )
    }
}

/// Validates a session name: 1–63 chars of `[A-Za-z0-9._-]`, starting
/// alphanumeric. Names end up in state files and ssh `ProxyCommand`s.
pub fn validate_session_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid("name", format!("invalid session name {name:?}")).with("value", name))
    }
}

/// One line of `colab sessions` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSession {
    /// Local name, `None` for `[?]` (an assignment without local state, e.g.
    /// created in the browser or by another account's state file).
    pub name: Option<String>,
    pub endpoint: String,
    /// `Hardware:` label as printed (`CPU`, `A100`, …).
    pub hardware: String,
    /// Parsed accelerator, when recognised.
    pub accelerator: Option<Accelerator>,
    /// `Shape:` label (`Standard` / `High-RAM`).
    pub shape: String,
    /// `Variant:` label (`DEFAULT` / `GPU` / `TPU`).
    pub variant: String,
    /// `Status:` label when present.
    pub status: Option<String>,
}

impl ServerSession {
    pub fn high_mem(&self) -> bool {
        self.shape.eq_ignore_ascii_case("High-RAM")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    User,
    Guard {
        concern: Concern,
    },
    /// Provisioning rejected or cancelled this runtime.
    Provision,
    /// It disappeared (VM watchdog, Colab idle reclaim, or stopped elsewhere).
    Vanished,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopRecord {
    /// When the app noticed or performed the stop.
    #[serde(with = "serde_secs")]
    pub at: SystemTime,
    pub reason: StopReason,
}

/// What the app knows about the VM-side watchdog of a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchdogInfo {
    /// Absolute deadline (local clock) the watchdog was given, if any. Empty
    /// when a start of the watchdog failed, because nothing on the runtime
    /// enforces a deadline then.
    #[serde(default, with = "serde_secs::option")]
    pub deadline: Option<SystemTime>,
    #[serde(default, with = "crate::clock::serde_dur::option")]
    pub idle_timeout: Option<Duration>,
    #[serde(default)]
    pub cancelled: bool,
    /// The stop is armed on the runtime: the last reply from the VM (start,
    /// extend, cancel or status) showed the watchdog running, not cancelled
    /// and able to release the runtime. Only then is the runtime released at
    /// `deadline` (or when idle) while the app is closed. A watchdog that
    /// exits later still reads as armed until the next reply. `false` when not
    /// confirmed, which includes entries saved before this field existed.
    #[serde(default)]
    pub armed: bool,
}

impl WatchdogInfo {
    /// The deadline the runtime itself enforces: `deadline` while the
    /// watchdog is armed and not cancelled.
    pub fn enforced_deadline(&self) -> Option<SystemTime> {
        self.deadline.filter(|_| self.armed && !self.cancelled)
    }
}

/// A session the app provisioned (persisted in the ledger).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackedSession {
    pub name: String,
    pub account: AccountId,
    /// Server endpoint, once known.
    #[serde(default)]
    pub endpoint: Option<String>,
    pub accelerator: Accelerator,
    pub high_mem: bool,
    #[serde(default)]
    pub gpu: Option<String>,
    /// Probed GPU memory in MiB.
    #[serde(default)]
    pub vram_mib: Option<u32>,
    pub policy: GuardPolicy,
    /// Spend integration (its `started_at` is the session start).
    pub spend: SpendMeter,
    /// VM watchdog state, if one was started.
    #[serde(default)]
    pub watchdog: Option<WatchdogInfo>,
    #[serde(default)]
    pub stopped: Option<StopRecord>,
    /// When set, a stop was requested and the runtime was released, but whether
    /// the endpoint actually disappeared could not yet be confirmed. It holds
    /// the intended stop reason; reconcile finalises it into `stopped` once the
    /// endpoint is confirmed gone (and drops it when the runtime turns out to
    /// be a real orphan).
    #[serde(default)]
    pub stop_pending_confirmation: Option<StopReason>,
}

impl TrackedSession {
    pub fn started_at(&self) -> SystemTime {
        self.spend.started_at
    }

    /// Uptime at `now` (frozen at the stop time once stopped).
    pub fn uptime(&self, now: SystemTime) -> Duration {
        let end = self.stopped.as_ref().map(|s| s.at).unwrap_or(now);
        end.duration_since(self.started_at()).unwrap_or_default()
    }

    /// Earliest of the app-side runtime limit and the deadline the runtime's
    /// watchdog enforces.
    pub fn auto_stop_at(&self) -> Option<SystemTime> {
        let app = self.policy.max_runtime.map(|d| self.started_at() + d);
        let vm = self
            .watchdog
            .as_ref()
            .and_then(WatchdogInfo::enforced_deadline);
        match (app, vm) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// A session as shown in the UI: server view joined with the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub server: ServerSession,
    /// Owning account (the active one for listed sessions).
    pub account: Option<AccountId>,
    pub tracked: Option<TrackedSession>,
}

impl Session {
    /// Display name (`?` for orphans).
    pub fn display_name(&self) -> &str {
        self.server.name.as_deref().unwrap_or("?")
    }
}

/// Persistent list of tracked sessions (`<data>/ledger.json`).
#[derive(Debug)]
pub struct Ledger {
    path: Option<PathBuf>,
    entries: Vec<TrackedSession>,
}

impl Ledger {
    pub fn load(path: &Path) -> Result<Ledger> {
        let entries = match std::fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Vec::new(),
            Ok(s) => serde_json::from_str(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Ledger {
            path: Some(path.to_owned()),
            entries,
        })
    }

    /// A ledger that is never written to disk.
    pub fn in_memory() -> Ledger {
        Ledger {
            path: None,
            entries: Vec::new(),
        }
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> &[TrackedSession] {
        &self.entries
    }

    /// The live (not stopped) entry for `account`/`name`.
    pub fn live(&self, account: &AccountId, name: &str) -> Option<&TrackedSession> {
        self.entries
            .iter()
            .rev()
            .find(|e| &e.account == account && e.name == name && e.stopped.is_none())
    }

    pub fn live_mut(&mut self, account: &AccountId, name: &str) -> Option<&mut TrackedSession> {
        self.entries
            .iter_mut()
            .rev()
            .find(|e| &e.account == account && e.name == name && e.stopped.is_none())
    }

    /// Adds an entry (replacing a live one with the same key).
    pub fn upsert(&mut self, entry: TrackedSession) -> Result<()> {
        if let Some(e) = self.live_mut(&entry.account.clone(), &entry.name.clone()) {
            *e = entry;
        } else {
            self.entries.push(entry);
        }
        self.trim();
        self.save()
    }

    pub fn update(
        &mut self,
        account: &AccountId,
        name: &str,
        f: impl FnOnce(&mut TrackedSession),
    ) -> Result<bool> {
        let found = match self.live_mut(account, name) {
            Some(e) => {
                f(e);
                true
            }
            None => false,
        };
        if found {
            self.save()?;
        }
        Ok(found)
    }

    /// Keeps at most 200 stopped entries.
    fn trim(&mut self) {
        let stopped = self.entries.iter().filter(|e| e.stopped.is_some()).count();
        if stopped > 200 {
            let mut drop = stopped - 200;
            self.entries.retain(|e| {
                if drop > 0 && e.stopped.is_some() {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Writes the ledger (atomic replace).
    pub fn save(&self) -> Result<()> {
        if let Some(path) = &self.path {
            crate::paths::write_atomic(
                path,
                serde_json::to_string_pretty(&self.entries)?.as_bytes(),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accelerator_labels_and_flags() {
        assert_eq!(Accelerator::from_label("CPU"), Some(Accelerator::None));
        assert_eq!(Accelerator::from_label("a100"), Some(Accelerator::A100));
        assert_eq!(Accelerator::from_label("V5E1"), Some(Accelerator::TpuV5e1));
        assert_eq!(Accelerator::from_label("B200"), None);
        assert_eq!(Accelerator::A100.new_flags(), vec!["--gpu", "A100"]);
        assert_eq!(Accelerator::TpuV6e1.new_flags(), vec!["--tpu", "v6e1"]);
        assert!(Accelerator::None.new_flags().is_empty());
        assert!(!Accelerator::L4.supports_high_mem());
        assert_eq!(
            serde_json::to_string(&Accelerator::TpuV5e1).unwrap(),
            "\"V5E1\""
        );
        assert_eq!(
            serde_json::to_string(&Accelerator::A100).unwrap(),
            "\"A100\""
        );
    }

    #[test]
    fn session_names() {
        assert!(validate_session_name("llm-1.a_b").is_ok());
        for bad in ["", "-x", "a b", "a;b", "é", &"x".repeat(64)] {
            assert!(validate_session_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ledger_round_trip_and_auto_stop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut ledger = Ledger::load(&path).unwrap();
        let acct = AccountId::new("a1").unwrap();
        ledger
            .upsert(TrackedSession {
                name: "s".into(),
                account: acct.clone(),
                endpoint: None,
                accelerator: Accelerator::A100,
                high_mem: false,
                gpu: None,
                vram_mib: None,
                policy: GuardPolicy::default(),
                spend: SpendMeter::new(t0),
                watchdog: Some(WatchdogInfo {
                    deadline: Some(t0 + Duration::from_secs(3600)),
                    idle_timeout: None,
                    cancelled: false,
                    armed: true,
                }),
                stopped: None,
                stop_pending_confirmation: None,
            })
            .unwrap();
        let again = Ledger::load(&path).unwrap();
        let e = again.live(&acct, "s").unwrap();
        assert_eq!(e.auto_stop_at(), Some(t0 + Duration::from_secs(3600)));
        assert_eq!(
            e.uptime(t0 + Duration::from_secs(10)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn only_an_armed_watchdog_counts_as_a_stop_on_the_runtime() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let wd = WatchdogInfo {
            deadline: Some(t0 + Duration::from_secs(600)),
            idle_timeout: None,
            cancelled: false,
            armed: true,
        };
        assert_eq!(wd.enforced_deadline(), wd.deadline);
        let dead = WatchdogInfo {
            armed: false,
            ..wd.clone()
        };
        assert_eq!(dead.enforced_deadline(), None);
        let cancelled = WatchdogInfo {
            cancelled: true,
            ..wd.clone()
        };
        assert_eq!(cancelled.enforced_deadline(), None);

        // The earlier watchdog deadline counts only while it is armed; the
        // app-side limit (2 h by default) remains either way.
        let mut s = TrackedSession {
            name: "s".into(),
            account: AccountId::new("a1").unwrap(),
            endpoint: None,
            accelerator: Accelerator::T4,
            high_mem: false,
            gpu: None,
            vram_mib: None,
            policy: GuardPolicy::default(),
            spend: SpendMeter::new(t0),
            watchdog: Some(wd),
            stopped: None,
            stop_pending_confirmation: None,
        };
        assert_eq!(s.auto_stop_at(), Some(t0 + Duration::from_secs(600)));
        s.watchdog = Some(dead);
        assert_eq!(s.auto_stop_at(), Some(t0 + Duration::from_secs(7200)));

        // Ledgers written before `armed` existed load as not armed.
        let old: WatchdogInfo =
            serde_json::from_str(r#"{"deadline": 1000600, "cancelled": false}"#).unwrap();
        assert!(!old.armed);
    }
}
