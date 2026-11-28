//! Settings sections for core's `Defaults` and for where the CLI lives.
//!
//! Every field of `Defaults` has a row. Each change is saved to settings.json and pushed to the
//! backend (`AppState::update_defaults`).

use std::time::Duration;

use colab_playground_core::{Defaults, provision::MAX_ATTEMPTS_CAP};
use ely_gpui_component::{
    buttons::{Button, ButtonVariant},
    forms::{Choice, Input, NumberInput, Select, TextInput},
    layout::{Card, CardHeader},
    settings::{SettingsRow, SettingsSection},
    theme::{ActiveTheme, ControlSize},
};
use gpui::{App, Entity, IntoElement, ParentElement, SharedString, Styled, Window, div};

use crate::{
    backend,
    i18n::{keys, tr, tr_args},
    platform,
    settings::ControlPlane,
    state::AppState,
    ui::{Inputs, duration_field::DurationField, metrics, runtime_options::Controls, tagged},
};

/// The most runtimes per account the field accepts (input sanity, not a policy).
pub const MAX_CONCURRENT_FIELD: f64 = 20.0;

/// The selector around a defaults row's control: `def <field>`.
pub fn field_selector(field: &str) -> String {
    format!("def {field}")
}

/// The selector around an optional limit's switch: `def-switch <field>`.
pub fn switch_selector(field: &str) -> String {
    format!("def-switch {field}")
}

type Edit<T> = fn(&mut Defaults, T);

fn edit<T: 'static>(
    handle: &Entity<AppState>,
    apply: Edit<T>,
) -> impl Fn(T, &mut Window, &mut App) + 'static {
    let handle = handle.clone();
    move |value, _, cx| {
        handle.update(cx, |state, cx| {
            state.update_defaults(|d| apply(d, value), cx)
        })
    }
}

