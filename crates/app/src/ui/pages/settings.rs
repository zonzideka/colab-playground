//! Settings: language and appearance (live and persisted), the policy defaults for new
//! runtimes, where the CLI lives, and build information.

use ely_gpui_component::{
    buttons::{Button, ButtonVariant, CopyButton},
    data_display::{Badge, Tone},
    forms::{Choice, Select},
    layout::Page,
    primitives::{IconName, Tooltip},
    settings::{SettingsRow, SettingsSection, ThemeSelector},
    theme::{ActiveTheme, ContainerSize, ControlSize, TextSize},
    typography::{Caption, MiddleEllipsis},
};
use gpui::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div,
};

use crate::{
    i18n::{keys, tr, tr_args},
    route::Route,
    settings::LanguagePref,
    state::{AppState, appearance_pref, ely_appearance},
    ui::{Inputs, tagged},
};

pub const LANGUAGE_SELECTOR: &str = "settings-language";
/// Selector around the appearance segmented control. Its segments carry Ely's own selectors
/// `segment theme.light`, `segment theme.dark` and `segment theme.system`.
pub const APPEARANCE_SELECTOR: &str = "settings-appearance";
/// Selector around the settings file row's control (path, copy, open folder).
pub const FILE_SELECTOR: &str = "settings-file";
pub const FILE_PATH_SELECTOR: &str = "settings-file-path";
pub const OPEN_FOLDER_SELECTOR: &str = "settings-file-open";

/// The label of a language choice in the shown language.
pub fn language_label(pref: LanguagePref, state: &AppState, cx: &App) -> gpui::SharedString {
    match pref {
        LanguagePref::System => {
            let system = tr(cx, state.system_language().name_key());
            tr_args(cx, keys::SETTINGS_LANGUAGE_SYSTEM, &[("language", &system)])
        }
        LanguagePref::EnUs => tr(cx, keys::LANG_EN_US),
        LanguagePref::ZhCn => tr(cx, keys::LANG_ZH_CN),
        LanguagePref::ZhTw => tr(cx, keys::LANG_ZH_TW),
        LanguagePref::JaJp => tr(cx, keys::LANG_JA_JP),
        LanguagePref::KoKr => tr(cx, keys::LANG_KO_KR),
    }
}

pub fn render(state: &AppState, handle: &Entity<AppState>, inputs: &Inputs, cx: &App) -> Page {
    super::page(Route::Settings, keys::SETTINGS_SUBTITLE, cx)
        .children(
            state
                .settings_focus()
                .then(|| super::defaults::limit_focus(state, handle, cx)),
        )
        .child(general(state, handle, cx))
        .child(super::defaults::defaults_section(state, handle, cx))
        .child(super::defaults::host_section(state, handle, inputs, cx))
        .child(about(state, handle, cx))
}

fn general(state: &AppState, handle: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let choices =
        LanguagePref::ALL.map(|pref| Choice::new(pref.value(), language_label(pref, state, cx)));
    let on_language = handle.clone();
    let language = Select::new("settings-language-select", choices)
        .label(tr(cx, keys::SETTINGS_LANGUAGE_TITLE))
        .selected(state.settings().language.value())
        .on_change(move |value, _, cx| match LanguagePref::from_value(value) {
            Some(pref) => on_language.update(cx, |state, cx| state.set_language(pref, cx)),
            None => log::error!("settings: the language select offered unknown {value:?}"),
        });
    let on_appearance = handle.clone();
    let appearance = ThemeSelector::new(
        "settings-appearance-selector",
        ely_appearance(state.settings().appearance),
        move |appearance, _, cx| {
            on_appearance.update(cx, |state, cx| {
                state.set_appearance(appearance_pref(appearance), cx)
            })
        },
    );
    SettingsSection::new(tr(cx, keys::SETTINGS_GENERAL_TITLE))
        .description(tr(cx, keys::SETTINGS_GENERAL_DESCRIPTION))
        .row(
            SettingsRow::new(tr(cx, keys::SETTINGS_LANGUAGE_TITLE))
                .description(tr(cx, keys::SETTINGS_LANGUAGE_DESCRIPTION))
                .control(tagged(LANGUAGE_SELECTOR, language).w(theme.menu_width())),
        )
        .row(
            SettingsRow::new(tr(cx, keys::SETTINGS_APPEARANCE_TITLE))
                .description(tr(cx, keys::SETTINGS_APPEARANCE_DESCRIPTION))
                .control(tagged(APPEARANCE_SELECTOR, appearance)),
        )
}

