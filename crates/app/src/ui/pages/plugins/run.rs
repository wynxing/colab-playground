//! The run view: provisioning (when the run starts its own session), stages with progress,
//! the log, outputs, and once ready the endpoint cards (address, copy, health, forward
//! toggle, open, and a `curl` example for OpenAI-compatible endpoints). A failed run says
//! whether its runtime was stopped or is still billing, with Stop right there.

use colab_playground_core::{
    backend::PluginRunOutcome,
    plugins::{PluginInfo, ResolvedEndpoint},
    tunnel::{TunnelSpec, TunnelState},
    vm::RunState,
};
use ely_gpui_component::{
    buttons::{Button, ButtonVariant, CopyButton},
    data_display::{Badge, Tone},
    feedback::InlineMessage,
    forms::Switch,
    layout::{Card, CardHeader, Page, ScrollArea, Well},
    motion::ProgressBar,
    primitives::{IconName, Severity},
    terminal::AnsiText,
    theme::{ActiveTheme, ControlSize, TextSize},
    typography::Caption,
};
use gpui::{
    AnyElement, App, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, div,
};

use crate::{
    i18n::{keys, tr, tr_args},
    state::{AppState, Load, Op, PluginRun, RunPhase},
    ui::{
        error_view, metrics,
        parts::{cancel_work_button, step_icon, step_spinner},
        progress, tagged,
    },
};

/// Selector of Cancel while running.
pub const RUN_CANCEL: &str = "run-cancel";
/// Selector of "Stop and end billing" after a failure.
pub const RUN_STOP: &str = "run-stop";
/// Selector of the line saying the app could not check whether the run's runtime still runs.
pub const RUN_SESSION_UNKNOWN_SELECTOR: &str = "run-session-unknown";
/// Selector around an endpoint card: `endpoint <id>`.
pub const ENDPOINT_SELECTOR: &str = "endpoint";

pub(super) fn render(state: &AppState, handle: &Entity<AppState>, cx: &App) -> Page {
    let Some(run) = state.plugin_run() else {
        return Page::new("");
    };
    let info = state.plugin(&run.plugin);
    let name = info
        .map(|info| state.plugin_name(info))
        .unwrap_or_else(|| run.plugin.clone());
    let back = handle.clone();
    let mut page = Page::new(name)
        .subtitle(run.session.as_ref().map_or_else(
            || tr(cx, keys::RUN_SUBTITLE),
            |s| tr_args(cx, keys::RUN_ON_SESSION, &[("name", s)]),
        ))
        .action(
            Button::new("run-back", tr(cx, keys::ACTION_BACK))
                .variant(ButtonVariant::Ghost)
                .icon(IconName::ArrowLeft)
                .on_click(move |_, _, cx| back.update(cx, |state, cx| state.show_plugin_list(cx))),
        );
    let running = matches!(run.phase, RunPhase::Running);
    if run.provisions && !run.provision.events.is_empty() {
        let provisioning = running && run.session.is_none();
        page = page.child(
            Card::new()
                .header(
                    CardHeader::new(tr(cx, keys::RUN_PROVISIONING))
                        .description(progress::headline(&run.provision, cx)),
                )
                .child(progress::render(&run.provision, provisioning, cx))
                .children(
                    provisioning
                        .then(|| progress::no_answer_note(&run.provision, state.backend_now(), cx))
                        .flatten(),
                )
                .w_full(),
        );
    }
    if let Some(info) = info {
        page = page.child(stages(run, info, state, cx));
    }
    page = page.child(outcome(run, state, handle, cx));
    if let RunPhase::Ready(outcome) = &run.phase
        && let Some(info) = info
    {
        page = page.children(
            outcome
                .endpoints
                .iter()
                .map(|endpoint| endpoint_card(endpoint, outcome, info, state, handle, cx)),
        );
    }
    if let Some(status) = &run.status
        && !status.outputs.is_empty()
    {
        let theme = cx.theme();
        let rows = status.outputs.iter().map(|(key, value)| {
            let value = match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            div()
                .flex()
                .gap_3()
                .text_size(theme.text_size(TextSize::Sm))
                .child(
                    div()
                        .w(theme.label_width())
                        .text_color(theme.colors.fg_muted)
                        .child(key.clone()),
                )
                .child(div().flex_1().min_w_0().child(value))
        });
        page = page.child(
            Card::new()
                .header(CardHeader::new(tr(cx, keys::RUN_OUTPUTS)))
                .child(div().flex().flex_col().gap_1().children(rows))
                .w_full(),
        );
    }
    page.child(log(run, cx))
}

