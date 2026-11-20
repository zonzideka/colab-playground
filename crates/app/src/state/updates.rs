//! The update check and the in-place install: whether a newer release is published, and
//! downloading, verifying and swapping it in when the user asks.
//!
//! Automatic checks run from the usage poller (so never before the first frame or the setup
//! checks), at most once a day on the backend clock; the time of the last check is saved, so a
//! restart within the day does not check again. A failed automatic check is only shown in
//! Settings → About; a manual one also says so in a toast. A found release is announced once
//! per version (a banner the user can dismiss for that version) and stays marked in the sidebar
//! and in About until this build is at least that version.
//!
//! Installing is always manual: from the banner or About the user downloads the release
//! (verified against its SHA256SUMS by the backend), then restarts to swap it in. Platforms
//! whose release has no installable asset keep the older flow of opening the release page.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use colab_playground_core::{
    Error, ErrorKind,
    update::{self, Release, UpdateStatus, Version},
};
use ely_gpui_component::{feedback::Toast, primitives::Severity};
use gpui::Context;

use super::{AppState, Op};
use crate::{
    errors,
    i18n::{keys, tr},
    settings::OnByDefault,
};

/// How often the automatic check asks GitHub.
pub const UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone, Debug, Default, PartialEq)]
pub enum UpdateCheck {
    /// Not checked in this run.
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    /// The release is downloading (and then verifying) in the background.
    Downloading {
        release: Release,
        received: u64,
        total: Option<u64>,
    },
    /// The release is downloaded and verified, waiting for a restart to swap in.
    Ready {
        release: Release,
    },
    Failed(Error),
}

fn secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn is_newer(version: &str) -> bool {
    match (
        Version::parse(version),
        Version::parse(update::CURRENT_VERSION),
    ) {
        (Some(found), Some(current)) => found > current,
        _ => false,
    }
}

impl AppState {
    pub fn update_check(&self) -> &UpdateCheck {
        &self.update
    }

    /// The newer release to point at (found now or by an earlier check), while this build is
    /// older than it: `(version, page)`.
    pub fn update_available(&self) -> Option<(String, String)> {
        if let UpdateCheck::Available(release) = &self.update {
            return Some((release.version.clone(), release.page.clone()));
        }
        let version = self.settings.update_latest.clone()?;
        if !is_newer(&version) {
            return None;
        }
        let page = update::this_app_release_page(&version)?;
        Some((version, page))
    }

    pub fn update_notice(&self) -> Option<(String, String)> {
        self.update_available()
            .filter(|(version, _)| self.settings.update_dismissed.as_deref() != Some(version))
    }

    pub fn checks_updates(&self) -> bool {
        self.settings.check_updates.0
    }