fn about(state: &AppState, handle: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let backend = if crate::backend::shows_demo_badge(state.flavor()) {
        Badge::new(tr(cx, keys::BACKEND_MOCK)).tone(Tone::Warning)
    } else {
        Badge::new(tr(cx, keys::BACKEND_REAL)).tone(Tone::Info)
    };
    let file = match state.store().file() {
        Some(path) => settings_file(path.display().to_string(), handle, cx).into_any_element(),
        None => Caption::new(tr(cx, keys::SETTINGS_FILE_UNSAVED)).into_any_element(),
    };
    SettingsSection::new(tr(cx, keys::SETTINGS_ABOUT_TITLE))
        .description(tr(cx, keys::SETTINGS_ABOUT_DESCRIPTION))
        .row(
            SettingsRow::new(tr(cx, keys::SETTINGS_VERSION_TITLE))
                .control(div().child(env!("CARGO_PKG_VERSION"))),
        )
        .row(
            SettingsRow::new(tr(cx, keys::SETTINGS_LICENSE_TITLE)).control(div().child(format!(
                "{} · {}",
                env!("CARGO_PKG_LICENSE"),
                env!("COLAB_PLAYGROUND_COPYRIGHT")
            ))),
        )
        .row(SettingsRow::new(tr(cx, keys::SETTINGS_PROJECT_TITLE)).control(project_link()))
        .row(updates_row(state, handle, cx))
        .row(auto_check_row(state, handle, cx))
        .row(
            SettingsRow::new(tr(cx, keys::SETTINGS_BACKEND_TITLE))
                .description(tr(cx, keys::SETTINGS_BACKEND_DESCRIPTION))
                .control(backend),
        )
        .row(SettingsRow::new(tr(cx, keys::SETTINGS_FILE_TITLE)).control(file))
}

