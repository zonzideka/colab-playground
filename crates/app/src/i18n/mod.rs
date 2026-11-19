//! The app's message catalog on top of Ely's `I18n` global.
//!
//! Ely's `I18n` holds the app's catalogs and the shown locale. Ely's own components read
//! `ely_gpui_component::i18n::text`, which looks in the app's catalog first and then in Ely's
//! built-in table for the same locale tag, so registering `en-US`, `zh-CN`, `zh-TW`, `ja-JP` and
//! `ko-KR` here, each a tag Ely has a table for, keeps every Ely string translated too. App keys
//! all start with `app.` so they never shadow one of Ely's (`theme.system`, `select.placeholder`,
//! ...). A missing key panics in Ely, so code names keys only through the constants in [`keys`],
//! and the tests check that every key resolves in every locale.

mod en_us;
mod ja_jp;
mod ko_kr;
mod zh_cn;
mod zh_tw;

use ely_gpui_component::i18n::I18n;
use gpui::{App, SharedString};

use crate::settings::LanguagePref;

macro_rules! catalog_keys {
    ($($name:ident => $key:literal,)*) => {
        /// Every key of the app's catalog. Name keys through these constants, never literals.
        pub mod keys {
            $(
                #[doc = concat!("`", $key, "`")]
                pub const $name: &str = $key;
            )*
            /// Every key, in declaration order.
            pub const ALL: &[&str] = &[$($key,)*];
        }
    };
}