fn stages(run: &PluginRun, info: &PluginInfo, state: &AppState, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let tag = state.language().tag();
    let current = run.status.as_ref().and_then(|s| s.stage.clone());
    let current_index = current
        .as_ref()
        .and_then(|id| info.manifest.stages.iter().position(|s| &s.id == id));
    let ready = matches!(run.phase, RunPhase::Ready(_));
    let failed = matches!(run.phase, RunPhase::Failed(_) | RunPhase::Cancelled)
        || run
            .status
            .as_ref()
            .is_some_and(|s| s.state == RunState::Failed);
    let rows = info
        .manifest
        .stages
        .iter()
        .enumerate()
        .map(|(index, stage)| {
            let label = stage.label.get(tag).unwrap_or(&stage.id).to_owned();
            let is_current = Some(index) == current_index;
            let done = ready || current_index.is_some_and(|c| index < c);
            let marker = if done {
                step_icon(IconName::CircleCheck, theme.colors.success)
            } else if is_current && failed {
                step_icon(IconName::CircleX, theme.colors.danger)
            } else if is_current {
                step_spinner(("stage", index))
            } else {
                step_icon(IconName::Circle, theme.colors.fg_subtle)
            };
            let mut row = div().flex().flex_col().gap_1().child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(theme.text_size(TextSize::Sm))
                    .child(marker)
                    .child(
                        div()
                            .font_weight(if is_current {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::NORMAL
                            })
                            .child(label),
                    ),
            );
            if is_current && !done {
                if let Some(progress) = run.status.as_ref().and_then(|s| s.progress) {
                    row = row.child(div().pl_6().child(ProgressBar::new(
                        ("stage-progress", index),
                        progress.clamp(0.0, 1.0) as f32,
                    )));
                }
                if let Some(message) = run.status.as_ref().and_then(|s| s.message.clone()) {
                    row = row.child(div().pl_6().child(Caption::new(message)));
                }
            }
            tagged(format!("stage {}", stage.id), row)
        });
    Card::new()
        .header(CardHeader::new(tr(cx, keys::RUN_STAGES)))
        .child(div().flex().flex_col().gap_2().children(rows))
        .w_full()
        .into_any_element()
}

fn outcome(run: &PluginRun, state: &AppState, handle: &Entity<AppState>, cx: &App) -> AnyElement {
    let mut section = div().flex().flex_col().gap_2().w_full();
    match &run.phase {
        RunPhase::Running => {
            let cancelling = state.is_cancelling_run();
            let cancel = handle.clone();
            section = section.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(cancel_work_button(
                        RUN_CANCEL,
                        cancelling,
                        move |_, cx| cancel.update(cx, |state, cx| state.cancel_run(cx)),
                        cx,
                    ))
                    .child(Caption::new(tr(
                        cx,
                        if run.provisions {
                            keys::RUN_CANCEL_NOTE_NEW
                        } else {
                            keys::RUN_CANCEL_NOTE_EXISTING
                        },
                    ))),
            );
        }
        RunPhase::Ready(_) => {
            section = section.child(InlineMessage::new(
                Severity::Success,
                tr(cx, keys::RUN_READY),
            ));
        }
        RunPhase::Failed(error) => {
            section = section.child(error_view::alert(
                "run",
                Some(tr(cx, keys::RUN_FAILED)),
                error,
                None,
                handle,
                cx,
            ));
            section = section.child(billing_after_end(run, state, handle, cx));
        }
        RunPhase::Cancelled => {
            section = section.child(InlineMessage::new(
                Severity::Info,
                tr(cx, keys::RUN_CANCELLED),
            ));
            section = section.child(billing_after_end(run, state, handle, cx));
        }
    }
    if !matches!(run.phase, RunPhase::Running) {
        let (again, plugin) = (handle.clone(), run.plugin.clone());
        section = section.child(
            div().flex().child(tagged(
                "run-again",
                Button::new("run-again", tr(cx, keys::RUN_AGAIN))
                    .icon(IconName::RotateCcw)
                    .on_click(move |_, _, cx| {
                        again.update(cx, |state, cx| state.open_run_form(plugin.clone(), cx))
                    }),
            )),
        );
    }
    section.into_any_element()
}

