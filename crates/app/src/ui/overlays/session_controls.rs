//! Confirmation for restarting a runtime's Jupyter kernel, and the IDE Remote-SSH helper
//! dialog (copyable `ProxyCommand` / isolated `-F` config snippet; never touches `~/.ssh/config`).

use colab_playground_core::tunnel::ssh_config_command;
use ely_gpui_component::{
    buttons::{Button, ButtonVariant, CopyButton},
    feedback::InlineMessage,
    layout::Well,
    overlays::Dialog,
    primitives::{IconName, Severity},
    theme::{ActiveTheme, ControlSize, TextSize},
    typography::{Caption, Paragraph},
};
use gpui::{AnyElement, App, Entity, IntoElement, ParentElement, Styled, div};

use super::scrolled;
use crate::{
    i18n::{keys, tr, tr_args},
    state::{AppState, Load, RemoteSshPrompt, RestartKernelPrompt},
    ui::{error_view, parts::{Layer, busy_line}, tagged},
};

pub const RESTART_KERNEL_CONFIRM: &str = "restart-kernel-confirm";
pub const REMOTE_SSH_BODY: &str = "remote-ssh-body";
pub const REMOTE_SSH_WRITE_TEMP: &str = "remote-ssh-write-temp";
pub const REMOTE_SSH_TEMP_COMMAND: &str = "remote-ssh-temp-command";
pub const REMOTE_SSH_RESET_SSH: &str = "remote-ssh-reset-ssh";

pub(super) fn restart_kernel(
    prompt: &RestartKernelPrompt,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let busy = state.is_restarting_kernel(&prompt.target);
    let (cancel, confirm) = (
        tr(cx, keys::ACTION_CANCEL),
        tr(cx, keys::RESTART_KERNEL_ACTION),
    );
    let mut dialog = Dialog::new(
        "restart-kernel",
        tr(cx, keys::RESTART_KERNEL_TITLE),
        Layer::Overlay.close(handle),
    )
    .child(Paragraph::new(tr_args(
        cx,
        keys::RESTART_KERNEL_BODY,
        &[("name", &prompt.label)],
    )))
    .child(InlineMessage::new(
        Severity::Info,
        tr(cx, keys::RESTART_KERNEL_FILES_NOTE),
    ));
    if let Some(error) = &prompt.error {
        dialog = dialog.child(error_view::alert(
            "restart-kernel",
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
            move |_| Layer::Overlay.cancel_button(cancel, handle)
        })
        .action(move |_| {
            tagged(
                RESTART_KERNEL_CONFIRM,
                Button::new(RESTART_KERNEL_CONFIRM, confirm)
                    .variant(ButtonVariant::Danger)
                    .icon(IconName::RotateCcw)
                    .loading(busy)
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        go.update(cx, |state, cx| state.confirm_restart_kernel(cx))
                    }),
            )
        })
        .into_any_element()
}

fn copy_well(
    id_prefix: &str,
    label: impl Into<gpui::SharedString>,
    text: String,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(Caption::new(label))
                .child(CopyButton::new(
                    gpui::SharedString::from(format!("{id_prefix}-copy")),
                    text.clone(),
                )),
        )
        .child(
            Well::new()
                .font_family(theme.mono_family.clone())
                .text_size(theme.text_size(TextSize::Xs))
                .child(text),
        )
        .into_any_element()
}