catalog_keys! {
    APP_NAME => "app.name",
    NAV_OVERVIEW => "app.nav.overview",
    NAV_SESSIONS => "app.nav.sessions",
    NAV_PLUGINS => "app.nav.plugins",
    NAV_ACCOUNTS => "app.nav.accounts",
    NAV_SETTINGS => "app.nav.settings",
    SHELL_ACCOUNT_SIGNED_OUT => "app.shell.account.signed_out",
    SHELL_ACCOUNT_HINT => "app.shell.account.hint",
    SHELL_USAGE => "app.shell.usage",
    SHELL_USAGE_HINT => "app.shell.usage.hint",
    SHELL_BACKEND_MOCK => "app.shell.backend.mock",
    SHELL_BACKEND_MOCK_HINT => "app.shell.backend.mock.hint",
    OVERVIEW_SUBTITLE => "app.overview.subtitle",
    SESSIONS_SUBTITLE => "app.sessions.subtitle",
    SESSIONS_EMPTY_TITLE => "app.sessions.empty.title",
    SESSIONS_EMPTY_BODY => "app.sessions.empty.body",
    PLUGINS_SUBTITLE => "app.plugins.subtitle",
    PLUGINS_EMPTY_TITLE => "app.plugins.empty.title",
    PLUGINS_EMPTY_BODY => "app.plugins.empty.body",
    ACCOUNTS_SUBTITLE => "app.accounts.subtitle",
    ACCOUNTS_EMPTY_TITLE => "app.accounts.empty.title",
    ACCOUNTS_EMPTY_BODY => "app.accounts.empty.body",
    SETTINGS_SUBTITLE => "app.settings.subtitle",
    SETTINGS_GENERAL_TITLE => "app.settings.general.title",
    SETTINGS_GENERAL_DESCRIPTION => "app.settings.general.description",
    SETTINGS_LANGUAGE_TITLE => "app.settings.language.title",
    SETTINGS_LANGUAGE_DESCRIPTION => "app.settings.language.description",
    SETTINGS_LANGUAGE_SYSTEM => "app.settings.language.system",
    LANG_EN_US => "app.language.en_us",
    LANG_ZH_CN => "app.language.zh_cn",
    LANG_ZH_TW => "app.language.zh_tw",
    LANG_JA_JP => "app.language.ja_jp",
    LANG_KO_KR => "app.language.ko_kr",
    SETTINGS_APPEARANCE_TITLE => "app.settings.appearance.title",
    SETTINGS_APPEARANCE_DESCRIPTION => "app.settings.appearance.description",
    SETTINGS_ABOUT_TITLE => "app.settings.about.title",
    SETTINGS_ABOUT_DESCRIPTION => "app.settings.about.description",
    SETTINGS_VERSION_TITLE => "app.settings.version.title",
    SETTINGS_BACKEND_TITLE => "app.settings.backend.title",
    SETTINGS_BACKEND_DESCRIPTION => "app.settings.backend.description",
    BACKEND_MOCK => "app.backend.mock",
    BACKEND_REAL => "app.backend.real",
    SETTINGS_FILE_TITLE => "app.settings.file.title",
    SETTINGS_FILE_UNSAVED => "app.settings.file.unsaved",
    SETTINGS_FILE_OPEN => "app.settings.file.open",
    TOAST_OPEN_FOLDER_FAILED_TITLE => "app.toast.open_folder_failed.title",
    TOAST_SAVE_FAILED_TITLE => "app.toast.save_failed.title",
    TOAST_SETTINGS_RESET_TITLE => "app.toast.settings_reset.title",
    TOAST_SETTINGS_RESET_BODY => "app.toast.settings_reset.body",
    TOAST_SETTINGS_RESET_BODY_LOST => "app.toast.settings_reset.body_lost",
    TOAST_DEFAULTS_RESET_TITLE => "app.toast.defaults_reset.title",
    TOAST_DEFAULTS_RESET_BODY => "app.toast.defaults_reset.body",
    // Actions
    ACTION_CANCEL => "app.action.cancel",
    ACTION_CLOSE => "app.action.close",
    ACTION_RETRY => "app.action.retry",
    ACTION_REFRESH => "app.action.refresh",
    ACTION_CHECK_AGAIN => "app.action.check_again",
    ACTION_DISMISS => "app.action.dismiss",
    ACTION_INSTALL_CLI => "app.action.install_cli",
    ACTION_SIGN_IN => "app.action.sign_in",
    ACTION_SIGN_IN_AGAIN => "app.action.sign_in_again",
    ACTION_ADD_ACCOUNT => "app.action.add_account",
    ACTION_SWITCH_ACCOUNT => "app.action.switch_account",
    ACTION_INSTALL_WSL => "app.action.install_wsl",
    ACTION_VIEW_SESSIONS => "app.action.view_sessions",
    // Formats
    FMT_CU => "app.fmt.cu",
    FMT_CU_RATE => "app.fmt.cu_rate",
    FMT_DAYS_HOURS => "app.fmt.days_hours",
    FMT_HOURS_MINUTES => "app.fmt.hours_minutes",
    FMT_MINUTES => "app.fmt.minutes",
    FMT_UNDER_MINUTE => "app.fmt.under_minute",
    FMT_JUST_NOW => "app.fmt.just_now",
    FMT_MINUTES_AGO => "app.fmt.minutes_ago",
    FMT_HOURS_AGO => "app.fmt.hours_ago",
    FMT_DAYS_AGO => "app.fmt.days_ago",
    FMT_APPROX => "app.fmt.approx",
    FMT_LIST_SEPARATOR => "app.fmt.list_separator",
    // Usage
    USAGE_BADGE => "app.usage.badge",
    USAGE_OUTDATED => "app.usage.outdated",
    USAGE_TIP_LEFT => "app.usage.tip.left",
    USAGE_TIP_IDLE => "app.usage.tip.idle",
    USAGE_TIP_EMPTY => "app.usage.tip.empty",
    USAGE_TIP_UPDATED => "app.usage.tip.updated",
    USAGE_TIP_STALE => "app.usage.tip.stale",
    USAGE_TIP_OLD => "app.usage.tip.old",
    USAGE_TIP_UNKNOWN => "app.usage.tip.unknown",
    USAGE_REFRESH => "app.usage.refresh",
    // Overview
    CHECKLIST_TITLE => "app.checklist.title",
    CHECKLIST_DESCRIPTION => "app.checklist.description",
    STEP_CHECKING => "app.step.checking",
    STEP_WAITING => "app.step.waiting",
    STEP_WSL_TITLE => "app.step.wsl.title",
    STEP_WSL_DONE => "app.step.wsl.done",
    STEP_WSL_TODO => "app.step.wsl.todo",
    STEP_SHELL_TITLE => "app.step.shell.title",
    STEP_SHELL_DONE => "app.step.shell.done",
    STEP_SHELL_NO_SSH => "app.step.shell.no_ssh",
    STEP_SHELL_TODO => "app.step.shell.todo",
    STEP_CLI_TITLE => "app.step.cli.title",
    STEP_CLI_DONE => "app.step.cli.done",
    STEP_CLI_DONE_UNKNOWN => "app.step.cli.done_unknown",
    STEP_CLI_TODO => "app.step.cli.todo",
    STEP_SIGNIN_TITLE => "app.step.signin.title",
    STEP_SIGNIN_DONE => "app.step.signin.done",
    STEP_SIGNIN_TODO => "app.step.signin.todo",
    STEP_SIGNIN_UNADOPTED => "app.step.signin.unadopted",
    INSTALL_METHOD_LABEL => "app.install.method_label",
    INSTALL_UV => "app.install.uv",
    INSTALL_UV_FIRST => "app.install.uv_first",
    INSTALL_PIP => "app.install.pip",
    INSTALL_START => "app.install.start",
    INSTALL_RUNNING => "app.install.running",
    INSTALL_CANCELLING => "app.install.cancelling",
    INSTALL_DONE => "app.install.done",
    INSTALL_FAILED => "app.install.failed",
    INSTALL_CANCELLED => "app.install.cancelled",
    SUMMARY_TITLE => "app.summary.title",
    SUMMARY_ACTIVE => "app.summary.active",
    SUMMARY_BALANCE => "app.summary.balance",
    SUMMARY_RATE => "app.summary.rate",
    SUMMARY_TIME_LEFT => "app.summary.time_left",
    SUMMARY_RUNTIMES => "app.summary.runtimes",
    SUMMARY_RUNTIMES_ALL => "app.summary.runtimes_all",
    SUMMARY_UNKNOWN => "app.summary.unknown",
    SUMMARY_NOT_BURNING => "app.summary.not_burning",
    SUMMARY_EMPTY => "app.summary.empty",
    UNMANAGED_TITLE => "app.unmanaged.title",
    UNMANAGED_ONE => "app.unmanaged.one",
    UNMANAGED_MANY => "app.unmanaged.many",
    UNMANAGED_LAST_TITLE => "app.unmanaged.last_title",
    UNMANAGED_UNKNOWN_TITLE => "app.unmanaged.unknown_title",
    UNMANAGED_UNKNOWN => "app.unmanaged.unknown",
    UNMANAGED_LAST_ONE => "app.unmanaged.last_one",
    UNMANAGED_LAST_MANY => "app.unmanaged.last_many",
    BACKEND_START_FAILED => "app.backend.start_failed",
    // Accounts
    ACCOUNTS_ACTIVE => "app.accounts.active",
    ACCOUNTS_SWITCH => "app.accounts.switch",
    ACCOUNTS_REMOVE => "app.accounts.remove",
    ACCOUNTS_REMOVE_ACTIVE => "app.accounts.remove_active",
    ACCOUNTS_REMOVE_RUNNING => "app.accounts.remove_running",
    ACCOUNTS_NO_EMAIL => "app.accounts.no_email",
    ACCOUNTS_BALANCE => "app.accounts.balance",
    ACCOUNTS_RATE => "app.accounts.rate",
    ACCOUNTS_TIME_LEFT => "app.accounts.time_left",
    ACCOUNTS_UPDATED => "app.accounts.updated",
    ACCOUNTS_TOTAL_TITLE => "app.accounts.total.title",
    ACCOUNTS_TOTAL_MISSING_ONE => "app.accounts.total.missing_one",
    ACCOUNTS_TOTAL_MISSING_MANY => "app.accounts.total.missing_many",
    ACCOUNTS_USAGE_FAILED => "app.accounts.usage_failed",
    ACCOUNTS_UNADOPTED_TITLE => "app.accounts.unadopted.title",
    ACCOUNTS_UNADOPTED_TITLE_NO_EMAIL => "app.accounts.unadopted.title_no_email",
    ACCOUNTS_UNADOPTED_BODY => "app.accounts.unadopted.body",
    ACCOUNTS_ADOPT => "app.accounts.adopt",
    ACCOUNTS_RECOVERED_TITLE => "app.accounts.recovered.title",
    ACCOUNTS_RECOVERED_BODY => "app.accounts.recovered.body",
    ACCOUNTS_KEYRING_TITLE => "app.accounts.keyring.title",
    ACCOUNTS_KEYRING_BODY => "app.accounts.keyring.body",
    ACCOUNTS_NEEDS_CLI => "app.accounts.needs_cli",
    ACCOUNTS_LOADING => "app.accounts.loading",
    SWITCHER_GROUP => "app.switcher.group",
    SWITCHER_ADD => "app.switcher.add",
    SWITCHER_MANAGE => "app.switcher.manage",
    // Dialogs
    ADD_TITLE => "app.add.title",
    ADD_INTRO => "app.add.intro",
    ADD_IMPACT => "app.add.impact",
    ADD_CONTINUE => "app.add.continue",
    ADD_STARTING => "app.add.starting",
    ADD_OPENED => "app.add.opened",
    ADD_OPEN_AGAIN => "app.add.open_again",
    ADD_CODE_LABEL => "app.add.code_label",
    ADD_CODE_PLACEHOLDER => "app.add.code_placeholder",
    ADD_SUBMIT => "app.add.submit",
    ADD_SUBMITTING => "app.add.submitting",
    ADD_START_AGAIN => "app.add.start_again",
    ADD_DONE => "app.add.done",
    ADD_ROLLED_BACK => "app.add.rolled_back",
    SWITCH_TITLE => "app.switch.title",
    SWITCH_LOADING => "app.switch.loading",
    SWITCH_NOTHING => "app.switch.nothing",
    SWITCH_TUNNELS => "app.switch.tunnels",
    SWITCH_TUNNEL => "app.switch.tunnel",
    SWITCH_SESSIONS => "app.switch.sessions",
    SWITCH_SESSION => "app.switch.session",
    SWITCH_SESSION_DEADLINE => "app.switch.session_deadline",
    SWITCH_CONFIRM => "app.switch.confirm",
    SWITCH_DONE => "app.switch.done",
    REMOVE_TITLE => "app.remove.title",
    REMOVE_BODY => "app.remove.body",
    REMOVE_CONFIRM => "app.remove.confirm",
    REMOVE_DONE => "app.remove.done",
    ADOPT_DONE => "app.adopt.done",
    // Settings: defaults and host
    DEFAULTS_TITLE => "app.defaults.title",
    DEFAULTS_DESCRIPTION => "app.defaults.description",
    DEF_LIMIT => "app.def.limit",
    DEF_NO_LIMIT => "app.def.no_limit",
    DEF_MAX_RUNTIME => "app.def.max_runtime",
    DEF_MAX_RUNTIME_HINT => "app.def.max_runtime_hint",
    DEF_MAX_SPEND => "app.def.max_spend",
    DEF_MAX_SPEND_HINT => "app.def.max_spend_hint",
    DEF_MIN_BALANCE => "app.def.min_balance",
    DEF_MIN_BALANCE_HINT => "app.def.min_balance_hint",
    DEF_IDLE => "app.def.idle",
    DEF_IDLE_HINT => "app.def.idle_hint",
    DEF_CONCURRENT => "app.def.concurrent",
    DEF_CONCURRENT_HINT => "app.def.concurrent_hint",
    DEF_ATTEMPTS => "app.def.attempts",
    DEF_ATTEMPTS_HINT => "app.def.attempts_hint",
    DEF_RETRY_DELAY => "app.def.retry_delay",
    DEF_ON_EXHAUSTED => "app.def.on_exhausted",
    DEF_EXHAUSTED_STOP => "app.def.exhausted_stop",
    DEF_EXHAUSTED_KEEP => "app.def.exhausted_keep",
    DEF_POLL_ACTIVE => "app.def.poll_active",
    DEF_POLL_IDLE => "app.def.poll_idle",
    DEF_POLL_HINT => "app.def.poll_hint",
    DEF_ACCELERATOR => "app.def.accelerator",
    DEF_HIGH_MEM => "app.def.high_mem",
    DEF_MIN_VRAM => "app.def.min_vram",
    DEF_MIN_VRAM_HINT => "app.def.min_vram_hint",
    DEF_WINDDOWN => "app.def.winddown",
    DEF_WINDDOWN_HINT => "app.def.winddown_hint",
    DEF_RESET => "app.def.reset",
    UNIT_GB => "app.unit.gb",
    UNIT_CU => "app.unit.cu",
    ACC_NONE => "app.acc.none",
    ACC_T4 => "app.acc.t4",
    ACC_L4 => "app.acc.l4",
    ACC_G4 => "app.acc.g4",
    ACC_A100 => "app.acc.a100",
    ACC_H100 => "app.acc.h100",
    ACC_V5E1 => "app.acc.v5e1",
    ACC_V6E1 => "app.acc.v6e1",
    HOST_TITLE => "app.host.title",
    HOST_DESCRIPTION => "app.host.description",
    HOST_WSL => "app.host.wsl",
    HOST_WSL_DEFAULT => "app.host.wsl_default",
    HOST_COLAB_PATH => "app.host.colab_path",
    HOST_PYTHON_PATH => "app.host.python_path",
    HOST_PATH_PLACEHOLDER => "app.host.path_placeholder",
    HOST_APPLY => "app.host.apply",
    HOST_BOTH_NEEDED => "app.host.both_needed",
    HOST_MOCK_NOTE => "app.host.mock_note",
    HOST_BUSY => "app.host.busy",
    // Errors (one per core ErrorKind) and next steps
    ERR_UNKNOWN => "app.err.unknown",
    ERR_DETAILS => "app.err.details",
    ERR_NOT_INSTALLED => "app.err.not_installed",
    ERR_AUTH_REQUIRED => "app.err.auth_required",
    ERR_NOT_ENTITLED => "app.err.not_entitled",
    ERR_CAPACITY_REFUSED => "app.err.capacity_refused",
    ERR_ALLOCATION_TIMEOUT => "app.err.allocation_timeout",
    ERR_SSH_BUSY => "app.err.ssh_busy",
    ERR_SESSION_GONE => "app.err.session_gone",
    ERR_CONNECTION_LOST => "app.err.connection_lost",
    ERR_EXEC_TIMEOUT => "app.err.exec_timeout",
    ERR_PORT_IN_USE => "app.err.port_in_use",
    ERR_PORT_RESERVED => "app.err.port_reserved",
    ERR_INVALID_INPUT => "app.err.invalid_input",
    ERR_NO_ACTIVE_ACCOUNT => "app.err.no_active_account",
    ERR_ACTIVE_NOT_ADOPTED => "app.err.active_not_adopted",
    ERR_ACCOUNT_NOT_ACTIVE => "app.err.account_not_active",
    ERR_ACCOUNT_NOT_FOUND => "app.err.account_not_found",
    ERR_KEYRING_UNAVAILABLE => "app.err.keyring_unavailable",
    ERR_ADD_IN_PROGRESS => "app.err.add_in_progress",
    ERR_LEGACY_TOKEN => "app.err.legacy_token",
    ERR_LOGIN_CANCELLED => "app.err.login_cancelled",
    ERR_LOGIN_INVALID_CODE => "app.err.login_invalid_code",
    ERR_LOGIN_UNVERIFIED => "app.err.login_unverified",
    ERR_CANCELLED => "app.err.cancelled",
    ERR_UNSUPPORTED => "app.err.unsupported",
    ERR_WSL_UNAVAILABLE => "app.err.wsl_unavailable",
    ERR_COMMAND_TOO_LONG => "app.err.command_too_long",
    ERR_SESSION_LIMIT => "app.err.session_limit",
    ERR_BUSY => "app.err.busy",
    ERR_PROVISION_EXHAUSTED => "app.err.provision_exhausted",
    ERR_ORPHAN_SUSPECTED => "app.err.orphan_suspected",
    ERR_STOP_UNCONFIRMED => "app.err.stop_unconfirmed",
    ERR_MANIFEST_INVALID => "app.err.manifest_invalid",
    ERR_PLUGIN_EXISTS => "app.err.plugin_exists",
    ERR_PLUGIN_NOT_FOUND => "app.err.plugin_not_found",
    ERR_PLUGIN_RUN_FAILED => "app.err.plugin_run_failed",
    ERR_HEALTH_CHECK_FAILED => "app.err.health_check_failed",
    ERR_GIT_FAILED => "app.err.git_failed",
    ERR_INSTALL_FAILED => "app.err.install_failed",
    ERR_HTTP => "app.err.http",
    ERR_HTTP_UNAVAILABLE => "app.err.http_unavailable",
    ERR_PARSE => "app.err.parse",
    ERR_IO => "app.err.io",
    ERR_TIMEOUT => "app.err.timeout",
    ERR_UNSUPPORTED_CLI => "app.err.unsupported_cli",
    ERR_OTHER => "app.err.other",
    NS_TRY_AGAIN => "app.ns.try_again",
    NS_RETRY_LATER => "app.ns.retry_later",
    NS_SWITCH_ACCOUNT => "app.ns.switch_account",
    NS_ADD_ACCOUNT => "app.ns.add_account",
    NS_SIGN_IN_AGAIN => "app.ns.sign_in_again",
    NS_UNLOCK_KEYRING => "app.ns.unlock_keyring",
    NS_OTHER_ACCELERATOR => "app.ns.other_accelerator",
    NS_STOP_A_SESSION => "app.ns.stop_a_session",
    NS_OTHER_PORT => "app.ns.other_port",
    NS_CLOSE_CONNECTION => "app.ns.close_connection",
    NS_INSTALL_CLI => "app.ns.install_cli",
    NS_INSTALL_WSL => "app.ns.install_wsl",
    NS_UPDATE_APP => "app.ns.update_app",
    NS_CHECK_INPUT => "app.ns.check_input",
    NS_CHECK_NETWORK => "app.ns.check_network",
    NS_REFRESH => "app.ns.refresh",
    NS_REVIEW_RUNTIMES => "app.ns.review_runtimes",
    NS_VIEW_LOG => "app.ns.view_log",
    NS_REPORT_PROBLEM => "app.ns.report_problem",
    ACCOUNT_CHANGE_BLOCKED => "app.b2.account_change_blocked",
    ACTION_BACK => "app.b2.action_back",
    ACTION_CLEAR => "app.b2.action_clear",
    ACTION_DELETE => "app.b2.action_delete",
    ACTION_HIDE => "app.b2.action_hide",
    ACTION_OPEN_BROWSER => "app.b2.action_open_browser",
    ACTION_REMOVE => "app.b2.action_remove",
    ACTION_SAVE => "app.b2.action_save",
    ACTION_SHOW => "app.b2.action_show",
    AUTO_STOP_CANCELLED => "app.b2.auto_stop_cancelled",
    DEPLOY_ACTION => "app.b2.deploy_action",
    DEPLOY_SUBTITLE => "app.b2.deploy_subtitle",
    DEPLOY_TITLE => "app.b2.deploy_title",
    ENDED_GUARD => "app.b2.ended_guard",
    ENDED_LINE => "app.b2.ended_line",
    ENDED_TITLE => "app.b2.ended_title",
    ENDED_USER => "app.b2.ended_user",
    ENDED_VANISHED => "app.b2.ended_vanished",
    ENDPOINT_CURL => "app.b2.endpoint_curl",
    ENDPOINT_CURL_KEY => "app.b2.endpoint_curl_key",
    ENDPOINT_FORWARD => "app.b2.endpoint_forward",
    ENDPOINT_HEALTHY => "app.b2.endpoint_healthy",
    ENDPOINT_NOT_FORWARDED => "app.b2.endpoint_not_forwarded",
    ENDPOINT_UNHEALTHY => "app.b2.endpoint_unhealthy",
    EXTEND_DONE => "app.b2.extend_done",
    FMT_GB => "app.b2.fmt_gb",
    FMT_MB => "app.b2.fmt_mb",
    FMT_KB => "app.b2.fmt_kb",
    FMT_SECONDS => "app.b2.fmt_seconds",
    FORWARD_ACTION => "app.b2.forward_action",
    FORWARD_BILLING_NOTE => "app.b2.forward_billing_note",
    FORWARD_CLOSE => "app.b2.forward_close",
    FORWARD_CLOSED => "app.b2.forward_closed",
    FORWARD_CLOSED_BILLING => "app.b2.forward_closed_billing",
    FORWARD_HEALTH => "app.b2.forward_health",
    FORWARD_HEALTH_HINT => "app.b2.forward_health_hint",
    FORWARD_HEALTH_PLACEHOLDER => "app.b2.forward_health_placeholder",
    FORWARD_INTRO => "app.b2.forward_intro",
    FORWARD_LOCAL => "app.b2.forward_local",
    FORWARD_REMOTE => "app.b2.forward_remote",
    FORWARD_TITLE => "app.b2.forward_title",
    GUARD_BALANCE => "app.b2.guard_balance",
    GUARD_ROUND_FAILED => "app.b2.guard_round_failed",
    GUARD_RUNTIME => "app.b2.guard_runtime",
    GUARD_SPEND => "app.b2.guard_spend",
    GUARD_STILL_BILLING => "app.b2.guard_still_billing",
    GUARD_STOPPED_TITLE => "app.b2.guard_stopped_title",
    GUARD_STOP_FAILED_TITLE => "app.b2.guard_stop_failed_title",
    GUARD_UNKNOWN => "app.b2.guard_unknown",
    GUARD_WARN_LINE => "app.b2.guard_warn_line",
    GUARD_WARN_TITLE => "app.b2.guard_warn_title",
    HF_LISTING => "app.b2.hf_listing",
    HF_LIST_FILES => "app.b2.hf_list_files",
    HF_NO_RESULTS => "app.b2.hf_no_results",
    HF_RESULT => "app.b2.hf_result",
    HF_RESULT_GATED => "app.b2.hf_result_gated",
    HF_SEARCH => "app.b2.hf_search",
    HF_SEARCHING => "app.b2.hf_searching",
    HF_SEARCH_ACTION => "app.b2.hf_search_action",
    HF_SEARCH_PLACEHOLDER => "app.b2.hf_search_placeholder",
    HF_TOKEN_DELETED => "app.b2.hf_token_deleted",
    HF_TOKEN_FIELD => "app.b2.hf_token_field",
    HF_TOKEN_HINT => "app.b2.hf_token_hint",
    HF_TOKEN_NONE => "app.b2.hf_token_none",
    HF_TOKEN_PLACEHOLDER => "app.b2.hf_token_placeholder",
    HF_TOKEN_SAVED => "app.b2.hf_token_saved",
    HF_TOKEN_STORED => "app.b2.hf_token_stored",
    HF_TOKEN_TITLE => "app.b2.hf_token_title",
    HF_TOKEN_UNKNOWN => "app.b2.hf_token_unknown",
    LIMIT_BALANCE => "app.b2.limit_balance",
    LIMIT_RUNTIME => "app.b2.limit_runtime",
    LIMIT_SPEND => "app.b2.limit_spend",
    NEW_CANCELLED => "app.b2.new_cancelled",
    NEW_CANCELLING => "app.b2.new_cancelling",
    NEW_CANCEL_NOTE => "app.b2.new_cancel_note",
    NEW_CHANGE => "app.b2.new_change",
    NEW_FAILED => "app.b2.new_failed",
    NEW_INTRO => "app.b2.new_intro",
    NEW_NAME => "app.b2.new_name",
    NEW_NAME_PLACEHOLDER => "app.b2.new_name_placeholder",
    NEW_READY => "app.b2.new_ready",
    NEW_SHOW_SESSIONS => "app.b2.new_show_sessions",
    NEW_START => "app.b2.new_start",
    NEW_TITLE => "app.b2.new_title",
    PLUGINS_LOADING => "app.b2.plugins_loading",
    PLUGIN_ASKS => "app.b2.plugin_asks",
    PLUGIN_BUILT_IN => "app.b2.plugin_built_in",
    PLUGIN_BUILT_IN_REMOVE => "app.b2.plugin_built_in_remove",
    PLUGIN_CHECK => "app.b2.plugin_check",
    PLUGIN_CHECKING => "app.b2.plugin_checking",
    PLUGIN_ENABLED => "app.b2.plugin_enabled",
    PLUGIN_FROM => "app.b2.plugin_from",
    PLUGIN_INPUT_SECRET => "app.b2.plugin_input_secret",
    PLUGIN_INSTALLED => "app.b2.plugin_installed",
    PLUGIN_INSTALL_ACTION => "app.b2.plugin_install_action",
    PLUGIN_INSTALL_HINT => "app.b2.plugin_install_hint",
    PLUGIN_INSTALL_TITLE => "app.b2.plugin_install_title",
    PLUGIN_NEEDS_HF_TOKEN => "app.b2.plugin_needs_hf_token",
    PLUGIN_OFF => "app.b2.plugin_off",
    PLUGIN_OFFERS => "app.b2.plugin_offers",
    PLUGIN_OPEN => "app.b2.plugin_open",
    PLUGIN_RECEIVES => "app.b2.plugin_receives",
    PLUGIN_RECEIVES_NOTHING => "app.b2.plugin_receives_nothing",
    PLUGIN_REMOVED => "app.b2.plugin_removed",
    PLUGIN_REMOVE_BODY => "app.b2.plugin_remove_body",
    PLUGIN_REMOVE_TITLE => "app.b2.plugin_remove_title",
    PLUGIN_REPLACES => "app.b2.plugin_replaces",
    PLUGIN_RUNS_ON_RUNTIME => "app.b2.plugin_runs_on_runtime",
    PLUGIN_SCRIPT => "app.b2.plugin_script",
    PLUGIN_SCRIPT_VALUE => "app.b2.plugin_script_value",
    PLUGIN_SOURCE => "app.b2.plugin_source",
    PLUGIN_SOURCE_PLACEHOLDER => "app.b2.plugin_source_placeholder",
    PLUGIN_VERSION => "app.b2.plugin_version",
    PLUGIN_VERSION_SHORT => "app.b2.plugin_version_short",
    PROVISION_READY_TOAST => "app.b2.provision_ready_toast",
    PROV_ALLOCATING => "app.b2.prov_allocating",
    PROV_ALLOCATION_FAILED => "app.b2.prov_allocation_failed",
    PROV_ATTEMPT => "app.b2.prov_attempt",
    PROV_ATTEMPTS_BILLED => "app.b2.prov_attempts_billed",
    PROV_FAILED => "app.b2.prov_failed",
    PROV_NO_GPU => "app.b2.prov_no_gpu",
    PROV_ORPHAN_LEFT => "app.b2.prov_orphan_left",
    PROV_ORPHAN_RELEASED => "app.b2.prov_orphan_released",
    PROV_PROBED => "app.b2.prov_probed",
    PROV_READY => "app.b2.prov_ready",
    PROV_READY_BELOW => "app.b2.prov_ready_below",
    PROV_REJECTED => "app.b2.prov_rejected",
    PROV_RELEASING => "app.b2.prov_releasing",
    PROV_RETRYING => "app.b2.prov_retrying",
    PROV_STARTING => "app.b2.prov_starting",
    PROV_VRAM_LOTTERY => "app.b2.prov_vram_lottery",
    PROV_WATCHDOG => "app.b2.prov_watchdog",
    PROV_WATCHDOG_FAILED => "app.b2.prov_watchdog_failed",
    PROV_WATCHDOG_NO_DEADLINE => "app.b2.prov_watchdog_no_deadline",
    QUANT_ESTIMATE_NOTE => "app.b2.quant_estimate_note",
    QUANT_FILES => "app.b2.quant_files",
    QUANT_FITS => "app.b2.quant_fits",
    QUANT_NOTHING_FITS => "app.b2.quant_nothing_fits",
    QUANT_NO_GPU => "app.b2.quant_no_gpu",
    QUIT_LEAVE => "app.b2.quit_leave",
    QUIT_LEAVE_EXPLAINED => "app.b2.quit_leave_explained",
    QUIT_PROVISIONING => "app.b2.quit_provisioning",
    QUIT_RUNNING_PLUGIN => "app.b2.quit_running_plugin",
    QUIT_STOPPING => "app.b2.quit_stopping",
    QUIT_STOP_ALL => "app.b2.quit_stop_all",
    QUIT_STOP_FAILED => "app.b2.quit_stop_failed",
    QUIT_TITLE => "app.b2.quit_title",
    QUIT_UNKNOWN => "app.b2.quit_unknown",
    QUIT_UNWATCHED_MANY => "app.b2.quit_unwatched_many",
    QUIT_UNWATCHED_ONE => "app.b2.quit_unwatched_one",
    QUIT_WATCHED_MANY => "app.b2.quit_watched_many",
    QUIT_WATCHED_ONE => "app.b2.quit_watched_one",
    RUNTIME_AUTO_STOP => "app.b2.runtime_auto_stop",
    RUNTIME_AUTO_STOP_IN => "app.b2.runtime_auto_stop_in",
    RUNTIME_AUTO_STOP_NONE => "app.b2.runtime_auto_stop_none",
    RUNTIME_AUTO_STOP_OFF => "app.b2.runtime_auto_stop_off",
    RUNTIME_CANCEL_AUTO_STOP => "app.b2.runtime_cancel_auto_stop",
    RUNTIME_EXTEND_BY => "app.b2.runtime_extend_by",
    RUNTIME_FORWARDS => "app.b2.runtime_forwards",
    RUNTIME_HARDWARE => "app.b2.runtime_hardware",
    RUNTIME_HIGH_MEM => "app.b2.runtime_high_mem",
    RUNTIME_RATE => "app.b2.runtime_rate",
    RUNTIME_SPEND => "app.b2.runtime_spend",
    RUNTIME_STOPPING => "app.b2.runtime_stopping",
    RUNTIME_SWITCH => "app.b2.runtime_switch",
    RUNTIME_SWITCH_HINT => "app.b2.runtime_switch_hint",
    RUNTIME_UNMANAGED => "app.b2.runtime_unmanaged",
    RUNTIME_UNMANAGED_HINT => "app.b2.runtime_unmanaged_hint",
    RUNTIME_UNNAMED_REASON => "app.b2.runtime_unnamed_reason",
    RUNTIME_UPTIME => "app.b2.runtime_uptime",
    RUN_AGAIN => "app.b2.run_again",
    RUN_BANNER_CANCELLED => "app.b2.run_banner_cancelled",
    RUN_BANNER_FAILED => "app.b2.run_banner_failed",
    RUN_BANNER_READY => "app.b2.run_banner_ready",
    RUN_BANNER_RUNNING => "app.b2.run_banner_running",
    RUN_CANCELLED => "app.b2.run_cancelled",
    RUN_CANCEL_NOTE_EXISTING => "app.b2.run_cancel_note_existing",
    RUN_CANCEL_NOTE_NEW => "app.b2.run_cancel_note_new",
    RUN_EXISTING_NOTE => "app.b2.run_existing_note",
    RUN_FAILED => "app.b2.run_failed",
    RUN_HF_TOKEN_NONE => "app.b2.run_hf_token_none",
    RUN_HF_TOKEN_SENT => "app.b2.run_hf_token_sent",
    RUN_INPUTS => "app.b2.run_inputs",
    RUN_LOG => "app.b2.run_log",
    RUN_NO_SESSION => "app.b2.run_no_session",
    RUN_ON_SESSION => "app.b2.run_on_session",
    RUN_OUTPUTS => "app.b2.run_outputs",
    RUN_PROVISIONING => "app.b2.run_provisioning",
    RUN_READY => "app.b2.run_ready",
    RUN_READY_TOAST => "app.b2.run_ready_toast",
    RUN_SECRET_NOTE => "app.b2.run_secret_note",
    RUN_SESSION_BILLING => "app.b2.run_session_billing",
    RUN_SESSION_STOPPED => "app.b2.run_session_stopped",
    RUN_CHECKING => "app.b2.run_checking",
    RUN_SESSION_UNKNOWN => "app.b2.run_session_unknown",
    RUN_STAGES => "app.b2.run_stages",
    RUN_START => "app.b2.run_start",
    RUN_START_NOTE => "app.b2.run_start_note",
    RUN_STOP_ON_FAILURE => "app.b2.run_stop_on_failure",
    RUN_STOP_ON_FAILURE_OFF => "app.b2.run_stop_on_failure_off",
    RUN_STOP_ON_FAILURE_ON => "app.b2.run_stop_on_failure_on",
    RUN_SUBTITLE => "app.b2.run_subtitle",
    RUN_TARGET => "app.b2.run_target",
    RUN_TARGET_HINT => "app.b2.run_target_hint",
    RUN_TARGET_NEW => "app.b2.run_target_new",
    SESSIONS_ACCOUNT_FAILED => "app.b2.sessions_account_failed",
    SESSIONS_COUNT_MANY => "app.b2.sessions_count_many",
    SESSIONS_COUNT_ONE => "app.b2.sessions_count_one",
    SESSIONS_FAILED => "app.b2.sessions_failed",
    SESSIONS_GROUP => "app.b2.sessions_group",
    SESSIONS_GROUP_ACTIVE => "app.b2.sessions_group_active",
    SESSIONS_LOADING => "app.b2.sessions_loading",
    SESSIONS_REFRESH => "app.b2.sessions_refresh",
    SESSIONS_SETUP_ACTION => "app.b2.sessions_setup_action",
    SESSIONS_SETUP_BODY => "app.b2.sessions_setup_body",
    SESSIONS_SETUP_TITLE => "app.b2.sessions_setup_title",
    SESSIONS_STARTING => "app.b2.sessions_starting",
    STOP_ACTION => "app.b2.stop_action",
    STOP_BODY => "app.b2.stop_body",
    STOP_DONE => "app.b2.stop_done",
    STOP_TITLE => "app.b2.stop_title",
    STOP_WIPED => "app.b2.stop_wiped",
    TUNNEL_FAILED => "app.b2.tunnel_failed",
    TUNNEL_RECONNECTING => "app.b2.tunnel_reconnecting",
    TUNNEL_RECONNECTING_SHORT => "app.b2.tunnel_reconnecting_short",
    TUNNEL_STARTING => "app.b2.tunnel_starting",
    TUNNEL_STOPPED => "app.b2.tunnel_stopped",
    TUNNEL_UP => "app.b2.tunnel_up",
    TUNNEL_UP_UNHEALTHY => "app.b2.tunnel_up_unhealthy",
    VANISHED_TITLE => "app.b2.vanished_title",
    AUTO_STOP_SET => "app.b3.auto_stop_set",
    CANCEL_AUTO_STOP_BODY => "app.b3.cancel_auto_stop_body",
    CANCEL_AUTO_STOP_KEEP => "app.b3.cancel_auto_stop_keep",
    CANCEL_AUTO_STOP_REARM => "app.b3.cancel_auto_stop_rearm",
    CANCEL_AUTO_STOP_TITLE => "app.b3.cancel_auto_stop_title",
    FMT_DAYS => "app.b3.fmt_days",
    FMT_HOURS => "app.b3.fmt_hours",
    HOST_PYTHON_HINT => "app.b3.host_python_hint",
    LIMIT_FOCUS_HINT => "app.b3.limit_focus_hint",
    MENU_QUIT => "app.b3.menu_quit",
    NEW_LIMIT_BODY => "app.b3.new_limit_body",
    NEW_LIMIT_COUNTED => "app.b3.new_limit_counted",
    NEW_LIMIT_HINT => "app.b3.new_limit_hint",
    NEW_OPEN_SETTINGS => "app.b3.new_open_settings",
    PROV_ALLOCATED => "app.b3.prov_allocated",
    PROV_RELEASED => "app.b3.prov_released",
    PROV_RETRIED => "app.b3.prov_retried",
    QUANT_FITS_ASSUMED => "app.b3.quant_fits_assumed",
    QUIT_STOP_WIPED => "app.b3.quit_stop_wiped",
    RUNTIME_SET_AUTO_STOP => "app.b3.runtime_set_auto_stop",
    SESSIONS_ESTIMATE_NOTE => "app.b3.sessions_estimate_note",
    RUNTIME_AUTO_STOP_APP_ONLY => "app.b5.runtime_auto_stop_app_only",
    RUN_LIMIT_HINT => "app.b3.run_limit_hint",
    SWITCH_UNMANAGED => "app.b3.switch_unmanaged",
    SWITCH_UNMANAGED_ITEM => "app.b3.switch_unmanaged_item",
    UNIT_HOURS => "app.b3.unit_hours",
    UNIT_MINUTES => "app.b3.unit_minutes",
    UNIT_SECONDS => "app.b3.unit_seconds",
    ERR_RATE_LIMITED => "app.b7.err_rate_limited",
    UPDATE_AUTO => "app.b7.update_auto",
    UPDATE_AUTO_HINT => "app.b7.update_auto_hint",
    UPDATE_AVAILABLE => "app.b7.update_available",
    UPDATE_BADGE => "app.b7.update_badge",
    UPDATE_CHECK => "app.b7.update_check",
    UPDATE_CHECKING => "app.b7.update_checking",
    UPDATE_DISMISS => "app.b7.update_dismiss",
    UPDATE_FAILED => "app.b7.update_failed",
    UPDATE_FAILED_TITLE => "app.b7.update_failed_title",
    UPDATE_NOTICE_BODY => "app.b7.update_notice_body",
    UPDATE_NOTICE_TITLE => "app.b7.update_notice_title",
    UPDATE_NOT_CHECKED => "app.b7.update_not_checked",
    UPDATE_OPEN_PAGE => "app.b7.update_open_page",
    UPDATE_TITLE => "app.b7.update_title",
    UPDATE_TRY_AGAIN => "app.b7.update_try_again",
    UPDATE_UP_TO_DATE => "app.b7.update_up_to_date",
    SETTINGS_LICENSE_TITLE => "app.b8.settings_license_title",
    SETTINGS_PROJECT_TITLE => "app.b8.settings_project_title",
    QUIT_TITLE_CHECKING => "app.b9.quit_title_checking",
    QUIT_CHECKING => "app.b9.quit_checking",
    QUIT_TITLE_UNCHECKED => "app.b9.quit_title_unchecked",
    QUIT_UNCHECKED => "app.b9.quit_unchecked",
    QUIT_STOP_ALL_FAILED => "app.b9.quit_stop_all_failed",
    SUMMARY_INCOMPLETE => "app.b10.summary_incomplete",
    SUMMARY_NOT_COUNTED => "app.b10.summary_not_counted",
    SESSIONS_EMPTY_PARTIAL_TITLE => "app.b10.sessions_empty_partial_title",
    SWITCH_UNMANAGED_UNKNOWN => "app.b10.switch_unmanaged_unknown",
    PROV_RECONCILING => "app.b11.prov_reconciling",
    PROV_RECONCILED => "app.b11.prov_reconciled",
    PROV_NO_ANSWER => "app.b11.prov_no_answer",
}

