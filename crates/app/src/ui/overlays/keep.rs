//! Keep Plan configuration, interactive Google Drive OAuth mount, and VM file upload dialogs.

use std::rc::Rc;

use ely_gpui_component::{
    buttons::{Button, ButtonVariant, CopyButton},
    feedback::InlineMessage,
    forms::{Input, Switch, TextInput},
    overlays::Dialog,
    primitives::{IconName, Severity},
    theme::{ActiveTheme, TextSize},
    typography::{Caption, MiddleEllipsis, Paragraph},
};
use gpui::{AnyElement, App, Entity, IntoElement, ParentElement, Styled, div};

use super::scrolled;
use crate::{
    i18n::{keys, tr, tr_args},
    state::{AppState, DriveMountDialog, DriveMountPhase, KeepPlanDialog, KeepPlanInputs, Load, VmUploadDialog},
    ui::{
        Inputs, error_view,
        parts::{Layer, busy_line, field},
        tagged,
    },
};

pub const KEEP_BODY: &str = "keep-body";
pub const KEEP_SUBMIT: &str = "keep-submit";
pub const KEEP_LOCAL_TOGGLE: &str = "keep-local-toggle";
pub const KEEP_HF_TOGGLE: &str = "keep-hf-toggle";
pub const KEEP_DRIVE_TOGGLE: &str = "keep-drive-toggle";
pub const KEEP_DRIVE_WARNING: &str = "keep-drive-warning";
pub const DRIVE_MOUNT_CONFIRM: &str = "drive-mount-confirm";
pub const DRIVE_MOUNT_OPEN_AGAIN: &str = "drive-mount-open-again";
pub const VM_UPLOAD_SUBMIT: &str = "vm-upload-submit";

fn submit_keep(handle: &Entity<AppState>, inputs: &Inputs, cx: &mut App) {
    let text = |input: &Entity<TextInput>, cx: &App| input.read(cx).text().to_owned();
    let draft = KeepPlanInputs {
        paths: text(&inputs.keep_paths, cx),
        local_folder: text(&inputs.keep_local_folder, cx),
        hf_repo: text(&inputs.keep_hf_repo, cx),
        hf_subpath: text(&inputs.keep_hf_subpath, cx),
        hf_interval: text(&inputs.keep_hf_interval, cx),
        drive_mount: text(&inputs.keep_drive_mount, cx),
        drive_subpath: text(&inputs.keep_drive_subpath, cx),
    };
    handle.update(cx, |state, cx| state.submit_keep_plan(draft, cx));
}

