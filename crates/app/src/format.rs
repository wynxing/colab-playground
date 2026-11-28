//! Numbers, durations and ages in the shown language.

use std::time::{Duration, SystemTime};

use colab_playground_core::{
    backend::InstallMethod,
    guard::{Concern, Limit},
    sessions::Accelerator,
};
use ely_gpui_component::i18n::I18n;
use gpui::{App, SharedString};

use crate::i18n::{keys, tr, tr_args};

/// `value` with the shown locale's separators and `decimals` places.
pub fn number(cx: &App, value: f64, decimals: usize) -> String {
    cx.global::<I18n>().locale().number(value, decimals)
}

/// Compute units, such as `87.5 CU`.
pub fn cu(cx: &App, value: f64) -> SharedString {
    tr_args(cx, keys::FMT_CU, &[("value", &number(cx, value, 1))])
}

/// A burn rate, such as `1.2 CU/h`.
pub fn cu_rate(cx: &App, value: f64) -> SharedString {
    tr_args(cx, keys::FMT_CU_RATE, &[("value", &number(cx, value, 2))])
}

/// An estimate, marked `≈`.
pub fn approx(cx: &App, value: &str) -> SharedString {
    tr_args(cx, keys::FMT_APPROX, &[("value", value)])
}

/// A duration in words, rounded to minutes (days and hours past a day).
pub fn duration(cx: &App, duration: Duration) -> SharedString {
    let minutes = duration.as_secs() / 60;
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    if days > 0 && hours == 0 {
        tr_args(cx, keys::FMT_DAYS, &[("d", &days.to_string())])
    } else if days > 0 {
        tr_args(
            cx,
            keys::FMT_DAYS_HOURS,
            &[("d", &days.to_string()), ("h", &hours.to_string())],
        )
    } else if hours > 0 && mins == 0 {
        tr_args(cx, keys::FMT_HOURS, &[("h", &hours.to_string())])
    } else if hours > 0 {
        tr_args(
            cx,
            keys::FMT_HOURS_MINUTES,
            &[("h", &hours.to_string()), ("m", &mins.to_string())],
        )
    } else if mins > 0 {
        tr_args(cx, keys::FMT_MINUTES, &[("m", &mins.to_string())])
    } else {
        tr(cx, keys::FMT_UNDER_MINUTE)
    }
}

/// How long ago `then` was at `now` (both on the backend's clock).
pub fn age(cx: &App, then: SystemTime, now: SystemTime) -> SharedString {
    let minutes = now.duration_since(then).unwrap_or_default().as_secs() / 60;
    match minutes {
        0 => tr(cx, keys::FMT_JUST_NOW),
        1..60 => tr_args(cx, keys::FMT_MINUTES_AGO, &[("n", &minutes.to_string())]),
        60..1440 => tr_args(
            cx,
            keys::FMT_HOURS_AGO,
            &[("n", &(minutes / 60).to_string())],
        ),
        _ => tr_args(
            cx,
            keys::FMT_DAYS_AGO,
            &[("n", &(minutes / 1440).to_string())],
        ),
    }
}

/// An account's email, or "Unknown email" for an account that has none.
pub fn email(cx: &App, email: Option<&str>) -> String {
    email.map_or_else(
        || tr(cx, keys::ACCOUNTS_NO_EMAIL).to_string(),
        str::to_owned,
    )
}

pub fn list(cx: &App, items: &[String]) -> String {
    items.join(&tr(cx, keys::FMT_LIST_SEPARATOR))
}

pub fn accelerator_key(accelerator: Accelerator) -> &'static str {
    match accelerator {
        Accelerator::None => keys::ACC_NONE,
        Accelerator::T4 => keys::ACC_T4,
        Accelerator::L4 => keys::ACC_L4,
        Accelerator::G4 => keys::ACC_G4,
        Accelerator::A100 => keys::ACC_A100,
        Accelerator::H100 => keys::ACC_H100,
        Accelerator::TpuV5e1 => keys::ACC_V5E1,
        Accelerator::TpuV6e1 => keys::ACC_V6E1,
    }
}

