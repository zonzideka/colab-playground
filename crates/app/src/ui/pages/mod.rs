//! One function per page. A page that needs local state (text fields, scroll positions) keeps it
//! in `AppState` or in an entity the root view owns (see `ui::Inputs`).

pub mod accounts;
pub mod defaults;
pub mod overview;
pub mod plugins;
pub mod sessions;
pub mod settings;

use ely_gpui_component::{
    layout::Page,
    theme::{ActiveTheme, ContainerSize},
};
use gpui::{AnyElement, App, Entity, IntoElement, ParentElement, Styled, div};

use crate::{
    i18n::tr,
    route::Route,
    state::AppState,
    ui::{Inputs, tagged},
};

pub fn render(
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> AnyElement {
    let route = state.route();
    let notice = update_banner(state, handle, cx);
    let page = match route {
        Route::Overview => overview::render(state, handle, cx),
        Route::Sessions => sessions::render(state, handle, cx),
        Route::Plugins => plugins::render(state, handle, inputs, cx),
        Route::Accounts => accounts::render(state, handle, cx),
        Route::Settings => settings::render(state, handle, inputs, cx),
    };
    let framed = frame(route, page, cx);
    match notice {
        Some(notice) => div()
            .w_full()
            .flex()
            .flex_col()
            .child(notice)
            .child(framed)
            .into_any_element(),
        None => framed,
    }
}

pub const UPDATE_NOTICE_SELECTOR: &str = "update-notice";

/// A newer release, announced once per version above every page until dismissed for that
/// version. When this platform can install it, the banner offers the in-place install and
/// follows it through the download to the restart; otherwise it offers the release page.
fn update_banner(state: &AppState, handle: &Entity<AppState>, cx: &App) -> Option<AnyElement> {
    use crate::state::UpdateCheck;
    let (version, _) = state.update_notice()?;
    let theme = cx.theme();
    let size = ely_gpui_component::theme::ControlSize::Sm;
    let ghost = ely_gpui_component::buttons::ButtonVariant::Ghost;
    let dismiss = {
        let dismiss = handle.clone();
        let dismissed = version.clone();
        tagged(
            "update-notice-dismiss",
            ely_gpui_component::buttons::Button::new(
                "update-notice-dismiss",
                tr(cx, crate::i18n::keys::UPDATE_DISMISS),
            )
            .size(size)
            .variant(ghost)
            .on_click(move |_, _, cx| {
                dismiss.update(cx, |state, cx| state.dismiss_update(dismissed.clone(), cx))
            }),
        )
    };
    let open = |primary: bool| {
        let open = handle.clone();
        let button = ely_gpui_component::buttons::Button::new(
            "update-notice-open",
            tr(cx, crate::i18n::keys::UPDATE_OPEN_PAGE),
        )
        .size(size);
        let button = if primary {
            button.primary()
        } else {
            button.variant(ghost)
        };
        tagged(
            "update-notice-open",
            button
                .on_click(move |_, _, cx| open.update(cx, |state, cx| state.open_update_page(cx))),
        )
    };
    let (body, actions) = match state.update_check() {
        UpdateCheck::Downloading { .. } => {
            let cancel = handle.clone();
            (
                state
                    .update_progress(cx)
                    .unwrap_or_else(|| tr(cx, crate::i18n::keys::UPDATE_NOTICE_BODY_INSTALL)),
                div().flex().gap_2().child(tagged(
                    "update-notice-cancel",
                    ely_gpui_component::buttons::Button::new(
                        "update-notice-cancel",
                        tr(cx, crate::i18n::keys::ACTION_CANCEL),
                    )
                    .size(size)
                    .variant(ghost)
                    .on_click(move |_, _, cx| {
                        cancel.update(cx, |state, cx| state.cancel_update_install(cx))
                    }),
                )),
            )
        }
        UpdateCheck::Ready { release } => {
            let restart = handle.clone();
            (
                crate::i18n::tr_args(
                    cx,
                    crate::i18n::keys::UPDATE_READY,
                    &[("version", &release.version)],
                ),
                div()
                    .flex()
                    .gap_2()
                    .child(tagged(
                        "update-notice-restart",
                        ely_gpui_component::buttons::Button::new(
                            "update-notice-restart",
                            tr(cx, crate::i18n::keys::UPDATE_RESTART),
                        )
                        .size(size)
                        .primary()
                        .on_click(move |_, _, cx| {
                            restart.update(cx, |state, cx| state.restart_to_update(cx))
                        }),
                    ))
                    .child(dismiss),
            )
        }
        _ if state.update_installable() => {
            let install = handle.clone();
            (
                tr(cx, crate::i18n::keys::UPDATE_NOTICE_BODY_INSTALL),
                div()
                    .flex()
                    .gap_2()
                    .child(tagged(
                        "update-notice-install",
                        ely_gpui_component::buttons::Button::new(
                            "update-notice-install",
                            tr(cx, crate::i18n::keys::UPDATE_INSTALL),
                        )
                        .size(size)
                        .primary()
                        .on_click(move |_, _, cx| {
                            install.update(cx, |state, cx| state.install_update(cx))
                        }),
                    ))
                    .child(open(false))
                    .child(dismiss),
            )
        }
        _ => (
            tr(cx, crate::i18n::keys::UPDATE_NOTICE_BODY),
            div().flex().gap_2().child(open(true)).child(dismiss),
        ),
    };
    Some(
        tagged(
            UPDATE_NOTICE_SELECTOR,
            div()
                .w_full()
                .max_w(theme.container_width(ContainerSize::Lg))
                .mx_auto()
                .px_6()
                .pt_4()
                .child(
                    ely_gpui_component::feedback::Alert::new(
                        "update-notice",
                        ely_gpui_component::primitives::Severity::Info,
                        crate::i18n::tr_args(
                            cx,
                            crate::i18n::keys::UPDATE_NOTICE_TITLE,
                            &[("version", &version)],
                        ),
                    )
                    .body(body)
                    .action(actions),
                ),
        )
        .w_full()
        .into_any_element(),
    )
}

/// The page under its title, centred at a readable width. In test builds the frame carries the
/// selector `page <slug> <title>`, so tests can check which page shows in which language.
fn frame(route: Route, page: Page, cx: &App) -> AnyElement {
    let title = tr(cx, route.title_key());
    let theme = cx.theme();
    tagged(
        format!("page {} {title}", route.slug()),
        div()
            .w_full()
            .max_w(theme.container_width(ContainerSize::Lg))
            .mx_auto()
            .child(page),
    )
    .w_full()
    .into_any_element()
}

pub fn page(route: Route, subtitle_key: &str, cx: &App) -> Page {
    Page::new(tr(cx, route.title_key())).subtitle(tr(cx, subtitle_key))
}
