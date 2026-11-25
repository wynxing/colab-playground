//! Settings: language and appearance (live and persisted), the policy defaults for new
//! runtimes, where the CLI lives, and build information.

use ely_gpui_component::{
    buttons::{Button, ButtonVariant, CopyButton},
    data_display::{Badge, Tone},
    forms::{Choice, Select, Switch},
    layout::Page,
    primitives::{IconName, Tooltip},
    settings::{SettingsRow, SettingsSection, ThemeSelector},
    theme::{ActiveTheme, ContainerSize, ControlSize, TextSize},
    typography::{Caption, MiddleEllipsis},
};
use gpui::{
    AnyElement, App, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div,
};

use colab_playground_core::updater::SelfUpdate;

use crate::{
    i18n::{keys, tr, tr_args},
    route::Route,
    settings::LanguagePref,
    state::{AppState, appearance_pref, ely_appearance},
    ui::{Inputs, overlays::unsupported_reason, tagged},
};

pub const LANGUAGE_SELECTOR: &str = "settings-language";
/// Selector around the appearance segmented control. Its segments carry Ely's own selectors
/// `segment theme.light`, `segment theme.dark` and `segment theme.system`.
pub const APPEARANCE_SELECTOR: &str = "settings-appearance";
/// Selector around the settings file row's control (path, copy, open folder).
pub const FILE_SELECTOR: &str = "settings-file";
pub const FILE_PATH_SELECTOR: &str = "settings-file-path";
pub const OPEN_FOLDER_SELECTOR: &str = "settings-file-open";
/// The agent endpoint's own selectors. The token has one for each state, so a test can tell
/// "hidden" from "shown" without the token itself ever being in a selector.
pub const AGENTS_TOGGLE_SELECTOR: &str = "settings-agents-toggle";
pub const AGENTS_ADDRESS_SELECTOR: &str = "settings-agents-address";
pub const AGENTS_TOKEN_SELECTOR: &str = "settings-agents-token";
pub const AGENTS_TOKEN_HIDDEN_SELECTOR: &str = "settings-agents-token-hidden";
pub const AGENTS_TOKEN_SHOWN_SELECTOR: &str = "settings-agents-token-shown";
pub const AGENTS_TOKEN_REVEAL_SELECTOR: &str = "settings-agents-token-reveal";
pub const AGENTS_TOKEN_ROTATE_SELECTOR: &str = "settings-agents-token-rotate";
pub const AGENTS_CONFIG_SELECTOR: &str = "settings-agents-config";

