//! One runtime: what it is, what it costs, when it stops, its forwards, and the actions it
//! allows. Stop works for every runtime on every account. Extend, cancelling auto-stop and
//! forwarding need the owning account to be active (the card offers to switch instead) and a
//! named runtime (runtimes opened elsewhere show them disabled with the reason).

use colab_playground_core::{
    ErrorKind,
    backend::Runtime,
    guard::{Concern, Limit},
    tunnel::{TunnelInfo, TunnelState},
};
use ely_gpui_component::{
    buttons::{Button, ButtonVariant, CopyButton, IconButton},
    data_display::{Badge, Tone},
    feedback::{Alert, InlineMessage},
    layout::Card,
    primitives::{IconName, Severity, Tooltip},
    theme::{ActiveTheme, ControlSize, TextSize},
    typography::Caption,
};
use gpui::{
    AnyElement, App, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, div,
};

use crate::{
    errors, format,
    i18n::{keys, tr, tr_args},
    state::{AppState, Op, key_of, local_url, runtime_label},
    ui::{
        parts::{captioned, figure},
        tagged,
    },
};

/// Selector around a runtime's card: `runtime <label>`.
pub fn runtime_selector(label: &str) -> String {
    format!("runtime {label}")
}
pub fn stop_selector(label: &str) -> String {
    format!("runtime-stop {label}")
}
pub fn extend_selector(label: &str) -> String {
    format!("runtime-extend {label}")
}
pub fn forward_selector(label: &str) -> String {
    format!("runtime-forward {label}")
}
pub fn cancel_auto_stop_selector(label: &str) -> String {
    format!("runtime-cancel-auto-stop {label}")
}
pub fn set_auto_stop_selector(label: &str) -> String {
    format!("runtime-set-auto-stop {label}")
}
pub fn switch_selector(label: &str) -> String {
    format!("runtime-switch {label}")
}
pub fn open_browser_selector(label: &str) -> String {
    format!("runtime-open-browser {label}")
}
pub fn restart_kernel_selector(label: &str) -> String {
    format!("runtime-restart-kernel {label}")
}
pub fn ide_ssh_selector(label: &str) -> String {
    format!("runtime-ide-ssh {label}")
}
pub fn reset_ssh_selector(label: &str) -> String {
    format!("runtime-reset-ssh {label}")
}

fn stat(label: SharedString, value: SharedString, cx: &App) -> impl IntoElement {
    captioned(label, figure(value, TextSize::Base, cx))
        .flex_1()
        .min_w(cx.theme().label_width())
        .gap_0p5()
}