/// A message table: `(key, message)` pairs; `{name}` marks an argument.
pub type Messages = &'static [(&'static str, &'static str)];

/// The app's catalogs, `en-US` first (Ely checks every later catalog against the first).
pub const CATALOGS: [(&str, Messages); 5] = [
    ("en-US", en_us::MESSAGES),
    ("zh-CN", zh_cn::MESSAGES),
    ("zh-TW", zh_tw::MESSAGES),
    ("ja-JP", ja_jp::MESSAGES),
    ("ko-KR", ko_kr::MESSAGES),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    EnUs,
    ZhCn,
    /// Traditional Chinese, Taiwan wording.
    ZhTw,
    JaJp,
    KoKr,
}

impl Language {
    /// Every language, in catalog order.
    pub const ALL: [Language; 5] = [
        Language::EnUs,
        Language::ZhCn,
        Language::ZhTw,
        Language::JaJp,
        Language::KoKr,
    ];

    /// The BCP 47 tag Ely's `I18n` knows it by.
    pub fn tag(self) -> &'static str {
        match self {
            Language::EnUs => "en-US",
            Language::ZhCn => "zh-CN",
            Language::ZhTw => "zh-TW",
            Language::JaJp => "ja-JP",
            Language::KoKr => "ko-KR",
        }
    }

    /// The key of its name in its own language.
    pub fn name_key(self) -> &'static str {
        match self {
            Language::EnUs => keys::LANG_EN_US,
            Language::ZhCn => keys::LANG_ZH_CN,
            Language::ZhTw => keys::LANG_ZH_TW,
            Language::JaJp => keys::LANG_JA_JP,
            Language::KoKr => keys::LANG_KO_KR,
        }
    }

    /// The language for an OS locale such as `zh-Hant-TW`, `zh_HK.UTF-8`, `ja_JP.UTF-8` or
    /// `en-GB`: Japanese for any `ja` locale and Korean for any `ko` one; Traditional Chinese for
    /// a `Hant` script or a TW/HK/MO region, Simplified for any other Chinese; English otherwise.
    /// An explicit script wins over the region (`zh-Hans-HK` is Simplified).
    pub fn from_os_locale(locale: &str) -> Language {
        let tag = locale
            .split(['.', '@'])
            .next()
            .unwrap_or_default()
            .replace('_', "-");
        let mut parts = tag.split('-').filter(|part| !part.is_empty());
        let language = parts.next().unwrap_or_default().to_ascii_lowercase();
        match language.as_str() {
            "ja" => return Language::JaJp,
            "ko" => return Language::KoKr,
            "zh" => {}
            _ => return Language::EnUs,
        }
        let (mut script, mut region) = (None, None);
        for part in parts {
            if part.len() == 4 && part.chars().all(|c| c.is_ascii_alphabetic()) {
                script.get_or_insert(part.to_ascii_lowercase());
            } else if part.len() == 2
                || (part.len() == 3 && part.chars().all(|c| c.is_ascii_digit()))
            {
                region.get_or_insert(part.to_ascii_uppercase());
            }
        }
        match script.as_deref() {
            Some("hant") => Language::ZhTw,
            Some("hans") => Language::ZhCn,
            _ => match region.as_deref() {
                Some("TW" | "HK" | "MO") => Language::ZhTw,
                _ => Language::ZhCn,
            },
        }
    }
}