pub fn install_method_key(method: InstallMethod) -> &'static str {
    match method {
        InstallMethod::Uv => keys::INSTALL_UV,
        InstallMethod::InstallUvThenCli => keys::INSTALL_UV_FIRST,
        InstallMethod::PipUser => keys::INSTALL_PIP,
    }
}

/// A stable value for an install method (select values, selectors).
pub fn install_method_value(method: InstallMethod) -> &'static str {
    match method {
        InstallMethod::Uv => "uv",
        InstallMethod::InstallUvThenCli => "uv-first",
        InstallMethod::PipUser => "pip",
    }
}

/// GPU memory in whole GB (`mib` / 1024, rounded).
pub fn vram(cx: &App, mib: u32) -> SharedString {
    let gb = (f64::from(mib) / 1024.0).round();
    tr_args(cx, keys::FMT_GB, &[("n", &number(cx, gb, 0))])
}

/// What hardware came back: the GPU's name and memory, either part when known.
pub fn hardware(cx: &App, gpu: Option<&str>, vram_mib: Option<u32>) -> Option<SharedString> {
    match (gpu, vram_mib) {
        (Some(gpu), Some(mib)) => Some(format!("{gpu} · {}", vram(cx, mib)).into()),
        (Some(gpu), None) => Some(gpu.to_owned().into()),
        (None, Some(mib)) => Some(vram(cx, mib)),
        (None, None) => None,
    }
}

pub fn concern(cx: &App, concern: &Concern) -> SharedString {
    match concern {
        Concern::Limit {
            limit,
            current,
            threshold,
            estimated,
        } => {
            let (now, of) = match limit {
                Limit::Runtime => (
                    duration(cx, Duration::from_secs_f64(current.max(0.0))),
                    duration(cx, Duration::from_secs_f64(threshold.max(0.0))),
                ),
                Limit::Spend | Limit::Balance => (cu(cx, *current), cu(cx, *threshold)),
            };
            let now = if *estimated { approx(cx, &now) } else { now };
            let key = match limit {
                Limit::Runtime => keys::GUARD_RUNTIME,
                Limit::Spend => keys::GUARD_SPEND,
                Limit::Balance => keys::GUARD_BALANCE,
            };
            tr_args(cx, key, &[("current", &now), ("limit", &of)])
        }
        Concern::UsageUnknown { limit } => {
            let name = tr(cx, limit_key(*limit));
            tr_args(cx, keys::GUARD_UNKNOWN, &[("limit", &name)])
        }
    }
}

pub fn concerns(cx: &App, concerns: &[Concern]) -> SharedString {
    let items: Vec<String> = concerns
        .iter()
        .map(|c| concern(cx, c).to_string())
        .collect();
    list(cx, &items).into()
}

pub fn limit_key(limit: Limit) -> &'static str {
    match limit {
        Limit::Runtime => keys::LIMIT_RUNTIME,
        Limit::Spend => keys::LIMIT_SPEND,
        Limit::Balance => keys::LIMIT_BALANCE,
    }
}

/// A fraction from 0 to 1 as a percent. Values outside that range are clamped for display.
pub fn percent(cx: &App, fraction: f64) -> SharedString {
    let shown = (fraction * 100.0).clamp(0.0, 100.0);
    tr_args(cx, keys::RES_UTIL, &[("n", &number(cx, shown, 0))])
}

/// Bytes as MB or GB.
pub fn bytes(cx: &App, bytes: u64) -> SharedString {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    if mib >= 1024.0 {
        tr_args(cx, keys::FMT_GB, &[("n", &number(cx, mib / 1024.0, 1))])
    } else {
        if mib >= 1.0 {
            tr_args(cx, keys::FMT_MB, &[("n", &number(cx, mib, 1))])
        } else {
            let kib = (bytes as f64 / 1024.0).max(0.1);
            tr_args(cx, keys::FMT_KB, &[("n", &number(cx, kib, 1))])
        }
    }
}

/// A short wait: whole seconds under a minute ("6 s"), otherwise as [`duration`].
pub fn short_wait(cx: &App, secs: u64) -> SharedString {
    if secs < 60 {
        tr_args(cx, keys::FMT_SECONDS, &[("n", &secs.to_string())])
    } else {
        duration(cx, Duration::from_secs(secs))
    }
}
