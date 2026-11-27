//! Stop and end billing, confirmed once, stating that the runtime's data is wiped.

use ely_gpui_component::{
    buttons::{Button, ButtonVariant},
    feedback::InlineMessage,
    forms::Switch,
    overlays::Dialog,
    primitives::{IconName, Severity},
    typography::Paragraph,
};
use gpui::{AnyElement, App, Entity, IntoElement, ParentElement};

use crate::{
    format,
    i18n::{keys, tr, tr_args},
    state::{AppState, StopPrompt},
    ui::{error_view, parts::Layer, tagged},
};

pub const STOP_CONFIRM: &str = "stop-confirm";
pub const STOP_KEEP_TOGGLE: &str = "stop-save-keep";
pub const STOP_FORCE_CONFIRM: &str = "stop-force-confirm";

pub(super) fn render(
    prompt: &StopPrompt,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let email = format::email(cx, prompt.email.as_deref());
    let stopping = state.is_stopping(&prompt.target);
    let (cancel, confirm) = (tr(cx, keys::ACTION_CANCEL), tr(cx, keys::STOP_ACTION));
    let mut dialog = Dialog::new(
        "stop-runtime",
        tr(cx, keys::STOP_TITLE),
        Layer::Overlay.close(handle),
    )
    .child(Paragraph::new(tr_args(
        cx,
        keys::STOP_BODY,
        &[("name", &prompt.label), ("email", &email)],
    )))
    .child(InlineMessage::new(
        Severity::Warning,
        tr(cx, keys::STOP_WIPED),
    ));
    if prompt.has_keep_plan {
        let toggle = handle.clone();
        dialog = dialog.child(tagged(
            STOP_KEEP_TOGGLE,
            Switch::new(STOP_KEEP_TOGGLE, prompt.save_keep)
                .label(tr(cx, keys::STOP_KEEP_TOGGLE))
                .on_change(move |on, _, cx| {
                    toggle.update(cx, |state, cx| state.toggle_stop_save_keep(on, cx))
                }),
        ));
    }
    if let Some(error) = &prompt.error {
        dialog = dialog.child(error_view::alert("stop", None, error, None, handle, cx));
        if prompt.keep_failed {
            dialog = dialog.child(InlineMessage::new(
                Severity::Danger,
                tr(cx, keys::STOP_KEEP_FAILED_HINT),
            ));
        }
    }
    let go = handle.clone();
    let force_go = handle.clone();
    let keep_failed = prompt.keep_failed;
    let force_label = tr(cx, keys::STOP_FORCE_ACTION);
    let mut dialog = dialog
        .action({
            let handle = handle.clone();
            move |_| Layer::Overlay.cancel_button(cancel, handle)
        })
        .action(move |_| {
            tagged(
                STOP_CONFIRM,
                Button::new(STOP_CONFIRM, confirm)
                    .variant(ButtonVariant::Danger)
                    .icon(IconName::Square)
                    .loading(stopping)
                    .disabled(stopping)
                    .on_click(move |_, _, cx| go.update(cx, |state, cx| state.confirm_stop(cx))),
            )
        });
    if keep_failed {
        dialog = dialog.action(move |_| {
            tagged(
                STOP_FORCE_CONFIRM,
                Button::new(STOP_FORCE_CONFIRM, force_label)
                    .variant(ButtonVariant::Danger)
                    .icon(IconName::Square)
                    .loading(stopping)
                    .disabled(stopping)
                    .on_click(move |_, _, cx| {
                        force_go.update(cx, |state, cx| state.force_stop(cx))
                    }),
            )
        });
    }
    dialog.into_any_element()
}

pub const CANCEL_AUTO_STOP_CONFIRM: &str = "cancel-auto-stop-confirm";

/// Cancel auto-stop, confirmed once: afterwards the runtime bills until stopped by hand.
pub(super) fn cancel_auto_stop(
    prompt: &StopPrompt,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let busy = state.is_cancelling_auto_stop(&prompt.target);
    let (keep, confirm) = (
        tr(cx, keys::CANCEL_AUTO_STOP_KEEP),
        tr(cx, keys::RUNTIME_CANCEL_AUTO_STOP),
    );
    let mut dialog = Dialog::new(
        "cancel-auto-stop",
        tr(cx, keys::CANCEL_AUTO_STOP_TITLE),
        Layer::Overlay.close(handle),
    )
    .child(InlineMessage::new(
        Severity::Warning,
        tr_args(cx, keys::CANCEL_AUTO_STOP_BODY, &[("name", &prompt.label)]),
    ))
    .children(
        state
            .rearm_duration()
            .map(|_| Paragraph::new(tr(cx, keys::CANCEL_AUTO_STOP_REARM))),
    );
    if let Some(error) = &prompt.error {
        dialog = dialog.child(error_view::alert(
            "cancel-auto-stop",
            None,
            error,
            None,
            handle,
            cx,
        ));
    }
    let go = handle.clone();
    dialog
        .action({
            let handle = handle.clone();
            move |_| Layer::Overlay.cancel_button(keep, handle)
        })
        .action(move |_| {
            tagged(
                CANCEL_AUTO_STOP_CONFIRM,
                Button::new(CANCEL_AUTO_STOP_CONFIRM, confirm)
                    .variant(ButtonVariant::Danger)
                    .loading(busy)
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        go.update(cx, |state, cx| state.confirm_cancel_auto_stop(cx))
                    }),
            )
        })
        .into_any_element()
}
