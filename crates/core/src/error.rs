//! Machine-readable errors.
//!
//! Core never bakes user-facing prose into errors. Every [`Error`] carries an
//! [`ErrorKind`] that the UI maps to a localised message, a small set of named
//! parameters to interpolate into that message, and a free-form `detail` string
//! meant for logs and "show details" panes (usually the tail of a CLI's stderr).

use std::collections::BTreeMap;
use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What went wrong, independent of language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    /// The `colab` executable (or its helper interpreter) cannot be found.
    NotInstalled,
    /// The CLI has no usable credential and would start an interactive login.
    AuthRequired,
    /// Colab rejected the accelerator (HTTP 400): no entitlement or quota.
    NotEntitled,
    /// Colab refused the allocation (HTTP 412): too many sessions or a
    /// temporary capacity limit. Retrying later may help.
    CapacityRefused,
    /// Colab never answered the request for a runtime: `colab new` gave up
    /// after its own retries (each waits about two minutes for a reply).
    AllocationTimeout,
    /// A runtime already has its single allowed `colab ssh` connection (HTTP 429).
    SshBusy,
    /// The named session no longer exists (locally or on the server).
    SessionGone,
    /// The kernel connection dropped during `colab exec`.
    ConnectionLost,
    /// `colab exec` produced no output for longer than its idle timeout.
    ExecTimeout,
    /// The local end of a port forward is already taken. Param: `port`.
    PortInUse,
    /// The port is reserved (VM port 8080 belongs to Colab). Param: `port`.
    PortReserved,
    /// An argument failed validation. Params: `field`, optionally `value`.
    InvalidInput,
    /// No adopted account is active; adopt or add one first.
    NoActiveAccount,
    /// The credential in the CLI's slot is not saved as an account yet.
    ActiveAccountNotAdopted,
    /// The requested account is not the active one (CLI calls only run for the
    /// active account, to keep per-account state files consistent). Param: `account`.
    AccountNotActive,
    /// The account id is unknown. Param: `account`.
    AccountNotFound,
    /// The OS keyring backend is missing or locked.
    KeyringUnavailable,
    /// Linux has no Secret Service provider installed
    /// (`gnome-keyring-daemon` is missing), so there is no keyring to
    /// unlock; installing the provider is the fix.
    KeyringNotInstalled,
    /// A previous add-account flow was interrupted; roll it back first.
    AddInProgress,
    /// The CLI still has a legacy plaintext token file; it would be migrated
    /// into the slot during login, so adding an account is refused.
    LegacyTokenPresent,
    /// The login was cancelled (by the user or because the CLI exited early).
    LoginCancelled,
    /// The authorisation code was rejected.
    LoginInvalidCode,
    /// The new credential could not be verified (no email, refresh failed).
    LoginVerificationFailed,
    /// The operation was cancelled through its [`crate::CancelToken`].
    Cancelled,
    /// The operation is not supported on this host (e.g. POSIX scripts on a
    /// Windows `Host::Local`).
    Unsupported,
    /// WSL is not installed or has no distribution.
    WslUnavailable,
    /// A command line would exceed the platform limit.
    CommandTooLong,
    /// The app already runs `max_concurrent_sessions` sessions on this
    /// account. Param: `limit`.
    SessionLimitReached,
    /// Another account operation holds the credential slot; retry shortly.
    Busy,
    /// Hardware never matched the requested spec within `max_attempts`.
    /// Params: `attempts`, `gpu`, `vram_mib`.
    ProvisionExhausted,
    /// A VM may have been left allocated and could not be verified as released.
    /// Param: `endpoint` (when known).
    OrphanSuspected,
    /// A stop was requested and the runtime was released, but whether the
    /// endpoint actually disappeared could not be confirmed. Param: `endpoint`
    /// (when known).
    StopUnconfirmed,
    /// A plugin manifest is invalid. Params: `field`, `reason`.
    ManifestInvalid,
    /// A plugin with this id is already installed. Param: `plugin`.
    PluginExists,
    /// No plugin with this id. Param: `plugin`.
    PluginNotFound,
    /// A plugin run reported `failed`. Params: `stage`, `message`.
    PluginRunFailed,
    /// A plugin's endpoint did not answer its health probe.
    HealthCheckFailed,
    /// `git clone` failed. Param: `url`.
    GitFailed,
    /// Installing the CLI failed. Param: `exit_code`.
    InstallFailed,
    /// An HTTP request failed. Params: `status` (when there was one), `url`.
    Http,
    /// HTTP support was compiled out (`http` feature disabled).
    HttpUnavailable,
    /// A service is limiting requests (e.g. GitHub's API rate limit). Params: `status`, `url`.
    RateLimited,
    /// Output from the CLI or a remote script could not be parsed.
    Parse,
    /// Local file-system or process I/O failed.
    Io,
    /// The operation took too long.
    Timeout,
    /// The installed CLI's internals differ from what the helper expects
    /// (its Python client API changed).
    UnsupportedCliVersion,
    /// Anything else; see `detail`.
    Other,
}

