//! Hardware targeting with retry.
//!
//! Colab picks the concrete machine (an A100 may be 40 GB or 80 GB), so the
//! loop is: `new` → probe hardware via `colab exec` → keep it if it meets the
//! spec, otherwise `stop`, wait, retry. The loop is written against
//! [`ProvisionOps`] so the real backend and the mock share it exactly.
//!
//! Safety properties:
//!
//! * A cancelled `colab new` is killed: Colab may never answer it, and the
//!   CLI then waits about ten minutes before giving up. The request may still
//!   allocate a VM (recorded by name if the CLI got the reply just before the
//!   kill, or a `[?]` assignment otherwise), so the account is checked right
//!   away and again after [`KILL_SETTLE`], and whatever appeared is stopped or
//!   reaped. A cancellation that lands after `new` returned stops its VM.
//! * After a failed `new`, any VM it allocated anyway is stopped (by name) or
//!   reaped (by endpoint, for `[?]` assignments that appeared meanwhile). A
//!   `new` that timed out or got no answer gets the same two looks as a kill.
//! * Every stop is confirmed with `colab sessions`; an unverifiable release is
//!   reported as [`ErrorKind::OrphanSuspected`] instead of being ignored.
//! * `NotEntitled` (and auth/installation problems) abort immediately.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::cli::StopOutcome;
use crate::clock::{CancelToken, Clock, serde_dur};
use crate::error::{Error, ErrorKind, Result};
use crate::guard::GuardPolicy;
use crate::sessions::{Accelerator, ServerSession, WatchdogInfo, validate_session_name};

/// Hard cap on attempts (a safety limit, not a user default).
pub const MAX_ATTEMPTS_CAP: u32 = 8;

pub const WATCHDOG_ATTEMPTS: u32 = 3;

/// How long provisioning waits after killing `colab new` before it checks the
/// account a second time for a VM that request allocated anyway. An answered
/// request takes about 3 s; the CLI itself waits 10 s before re-checking a
/// request that got no answer.
pub const KILL_SETTLE: Duration = Duration::from_secs(10);

/// Caps how many sessions the app keeps on one account, so a retry loop can
/// never fan out into many billed VMs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLimit {
    /// `max_concurrent_sessions` (user-editable, default 2).
    pub max_concurrent: u32,
    /// Slots held by other provisioning runs of this app that have not
    /// created their session yet.
    pub reserved: u32,
}

