//! An agent asked for something that spends money: the window puts the question, and the agent's
//! own response waits for the answer. Built the way the stop dialog is, and for the same reason —
//! what spends money is the user's decision, whoever asked.

use colab_playground_core::error::Denial;
use ely_gpui_component::{
    buttons::{Button, ButtonVariant},
    feedback::InlineMessage,
    overlays::Dialog,
    primitives::Severity,
    typography::Paragraph,
};
use gpui::{AnyElement, App, Entity, IntoElement, ParentElement};

use crate::{
    format,
    i18n::{keys, tr, tr_args},
    state::{AppState, ApprovalPrompt},
    ui::{parts::Layer, tagged},
};

/// The button that lets the call through.
pub const APPROVAL_ALLOW: &str = "mcp-approval-allow";
/// The button that turns it down.
pub const APPROVAL_REFUSE: &str = "mcp-approval-refuse";

pub(super) fn render(prompt: &ApprovalPrompt, handle: &Entity<AppState>, cx: &App) -> AnyElement {
    let (allow, refuse) = (handle.clone(), handle.clone());
    let (allowed, refused) = (tr(cx, keys::APPROVAL_ALLOW), tr(cx, keys::APPROVAL_REFUSE));
    let mut dialog = Dialog::new(
        "mcp-approval",
        tr(cx, keys::APPROVAL_TITLE),
        Layer::Overlay.close(handle),
    )
    .child(Paragraph::new(tr_args(
        cx,
        keys::APPROVAL_BODY,
        &[("action", &prompt.action), ("target", &prompt.target)],
    )))
    .child(Paragraph::new(tr_args(
        cx,
        keys::APPROVAL_CLIENT,
        &[("client", &prompt.client)],
    )));
    if let Some(rate) = prompt.units_per_hour {
        dialog = dialog.child(InlineMessage::new(
            Severity::Warning,
            tr_args(
                cx,
                keys::APPROVAL_RATE,
                &[("rate", &format::cu_rate(cx, rate))],
            ),
        ));
    }
    dialog
        .child(Paragraph::new(tr(cx, keys::APPROVAL_REFUSED)))
        .action(move |_| {
            tagged(
                APPROVAL_REFUSE,
                Button::new(APPROVAL_REFUSE, refused.clone())
                    .variant(ButtonVariant::Ghost)
                    .on_click(move |_, _, cx| {
                        refuse.update(cx, |state, cx| state.refuse_agent_call(Denial::Refused, cx))
                    }),
            )
        })
        .action(move |_| {
            tagged(
                APPROVAL_ALLOW,
                Button::new(APPROVAL_ALLOW, allowed)
                    .variant(ButtonVariant::Danger)
                    .on_click(move |_, _, cx| {
                        allow.update(cx, |state, cx| state.allow_agent_call(cx))
                    }),
            )
        })
        .into_any_element()
}