pub(super) fn render(
    runtime: &Runtime,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let label = runtime_label(runtime);
    // Element ids come from this, not the label: two runtimes can share a label (the same
    // name on two accounts, or no name and the same hardware).
    let key = key_of(&runtime.id);
    let now = state.sessions_now();
    let unknown = || tr(cx, keys::SUMMARY_UNKNOWN);
    let tracked = runtime.tracked.as_ref();
    let hardware = tracked
        .and_then(|t| format::hardware(cx, t.gpu.as_deref(), t.vram_mib))
        .unwrap_or_else(|| runtime.hardware.clone().into());
    let uptime = match (tracked, now) {
        (Some(t), Some(now)) => format::duration(
            cx,
            now.duration_since(t.spend.started_at).unwrap_or_default(),
        ),
        _ => unknown(),
    };
    let rate = runtime
        .estimated_rate_cu_per_hour
        .map_or_else(unknown, |r| format::approx(cx, &format::cu_rate(cx, r)));
    let spend = runtime
        .estimated_spend_cu
        .map_or_else(unknown, |s| format::approx(cx, &format::cu(cx, s)));
    let auto_stop = match (runtime.auto_stop_at, now) {
        (Some(at), Some(now)) => tr_args(
            cx,
            keys::RUNTIME_AUTO_STOP_IN,
            &[(
                "time",
                &format::duration(cx, at.duration_since(now).unwrap_or_default()),
            )],
        ),
        (Some(_), None) => unknown(),
        (None, _) if runtime.managed => tr(cx, keys::RUNTIME_AUTO_STOP_OFF),
        (None, _) => tr(cx, keys::RUNTIME_AUTO_STOP_NONE),
    };
    let stopping = state.is_stopping(&runtime.id);
    let mut badges = div().flex().flex_wrap().gap_1p5();
    if !runtime.managed {
        badges = badges.child(Badge::new(tr(cx, keys::RUNTIME_UNMANAGED)).tone(Tone::Warning));
    }
    if runtime.high_mem {
        badges = badges.child(Badge::new(tr(cx, keys::RUNTIME_HIGH_MEM)));
    }
    if stopping {
        badges = badges.child(Badge::new(tr(cx, keys::RUNTIME_STOPPING)).tone(Tone::Info));
    }
    let header = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            div()
                .text_size(theme.text_size(TextSize::Lg))
                .font_weight(FontWeight::SEMIBOLD)
                .child(label.clone()),
        )
        .child(badges);
    let stats = div()
        .flex()
        .flex_wrap()
        .gap_4()
        .child(stat(tr(cx, keys::RUNTIME_HARDWARE), hardware, cx))
        .child(stat(tr(cx, keys::RUNTIME_UPTIME), uptime, cx))
        .child(stat(tr(cx, keys::RUNTIME_RATE), rate, cx))
        .child(stat(tr(cx, keys::RUNTIME_SPEND), spend, cx))
        .child(stat(tr(cx, keys::RUNTIME_AUTO_STOP), auto_stop, cx));
    let mut card = Card::new().child(header).child(stats);
    if !runtime.managed {
        card = card.child(Caption::new(tr(cx, keys::RUNTIME_UNMANAGED_HINT)));
    }
    let armed = tracked
        .and_then(|t| t.watchdog.as_ref())
        .and_then(|w| w.enforced_deadline())
        .is_some();
    if runtime.auto_stop_at.is_some() && !armed {
        // The time comes from the app-side limit alone; the quit prompt says the same.
        card = card.child(tagged(
            format!("auto-stop-app-only {label}"),
            InlineMessage::new(Severity::Warning, tr(cx, keys::RUNTIME_AUTO_STOP_APP_ONLY)),
        ));
    }
    if let Some(error) = state.guard_stop_failure(&runtime.id) {
        let retry = handle.clone();
        let target = runtime.id.clone();
        card = card.child(tagged(
            format!("guard-stop-failed {label}"),
            Alert::new(
                SharedString::from(format!("guard-stop-failed-{key}")),
                Severity::Danger,
                tr(cx, keys::GUARD_STILL_BILLING),
            )
            .body(errors::explain(error, false, cx).message)
            .action(
                Button::new(
                    SharedString::from(format!("guard-retry-{key}")),
                    tr(cx, keys::STOP_ACTION),
                )
                .size(ControlSize::Sm)
                .on_click(move |_, _, cx| {
                    retry.update(cx, |state, cx| state.request_stop(target.clone(), cx))
                }),
            ),
        ));
    }
    let warnings = state.guard_warnings(&runtime.id);
    if !warnings.is_empty() {
        let near_auto_stop = warnings.iter().any(|c| {
            matches!(
                c,
                Concern::Limit {
                    limit: Limit::Runtime,
                    ..
                }
            )
        });
        let unknown_only = warnings
            .iter()
            .all(|c| matches!(c, Concern::UsageUnknown { .. }));
        let mut message = div().flex().flex_col().gap_1().child(InlineMessage::new(
            if unknown_only {
                Severity::Info
            } else {
                Severity::Warning
            },
            tr_args(
                cx,
                keys::GUARD_WARN_LINE,
                &[("what", &format::concerns(cx, warnings))],
            ),
        ));
        if near_auto_stop
            && runtime.on_active_account
            && let Some(by) = state.rearm_duration()
        {
            let extend = handle.clone();
            let target = runtime.id.clone();
            message = message.child(
                div().child(
                    Button::new(
                        SharedString::from(format!("guard-extend-{key}")),
                        tr_args(
                            cx,
                            keys::RUNTIME_EXTEND_BY,
                            &[("time", &format::duration(cx, by))],
                        ),
                    )
                    .size(ControlSize::Sm)
                    .on_click(move |_, _, cx| {
                        extend.update(cx, |state, cx| state.extend_session(target.clone(), cx))
                    }),
                ),
            );
        }
        card = card.child(tagged(format!("guard-warning {label}"), message));
    }
    let tunnels = state.tunnels_for(&runtime.id);
    if !tunnels.is_empty() {
        let ssh_busy = runtime.on_active_account
            && runtime.id.name.is_some()
            && tunnels.iter().any(|tunnel| {
                matches!(
                    &tunnel.state,
                    TunnelState::Failed { error } if error.is(ErrorKind::SshBusy)
                )
            });
        let mut forwards = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(Caption::new(tr(cx, keys::RUNTIME_FORWARDS)))
            .children(
                tunnels
                    .into_iter()
                    .map(|tunnel| forward_row(tunnel, handle, cx)),
            );
        if ssh_busy {
            let resetting = state.is_resetting_ssh(&runtime.id);
            let (reset_handle, reset_target) = (handle.clone(), runtime.id.clone());
            forwards = forwards.child(div().child(tagged(
                reset_ssh_selector(&label),
                Button::new(
                    SharedString::from(format!("reset-ssh-{key}")),
                    tr(cx, keys::RESET_SSH_ACTION),
                )
                .variant(ButtonVariant::Ghost)
                .size(ControlSize::Sm)
                .loading(resetting)
                .disabled(resetting)
                .on_click(move |_, _, cx| {
                    reset_handle.update(cx, |state, cx| {
                        state.reset_ssh_sessions(reset_target.clone(), cx)
                    })
                }),
            )));
        }
        card = card.child(forwards);
    }
    card = card.child(actions(runtime, &label, stopping, state, handle, cx));
    tagged(runtime_selector(&label), card)
        .w_full()
        .into_any_element()
}

