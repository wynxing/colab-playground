//! The options for a new runtime, shared by the New session dialog and the plugin run form:
//! hardware, the minimum GPU memory (with the A100 40/80 GB explanation), attempts, what to
//! do when they run out, and the cost limits. Values start from `Defaults`; edits apply to
//! this one action only. Each control's wrapper carries the selector `prov <field>`.

use std::{rc::Rc, time::SystemTime};

use colab_playground_core::{
    provision::ProvisionSpec,
    usage::{RateReading, published_rate},
};
use ely_gpui_component::{layout::bring_into_view, theme::ActiveTheme, typography::Caption};
use gpui::{
    AnyElement, App, Axis, IntoElement, ParentElement, RenderOnce, ScrollHandle, SharedString,
    Styled, Window, canvas, div, prelude::FluentBuilder,
};

use crate::{
    format,
    i18n::{keys, tr, tr_args},
    ui::{parts::field_label, runtime_options::Controls, tagged},
};

pub type SpecEdit = Rc<dyn Fn(&mut App, &dyn Fn(&mut ProvisionSpec))>;

pub fn field_selector(field: &str) -> String {
    format!("prov {field}")
}

/// The burn rate's selector carries the value it draws, so a test can tell what the row reads.
pub fn rate_selector(value: &str) -> String {
    format!("{} {value}", field_selector("rate"))
}

fn row(
    field: &str,
    label: SharedString,
    hint: Option<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_3()
        .py_1()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .flex_1()
                .min_w(theme.label_width())
                .child(field_label(label, cx))
                .children(hint.map(Caption::new)),
        )
        .child(tagged(field_selector(field), control))
        .into_any_element()
}

/// Turning a limit on can wrap its value onto a new line below the visible part. The row is
/// brought into view after layout: bounds read at the click predate the new line.
#[derive(IntoElement)]
struct Reveal {
    id: SharedString,
    on: bool,
    scroll: ScrollHandle,
    row: AnyElement,
}

