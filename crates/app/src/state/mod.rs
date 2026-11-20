//! The app's root state: one entity, published as a global.
//!
//! Views read [`AppState`] in `render` and change it only through its methods; each method
//! calls `cx.notify()` so observing views re-render. Every backend call goes through
//! `AppState::run_backend` (see `ops`): it runs the blocking call on the background executor
//! with a `CancelToken`, and applies the result on the foreground.

mod accounts;
mod defaults;
mod environment;
mod ops;
mod plugins;
mod provision;
mod quit;
mod runs;
mod sessions;
mod tunnels;
mod updates;
mod usage;

use std::sync::Arc;

use colab_playground_core::{
    Error, ErrorKind, accounts::AccountsView, backend::Backend, backend::BackendKind as Flavor,
    backend::LoginFlow, host::ProbeReport,
};
use ely_gpui_component::{
    feedback::{Toast, Toaster},
    primitives::Severity,
    settings::Appearance,
    theme::{ActiveTheme, Mode, Theme},
};
use gpui::{App, AppContext, Context, Entity, FontFallbacks, Global, Task};

pub use accounts::{
    AccountDialog, AccountRow, AddPhase, RemoveBlock, SignInStatus, account_row, sign_in_status,
};
pub use defaults::{MIN_DURATION, sanitize};
pub use environment::{
    Checklist, EnvironmentStep, InstallPhase, InstallState, Step, StepState, checklist,
    install_methods,
};
pub use ops::{Op, key_of};
pub use plugins::{HF_TOKEN, InstallPrompt, RemovePrompt, is_git_source};
pub use provision::{LimitReached, NewSession, ProvisionPhase, ProvisionProgress};
pub use quit::{Billing, QuitPrompt};
pub use runs::{HF_RESULTS, HfLookup, PluginRun, PluginView, RunForm, RunPhase};
pub use sessions::{EndReason, Ended, StopPrompt, runtime_label};
pub use tunnels::{FORWARD_START_PORT, ForwardForm, local_url};
pub use updates::{UPDATE_INTERVAL, UpdateCheck, is_newer};
pub use usage::{
    POLL_CHECK, UsageSummary, is_stale as usage_is_stale, stale_note as usage_stale_note, summarize,
};

use crate::{
    backend::{self, BackendSource},
    i18n::{self, Language, keys, tr, tr_args},
    platform,
    route::Route,
    settings::{AppearancePref, LanguagePref, LoadOutcome, Settings, SettingsStore, folder_url},
};

/// What the state is built from; `app::run` reads it from the process, tests build it by hand
/// (always with a headless mock).
#[derive(Clone)]
pub struct StateOptions {
    pub source: BackendSource,
    pub store: SettingsStore,
    /// The OS locale, for the `System` language.
    pub os_locale: Option<String>,
}

/// A value loaded from the backend.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Load<T> {
    /// Not asked for (yet).
    #[default]
    Idle,
    /// Being fetched for the first time.
    Loading,
    Ready(T),
    Failed(Error),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(value) => Some(value),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<&Error> {
        match self {
            Load::Failed(error) => Some(error),
            _ => None,
        }
    }

    /// Whether a first fetch is under way.
    pub fn is_loading(&self) -> bool {
        matches!(self, Load::Loading)
    }

    /// `Loading` unless a value is already shown (a refresh keeps it on screen).
    fn begin(&mut self) {
        if !matches!(self, Load::Ready(_)) {
            *self = Load::Loading;
        }
    }
}

/// Everything the shell shows. See the crate docs for the pattern.
pub struct AppState {
    route: Route,
    settings: Settings,
    store: SettingsStore,
    os_locale: Option<String>,
    language: Language,
    system_mode: Mode,
    /// Installed CJK families, looked up once at startup.
    cjk_fonts: Vec<String>,
    font_fallbacks: Option<FontFallbacks>,
    toaster: Entity<Toaster>,
    save: Option<Task<()>>,
    open_folder: Option<Task<()>>,
    source: BackendSource,
    backend: Result<Arc<dyn Backend>, Error>,
    pub(crate) ops: ops::Ops,
    probe: Load<ProbeReport>,
    install: InstallState,
    accounts: Load<AccountsView>,
    recovered_notice: bool,
    dialog: Option<AccountDialog>,
    login_flow: Option<Box<dyn LoginFlow>>,
    usage: usage::UsageCache,
    defaults_push: Option<Task<()>>,
    _poller: Option<Task<()>>,
    overlay: Option<Overlay>,
    sessions: sessions::SessionsState,
    new_session: Option<NewSession>,
    plugins: plugins::PluginsState,
    plugin_view: PluginView,
    run_form: Option<RunForm>,
    plugin_run: Option<PluginRun>,
    pending_inputs: Vec<(InputSlot, String)>,
    quit: quit::QuitState,
    stop_all_pending: bool,
    settings_focus: bool,
    update: updates::UpdateCheck,
    /// A downloaded, verified update waiting for `restart_to_update`.
    staged_update: Option<colab_playground_core::self_update::StagedUpdate>,
    /// Whether the stale-update cleanup has run in this process.
    update_cleanup_done: bool,
}