pub(super) fn keep_plan(
    dialog_state: &KeepPlanDialog,
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> AnyElement {
    let saving = state.is_saving_keep_plan(dialog_state.target.as_ref());
    let mut body = div()
        .flex()
        .flex_col()
        .gap_3()
        .child(Paragraph::new(tr(cx, keys::KEEP_SUBTITLE)));
    if let Some(error) = &dialog_state.error {
        let retry: error_view::Retry = {
            let (handle, inputs) = (handle.clone(), inputs.clone());
            Rc::new(move |_, cx| submit_keep(&handle, &inputs, cx))
        };
        body = body.child(error_view::alert(
            "keep-plan",
            None,
            error,
            Some(retry),
            handle,
            cx,
        ));
    }
    body = body.child(field(
        tr(cx, keys::KEEP_PATHS),
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(tagged("keep-paths", Input::new(&inputs.keep_paths)))
            .child(Caption::new(tr(cx, keys::KEEP_PATHS_HINT))),
        cx,
    ));

    let local_toggle = handle.clone();
    body = body.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(tagged(
                KEEP_LOCAL_TOGGLE,
                Switch::new(KEEP_LOCAL_TOGGLE, dialog_state.local_enabled)
                    .label(tr(cx, keys::KEEP_LOCAL_ENABLE))
                    .on_change(move |on, _, cx| {
                        local_toggle.update(cx, |state, cx| state.toggle_keep_local(on, cx))
                    }),
            ))
            .children(dialog_state.local_enabled.then(|| {
                tagged(
                    "keep-local-folder",
                    Input::new(&inputs.keep_local_folder),
                )
            })),
    );

    let hf_toggle = handle.clone();
    let mut hf_section = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(tagged(
            KEEP_HF_TOGGLE,
            Switch::new(KEEP_HF_TOGGLE, dialog_state.hf_enabled)
                .label(tr(cx, keys::KEEP_HF_ENABLE))
                .on_change(move |on, _, cx| {
                    hf_toggle.update(cx, |state, cx| state.toggle_keep_hf(on, cx))
                }),
        ));
    if dialog_state.hf_enabled {
        if !matches!(state.hf_token(), Load::Ready(true)) {
            hf_section = hf_section.child(InlineMessage::new(
                Severity::Warning,
                tr(cx, keys::KEEP_HF_NO_TOKEN),
            ));
        }
        hf_section = hf_section
            .child(tagged("keep-hf-repo", Input::new(&inputs.keep_hf_repo)))
            .child(tagged("keep-hf-subpath", Input::new(&inputs.keep_hf_subpath)))
            .child(tagged(
                "keep-hf-interval",
                Input::new(&inputs.keep_hf_interval),
            ));
    }
    body = body.child(hf_section);

    let drive_toggle = handle.clone();
    let mut drive_section = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(tagged(
            KEEP_DRIVE_TOGGLE,
            Switch::new(KEEP_DRIVE_TOGGLE, dialog_state.drive_enabled)
                .label(tr(cx, keys::KEEP_DRIVE_ENABLE))
                .on_change(move |on, _, cx| {
                    drive_toggle.update(cx, |state, cx| state.toggle_keep_drive(on, cx))
                }),
        ));
    if dialog_state.drive_enabled {
        drive_section = drive_section
            .child(tagged(
                KEEP_DRIVE_WARNING,
                InlineMessage::new(Severity::Warning, tr(cx, keys::KEEP_DRIVE_WARNING)),
            ))
            .child(tagged(
                "keep-drive-mount",
                Input::new(&inputs.keep_drive_mount),
            ))
            .child(tagged(
                "keep-drive-subpath",
                Input::new(&inputs.keep_drive_subpath),
            ));
    }
    body = body.child(drive_section);

    let dialog = Dialog::new(
        "keep-plan",
        tr(cx, keys::KEEP_TITLE),
        Layer::Overlay.close(handle),
    )
    .child(scrolled(KEEP_BODY, None, body));
    let (cancel, save) = (tr(cx, keys::ACTION_CANCEL), tr(cx, keys::KEEP_SAVE));
    let (go, fields) = (handle.clone(), inputs.clone());
    dialog
        .action({
            let handle = handle.clone();
            move |_| Layer::Overlay.cancel_button(cancel, handle)
        })
        .action(move |_| {
            tagged(
                KEEP_SUBMIT,
                Button::new(KEEP_SUBMIT, save)
                    .primary()
                    .icon(IconName::CircleCheck)
                    .loading(saving)
                    .disabled(saving)
                    .on_click(move |_, _, cx| submit_keep(&go, &fields, cx)),
            )
        })
        .into_any_element()
}