impl Default for SessionLimit {
    fn default() -> Self {
        crate::defaults::Defaults::default().session_limit()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExhausted {
    #[default]
    StopAndFail,
    /// Keep the last VM even though it is below spec.
    KeepLast,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvisionSpec {
    /// Session name (must not exist yet).
    pub name: String,
    pub accelerator: Accelerator,
    /// Request the high-RAM shape.
    pub high_mem: bool,
    /// Minimum GPU memory, e.g. `Some(70_000)` to insist on an 80 GB A100.
    pub min_vram_mib: Option<u32>,
    /// Attempts (pre-filled from `Defaults`, clamped to 1..=8).
    pub max_attempts: u32,
    /// Wait between attempts (pre-filled from `Defaults`).
    #[serde(with = "serde_dur")]
    pub retry_delay: Duration,
    pub on_exhausted: OnExhausted,
    /// Cost limits; also configures the VM watchdog.
    pub guard: GuardPolicy,
}

impl ProvisionSpec {
    /// A spec for `name` on `accelerator`, everything else from `defaults`.
    pub fn new(
        name: impl Into<String>,
        accelerator: Accelerator,
        defaults: &crate::defaults::Defaults,
    ) -> Self {
        ProvisionSpec {
            accelerator,
            ..defaults.provision_spec(name)
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_session_name(&self.name)?;
        if self.min_vram_mib.is_some_and(|m| m > 0) && !self.accelerator.is_gpu() {
            return Err(Error::invalid(
                "min_vram_mib",
                "a VRAM minimum needs a GPU accelerator",
            ));
        }
        Ok(())
    }

    /// `max_attempts` clamped to `1..=MAX_ATTEMPTS_CAP`.
    pub fn attempts(&self) -> u32 {
        self.max_attempts.clamp(1, MAX_ATTEMPTS_CAP)
    }

    fn min_vram(&self) -> Option<u32> {
        self.min_vram_mib.filter(|m| *m > 0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    /// GPU name from `nvidia-smi` (`None` on CPU/TPU or if probing failed).
    pub gpu: Option<String>,
    /// Total GPU memory in MiB.
    pub vram_mib: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProvisionEvent {
    /// `colab new` is running.
    Allocating {
        /// 1-based attempt.
        attempt: u32,
        max_attempts: u32,
    },
    /// Until this (or `AllocationFailed`) arrives, Colab has not answered the
    /// request; the hardware probe comes after it.
    Allocated {
        /// Attempt.
        attempt: u32,
    },
    /// `colab new` failed (retryable unless followed by `Failed`).
    AllocationFailed {
        attempt: u32,
        error: Error,
    },
    Probed {
        attempt: u32,
        gpu: Option<String>,
        vram_mib: Option<u32>,
    },
    Rejected {
        attempt: u32,
        gpu: Option<String>,
        vram_mib: Option<u32>,
        min_vram_mib: u32,
    },
    /// A VM is being stopped (rejected, cancelled, or cleanup).
    Releasing {
        attempt: u32,
    },
    /// `colab new` was killed by a cancel: checking whether the request
    /// allocated a VM anyway (any found is reported as `OrphanDetected`).
    Reconciling {
        /// Attempt.
        attempt: u32,
    },
    /// An unnamed assignment appeared after a failed `new`.
    OrphanDetected {
        endpoint: String,
        released: bool,
    },
    Retrying {
        /// The upcoming attempt.
        attempt: u32,
        /// Delay in seconds.
        delay_secs: u64,
    },
    WatchdogStarted {
        /// Its deadline (local clock), if any.
        #[serde(with = "crate::clock::serde_secs::option")]
        deadline: Option<SystemTime>,
    },
    /// The VM watchdog could not be started; the session is only protected
    /// by the app-side guard while the app runs.
    WatchdogFailed {
        error: Error,
    },
    Ready {
        name: String,
        gpu: Option<String>,
        vram_mib: Option<u32>,
        /// Kept although below spec (`OnExhausted::KeepLast`).
        below_spec: bool,
    },
    Failed {
        error: Error,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvisionOutcome {
    pub name: String,
    /// Server endpoint of the kept runtime, confirmed from the assignments
    /// listing (empty when it could not be confirmed).
    pub endpoint: String,
    pub hardware: Hardware,
    pub attempts: u32,
    /// Kept although below spec.
    pub below_spec: bool,
    /// Watchdog state (`None` if not started or failed).
    pub watchdog: Option<WatchdogInfo>,
    /// When the kept VM was allocated.
    #[serde(with = "crate::clock::serde_secs")]
    pub started_at: SystemTime,
}

/// The side effects [`provision`] needs.
pub trait ProvisionOps {
    /// `colab sessions` for the active account.
    fn sessions(&self, cancel: &CancelToken) -> Result<Vec<ServerSession>>;
    /// `colab new`.
    fn create(
        &self,
        name: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<()>;
    /// Probe GPU name and memory with `colab exec`.
    fn probe(&self, name: &str, cancel: &CancelToken) -> Result<Hardware>;
    /// `colab stop`.
    fn stop(&self, name: &str, cancel: &CancelToken) -> Result<StopOutcome>;
    /// Release an unnamed assignment by endpoint; `Ok(true)` if released.
    fn reap(&self, endpoint: &str, cancel: &CancelToken) -> Result<bool>;
    fn start_watchdog(
        &self,
        name: &str,
        policy: &GuardPolicy,
        cancel: &CancelToken,
    ) -> Result<Option<WatchdogInfo>>;
}

fn aborts_immediately(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::NotEntitled
            // The CLI already retried for about ten minutes.
            | ErrorKind::AllocationTimeout
            | ErrorKind::AuthRequired
            | ErrorKind::KeyringUnavailable
            | ErrorKind::NotInstalled
            | ErrorKind::InvalidInput
            | ErrorKind::OrphanSuspected
            | ErrorKind::WslUnavailable
            | ErrorKind::Unsupported
    )
}

/// Starts the VM watchdog of `name` for `policy`, up to [`WATCHDOG_ATTEMPTS`]
/// times; returns the last error when every attempt failed. Provisioning
/// starts it this way, and so does setting an auto-stop on a runtime whose
/// watchdog is not running. Not cancellable, so the caller always gets the
/// start's own reply, the only word on whether the runtime has its stop.
pub fn start_watchdog(
    ops: &dyn ProvisionOps,
    name: &str,
    policy: &GuardPolicy,
) -> Result<Option<WatchdogInfo>> {
    let nc = CancelToken::new();
    let mut last = Error::new(ErrorKind::Other);
    for _ in 0..WATCHDOG_ATTEMPTS {
        match ops.start_watchdog(name, policy, &nc) {
            Ok(w) => return Ok(w),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Stops `name` and confirms it is gone from `colab sessions`.
pub fn release(ops: &dyn ProvisionOps, name: &str) -> Result<()> {
    let nc = CancelToken::new();
    let mut last_err = None;
    for _ in 0..2 {
        if let Err(e) = ops.stop(name, &nc) {
            if e.kind == ErrorKind::SessionGone {
                // Already gone locally; still verify on the server below.
            } else {
                last_err = Some(e);
            }
        }
        match ops.sessions(&nc) {
            Ok(list) if !list.iter().any(|s| s.name.as_deref() == Some(name)) => return Ok(()),
            Ok(_) => {}
            Err(e) => last_err = Some(e),
        }
    }
    Err(Error::new(ErrorKind::OrphanSuspected)
        .with("name", name)
        .detail(
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "still listed after stop".into()),
        ))
}

/// The endpoint of the kept session: the assignment that appeared since
/// `before`, preferring one named `name`. The listing reports an assignment
/// whose state-file record is missing as `[?]` (unnamed), so the name is a
/// hint, not a requirement — the diff alone confirms which endpoint the just
/// answered `colab new` allocated.
fn kept_endpoint(ops: &dyn ProvisionOps, name: &str, before: &BTreeSet<String>) -> String {
    let nc = CancelToken::new();
    let Ok(list) = ops.sessions(&nc) else {
        return String::new();
    };
    let created: Vec<&ServerSession> = list
        .iter()
        .filter(|s| !before.contains(&s.endpoint))
        .collect();
    created
        .iter()
        .find(|s| s.name.as_deref() == Some(name))
        .copied()
        .or_else(|| (created.len() == 1).then_some(created[0]))
        .map(|s| s.endpoint.clone())
        .unwrap_or_default()
}

/// Cleans up after a failed `colab new`: stops a VM recorded under `name` and
/// reaps `[?]` assignments that were not present before.
fn cleanup_failed_create(
    ops: &dyn ProvisionOps,
    name: &str,
    before: &BTreeSet<String>,
    events: &dyn Fn(ProvisionEvent),
) -> Result<()> {
    let nc = CancelToken::new();
    let list = ops.sessions(&nc).map_err(|e| {
        Error::new(ErrorKind::OrphanSuspected)
            .detail(format!("could not verify after a failed `colab new`: {e}"))
    })?;
    if list.iter().any(|s| s.name.as_deref() == Some(name)) {
        release(ops, name)?;
    }
    for s in list
        .iter()
        .filter(|s| s.name.is_none() && !before.contains(&s.endpoint))
    {
        let released = ops.reap(&s.endpoint, &nc).unwrap_or(false);
        events(ProvisionEvent::OrphanDetected {
            endpoint: s.endpoint.clone(),
            released,
        });
        if !released {
            return Err(Error::new(ErrorKind::OrphanSuspected).with("endpoint", &s.endpoint));
        }
    }
    Ok(())
}

/// Cleans up after a `colab new` whose request was abandoned without an answer
/// (killed by a cancel or at its time limit, or given up by the CLI): the
/// request may still allocate a VM after the first look, so the account is
/// checked again after [`KILL_SETTLE`]. Neither the wait nor the checks can be
/// cancelled.
fn cleanup_killed_create(
    ops: &dyn ProvisionOps,
    name: &str,
    before: &BTreeSet<String>,
    clock: &dyn Clock,
    events: &dyn Fn(ProvisionEvent),
) -> Result<()> {
    cleanup_failed_create(ops, name, before, events)?;
    let _ = clock.sleep(KILL_SETTLE, &CancelToken::new());
    cleanup_failed_create(ops, name, before, events)
}

/// Runs the provisioning loop. Emits every step through `events`; also emits
/// `Failed` before returning an error (except for cancellation).
///
/// Named sessions already on the account plus `limit.reserved` must stay
/// below `limit.max_concurrent`, otherwise `SessionLimitReached` is returned
/// before anything is created. (`[?]` runtimes the app did not create do not
/// count; a rejected VM is released before the next attempt.)
pub fn provision(
    ops: &dyn ProvisionOps,
    spec: &ProvisionSpec,
    limit: SessionLimit,
    clock: &dyn Clock,
    cancel: &CancelToken,
    events: &dyn Fn(ProvisionEvent),
) -> Result<ProvisionOutcome> {
    let result = run(ops, spec, limit, clock, cancel, events);
    if let Err(e) = &result
        && e.kind != ErrorKind::Cancelled
    {
        events(ProvisionEvent::Failed { error: e.clone() });
    }
    result
}

fn run(
    ops: &dyn ProvisionOps,
    spec: &ProvisionSpec,
    limit: SessionLimit,
    clock: &dyn Clock,
    cancel: &CancelToken,
    events: &dyn Fn(ProvisionEvent),
) -> Result<ProvisionOutcome> {
    spec.validate()?;
    let max = spec.attempts();
    let name = spec.name.as_str();
    let nc = CancelToken::new();
    cancel.check()?;
    let existing = ops.sessions(cancel)?;
    if existing.iter().any(|s| s.name.as_deref() == Some(name)) {
        return Err(
            Error::invalid("name", "a session with this name already exists").with("value", name),
        );
    }
    let named = existing.iter().filter(|s| s.name.is_some()).count() as u32;
    if named + limit.reserved >= limit.max_concurrent.max(1) {
        return Err(Error::new(ErrorKind::SessionLimitReached).with("limit", limit.max_concurrent));
    }
    let mut before: BTreeSet<String> = existing.into_iter().map(|s| s.endpoint).collect();

    for attempt in 1..=max {
        cancel.check()?;
        events(ProvisionEvent::Allocating {
            attempt,
            max_attempts: max,
        });
        if let Err(e) = ops.create(name, spec.accelerator, spec.high_mem, cancel) {
            if e.kind == ErrorKind::Cancelled {
                events(ProvisionEvent::Reconciling { attempt });
                cleanup_killed_create(ops, name, &before, clock, events)?;
                return Err(e);
            }
            events(ProvisionEvent::AllocationFailed {
                attempt,
                error: e.clone(),
            });
            if matches!(e.kind, ErrorKind::Timeout | ErrorKind::AllocationTimeout) {
                // Killed at the CLI call's own time limit, or the CLI gave up on its last
                // request without an answer: either may still get a VM after the first look.
                cleanup_killed_create(ops, name, &before, clock, events)?;
            } else {
                cleanup_failed_create(ops, name, &before, events)?;
            }
            if aborts_immediately(e.kind) || attempt == max {
                return Err(e);
            }
            if cancel.is_cancelled() {
                return Err(Error::cancelled());
            }
            events(ProvisionEvent::Retrying {
                attempt: attempt + 1,
                delay_secs: spec.retry_delay.as_secs(),
            });
            clock.sleep(spec.retry_delay, cancel)?;
            continue;
        }
        events(ProvisionEvent::Allocated { attempt });
        let started_at = clock.now();
        if cancel.is_cancelled() {
            events(ProvisionEvent::Releasing { attempt });
            release(ops, name)?;
            return Err(Error::cancelled());
        }

        let hardware = match ops.probe(name, cancel) {
            Ok(hw) => hw,
            Err(e) if e.kind == ErrorKind::Cancelled => {
                events(ProvisionEvent::Releasing { attempt });
                release(ops, name)?;
                return Err(e);
            }
            Err(e) if e.kind == ErrorKind::SessionGone => {
                events(ProvisionEvent::AllocationFailed {
                    attempt,
                    error: e.clone(),
                });
                let _ = release(ops, name);
                if attempt == max {
                    return Err(e);
                }
                events(ProvisionEvent::Retrying {
                    attempt: attempt + 1,
                    delay_secs: spec.retry_delay.as_secs(),
                });
                clock.sleep(spec.retry_delay, cancel)?;
                continue;
            }
            // Unknown hardware: acceptable only without a VRAM requirement.
            Err(_) => Hardware::default(),
        };
        events(ProvisionEvent::Probed {
            attempt,
            gpu: hardware.gpu.clone(),
            vram_mib: hardware.vram_mib,
        });

        let meets = match spec.min_vram() {
            None => true,
            Some(min) => hardware.vram_mib.is_some_and(|v| v >= min),
        };
        let below_spec = !meets;
        if !meets {
            let min = spec.min_vram().unwrap_or(0);
            events(ProvisionEvent::Rejected {
                attempt,
                gpu: hardware.gpu.clone(),
                vram_mib: hardware.vram_mib,
                min_vram_mib: min,
            });
            let keep = attempt == max && spec.on_exhausted == OnExhausted::KeepLast;
            if !keep {
                events(ProvisionEvent::Releasing { attempt });
                release(ops, name)?;
                if attempt == max {
                    return Err(Error::new(ErrorKind::ProvisionExhausted)
                        .with("attempts", max)
                        .with("gpu", hardware.gpu.clone().unwrap_or_default())
                        .with(
                            "vram_mib",
                            hardware.vram_mib.map(|v| v.to_string()).unwrap_or_default(),
                        ));
                }
                // Assignments listed now belong to us or pre-existed; only new
                // `[?]` entries after the next failed `new` count as orphans.
                if let Ok(list) = ops.sessions(&nc) {
                    before = list.into_iter().map(|s| s.endpoint).collect();
                }
                events(ProvisionEvent::Retrying {
                    attempt: attempt + 1,
                    delay_secs: spec.retry_delay.as_secs(),
                });
                clock.sleep(spec.retry_delay, cancel)?;
                continue;
            }
        }

        if cancel.is_cancelled() {
            events(ProvisionEvent::Releasing { attempt });
            release(ops, name)?;
            return Err(Error::cancelled());
        }
        let watchdog = if spec.guard.needs_watchdog() {
            match start_watchdog(ops, name, &spec.guard) {
                Ok(w) => {
                    events(ProvisionEvent::WatchdogStarted {
                        deadline: w.as_ref().and_then(|w| w.deadline),
                    });
                    w
                }
                Err(e) => {
                    events(ProvisionEvent::WatchdogFailed { error: e });
                    None
                }
            }
        } else {
            None
        };
        if cancel.is_cancelled() {
            events(ProvisionEvent::Releasing { attempt });
            release(ops, name)?;
            return Err(Error::cancelled());
        }
        let endpoint = kept_endpoint(ops, name, &before);
        events(ProvisionEvent::Ready {
            name: name.to_owned(),
            gpu: hardware.gpu.clone(),
            vram_mib: hardware.vram_mib,
            below_spec,
        });
        return Ok(ProvisionOutcome {
            name: name.to_owned(),
            endpoint,
            hardware,
            attempts: attempt,
            below_spec,
            watchdog,
            started_at,
        });
    }
    unreachable!("the loop returns on its last attempt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Scripted fake: each `create` pops a behaviour.
    #[derive(Default)]
    struct Fake {
        creates: RefCell<VecDeque<std::result::Result<u32, ErrorKind>>>,
        orphan_on_fail: bool,
        live: RefCell<Vec<ServerSession>>,
        log: RefCell<Vec<String>>,
        stop_fails: bool,
        cancel_on_probe: Option<CancelToken>,
        /// The user cancels while `colab new` waits for Colab, and the host kills it.
        cancel_on_create: Option<CancelToken>,
        /// The Nth `sessions` call (1-based) is the first to list an unnamed VM that a
        /// killed `colab new` got after all.
        orphan_on_listing: Option<usize>,
        vram: RefCell<Option<u32>>,
        /// The next N watchdog starts fail.
        watchdog_failures: RefCell<u32>,
    }

    fn sess(name: Option<&str>, ep: &str) -> ServerSession {
        ServerSession {
            name: name.map(str::to_owned),
            endpoint: ep.into(),
            hardware: "A100".into(),
            accelerator: Some(Accelerator::A100),
            shape: "Standard".into(),
            variant: "GPU".into(),
            status: None,
        }
    }

    impl ProvisionOps for Fake {
        fn sessions(&self, _: &CancelToken) -> Result<Vec<ServerSession>> {
            self.log.borrow_mut().push("sessions".into());
            let listings = self
                .log
                .borrow()
                .iter()
                .filter(|l| *l == "sessions")
                .count();
            if self.orphan_on_listing == Some(listings) {
                self.live.borrow_mut().push(sess(None, "late-orphan"));
            }
            Ok(self.live.borrow().clone())
        }
        fn create(&self, name: &str, _: Accelerator, _: bool, cancel: &CancelToken) -> Result<()> {
            self.log.borrow_mut().push("create".into());
            if let Some(c) = &self.cancel_on_create {
                c.cancel();
                assert!(cancel.is_cancelled(), "`colab new` runs under the cancel");
                if self.orphan_on_fail {
                    self.live.borrow_mut().push(sess(None, "orphan-1"));
                }
                return Err(Error::cancelled());
            }
            match self
                .creates
                .borrow_mut()
                .pop_front()
                .expect("unexpected create")
            {
                Ok(vram) => {
                    *self.vram.borrow_mut() = Some(vram);
                    self.live
                        .borrow_mut()
                        .push(sess(Some(name), &format!("ep-{}", self.log.borrow().len())));
                    Ok(())
                }
                Err(kind) => {
                    if self.orphan_on_fail {
                        self.live.borrow_mut().push(sess(None, "orphan-1"));
                    }
                    Err(Error::new(kind))
                }
            }
        }
        fn probe(&self, _: &str, _: &CancelToken) -> Result<Hardware> {
            self.log.borrow_mut().push("probe".into());
            if let Some(c) = &self.cancel_on_probe {
                c.cancel();
            }
            Ok(Hardware {
                gpu: Some("NVIDIA A100-SXM4".into()),
                vram_mib: *self.vram.borrow(),
            })
        }
        fn stop(&self, name: &str, _: &CancelToken) -> Result<StopOutcome> {
            self.log.borrow_mut().push(format!("stop {name}"));
            if !self.stop_fails {
                self.live
                    .borrow_mut()
                    .retain(|s| s.name.as_deref() != Some(name));
            }
            Ok(StopOutcome::Terminated)
        }
        fn reap(&self, endpoint: &str, _: &CancelToken) -> Result<bool> {
            self.log.borrow_mut().push(format!("reap {endpoint}"));
            self.live.borrow_mut().retain(|s| s.endpoint != endpoint);
            Ok(true)
        }
        fn start_watchdog(
            &self,
            _: &str,
            _: &GuardPolicy,
            _: &CancelToken,
        ) -> Result<Option<WatchdogInfo>> {
            self.log.borrow_mut().push("watchdog".into());
            let mut failures = self.watchdog_failures.borrow_mut();
            if *failures > 0 {
                *failures -= 1;
                return Err(Error::new(ErrorKind::Other).detail("not running"));
            }
            Ok(Some(WatchdogInfo {
                deadline: None,
                idle_timeout: None,
                cancelled: false,
                armed: true,
            }))
        }
    }

    fn spec(min: Option<u32>, attempts: u32, on: OnExhausted) -> ProvisionSpec {
        ProvisionSpec {
            min_vram_mib: min,
            max_attempts: attempts,
            on_exhausted: on,
            ..ProvisionSpec::new(
                "llm",
                Accelerator::A100,
                &crate::defaults::Defaults::default(),
            )
        }
    }

    fn run_fake(
        f: &Fake,
        s: &ProvisionSpec,
        cancel: &CancelToken,
    ) -> (Result<ProvisionOutcome>, Vec<ProvisionEvent>, ManualClock) {
        let clock = ManualClock::fixed();
        let events = RefCell::new(Vec::new());
        let r = provision(f, s, SessionLimit::default(), &clock, cancel, &|e| {
            events.borrow_mut().push(e)
        });
        (r, events.into_inner(), clock)
    }

    #[test]
    fn retries_from_40_to_80_gb() {
        let f = Fake {
            creates: RefCell::new(VecDeque::from([Ok(40_960), Ok(81_920)])),
            ..Default::default()
        };
        let (r, ev, clock) = run_fake(
            &f,
            &spec(Some(70_000), 3, OnExhausted::StopAndFail),
            &CancelToken::new(),
        );
        let out = r.unwrap();
        assert_eq!(out.attempts, 2);
        assert_eq!(out.hardware.vram_mib, Some(81_920));
        assert!(!out.below_spec);
        // Each answered `colab new` is reported before its hardware is probed.
        let steps: Vec<&str> = ev
            .iter()
            .filter_map(|e| match e {
                ProvisionEvent::Allocating { .. } => Some("allocating"),
                ProvisionEvent::Allocated { .. } => Some("allocated"),
                ProvisionEvent::Probed { .. } => Some("probed"),
                _ => None,
            })
            .collect();
        assert_eq!(
            steps,
            [
                "allocating",
                "allocated",
                "probed",
                "allocating",
                "allocated",
                "probed"
            ]
        );
        assert!(
            ev.iter()
                .any(|e| matches!(e, ProvisionEvent::Rejected { attempt: 1, .. }))
        );
        assert!(ev.iter().any(|e| matches!(
            e,
            ProvisionEvent::Retrying {
                attempt: 2,
                delay_secs: 20
            }
        )));
        assert!(matches!(ev.last(), Some(ProvisionEvent::Ready { .. })));
        assert_eq!(f.live.borrow().len(), 1, "the rejected VM was stopped");
        assert!(
            clock.now() > ManualClock::fixed().now(),
            "the retry delay used the injected clock"
        );
        assert!(f.log.borrow().contains(&"watchdog".to_owned()));
    }

    #[test]
    fn the_watchdog_start_is_retried_then_reported() {
        let watchdogs = |f: &Fake| f.log.borrow().iter().filter(|l| *l == "watchdog").count();
        let f = Fake {
            watchdog_failures: RefCell::new(2),
            ..Default::default()
        };
        let policy = GuardPolicy::default();
        let w = start_watchdog(&f, "s", &policy).unwrap();
        assert!(w.is_some_and(|w| w.armed));
        assert_eq!(watchdogs(&f), 3);

        let f = Fake {
            watchdog_failures: RefCell::new(WATCHDOG_ATTEMPTS),
            ..Default::default()
        };
        let err = start_watchdog(&f, "s", &policy).unwrap_err();
        assert_eq!(err.detail, "not running");
        assert_eq!(watchdogs(&f), WATCHDOG_ATTEMPTS as usize);

        // Provisioning keeps the session and reports that the stop is not on it.
        let f = Fake {
            creates: RefCell::new(VecDeque::from([Ok(15_360)])),
            watchdog_failures: RefCell::new(WATCHDOG_ATTEMPTS),
            ..Default::default()
        };
        let (r, ev, _) = run_fake(
            &f,
            &spec(None, 1, OnExhausted::StopAndFail),
            &CancelToken::new(),
        );
        assert_eq!(r.unwrap().watchdog, None);
        assert!(
            ev.iter()
                .any(|e| matches!(e, ProvisionEvent::WatchdogFailed { .. }))
        );
    }

    #[test]
    fn keep_last_returns_below_spec() {
        let f = Fake {
            creates: RefCell::new(VecDeque::from([Ok(40_960), Ok(40_960)])),
            ..Default::default()
        };
        let (r, _, _) = run_fake(
            &f,
            &spec(Some(70_000), 2, OnExhausted::KeepLast),
            &CancelToken::new(),
        );
        let out = r.unwrap();
        assert!(out.below_spec);
        assert_eq!(f.live.borrow().len(), 1);
    }

    #[test]
    fn stop_and_fail_leaves_nothing() {
        let f = Fake {
            creates: RefCell::new(VecDeque::from([Ok(40_960), Ok(40_960)])),
            ..Default::default()
        };
        let (r, ev, _) = run_fake(
            &f,
            &spec(Some(70_000), 2, OnExhausted::StopAndFail),
            &CancelToken::new(),
        );
        assert!(r.unwrap_err().is(ErrorKind::ProvisionExhausted));
        assert!(f.live.borrow().is_empty());
        assert!(matches!(ev.last(), Some(ProvisionEvent::Failed { .. })));
    }

    #[test]
    fn not_entitled_and_unanswered_requests_abort_without_retry() {
        for kind in [ErrorKind::NotEntitled, ErrorKind::AllocationTimeout] {
            let f = Fake {
                creates: RefCell::new(VecDeque::from([Err(kind)])),
                ..Default::default()
            };
            let (r, _, _) = run_fake(
                &f,
                &spec(None, 3, OnExhausted::StopAndFail),
                &CancelToken::new(),
            );
            assert!(r.unwrap_err().is(kind));
            assert_eq!(
                f.log.borrow().iter().filter(|l| *l == "create").count(),
                1,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn cancelling_during_create_kills_it_and_reaps_what_it_left() {
        let cancel = CancelToken::new();
        let f = Fake {
            cancel_on_create: Some(cancel.clone()),
            orphan_on_fail: true,
            ..Default::default()
        };
        let (r, ev, _) = run_fake(&f, &spec(None, 3, OnExhausted::StopAndFail), &cancel);
        assert!(r.unwrap_err().is(ErrorKind::Cancelled));
        assert!(f.live.borrow().is_empty(), "the cancel left a VM behind");
        assert!(f.log.borrow().contains(&"reap orphan-1".to_owned()));
        assert!(
            ev.iter()
                .any(|e| matches!(e, ProvisionEvent::Reconciling { attempt: 1 }))
        );
        assert!(
            ev.iter()
                .any(|e| matches!(e, ProvisionEvent::OrphanDetected { released: true, .. }))
        );
        assert!(!ev.iter().any(|e| matches!(
            e,
            ProvisionEvent::Failed { .. }
                | ProvisionEvent::AllocationFailed { .. }
                | ProvisionEvent::Retrying { .. }
        )));
        assert_eq!(f.log.borrow().iter().filter(|l| *l == "create").count(), 1);
    }

    #[test]
    fn a_vm_that_shows_up_after_the_kill_is_reaped_on_the_second_look() {
        let cancel = CancelToken::new();
        // Listing 1 is the check before creating, 2 the first look after the kill.
        let f = Fake {
            cancel_on_create: Some(cancel.clone()),
            orphan_on_listing: Some(3),
            ..Default::default()
        };
        let (r, ev, clock) = run_fake(&f, &spec(None, 3, OnExhausted::StopAndFail), &cancel);
        assert!(r.unwrap_err().is(ErrorKind::Cancelled));
        assert!(f.log.borrow().contains(&"reap late-orphan".to_owned()));
        assert!(f.live.borrow().is_empty(), "the late VM was left billing");
        assert!(
            ev.iter()
                .any(|e| matches!(e, ProvisionEvent::OrphanDetected { released: true, .. }))
        );
        assert_eq!(
            clock.now(),
            ManualClock::fixed().now() + KILL_SETTLE,
            "the second look waited on the injected clock"
        );
    }

    #[test]
    fn an_unanswered_or_timed_out_create_also_gets_a_second_look() {
        for kind in [ErrorKind::Timeout, ErrorKind::AllocationTimeout] {
            let f = Fake {
                creates: RefCell::new(VecDeque::from([Err(kind)])),
                orphan_on_listing: Some(3),
                ..Default::default()
            };
            let (r, _, _) = run_fake(
                &f,
                &spec(None, 1, OnExhausted::StopAndFail),
                &CancelToken::new(),
            );
            assert!(r.unwrap_err().is(kind));
            assert!(
                f.log.borrow().contains(&"reap late-orphan".to_owned()),
                "{kind:?}"
            );
            assert!(f.live.borrow().is_empty(), "{kind:?} left a VM billing");
        }
    }

    #[test]
    fn failed_create_reaps_new_orphans_then_retries() {
        let f = Fake {
            creates: RefCell::new(VecDeque::from([
                Err(ErrorKind::CapacityRefused),
                Ok(15_360),
            ])),
            orphan_on_fail: true,
            ..Default::default()
        };
        let (r, ev, _) = run_fake(
            &f,
            &spec(None, 3, OnExhausted::StopAndFail),
            &CancelToken::new(),
        );
        assert!(r.is_ok());
        assert!(f.log.borrow().contains(&"reap orphan-1".to_owned()));
        assert!(
            ev.iter()
                .any(|e| matches!(e, ProvisionEvent::OrphanDetected { released: true, .. }))
        );
    }

    #[test]
    fn cancellation_after_create_releases_the_vm() {
        let cancel = CancelToken::new();
        let f = Fake {
            creates: RefCell::new(VecDeque::from([Ok(81_920)])),
            cancel_on_probe: Some(cancel.clone()),
            ..Default::default()
        };
        let (r, ev, _) = run_fake(&f, &spec(None, 3, OnExhausted::StopAndFail), &cancel);
        assert!(r.unwrap_err().is(ErrorKind::Cancelled));
        assert!(f.live.borrow().is_empty(), "cancellation left a VM behind");
        assert!(
            !ev.iter()
                .any(|e| matches!(e, ProvisionEvent::Failed { .. }))
        );
    }

    #[test]
    fn unverifiable_stop_is_reported() {
        let f = Fake {
            creates: RefCell::new(VecDeque::from([Ok(40_960)])),
            stop_fails: true,
            ..Default::default()
        };
        let (r, _, _) = run_fake(
            &f,
            &spec(Some(70_000), 1, OnExhausted::StopAndFail),
            &CancelToken::new(),
        );
        assert!(r.unwrap_err().is(ErrorKind::OrphanSuspected));
    }

    #[test]
    fn session_cap_is_enforced_before_creating() {
        let f = Fake::default();
        f.live.borrow_mut().push(sess(Some("other"), "ep-o"));
        f.live.borrow_mut().push(sess(None, "browser"));
        let clock = ManualClock::fixed();
        let s = spec(None, 3, OnExhausted::StopAndFail);
        let limit = SessionLimit {
            max_concurrent: 2,
            reserved: 1,
        };
        let err = provision(&f, &s, limit, &clock, &CancelToken::new(), &|_| {}).unwrap_err();
        assert!(err.is(ErrorKind::SessionLimitReached));
        assert_eq!(err.param("limit"), Some("2"));
        assert!(!f.log.borrow().contains(&"create".to_owned()));
        // The unmanaged `[?]` runtime does not count against the cap.
        f.creates.borrow_mut().push_back(Ok(15_360));
        let limit = SessionLimit {
            max_concurrent: 2,
            reserved: 0,
        };
        assert!(provision(&f, &s, limit, &clock, &CancelToken::new(), &|_| {}).is_ok());
    }

    #[test]
    fn existing_name_and_bad_specs_are_refused() {
        let f = Fake::default();
        f.live.borrow_mut().push(sess(Some("llm"), "ep"));
        let (r, _, _) = run_fake(
            &f,
            &spec(None, 3, OnExhausted::StopAndFail),
            &CancelToken::new(),
        );
        assert!(r.unwrap_err().is(ErrorKind::InvalidInput));
        let mut s = ProvisionSpec::new(
            "cpu",
            Accelerator::None,
            &crate::defaults::Defaults::default(),
        );
        s.min_vram_mib = Some(1000);
        assert!(s.validate().is_err());
        s.max_attempts = 99;
        assert_eq!(s.attempts(), MAX_ATTEMPTS_CAP);
    }

    #[test]
    fn kept_endpoint_confirms_an_unnamed_session_by_endpoint_diff() {
        // The kept session may be missing from the state file, so `sessions()`
        // lists it as `[?]`; the endpoint that appeared since `before` is
        // still the one this attempt allocated.
        let f = Fake::default();
        f.live.borrow_mut().push(sess(None, "new-endpoint"));
        assert_eq!(kept_endpoint(&f, "llm", &BTreeSet::new()), "new-endpoint");

        // A named new endpoint is preferred over an unnamed one.
        f.live.borrow_mut().clear();
        f.live.borrow_mut().push(sess(None, "other"));
        f.live.borrow_mut().push(sess(Some("llm"), "mine"));
        assert_eq!(kept_endpoint(&f, "llm", &BTreeSet::new()), "mine");

        // Pre-existing endpoints are not the kept session.
        let before = BTreeSet::from(["old-endpoint".to_owned()]);
        f.live.borrow_mut().clear();
        f.live.borrow_mut().push(sess(None, "old-endpoint"));
        f.live.borrow_mut().push(sess(None, "new-endpoint"));
        assert_eq!(kept_endpoint(&f, "llm", &before), "new-endpoint");
    }
}
