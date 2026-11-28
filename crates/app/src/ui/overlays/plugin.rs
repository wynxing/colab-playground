//! Installing a plugin (check first, then confirm with the manifest summary, what it
//! receives, and that its script runs on the user's paid runtime) and removing one.

use colab_playground_core::plugins::{Capability, InputKind, PluginOrigin, RunMode};
use ely_gpui_component::{
    buttons::{Button, ButtonVariant},
    feedback::InlineMessage,
    overlays::Dialog,
    primitives::Severity,
    theme::{ActiveTheme, TextSize},
    typography::{Caption, Paragraph},
};
use gpui::{
    AnyElement, App, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, div,
};

use crate::{
    format,
    i18n::{keys, tr, tr_args},
    state::{AppState, InstallPrompt, RemovePrompt},
    ui::{
        error_view,
        parts::{Layer, busy_line},
        tagged,
    },
};

pub const INSTALL_CONFIRM: &str = "plugin-install-confirm";
pub const REMOVE_CONFIRM: &str = "plugin-remove-confirm";

fn fact(label: SharedString, value: SharedString, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex()
        .gap_3()
        .text_size(theme.text_size(TextSize::Sm))
        .child(
            div()
                .w(theme.label_width())
                .flex_none()
                .text_color(theme.colors.fg_muted)
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(value))
}

