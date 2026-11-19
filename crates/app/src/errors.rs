//! The one place that turns core's typed errors into words and a next step.
//!
//! Core never writes prose: an [`Error`] carries an [`ErrorKind`], named parameters, a log
//! detail and a [`NextStep`]. [`explain`] maps the kind to a catalog message (parameters filled
//! leniently, since some are only sometimes known), the next step to either a button the app
//! can act on now ([`ErrorAction`]) or a sentence, and keeps the detail for a "details" line.
//! Kinds and steps added to core later (both enums are non-exhaustive) still show a message
//! naming the kind.

use colab_playground_core::{Error, ErrorKind, NextStep};
use gpui::{App, SharedString};

use crate::i18n::{keys, tr, tr_lenient};

/// Something the app can do about an error right away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorAction {
    Retry,
    /// Check the environment again.
    Refresh,
    /// Go to the CLI install on the Overview page.
    InstallCli,
    /// Open the add-account dialog.
    AddAccount,
    /// Sign in again (the add-account dialog).
    SignInAgain,
    /// Go to the Accounts page to switch.
    SwitchAccount,
    /// Open Microsoft's WSL install guide.
    InstallWsl,
}

impl ErrorAction {
    pub fn label_key(self) -> &'static str {
        match self {
            ErrorAction::Retry => keys::ACTION_RETRY,
            ErrorAction::Refresh => keys::ACTION_CHECK_AGAIN,
            ErrorAction::InstallCli => keys::ACTION_INSTALL_CLI,
            ErrorAction::AddAccount => keys::ACTION_ADD_ACCOUNT,
            ErrorAction::SignInAgain => keys::ACTION_SIGN_IN_AGAIN,
            ErrorAction::SwitchAccount => keys::ACTION_SWITCH_ACCOUNT,
            ErrorAction::InstallWsl => keys::ACTION_INSTALL_WSL,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Explained {
    pub message: SharedString,
    /// What to do, when there is no button for it (or to go with the button).
    pub hint: Option<SharedString>,
    pub action: Option<ErrorAction>,
    /// Technical detail for a "details" line (untranslated, never secret).
    pub detail: Option<SharedString>,
}

/// The catalog key for `kind`, or `None` for a kind this build does not know.
pub fn kind_key(kind: ErrorKind) -> Option<&'static str> {
    use ErrorKind as K;
    Some(match kind {
        K::NotInstalled => keys::ERR_NOT_INSTALLED,
        K::AuthRequired => keys::ERR_AUTH_REQUIRED,
        K::NotEntitled => keys::ERR_NOT_ENTITLED,
        K::CapacityRefused => keys::ERR_CAPACITY_REFUSED,
        K::AllocationTimeout => keys::ERR_ALLOCATION_TIMEOUT,
        K::SshBusy => keys::ERR_SSH_BUSY,
        K::SessionGone => keys::ERR_SESSION_GONE,
        K::ConnectionLost => keys::ERR_CONNECTION_LOST,
        K::ExecTimeout => keys::ERR_EXEC_TIMEOUT,
        K::PortInUse => keys::ERR_PORT_IN_USE,
        K::PortReserved => keys::ERR_PORT_RESERVED,
        K::InvalidInput => keys::ERR_INVALID_INPUT,
        K::NoActiveAccount => keys::ERR_NO_ACTIVE_ACCOUNT,
        K::ActiveAccountNotAdopted => keys::ERR_ACTIVE_NOT_ADOPTED,
        K::AccountNotActive => keys::ERR_ACCOUNT_NOT_ACTIVE,
        K::AccountNotFound => keys::ERR_ACCOUNT_NOT_FOUND,
        K::KeyringUnavailable => keys::ERR_KEYRING_UNAVAILABLE,
        K::AddInProgress => keys::ERR_ADD_IN_PROGRESS,
        K::LegacyTokenPresent => keys::ERR_LEGACY_TOKEN,
        K::LoginCancelled => keys::ERR_LOGIN_CANCELLED,
        K::LoginInvalidCode => keys::ERR_LOGIN_INVALID_CODE,
        K::LoginVerificationFailed => keys::ERR_LOGIN_UNVERIFIED,
        K::Cancelled => keys::ERR_CANCELLED,
        K::Unsupported => keys::ERR_UNSUPPORTED,
        K::WslUnavailable => keys::ERR_WSL_UNAVAILABLE,
        K::CommandTooLong => keys::ERR_COMMAND_TOO_LONG,
        K::SessionLimitReached => keys::ERR_SESSION_LIMIT,
        K::Busy => keys::ERR_BUSY,
        K::ProvisionExhausted => keys::ERR_PROVISION_EXHAUSTED,
        K::OrphanSuspected => keys::ERR_ORPHAN_SUSPECTED,
        K::StopUnconfirmed => keys::ERR_STOP_UNCONFIRMED,
        K::ManifestInvalid => keys::ERR_MANIFEST_INVALID,
        K::PluginExists => keys::ERR_PLUGIN_EXISTS,
        K::PluginNotFound => keys::ERR_PLUGIN_NOT_FOUND,
        K::PluginRunFailed => keys::ERR_PLUGIN_RUN_FAILED,
        K::HealthCheckFailed => keys::ERR_HEALTH_CHECK_FAILED,
        K::GitFailed => keys::ERR_GIT_FAILED,
        K::InstallFailed => keys::ERR_INSTALL_FAILED,
        K::Http => keys::ERR_HTTP,
        K::HttpUnavailable => keys::ERR_HTTP_UNAVAILABLE,
        K::RateLimited => keys::ERR_RATE_LIMITED,
        K::Parse => keys::ERR_PARSE,
        K::Io => keys::ERR_IO,
        K::Timeout => keys::ERR_TIMEOUT,
        K::UnsupportedCliVersion => keys::ERR_UNSUPPORTED_CLI,
        K::Other => keys::ERR_OTHER,
        _ => return None,
    })
}

/// What `step` offers: a button the app can act on in this build, and/or a sentence.
/// `can_retry` says whether the caller can run the failed thing again.
pub fn next_step(step: NextStep, can_retry: bool) -> (Option<ErrorAction>, Option<&'static str>) {
    use NextStep as S;
    match step {
        S::None => (None, None),
        S::TryAgain if can_retry => (Some(ErrorAction::Retry), None),
        S::TryAgain => (None, Some(keys::NS_TRY_AGAIN)),
        S::RetryLater if can_retry => (Some(ErrorAction::Retry), Some(keys::NS_RETRY_LATER)),
        S::RetryLater => (None, Some(keys::NS_RETRY_LATER)),
        S::SwitchAccount => (Some(ErrorAction::SwitchAccount), None),
        S::AddAccount => (Some(ErrorAction::AddAccount), None),
        S::SignInAgain => (Some(ErrorAction::SignInAgain), None),
        S::UnlockKeyring => (Some(ErrorAction::Refresh), Some(keys::NS_UNLOCK_KEYRING)),
        S::InstallCli => (Some(ErrorAction::InstallCli), None),
        S::InstallWsl => (Some(ErrorAction::InstallWsl), Some(keys::NS_INSTALL_WSL)),
        S::Refresh => (Some(ErrorAction::Refresh), None),
        S::ChooseOtherAccelerator => (None, Some(keys::NS_OTHER_ACCELERATOR)),
        S::StopASession => (None, Some(keys::NS_STOP_A_SESSION)),
        S::ChooseOtherPort => (None, Some(keys::NS_OTHER_PORT)),
        S::CloseOtherConnection => (None, Some(keys::NS_CLOSE_CONNECTION)),
        S::UpdateApp => (None, Some(keys::NS_UPDATE_APP)),
        S::CheckInput => (None, Some(keys::NS_CHECK_INPUT)),
        S::CheckNetwork if can_retry => (Some(ErrorAction::Retry), Some(keys::NS_CHECK_NETWORK)),
        S::CheckNetwork => (None, Some(keys::NS_CHECK_NETWORK)),
        S::ReviewRuntimes => (None, Some(keys::NS_REVIEW_RUNTIMES)),
        S::ViewLog => (None, Some(keys::NS_VIEW_LOG)),
        S::ReportProblem => (None, Some(keys::NS_REPORT_PROBLEM)),
        _ => (None, None),
    }
}

pub fn explain(error: &Error, can_retry: bool, cx: &App) -> Explained {
    let params: Vec<(&str, &str)> = error
        .params
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let message = match kind_key(error.kind) {
        Some(key) => tr_lenient(cx, key, &params),
        None => tr_lenient(cx, keys::ERR_UNKNOWN, &[("kind", error.kind.as_str())]),
    };
    let (action, hint) = next_step(error.next_step, can_retry);
    let detail = (!error.detail.trim().is_empty())
        .then(|| tr_lenient(cx, keys::ERR_DETAILS, &[("detail", error.detail.trim())]));
    Explained {
        message,
        hint: hint.map(|key| tr(cx, key)),
        action,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use colab_playground_core::{ErrorKind, NextStep};

    use super::{ErrorAction, kind_key, next_step};

    #[test]
    fn every_kind_this_build_knows_has_a_message() {
        let kinds = [
            ErrorKind::NotInstalled,
            ErrorKind::AuthRequired,
            ErrorKind::KeyringUnavailable,
            ErrorKind::LoginInvalidCode,
            ErrorKind::InstallFailed,
            ErrorKind::UnsupportedCliVersion,
            ErrorKind::Other,
        ];
        for kind in kinds {
            assert!(kind_key(kind).is_some(), "{kind:?}");
        }
    }

    #[test]
    fn steps_become_buttons_only_where_the_app_can_act() {
        assert_eq!(
            next_step(NextStep::InstallCli, false).0,
            Some(ErrorAction::InstallCli)
        );
        assert_eq!(
            next_step(NextStep::TryAgain, true).0,
            Some(ErrorAction::Retry)
        );
        assert_eq!(next_step(NextStep::TryAgain, false).0, None);
        assert!(next_step(NextStep::TryAgain, false).1.is_some());
        assert_eq!(next_step(NextStep::StopASession, true).0, None);
        assert_eq!(next_step(NextStep::None, true), (None, None));
    }
}
