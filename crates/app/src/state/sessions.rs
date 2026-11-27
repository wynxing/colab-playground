//! Sessions: every runtime on every saved account, stopping (any runtime, any account),
//! extending and cancelling auto-stop, what the cost guard decided, and runtimes that ended.
//!
//! A runtime that ended drops out of `runtimes()`, so the app keeps its own short list of
//! ended runtimes with the reason, for the reason to stay visible.

use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, SystemTime},
};

use colab_playground_core::{
    Error, ErrorKind,
    accounts::AccountId,
    backend::{GuardReport, Runtime, RuntimeRef, RuntimesView, StopOptions},
    guard::{Concern, Decision},
    tunnel::{TunnelId, TunnelInfo},
};
use ely_gpui_component::{feedback::Toast, primitives::Severity};
use gpui::Context;

use super::{AppState, Load, Op, Overlay, ops::key_of};
use crate::{
    errors, format,
    i18n::{keys, tr, tr_args},
};

/// How many ended runtimes the Sessions page keeps listing.
const ENDED_KEPT: usize = 20;

/// Notify when an armed auto-stop deadline is within this window.
const AUTO_STOP_SOON: Duration = Duration::from_secs(10 * 60);

#[derive(Default)]
pub struct SessionsState {
    pub(crate) runtimes: Load<RuntimesView>,
    /// A listing was asked for while one ran: what that one finds may be older than the change
    /// behind the request, so one more follows it.
    pub(crate) relist: bool,
    pub(crate) now: Option<SystemTime>,
    pub(crate) ended: Vec<Ended>,
    pub(crate) warnings: HashMap<RuntimeRef, Vec<Concern>>,
    pub(crate) stop_failures: HashMap<RuntimeRef, Error>,
    pub(crate) tunnels: BTreeMap<TunnelId, TunnelInfo>,
    pub(crate) guard_error: Option<Error>,
}