pub(super) fn remote_ssh(
    prompt: &RemoteSshPrompt,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let writing_temp = state.is_writing_temp_ssh_config(&prompt.target);
    let resetting_ssh = state.is_resetting_ssh(&prompt.target);
    let mut body = div()
        .flex()
        .flex_col()
        .gap_3()
        .child(Paragraph::new(tr_args(
            cx,
            keys::REMOTE_SSH_INTRO,
            &[("name", &prompt.label)],
        )))
        .child(InlineMessage::new(
            Severity::Info,
            tr(cx, keys::REMOTE_SSH_NO_CONFIG_EDIT),
        ));
    match &prompt.info {
        Load::Idle | Load::Loading => {
            body = body.child(busy_line("remote-ssh-loading", tr(cx, keys::SESSIONS_LOADING)));
        }
        Load::Failed(error) => {
            let retry_handle = handle.clone();
            let target = prompt.target.clone();
            let retry = std::rc::Rc::new(move |_: &mut gpui::Window, cx: &mut App| {
                retry_handle.update(cx, |state, cx| state.load_remote_ssh(target.clone(), cx));
            });
            body = body.child(error_view::alert(
                "remote-ssh",
                None,
                error,
                Some(retry),
                handle,
                cx,
            ));
        }
        Load::Ready(info) => {
            body = body
                .child(copy_well(
                    "remote-ssh-cmd",
                    tr(cx, keys::REMOTE_SSH_COMMAND_LABEL),
                    info.command.clone(),
                    cx,
                ))
                .child(copy_well(
                    "remote-ssh-cfg",
                    tr(cx, keys::REMOTE_SSH_CONFIG_LABEL),
                    info.config_snippet.clone(),
                    cx,
                ))
                .child(InlineMessage::new(
                    Severity::Warning,
                    tr(cx, keys::REMOTE_SSH_STOPPED_NOTE),
                ));
            let write_handle = handle.clone();
            let write_target = prompt.target.clone();
            body = body.child(
                div().flex().flex_wrap().items_center().gap_2().child(tagged(
                    REMOTE_SSH_WRITE_TEMP,
                    Button::new(REMOTE_SSH_WRITE_TEMP, tr(cx, keys::REMOTE_SSH_WRITE_TEMP))
                        .variant(ButtonVariant::Ghost)
                        .size(ControlSize::Sm)
                        .loading(writing_temp)
                        .disabled(writing_temp)
                        .on_click(move |_, _, cx| {
                            write_handle.update(cx, |state, cx| {
                                state.write_temp_ssh_config(write_target.clone(), cx)
                            })
                        }),
                )),
            );
            if let Some(error) = &prompt.temp_config_error {
                body = body.child(error_view::alert(
                    "remote-ssh-temp",
                    None,
                    error,
                    None,
                    handle,
                    cx,
                ));
            }
            if let Some(path) = &prompt.temp_config_path {
                let temp_cmd = ssh_config_command(path, &info.host_alias);
                body = body.child(tagged(
                    REMOTE_SSH_TEMP_COMMAND,
                    copy_well(
                        "remote-ssh-temp-cmd",
                        tr_args(cx, keys::REMOTE_SSH_TEMP_COMMAND, &[("path", path)]),
                        temp_cmd,
                        cx,
                    ),
                ));
            }
            let reset_handle = handle.clone();
            let reset_target = prompt.target.clone();
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(Caption::new(tr(cx, keys::REMOTE_SSH_RESET_HINT)))
                    .child(div().child(tagged(
                        REMOTE_SSH_RESET_SSH,
                        Button::new(REMOTE_SSH_RESET_SSH, tr(cx, keys::RESET_SSH_ACTION))
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .loading(resetting_ssh)
                            .disabled(resetting_ssh)
                            .on_click(move |_, _, cx| {
                                reset_handle.update(cx, |state, cx| {
                                    state.reset_ssh_sessions(reset_target.clone(), cx)
                                })
                            }),
                    ))),
            );
            if let Some(error) = &prompt.reset_error {
                body = body.child(error_view::alert(
                    "remote-ssh-reset",
                    None,
                    error,
                    None,
                    handle,
                    cx,
                ));
            }
        }
    }
    let close_label = tr(cx, keys::ACTION_CLOSE);
    Dialog::new(
        "remote-ssh",
        tr(cx, keys::REMOTE_SSH_TITLE),
        Layer::Overlay.close(handle),
    )
    .child(scrolled(REMOTE_SSH_BODY, None, body))
    .action({
        let handle = handle.clone();
        move |_| Layer::Overlay.cancel_button(close_label, handle)
    })
    .into_any_element()
}