/// The language a preference shows, given the OS locale (`None` when it is unknown).
pub fn resolve(pref: LanguagePref, os_locale: Option<&str>) -> Language {
    match pref {
        LanguagePref::System => os_locale.map_or(Language::EnUs, Language::from_os_locale),
        LanguagePref::EnUs => Language::EnUs,
        LanguagePref::ZhCn => Language::ZhCn,
        LanguagePref::ZhTw => Language::ZhTw,
        LanguagePref::JaJp => Language::JaJp,
        LanguagePref::KoKr => Language::KoKr,
    }
}

pub fn catalog(language: Language) -> I18n {
    CATALOGS
        .iter()
        .fold(I18n::new(language.tag()), |i18n, (tag, messages)| {
            i18n.catalog(tag, messages)
        })
}

/// Installs the catalog as the app's `I18n` global. Call after Ely's `init`.
pub fn install(language: Language, cx: &mut App) {
    log::info!("i18n: showing {}", language.tag());
    cx.set_global(catalog(language));
}

/// Shows `language` in every window (Ely's strings included).
pub fn set_language(language: Language, cx: &mut App) {
    I18n::set_locale(language.tag(), cx);
}

pub fn current(cx: &App) -> Language {
    let tag = cx.global::<I18n>().locale().tag;
    Language::ALL
        .into_iter()
        .find(|language| language.tag() == tag)
        .unwrap_or_else(|| panic!("i18n: the app has no catalog for {tag}"))
}

pub fn tr(cx: &App, key: &str) -> SharedString {
    cx.global::<I18n>().text(key, &[])
}

pub fn tr_args(cx: &App, key: &str, args: &[(&str, &str)]) -> SharedString {
    cx.global::<I18n>().text(key, args)
}

/// The raw message for `key` in the shown language, `{name}` placeholders unfilled.
pub fn template(cx: &App, key: &str) -> &'static str {
    let tag = current(cx).tag();
    CATALOGS
        .iter()
        .find(|(each, _)| *each == tag)
        .and_then(|(_, messages)| messages.iter().find(|(k, _)| *k == key))
        .map(|(_, message)| *message)
        .unwrap_or_else(|| panic!("i18n: no message {key:?} in {tag}"))
}

/// The message for `key` with `{name}` filled from `args`; a placeholder with no argument is
/// left as written instead of panicking (for values that are only sometimes known, such as
/// an error's parameters).
pub fn tr_lenient(cx: &App, key: &str, args: &[(&str, &str)]) -> SharedString {
    fill_lenient(template(cx, key), args).into()
}

pub fn fill_lenient(message: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &after[..close];
        match args.iter().find(|(arg, _)| *arg == name) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[open..open + close + 2]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests;
