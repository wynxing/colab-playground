//! The Plugins page: the installed list (built-in ones marked), enable / disable / remove,
//! install from a folder or Git URL, the Hugging Face token, and — in place of the list — the
//! run form and the run view.

mod form;
mod run;

use colab_playground_core::plugins::{PluginInfo, PluginSource};
use ely_gpui_component::{
    buttons::{Button, ButtonVariant},
    data_display::{Badge, Tone},
    feedback::InlineMessage,
    forms::{Input, Switch},
    layout::{Card, CardHeader, Page},
    primitives::{IconName, Severity, Tooltip},
    theme::{ActiveTheme, ControlSize, TextSize},
    typography::Caption,
};
use gpui::{
    AnyElement, App, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, div,
};

use crate::{
    i18n::{keys, tr, tr_args},
    route::Route,
    state::{AppState, Load, Op, PluginView, RunPhase},
    ui::{Inputs, error_view, tagged},
};

pub use form::{RUN_KEEP_CONFIGURE, RUN_START, hf_result_selector};
pub use run::{
    ENDPOINT_MCP_APP_SELECTOR, ENDPOINT_SELECTOR, RUN_CANCEL, RUN_RESOURCES, RUN_RESOURCES_STALE,
    RUN_SESSION_UNKNOWN_SELECTOR, RUN_STOP,
};

/// Selector of the install source's Check button.
pub const INSTALL_CHECK: &str = "plugin-check";
/// Selector of a plugin's card: `plugin <id>`.
pub fn plugin_selector(id: &str) -> String {
    format!("plugin {id}")
}
pub fn run_selector(id: &str) -> String {
    format!("plugin-run {id}")
}
pub fn remove_selector(id: &str) -> String {
    format!("plugin-remove {id}")
}
pub fn enable_selector(id: &str) -> String {
    format!("plugin-enabled {id}")
}

pub fn render(state: &AppState, handle: &Entity<AppState>, inputs: &Inputs, cx: &App) -> Page {
    match state.plugin_view() {
        PluginView::Form if state.run_form().is_some() => form::render(state, handle, inputs, cx),
        PluginView::Run if state.plugin_run().is_some() => run::render(state, handle, cx),
        _ => list(state, handle, inputs, cx),
    }
}

fn list(state: &AppState, handle: &Entity<AppState>, inputs: &Inputs, cx: &App) -> Page {
    let mut page = super::page(Route::Plugins, keys::PLUGINS_SUBTITLE, cx);
    if let Some(run) = state.plugin_run() {
        let name = state
            .plugin(&run.plugin)
            .map(|info| state.plugin_name(info))
            .unwrap_or_else(|| run.plugin.clone());
        let (severity, text) = match &run.phase {
            RunPhase::Running => (
                Severity::Info,
                tr_args(cx, keys::RUN_BANNER_RUNNING, &[("name", &name)]),
            ),
            RunPhase::Ready(_) => (
                Severity::Success,
                tr_args(cx, keys::RUN_BANNER_READY, &[("name", &name)]),
            ),
            RunPhase::Failed(_) => (
                Severity::Danger,
                tr_args(cx, keys::RUN_BANNER_FAILED, &[("name", &name)]),
            ),
            RunPhase::Cancelled => (
                Severity::Info,
                tr_args(cx, keys::RUN_BANNER_CANCELLED, &[("name", &name)]),
            ),
        };
        let show = handle.clone();
        page = page.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().child(InlineMessage::new(severity, text)))
                .child(
                    Button::new("plugin-run-show", tr(cx, keys::ACTION_SHOW))
                        .size(ControlSize::Sm)
                        .on_click(move |_, _, cx| {
                            show.update(cx, |state, cx| state.show_plugin_run(cx))
                        }),
                ),
        );
    }
    page = match state.plugins() {
        Load::Ready(list) => {
            page.children(list.iter().map(|info| plugin_card(info, state, handle, cx)))
        }
        Load::Failed(error) => {
            page.child(error_view::alert("plugins", None, error, None, handle, cx))
        }
        Load::Idle | Load::Loading => page.child(Caption::new(tr(cx, keys::PLUGINS_LOADING))),
    };
    page.child(install_card(state, handle, inputs, cx))
        .child(token_card(state, handle, inputs, cx))
}

