//! New session: the form (pre-filled from `Defaults`, every value overridable for this one
//! session) and the live provisioning progress. Cancel is always available and leaves no
//! billed runtime behind (core releases any VM it allocated).
//!
//! The same progress model serves a plugin run that provisions its own session.

use std::time::{Duration, SystemTime};

use colab_playground_core::{
    Error, ErrorKind,
    backend::accelerator_prefix,
    provision::{ProvisionEvent, ProvisionOutcome, ProvisionSpec},
};
use gpui::{Context, ScrollHandle};

use super::{AppState, InputSlot, Op, Overlay};
use crate::i18n::{keys, tr_args};

#[derive(Clone, Debug, PartialEq)]
pub enum ProvisionPhase {
    Editing,
    /// Asking Colab for a runtime (see the events).
    Running,
    Ready(ProvisionOutcome),
    Failed(Error),
    /// Cancelled; nothing is left running.
    Cancelled,
}

/// How long a request for a runtime may go unanswered before the progress says so. An answered
/// request takes up to about 30 s, and Colab's own "unavailable" reply comes at about 30 s too.
pub const SLOW_ANSWER: Duration = Duration::from_secs(60);

/// What provisioning reported so far, folded into what the progress view shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProvisionProgress {
    /// Every event, in order.
    pub events: Vec<ProvisionEvent>,
    /// By the backend's clock.
    pub latest_at: Option<SystemTime>,
}

impl ProvisionProgress {
    pub fn push(&mut self, event: ProvisionEvent, at: Option<SystemTime>) {
        self.events.push(event);
        self.latest_at = at;
    }

    /// The CLI retries silently for about ten minutes before it gives up, so this can grow
    /// that long while the latest event is still `Allocating`.
    pub fn unanswered_for(&self, now: SystemTime) -> Option<Duration> {
        if !matches!(self.last(), Some(ProvisionEvent::Allocating { .. })) {
            return None;
        }
        let waited = now.duration_since(self.latest_at?).ok()?;
        (waited >= SLOW_ANSWER).then_some(waited)
    }

    /// `(attempt, of)` of the latest attempt.
    pub fn attempt(&self) -> Option<(u32, u32)> {
        self.events.iter().rev().find_map(|event| match event {
            ProvisionEvent::Allocating {
                attempt,
                max_attempts,
            } => Some((*attempt, *max_attempts)),
            _ => None,
        })
    }

    pub fn last(&self) -> Option<&ProvisionEvent> {
        self.events.last()
    }
}

#[derive(Clone, Debug)]
pub struct NewSession {
    pub spec: ProvisionSpec,
    pub phase: ProvisionPhase,
    pub progress: ProvisionProgress,
    /// New on each opening and reset when a failed or cancelled attempt is edited again, so the
    /// form shows from the top.
    pub scroll: ScrollHandle,
    /// The name the form made up last. An empty name field starts the session under it, and
    /// it is the only text in the field that the next one replaces.
    suggested_name: String,
}

impl AppState {
    /// The new-session dialog's state, while one exists (also while it is hidden and
    /// provisioning continues).
    pub fn new_session(&self) -> Option<&NewSession> {
        self.new_session.as_ref()
    }

    /// Whether a runtime is being provisioned (by the dialog or a plugin run).
    pub fn is_provisioning(&self) -> bool {
        self.ops.is_running(Op::Provision)
    }

    /// The mock's clock in demo mode and tests, never the wall clock.
    pub fn backend_now(&self) -> Option<SystemTime> {
        self.backend.as_ref().ok().map(|backend| backend.now())
    }

    pub fn open_new_session(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.new_session
            && session.phase == ProvisionPhase::Running
        {
            self.overlay = Some(Overlay::NewSession);
            cx.notify();
            return;
        }
        let defaults = self.defaults().clone();
        let spec = defaults.provision_spec(String::new());
        self.new_session = Some(NewSession {
            spec,
            phase: ProvisionPhase::Editing,
            progress: ProvisionProgress::default(),
            scroll: ScrollHandle::new(),
            suggested_name: String::new(),
        });
        self.overlay = Some(Overlay::NewSession);
        // Whatever an earlier opening left in the field is not a name typed for this one.
        self.set_input(InputSlot::SessionName, String::new());
        self.suggest_session_name(cx);
        cx.notify();
    }