impl ErrorKind {
    /// Stable snake_case identifier (matches the serde representation).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::NotInstalled => "not_installed",
            ErrorKind::AuthRequired => "auth_required",
            ErrorKind::NotEntitled => "not_entitled",
            ErrorKind::CapacityRefused => "capacity_refused",
            ErrorKind::AllocationTimeout => "allocation_timeout",
            ErrorKind::SshBusy => "ssh_busy",
            ErrorKind::SessionGone => "session_gone",
            ErrorKind::ConnectionLost => "connection_lost",
            ErrorKind::ExecTimeout => "exec_timeout",
            ErrorKind::PortInUse => "port_in_use",
            ErrorKind::PortReserved => "port_reserved",
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::NoActiveAccount => "no_active_account",
            ErrorKind::ActiveAccountNotAdopted => "active_account_not_adopted",
            ErrorKind::AccountNotActive => "account_not_active",
            ErrorKind::AccountNotFound => "account_not_found",
            ErrorKind::KeyringUnavailable => "keyring_unavailable",
            ErrorKind::KeyringNotInstalled => "keyring_not_installed",
            ErrorKind::AddInProgress => "add_in_progress",
            ErrorKind::LegacyTokenPresent => "legacy_token_present",
            ErrorKind::LoginCancelled => "login_cancelled",
            ErrorKind::LoginInvalidCode => "login_invalid_code",
            ErrorKind::LoginVerificationFailed => "login_verification_failed",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::WslUnavailable => "wsl_unavailable",
            ErrorKind::CommandTooLong => "command_too_long",
            ErrorKind::SessionLimitReached => "session_limit_reached",
            ErrorKind::Busy => "busy",
            ErrorKind::ProvisionExhausted => "provision_exhausted",
            ErrorKind::OrphanSuspected => "orphan_suspected",
            ErrorKind::StopUnconfirmed => "stop_unconfirmed",
            ErrorKind::ManifestInvalid => "manifest_invalid",
            ErrorKind::PluginExists => "plugin_exists",
            ErrorKind::PluginNotFound => "plugin_not_found",
            ErrorKind::PluginRunFailed => "plugin_run_failed",
            ErrorKind::HealthCheckFailed => "health_check_failed",
            ErrorKind::GitFailed => "git_failed",
            ErrorKind::InstallFailed => "install_failed",
            ErrorKind::Http => "http",
            ErrorKind::HttpUnavailable => "http_unavailable",
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::Parse => "parse",
            ErrorKind::Io => "io",
            ErrorKind::Timeout => "timeout",
            ErrorKind::UnsupportedCliVersion => "unsupported_cli_version",
            ErrorKind::Other => "other",
        }
    }

    pub fn default_next_step(self) -> NextStep {
        use ErrorKind as K;
        match self {
            K::NotInstalled => NextStep::InstallCli,
            K::AuthRequired | K::LoginVerificationFailed | K::LegacyTokenPresent => {
                NextStep::SignInAgain
            }
            K::LoginInvalidCode | K::LoginCancelled | K::AddInProgress => NextStep::TryAgain,
            K::NotEntitled | K::ProvisionExhausted | K::AllocationTimeout => {
                NextStep::ChooseOtherAccelerator
            }
            K::CapacityRefused
            | K::ConnectionLost
            | K::ExecTimeout
            | K::Busy
            | K::Timeout
            | K::RateLimited => NextStep::RetryLater,
            K::SshBusy => NextStep::CloseOtherConnection,
            K::SessionGone | K::AccountNotFound | K::PluginNotFound | K::KeyringNotInstalled => {
                NextStep::Refresh
            }
            K::PortInUse | K::PortReserved => NextStep::ChooseOtherPort,
            K::InvalidInput | K::ManifestInvalid => NextStep::CheckInput,
            K::NoActiveAccount | K::ActiveAccountNotAdopted => NextStep::AddAccount,
            K::AccountNotActive => NextStep::SwitchAccount,
            K::KeyringUnavailable => NextStep::UnlockKeyring,
            K::SessionLimitReached => NextStep::StopASession,
            K::OrphanSuspected => NextStep::ReviewRuntimes,
            K::StopUnconfirmed => NextStep::Refresh,
            K::WslUnavailable => NextStep::InstallWsl,
            K::UnsupportedCliVersion => NextStep::UpdateApp,
            K::InstallFailed | K::PluginRunFailed => NextStep::ViewLog,
            K::Http | K::HttpUnavailable | K::GitFailed | K::HealthCheckFailed => {
                NextStep::CheckNetwork
            }
            K::PluginExists | K::Cancelled => NextStep::None,
            K::Unsupported | K::CommandTooLong | K::Parse | K::Io | K::Other => {
                NextStep::ReportProblem
            }
        }
    }

    /// Parses the identifier produced by [`ErrorKind::as_str`] (used by the
    /// account helper, which reports kinds as strings). Unknown values map to
    /// [`ErrorKind::Other`].
    pub fn from_str_lossy(s: &str) -> ErrorKind {
        serde_json::from_value(serde_json::Value::String(s.to_owned())).unwrap_or(ErrorKind::Other)
    }
}

