//! Sessions: every runtime on every saved account, stopping (any runtime, any account),
//! extending and cancelling auto-stop, what the cost guard decided, and runtimes that ended.
//!
//! A runtime that ended drops out of `runtimes()`, so the app keeps its own short list of
//! ended runtimes with the reason, for the reason to stay visible.

use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, SystemTime},
};

use colab_playground_core::{
    Error, ErrorKind,
    backend::{GuardReport, Runtime, RuntimeRef, RuntimesView, StopOptions},
    guard::{Concern, Decision},
    sessions::StopReason,
    tunnel::{TunnelId, TunnelInfo},
};
use ely_gpui_component::{feedback::Toast, primitives::Severity};
use gpui::Context;

use super::{AppState, Load, Op, Overlay, ops::key_of};
use crate::{
    errors, format,
    i18n::{keys, tr, tr_args},
};

/// How many ended runtimes the Sessions page keeps listing.
const ENDED_KEPT: usize = 20;

#[derive(Default)]
pub struct SessionsState {
    pub(crate) runtimes: Load<RuntimesView>,
    pub(crate) now: Option<SystemTime>,
    pub(crate) ended: Vec<Ended>,
    pub(crate) warnings: HashMap<RuntimeRef, Vec<Concern>>,
    pub(crate) stop_failures: HashMap<RuntimeRef, Error>,
    pub(crate) tunnels: BTreeMap<TunnelId, TunnelInfo>,
    pub(crate) guard_error: Option<Error>,
}