    /// Makes a name with the prefix of the hardware the form has chosen. An empty name field
    /// starts the session under it, and the field shows it unless the user typed another.
    fn suggest_session_name(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.new_session else {
            return;
        };
        let accelerator = session.spec.accelerator;
        let prefix = accelerator_prefix(accelerator);
        self.run_backend(
            Op::SessionName,
            cx,
            move |backend, _| Ok(backend.new_session_name(Some(&prefix))),
            move |state, result, cx| {
                let Some(session) = &mut state.new_session else {
                    return;
                };
                if session.phase != ProvisionPhase::Editing {
                    return;
                }
                // The hardware changed while this name was made (a request made meanwhile
                // was ignored).
                if session.spec.accelerator != accelerator {
                    state.suggest_session_name(cx);
                    return;
                }
                if let Ok(name) = result {
                    session.spec.name = name.clone();
                    let replaces = std::mem::replace(&mut session.suggested_name, name.clone());
                    state.offer_input(InputSlot::SessionName, replaces, name);
                    cx.notify();
                }
            },
        );
    }

    pub fn edit_new_session(
        &mut self,
        edit: impl FnOnce(&mut ProvisionSpec),
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = &mut self.new_session
            && session.phase == ProvisionPhase::Editing
        {
            let accelerator = session.spec.accelerator;
            edit(&mut session.spec);
            if !session.spec.accelerator.is_gpu() {
                session.spec.min_vram_mib = None;
            }
            if session.spec.accelerator != accelerator {
                self.suggest_session_name(cx);
            }
            cx.notify();
        }
    }

    pub fn start_provision(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(session) = &mut self.new_session else {
            return;
        };
        if session.phase != ProvisionPhase::Editing
            && !matches!(
                session.phase,
                ProvisionPhase::Failed(_) | ProvisionPhase::Cancelled
            )
        {
            return;
        }
        let name = name.trim();
        // Not the name already in the spec: after a failed attempt that is the one typed for it.
        session.spec.name = if name.is_empty() {
            session.suggested_name.clone()
        } else {
            name.to_owned()
        };
        if let Err(error) = session.spec.validate() {
            session.phase = ProvisionPhase::Failed(error);
            cx.notify();
            return;
        }
        session.phase = ProvisionPhase::Running;
        session.progress = ProvisionProgress::default();
        let spec = session.spec.clone();
        self.run_backend_streaming(
            Op::Provision,
            cx,
            move |backend, events, cancel| backend.provision(spec, events, cancel),
            |state, event, cx| {
                let at = state.backend_now();
                if let Some(session) = &mut state.new_session {
                    session.progress.push(event, at);
                    cx.notify();
                }
            },
            |state, result, cx| {
                if let Ok(outcome) = &result {
                    let name = outcome.name.clone();
                    state.toast_done(
                        tr_args(cx, keys::PROVISION_READY_TOAST, &[("name", &name)]),
                        cx,
                    );
                    state.count_ready(cx);
                }
                let Some(session) = &mut state.new_session else {
                    return;
                };
                session.phase = match result {
                    Ok(outcome) => ProvisionPhase::Ready(outcome),
                    Err(error) if error.is(ErrorKind::Cancelled) => ProvisionPhase::Cancelled,
                    Err(error) => ProvisionPhase::Failed(error),
                };
                state.after_runtime_change(cx);
                state.continue_stop_all(cx);
                cx.notify();
            },
        );
        cx.notify();
    }

    /// Provisions without the form: an agent asked for a runtime, the window asked the user, and
    /// what follows is the same progress the new-session dialog shows. The spec has been checked
    /// already — a bad one is an answer to the agent, not a dialog for the user.
    pub(crate) fn provision_for_agent(&mut self, spec: ProvisionSpec, cx: &mut Context<Self>) {
        let name = spec.name.clone();
        self.new_session = Some(NewSession {
            suggested_name: name.clone(),
            spec,
            phase: ProvisionPhase::Editing,
            progress: ProvisionProgress::default(),
            scroll: ScrollHandle::new(),
        });
        self.overlay = Some(Overlay::NewSession);
        self.start_provision(name, cx);
    }