/// What the user can do about an error (the UI turns it into a button or hint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NextStep {
    /// Nothing to suggest (e.g. the user cancelled).
    None,
    /// Try the same thing again now.
    TryAgain,
    /// Wait and retry (capacity, network hiccup, busy).
    RetryLater,
    /// Switch to the account that owns the resource.
    SwitchAccount,
    /// Add or adopt an account.
    AddAccount,
    SignInAgain,
    /// Unlock or configure the OS keyring.
    UnlockKeyring,
    ChooseOtherAccelerator,
    /// Stop a running session first.
    StopASession,
    ChooseOtherPort,
    /// Close the other ssh connection to this runtime.
    CloseOtherConnection,
    InstallCli,
    InstallWsl,
    /// Update Colab Playground (the CLI changed underneath it).
    UpdateApp,
    CheckInput,
    CheckNetwork,
    /// Refresh the view; the thing is gone.
    Refresh,
    /// Review the runtime list (a VM may still be billing).
    ReviewRuntimes,
    ViewLog,
    /// Unexpected; report it.
    ReportProblem,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error with a machine-readable kind, named parameters and a log detail.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub struct Error {
    pub kind: ErrorKind,
    /// Named values for the localised message (e.g. `port` → `"8080"`).
    pub params: BTreeMap<String, String>,
    /// Untranslated technical detail for logs; may be empty. Never contains secrets.
    pub detail: String,
    /// Suggested next step (defaults from the kind; may be overridden).
    pub next_step: NextStep,
}

impl Error {
    pub fn new(kind: ErrorKind) -> Self {
        Error {
            kind,
            params: BTreeMap::new(),
            detail: String::new(),
            next_step: kind.default_next_step(),
        }
    }

    pub fn with(mut self, key: &str, value: impl ToString) -> Self {
        self.params.insert(key.to_owned(), value.to_string());
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn next(mut self, step: NextStep) -> Self {
        self.next_step = step;
        self
    }

    /// Changes the kind and resets the next step to that kind's default.
    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self.next_step = kind.default_next_step();
        self
    }

    /// Shorthand for [`ErrorKind::InvalidInput`] with a `field` parameter.
    pub fn invalid(field: &str, detail: impl Into<String>) -> Self {
        Error::new(ErrorKind::InvalidInput)
            .with("field", field)
            .detail(detail)
    }

    pub fn cancelled() -> Self {
        Error::new(ErrorKind::Cancelled)
    }

    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    pub fn is(&self, kind: ErrorKind) -> bool {
        self.kind == kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)?;
        if !self.params.is_empty() {
            let parts: Vec<String> = self
                .params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            write!(f, " ({})", parts.join(", "))?;
        }
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        Ok(())
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Error::new(kind)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(ErrorKind::Io).detail(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::new(ErrorKind::Parse).detail(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips_through_strings() {
        for kind in [
            ErrorKind::NotEntitled,
            ErrorKind::SshBusy,
            ErrorKind::LoginInvalidCode,
            ErrorKind::AllocationTimeout,
        ] {
            assert_eq!(ErrorKind::from_str_lossy(kind.as_str()), kind);
        }
        assert_eq!(ErrorKind::from_str_lossy("nope"), ErrorKind::Other);
    }

    #[test]
    fn errors_carry_a_next_step() {
        assert_eq!(
            Error::new(ErrorKind::NotEntitled).next_step,
            NextStep::ChooseOtherAccelerator
        );
        assert_eq!(
            Error::new(ErrorKind::AccountNotActive).next_step,
            NextStep::SwitchAccount
        );
        assert_eq!(
            Error::new(ErrorKind::NotInstalled).next_step,
            NextStep::InstallCli
        );
        let e = Error::new(ErrorKind::Other).next(NextStep::RetryLater);
        assert_eq!(e.next_step, NextStep::RetryLater);
        assert_eq!(
            e.with_kind(ErrorKind::AuthRequired).next_step,
            NextStep::SignInAgain
        );
        let json = serde_json::to_value(Error::new(ErrorKind::SshBusy)).unwrap();
        assert_eq!(json["next_step"], "close_other_connection");
        assert_eq!(json["kind"], "ssh_busy");
        for kind in [
            ErrorKind::UnsupportedCliVersion,
            ErrorKind::SessionLimitReached,
            ErrorKind::Busy,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
        }
    }

    #[test]
    fn display_includes_params_and_detail() {
        let e = Error::new(ErrorKind::PortInUse)
            .with("port", 8000)
            .detail("bind failed");
        assert_eq!(e.to_string(), "port_in_use (port=8000): bind failed");
        assert_eq!(e.param("port"), Some("8000"));
    }
}