fn actions(
    runtime: &Runtime,
    label: &str,
    stopping: bool,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let target = runtime.id.clone();
    let key = key_of(&target);
    let stop = {
        let (handle, target) = (handle.clone(), target.clone());
        tagged(
            stop_selector(label),
            Button::new(
                SharedString::from(format!("stop-{key}")),
                tr(cx, keys::STOP_ACTION),
            )
            .variant(ButtonVariant::Danger)
            .icon(IconName::Square)
            .size(ControlSize::Sm)
            .loading(stopping)
            .disabled(stopping)
            .on_click(move |_, _, cx| {
                handle.update(cx, |state, cx| state.request_stop(target.clone(), cx))
            }),
        )
    };
    let mut row = div().flex().flex_wrap().items_center().gap_2().child(stop);
    if !runtime.on_active_account {
        let (handle, account) = (handle.clone(), runtime.id.account.clone());
        return row
            .child(tagged(
                switch_selector(label),
                Button::new(
                    SharedString::from(format!("switch-{key}")),
                    tr(cx, keys::RUNTIME_SWITCH),
                )
                .size(ControlSize::Sm)
                .icon(IconName::Link)
                .on_click(move |_, _, cx| {
                    handle.update(cx, |state, cx| state.request_switch(account.clone(), cx))
                }),
            ))
            .child(Caption::new(tr(cx, keys::RUNTIME_SWITCH_HINT)))
            .into_any_element();
    }
    let named = runtime.id.name.is_some();
    // Actions that cannot apply are left out; only forwarding stays visible (disabled, with
    // the reason) on runtimes this app cannot address by name.
    if named && runtime.auto_stop_at.is_some() {
        if let Some(by) = state.rearm_duration() {
            let (handle, target) = (handle.clone(), target.clone());
            let busy = state.is_busy(Op::Extend(key_of(&target)));
            row = row.child(tagged(
                extend_selector(label),
                Button::new(
                    SharedString::from(format!("extend-{key}")),
                    tr_args(
                        cx,
                        keys::RUNTIME_EXTEND_BY,
                        &[("time", &format::duration(cx, by))],
                    ),
                )
                .size(ControlSize::Sm)
                .loading(busy)
                .on_click(move |_, _, cx| {
                    handle.update(cx, |state, cx| state.extend_session(target.clone(), cx))
                }),
            ));
        }
        let (handle, target) = (handle.clone(), target.clone());
        row = row.child(tagged(
            cancel_auto_stop_selector(label),
            Button::new(
                SharedString::from(format!("cancel-auto-stop-{key}")),
                tr(cx, keys::RUNTIME_CANCEL_AUTO_STOP),
            )
            .variant(ButtonVariant::Ghost)
            .size(ControlSize::Sm)
            .on_click(move |_, _, cx| {
                handle.update(cx, |state, cx| {
                    state.request_cancel_auto_stop(target.clone(), cx)
                })
            }),
        ));
    } else if named
        && runtime.managed
        && let Some(by) = state.rearm_duration()
    {
        let (handle, target) = (handle.clone(), target.clone());
        let busy = state.is_busy(Op::Extend(key_of(&target)));
        row = row.child(tagged(
            set_auto_stop_selector(label),
            Button::new(
                SharedString::from(format!("set-auto-stop-{key}")),
                tr_args(
                    cx,
                    keys::RUNTIME_SET_AUTO_STOP,
                    &[("time", &format::duration(cx, by))],
                ),
            )
            .size(ControlSize::Sm)
            .loading(busy)
            .on_click(move |_, _, cx| {
                handle.update(cx, |state, cx| state.set_auto_stop(target.clone(), cx))
            }),
        ));
    }
    let (open_handle, open_target) = (handle.clone(), target.clone());
    let opening_browser = state.is_opening_browser(&target);
    let open_browser = Button::new(
        SharedString::from(format!("open-browser-{key}")),
        tr(cx, keys::OPEN_BROWSER_ACTION),
    )
    .variant(ButtonVariant::Ghost)
    .icon(IconName::ExternalLink)
    .size(ControlSize::Sm)
    .loading(opening_browser)
    .disabled(opening_browser)
    .on_click(move |_, _, cx| {
        open_handle.update(cx, |state, cx| {
            state.open_in_browser(open_target.clone(), cx)
        })
    });
    let (restart_handle, restart_target) = (handle.clone(), target.clone());
    let restarting_kernel = state.is_restarting_kernel(&target);
    let restart_kernel = Button::new(
        SharedString::from(format!("restart-kernel-{key}")),
        tr(cx, keys::RESTART_KERNEL_ACTION),
    )
    .variant(ButtonVariant::Ghost)
    .icon(IconName::RotateCcw)
    .size(ControlSize::Sm)
    .loading(restarting_kernel)
    .disabled(restarting_kernel)
    .on_click(move |_, _, cx| {
        restart_handle.update(cx, |state, cx| {
            state.request_restart_kernel(restart_target.clone(), cx)
        })
    });
    let (ide_handle, ide_target) = (handle.clone(), target.clone());
    let ide_ssh = Button::new(
        SharedString::from(format!("ide-ssh-{key}")),
        tr(cx, keys::REMOTE_SSH_ACTION),
    )
    .variant(ButtonVariant::Ghost)
    .icon(IconName::Terminal)
    .size(ControlSize::Sm)
    .on_click(move |_, _, cx| {
        ide_handle.update(cx, |state, cx| {
            state.open_remote_ssh_dialog(ide_target.clone(), cx)
        })
    });
    let (forward_handle, forward_target) = (handle.clone(), target.clone());
    let forward = Button::new(
        SharedString::from(format!("forward-{key}")),
        tr(cx, keys::FORWARD_TITLE),
    )
    .variant(ButtonVariant::Ghost)
    .icon(IconName::Link)
    .size(ControlSize::Sm)
    .on_click(move |_, _, cx| {
        forward_handle.update(cx, |state, cx| {
            state.open_forward_dialog(forward_target.clone(), cx)
        })
    });
    row.child(tagged(
        open_browser_selector(label),
        named_action(named, format!("open-browser-wrap-{key}"), open_browser, cx),
    ))
    .child(tagged(
        restart_kernel_selector(label),
        named_action(
            named,
            format!("restart-kernel-wrap-{key}"),
            restart_kernel,
            cx,
        ),
    ))
    .child(tagged(
        ide_ssh_selector(label),
        named_action(named, format!("ide-ssh-wrap-{key}"), ide_ssh, cx),
    ))
    .child(tagged(
        forward_selector(label),
        named_action(named, format!("forward-wrap-{key}"), forward, cx),
    ))
    .into_any_element()
}