/// After a failed or cancelled run: is anything still billing?
fn billing_after_end(
    run: &PluginRun,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    // Decided from the runtime list reloaded after the run ended, not from what the run
    // asked for: only the list says whether the runtime is really gone.
    if run.session.is_none() {
        return Caption::new(tr(cx, keys::RUN_NO_SESSION)).into_any_element();
    }
    if let Some(target) = state.run_runtime() {
        let stop = handle.clone();
        return div()
            .flex()
            .flex_col()
            .gap_2()
            .child(InlineMessage::new(
                Severity::Warning,
                tr(cx, keys::RUN_SESSION_BILLING),
            ))
            .child(
                div().flex().child(tagged(
                    RUN_STOP,
                    Button::new(RUN_STOP, tr(cx, keys::STOP_ACTION))
                        .variant(ButtonVariant::Danger)
                        .icon(IconName::Square)
                        .on_click(move |_, _, cx| {
                            stop.update(cx, |state, cx| state.request_stop(target.clone(), cx))
                        }),
                )),
            )
            .into_any_element();
    }
    if state.is_busy(Op::Runtimes) || state.runtimes().is_loading() {
        return Caption::new(tr(cx, keys::RUN_CHECKING)).into_any_element();
    }
    // Gone only if the list is complete for the run's account (any account, when the run did
    // not record one).
    let listed = match state.runtimes() {
        Load::Ready(view) => !view.errors.iter().any(|failure| {
            run.account
                .as_ref()
                .is_none_or(|account| &failure.account == account)
        }),
        _ => false,
    };
    if listed {
        tagged(
            "run-session-stopped",
            InlineMessage::new(Severity::Info, tr(cx, keys::RUN_SESSION_STOPPED)),
        )
        .into_any_element()
    } else {
        tagged(
            RUN_SESSION_UNKNOWN_SELECTOR,
            InlineMessage::new(Severity::Warning, tr(cx, keys::RUN_SESSION_UNKNOWN)),
        )
        .into_any_element()
    }
}