/// The label of a language choice in the shown language.
pub fn language_label(pref: LanguagePref, state: &AppState, cx: &App) -> SharedString {
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
        .row(feedback_row(cx))
        .row(updates_row(state, handle, cx))
        .row(auto_check_row(state, handle, cx))
        .child(agents_section(state, handle, cx))
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
/// Selector of "Open download page" in About (a copy that cannot replace itself).
pub const UPDATE_OPEN_SELECTOR: &str = "update-open";
/// Selector of "Download and restart" in About.
pub const UPDATE_INSTALL_SELECTOR: &str = "update-install";
pub const UPDATE_AUTO_SELECTOR: &str = "update-auto";

/// The states of the update status that also decide what "Check for updates" shows.
const STATUS_CHECKING: &str = "checking";
const STATUS_FAILED: &str = "failed";

/// The update status, "Check for updates", and for a newer release "Download and restart" or,
/// when this copy cannot replace itself, "Open download page" with the reason. Selector of the
/// status: `update-status <state>`.
fn updates_row(state: &AppState, handle: &Entity<AppState>, cx: &App) -> SettingsRow {
    use crate::state::UpdateCheck;
    let available = state.update_available();
    let unverified = available
        .as_ref()
        .is_some_and(|(version, _)| state.update_unverified(version));
    let (name, status): (&str, SharedString) = match (state.update_check(), &available) {
        (UpdateCheck::Checking, _) => (STATUS_CHECKING, tr(cx, keys::UPDATE_CHECKING)),
        (_, Some((version, _))) if unverified => (
            "unverified",
            tr_args(cx, keys::UPDATE_UNVERIFIED_STATUS, &[("version", version)]),
        ),
        (_, Some((version, _))) => match state.self_update() {
            Some(SelfUpdate::Unsupported(reason)) => (
                "available-manual",
                tr_args(
                    cx,
                    keys::UPDATE_AVAILABLE_MANUAL,
                    &[
                        ("version", version),
                        ("why", &unsupported_reason(reason, cx)),
                    ],
                ),
            ),
            _ => (
                "available",
                tr_args(cx, keys::UPDATE_AVAILABLE, &[("version", version)]),
            ),
        },
        (UpdateCheck::Failed(error), None) => (
            STATUS_FAILED,
            tr_args(
                cx,
                keys::UPDATE_FAILED,
                &[("why", &crate::errors::explain(error, false, cx).message)],
            ),
        ),
        (UpdateCheck::UpToDate, None) => ("up-to-date", tr(cx, keys::UPDATE_UP_TO_DATE)),
        _ => ("idle", tr(cx, keys::UPDATE_NOT_CHECKED)),
    };
    let (checking, failed) = (name == STATUS_CHECKING, name == STATUS_FAILED);
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
            .disabled(checking)
            .on_click(move |_, _, cx| check.update(cx, |state, cx| state.check_updates_now(cx))),
        ));
    if available.is_some() && state.can_self_update() {
        let install = handle.clone();
        controls = controls.child(tagged(
            UPDATE_INSTALL_SELECTOR,
            Button::new(UPDATE_INSTALL_SELECTOR, tr(cx, keys::UPDATE_INSTALL))
                .size(ControlSize::Sm)
                .primary()
                .icon(IconName::Download)
                .on_click(move |_, _, cx| install.update(cx, |state, cx| state.begin_update(cx))),
        ));
    } else if available.is_some() && !unverified && state.self_update().is_some() {
        let open = handle.clone();
        controls = controls.child(tagged(
            UPDATE_OPEN_SELECTOR,
            Button::new(UPDATE_OPEN_SELECTOR, tr(cx, keys::UPDATE_OPEN_PAGE))
                .size(ControlSize::Sm)
                .primary()
                .icon(IconName::ExternalLink)
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
            Switch::new(UPDATE_AUTO_SELECTOR, state.checks_updates())
                .name(tr(cx, keys::UPDATE_AUTO))
                .on_change(move |on, _, cx| {
                    toggle.update(cx, |state, cx| state.set_check_updates(on, cx))
                }),
        ))
}