pub(super) fn install(
    prompt: &InstallPrompt,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let dialog = Dialog::new(
        "plugin-install",
        tr(cx, keys::PLUGIN_INSTALL_TITLE),
        Layer::Overlay.close(handle),
    );
    let cancel = tr(cx, keys::ACTION_CANCEL);
    let cancel_action = {
        let handle = handle.clone();
        move |_| Layer::Overlay.cancel_button(cancel, handle)
    };
    match prompt {
        InstallPrompt::Checking => dialog
            .child(busy_line("plugin-checking", tr(cx, keys::PLUGIN_CHECKING)))
            .action(cancel_action)
            .into_any_element(),
        InstallPrompt::Failed(error) => dialog
            .child(error_view::alert(
                "plugin-install",
                None,
                error,
                None,
                handle,
                cx,
            ))
            .action(cancel_action)
            .into_any_element(),
        InstallPrompt::Preview(preview) => {
            let tag = state.language().tag();
            let manifest = &preview.manifest;
            let name = manifest.name.get(tag).unwrap_or(&manifest.id).to_owned();
            let origin = match &preview.origin {
                PluginOrigin::Folder { path } => path.clone(),
                PluginOrigin::Git { url } => url.clone(),
            };
            let inputs: Vec<String> = manifest
                .inputs
                .iter()
                .map(|input| {
                    let label = input.label.get(tag).unwrap_or(&input.id).to_owned();
                    if input.kind == InputKind::Secret {
                        tr_args(cx, keys::PLUGIN_INPUT_SECRET, &[("name", &label)]).to_string()
                    } else {
                        label
                    }
                })
                .collect();
            let needs: Vec<String> = manifest
                .needs
                .iter()
                .map(|need| match need {
                    Capability::HfToken => tr(cx, keys::PLUGIN_NEEDS_HF_TOKEN).to_string(),
                    Capability::GcpAuth => tr(cx, keys::PLUGIN_NEEDS_GCP_AUTH).to_string(),
                    Capability::Drive => tr(cx, keys::PLUGIN_NEEDS_DRIVE).to_string(),
                })
                .collect();
            let ports: Vec<String> = manifest
                .endpoints
                .iter()
                .map(|endpoint| endpoint.label.get(tag).unwrap_or(&endpoint.id).to_owned())
                .collect();
            let theme = cx.theme();
            let mut facts = div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .text_size(theme.text_size(TextSize::Lg))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(name.clone()),
                )
                .children(
                    manifest
                        .description
                        .get(tag)
                        .map(|d| Caption::new(d.to_owned())),
                )
                .child(fact(
                    tr(cx, keys::PLUGIN_VERSION),
                    manifest.version.clone().into(),
                    cx,
                ))
                .child(fact(tr(cx, keys::PLUGIN_FROM), origin.into(), cx))
                .child(fact(
                    tr(cx, keys::PLUGIN_SCRIPT),
                    tr_args(
                        cx,
                        keys::PLUGIN_SCRIPT_VALUE,
                        &[
                            ("file", &manifest.run.script),
                            ("size", &format::bytes(cx, preview.script_bytes)),
                        ],
                    ),
                    cx,
                ));
            if manifest.run.mode == RunMode::Job {
                facts = facts.child(fact(
                    tr(cx, keys::PLUGIN_MODE),
                    tr(cx, keys::PLUGIN_MODE_JOB),
                    cx,
                ));
            }
            let slots: Vec<String> = manifest
                .secrets
                .iter()
                .map(|slot| {
                    let label = slot.label.get(tag).unwrap_or(&slot.id).to_owned();
                    tr_args(cx, keys::PLUGIN_NEEDS_SECRET, &[("name", &label)]).to_string()
                })
                .collect();
            if !slots.is_empty() {
                facts = facts.child(fact(
                    tr(cx, keys::PLUGIN_SECRETS_TITLE),
                    format::list(cx, &slots).into(),
                    cx,
                ));
            }
            if !manifest.assets.is_empty() {
                let count = manifest.assets.len().to_string();
                facts = facts.child(fact(
                    tr(cx, keys::PLUGIN_FILES),
                    tr_args(cx, keys::PLUGIN_ASSETS, &[("count", &count)]),
                    cx,
                ));
            }
            if !inputs.is_empty() {
                facts = facts.child(fact(
                    tr(cx, keys::PLUGIN_ASKS),
                    format::list(cx, &inputs).into(),
                    cx,
                ));
            }
            facts = facts.child(fact(
                tr(cx, keys::PLUGIN_RECEIVES),
                if needs.is_empty() {
                    tr(cx, keys::PLUGIN_RECEIVES_NOTHING)
                } else {
                    format::list(cx, &needs).into()
                },
                cx,
            ));
            if !ports.is_empty() {
                facts = facts.child(fact(
                    tr(cx, keys::PLUGIN_OFFERS),
                    format::list(cx, &ports).into(),
                    cx,
                ));
            }
            let mut dialog = dialog.child(facts).child(InlineMessage::new(
                Severity::Warning,
                tr(cx, keys::PLUGIN_RUNS_ON_RUNTIME),
            ));
            if let Some(version) = &preview.replaces_version {
                dialog = dialog.child(InlineMessage::new(
                    Severity::Info,
                    tr_args(cx, keys::PLUGIN_REPLACES, &[("version", version)]),
                ));
            }
            let confirm = tr(cx, keys::PLUGIN_INSTALL_ACTION);
            let go = handle.clone();
            dialog
                .action(cancel_action)
                .action(move |_| {
                    tagged(
                        INSTALL_CONFIRM,
                        Button::new(INSTALL_CONFIRM, confirm).primary().on_click(
                            move |_, _, cx| go.update(cx, |state, cx| state.confirm_install(cx)),
                        ),
                    )
                })
                .into_any_element()
        }
    }
}

pub(super) fn remove(prompt: &RemovePrompt, handle: &Entity<AppState>, cx: &App) -> AnyElement {
    let mut dialog = Dialog::new(
        "plugin-remove",
        tr_args(cx, keys::PLUGIN_REMOVE_TITLE, &[("name", &prompt.name)]),
        Layer::Overlay.close(handle),
    )
    .child(Paragraph::new(tr(cx, keys::PLUGIN_REMOVE_BODY)));
    if let Some(error) = &prompt.error {
        dialog = dialog.child(error_view::alert(
            "plugin-remove",
            None,
            error,
            None,
            handle,
            cx,
        ));
    }
    let (cancel, confirm) = (tr(cx, keys::ACTION_CANCEL), tr(cx, keys::ACTION_REMOVE));
    let go = handle.clone();
    dialog
        .action({
            let handle = handle.clone();
            move |_| Layer::Overlay.cancel_button(cancel, handle)
        })
        .action(move |_| {
            tagged(
                REMOVE_CONFIRM,
                Button::new(REMOVE_CONFIRM, confirm)
                    .variant(ButtonVariant::Danger)
                    .on_click(move |_, _, cx| {
                        go.update(cx, |state, cx| state.confirm_remove_plugin(cx))
                    }),
            )
        })
        .into_any_element()
}