fn endpoint_card(
    endpoint: &ResolvedEndpoint,
    outcome: &PluginRunOutcome,
    info: &PluginInfo,
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let tag = state.language().tag();
    let label = endpoint.label.get(tag).unwrap_or(&endpoint.id).to_owned();
    let tunnel = state.tunnels().find(|t| {
        t.spec.account == outcome.account
            && t.spec.session == outcome.session
            && t.spec.remote_port == endpoint.remote_port
            && !t.state.is_terminal()
    });
    let local_port = tunnel.map_or(endpoint.remote_port, |t| t.spec.local_port);
    let base = format!("http://localhost:{local_port}");
    let url = format!("{base}{}", endpoint.path.trim_end_matches('/'));
    let open = tunnel.is_some_and(|t| t.state == TunnelState::Up);
    let health = match tunnel {
        None => Badge::new(tr(cx, keys::ENDPOINT_NOT_FORWARDED)),
        Some(t) => match (&t.state, t.healthy) {
            (TunnelState::Up, Some(true)) => {
                Badge::new(tr(cx, keys::ENDPOINT_HEALTHY)).tone(Tone::Success)
            }
            (TunnelState::Up, Some(false)) => {
                Badge::new(tr(cx, keys::ENDPOINT_UNHEALTHY)).tone(Tone::Warning)
            }
            (TunnelState::Up, None) => Badge::new(tr(cx, keys::TUNNEL_UP)).tone(Tone::Success),
            (TunnelState::Reconnecting { .. }, _) => {
                Badge::new(tr(cx, keys::TUNNEL_RECONNECTING_SHORT)).tone(Tone::Warning)
            }
            _ => Badge::new(tr(cx, keys::TUNNEL_STARTING)).tone(Tone::Info),
        },
    };
    let toggle = {
        let handle = handle.clone();
        let id = tunnel.map(|t| t.id);
        let spec = TunnelSpec {
            account: outcome.account.clone(),
            session: outcome.session.clone(),
            local_port: endpoint.remote_port,
            remote_port: endpoint.remote_port,
            health_path: endpoint.health.clone(),
        };
        Switch::new(
            SharedString::from(format!("endpoint-forward-{}", endpoint.id)),
            tunnel.is_some(),
        )
        .name(tr(cx, keys::ENDPOINT_FORWARD))
        .on_change(move |on, _, cx| {
            handle.update(cx, |state, cx| match (on, id) {
                (true, _) => state.open_tunnel(spec.clone(), false, cx),
                (false, Some(id)) => state.close_forward(id, cx),
                (false, None) => {}
            })
        })
    };
    let open_url = format!("{base}/");
    let mut card = Card::new()
        .header(CardHeader::new(label).action(health))
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .font_family(theme.mono_family.clone())
                        .text_size(theme.text_size(TextSize::Sm))
                        .child(url.clone()),
                )
                .child(CopyButton::new(
                    SharedString::from(format!("endpoint-copy-{}", endpoint.id)),
                    url.clone(),
                ))
                .child(
                    Button::new(
                        SharedString::from(format!("endpoint-open-{}", endpoint.id)),
                        tr(cx, keys::ACTION_OPEN_BROWSER),
                    )
                    .size(ControlSize::Sm)
                    .icon(IconName::ExternalLink)
                    .disabled(!open)
                    .on_click(move |_, _, cx| cx.open_url(&open_url)),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .flex_wrap()
                .child(tagged(format!("endpoint-forward {}", endpoint.id), toggle))
                .child(
                    div()
                        .text_size(theme.text_size(TextSize::Sm))
                        .child(tr(cx, keys::ENDPOINT_FORWARD)),
                )
                .child(Caption::new(tr(cx, keys::FORWARD_BILLING_NOTE))),
        );
    if endpoint.kind == "openai" {
        let model = outcome
            .status
            .outputs
            .get("model_id")
            .and_then(|v| v.as_str())
            .unwrap_or("MODEL")
            .to_owned();
        let has_key = info
            .manifest
            .inputs
            .iter()
            .any(|i| i.kind == colab_playground_core::plugins::InputKind::Secret);
        let auth = if has_key {
            " \\\n  -H \"Authorization: Bearer $API_KEY\""
        } else {
            ""
        };
        let curl = format!(
            "curl {url}/chat/completions \\\n  -H \"Content-Type: application/json\"{auth} \\\n  -d '{{\"model\": \"{model}\", \"messages\": [{{\"role\": \"user\", \"content\": \"Hello\"}}]}}'"
        );
        card = card.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(Caption::new(tr(cx, keys::ENDPOINT_CURL)))
                        .child(CopyButton::new(
                            SharedString::from(format!("endpoint-curl-{}", endpoint.id)),
                            curl.clone(),
                        )),
                )
                .child(
                    Well::new()
                        .font_family(theme.mono_family.clone())
                        .text_size(theme.text_size(TextSize::Xs))
                        .child(curl),
                )
                .children(has_key.then(|| Caption::new(tr(cx, keys::ENDPOINT_CURL_KEY)))),
        );
    }
    if endpoint.kind == "comfyui" {
        // The Comfy MCP server runs on the user's computer and drives the
        // ComfyUI behind this forward; the address is what it needs.
        let env = format!("COMFYUI_URL={base}");
        card = card.child(tagged(
            format!("endpoint-mcp {}", endpoint.id),
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(Caption::new(tr(cx, keys::ENDPOINT_MCP)))
                        .child(CopyButton::new(
                            SharedString::from(format!("endpoint-mcp-copy-{}", endpoint.id)),
                            env.clone(),
                        )),
                )
                .child(
                    Well::new()
                        .font_family(theme.mono_family.clone())
                        .text_size(theme.text_size(TextSize::Xs))
                        .child(env),
                )
                .child(Caption::new(tr(cx, keys::ENDPOINT_MCP_NOTE))),
        ));
    }
    tagged(format!("{ENDPOINT_SELECTOR} {}", endpoint.id), card)
        .w_full()
        .into_any_element()
}

fn log(run: &PluginRun, cx: &App) -> AnyElement {
    let theme = cx.theme();
    if run.log.is_empty() {
        return div().into_any_element();
    }
    let lines = run.log.lines().map(|line| AnsiText::new(line.to_owned()));
    Card::new()
        .header(CardHeader::new(tr(cx, keys::RUN_LOG)))
        .child(
            Well::new().h(metrics::RUN_LOG_HEIGHT).child(
                ScrollArea::new("run-log").vertical().size_full().child(
                    div()
                        .flex()
                        .flex_col()
                        .font_family(theme.mono_family.clone())
                        .children(lines),
                ),
            ),
        )
        .w_full()
        .into_any_element()
}