/// A runtime that ended while the app watched, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Ended {
    pub runtime: RuntimeRef,
    pub label: String,
    pub email: Option<String>,
    pub reason: EndReason,
    /// When (backend clock).
    pub at: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EndReason {
    User,
    Guard(Concern),
    /// It disappeared: its watchdog, Colab, or a stop elsewhere.
    Vanished,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StopPrompt {
    pub target: RuntimeRef,
    pub label: String,
    pub email: Option<String>,
    pub error: Option<Error>,
}

/// The name a runtime is shown by: its session name, or its hardware for runtimes opened
/// elsewhere (they have no name).
pub fn runtime_label(runtime: &Runtime) -> String {
    runtime
        .id
        .name
        .clone()
        .unwrap_or_else(|| runtime.hardware.clone())
}

impl AppState {
    /// Runtimes on every saved account, as last listed.
    pub fn runtimes(&self) -> &Load<RuntimesView> {
        &self.sessions.runtimes
    }

    /// The backend clock at the last listing.
    pub fn sessions_now(&self) -> Option<SystemTime> {
        self.sessions.now.or(self.usage_now())
    }

    pub fn runtime(&self, target: &RuntimeRef) -> Option<&Runtime> {
        self.sessions
            .runtimes
            .ready()?
            .runtimes
            .iter()
            .find(|runtime| &runtime.id == target)
    }

    /// Runtimes that ended while the app watched, newest first.
    pub fn ended(&self) -> &[Ended] {
        &self.sessions.ended
    }

    /// What the guard last said about `target` (close to a limit, or cannot check).
    pub fn guard_warnings(&self, target: &RuntimeRef) -> &[Concern] {
        self.sessions
            .warnings
            .get(target)
            .map_or(&[], Vec::as_slice)
    }

    /// The guard tried to stop `target` and failed: it is still billing.
    pub fn guard_stop_failure(&self, target: &RuntimeRef) -> Option<&Error> {
        self.sessions.stop_failures.get(target)
    }

    pub fn guard_error(&self) -> Option<&Error> {
        self.sessions.guard_error.as_ref()
    }

    pub fn is_stopping(&self, target: &RuntimeRef) -> bool {
        self.ops.is_running(Op::Stop(key_of(target)))
    }

    /// Whether a guard round may find something to check: the app manages a live runtime, or
    /// the list is not known (loading or failed), so the guard keeps watching.
    pub fn has_managed_runtimes(&self) -> bool {
        match &self.sessions.runtimes {
            Load::Ready(view) => view.runtimes.iter().any(|runtime| runtime.managed),
            _ => self.has_saved_accounts(),
        }
    }

    pub fn load_runtimes(&mut self, cx: &mut Context<Self>) {
        if !self.cli_installed() || !self.has_saved_accounts() {
            return;
        }
        self.sessions.runtimes.begin();
        self.quit_waits_for_listing();
        self.run_backend(
            Op::Runtimes,
            cx,
            |backend, cancel| Ok((backend.runtimes(cancel)?, backend.tunnels(), backend.now())),
            |state, result, cx| {
                match result {
                    Ok((view, tunnels, now)) => {
                        state.sessions.now = Some(now);
                        let live: Vec<&RuntimeRef> =
                            view.runtimes.iter().map(|runtime| &runtime.id).collect();
                        state.sessions.warnings.retain(|key, _| live.contains(&key));
                        state
                            .sessions
                            .stop_failures
                            .retain(|key, _| live.contains(&key));
                        state.sessions.tunnels = tunnels
                            .into_iter()
                            .map(|tunnel| (tunnel.id, tunnel))
                            .collect();
                        state.sessions.runtimes = Load::Ready(view);
                    }
                    Err(error) => {
                        log::warn!("sessions: listing failed: {error}");
                        state.sessions.runtimes = Load::Failed(error);
                    }
                }
                cx.notify();
            },
        );
    }

    pub fn request_stop(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(runtime) = self.runtime(&target) else {
            return;
        };
        self.overlay = Some(Overlay::Stop(StopPrompt {
            label: runtime_label(runtime),
            email: runtime.account_email.clone(),
            target,
            error: None,
        }));
        cx.notify();
    }

    pub fn confirm_stop(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::Stop(prompt)) = &self.overlay else {
            return;
        };
        let prompt = prompt.clone();
        self.stop_runtime(prompt.target.clone(), cx);
    }

    /// Stops `target` now (the caller has confirmed).
    pub(crate) fn stop_runtime(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let label = self
            .runtime(&target)
            .map(runtime_label)
            .unwrap_or_else(|| target.name.clone().unwrap_or_default());
        let email = self
            .runtime(&target)
            .and_then(|runtime| runtime.account_email.clone());
        let work_target = target.clone();
        self.run_backend(
            Op::Stop(key_of(&target)),
            cx,
            move |backend, cancel| {
                backend.stop_session(&work_target, StopOptions::default(), cancel)?;
                Ok(backend.now())
            },
            move |state, result, cx| {
                let prompt_open =
                    matches!(&state.overlay, Some(Overlay::Stop(p)) if p.target == target);
                match result {
                    Ok(now) => {
                        if prompt_open {
                            state.overlay = None;
                        }
                        state.forget_runtime(&target);
                        state.record_ended(
                            target.clone(),
                            label.clone(),
                            email,
                            EndReason::User,
                            Some(now),
                        );
                        state.toast_done(tr_args(cx, keys::STOP_DONE, &[("name", &label)]), cx);
                        state.after_runtime_change(cx);
                    }
                    Err(error) => {
                        log::warn!("sessions: stopping {label} failed: {error}");
                        if prompt_open && let Some(Overlay::Stop(prompt)) = &mut state.overlay {
                            prompt.error = Some(error);
                        } else {
                            state.toast_error(&error, cx);
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    /// How much time Extend adds to `runtime`'s auto-stop: the default runtime limit
    /// (Settings, else core's own). The session's recorded limit is not used: it counts from
    /// the session's start and grows with every extension.
    pub fn extension_for(&self, runtime: &Runtime) -> Option<Duration> {
        let _ = runtime;
        self.rearm_duration()
    }

    /// Adds [`Self::extension_for`] to `target`'s auto-stop; it never moves earlier. Core's
    /// `extend_session` sets the stop a given time from now, so the time still left is measured
    /// on the backend clock inside the call and the extension is added to it. A runtime without
    /// an auto-stop gets one that far from now.
    pub fn extend_session(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(runtime) = self.runtime(&target) else {
            return;
        };
        let Some(by) = self.extension_for(runtime) else {
            return;
        };
        let stop_at = runtime.auto_stop_at;
        let work_target = target.clone();
        self.run_backend(
            Op::Extend(key_of(&target)),
            cx,
            move |backend, cancel| {
                let left = stop_at
                    .and_then(|at| at.duration_since(backend.now()).ok())
                    .unwrap_or_default();
                let total = left + by;
                let info = backend.extend_session(&work_target, total, cancel)?;
                Ok((info, total))
            },
            move |state, result, cx| {
                match result {
                    Ok((info, total)) => {
                        state.sessions.warnings.remove(&target);
                        let title = tr_args(
                            cx,
                            keys::EXTEND_DONE,
                            &[("time", &format::duration(cx, total))],
                        );
                        state.toast_auto_stop(title, info.armed, cx);
                        state.load_runtimes(cx);
                    }
                    Err(error) => state.toast_error(&error, cx),
                }
                cx.notify();
            },
        );
    }

    /// Asks before cancelling `target`'s auto-stop: afterwards it bills until stopped by hand.
    pub fn request_cancel_auto_stop(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(runtime) = self.runtime(&target) else {
            return;
        };
        self.overlay = Some(Overlay::CancelAutoStop(StopPrompt {
            label: runtime_label(runtime),
            email: runtime.account_email.clone(),
            target,
            error: None,
        }));
        cx.notify();
    }

    pub fn confirm_cancel_auto_stop(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::CancelAutoStop(prompt)) = &self.overlay else {
            return;
        };
        let target = prompt.target.clone();
        self.cancel_auto_stop(target, cx);
    }

    pub fn is_cancelling_auto_stop(&self, target: &RuntimeRef) -> bool {
        self.ops.is_running(Op::CancelAutoStop(key_of(target)))
    }

    pub fn cancel_auto_stop(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let work_target = target.clone();
        self.run_backend(
            Op::CancelAutoStop(key_of(&target)),
            cx,
            move |backend, cancel| backend.cancel_auto_stop(&work_target, cancel),
            move |state, result, cx| {
                let asking = matches!(&state.overlay, Some(Overlay::CancelAutoStop(p)) if p.target == target);
                match result {
                    Ok(()) => {
                        if asking {
                            state.overlay = None;
                        }
                        state.sessions.warnings.remove(&target);
                        state.toast(
                            Toast::new(tr(cx, keys::AUTO_STOP_CANCELLED))
                                .severity(Severity::Warning),
                            cx,
                        );
                        state.load_runtimes(cx);
                    }
                    Err(error) => {
                        if asking && let Some(Overlay::CancelAutoStop(prompt)) = &mut state.overlay {
                            prompt.error = Some(error);
                        } else {
                            state.toast_error(&error, cx);
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    /// How far ahead "Set auto-stop" puts the stop on a runtime that has none: the default
    /// runtime limit (core's own default when Settings has none).
    pub fn rearm_duration(&self) -> Option<Duration> {
        self.defaults()
            .max_runtime
            .or(colab_playground_core::Defaults::default().max_runtime)
    }

    /// Sets an auto-stop again on `target` (after it was cancelled), [`Self::rearm_duration`]
    /// from now. Core's `extend_session` restarts the watchdog on the runtime; when that fails
    /// the limit still holds through the app-side guard, and the toast says so.
    pub fn set_auto_stop(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(by) = self.rearm_duration() else {
            return;
        };
        let work_target = target.clone();
        self.run_backend(
            Op::Extend(key_of(&target)),
            cx,
            move |backend, cancel| backend.extend_session(&work_target, by, cancel),
            move |state, result, cx| {
                match result {
                    Ok(info) => {
                        let title = tr_args(
                            cx,
                            keys::AUTO_STOP_SET,
                            &[("time", &format::duration(cx, by))],
                        );
                        state.toast_auto_stop(title, info.armed, cx);
                        state.load_runtimes(cx);
                    }
                    Err(error) => state.toast_error(&error, cx),
                }
                let _ = &target;
                cx.notify();
            },
        );
    }

    /// Applies one guard round: warnings (a toast only when they change), automatic stops,
    /// failed stops (still billing) and runtimes that disappeared.
    pub(crate) fn apply_guard(&mut self, report: GuardReport, cx: &mut Context<Self>) {
        self.sessions.guard_error = None;
        let mut changed = false;
        for decision in report.decisions {
            let target = decision.session.clone();
            let label = self
                .runtime(&target)
                .map(runtime_label)
                .unwrap_or_else(|| target.name.clone().unwrap_or_default());
            let email = self
                .runtime(&target)
                .and_then(|runtime| runtime.account_email.clone());
            match decision.decision {
                Decision::Ok => {
                    self.sessions.warnings.remove(&target);
                }
                Decision::Warn(concerns) => {
                    let before = self.sessions.warnings.get(&target);
                    let limits: Vec<&Concern> = concerns
                        .iter()
                        .filter(|c| matches!(c, Concern::Limit { .. }))
                        .collect();
                    let was: Vec<&Concern> = before
                        .map(|b| {
                            b.iter()
                                .filter(|c| matches!(c, Concern::Limit { .. }))
                                .collect()
                        })
                        .unwrap_or_default();
                    let same_kinds = limits.len() == was.len()
                        && limits
                            .iter()
                            .zip(&was)
                            .all(|(a, b)| limit_of(a) == limit_of(b));
                    if !limits.is_empty() && !same_kinds {
                        let what = format::concerns(cx, &concerns);
                        self.toast(
                            Toast::new(tr_args(cx, keys::GUARD_WARN_TITLE, &[("name", &label)]))
                                .body(what)
                                .severity(Severity::Warning)
                                .stay(None),
                            cx,
                        );
                    }
                    self.sessions.warnings.insert(target, concerns);
                }
                Decision::Stop(concern) => {
                    if decision.stopped {
                        self.forget_runtime(&target);
                        self.toast(
                            Toast::new(tr_args(cx, keys::GUARD_STOPPED_TITLE, &[("name", &label)]))
                                .body(format::concern(cx, &concern))
                                .severity(Severity::Danger)
                                .stay(None),
                            cx,
                        );
                        self.record_ended(
                            target,
                            label,
                            email,
                            EndReason::Guard(concern),
                            self.usage_now(),
                        );
                        changed = true;
                    } else {
                        // Not stopped: a failed stop, or (should core ever report neither) a
                        // stop that did not happen. Either way it is still billing.
                        let error = decision.error.unwrap_or_else(|| {
                            Error::new(ErrorKind::Other)
                                .detail("the cost guard decided to stop it but no stop was made")
                        });
                        let first = !self.sessions.stop_failures.contains_key(&target);
                        if first {
                            self.toast(
                                Toast::new(tr_args(
                                    cx,
                                    keys::GUARD_STOP_FAILED_TITLE,
                                    &[("name", &label)],
                                ))
                                .body(errors::explain(&error, false, cx).message)
                                .severity(Severity::Danger)
                                .stay(None),
                                cx,
                            );
                        }
                        self.sessions.stop_failures.insert(target, error);
                    }
                }
            }
        }
        for gone in report.vanished {
            let target = gone.runtime;
            let label = self
                .runtime(&target)
                .map(runtime_label)
                .unwrap_or_else(|| target.name.clone().unwrap_or_default());
            let email = self
                .runtime(&target)
                .and_then(|runtime| runtime.account_email.clone());
            self.forget_runtime(&target);
            self.toast(
                Toast::new(tr_args(cx, keys::VANISHED_TITLE, &[("name", &label)]))
                    .severity(Severity::Info),
                cx,
            );
            let reason = match gone.reason {
                StopReason::User => EndReason::User,
                StopReason::Guard { concern } => EndReason::Guard(concern),
                StopReason::Provision | StopReason::Vanished => EndReason::Vanished,
            };
            self.record_ended(target, label, email, reason, self.usage_now());
            changed = true;
        }
        if changed {
            self.load_runtimes(cx);
        }
        cx.notify();
    }

    /// A stop elsewhere, a provision or a switch changed what runs: list again and refresh
    /// usage.
    pub(crate) fn after_runtime_change(&mut self, cx: &mut Context<Self>) {
        self.load_runtimes(cx);
        self.refresh_usage(true, cx);
    }

    fn forget_runtime(&mut self, target: &RuntimeRef) {
        self.sessions.warnings.remove(target);
        self.sessions.stop_failures.remove(target);
        self.sessions.tunnels.retain(|_, tunnel| {
            !(tunnel.spec.account == target.account
                && Some(&tunnel.spec.session) == target.name.as_ref())
        });
        if let Load::Ready(view) = &mut self.sessions.runtimes {
            view.runtimes.retain(|runtime| &runtime.id != target);
        }
    }

    fn record_ended(
        &mut self,
        runtime: RuntimeRef,
        label: String,
        email: Option<String>,
        reason: EndReason,
        at: Option<SystemTime>,
    ) {
        self.sessions.ended.retain(|ended| ended.runtime != runtime);
        self.sessions.ended.insert(
            0,
            Ended {
                runtime,
                label,
                email,
                reason,
                at,
            },
        );
        self.sessions.ended.truncate(ENDED_KEPT);
    }

    pub fn clear_ended(&mut self, cx: &mut Context<Self>) {
        self.sessions.ended.clear();
        cx.notify();
    }

    /// After moving or setting an auto-stop: whether the runtime itself will stop then (its
    /// watchdog is armed, so it works with the app closed) or only this app will, while open.
    fn toast_auto_stop(&self, title: gpui::SharedString, armed: bool, cx: &mut Context<Self>) {
        let toast = if armed {
            Toast::new(title)
                .body(tr(cx, keys::PROV_WATCHDOG))
                .severity(Severity::Success)
        } else {
            Toast::new(title)
                .body(tr(cx, keys::PROV_WATCHDOG_FAILED))
                .severity(Severity::Warning)
                .stay(None)
        };
        self.toast(toast, cx);
    }

    /// An error toast for a failed action, explained in the user's terms.
    pub(crate) fn toast_error(&self, error: &Error, cx: &mut Context<Self>) {
        let explained = errors::explain(error, false, cx);
        let mut toast = Toast::new(explained.message).severity(Severity::Danger);
        if let Some(hint) = explained.hint {
            toast = toast.body(hint);
        }
        self.toast(toast, cx);
    }
}

fn limit_of(concern: &Concern) -> Option<colab_playground_core::guard::Limit> {
    match concern {
        Concern::Limit { limit, .. } | Concern::UsageUnknown { limit } => Some(*limit),
    }
}