    pub fn set_check_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.check_updates = OnByDefault(on);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Hides the banner for `version`; the sidebar and About keep pointing at it.
    pub fn dismiss_update(&mut self, version: String, cx: &mut Context<Self>) {
        self.settings.update_dismissed = Some(version);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Opens the release page in the browser: the fallback for platforms whose release has
    /// no installable asset, and for a download that failed.
    pub fn open_update_page(&self, cx: &mut Context<Self>) {
        if let Some((_, page)) = self.update_available() {
            cx.open_url(&page);
        }
    }

    /// Whether the found release can be installed from inside the app on this platform.
    pub fn update_installable(&self) -> bool {
        match &self.update {
            UpdateCheck::Available(release) => release.installable_asset().is_some(),
            _ => false,
        }
    }

    /// The localized one-line progress of a running install download, for the banner and About.
    pub fn update_progress(&self, cx: &gpui::App) -> Option<gpui::SharedString> {
        let UpdateCheck::Downloading {
            received, total, ..
        } = &self.update
        else {
            return None;
        };
        Some(match total {
            Some(total) if *total > 0 => crate::i18n::tr_args(
                cx,
                keys::UPDATE_DOWNLOADING_PCT,
                &[("percent", &((*received * 100 / *total).to_string()))],
            ),
            _ => crate::i18n::tr_args(
                cx,
                keys::UPDATE_DOWNLOADING_SIZE,
                &[("size", &crate::format::bytes(cx, *received))],
            ),
        })
    }

    /// Downloads the found release in the background; success lands in
    /// [`UpdateCheck::Ready`] with the staged update kept for [`Self::restart_to_update`].
    /// A failure is toasted and the state returns to `Available`, so the user can retry or
    /// fall back to the release page.
    pub fn install_update(&mut self, cx: &mut Context<Self>) {
        let UpdateCheck::Available(release) = &self.update else {
            return;
        };
        if release.installable_asset().is_none() {
            return;
        }
        let release = release.clone();
        self.update = UpdateCheck::Downloading {
            release: release.clone(),
            received: 0,
            total: None,
        };
        let for_done = release.clone();
        self.run_backend_streaming(
            Op::UpdateInstall,
            cx,
            move |backend, events, cancel| backend.stage_update(&release, events, cancel),
            |state, (received, total), _cx| {
                if let UpdateCheck::Downloading {
                    received: r,
                    total: t,
                    ..
                } = &mut state.update
                {
                    *r = received;
                    *t = total;
                }
            },
            move |state, result, cx| {
                match result {
                    Ok(staged) => {
                        state.staged_update = Some(staged);
                        state.update = UpdateCheck::Ready { release: for_done };
                    }
                    Err(error) if error.kind == ErrorKind::Cancelled => {
                        state.update = UpdateCheck::Available(for_done);
                    }
                    Err(error) => {
                        state.install_failed(error, cx);
                        state.update = UpdateCheck::Available(for_done);
                    }
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    /// Asks a running install download to stop; the state returns to `Available` when the
    /// backend call winds down.
    pub fn cancel_update_install(&mut self, cx: &mut Context<Self>) {
        self.ops.cancel(Op::UpdateInstall);
        cx.notify();
    }

    /// Swaps the staged update in and exits, so the spawned new instance takes over. A
    /// failed swap keeps the staged update and the `Ready` state, ready to try again.
    pub fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let Some(staged) = self.staged_update.take() else {
            return;
        };
        let applied = std::env::current_exe()
            .map_err(colab_playground_core::Error::from)
            .and_then(|exe| colab_playground_core::self_update::apply(&staged, &exe));
        match applied {
            Ok(()) => {
                self.persist_settings(cx);
                std::process::exit(0);
            }
            Err(error) => {
                self.staged_update = Some(staged);
                self.install_failed(error, cx);
            }
        }
    }

    fn install_failed(&mut self, error: Error, cx: &mut Context<Self>) {
        log::warn!("updates: the install failed: {error}");
        let message = errors::explain(&error, false, cx).message;
        let toast = Toast::new(tr(cx, keys::UPDATE_INSTALL_FAILED_TITLE))
            .body(message)
            .severity(Severity::Warning);
        self.toast(toast, cx);
        cx.notify();
    }

    pub fn check_updates_now(&mut self, cx: &mut Context<Self>) {
        self.run_update_check(true, cx);
    }

    /// One poller wake: checks when automatic checks are on and a day has passed since the
    /// last check (on the backend clock). The first wake also removes the leftovers of an
    /// earlier in-place update (the replaced install's backup, staged downloads).
    pub(crate) fn maybe_check_updates(&mut self, cx: &mut Context<Self>) {
        if !self.update_cleanup_done {
            self.update_cleanup_done = true;
            if let (Ok(current_exe), Some(data_dir)) = (
                std::env::current_exe(),
                colab_playground_core::paths::default_local_data_dir(),
            ) {
                colab_playground_core::self_update::cleanup_stale(&current_exe, &data_dir);
            }
        }
        if self.checks_updates() && !self.ops.is_running(Op::UpdateCheck) {
            self.run_update_check(false, cx);
        }
    }

    fn run_update_check(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.ops.is_running(Op::UpdateCheck) {
            return;
        }
        let last = self.settings.update_checked_at;
        if manual {
            self.update = UpdateCheck::Checking;
        }
        self.run_backend(
            Op::UpdateCheck,
            cx,
            move |backend, cancel| {
                let now = secs(backend.now());
                let due = manual || last.is_none_or(|last| now >= last + UPDATE_INTERVAL.as_secs());
                if !due {
                    return Ok(None);
                }
                Ok(Some((backend.check_for_update(cancel), now)))
            },
            move |state, result, cx| {
                let Ok(Some((outcome, now))) = result else {
                    if let Err(error) = result {
                        state.update_failed(error, manual, cx);
                    }
                    return;
                };
                state.settings.update_checked_at = Some(now);
                match outcome {
                    Ok(UpdateStatus::UpToDate) => {
                        state.update = UpdateCheck::UpToDate;
                        if state
                            .settings
                            .update_latest
                            .as_deref()
                            .is_some_and(|v| !is_newer(v))
                        {
                            state.settings.update_latest = None;
                        }
                    }
                    Ok(UpdateStatus::Available(release)) => {
                        state.settings.update_latest = Some(release.version.clone());
                        // A staged update for a different version is superseded.
                        if state
                            .staged_update
                            .as_ref()
                            .is_some_and(|staged| staged.version != release.version)
                        {
                            state.staged_update = None;
                        }
                        // A check landing mid-install must not stomp the install states.
                        if !matches!(
                            state.update,
                            UpdateCheck::Downloading { .. } | UpdateCheck::Ready { .. }
                        ) {
                            state.update = UpdateCheck::Available(release);
                        }
                    }
                    Err(error) => state.update_failed(error, manual, cx),
                }
                state.persist_settings(cx);
                cx.notify();
            },
        );
        cx.notify();
    }

    fn update_failed(&mut self, error: Error, manual: bool, cx: &mut Context<Self>) {
        log::warn!("updates: the check failed: {error}");
        if manual {
            let message = errors::explain(&error, false, cx).message;
            let toast = Toast::new(tr(cx, keys::UPDATE_FAILED_TITLE))
                .body(message)
                .severity(Severity::Warning);
            self.toast(toast, cx);
        }
        self.update = UpdateCheck::Failed(error);
        cx.notify();
    }
}