/// The local agent endpoint: what an agent on this computer may read through it, where it answers
/// and the token it has to bring. Off unless the user turns it on, and the address and token are
/// only shown while there is something to connect to.
fn agents_section(state: &AppState, handle: &Entity<AppState>, cx: &App) -> SettingsSection {
    let theme = cx.theme();
    let agents = state.agents();
    let shown = state.agents_revealed();
    let (toggle, reveal, rotate) = (handle.clone(), handle.clone(), handle.clone());

    let mut section = SettingsSection::new(tr(cx, keys::SETTINGS_AGENTS_TITLE))
        .description(tr(cx, keys::SETTINGS_AGENTS_DESCRIPTION))
        .row(
            SettingsRow::new(tr(cx, keys::SETTINGS_AGENTS_TOGGLE))
                .description(tr(cx, keys::SETTINGS_AGENTS_TOGGLE_HINT))
                .control(tagged(
                    AGENTS_TOGGLE_SELECTOR,
                    Switch::new(AGENTS_TOGGLE_SELECTOR, state.agents_enabled())
                        .name(tr(cx, keys::SETTINGS_AGENTS_TOGGLE))
                        .on_change(move |on, _, cx| {
                            toggle.update(cx, |state, cx| state.set_agents_enabled(on, cx))
                        }),
                )),
        );

    // Where it answers — or why it is not answering, which the user has to be able to see: a port
    // another copy of the app holds is the usual reason.
    section =
        match (agents.address(), agents.error()) {
            (Some(address), _) => section.row(
                SettingsRow::new(tr(cx, keys::SETTINGS_AGENTS_ADDRESS_TITLE)).control(tagged(
                    AGENTS_ADDRESS_SELECTOR,
                    div()
                        .font_family(theme.mono_family.clone())
                        .child(format!("http://{address}/mcp")),
                )),
            ),
            (None, Some(error)) => {
                let port = error.param("port").unwrap_or_default().to_owned();
                section.row(
                    SettingsRow::new(tr(cx, keys::SETTINGS_AGENTS_ADDRESS_TITLE)).description(
                        tr_args(cx, keys::SETTINGS_AGENTS_PORT_TAKEN, &[("port", &port)]),
                    ),
                )
            }
            (None, None) => section.row(
                SettingsRow::new(tr(cx, keys::SETTINGS_AGENTS_ADDRESS_TITLE))
                    .description(tr(cx, keys::SETTINGS_AGENTS_OFF)),
            ),
        };

    let token: AnyElement = match (agents.token(), shown) {
        (Some(token), true) => tagged(AGENTS_TOKEN_SHOWN_SELECTOR, div().child(token.to_owned()))
            .font_family(theme.mono_family.clone())
            .into_any_element(),
        (Some(_), false) => {
            tagged(AGENTS_TOKEN_HIDDEN_SELECTOR, div().child("••••••••")).into_any_element()
        }
        (None, _) => Caption::new(tr(cx, keys::SETTINGS_AGENTS_OFF)).into_any_element(),
    };
    let token_controls = div()
        .flex()
        .items_center()
        .gap_1()
        .child(token)
        .child(tagged(
            AGENTS_TOKEN_REVEAL_SELECTOR,
            Button::new(
                "settings-agents-reveal",
                tr(
                    cx,
                    if shown {
                        keys::SETTINGS_AGENTS_TOKEN_HIDE
                    } else {
                        keys::SETTINGS_AGENTS_TOKEN_REVEAL
                    },
                ),
            )
            .variant(ButtonVariant::Ghost)
            .size(ControlSize::Sm)
            .on_click(move |_, _, cx| reveal.update(cx, |state, cx| state.toggle_agents_token(cx))),
        ))
        .child(tagged(
            AGENTS_TOKEN_ROTATE_SELECTOR,
            Button::new(
                "settings-agents-rotate",
                tr(cx, keys::SETTINGS_AGENTS_TOKEN_ROTATE),
            )
            .variant(ButtonVariant::Ghost)
            .size(ControlSize::Sm)
            .on_click(move |_, _, cx| rotate.update(cx, |state, cx| state.rotate_agents_token(cx))),
        ));
    section = section.row(
        SettingsRow::new(tr(cx, keys::SETTINGS_AGENTS_TOKEN_TITLE))
            .description(tr(cx, keys::SETTINGS_AGENTS_TOKEN_HINT))
            .control(tagged(AGENTS_TOKEN_SELECTOR, token_controls)),
    );

    // What a client needs, ready to paste: the same address and token that are on screen.
    if let (Some(address), Some(token)) = (agents.address(), agents.token()) {
        let config = serde_json::json!({
            "mcpServers": {
                "colab-playground": {
                    "type": "http",
                    "url": format!("http://{address}/mcp"),
                    "headers": { "Authorization": format!("Bearer {token}") },
                }
            }
        })
        .to_string();
        section = section.row(
            SettingsRow::new(tr(cx, keys::SETTINGS_AGENTS_CONFIG_TITLE))
                .description(tr(cx, keys::SETTINGS_AGENTS_CONFIG_HINT))
                .control(tagged(
                    AGENTS_CONFIG_SELECTOR,
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .max_w(theme.container_width(ContainerSize::Sm))
                        .child(div().min_w_0().child(MiddleEllipsis::new(config.clone())))
                        .child(
                            div()
                                .flex_none()
                                .child(CopyButton::new("settings-agents-config-copy", config)),
                        ),
                )),
        );
    }

    section
}

pub const PROJECT_LINK_SELECTOR: &str = "about-project";

/// The project page (the package repository), opened in the browser like the app's other links.
fn project_link() -> impl IntoElement {
    let url = crate::state::PROJECT_PAGE;
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

pub const FEEDBACK_LINK_SELECTOR: &str = "about-feedback";

/// Feedback at any time, since the prompt stops asking (`state::feedback`).
fn feedback_row(cx: &App) -> SettingsRow {
    let url = crate::state::feedback_page();
    SettingsRow::new(tr(cx, keys::SETTINGS_FEEDBACK_TITLE))
        .description(tr(cx, keys::SETTINGS_FEEDBACK_DESCRIPTION))
        .control(tagged(
            FEEDBACK_LINK_SELECTOR,
            Button::new(FEEDBACK_LINK_SELECTOR, tr(cx, keys::FEEDBACK_GIVE))
                .size(ControlSize::Sm)
                .icon(IconName::ExternalLink)
                .on_click(move |_, _, cx| cx.open_url(&url)),
        ))
}