fn named_action(named: bool, wrap_id: String, button: Button, cx: &App) -> AnyElement {
    if named {
        button.into_any_element()
    } else {
        div()
            .id(SharedString::from(wrap_id))
            .tooltip(Tooltip::text(tr(cx, keys::RUNTIME_UNNAMED_REASON)))
            .child(button.disabled(true))
            .into_any_element()
    }
}

/// One forward: state, address (copy, open) and close.
pub fn forward_row(tunnel: &TunnelInfo, handle: &Entity<AppState>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let url = local_url(&tunnel.spec);
    let (text, tone) = match &tunnel.state {
        TunnelState::Starting => (tr(cx, keys::TUNNEL_STARTING), Tone::Info),
        TunnelState::Up => match tunnel.healthy {
            Some(false) => (tr(cx, keys::TUNNEL_UP_UNHEALTHY), Tone::Warning),
            _ => (tr(cx, keys::TUNNEL_UP), Tone::Success),
        },
        TunnelState::Reconnecting { attempt, .. } => (
            tr_args(
                cx,
                keys::TUNNEL_RECONNECTING,
                &[("n", &attempt.to_string())],
            ),
            Tone::Warning,
        ),
        TunnelState::Failed { .. } => (tr(cx, keys::TUNNEL_FAILED), Tone::Danger),
        TunnelState::Stopped => (tr(cx, keys::TUNNEL_STOPPED), Tone::Neutral),
    };
    let id = tunnel.id;
    let close = handle.clone();
    let open_url = url.clone();
    let terminal = tunnel.state.is_terminal();
    let mut row = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .text_size(theme.text_size(TextSize::Sm))
        .child(Badge::new(text).tone(tone))
        .child(
            div()
                .font_family(theme.mono_family.clone())
                .child(format!("{url} → {}", tunnel.spec.remote_port)),
        );
    if !terminal {
        row = row
            .child(CopyButton::new(
                SharedString::from(format!("copy-tunnel-{}", id.0)),
                url.clone(),
            ))
            .child(
                IconButton::new(
                    SharedString::from(format!("open-tunnel-{}", id.0)),
                    IconName::ExternalLink,
                )
                .tooltip(tr(cx, keys::ACTION_OPEN_BROWSER))
                .variant(ButtonVariant::Ghost)
                .on_click(move |_, _, cx| cx.open_url(&open_url)),
            );
    }
    if let TunnelState::Failed { error } = &tunnel.state {
        row = row.child(Caption::new(errors::explain(error, false, cx).message));
    }
    row.child(tagged(
        format!("tunnel-close {}", tunnel.spec.local_port),
        IconButton::new(
            SharedString::from(format!("close-tunnel-{}", id.0)),
            IconName::X,
        )
        .tooltip(tr(
            cx,
            if terminal {
                keys::ACTION_DISMISS
            } else {
                keys::FORWARD_CLOSE
            },
        ))
        .variant(ButtonVariant::Ghost)
        .on_click(move |_, _, cx| {
            close.update(cx, |state, cx| {
                if terminal {
                    state.dismiss_forward(id, cx)
                } else {
                    state.close_forward(id, cx)
                }
            })
        }),
    ))
    .into_any_element()
}