pub(super) fn drive_mount(
    dialog_state: &DriveMountDialog,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let dialog = Dialog::new(
        "drive-mount",
        tr(cx, keys::KEEP_DRIVE_MODAL_TITLE),
        Layer::Overlay.close(handle),
    );
    let cancel = tr(cx, keys::ACTION_CANCEL);
    let cancel_action = {
        let handle = handle.clone();
        move |_| Layer::Overlay.cancel_button(cancel, handle)
    };
    let mut dialog = dialog.child(tagged(
        KEEP_DRIVE_WARNING,
        InlineMessage::new(Severity::Warning, tr(cx, keys::KEEP_DRIVE_WARNING)),
    ));
    match &dialog_state.phase {
        DriveMountPhase::Starting => dialog
            .child(busy_line(
                "drive-mount-starting",
                tr(cx, keys::KEEP_DRIVE_MODAL_STARTING),
            ))
            .action(cancel_action)
            .into_any_element(),
        DriveMountPhase::Consent { url } | DriveMountPhase::Confirming { url } => {
            let confirming = matches!(dialog_state.phase, DriveMountPhase::Confirming { .. })
                || state.is_mounting_drive(&dialog_state.target);
            let theme = cx.theme();
            let open = url.clone();
            let link = div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(theme.mono_family.clone())
                        .text_size(theme.text_size(TextSize::Xs))
                        .text_color(theme.colors.fg_muted)
                        .child(MiddleEllipsis::new(url.clone())),
                )
                .child(CopyButton::new("drive-copy-link", url.clone()))
                .child(tagged(
                    DRIVE_MOUNT_OPEN_AGAIN,
                    Button::new(DRIVE_MOUNT_OPEN_AGAIN, tr(cx, keys::KEEP_DRIVE_MODAL_OPEN_AGAIN))
                        .variant(ButtonVariant::Ghost)
                        .icon(IconName::ExternalLink)
                        .on_click(move |_, _, cx| cx.open_url(&open)),
                ));
            let go = handle.clone();
            let confirm_label = tr(cx, keys::KEEP_DRIVE_MODAL_CONFIRM);
            dialog = dialog
                .child(Paragraph::new(tr_args(
                    cx,
                    keys::KEEP_DRIVE_MODAL_BODY,
                    &[
                        ("name", &dialog_state.label),
                        ("path", &dialog_state.mount_path),
                    ],
                )))
                .child(link);
            if confirming {
                dialog = dialog.child(busy_line(
                    "drive-mount-confirming",
                    tr_args(
                        cx,
                        keys::KEEP_DRIVE_MODAL_CONFIRMING,
                        &[("name", &dialog_state.label)],
                    ),
                ));
            }
            dialog
                .action(cancel_action)
                .action(move |_| {
                    tagged(
                        DRIVE_MOUNT_CONFIRM,
                        Button::new(DRIVE_MOUNT_CONFIRM, confirm_label)
                            .primary()
                            .icon(IconName::CircleCheck)
                            .loading(confirming)
                            .disabled(confirming)
                            .on_click(move |_, _, cx| {
                                go.update(cx, |state, cx| state.confirm_drive_mount(cx))
                            }),
                    )
                })
                .into_any_element()
        }
        DriveMountPhase::Failed { error } => {
            let retry: error_view::Retry = {
                let handle = handle.clone();
                let target = dialog_state.target.clone();
                let path = dialog_state.mount_path.clone();
                Rc::new(move |_, cx| {
                    handle.update(cx, |state, cx| {
                        state.begin_drive_mount(target.clone(), Some(path.clone()), cx)
                    })
                })
            };
            dialog
                .child(error_view::alert(
                    "drive-mount",
                    None,
                    error,
                    Some(retry),
                    handle,
                    cx,
                ))
                .action(cancel_action)
                .into_any_element()
        }
    }
}

fn submit_upload(handle: &Entity<AppState>, inputs: &Inputs, cx: &mut App) {
    let local = inputs.upload_local.read(cx).text().to_owned();
    let remote = inputs.upload_remote.read(cx).text().to_owned();
    handle.update(cx, |state, cx| state.submit_upload(&local, &remote, cx));
}

pub(super) fn vm_upload(
    dialog_state: &VmUploadDialog,
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> AnyElement {
    let uploading = state.is_uploading(&dialog_state.target);
    let mut body = div()
        .flex()
        .flex_col()
        .gap_3()
        .child(Paragraph::new(tr(cx, keys::VM_UPLOAD_INTRO)));
    if let Some(error) = &dialog_state.error {
        let retry: error_view::Retry = {
            let (handle, inputs) = (handle.clone(), inputs.clone());
            Rc::new(move |_, cx| submit_upload(&handle, &inputs, cx))
        };
        body = body.child(error_view::alert(
            "vm-upload",
            None,
            error,
            Some(retry),
            handle,
            cx,
        ));
    }
    body = body
        .child(field(
            tr(cx, keys::VM_UPLOAD_LOCAL),
            tagged("vm-upload-local", Input::new(&inputs.upload_local)),
            cx,
        ))
        .child(field(
            tr(cx, keys::VM_UPLOAD_REMOTE),
            tagged("vm-upload-remote", Input::new(&inputs.upload_remote)),
            cx,
        ));
    let (cancel, upload) = (tr(cx, keys::ACTION_CANCEL), tr(cx, keys::VM_UPLOAD_SUBMIT));
    let (go, fields) = (handle.clone(), inputs.clone());
    Dialog::new(
        "vm-upload",
        tr_args(
            cx,
            keys::VM_UPLOAD_TITLE,
            &[("name", &dialog_state.label)],
        ),
        Layer::Overlay.close(handle),
    )
    .child(scrolled("vm-upload-body", None, body))
    .action({
        let handle = handle.clone();
        move |_| Layer::Overlay.cancel_button(cancel, handle)
    })
    .action(move |_| {
        tagged(
            VM_UPLOAD_SUBMIT,
            Button::new(VM_UPLOAD_SUBMIT, upload)
                .primary()
                .icon(IconName::Plus)
                .loading(uploading)
                .disabled(uploading)
                .on_click(move |_, _, cx| submit_upload(&go, &fields, cx)),
        )
    })
    .into_any_element()
}
