//! Compact RAM, VRAM and disk. A missing number stays blank: a failed poll
//! shows the last reading with an expired or unknown mark, never a zero bar.

use colab_playground_core::resources::{ResourceFault, used_ratio};
use ely_gpui_component::{
    data_display::{Badge, Tone},
    motion::ProgressBar,
    theme::{ActiveTheme, TextSize},
};
use gpui::{AnyElement, App, IntoElement, ParentElement, SharedString, Styled, div};

use crate::{
    format,
    i18n::{keys, tr, tr_args},
    state::ResourceView,
    ui::{
        parts::{captioned, figure},
        tagged,
    },
};

/// Selector around a session card's meters: `runtime-resources <label>`.
pub fn resources_selector(label: &str) -> String {
    format!("runtime-resources {label}")
}

/// Selector of the stale mark on that card.
pub fn resources_stale_selector(label: &str) -> String {
    format!("runtime-resources-stale {label}")
}

/// Selector of "no GPU" on that card, only while the reading is fresh.
pub fn resources_vram_none_selector(label: &str) -> String {
    format!("runtime-vram-none {label}")
}

pub fn session_meters(label: &str, view: Option<&ResourceView>, cx: &App) -> AnyElement {
    meters(
        resources_selector(label),
        resources_stale_selector(label),
        Some(resources_vram_none_selector(label)),
        view,
        cx,
    )
}

pub fn run_meters(view: Option<&ResourceView>, cx: &App) -> AnyElement {
    meters(
        crate::ui::pages::plugins::RUN_RESOURCES.to_owned(),
        crate::ui::pages::plugins::RUN_RESOURCES_STALE.to_owned(),
        None,
        view,
        cx,
    )
}

fn meters(
    selector: String,
    stale_selector: String,
    vram_none: Option<String>,
    view: Option<&ResourceView>,
    cx: &App,
) -> AnyElement {
    let reading = view.and_then(|view| view.reading.as_ref());
    let fault = view.and_then(|view| view.fault);
    let memory = reading.map(|reading| reading.memory);
    let summary = reading.map(|reading| reading.vram());
    let disk = reading.and_then(|reading| reading.primary_disk());
    // An empty GPU list on a fresh reading means the runtime has no GPU.
    // A fault must not turn a previous GPU into "none", nor a CPU into a zero bar.
    let no_gpu = fault.is_none() && reading.is_some_and(|reading| reading.gpus.is_empty());

    let ram_used = memory.and_then(|memory| memory.used_bytes());
    let ram_total = memory.and_then(|memory| memory.total_bytes);
    let vram_used = summary.as_ref().and_then(|summary| summary.used_bytes);
    let vram_total = summary.as_ref().and_then(|summary| summary.total_bytes);
    let disk_used = disk.and_then(|disk| disk.used_bytes);
    let disk_total = disk.and_then(|disk| disk.total_bytes);

    let mut vram_text = if no_gpu {
        tr(cx, keys::RES_NO_GPU)
    } else {
        pair_or_word(vram_used, vram_total, fault, cx)
    };
    if let Some(fraction) = summary.as_ref().and_then(|summary| summary.utilization) {
        vram_text = SharedString::from(format!("{vram_text} · {}", format::percent(cx, fraction)));
    }

    let vram = meter(
        tr(cx, keys::RES_VRAM),
        vram_text,
        used_ratio(vram_used, vram_total),
        SharedString::from(format!("{selector}-vram")),
        cx,
    );
    let vram = match vram_none.filter(|_| no_gpu) {
        Some(none) => tagged(none, vram).into_any_element(),
        None => vram.into_any_element(),
    };

    let mut row = div()
        .w_full()
        .flex()
        .flex_wrap()
        .gap_4()
        .child(meter(
            tr(cx, keys::RES_RAM),
            pair_or_word(ram_used, ram_total, fault, cx),
            used_ratio(ram_used, ram_total),
            SharedString::from(format!("{selector}-ram")),
            cx,
        ))
        .child(vram)
        .child(meter(
            tr(cx, keys::RES_DISK),
            pair_or_word(disk_used, disk_total, fault, cx),
            used_ratio(disk_used, disk_total),
            SharedString::from(format!("{selector}-disk")),
            cx,
        ));
    if show_stale(view) {
        row = row.child(tagged(
            stale_selector,
            Badge::new(stale_word(fault, cx)).tone(Tone::Warning),
        ));
    }
    tagged(selector, row).into_any_element()
}

fn meter(
    label: SharedString,
    value: SharedString,
    ratio: Option<f64>,
    id: SharedString,
    cx: &App,
) -> gpui::Div {
    let mut column = captioned(label, figure(value, TextSize::Base, cx))
        .flex_1()
        .min_w(cx.theme().label_width())
        .gap_0p5();
    if let Some(ratio) = ratio {
        column = column.child(ProgressBar::new(id, ratio.clamp(0.0, 1.0) as f32));
    }
    column
}

/// Both sides present: `used / total`. Otherwise the fault's word, never `0`.
fn pair_or_word(
    used: Option<u64>,
    total: Option<u64>,
    fault: Option<ResourceFault>,
    cx: &App,
) -> SharedString {
    match (used, total) {
        (Some(used), Some(total)) => {
            let used_text = format::bytes(cx, used);
            let total_text = format::bytes(cx, total);
            tr_args(
                cx,
                keys::RES_PAIR,
                &[("used", used_text.as_ref()), ("total", total_text.as_ref())],
            )
        }
        _ => stale_word(fault, cx),
    }
}

fn stale_word(fault: Option<ResourceFault>, cx: &App) -> SharedString {
    match fault {
        Some(ResourceFault::Expired) => tr(cx, keys::RES_EXPIRED),
        _ => tr(cx, keys::RES_UNKNOWN),
    }
}

/// Unavailable with no earlier reading is just "unknown", not a failed poll.
fn show_stale(view: Option<&ResourceView>) -> bool {
    let Some(view) = view else {
        return false;
    };
    match view.fault {
        Some(ResourceFault::Expired | ResourceFault::Unreachable) => true,
        Some(ResourceFault::Unavailable) => view.reading.is_some(),
        None => false,
    }
}