/// The settings file's path cut in the middle to the room there is (whole on hover), with
/// copy and "Open folder". It drops under the row's title when the row is narrow.
fn settings_file(path: String, handle: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let open = handle.clone();
    tagged(
        FILE_SELECTOR,
        div()
            .flex()
            .items_center()
            .gap_1()
            .max_w(theme.container_width(ContainerSize::Sm))
            .child(
                div()
                    .id(FILE_PATH_SELECTOR)
                    .debug_selector(|| FILE_PATH_SELECTOR.into())
                    .min_w_0()
                    .font_family(theme.mono_family.clone())
                    .text_size(theme.text_size(TextSize::Sm))
                    .tooltip(Tooltip::text(path.clone()))
                    .child(MiddleEllipsis::new(path.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .child(CopyButton::new("settings-file-copy", path)),
            )
            .child(
                tagged(
                    OPEN_FOLDER_SELECTOR,
                    Button::new("settings-file-open", tr(cx, keys::SETTINGS_FILE_OPEN))
                        .icon(IconName::FolderOpen)
                        .variant(ButtonVariant::Ghost)
                        .size(ControlSize::Sm)
                        .on_click(move |_, _, cx| {
                            open.update(cx, |state, cx| state.open_config_folder(cx))
                        }),
                )
                .flex_none(),
            ),
    )
}

pub const UPDATE_CHECK_SELECTOR: &str = "update-check";
/// Selector of "Open download page" in About.
pub const UPDATE_OPEN_SELECTOR: &str = "update-open";
/// Selector of "Download and install" in About.
pub const UPDATE_INSTALL_SELECTOR: &str = "update-install";
/// Selector of "Restart to update" in About.
pub const UPDATE_RESTART_SELECTOR: &str = "update-restart";
/// Selector of the install download's Cancel in About.
pub const UPDATE_CANCEL_SELECTOR: &str = "update-cancel";
pub const UPDATE_AUTO_SELECTOR: &str = "update-auto";

/// The update status and its actions: "Check for updates", and for a newer release the
/// in-place install where this platform supports it (followed through the download to the
/// restart), with "Open download page" as the fallback. Selector of the status:
/// `update-status <state>`.
fn updates_row(state: &AppState, handle: &Entity<AppState>, cx: &App) -> SettingsRow {
    use crate::state::UpdateCheck;
    let checking = matches!(state.update_check(), UpdateCheck::Checking);
    let downloading = matches!(state.update_check(), UpdateCheck::Downloading { .. });
    let available = state.update_available();
    let (name, status): (&str, SharedString) = match (state.update_check(), &available) {
        (UpdateCheck::Checking, _) => ("checking", tr(cx, keys::UPDATE_CHECKING)),
        (UpdateCheck::Downloading { .. }, _) => (
            "downloading",
            state
                .update_progress(cx)
                .unwrap_or_else(|| tr(cx, keys::UPDATE_NOT_CHECKED)),
        ),
        (UpdateCheck::Ready { release }, _) => (
            "ready",
            tr_args(cx, keys::UPDATE_READY, &[("version", &release.version)]),
        ),
        (UpdateCheck::Failed(error), Some(_)) => (
            "failed",
            tr_args(
                cx,
                keys::UPDATE_INSTALL_FAILED,
                &[("why", &crate::errors::explain(error, false, cx).message)],
            ),
        ),
        (_, Some((version, _))) => (
            "available",
            tr_args(cx, keys::UPDATE_AVAILABLE, &[("version", version)]),
        ),
        (UpdateCheck::Failed(error), None) => (
            "failed",
            tr_args(
                cx,
                keys::UPDATE_FAILED,
                &[("why", &crate::errors::explain(error, false, cx).message)],
            ),
        ),
        (UpdateCheck::UpToDate, None) => ("up-to-date", tr(cx, keys::UPDATE_UP_TO_DATE)),
        _ => ("idle", tr(cx, keys::UPDATE_NOT_CHECKED)),
    };
    let failed = matches!(state.update_check(), UpdateCheck::Failed(_)) && available.is_none();
    let check = handle.clone();
    let mut controls = div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_2()
        .child(tagged(
            UPDATE_CHECK_SELECTOR,
            Button::new(
                UPDATE_CHECK_SELECTOR,
                tr(
                    cx,
                    if failed {
                        keys::UPDATE_TRY_AGAIN
                    } else {
                        keys::UPDATE_CHECK
                    },
                ),
            )
            .size(ControlSize::Sm)
            .loading(checking)
            .disabled(checking || downloading)
            .on_click(move |_, _, cx| check.update(cx, |state, cx| state.check_updates_now(cx))),
        ));
    if downloading {
        let cancel = handle.clone();
        controls = controls.child(tagged(
            UPDATE_CANCEL_SELECTOR,
            Button::new(UPDATE_CANCEL_SELECTOR, tr(cx, keys::ACTION_CANCEL))
                .size(ControlSize::Sm)
                .on_click(move |_, _, cx| {
                    cancel.update(cx, |state, cx| state.cancel_update_install(cx))
                }),
        ));
    } else if matches!(state.update_check(), UpdateCheck::Ready { .. }) {
        let restart = handle.clone();
        controls = controls.child(tagged(
            UPDATE_RESTART_SELECTOR,
            Button::new(UPDATE_RESTART_SELECTOR, tr(cx, keys::UPDATE_RESTART))
                .size(ControlSize::Sm)
                .primary()
                .on_click(move |_, _, cx| {
                    restart.update(cx, |state, cx| state.restart_to_update(cx))
                }),
        ));
    } else if available.is_some() {
        let installable = state.update_installable();
        if installable {
            let install = handle.clone();
            controls = controls.child(tagged(
                UPDATE_INSTALL_SELECTOR,
                Button::new(UPDATE_INSTALL_SELECTOR, tr(cx, keys::UPDATE_INSTALL))
                    .size(ControlSize::Sm)
                    .primary()
                    .on_click(move |_, _, cx| {
                        install.update(cx, |state, cx| state.install_update(cx))
                    }),
            ));
        }
        let open = handle.clone();
        let open_button = Button::new(UPDATE_OPEN_SELECTOR, tr(cx, keys::UPDATE_OPEN_PAGE))
            .size(ControlSize::Sm)
            .icon(IconName::ExternalLink);
        let open_button = if installable {
            open_button
        } else {
            open_button.primary()
        };
        controls = controls.child(tagged(
            UPDATE_OPEN_SELECTOR,
            open_button
                .on_click(move |_, _, cx| open.update(cx, |state, cx| state.open_update_page(cx))),
        ));
    }
    SettingsRow::new(tr(cx, keys::UPDATE_TITLE))
        .description(status.clone())
        .control(tagged(format!("update-status {name}"), controls))
}

/// "Check for updates automatically", with what it sends.
fn auto_check_row(state: &AppState, handle: &Entity<AppState>, cx: &App) -> SettingsRow {
    let toggle = handle.clone();
    SettingsRow::new(tr(cx, keys::UPDATE_AUTO))
        .description(tr(cx, keys::UPDATE_AUTO_HINT))
        .control(tagged(
            UPDATE_AUTO_SELECTOR,
            ely_gpui_component::forms::Switch::new(UPDATE_AUTO_SELECTOR, state.checks_updates())
                .name(tr(cx, keys::UPDATE_AUTO))
                .on_change(move |on, _, cx| {
                    toggle.update(cx, |state, cx| state.set_check_updates(on, cx))
                }),
        ))
}

pub const PROJECT_LINK_SELECTOR: &str = "about-project";

/// The project page (the package repository), opened in the browser like the app's other links.
fn project_link() -> impl IntoElement {
    let url = env!("CARGO_PKG_REPOSITORY");
    let shown = url.trim_start_matches("https://");
    tagged(
        PROJECT_LINK_SELECTOR,
        Button::new(PROJECT_LINK_SELECTOR, shown)
            .variant(ButtonVariant::Ghost)
            .size(ControlSize::Sm)
            .icon(IconName::ExternalLink)
            .on_click(move |_, _, cx| cx.open_url(url)),
    )
}