fn row(
    field: &str,
    title: &str,
    hint: Option<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> SettingsRow {
    let row = SettingsRow::new(tr(cx, title)).control(tagged(field_selector(field), control));
    match hint {
        Some(hint) => row.description(hint),
        None => row,
    }
}

/// A duration that is set to the second.
fn seconds_field(
    id: &'static str,
    value: Duration,
    on_change: impl Fn(Duration, &mut Window, &mut App) + 'static,
) -> DurationField {
    DurationField::new(id, value, on_change).seconds()
}

pub fn defaults_section(state: &AppState, handle: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let d = state.defaults();
    let core = Defaults::default();
    let theme = cx.theme();
    let controls = Controls {
        prefix: "def",
        switch_selector: Some(switch_selector),
        cx,
    };

    let max_runtime = controls.max_runtime(d.max_runtime, edit(handle, |d, v| d.max_runtime = v));
    let max_spend = controls.max_spend(d.max_spend_cu, edit(handle, |d, v| d.max_spend_cu = v));
    let min_balance =
        controls.min_balance(d.min_balance_cu, edit(handle, |d, v| d.min_balance_cu = v));
    let idle = controls.idle_timeout(d.idle_timeout, edit(handle, |d, v| d.idle_timeout = v));
    let concurrent = div().w(theme.menu_width()).child(
        NumberInput::new("def-concurrent", f64::from(d.max_concurrent_sessions))
            .range(1.0, MAX_CONCURRENT_FIELD)
            .precision(0)
            .step(1.0)
            .on_change(edit(handle, |d, v: f64| {
                d.max_concurrent_sessions = v.round() as u32
            })),
    );
    let attempts = controls.attempts(d.max_attempts, edit(handle, |d, v| d.max_attempts = v));
    let exhausted = controls.on_exhausted(d.on_exhausted, edit(handle, |d, v| d.on_exhausted = v));
    let accelerator = controls.accelerator(d.accelerator, edit(handle, |d, v| d.accelerator = v));
    let accelerator_hint = state
        .hardware_catalog()
        .blocks(d.accelerator, d.high_mem)
        .then(|| tr(cx, keys::PROV_INELIGIBLE));
    let high_mem = controls.high_mem(d.high_mem, edit(handle, |d, on| d.high_mem = on));
    let min_vram = controls.min_vram(d.min_vram_mib, edit(handle, |d, v| d.min_vram_mib = v));
    let reset = {
        let handle = handle.clone();
        tagged(
            "def-reset",
            Button::new("def-reset", tr(cx, keys::DEF_RESET))
                .variant(ButtonVariant::Ghost)
                .size(ControlSize::Sm)
                .disabled(*d == core)
                .on_click(move |_, _, cx| handle.update(cx, |state, cx| state.reset_defaults(cx))),
        )
    };
    SettingsSection::new(tr(cx, keys::DEFAULTS_TITLE))
        .description(tr(cx, keys::DEFAULTS_DESCRIPTION))
        .row(row(
            "max_runtime",
            keys::DEF_MAX_RUNTIME,
            Some(tr(cx, keys::DEF_MAX_RUNTIME_HINT)),
            max_runtime,
            cx,
        ))
        .row(row(
            "max_spend",
            keys::DEF_MAX_SPEND,
            Some(tr(cx, keys::DEF_MAX_SPEND_HINT)),
            max_spend,
            cx,
        ))
        .row(row(
            "min_balance",
            keys::DEF_MIN_BALANCE,
            Some(tr(cx, keys::DEF_MIN_BALANCE_HINT)),
            min_balance,
            cx,
        ))
        .row(row(
            "idle_timeout",
            keys::DEF_IDLE,
            Some(tr(cx, keys::DEF_IDLE_HINT)),
            idle,
            cx,
        ))
        .row(row(
            "max_concurrent",
            keys::DEF_CONCURRENT,
            Some(tr(cx, keys::DEF_CONCURRENT_HINT)),
            concurrent,
            cx,
        ))
        .row(row(
            "max_attempts",
            keys::DEF_ATTEMPTS,
            Some(tr_args(
                cx,
                keys::DEF_ATTEMPTS_HINT,
                &[("cap", &MAX_ATTEMPTS_CAP.to_string())],
            )),
            attempts,
            cx,
        ))
        .row(row(
            "retry_delay",
            keys::DEF_RETRY_DELAY,
            None,
            seconds_field(
                "def-retry-delay",
                d.retry_delay,
                edit(handle, |d, v: Duration| d.retry_delay = v),
            ),
            cx,
        ))
        .row(row(
            "on_exhausted",
            keys::DEF_ON_EXHAUSTED,
            None,
            exhausted,
            cx,
        ))
        .row(row(
            "accelerator",
            keys::DEF_ACCELERATOR,
            accelerator_hint,
            accelerator,
            cx,
        ))
        .row(row("high_mem", keys::DEF_HIGH_MEM, None, high_mem, cx))
        .row(row(
            "min_vram",
            keys::DEF_MIN_VRAM,
            Some(tr(cx, keys::DEF_MIN_VRAM_HINT)),
            min_vram,
            cx,
        ))
        .row(row(
            "winddown",
            keys::DEF_WINDDOWN,
            Some(tr(cx, keys::DEF_WINDDOWN_HINT)),
            seconds_field(
                "def-winddown",
                d.winddown_grace,
                edit(handle, |d, v: Duration| d.winddown_grace = v),
            ),
            cx,
        ))
        .row(row(
            "poll_active",
            keys::DEF_POLL_ACTIVE,
            Some(tr(cx, keys::DEF_POLL_HINT)),
            seconds_field(
                "def-poll-active",
                d.usage_poll_active,
                edit(handle, |d, v: Duration| d.usage_poll_active = v),
            ),
            cx,
        ))
        .row(row(
            "poll_idle",
            keys::DEF_POLL_IDLE,
            None,
            seconds_field(
                "def-poll-idle",
                d.usage_poll_idle,
                edit(handle, |d, v: Duration| d.usage_poll_idle = v),
            ),
            cx,
        ))
        .row(SettingsRow::new(tr(cx, keys::DEF_RESET)).control(reset))
}

/// Where the CLI lives: the WSL distribution (Windows) and the `colab` paths.
pub fn host_section(
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let simulated = backend::shows_demo_badge(state.flavor());
    let busy = !state.can_change_host();
    let plane = state.settings().control_plane;
    let pick_plane = handle.clone();
    let mut section =
        SettingsSection::new(tr(cx, keys::HOST_TITLE)).description(tr(cx, keys::HOST_DESCRIPTION));
    section = section.row(row(
        "control_plane",
        keys::SETTINGS_CONTROL_PLANE,
        Some(tr(cx, keys::SETTINGS_CONTROL_HINT)),
        div().w(theme.menu_width()).child(
            Select::new(
                "host-control-plane",
                vec![
                    Choice::new("cli", tr(cx, keys::SETTINGS_CONTROL_CLI)),
                    Choice::new("direct", tr(cx, keys::SETTINGS_CONTROL_DIRECT)),
                ],
            )
            .label(tr(cx, keys::SETTINGS_CONTROL_PLANE))
            .selected(if plane == ControlPlane::Direct {
                "direct"
            } else {
                "cli"
            })
            .disabled(busy)
            .on_change(move |value, _, cx| {
                let plane = if value == "direct" {
                    ControlPlane::Direct
                } else {
                    ControlPlane::Cli
                };
                pick_plane.update(cx, |state, cx| state.set_control_plane(plane, cx));
            }),
        ),
        cx,
    ));
    if platform::uses_wsl() {
        let distros: Vec<String> = state
            .probe()
            .ready()
            .map(|report| report.wsl_distros.clone())
            .unwrap_or_default();
        let current = state.settings().wsl_distro.clone();
        let mut choices = vec![Choice::new("", tr(cx, keys::HOST_WSL_DEFAULT))];
        choices.extend(distros.iter().map(|d| Choice::new(d.clone(), d.clone())));
        if let Some(current) = current.as_ref().filter(|c| !distros.contains(c)) {
            choices.push(Choice::new(current.clone(), current.clone()));
        }
        let pick = handle.clone();
        section = section.row(row(
            "wsl_distro",
            keys::HOST_WSL,
            None,
            div().w(theme.menu_width()).child(
                Select::new("host-wsl", choices)
                    .label(tr(cx, keys::HOST_WSL))
                    .selected(current.unwrap_or_default())
                    .disabled(busy)
                    .on_change(move |value, _, cx| {
                        let distro = (!value.is_empty()).then(|| value.to_string());
                        pick.update(cx, |state, cx| state.set_wsl_distro(distro, cx));
                    }),
            ),
            cx,
        ));
    }
    let path = |input: &Entity<TextInput>| {
        div()
            .w(metrics::path_field_width(theme))
            .child(Input::new(input))
    };
    let (apply, colab, python) = (
        handle.clone(),
        inputs.colab_path.clone(),
        inputs.colab_python.clone(),
    );
    // Idle on the real backend, the section's description right above says it all.
    let mut apply_row = SettingsRow::new(tr(cx, keys::HOST_APPLY));
    if simulated {
        apply_row = apply_row.description(tr(cx, keys::HOST_MOCK_NOTE));
    } else if busy {
        apply_row = apply_row.description(tr(cx, keys::HOST_BUSY));
    }
    section = section
        .row(row(
            "colab_path",
            keys::HOST_COLAB_PATH,
            None,
            path(&inputs.colab_path),
            cx,
        ))
        .row(row(
            "colab_python",
            keys::HOST_PYTHON_PATH,
            Some(tr(cx, keys::HOST_PYTHON_HINT)),
            path(&inputs.colab_python),
            cx,
        ))
        .row(
            apply_row.control(tagged(
                "host-apply",
                Button::new("host-apply", tr(cx, keys::HOST_APPLY))
                    .size(ControlSize::Sm)
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        let read =
                            |input: &Entity<TextInput>, cx: &App| input.read(cx).text().to_owned();
                        let (colab, python) = (read(&colab, cx), read(&python, cx));
                        apply.update(cx, |state, cx| state.apply_host_paths(&colab, &python, cx));
                    }),
            )),
        );
    section
}

/// Selector of the session-limit card shown at the top of Settings from the New session
/// dialog.
pub const LIMIT_FOCUS_SELECTOR: &str = "settings-limit-focus";

/// The session limit, called out at the top of Settings when the New session dialog sends
/// the user here.
pub fn limit_focus(state: &AppState, handle: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let d = state.defaults();
    tagged(
        LIMIT_FOCUS_SELECTOR,
        Card::new()
            .header(
                CardHeader::new(tr(cx, keys::DEF_CONCURRENT))
                    .description(tr(cx, keys::LIMIT_FOCUS_HINT)),
            )
            .child(
                div().w(theme.menu_width()).child(
                    NumberInput::new("def-concurrent-focus", f64::from(d.max_concurrent_sessions))
                        .range(1.0, MAX_CONCURRENT_FIELD)
                        .precision(0)
                        .step(1.0)
                        .on_change(edit(handle, |d, v: f64| {
                            d.max_concurrent_sessions = v.round() as u32
                        })),
                ),
            ),
    )
    .w_full()
}
