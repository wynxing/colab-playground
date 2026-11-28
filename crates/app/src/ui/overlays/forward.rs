//! Forward a port: remote and local port (digits only, so a port never reads "8,000") and an
//! optional health path. Port 8080 on the runtime is refused with the reason.

use std::rc::Rc;

use colab_playground_core::{ErrorKind, cli::RESERVED_VM_PORT};
use ely_gpui_component::{
    buttons::{Button, ButtonVariant},
    feedback::InlineMessage,
    forms::{Input, TextInput},
    overlays::Dialog,
    primitives::{IconName, Severity},
    theme::{ActiveTheme, ControlSize},
    typography::{Caption, Paragraph},
};
use gpui::{AnyElement, App, Entity, IntoElement, ParentElement, Styled, div};

use super::scrolled;
use crate::{
    i18n::{keys, tr, tr_args},
    state::{AppState, ForwardForm},
    ui::{
        Inputs, error_view,
        parts::{Layer, field},
        tagged,
    },
};

pub const FORWARD_SUBMIT: &str = "forward-submit";
pub const FORWARD_RESET_SSH: &str = "forward-reset-ssh";
/// Selector of the note shown for the reserved port.
pub const RESERVED_NOTE: &str = "forward-reserved";
pub const FORWARD_BODY: &str = "forward-body";

fn submit(handle: &Entity<AppState>, inputs: &Inputs, cx: &mut App) {
    let text = |input: &Entity<TextInput>, cx: &App| input.read(cx).text().to_owned();
    let (remote, local) = (
        text(&inputs.forward_remote, cx),
        text(&inputs.forward_local, cx),
    );
    let health = text(&inputs.health, cx).trim().to_owned();
    let health = (!health.is_empty()).then_some(health);
    handle.update(cx, |state, cx| {
        state.submit_forward(&remote, &local, health, cx)
    });
}

pub(super) fn render(
    form: &ForwardForm,
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let opening = state.is_opening_forward(&form.spec(None));
    let ports = div()
        .flex()
        .flex_wrap()
        .gap_4()
        .child(field(
            tr(cx, keys::FORWARD_REMOTE),
            tagged(
                "forward-remote",
                div()
                    .w(theme.menu_width())
                    .child(Input::new(&inputs.forward_remote)),
            ),
            cx,
        ))
        .child(field(
            tr(cx, keys::FORWARD_LOCAL),
            tagged(
                "forward-local",
                div()
                    .w(theme.menu_width())
                    .child(Input::new(&inputs.forward_local)),
            ),
            cx,
        ));
    let mut body = div()
        .flex()
        .flex_col()
        .gap_3()
        .child(Paragraph::new(tr_args(
            cx,
            keys::FORWARD_INTRO,
            &[("name", &form.session)],
        )))
        .child(InlineMessage::new(
            Severity::Info,
            tr(cx, keys::FORWARD_BILLING_NOTE),
        ));
    // The outcome of the last try goes first, so it is seen without scrolling.
    match &form.error {
        Some(error) if error.is(ErrorKind::PortReserved) => {
            body = body.child(tagged(
                RESERVED_NOTE,
                InlineMessage::new(
                    Severity::Danger,
                    tr_args(
                        cx,
                        keys::ERR_PORT_RESERVED,
                        &[("port", &RESERVED_VM_PORT.to_string())],
                    ),
                ),
            ));
        }
        Some(error) => {
            let retry: error_view::Retry = {
                let (handle, inputs) = (handle.clone(), inputs.clone());
                Rc::new(move |_, cx| submit(&handle, &inputs, cx))
            };
            body = body.child(error_view::alert(
                "forward",
                None,
                error,
                Some(retry),
                handle,
                cx,
            ));
            if error.is(ErrorKind::SshBusy) {
                let resetting = state.is_resetting_ssh(&form.target);
                let (reset_handle, reset_target) = (handle.clone(), form.target.clone());
                body = body.child(
                    div().child(tagged(
                        FORWARD_RESET_SSH,
                        Button::new(FORWARD_RESET_SSH, tr(cx, keys::RESET_SSH_ACTION))
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .loading(resetting)
                            .disabled(resetting)
                            .on_click(move |_, _, cx| {
                                reset_handle.update(cx, |state, cx| {
                                    state.reset_ssh_sessions(reset_target.clone(), cx)
                                })
                            }),
                    )),
                );
            }
        }
        None => {}
    }
    body = body.child(ports).child(field(
        tr(cx, keys::FORWARD_HEALTH),
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(tagged("forward-health", Input::new(&inputs.health)))
            .child(Caption::new(tr(cx, keys::FORWARD_HEALTH_HINT))),
        cx,
    ));
    let dialog = Dialog::new(
        "forward",
        tr(cx, keys::FORWARD_TITLE),
        Layer::Overlay.close(handle),
    )
    .child(scrolled(FORWARD_BODY, None, body));
    let (cancel, label) = (tr(cx, keys::ACTION_CANCEL), tr(cx, keys::FORWARD_ACTION));
    let (go, fields) = (handle.clone(), inputs.clone());
    dialog
        .action({
            let handle = handle.clone();
            move |_| Layer::Overlay.cancel_button(cancel, handle)
        })
        .action(move |_| {
            tagged(
                FORWARD_SUBMIT,
                Button::new(FORWARD_SUBMIT, label)
                    .primary()
                    .icon(IconName::Link)
                    .loading(opening)
                    .disabled(opening)
                    .on_click(move |_, _, cx| submit(&go, &fields, cx)),
            )
        })
        .into_any_element()
}