impl RenderOnce for Reveal {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // What the switch showed last frame; it starts as now, so opening the form moves
        // nothing.
        let on = self.on;
        let was = window.use_keyed_state(self.id, cx, |_, _| on);
        let turned_on = on && !*was.read(cx);
        if *was.read(cx) != on {
            was.update(cx, |was, _| *was = on);
        }
        let scroll = self.scroll;
        div().relative().child(self.row).when(turned_on, |row| {
            row.child(
                canvas(
                    move |bounds, window, _| {
                        if bring_into_view(&scroll, bounds, Axis::Vertical) {
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
        })
    }
}

fn set<T: Copy + 'static>(
    edit: &SpecEdit,
    apply: fn(&mut ProvisionSpec, T),
) -> impl Fn(T, &mut Window, &mut App) + 'static {
    let edit = edit.clone();
    move |value, _, cx| edit(cx, &|spec| apply(spec, value))
}

/// The options for `spec`; `prefix` keeps element ids apart when two forms exist. With
/// `scroll` (the area the form scrolls in), a limit switched on brings its row into view.
/// `rate` is `AppState::measured_rate` of `spec`.
pub fn render(
    prefix: &'static str,
    spec: &ProvisionSpec,
    rate: Option<(RateReading, SystemTime)>,
    edit: SpecEdit,
    scroll: Option<&ScrollHandle>,
    ineligible: bool,
    cx: &App,
) -> AnyElement {
    let controls = Controls {
        prefix,
        switch_selector: None,
        cx,
    };
    let accelerator =
        controls.accelerator(spec.accelerator, set(&edit, |spec, v| spec.accelerator = v));
    let high_mem = controls.high_mem(spec.high_mem, set(&edit, |spec, on| spec.high_mem = on));
    let gpu = spec.accelerator.is_gpu();
    let min_vram = controls.min_vram(
        spec.min_vram_mib,
        set(&edit, |spec, v| spec.min_vram_mib = v),
    );
    let attempts = controls.attempts(spec.attempts(), set(&edit, |spec, v| spec.max_attempts = v));
    let exhausted = controls.on_exhausted(
        spec.on_exhausted,
        set(&edit, |spec, v| spec.on_exhausted = v),
    );
    let guard = &spec.guard;
    let max_runtime = controls.max_runtime(
        guard.max_runtime,
        set(&edit, |spec, v| spec.guard.max_runtime = v),
    );
    let max_spend = controls.max_spend(
        guard.max_spend_cu,
        set(&edit, |spec, v| spec.guard.max_spend_cu = v),
    );
    let min_balance = controls.min_balance(
        guard.min_balance_cu,
        set(&edit, |spec, v| spec.guard.min_balance_cu = v),
    );
    let idle = controls.idle_timeout(
        guard.idle_timeout,
        set(&edit, |spec, v| spec.guard.idle_timeout = v),
    );
    // What the runtime being set up will burn is not known before it runs: the figure is the
    // one measured on the same hardware, or one other users published until then, so it is
    // marked as an estimate.
    let rate_text = |rate: f64| format::approx(cx, &format::cu_rate(cx, rate));
    let (rate_value, rate_hint) = match rate {
        Some((reading, now)) => (
            rate_text(reading.cu_per_hour),
            tr_args(
                cx,
                keys::PROV_RATE_MEASURED,
                &[("age", &format::age(cx, reading.at, now))],
            ),
        ),
        None => (
            rate_text(published_rate(spec.accelerator, spec.high_mem)),
            tr(cx, keys::PROV_RATE_PUBLISHED),
        ),
    };
    let limit_row = |field: &'static str,
                     on: bool,
                     label: SharedString,
                     hint: Option<SharedString>,
                     control: AnyElement| {
        let row = row(field, label, hint, control, cx);
        match scroll {
            Some(scroll) => Reveal {
                id: SharedString::from(format!("{prefix}-reveal-{field}")),
                on,
                scroll: scroll.clone(),
                row,
            }
            .into_any_element(),
            None => row,
        }
    };
    let mut rows = vec![
        row(
            "accelerator",
            tr(cx, keys::DEF_ACCELERATOR),
            ineligible.then(|| tr(cx, keys::PROV_INELIGIBLE)),
            accelerator,
            cx,
        ),
        row("high_mem", tr(cx, keys::DEF_HIGH_MEM), None, high_mem, cx),
        row(
            "rate",
            tr(cx, keys::RUNTIME_RATE),
            Some(rate_hint),
            tagged(rate_selector(&rate_value), div().child(rate_value)),
            cx,
        ),
    ];
    if gpu {
        rows.push(limit_row(
            "min_vram",
            spec.min_vram_mib.is_some(),
            tr(cx, keys::DEF_MIN_VRAM),
            Some(tr(cx, keys::PROV_VRAM_LOTTERY)),
            min_vram,
        ));
    }
    rows.extend([
        row(
            "attempts",
            tr(cx, keys::DEF_ATTEMPTS),
            Some(tr(cx, keys::PROV_ATTEMPTS_BILLED)),
            attempts,
            cx,
        ),
        row(
            "on_exhausted",
            tr(cx, keys::DEF_ON_EXHAUSTED),
            None,
            exhausted,
            cx,
        ),
        limit_row(
            "max_runtime",
            guard.max_runtime.is_some(),
            tr(cx, keys::DEF_MAX_RUNTIME),
            Some(tr(cx, keys::DEF_MAX_RUNTIME_HINT)),
            max_runtime,
        ),
        limit_row(
            "max_spend",
            guard.max_spend_cu.is_some(),
            tr(cx, keys::DEF_MAX_SPEND),
            None,
            max_spend,
        ),
        limit_row(
            "min_balance",
            guard.min_balance_cu.is_some(),
            tr(cx, keys::DEF_MIN_BALANCE),
            None,
            min_balance,
        ),
        limit_row(
            "idle_timeout",
            guard.idle_timeout.is_some(),
            tr(cx, keys::DEF_IDLE),
            None,
            idle,
        ),
    ]);
    let mut form = div().flex().flex_col().gap_1().children(rows);
    if ineligible {
        form = form.child(tagged(
            "prov accelerator-ineligible",
            Caption::new(tr(cx, keys::PROV_INELIGIBLE)),
        ));
    }
    form.into_any_element()
}