    /// Asks provisioning to stop; it releases any VM it allocated before returning.
    pub fn cancel_provision(&mut self, cx: &mut Context<Self>) {
        self.ops.cancel(Op::Provision);
        cx.notify();
    }

    /// Whether a cancel was asked and provisioning is still winding down.
    pub fn is_cancelling_provision(&self) -> bool {
        self.ops.is_cancelling(Op::Provision)
    }

    /// Closes the dialog. While provisioning runs it only hides (the Sessions page shows the
    /// progress with Cancel); otherwise the form is dropped.
    pub fn close_new_session(&mut self, cx: &mut Context<Self>) {
        if matches!(self.overlay, Some(Overlay::NewSession)) {
            self.overlay = None;
        }
        let running = self
            .new_session
            .as_ref()
            .is_some_and(|session| session.phase == ProvisionPhase::Running);
        if !running {
            self.new_session = None;
        }
        cx.notify();
    }

    /// Back to the form after a failure or cancel, with the same values.
    pub fn edit_again(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &mut self.new_session
            && matches!(
                session.phase,
                ProvisionPhase::Failed(_) | ProvisionPhase::Cancelled
            )
        {
            session.phase = ProvisionPhase::Editing;
            session.progress = ProvisionProgress::default();
            session.scroll.set_offset(gpui::Point::default());
            let name = session.spec.name.clone();
            self.set_input(InputSlot::SessionName, name);
            cx.notify();
        }
    }
}

/// The session cap on the active account, when it is reached.
#[derive(Clone, Debug, PartialEq)]
pub struct LimitReached {
    /// `max_concurrent_sessions` (this app's own setting).
    pub limit: u32,
    pub email: Option<String>,
    /// Runtimes that count: those this app can address by name (it started them).
    pub counted: Vec<String>,
}

impl AppState {
    /// The session cap, checked before anything is asked of Colab (core still enforces it).
    /// Like core, it counts named runtimes on the active account; runtimes opened elsewhere do
    /// not count.
    pub fn session_limit_reached(&self) -> Option<LimitReached> {
        let view = self.runtimes().ready()?;
        let limit = self.defaults().max_concurrent_sessions.max(1);
        let counted: Vec<String> = view
            .runtimes
            .iter()
            .filter(|r| r.on_active_account)
            .filter_map(|r| r.id.name.clone())
            .collect();
        (counted.len() as u32 >= limit).then(|| LimitReached {
            limit,
            email: self.active_email(),
            counted,
        })
    }

    /// Opens Settings with the session limit called out at the top.
    pub fn open_limit_setting(&mut self, cx: &mut Context<Self>) {
        self.close_overlay(cx);
        self.new_session = None;
        self.navigate(crate::route::Route::Settings, cx);
        self.settings_focus = true;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use colab_playground_core::provision::ProvisionEvent;

    use super::{ProvisionProgress, SLOW_ANSWER};

    fn asking(at: SystemTime) -> ProvisionProgress {
        let mut progress = ProvisionProgress::default();
        progress.push(
            ProvisionEvent::Allocating {
                attempt: 1,
                max_attempts: 3,
            },
            Some(at),
        );
        progress
    }

    #[test]
    fn an_unanswered_request_counts_from_the_ask_once_it_is_overdue() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let progress = asking(at);
        assert_eq!(progress.unanswered_for(at + Duration::from_secs(30)), None);
        assert_eq!(progress.unanswered_for(at + SLOW_ANSWER), Some(SLOW_ANSWER));
        let later = Duration::from_secs(3 * 60 + 20);
        assert_eq!(progress.unanswered_for(at + later), Some(later));
        assert_eq!(
            progress.unanswered_for(at - Duration::from_secs(5)),
            None,
            "a clock that went back"
        );
    }

    #[test]
    fn an_answer_or_an_unknown_time_ends_the_count() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let late = at + Duration::from_secs(600);
        let mut answered = asking(at);
        answered.push(ProvisionEvent::Allocated { attempt: 1 }, Some(at));
        assert_eq!(answered.unanswered_for(late), None, "probing the hardware");
        let mut untimed = asking(at);
        untimed.latest_at = None;
        assert_eq!(untimed.unanswered_for(late), None);
        assert_eq!(ProvisionProgress::default().unanswered_for(late), None);
    }
}