/// A dialog other than the account ones (those are [`AccountDialog`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Overlay {
    /// Stop and end billing, confirmed once.
    Stop(StopPrompt),
    /// Cancel a runtime's auto-stop, confirmed once.
    CancelAutoStop(StopPrompt),
    NewSession,
    Forward(ForwardForm),
    /// Install a plugin, after checking it.
    InstallPlugin(InstallPrompt),
    RemovePlugin(RemovePrompt),
    /// Quit with runtimes still billing.
    Quit(QuitPrompt),
}

/// A text field the root view owns, for state to fill (`AppState::set_input`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InputSlot {
    SessionName,
    /// The fresh session name in the run form.
    RunSessionName,
    /// The plugin folder or Git URL.
    PluginSource,
    /// The forward's port on the runtime.
    ForwardRemote,
    /// The forward's port on this computer.
    ForwardLocal,
    Health,
    /// A run-form text, secret or model field, by input id.
    Field(String),
}

struct GlobalAppState(Entity<AppState>);

impl Global for GlobalAppState {}

pub fn ely_appearance(pref: AppearancePref) -> Appearance {
    match pref {
        AppearancePref::System => Appearance::System,
        AppearancePref::Light => Appearance::Light,
        AppearancePref::Dark => Appearance::Dark,
    }
}

pub fn appearance_pref(appearance: Appearance) -> AppearancePref {
    match appearance {
        Appearance::System => AppearancePref::System,
        Appearance::Light => AppearancePref::Light,
        Appearance::Dark => AppearancePref::Dark,
    }
}

impl AppState {
    /// Call once, after Ely's `init`.
    pub fn init(options: StateOptions, cx: &mut App) -> Entity<AppState> {
        let loaded = options.store.load();
        let language = i18n::resolve(loaded.settings.language, options.os_locale.as_deref());
        i18n::install(language, cx);
        let system_mode = Mode::from(cx.window_appearance());
        let installed = cx.text_system().all_font_names();
        let cjk_fonts: Vec<String> = Language::ALL
            .into_iter()
            .flat_map(platform::cjk_font_candidates)
            .filter(|family| installed.iter().any(|name| name == family))
            .map(str::to_owned)
            .collect();
        let backend = backend::build(
            &options.source,
            &loaded.settings.host_setup(),
            &loaded.settings.defaults,
        );
        if let Err(error) = &backend {
            log::error!("app: the backend failed to start: {error}");
        }
        let flavor = options.source.flavor();
        let state = cx.new(|cx| {
            let mut state = AppState {
                route: Route::default(),
                settings: loaded.settings.clone(),
                store: options.store,
                os_locale: options.os_locale,
                language,
                system_mode,
                font_fallbacks: None,
                cjk_fonts,
                toaster: cx.new(|_| Toaster::default()),
                save: None,
                open_folder: None,
                source: options.source,
                backend,
                ops: ops::Ops::default(),
                probe: Load::Idle,
                install: InstallState::default(),
                accounts: Load::Idle,
                recovered_notice: false,
                dialog: None,
                login_flow: None,
                usage: usage::UsageCache::default(),
                defaults_push: None,
                _poller: None,
                overlay: None,
                sessions: sessions::SessionsState::default(),
                new_session: None,
                plugins: plugins::PluginsState::default(),
                plugin_view: PluginView::default(),
                run_form: None,
                plugin_run: None,
                pending_inputs: Vec::new(),
                quit: quit::QuitState::default(),
                stop_all_pending: false,
                settings_focus: false,
                update: updates::UpdateCheck::default(),
                staged_update: None,
                update_cleanup_done: false,
            };
            state.font_fallbacks = platform::font_fallbacks(language, &state.cjk_fonts);
            state.apply_mode(true, cx);
            if let LoadOutcome::Corrupt { backup, .. } = &loaded.outcome {
                state.report_reset(backup.as_deref(), cx);
            }
            if !loaded.reset_defaults.is_empty() {
                state.report_defaults_reset(&loaded.reset_defaults, cx);
            }
            state._poller = Some(usage::start_poller(cx));
            state.check_environment(cx);
            state
        });
        log::info!(
            "app: backend {flavor:?}, language {}, appearance {:?} (system {system_mode:?}, showing {:?})",
            language.tag(),
            loaded.settings.appearance,
            cx.theme().mode()
        );
        cx.set_global(GlobalAppState(state.clone()));
        // Globals are dropped after the app's entities, so a handle kept in one would outlive
        // them; release it when the app quits.
        cx.on_app_quit(|cx| {
            cx.remove_global::<GlobalAppState>();
            async {}
        })
        .detach();
        // Cmd+Q (bound with the app menu on macOS only) asks the same question as closing the
        // window.
        platform::install_quit(cx);
        cx.on_action(|_: &platform::Quit, cx: &mut App| {
            AppState::global(cx).update(cx, |state, cx| state.request_quit(cx));
        });
        state
    }