fn plugin_card(
    info: &PluginInfo,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let id = info.manifest.id.clone();
    let name = state.plugin_name(info);
    let tag = state.language().tag();
    let bundled = info.source == PluginSource::Bundled;
    let busy = state.is_busy(Op::PluginChange(crate::state::key_of(&id)));
    let mut title = div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .text_size(theme.text_size(TextSize::Lg))
                .font_weight(FontWeight::SEMIBOLD)
                .child(name),
        )
        .child(Caption::new(tr_args(
            cx,
            keys::PLUGIN_VERSION_SHORT,
            &[("version", &info.manifest.version)],
        )));
    if bundled {
        title = title.child(Badge::new(tr(cx, keys::PLUGIN_BUILT_IN)).tone(Tone::Info));
    }
    if !info.enabled {
        title = title.child(Badge::new(tr(cx, keys::PLUGIN_OFF)));
    }
    let toggle = handle.clone();
    let toggle_id = id.clone();
    let enabled = Switch::new(
        SharedString::from(format!("plugin-enabled-{id}")),
        info.enabled,
    )
    .name(tr(cx, keys::PLUGIN_ENABLED))
    .disabled(busy)
    .on_change(move |on, _, cx| {
        toggle.update(cx, |state, cx| {
            state.set_plugin_enabled(toggle_id.clone(), on, cx)
        })
    });
    let remove = {
        let (handle, id) = (handle.clone(), id.clone());
        let button = Button::new(
            SharedString::from(format!("remove-{id}")),
            tr(cx, keys::ACTION_REMOVE),
        )
        .variant(ButtonVariant::Ghost)
        .size(ControlSize::Sm)
        .disabled(bundled || busy)
        .on_click(move |_, _, cx| {
            handle.update(cx, |state, cx| state.request_remove_plugin(id.clone(), cx))
        });
        let wrap = div().id(SharedString::from(format!(
            "remove-wrap-{}",
            info.manifest.id
        )));
        if bundled {
            wrap.tooltip(Tooltip::text(tr(cx, keys::PLUGIN_BUILT_IN_REMOVE)))
                .child(button)
        } else {
            wrap.child(button)
        }
    };
    let run = {
        let (handle, id) = (handle.clone(), id.clone());
        Button::new(
            SharedString::from(format!("run-{id}")),
            tr(cx, keys::PLUGIN_OPEN),
        )
        .primary()
        .size(ControlSize::Sm)
        .icon(IconName::Rocket)
        .disabled(!info.enabled)
        .on_click(move |_, _, cx| {
            handle.update(cx, |state, cx| state.open_run_form(id.clone(), cx))
        })
    };
    tagged(
        plugin_selector(&id),
        Card::new()
            .child(title)
            .children(
                info.manifest
                    .description
                    .get(tag)
                    .map(|d| Caption::new(d.to_owned())),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .child(tagged(run_selector(&id), run))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(tagged(enable_selector(&id), enabled))
                            .child(
                                div()
                                    .text_size(theme.text_size(TextSize::Sm))
                                    .child(tr(cx, keys::PLUGIN_ENABLED)),
                            ),
                    )
                    .child(tagged(remove_selector(&id), remove)),
            ),
    )
    .w_full()
    .into_any_element()
}

fn install_card(
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let (check, source) = (handle.clone(), inputs.plugin_source.clone());
    let busy = state.is_busy(Op::InspectPlugin);
    Card::new()
        .header(
            CardHeader::new(tr(cx, keys::PLUGIN_INSTALL_TITLE))
                .description(tr(cx, keys::PLUGIN_INSTALL_HINT)),
        )
        .child(
            div()
                .flex()
                .items_end()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(theme.label_width())
                        .child(Input::new(&inputs.plugin_source)),
                )
                .child(tagged(
                    INSTALL_CHECK,
                    Button::new(INSTALL_CHECK, tr(cx, keys::PLUGIN_CHECK))
                        .loading(busy)
                        .on_click(move |_, _, cx| {
                            let text = source.read(cx).text().to_owned();
                            check.update(cx, |state, cx| state.inspect_plugin(text, cx))
                        }),
                )),
        )
        .w_full()
        .into_any_element()
}

fn token_card(
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let stored = state.hf_token().ready().copied();
    let status = match state.hf_token() {
        Load::Ready(true) => Badge::new(tr(cx, keys::HF_TOKEN_STORED)).tone(Tone::Success),
        Load::Ready(false) => Badge::new(tr(cx, keys::HF_TOKEN_NONE)),
        Load::Failed(_) => Badge::new(tr(cx, keys::HF_TOKEN_UNKNOWN)).tone(Tone::Warning),
        _ => Badge::new(tr(cx, keys::SUMMARY_UNKNOWN)),
    };
    let (save, field) = (handle.clone(), inputs.hf_token.clone());
    let delete = handle.clone();
    let busy = state.is_busy(Op::Secret);
    let mut row = div()
        .flex()
        .flex_wrap()
        .items_end()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w(theme.label_width())
                .child(Input::new(&inputs.hf_token)),
        )
        .child(tagged(
            "hf-token-save",
            Button::new("hf-token-save", tr(cx, keys::ACTION_SAVE))
                .loading(busy)
                .disabled(busy)
                .on_click(move |_, _, cx| {
                    // The value goes straight from the masked field to the vault; the field is
                    // cleared and the app keeps nothing.
                    let value = field.read(cx).text().to_owned();
                    field.update(cx, |input, cx| input.set_text(String::new(), cx));
                    save.update(cx, |state, cx| state.save_hf_token(value, cx))
                }),
        ));
    if stored == Some(true) {
        row = row.child(tagged(
            "hf-token-delete",
            Button::new("hf-token-delete", tr(cx, keys::ACTION_DELETE))
                .variant(ButtonVariant::Ghost)
                .on_click(move |_, _, cx| delete.update(cx, |state, cx| state.delete_hf_token(cx))),
        ));
    }
    tagged(
        "hf-token",
        Card::new()
            .header(
                CardHeader::new(tr(cx, keys::HF_TOKEN_TITLE))
                    .description(tr(cx, keys::HF_TOKEN_HINT))
                    .action(status),
            )
            .child(row),
    )
    .w_full()
    .into_any_element()
}