/// A runtime that ended while the app watched, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Ended {
    pub runtime: RuntimeRef,
    pub label: String,
    pub email: Option<String>,
    pub reason: EndReason,
    /// When (backend clock).
    pub at: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EndReason {
    User,
    Guard(Concern),
    /// It disappeared: its watchdog, Colab, or a stop elsewhere.
    Vanished,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StopPrompt {
    pub target: RuntimeRef,
    pub label: String,
    pub email: Option<String>,
    pub error: Option<Error>,
}

/// The name a runtime is shown by: its session name, or its hardware for runtimes opened
/// elsewhere (they have no name).
pub fn runtime_label(runtime: &Runtime) -> String {
    runtime
        .id
        .name
        .clone()
        .unwrap_or_else(|| runtime.hardware.clone())
}

impl AppState {
    /// Runtimes on every saved account, as last listed.
    pub fn runtimes(&self) -> &Load<RuntimesView> {
        &self.sessions.runtimes
    }

    /// The backend clock at the last listing.
    pub fn sessions_now(&self) -> Option<SystemTime> {
        self.sessions.now.or(self.usage_now())
    }

    pub fn runtime(&self, target: &RuntimeRef) -> Option<&Runtime> {
        self.sessions
            .runtimes
            .ready()?
            .runtimes
            .iter()
            .find(|runtime| &runtime.id == target)
    }

    /// Runtimes that ended while the app watched, newest first.
    pub fn ended(&self) -> &[Ended] {
        &self.sessions.ended
    }

    /// What the guard last said about `target` (close to a limit, or cannot check).
    pub fn guard_warnings(&self, target: &RuntimeRef) -> &[Concern] {
        self.sessions
            .warnings
            .get(target)
            .map_or(&[], Vec::as_slice)
    }

    /// The guard tried to stop `target` and failed: it is still billing.
    pub fn guard_stop_failure(&self, target: &RuntimeRef) -> Option<&Error> {
        self.sessions.stop_failures.get(target)
    }

    pub fn guard_error(&self) -> Option<&Error> {
        self.sessions.guard_error.as_ref()
    }

    pub fn is_stopping(&self, target: &RuntimeRef) -> bool {
        self.ops.is_running(Op::Stop(key_of(target)))
    }

    /// Whether a guard round may find something to check: the app manages a live runtime, or
    /// the list is not known (loading or failed), so the guard keeps watching.
    pub fn has_managed_runtimes(&self) -> bool {
        match &self.sessions.runtimes {
            Load::Ready(view) => view.runtimes.iter().any(|runtime| runtime.managed),
            _ => self.has_saved_accounts(),
        }
    }

    /// Lists the runtimes because they may have changed, or because the user asked. A listing
    /// already under way may have read an account before that change, and its result replaces
    /// the list, so one more listing follows it: one, however many requests arrive meanwhile.
    pub fn load_runtimes(&mut self, cx: &mut Context<Self>) {
        self.list_runtimes(true, cx);
    }

    /// Lists the runtimes for a request no change stands behind: the periodic listing, and a
    /// close that needs the list. A listing under way serves it. Were the periodic one to ask
    /// for one more, a listing slower than the polling interval would be followed by another
    /// without end; a close would wait for a listing that tells it nothing new.
    pub fn poll_runtimes(&mut self, cx: &mut Context<Self>) {
        self.list_runtimes(false, cx);
    }

    fn list_runtimes(&mut self, changed: bool, cx: &mut Context<Self>) {
        if !self.cli_installed() || !self.has_saved_accounts() {
            return;
        }
        self.sessions.runtimes.begin();
        self.quit_waits_for_listing();
        if self.ops.is_running(Op::Runtimes) {
            self.sessions.relist |= changed;
            return;
        }
        self.sessions.relist = false;
        self.run_backend(
            Op::Runtimes,
            cx,
            |backend, cancel| Ok((backend.runtimes(cancel)?, backend.tunnels(), backend.now())),
            |state, result, cx| {
                match result {
                    Ok((view, tunnels, now)) => {
                        state.sessions.now = Some(now);
                        let live: Vec<&RuntimeRef> =
                            view.runtimes.iter().map(|runtime| &runtime.id).collect();
                        state.sessions.warnings.retain(|key, _| live.contains(&key));
                        state
                            .sessions
                            .stop_failures
                            .retain(|key, _| live.contains(&key));
                        state
                            .auto_stop_soon_notified
                            .retain(|key| live.contains(&key));
                        state.sessions.tunnels = tunnels
                            .into_iter()
                            .map(|tunnel| (tunnel.id, tunnel))
                            .collect();
                        state.sessions.runtimes = Load::Ready(view);
                        state.drop_gone_run_target();
                        state.check_auto_stop_soon(cx);
                        state.discover_active_runs(cx);
                    }
                    Err(error) => {
                        log::warn!("sessions: listing failed: {error}");
                        state.sessions.runtimes = Load::Failed(error);
                    }
                }
                if std::mem::take(&mut state.sessions.relist) {
                    state.load_runtimes(cx);
                }
                // The listing just landed: what an agent reads about sessions is now current.
                state.publish_snapshot();
                state.listed_for_feedback(cx);
                cx.notify();
            },
        );
    }

    pub fn request_stop(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(runtime) = self.runtime(&target) else {
            return;
        };
        self.overlay = Some(Overlay::Stop(StopPrompt {
            label: runtime_label(runtime),
            email: runtime.account_email.clone(),
            target,
            error: None,
        }));
        cx.notify();
    }

    pub fn confirm_stop(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::Stop(prompt)) = &self.overlay else {
            return;
        };
        let prompt = prompt.clone();
        self.stop_runtime(prompt.target.clone(), cx);
    }

    /// Stops `target` now (the caller has confirmed).
    pub(crate) fn stop_runtime(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let label = self
            .runtime(&target)
            .map(runtime_label)
            .unwrap_or_else(|| target.name.clone().unwrap_or_default());
        let email = self
            .runtime(&target)
            .and_then(|runtime| runtime.account_email.clone());
        let work_target = target.clone();
        self.run_backend(
            Op::Stop(key_of(&target)),
            cx,
            move |backend, cancel| {
                backend.stop_session(&work_target, StopOptions::default(), cancel)?;
                Ok(backend.now())
            },
            move |state, result, cx| {
                let prompt_open =
                    matches!(&state.overlay, Some(Overlay::Stop(p)) if p.target == target);
                match result {
                    Ok(now) => {
                        if prompt_open {
                            state.overlay = None;
                        }
                        state.forget_runtime(&target);
                        state.record_ended(
                            target.clone(),
                            label.clone(),
                            email,
                            EndReason::User,
                            Some(now),
                        );
                        state.toast_done(tr_args(cx, keys::STOP_DONE, &[("name", &label)]), cx);
                        state.emit_session_event(
                            "session_stopped",
                            target.name.as_deref(),
                            serde_json::json!({ "by": "user" }),
                            cx,
                        );
                        state.after_runtime_change(cx);
                        state.ask_for_feedback_after_stop(cx);
                    }
                    Err(error) => {
                        log::warn!("sessions: stopping {label} failed: {error}");
                        if prompt_open && let Some(Overlay::Stop(prompt)) = &mut state.overlay {
                            prompt.error = Some(error);
                        } else {
                            state.toast_error(&error, cx);
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    /// Adds [`Self::rearm_duration`] to `target`'s auto-stop; it never moves earlier. Core reads
    /// the time still left inside the call, not from the listing here, which can be older than
    /// the last extension. A runtime without an auto-stop gets one that far from now.
    pub fn extend_session(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(by) = self.rearm_duration() else {
            return;
        };
        self.extend_runtime(target, by, cx);
    }

    /// Adds `by` to `target`'s auto-stop — what the Extend button asks for with the app's own
    /// duration, and what an agent asks for with the minutes it named.
    pub(crate) fn extend_runtime(
        &mut self,
        target: RuntimeRef,
        by: Duration,
        cx: &mut Context<Self>,
    ) {
        if self.runtime(&target).is_none() {
            return;
        }
        let work_target = target.clone();
        self.run_backend(
            Op::Extend(key_of(&target)),
            cx,
            move |backend, cancel| backend.extend_session(&work_target, by, cancel),
            move |state, result, cx| {
                match result {
                    Ok(set) => {
                        state.sessions.warnings.remove(&target);
                        state.auto_stop_soon_notified.remove(&target);
                        let title = tr_args(
                            cx,
                            keys::EXTEND_DONE,
                            &[("time", &format::duration(cx, set.runtime))],
                        );
                        state.toast_auto_stop(title, set.watchdog.armed, cx);
                        state.load_runtimes(cx);
                    }
                    Err(error) => state.toast_error(&error, cx),
                }
                cx.notify();
            },
        );
    }

    /// Asks before cancelling `target`'s auto-stop: afterwards it bills until stopped by hand.
    pub fn request_cancel_auto_stop(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(runtime) = self.runtime(&target) else {
            return;
        };
        self.overlay = Some(Overlay::CancelAutoStop(StopPrompt {
            label: runtime_label(runtime),
            email: runtime.account_email.clone(),
            target,
            error: None,
        }));
        cx.notify();
    }

    pub fn confirm_cancel_auto_stop(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::CancelAutoStop(prompt)) = &self.overlay else {
            return;
        };
        let target = prompt.target.clone();
        self.cancel_auto_stop(target, cx);
    }

    pub fn is_cancelling_auto_stop(&self, target: &RuntimeRef) -> bool {
        self.ops.is_running(Op::CancelAutoStop(key_of(target)))
    }

    pub fn cancel_auto_stop(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let work_target = target.clone();
        self.run_backend(
            Op::CancelAutoStop(key_of(&target)),
            cx,
            move |backend, cancel| backend.cancel_auto_stop(&work_target, cancel),
            move |state, result, cx| {
                let asking = matches!(&state.overlay, Some(Overlay::CancelAutoStop(p)) if p.target == target);
                match result {
                    Ok(()) => {
                        if asking {
                            state.overlay = None;
                        }
                        state.sessions.warnings.remove(&target);
                        state.auto_stop_soon_notified.remove(&target);
                        state.toast(
                            Toast::new(tr(cx, keys::AUTO_STOP_CANCELLED))
                                .severity(Severity::Warning),
                            cx,
                        );
                        state.load_runtimes(cx);
                    }
                    Err(error) => {
                        if asking && let Some(Overlay::CancelAutoStop(prompt)) = &mut state.overlay {
                            prompt.error = Some(error);
                        } else {
                            state.toast_error(&error, cx);
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    /// How much time Extend adds to a runtime's auto-stop, and how far ahead "Set auto-stop"
    /// puts the stop on a runtime that has none: the default runtime limit (Settings, else
    /// core's own). The session's recorded limit is not used: it counts from the session's
    /// start and grows with every extension.
    pub fn rearm_duration(&self) -> Option<Duration> {
        self.defaults()
            .max_runtime
            .or(colab_playground_core::Defaults::default().max_runtime)
    }

    /// Sets an auto-stop again on `target` (after it was cancelled), [`Self::rearm_duration`]
    /// from now. Core's `set_auto_stop` restarts the watchdog on the runtime; when that fails
    /// the limit still holds through the app-side guard, and the toast says so.
    pub fn set_auto_stop(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(by) = self.rearm_duration() else {
            return;
        };
        let work_target = target.clone();
        self.run_backend(
            Op::Extend(key_of(&target)),
            cx,
            move |backend, cancel| backend.set_auto_stop(&work_target, by, cancel),
            move |state, result, cx| {
                match result {
                    Ok(info) => {
                        state.auto_stop_soon_notified.remove(&target);
                        let title = tr_args(
                            cx,
                            keys::AUTO_STOP_SET,
                            &[("time", &format::duration(cx, by))],
                        );
                        state.toast_auto_stop(title, info.armed, cx);
                        state.load_runtimes(cx);
                    }
                    Err(error) => state.toast_error(&error, cx),
                }
                cx.notify();
            },
        );
    }

    /// Applies one guard round: warnings (a toast only when they change), automatic stops,
    /// failed stops (still billing) and runtimes that disappeared.
    pub(crate) fn apply_guard(&mut self, report: GuardReport, cx: &mut Context<Self>) {
        self.sessions.guard_error = None;
        let mut changed = false;
        for decision in report.decisions {
            let target = decision.session.clone();
            let label = self
                .runtime(&target)
                .map(runtime_label)
                .unwrap_or_else(|| target.name.clone().unwrap_or_default());
            let email = self
                .runtime(&target)
                .and_then(|runtime| runtime.account_email.clone());
            match decision.decision {
                Decision::Ok => {
                    self.sessions.warnings.remove(&target);
                }
                Decision::Warn(concerns) => {
                    let before = self.sessions.warnings.get(&target);
                    let limits: Vec<&Concern> = concerns
                        .iter()
                        .filter(|c| matches!(c, Concern::Limit { .. }))
                        .collect();
                    let was: Vec<&Concern> = before
                        .map(|b| {
                            b.iter()
                                .filter(|c| matches!(c, Concern::Limit { .. }))
                                .collect()
                        })
                        .unwrap_or_default();
                    let same_kinds = limits.len() == was.len()
                        && limits
                            .iter()
                            .zip(&was)
                            .all(|(a, b)| limit_of(a) == limit_of(b));
                    let emit = if !limits.is_empty() && !same_kinds {
                        let what = format::concerns(cx, &concerns);
                        self.toast(
                            Toast::new(tr_args(cx, keys::GUARD_WARN_TITLE, &[("name", &label)]))
                                .body(what)
                                .severity(Severity::Warning)
                                .stay(None),
                            cx,
                        );
                        Some(concerns.clone())
                    } else {
                        None
                    };
                    let name = target.name.clone();
                    self.sessions.warnings.insert(target, concerns);
                    if let Some(event_concerns) = emit {
                        self.emit_session_event(
                            "guard_warning",
                            name.as_deref(),
                            serde_json::json!({ "concerns": event_concerns }),
                            cx,
                        );
                    }
                }
                Decision::Stop(concern) => {
                    if decision.stopped {
                        self.forget_runtime(&target);
                        self.toast(
                            Toast::new(tr_args(cx, keys::GUARD_STOPPED_TITLE, &[("name", &label)]))
                                .body(format::concern(cx, &concern))
                                .severity(Severity::Danger)
                                .stay(None),
                            cx,
                        );
                        let name = target.name.clone();
                        self.record_ended(
                            target,
                            label,
                            email,
                            EndReason::Guard(concern.clone()),
                            self.usage_now(),
                        );
                        self.emit_session_event(
                            "guard_stopped",
                            name.as_deref(),
                            serde_json::json!({ "concern": concern }),
                            cx,
                        );
                        changed = true;
                    } else {
                        // Not stopped: a failed stop, or (should core ever report neither) a
                        // stop that did not happen. Either way it is still billing.
                        let error = decision.error.unwrap_or_else(|| {
                            Error::new(ErrorKind::Other)
                                .detail("the cost guard decided to stop it but no stop was made")
                        });
                        let first = !self.sessions.stop_failures.contains_key(&target);
                        let err_kind = error.kind.as_str();
                        if first {
                            self.toast(
                                Toast::new(tr_args(
                                    cx,
                                    keys::GUARD_STOP_FAILED_TITLE,
                                    &[("name", &label)],
                                ))
                                .body(errors::explain(&error, false, cx).message)
                                .severity(Severity::Danger)
                                .stay(None),
                                cx,
                            );
                        }
                        let name = target.name.clone();
                        self.sessions.stop_failures.insert(target, error);
                        if first {
                            self.emit_session_event(
                                "guard_stop_failed",
                                name.as_deref(),
                                serde_json::json!({
                                    "concern": concern,
                                    "error": err_kind,
                                }),
                                cx,
                            );
                        }
                    }
                }
            }
        }
        for target in report.vanished {
            let already_ended = self
                .sessions
                .ended
                .iter()
                .any(|ended| ended.runtime == target);
            let label = self
                .runtime(&target)
                .map(runtime_label)
                .unwrap_or_else(|| target.name.clone().unwrap_or_default());
            let email = self
                .runtime(&target)
                .and_then(|runtime| runtime.account_email.clone());
            self.forget_runtime(&target);
            self.toast(
                Toast::new(tr_args(cx, keys::VANISHED_TITLE, &[("name", &label)]))
                    .severity(Severity::Info),
                cx,
            );
            let name = target.name.clone();
            self.record_ended(target, label, email, EndReason::Vanished, self.usage_now());
            if !already_ended {
                self.emit_session_event(
                    "session_vanished",
                    name.as_deref(),
                    serde_json::json!({}),
                    cx,
                );
            }
            changed = true;
        }
        if changed {
            self.load_runtimes(cx);
        }
        cx.notify();
    }

    /// Emits `auto_stop_soon` once per session when its armed auto-stop deadline is within
    /// [`AUTO_STOP_SOON`].
    pub(crate) fn check_auto_stop_soon(&mut self, cx: &mut Context<Self>) {
        let now = match (self.sessions.now, self.usage_now()) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
        .or_else(|| self.backend.as_ref().ok().map(|backend| backend.now()));
        let Some(now) = now else {
            return;
        };
        let due: Vec<(RuntimeRef, u64)> = self
            .sessions
            .runtimes
            .ready()
            .into_iter()
            .flat_map(|view| view.runtimes.iter())
            .filter_map(|runtime| {
                let stop_at = runtime.auto_stop_at?;
                let remaining = stop_at.duration_since(now).ok()?;
                if remaining <= AUTO_STOP_SOON {
                    Some((runtime.id.clone(), remaining.as_secs()))
                } else {
                    None
                }
            })
            .collect();
        for (target, remaining_seconds) in due {
            if self.auto_stop_soon_notified.insert(target.clone()) {
                self.emit_session_event(
                    "auto_stop_soon",
                    target.name.as_deref(),
                    serde_json::json!({ "remaining_seconds": remaining_seconds }),
                    cx,
                );
            }
        }
    }

    /// Emits `session_vanished` immediately if a failing plugin run learned that its session
    /// is already gone before the next guard round runs.
    pub(crate) fn notify_run_session_gone(
        &mut self,
        account: Option<AccountId>,
        session: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = session else {
            return;
        };
        let Some(account) =
            account.or_else(|| self.accounts.ready().and_then(|view| view.active.clone()))
        else {
            return;
        };
        let Some(runtime) = self
            .sessions
            .runtimes
            .ready()
            .and_then(|view| {
                view.runtimes.iter().find(|runtime| {
                    runtime.id.account == account && runtime.id.name.as_deref() == Some(name.as_str())
                })
            })
            .cloned()
        else {
            return;
        };
        let target = runtime.id.clone();
        self.forget_runtime(&target);
        self.record_ended(
            target,
            runtime_label(&runtime),
            runtime.account_email,
            EndReason::Vanished,
            self.sessions_now(),
        );
        self.emit_session_event(
            "session_vanished",
            Some(&name),
            serde_json::json!({}),
            cx,
        );
    }

    /// A stop elsewhere, a provision or a switch changed what runs: list again and refresh
    /// usage.
    pub(crate) fn after_runtime_change(&mut self, cx: &mut Context<Self>) {
        self.load_runtimes(cx);
        self.refresh_usage_after_change(cx);
    }

    fn forget_runtime(&mut self, target: &RuntimeRef) {
        self.sessions.warnings.remove(target);
        self.sessions.stop_failures.remove(target);
        self.auto_stop_soon_notified.remove(target);
        self.sessions.tunnels.retain(|_, tunnel| {
            !(tunnel.spec.account == target.account
                && Some(&tunnel.spec.session) == target.name.as_ref())
        });
        if let Load::Ready(view) = &mut self.sessions.runtimes {
            view.runtimes.retain(|runtime| &runtime.id != target);
        }
    }

    fn record_ended(
        &mut self,
        runtime: RuntimeRef,
        label: String,
        email: Option<String>,
        reason: EndReason,
        at: Option<SystemTime>,
    ) {
        self.sessions.ended.retain(|ended| ended.runtime != runtime);
        self.sessions.ended.insert(
            0,
            Ended {
                runtime,
                label,
                email,
                reason,
                at,
            },
        );
        self.sessions.ended.truncate(ENDED_KEPT);
    }

    pub fn clear_ended(&mut self, cx: &mut Context<Self>) {
        self.sessions.ended.clear();
        cx.notify();
    }

    /// After moving or setting an auto-stop: whether the runtime itself will stop then (its
    /// watchdog is armed, so it works with the app closed) or only this app will, while open.
    fn toast_auto_stop(&self, title: gpui::SharedString, armed: bool, cx: &mut Context<Self>) {
        let toast = if armed {
            Toast::new(title)
                .body(tr(cx, keys::PROV_WATCHDOG))
                .severity(Severity::Success)
        } else {
            Toast::new(title)
                .body(tr(cx, keys::PROV_WATCHDOG_FAILED))
                .severity(Severity::Warning)
                .stay(None)
        };
        self.toast(toast, cx);
    }

    /// An error toast for a failed action, explained in the user's terms.
    pub(crate) fn toast_error(&self, error: &Error, cx: &mut Context<Self>) {
        let explained = errors::explain(error, false, cx);
        let mut toast = Toast::new(explained.message).severity(Severity::Danger);
        if let Some(hint) = explained.hint {
            toast = toast.body(hint);
        }
        self.toast(toast, cx);
    }
}

fn limit_of(concern: &Concern) -> Option<colab_playground_core::guard::Limit> {
    match concern {
        Concern::Limit { limit, .. } | Concern::UsageUnknown { limit } => Some(*limit),
    }
}