    pub fn global(cx: &App) -> Entity<AppState> {
        cx.global::<GlobalAppState>().0.clone()
    }

    pub fn route(&self) -> Route {
        self.route
    }

    /// The open dialog other than the account ones.
    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    /// Closes that dialog (the new-session dialog only hides while it provisions).
    pub fn close_overlay(&mut self, cx: &mut Context<Self>) {
        if matches!(self.overlay, Some(Overlay::Quit(_))) {
            self.stop_all_pending = false;
        }
        match self.overlay {
            Some(Overlay::NewSession) => self.close_new_session(cx),
            Some(_) => {
                self.overlay = None;
                cx.notify();
            }
            None => {}
        }
    }

    /// Asks the root view to put `text` into a text field it owns.
    pub(crate) fn set_input(&mut self, slot: InputSlot, text: String) {
        self.pending_inputs.retain(|(pending, _)| pending != &slot);
        self.pending_inputs.push((slot, text));
    }

    /// Text-field contents the root view still has to apply.
    pub fn take_pending_inputs(&mut self) -> Vec<(InputSlot, String)> {
        std::mem::take(&mut self.pending_inputs)
    }

    pub fn has_saved_accounts(&self) -> bool {
        self.accounts
            .ready()
            .is_some_and(|view| !view.accounts.is_empty())
    }

    /// Why switching or adding an account must wait (a provision or plugin run is tied to
    /// the active account), if it must.
    pub fn account_change_block(&self) -> Option<&'static str> {
        (self.ops.is_running(Op::Provision) || self.ops.is_running(Op::PluginRun))
            .then_some(keys::ACCOUNT_CHANGE_BLOCKED)
    }

    pub fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        if self.route != route {
            log::info!("app: route {:?} -> {route:?}", self.route);
            self.route = route;
            if route != Route::Settings {
                self.settings_focus = false;
            }
            cx.notify();
        }
    }

    /// Whether Settings opens with the session limit called out at the top.
    pub fn settings_focus(&self) -> bool {
        self.settings_focus
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn store(&self) -> &SettingsStore {
        &self.store
    }

    pub fn flavor(&self) -> Flavor {
        self.source.flavor()
    }

    pub fn backend_error(&self) -> Option<&Error> {
        self.backend.as_ref().err()
    }

    pub fn os_locale(&self) -> Option<&str> {
        self.os_locale.as_deref()
    }

    /// The language on show (the preference resolved).
    pub fn language(&self) -> Language {
        self.language
    }

    pub fn system_language(&self) -> Language {
        i18n::resolve(LanguagePref::System, self.os_locale.as_deref())
    }

    /// The operating system's light or dark mode, as last reported by the window.
    pub fn system_mode(&self) -> Mode {
        self.system_mode
    }

    /// The mode the preference asks for right now.
    pub fn wanted_mode(&self) -> Mode {
        ely_appearance(self.settings.appearance).mode(self.system_mode)
    }

    pub fn font_fallbacks(&self) -> Option<&FontFallbacks> {
        self.font_fallbacks.as_ref()
    }

    pub fn toaster(&self) -> &Entity<Toaster> {
        &self.toaster
    }

    pub fn toast(&self, toast: Toast, cx: &mut Context<Self>) {
        self.toaster.update(cx, |toaster, cx| {
            toaster.push(toast, cx);
        });
    }

    pub(crate) fn toast_done(&self, title: gpui::SharedString, cx: &mut Context<Self>) {
        self.toast(Toast::new(title).severity(Severity::Success), cx);
    }

    /// Picks the display language, applies it at once and saves it.
    pub fn set_language(&mut self, pref: LanguagePref, cx: &mut Context<Self>) {
        if self.settings.language == pref {
            return;
        }
        log::info!("app: language preference -> {pref:?}");
        self.settings.language = pref;
        self.apply_language(cx);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Picks light, dark or system appearance, applies it and saves it.
    pub fn set_appearance(&mut self, pref: AppearancePref, cx: &mut Context<Self>) {
        if self.settings.appearance == pref {
            return;
        }
        log::info!("app: appearance preference -> {pref:?}");
        self.settings.appearance = pref;
        self.apply_mode(false, cx);
        self.persist_settings(cx);
        cx.notify();
    }

    /// The window reported the OS appearance; follows it when the preference is `System`.
    pub fn system_appearance_changed(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.system_mode == mode {
            return;
        }
        log::info!("app: system appearance -> {mode:?}");
        self.system_mode = mode;
        self.apply_mode(false, cx);
        cx.notify();
    }

    fn apply_language(&mut self, cx: &mut Context<Self>) {
        let language = i18n::resolve(self.settings.language, self.os_locale.as_deref());
        self.language = language;
        self.font_fallbacks = platform::font_fallbacks(language, &self.cjk_fonts);
        if i18n::current(cx) != language {
            i18n::set_language(language, cx);
        }
    }

    /// Puts the theme in the wanted mode: at once at startup, with Ely's fade afterwards.
    fn apply_mode(&self, now: bool, cx: &mut Context<Self>) {
        let mode = self.wanted_mode();
        if cx.theme().mode() == mode {
            return;
        }
        if now {
            Theme::set_mode_now(mode, cx);
        } else {
            Theme::set_mode(mode, cx);
        }
    }

    /// Writes the settings on the background executor. Saves run one after another, so the
    /// file always ends with the latest settings; a failure shows a toast.
    pub(crate) fn persist_settings(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let settings = self.settings.clone();
        let previous = self.save.take();
        self.save = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let saved = cx
                .background_spawn(async move { store.save(&settings) })
                .await;
            if let Err(error) = saved {
                log::error!("settings: save failed: {error}");
                let detail = error.to_string();
                this.update(cx, |state, cx| {
                    let toast = Toast::new(tr(cx, keys::TOAST_SAVE_FAILED_TITLE))
                        .body(detail)
                        .severity(Severity::Danger);
                    state.toast(toast, cx);
                })
                .ok();
            }
        }));
    }

    /// Opens the folder that holds the settings file in the system's file manager, creating it
    /// first when nothing has been saved yet. A failure shows a toast.
    pub fn open_config_folder(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.store.dir().map(std::path::Path::to_path_buf) else {
            log::warn!("settings: no config directory to open");
            return;
        };
        self.open_folder = Some(cx.spawn(async move |this, cx| {
            let opened = cx.background_spawn(async move { folder_url(&dir) }).await;
            match opened {
                Ok(url) => {
                    log::info!("settings: opening {url}");
                    cx.update(|cx| cx.open_url(&url));
                }
                Err(error) => {
                    log::error!("settings: cannot open the config folder: {error}");
                    this.update(cx, |state, cx| {
                        let toast = Toast::new(tr(cx, keys::TOAST_OPEN_FOLDER_FAILED_TITLE))
                            .body(error)
                            .severity(Severity::Danger);
                        state.toast(toast, cx);
                    })
                    .ok();
                }
            }
        }));
    }

    fn report_reset(&self, backup: Option<&std::path::Path>, cx: &mut Context<Self>) {
        let body = match backup {
            Some(path) => {
                let file = path.display().to_string();
                tr_args(cx, keys::TOAST_SETTINGS_RESET_BODY, &[("file", &file)])
            }
            None => tr(cx, keys::TOAST_SETTINGS_RESET_BODY_LOST),
        };
        let toast = Toast::new(tr(cx, keys::TOAST_SETTINGS_RESET_TITLE))
            .body(body)
            .severity(Severity::Warning);
        self.toast(toast, cx);
    }

    fn report_defaults_reset(&self, fields: &[String], cx: &mut Context<Self>) {
        let fields = fields.join(", ");
        let toast = Toast::new(tr(cx, keys::TOAST_DEFAULTS_RESET_TITLE))
            .body(tr_args(
                cx,
                keys::TOAST_DEFAULTS_RESET_BODY,
                &[("fields", &fields)],
            ))
            .severity(Severity::Warning);
        self.toast(toast, cx);
    }

    /// An error for "the CLI is not installed yet" (shown instead of calling the backend).
    pub(crate) fn not_installed() -> Error {
        Error::new(ErrorKind::NotInstalled)
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        self.ops.cancel_all();
        // Quitting mid sign-in: give the previous credential back before the process ends.
        if let Some(flow) = self.login_flow.take()
            && let Err(error) = flow.cancel()
        {
            log::error!("accounts: rolling back the sign-in on quit failed: {error}");
        }
    }
}
