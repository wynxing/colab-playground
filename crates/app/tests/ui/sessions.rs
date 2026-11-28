//! Sessions over the headless mock: provisioning (retry to an 80 GB A100, not entitled,
//! session limit, cancel), stopping any runtime on any account, forwards, the cost guard and
//! quitting with runtimes still billing.

use std::time::{Duration, SystemTime};

use colab_playground::{
    i18n::{self, Language},
    route::Route,
    state::{DriveMountPhase, EndReason, Load, Overlay, ProvisionPhase, ProvisionProgress},
    ui::{
        overlays::{
            DRIVE_MOUNT_CONFIRM, FORWARD_BODY, FORWARD_RESET_SSH, FORWARD_SUBMIT,
            KEEP_DRIVE_TOGGLE, KEEP_DRIVE_WARNING, KEEP_LOCAL_TOGGLE, KEEP_SUBMIT,
            NEW_SESSION_FORM, NEW_SESSION_LIMIT, NEW_SESSION_START, REMOTE_SSH_BODY,
            REMOTE_SSH_RESET_SSH, REMOTE_SSH_TEMP_COMMAND, REMOTE_SSH_WRITE_TEMP, RESERVED_NOTE,
            RESTART_KERNEL_CONFIRM, STOP_FORCE_CONFIRM, STOP_KEEP_TOGGLE, VM_UPLOAD_SUBMIT,
        },
        pages::{
            plugins::RUN_KEEP_CONFIGURE,
            sessions::{
                NEW_SESSION_SELECTOR, drive_mount_selector, forward_selector, ide_ssh_selector,
                keep_selector, keep_summary_selector, open_browser_selector, reset_ssh_selector,
                restart_kernel_selector, stop_selector, switch_selector, sync_keep_selector,
                upload_selector,
            },
        },
        progress, provision_form,
    },
};
use colab_playground_core::{
    CancelToken, ErrorKind,
    backend::Backend,
    guard::{Concern, Decision, Limit},
    mock::{MockFailures, MockScenario},
    provision::ProvisionEvent,
    sessions::Accelerator,
};
use gpui::{
    Context, IntoElement, ParentElement, Render, Styled, TestAppContext, Window, div, px, size,
};

use crate::support::{Options, TestApp, click_at, mount};

const STOP_CONFIRM: &str = "stop-confirm";

fn on_sessions(app: &mut TestApp<'_>) {
    app.click(Route::Sessions.nav_selector());
    assert_eq!(app.route(), Route::Sessions);
}

fn runtime_names(app: &mut TestApp<'_>) -> Vec<String> {
    app.read(|state, _| {
        state
            .runtimes()
            .ready()
            .map(|view| {
                view.runtimes
                    .iter()
                    .map(colab_playground::state::runtime_label)
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn phase(app: &mut TestApp<'_>) -> Option<ProvisionPhase> {
    app.read(|state, _| state.new_session().map(|s| s.phase.clone()))
}

fn events(app: &mut TestApp<'_>) -> Vec<ProvisionEvent> {
    app.read(|state, _| {
        state
            .new_session()
            .map(|s| s.progress.events.clone())
            .unwrap_or_default()
    })
}

fn provision(
    app: &mut TestApp<'_>,
    edit: impl FnOnce(&mut colab_playground_core::provision::ProvisionSpec),
) {
    on_sessions(app);
    app.click(NEW_SESSION_SELECTOR);
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::NewSession)
    ));
    let spec = app.read(|state, _| state.new_session().map(|s| s.spec.clone()));
    let spec = spec.expect("the form is open");
    assert!(!spec.name.is_empty(), "a generated name is filled in");
    assert_eq!(
        spec.max_attempts,
        app.read(|state, _| state.defaults().max_attempts),
        "pre-filled from Defaults"
    );
    app.update(|state, cx| state.edit_new_session(edit, cx));
    app.click(NEW_SESSION_START);
}

#[gpui::test]
fn a_40_gb_a100_is_given_back_and_the_retry_gets_80_gb(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    provision(&mut app, |spec| {
        spec.accelerator = Accelerator::A100;
        spec.min_vram_mib = Some(70 * 1024);
        spec.max_attempts = 3;
    });
    let Some(ProvisionPhase::Ready(outcome)) = phase(&mut app) else {
        panic!("expected a ready runtime, got {:?}", phase(&mut app));
    };
    assert_eq!(outcome.hardware.vram_mib, Some(81_920));
    assert_eq!(outcome.attempts, 2);
    let events = events(&mut app);
    assert!(
        events.iter().any(|e| matches!(
            e,
            ProvisionEvent::Rejected {
                vram_mib: Some(40_960),
                ..
            }
        )),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Retrying { .. }))
    );
    assert!(app.shown(colab_playground::ui::progress::PROGRESS_SELECTOR));
    assert_eq!(runtime_names(&mut app), std::slice::from_ref(&outcome.name));
}

#[gpui::test]
fn an_accelerator_the_account_cannot_use_says_so(cx: &mut TestAppContext) {
    let options = Options {
        failures: MockFailures {
            not_entitled: vec![Accelerator::H100],
            ..MockFailures::default()
        },
        ..Options::scenario(MockScenario::single())
    };
    let mut app = TestApp::with(cx, options);
    provision(&mut app, |spec| {
        spec.accelerator = Accelerator::H100;
        spec.min_vram_mib = None;
    });
    let Some(ProvisionPhase::Failed(error)) = phase(&mut app) else {
        panic!("expected a failure, got {:?}", phase(&mut app));
    };
    assert_eq!(error.kind, ErrorKind::NotEntitled);
    assert!(app.shown("error-provision"), "the failure is explained");
    assert!(
        !events(&mut app)
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Retrying { .. })),
        "not retried: retrying cannot help"
    );
    app.click("new-session-change");
    assert_eq!(
        phase(&mut app),
        Some(ProvisionPhase::Editing),
        "back to the form"
    );
}

#[gpui::test]
fn changing_options_after_a_failure_shows_the_form_from_its_top(cx: &mut TestAppContext) {
    use gpui::point;
    let options = Options {
        failures: MockFailures {
            not_entitled: vec![Accelerator::H100],
            ..MockFailures::default()
        },
        ..Options::scenario(MockScenario::single())
    };
    let mut app = TestApp::with(cx, options);
    app.resize(size(px(880.0), px(560.0)));
    open_new_session(&mut app);
    app.update(|state, cx| {
        state.edit_new_session(
            |spec| {
                spec.accelerator = Accelerator::H100;
                spec.min_vram_mib = None;
            },
            cx,
        )
    });
    let scroll = form_scroll(&mut app);
    scroll.set_offset(point(px(0.0), -scroll.max_offset().y));
    app.settle();
    assert!(scroll.offset().y < px(0.0), "scrolled to the end");
    let form = app.bounds(NEW_SESSION_FORM).expect("the form is drawn");
    let name = app
        .bounds("new-session-name")
        .expect("the name field is drawn");
    assert!(
        name.top() < form.top(),
        "the name {name:?} has scrolled above the form {form:?}"
    );
    app.click(NEW_SESSION_START);
    let failed = phase(&mut app);
    assert!(
        matches!(failed, Some(ProvisionPhase::Failed(_))),
        "expected a failure, got {failed:?}"
    );
    app.click("new-session-change");
    assert_eq!(
        phase(&mut app),
        Some(ProvisionPhase::Editing),
        "back to the form"
    );
    assert_eq!(
        form_scroll(&mut app).offset().y,
        px(0.0),
        "the form shows from its top again"
    );
    let form = app.bounds(NEW_SESSION_FORM).expect("the form is drawn");
    let name = app
        .bounds("new-session-name")
        .expect("the name field is drawn");
    assert!(
        name.top() >= form.top() - px(0.5) && name.bottom() <= form.bottom() + px(0.5),
        "the name {name:?} is inside the form {form:?}"
    );
}

#[gpui::test]
fn the_session_limit_is_explained_before_starting(cx: &mut TestAppContext) {
    // The demo's active account already has two named sessions (the default cap is 2), plus a
    // runtime opened in the browser that does not count.
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    app.click(NEW_SESSION_SELECTOR);
    let limit = app.read(|state, _| state.session_limit_reached());
    let limit = limit.expect("checked when the dialog opens");
    assert_eq!(limit.limit, 2);
    assert_eq!(limit.counted, ["a100-7f3a", "t4-91c2"]);
    assert!(
        app.shown(NEW_SESSION_LIMIT),
        "explained before anything is tried"
    );
    assert!(
        !app.shown(NEW_SESSION_START),
        "no Start that would only fail"
    );

    app.click("new-session-open-settings");
    assert_eq!(app.route(), Route::Settings);
    assert!(app.shown(colab_playground::ui::pages::defaults::LIMIT_FOCUS_SELECTOR));
    app.update(|state, cx| state.update_defaults(|d| d.max_concurrent_sessions = 3, cx));
    on_sessions(&mut app);
    app.click(NEW_SESSION_SELECTOR);
    assert!(app.read(|state, _| state.session_limit_reached().is_none()));
    assert!(
        app.shown(NEW_SESSION_START),
        "with a higher limit the form is back"
    );
}

#[gpui::test]
fn the_limit_error_from_core_offers_settings_and_sessions(cx: &mut TestAppContext) {
    // Core still enforces the cap (e.g. when the list was stale): the same explanation shows.
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    app.update(|state, cx| {
        state.open_new_session(cx);
        state.start_provision("extra-1".into(), cx);
    });
    let Some(ProvisionPhase::Failed(error)) = phase(&mut app) else {
        panic!("expected a failure, got {:?}", phase(&mut app));
    };
    assert_eq!(error.kind, ErrorKind::SessionLimitReached);
    assert!(app.shown(NEW_SESSION_LIMIT));
    app.click("new-session-open-sessions");
    assert!(app.read(|state, _| state.overlay().is_none()));
    assert_eq!(app.route(), Route::Sessions);
}

#[gpui::test]
fn cancelling_while_a_runtime_exists_leaves_nothing_billing(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    on_sessions(&mut app);
    app.click(NEW_SESSION_SELECTOR);
    app.update(|state, cx| state.edit_new_session(|spec| spec.accelerator = Accelerator::A100, cx));
    // Wait 1 is in `create`, before the VM exists; wait 2 is the hardware probe, with a VM
    // allocated: cancel there.
    app.cancel_switch.arm(2);
    app.click(NEW_SESSION_START);
    assert_eq!(phase(&mut app), Some(ProvisionPhase::Cancelled));
    let events = events(&mut app);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Allocating { .. })),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Releasing { .. })),
        "the allocated VM was released: {events:?}"
    );
    assert!(app.shown("provision-cancelled"));
    let left = app.mock.runtimes(&CancelToken::new()).expect("listing");
    assert!(left.runtimes.is_empty(), "nothing billed is left: {left:?}");
    assert!(runtime_names(&mut app).is_empty());
}

fn no_answer_for_a100() -> Options {
    Options {
        failures: MockFailures {
            allocation_hangs: vec![Accelerator::A100],
            ..MockFailures::default()
        },
        ..Options::scenario(MockScenario::single())
    }
}

#[gpui::test]
fn cancelling_a_request_colab_never_answers_ends_it_and_leaves_nothing(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, no_answer_for_a100());
    on_sessions(&mut app);
    app.click(NEW_SESSION_SELECTOR);
    app.update(|state, cx| state.edit_new_session(|spec| spec.accelerator = Accelerator::A100, cx));
    // Wait 1 is `colab new` waiting for an answer that never comes: cancel there. The mock's
    // request still gets a VM, with no name, as a real one may after it is killed.
    app.cancel_switch.arm(1);
    app.click(NEW_SESSION_START);
    assert_eq!(phase(&mut app), Some(ProvisionPhase::Cancelled));
    assert!(!app.read(|state, _| state.is_provisioning()));
    let events = events(&mut app);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Reconciling { attempt: 1 })),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::OrphanDetected { released: true, .. })),
        "the VM the request got anyway was stopped: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            ProvisionEvent::AllocationFailed { .. } | ProvisionEvent::Retrying { .. }
        )),
        "a cancel is not a failed attempt: {events:?}"
    );
    assert!(app.shown("provision-cancelled"));
    let left = app.mock.runtimes(&CancelToken::new()).expect("listing");
    assert!(left.runtimes.is_empty(), "nothing billed is left: {left:?}");
}

#[gpui::test]
fn a_request_colab_never_answers_fails_once_without_retrying(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, no_answer_for_a100());
    provision(&mut app, |spec| {
        spec.accelerator = Accelerator::A100;
        spec.max_attempts = 3;
    });
    let Some(ProvisionPhase::Failed(error)) = phase(&mut app) else {
        panic!("expected a failure, got {:?}", phase(&mut app));
    };
    assert_eq!(error.kind, ErrorKind::AllocationTimeout);
    assert!(app.shown("error-provision"), "the failure is explained");
    let events = events(&mut app);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ProvisionEvent::Allocating { .. }))
            .count(),
        1,
        "the CLI already retried for about ten minutes: {events:?}"
    );
    let left = app.mock.runtimes(&CancelToken::new()).expect("listing");
    assert!(left.runtimes.is_empty(), "{left:?}");
}

/// A headless backend call runs to completion in one go, so a request still waiting cannot be
/// caught in the app itself: the note is drawn on its own.
struct NoAnswerNote {
    progress: ProvisionProgress,
    now: SystemTime,
}

impl Render for NoAnswerNote {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .children(progress::no_answer_note(&self.progress, Some(self.now), cx))
    }
}

#[gpui::test]
fn a_request_colab_leaves_unanswered_says_so_once_it_is_overdue(cx: &mut TestAppContext) {
    let asked = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600);
    let mut asking = ProvisionProgress::default();
    asking.push(
        ProvisionEvent::Allocating {
            attempt: 1,
            max_attempts: 3,
        },
        Some(asked),
    );
    let mut answered = asking.clone();
    answered.push(ProvisionEvent::Allocated { attempt: 1 }, Some(asked));
    for (progress, after, shown) in [
        (asking.clone(), 30, false),
        (asking, 3 * 60, true),
        (answered, 3 * 60, false),
    ] {
        let (_, cx) = mount(cx, move |_, cx| {
            i18n::install(Language::EnUs, cx);
            NoAnswerNote {
                progress,
                now: asked + Duration::from_secs(after),
            }
        });
        assert_eq!(
            cx.debug_bounds(progress::NO_ANSWER_SELECTOR).is_some(),
            shown,
            "{after} s after asking"
        );
    }
}

#[gpui::test]
fn any_runtime_on_any_account_can_be_stopped(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    let names = runtime_names(&mut app);
    // A managed session, a runtime opened in the browser (no name: shown by its hardware),
    // and a session on the other account.
    for label in ["a100-7f3a", "L4", "cpu-0b5e"] {
        assert!(names.contains(&label.to_owned()), "{label} in {names:?}");
        app.click(&stop_selector(label));
        let Some(Overlay::Stop(prompt)) = app.read(|state, _| state.overlay().cloned()) else {
            panic!("expected the stop confirmation for {label}");
        };
        assert_eq!(prompt.label, label);
        assert!(app.shown(STOP_CONFIRM));
        app.click(STOP_CONFIRM);
        assert!(app.read(|state, _| state.overlay().is_none()), "{label}");
        assert!(
            !runtime_names(&mut app).contains(&label.to_owned()),
            "{label} is gone"
        );
        let ended = app.read(|state, _| state.ended().first().cloned());
        let ended = ended.expect("listed as ended");
        assert_eq!(
            (ended.label.as_str(), &ended.reason),
            (label, &EndReason::User)
        );
    }
    let left = app.mock.runtimes(&CancelToken::new()).expect("listing");
    assert_eq!(
        left.runtimes.len(),
        1,
        "only the T4 session is left: {left:?}"
    );
}

#[gpui::test]
fn a_busy_forward_says_so_then_comes_up_and_closes(cx: &mut TestAppContext) {
    let options = Options {
        failures: MockFailures {
            ssh_busy_attempts: 1,
            ..MockFailures::default()
        },
        ..Options::default()
    };
    let mut app = TestApp::with(cx, options);
    on_sessions(&mut app);
    app.click(&forward_selector("a100-7f3a"));
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::Forward(_))
    ));

    app.fill(|inputs| inputs.forward_remote.clone(), "8080");
    app.click(FORWARD_SUBMIT);
    assert!(app.shown(RESERVED_NOTE), "8080 is refused with the reason");
    assert!(
        app.read(|state, _| state.tunnels().next().is_none()),
        "nothing tried"
    );
    app.fill(|inputs| inputs.forward_remote.clone(), "8000");

    app.click(FORWARD_SUBMIT);
    let Some(Overlay::Forward(form)) = app.read(|state, _| state.overlay().cloned()) else {
        panic!("the dialog stays open with the error");
    };
    assert_eq!(form.error.map(|e| e.kind), Some(ErrorKind::SshBusy));
    assert!(app.shown("error-forward"));

    app.click(FORWARD_SUBMIT);
    assert!(
        app.read(|state, _| state.overlay().is_none()),
        "up: the dialog closes"
    );
    let up = app.read(|state, _| {
        state
            .tunnels()
            .map(|t| (t.spec.local_port, t.state.clone(), t.healthy))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        up,
        [(
            8000,
            colab_playground_core::tunnel::TunnelState::Up,
            Some(true)
        )]
    );
    assert!(app.shown("tunnel-close 8000"));

    app.click("tunnel-close 8000");
    assert!(app.read(|state, _| state.tunnels().next().is_none()));
    let closed = app.text(colab_playground::i18n::keys::FORWARD_CLOSED);
    assert!(app.toasts().contains(&closed), "{:?}", app.toasts());
    assert!(
        runtime_names(&mut app).contains(&"a100-7f3a".to_owned()),
        "closing a forward does not stop the runtime"
    );
}

#[gpui::test]
fn the_guard_warns_then_stops_as_time_passes(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    let a100 = app
        .read(|state, _| {
            state.runtimes().ready().and_then(|view| {
                view.runtimes
                    .iter()
                    .find(|r| r.id.name.as_deref() == Some("a100-7f3a"))
                    .map(|r| r.id.clone())
            })
        })
        .expect("the demo's A100 session");
    let limit = app.read(|state, _| state.defaults().max_runtime);
    assert_eq!(
        limit,
        Some(Duration::from_secs(2 * 3600)),
        "core's default limit"
    );

    // 100 of 120 minutes: past 80 %.
    app.advance(Duration::from_secs(100 * 60));
    let warnings = app.read(|state, _| state.guard_warnings(&a100).to_vec());
    assert!(
        warnings.iter().any(|c| matches!(
            c,
            Concern::Limit {
                limit: Limit::Runtime,
                ..
            }
        )),
        "{warnings:?}"
    );
    assert!(
        app.shown("guard-warning a100-7f3a"),
        "a persistent indicator"
    );
    let warned = app.cx.update(|_, cx| {
        colab_playground::i18n::tr_args(
            cx,
            colab_playground::i18n::keys::GUARD_WARN_TITLE,
            &[("name", "a100-7f3a")],
        )
    });
    let toasts = app.toasts();
    assert_eq!(
        toasts.iter().filter(|t| **t == warned).count(),
        1,
        "{toasts:?}"
    );
    app.advance(Duration::from_secs(60));
    assert_eq!(
        app.toasts().iter().filter(|t| **t == warned).count(),
        1,
        "not toasted again while the warning is the same"
    );

    app.advance(Duration::from_secs(25 * 60));
    assert!(
        !runtime_names(&mut app).contains(&"a100-7f3a".to_owned()),
        "stopped at the limit"
    );
    let ended = app.read(|state, _| {
        state
            .ended()
            .iter()
            .find(|e| e.label == "a100-7f3a")
            .cloned()
    });
    let ended = ended.expect("the reason stays visible");
    assert!(matches!(
        ended.reason,
        EndReason::Guard(Concern::Limit {
            limit: Limit::Runtime,
            ..
        })
    ));
    assert!(app.shown(colab_playground::ui::pages::sessions::ENDED_SELECTOR));
    let _ = Decision::Ok;
}

#[gpui::test]
fn closing_with_nothing_billing_just_quits(cx: &mut TestAppContext) {
    let app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    // The app quits rather than letting the window be removed.
    assert!(!app.cx.simulate_close(), "the app quits instead");
    app.cx.run_until_parked();
    assert!(
        app.state
            .read_with(&*app.cx, |state, _| state.quit_approved()),
        "nothing runs, so nothing to ask"
    );
    assert!(
        app.state
            .read_with(&*app.cx, |state, _| state.overlay().is_none())
    );
}

#[gpui::test]
fn closing_while_billing_asks_and_cancel_keeps_the_window(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    assert!(!app.cx.simulate_close(), "the close is held");
    app.settle();
    let Some(Overlay::Quit(prompt)) = app.read(|state, _| state.overlay().cloned()) else {
        panic!("the quit prompt is open");
    };
    assert_eq!(prompt.billing.watched.len(), 3, "{:?}", prompt.billing);
    assert_eq!(
        prompt.billing.unwatched.len(),
        1,
        "the runtime opened in the browser has no automatic stop"
    );
    app.click("overlay-cancel");
    assert!(app.read(|state, _| state.overlay().is_none()));
    assert!(!app.read(|state, _| state.quit_approved()));
}

#[gpui::test]
fn leave_running_and_quit_keeps_them_billing(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    let mock = app.mock.clone();
    assert!(!app.cx.simulate_close());
    app.settle();
    app.state
        .update(&mut **app.cx, |state, cx| state.quit_leaving_running(cx));
    app.cx.run_until_parked();
    assert!(
        app.state
            .read_with(&*app.cx, |state, _| state.quit_approved())
    );
    let left = mock.runtimes(&CancelToken::new()).expect("listing");
    assert_eq!(left.runtimes.len(), 4, "nothing was stopped");
}

#[gpui::test]
fn cancelling_auto_stop_asks_first_and_it_can_be_set_again(cx: &mut TestAppContext) {
    use colab_playground::ui::pages::sessions::{
        cancel_auto_stop_selector, set_auto_stop_selector,
    };
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    let auto_stop = |app: &mut TestApp<'_>| {
        app.read(|state, _| {
            state.runtimes().ready().and_then(|view| {
                view.runtimes
                    .iter()
                    .find(|r| r.id.name.as_deref() == Some("a100-7f3a"))
                    .map(|r| r.auto_stop_at)
            })
        })
    };
    assert!(matches!(auto_stop(&mut app), Some(Some(_))));
    app.click(&cancel_auto_stop_selector("a100-7f3a"));
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::CancelAutoStop(_))
    ));
    assert!(
        matches!(auto_stop(&mut app), Some(Some(_))),
        "nothing changed yet"
    );
    app.click(colab_playground::ui::overlays::CANCEL_AUTO_STOP_CONFIRM);
    assert!(app.read(|state, _| state.overlay().is_none()));
    assert_eq!(
        auto_stop(&mut app),
        Some(None),
        "it now bills until stopped"
    );
    assert!(
        !app.shown(&cancel_auto_stop_selector("a100-7f3a")),
        "nothing to cancel any more"
    );

    app.click(&set_auto_stop_selector("a100-7f3a"));
    assert!(
        matches!(auto_stop(&mut app), Some(Some(_))),
        "an auto-stop is set again"
    );
    assert_eq!(
        watchdog_armed(&mut app, "a100-7f3a"),
        Some(true),
        "on the runtime itself"
    );
    assert!(!app.shown("auto-stop-app-only a100-7f3a"));
    let billing = app.read(|state, _| state.billing());
    assert!(
        billing
            .watched
            .iter()
            .any(|r| r.name.as_deref() == Some("a100-7f3a")),
        "the quit prompt lists it as stopping on its own: {billing:?}"
    );
}

fn watchdog_armed(app: &mut TestApp<'_>, name: &str) -> Option<bool> {
    app.read(|state, _| {
        state.runtimes().ready().and_then(|view| {
            view.runtimes
                .iter()
                .find(|r| r.id.name.as_deref() == Some(name))
                .and_then(|r| r.tracked.as_ref())
                .and_then(|t| t.watchdog.as_ref())
                .map(|w| w.armed)
        })
    })
}

#[gpui::test]
fn an_auto_stop_the_runtime_could_not_take_applies_only_while_the_app_is_open(
    cx: &mut TestAppContext,
) {
    use colab_playground::ui::pages::sessions::{
        cancel_auto_stop_selector, set_auto_stop_selector,
    };
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    app.click(&cancel_auto_stop_selector("a100-7f3a"));
    app.click(colab_playground::ui::overlays::CANCEL_AUTO_STOP_CONFIRM);
    app.set_failures(MockFailures {
        watchdog_restart_fails: true,
        ..MockFailures::default()
    });
    app.click(&set_auto_stop_selector("a100-7f3a"));
    assert_eq!(watchdog_armed(&mut app, "a100-7f3a"), Some(false));
    let set = app.cx.update(|_, cx| {
        colab_playground::i18n::tr_args(
            cx,
            colab_playground::i18n::keys::AUTO_STOP_SET,
            &[("time", "2 h")],
        )
    });
    assert!(
        app.toasts_with_severity()
            .contains(&(set, Some(ely_gpui_component::primitives::Severity::Warning))),
        "a warning, not a success: {:?}",
        app.toasts_with_severity()
    );
    assert!(
        app.shown("auto-stop-app-only a100-7f3a"),
        "the card says it applies only while the app is open"
    );
    let billing = app.read(|state, _| state.billing());
    assert!(
        billing
            .unwatched
            .iter()
            .any(|r| r.name.as_deref() == Some("a100-7f3a")),
        "the quit prompt lists it as having no automatic stop: {billing:?}"
    );
}

#[gpui::test]
fn the_quit_prompt_names_what_stop_all_would_stop(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    assert!(!app.cx.simulate_close());
    app.settle();
    for label in ["a100-7f3a", "t4-91c2", "L4", "cpu-0b5e"] {
        assert!(
            app.shown(&format!("quit-runtime {label}")),
            "{label} is listed"
        );
    }
    assert!(app.shown("quit-wipe"), "the data-wipe line is drawn");
    assert!(
        app.shown("quit-unmanaged L4"),
        "marked as not started by this app"
    );
    assert!(!app.shown("quit-unmanaged a100-7f3a"));
}

#[gpui::test]
fn stop_all_waits_for_a_provision_in_flight_and_leaves_nothing(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    on_sessions(&mut app);
    app.click(NEW_SESSION_SELECTOR);
    app.update(|state, cx| {
        state.start_provision(String::new(), cx);
        assert!(state.is_provisioning());
        state.request_quit(cx);
        assert!(
            matches!(state.overlay(), Some(Overlay::Quit(_))),
            "a provision in flight counts"
        );
        assert!(!state.quit_approved());
        state.quit_stopping_all(cx);
        assert!(
            !state.is_busy(colab_playground::state::Op::StopAll),
            "nothing is stopped while the cancelled provision is still unwinding"
        );
        assert!(state.is_stopping_all(), "but Stop all is under way");
    });
    app.settle();
    let left = app.mock.runtimes(&CancelToken::new()).expect("listing");
    assert!(left.runtimes.is_empty(), "nothing billed is left: {left:?}");
    assert!(
        app.read(|state, _| state.quit_approved()),
        "quits once nothing is left"
    );
    assert_eq!(phase(&mut app), Some(ProvisionPhase::Cancelled));
}

#[gpui::test]
fn stop_all_and_quit_closes_only_when_nothing_is_left(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    assert!(!app.cx.simulate_close());
    app.settle();
    app.click(colab_playground::ui::overlays::QUIT_STOP_ALL);
    let left = app.mock.runtimes(&CancelToken::new()).expect("listing");
    assert!(left.runtimes.is_empty(), "{left:?}");
    assert!(app.read(|state, _| state.quit_approved()));
}

#[gpui::test]
fn the_quit_action_asks_like_closing_the_window(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    let menus = app
        .cx
        .update(|_, cx| colab_playground::platform::app_menus(cx));
    assert_eq!(menus.len(), 1, "the macOS app menu with Quit");
    assert_eq!(
        colab_playground::platform::quit_bindings().len(),
        1,
        "Cmd+Q"
    );
    app.cx.update(|window, cx| {
        window.dispatch_action(Box::new(colab_playground::platform::Quit), cx)
    });
    app.settle();
    assert!(
        matches!(
            app.read(|state, _| state.overlay().cloned()),
            Some(Overlay::Quit(_))
        ),
        "Cmd+Q opens the same prompt"
    );
}

/// Time until `name`'s auto-stop, on the mock's clock.
fn time_left(app: &mut TestApp<'_>, name: &str) -> Option<Duration> {
    let now = Backend::now(&app.mock);
    app.read(|state, _| {
        state.runtimes().ready().and_then(|view| {
            view.runtimes
                .iter()
                .find(|r| r.id.name.as_deref() == Some(name))
                .and_then(|r| r.auto_stop_at)
                .and_then(|at| at.duration_since(now).ok())
        })
    })
}

fn assert_close(actual: Duration, expected: Duration, what: &str) {
    let diff = actual.abs_diff(expected);
    assert!(
        diff <= Duration::from_secs(60),
        "{what}: {actual:?}, expected about {expected:?}"
    );
}

#[gpui::test]
fn extend_adds_to_the_time_left(cx: &mut TestAppContext) {
    use colab_playground::ui::pages::sessions::extend_selector;
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    let before = time_left(&mut app, "a100-7f3a").expect("the demo session has an auto-stop");
    assert_close(
        before,
        Duration::from_secs(109 * 60),
        "the demo's time left",
    );
    app.click(&extend_selector("a100-7f3a"));
    let after = time_left(&mut app, "a100-7f3a").expect("still an auto-stop");
    assert_close(
        after,
        before + Duration::from_secs(2 * 3600),
        "after Extend by 2 h",
    );
    let toast = app.cx.update(|_, cx| {
        colab_playground::i18n::tr_args(
            cx,
            colab_playground::i18n::keys::EXTEND_DONE,
            &[(
                "time",
                &colab_playground::format::duration(cx, before + Duration::from_secs(2 * 3600)),
            )],
        )
    });
    assert!(
        app.toasts().contains(&toast),
        "the toast gives the new time left: {:?}",
        app.toasts()
    );
}

/// An extension the listing on screen does not show yet (the Extend button is free again as
/// soon as its call returns, before the list is read again) is not lost to the next one.
#[gpui::test]
fn extend_adds_to_an_extension_the_listing_does_not_show_yet(cx: &mut TestAppContext) {
    use colab_playground::ui::pages::sessions::extend_selector;
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    let before = time_left(&mut app, "a100-7f3a").expect("the demo session has an auto-stop");
    let target = app.read(|state, _| {
        let view = state.runtimes().ready().expect("the runtimes are listed");
        view.runtimes
            .iter()
            .find(|r| r.id.name.as_deref() == Some("a100-7f3a"))
            .map(|r| r.id.clone())
            .expect("the demo session is listed")
    });
    let two_hours = Duration::from_secs(2 * 3600);
    app.mock
        .extend_session(&target, two_hours, &CancelToken::new())
        .expect("the first extension");
    assert_eq!(
        time_left(&mut app, "a100-7f3a"),
        Some(before),
        "the listing on screen is the one from before"
    );
    app.click(&extend_selector("a100-7f3a"));
    let after = time_left(&mut app, "a100-7f3a").expect("still an auto-stop");
    assert_close(after, before + 2 * two_hours, "after two extensions");
}

#[gpui::test]
fn extend_never_pulls_a_longer_auto_stop_in(cx: &mut TestAppContext) {
    use colab_playground::ui::pages::sessions::extend_selector;
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    provision(&mut app, |spec| {
        spec.accelerator = Accelerator::T4;
        spec.min_vram_mib = None;
        spec.guard.max_runtime = Some(Duration::from_secs(4 * 3600));
    });
    let Some(ProvisionPhase::Ready(outcome)) = phase(&mut app) else {
        panic!("expected a ready runtime, got {:?}", phase(&mut app));
    };
    app.click("overlay-cancel");
    let name = outcome.name;
    let before = time_left(&mut app, &name).expect("a 4 h auto-stop");
    assert_close(
        before,
        Duration::from_secs(4 * 3600),
        "the session's own 4 h",
    );
    app.click(&extend_selector(&name));
    let after = time_left(&mut app, &name).expect("still an auto-stop");
    assert!(after >= before, "never earlier: {before:?} -> {after:?}");
    assert_close(after, Duration::from_secs(6 * 3600), "4 h + 2 h");
}

#[gpui::test]
fn extend_after_setting_an_auto_stop_again_adds_too(cx: &mut TestAppContext) {
    use colab_playground::ui::pages::sessions::{
        cancel_auto_stop_selector, extend_selector, set_auto_stop_selector,
    };
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    app.click(&cancel_auto_stop_selector("a100-7f3a"));
    app.click(colab_playground::ui::overlays::CANCEL_AUTO_STOP_CONFIRM);
    app.click(&set_auto_stop_selector("a100-7f3a"));
    let set = time_left(&mut app, "a100-7f3a").expect("set again");
    assert_close(
        set,
        Duration::from_secs(2 * 3600),
        "Set auto-stop counts from now",
    );
    app.click(&extend_selector("a100-7f3a"));
    let after = time_left(&mut app, "a100-7f3a").expect("still an auto-stop");
    assert_close(after, Duration::from_secs(4 * 3600), "2 h left + 2 h");
}

fn form_scroll(app: &mut TestApp<'_>) -> gpui::ScrollHandle {
    app.read(|state, _| state.new_session().map(|s| s.scroll.clone()))
        .expect("the form is open")
}

fn open_new_session(app: &mut TestApp<'_>) {
    on_sessions(app);
    app.click(NEW_SESSION_SELECTOR);
    assert!(app.shown(NEW_SESSION_FORM), "the form is drawn");
}

fn wheel(app: &mut TestApp<'_>, over: gpui::Bounds<gpui::Pixels>, dy: f32) {
    app.cx.simulate_event(gpui::ScrollEvent {
        position: over.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(dy))),
        modifiers: gpui::Modifiers::none(),
        touch_phase: gpui::TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    app.settle();
}

#[gpui::test]
fn the_new_session_form_takes_the_room_the_window_has_and_scrolls_past_it(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    open_new_session(&mut app);
    assert_eq!(
        form_scroll(&mut app).offset().y,
        px(0.0),
        "opens at the top"
    );
    for (width, height) in [(880.0, 560.0), (1180.0, 780.0), (1180.0, 1100.0)] {
        app.resize(size(px(width), px(height)));
        let form = app.bounds(NEW_SESSION_FORM).expect("the form is drawn");
        let start = app.bounds(NEW_SESSION_START).expect("Start is drawn");
        let reach = form_scroll(&mut app).max_offset().y;
        // Only the form scrolls: the dialog's own body would clip it into the gap above
        // the actions (20 px), and the dialog keeps its 32 px margin to the window.
        assert!(
            start.top() - form.bottom() >= px(19.5),
            "{width}x{height}: the dialog body scrolls too: form ends {:?}, Start at {:?}",
            form.bottom(),
            start.top()
        );
        assert!(
            start.bottom() + px(24.0) <= px(height - 32.0) + px(0.5),
            "{width}x{height}: the dialog overflows the window"
        );
        if height < 1000.0 {
            assert!(reach > px(0.0), "{width}x{height}: the form scrolls");
        } else {
            assert_eq!(reach, px(0.0), "{width}x{height}: the whole form fits");
        }
    }
    app.resize(size(px(880.0), px(560.0)));
    let form = app.bounds(NEW_SESSION_FORM).expect("the form is drawn");
    wheel(&mut app, form, -120.0);
    assert!(
        form_scroll(&mut app).offset().y < px(0.0),
        "the wheel scrolls it"
    );
    let thumb = app.bounds("scrollbar-thumb").expect("a scrollbar shows");
    let start = app.bounds(NEW_SESSION_START).expect("Start is drawn");
    // Start ends where the content does; the card's padding beyond it is 24 px.
    assert!(
        thumb.left() >= start.right()
            && thumb.right() <= start.right() + px(24.0)
            && thumb.top() >= form.top()
            && thumb.bottom() <= form.bottom(),
        "the thumb {thumb:?} runs beside the form {form:?}, in the card's padding"
    );
}

#[gpui::test]
fn the_forward_dialog_with_an_error_scrolls_inside_a_dialog_that_fits_the_window(
    cx: &mut TestAppContext,
) {
    let options = Options {
        failures: MockFailures {
            ssh_busy_attempts: 1,
            ..MockFailures::default()
        },
        ..Options::default()
    };
    let mut app = TestApp::with(cx, options);
    app.resize(size(px(880.0), px(560.0)));
    on_sessions(&mut app);
    app.click(&forward_selector("a100-7f3a"));
    app.fill(|inputs| inputs.forward_remote.clone(), "8000");
    app.click(FORWARD_SUBMIT);
    assert!(app.shown("error-forward"), "the busy error is shown");
    let body = app.bounds(FORWARD_BODY).expect("the body is drawn");
    let submit = app.bounds(FORWARD_SUBMIT).expect("Forward is drawn");
    assert!(
        submit.top() - body.bottom() >= px(19.5),
        "the dialog body scrolls too: content ends {:?}, Forward at {:?}",
        body.bottom(),
        submit.top()
    );
    assert!(
        submit.bottom() + px(24.0) <= px(560.0 - 32.0) + px(0.5),
        "the dialog overflows the window"
    );
    let health = app
        .bounds("forward-health")
        .expect("the health path is drawn");
    assert!(
        health.bottom() > body.bottom(),
        "with the error the content outgrows the smallest window"
    );
    for _ in 0..10 {
        if app.bounds("forward-health").unwrap().bottom() <= body.bottom() {
            break;
        }
        wheel(&mut app, body, -80.0);
    }
    let health = app
        .bounds("forward-health")
        .expect("the health path is drawn");
    assert!(
        health.top() >= body.top() && health.bottom() <= body.bottom() + px(0.5),
        "the wheel brings the last field {health:?} into the body {body:?}"
    );
    let thumb = app.bounds("scrollbar-thumb").expect("a scrollbar shows");
    assert!(
        thumb.left() >= submit.right()
            && thumb.right() <= submit.right() + px(24.0)
            && thumb.top() >= body.top()
            && thumb.bottom() <= body.bottom(),
        "the thumb {thumb:?} runs beside the body {body:?}, in the card's padding"
    );
    // A taller window gives the body room until the error and every field fit.
    app.resize(size(px(880.0), px(780.0)));
    let tall = app.bounds(FORWARD_BODY).expect("the body is drawn");
    let error = app
        .bounds("error-forward")
        .expect("the error is still shown");
    let health = app
        .bounds("forward-health")
        .expect("the health path is drawn");
    assert!(
        tall.size.height > body.size.height,
        "the body grows with the window: {body:?} -> {tall:?}"
    );
    assert!(
        error.top() >= tall.top() && health.bottom() <= tall.bottom() + px(0.5),
        "everything fits: error {error:?}, last field {health:?}, body {tall:?}"
    );
}

#[gpui::test]
fn turning_on_a_limit_at_the_bottom_of_the_form_scrolls_its_value_into_view(
    cx: &mut TestAppContext,
) {
    use colab_playground::ui::provision_form::field_selector;
    use gpui::{point, px};
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    app.resize(size(px(880.0), px(560.0)));
    open_new_session(&mut app);
    app.update(|state, cx| state.edit_new_session(|spec| spec.guard.idle_timeout = None, cx));
    let scroll = form_scroll(&mut app);
    scroll.set_offset(point(px(0.0), -scroll.max_offset().y));
    app.settle();
    let idle = field_selector("idle_timeout");
    let form = app.bounds(NEW_SESSION_FORM).expect("the form is drawn");
    let control = app.bounds(&idle).expect("the idle limit is drawn");
    assert!(control.bottom() <= form.bottom(), "scrolled to the end");
    let before = scroll.offset().y;
    // The switch leads its control.
    click_at(app.cx, point(control.left() + px(10.0), control.center().y));
    assert!(
        app.read(|state, _| state
            .new_session()
            .map(|s| s.spec.guard.idle_timeout.is_some()))
            == Some(true),
        "the click turned the limit on"
    );
    let control = app.bounds(&idle).expect("the idle limit is drawn");
    let form = app.bounds(NEW_SESSION_FORM).expect("the form is drawn");
    assert!(
        scroll.offset().y < before,
        "the form scrolled on: {before:?} -> {:?}",
        scroll.offset().y
    );
    assert!(
        control.top() >= form.top() - px(0.5) && control.bottom() <= form.bottom() + px(0.5),
        "the value {control:?} is inside the form {form:?}"
    );
}

fn name_field(app: &mut TestApp<'_>) -> String {
    app.read_root(|root, cx| root.inputs().session_name.read(cx).text().to_owned())
}

fn pick_hardware(app: &mut TestApp<'_>, accelerator: Accelerator) {
    let index = Accelerator::ALL
        .iter()
        .position(|a| *a == accelerator)
        .expect("a known accelerator");
    app.pick_option(
        &provision_form::field_selector("accelerator"),
        index,
        Accelerator::ALL.len(),
    );
    assert_eq!(
        app.read(|state, _| state.new_session().map(|s| s.spec.accelerator)),
        Some(accelerator)
    );
}

#[gpui::test]
fn the_generated_name_follows_the_chosen_hardware(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    assert_eq!(
        app.read(|state, _| state.defaults().accelerator),
        Accelerator::A100
    );
    open_new_session(&mut app);
    let first = name_field(&mut app);
    assert!(first.starts_with("a100-"), "{first}");

    pick_hardware(&mut app, Accelerator::T4);
    let second = name_field(&mut app);
    assert!(second.starts_with("t4-"), "{second}");

    pick_hardware(&mut app, Accelerator::None);
    let third = name_field(&mut app);
    assert!(third.starts_with("cpu-"), "{third}");

    // An edit that leaves the hardware alone leaves the name alone.
    app.click(&provision_form::field_selector("high_mem"));
    assert!(app.read(|state, _| state.new_session().is_some_and(|s| s.spec.high_mem)));
    assert_eq!(name_field(&mut app), third);

    app.click(NEW_SESSION_START);
    let Some(ProvisionPhase::Ready(outcome)) = phase(&mut app) else {
        panic!("expected a ready runtime, got {:?}", phase(&mut app));
    };
    assert_eq!(outcome.name, third);
}

#[gpui::test]
fn a_typed_name_survives_a_hardware_change(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    open_new_session(&mut app);
    app.fill(|inputs| inputs.session_name.clone(), "my-experiment");
    pick_hardware(&mut app, Accelerator::T4);
    assert_eq!(name_field(&mut app), "my-experiment");

    // An emptied field starts the session under a name made for the hardware chosen since.
    app.fill(|inputs| inputs.session_name.clone(), "");
    app.click(NEW_SESSION_START);
    let Some(ProvisionPhase::Ready(outcome)) = phase(&mut app) else {
        panic!("expected a ready runtime, got {:?}", phase(&mut app));
    };
    assert!(outcome.name.starts_with("t4-"), "{}", outcome.name);
}

#[gpui::test]
fn the_form_shows_what_the_chosen_hardware_burns(cx: &mut TestAppContext) {
    use colab_playground::{format, ui::provision_form::rate_selector};
    use colab_playground_core::{mock::mock_rate, usage::published_rate};
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    let pick = |app: &mut TestApp<'_>, accelerator, high_mem| {
        app.update(|state, cx| {
            state.edit_new_session(
                |spec| {
                    spec.accelerator = accelerator;
                    spec.high_mem = high_mem;
                    spec.min_vram_mib = None;
                },
                cx,
            )
        });
    };
    let shows = |app: &mut TestApp<'_>, rate: f64| {
        let text = app
            .cx
            .update(|_, cx| format::approx(cx, &format::cu_rate(cx, rate)));
        assert!(app.shown(&rate_selector(&text)), "{text}");
    };
    let published = published_rate(Accelerator::T4, false);
    assert_ne!(published, mock_rate(Accelerator::T4));

    // Nothing ran yet: the figure is the one other users published.
    open_new_session(&mut app);
    pick(&mut app, Accelerator::T4, false);
    shows(&mut app, published);
    app.click(NEW_SESSION_START);
    assert!(matches!(phase(&mut app), Some(ProvisionPhase::Ready(_))));
    app.update(|state, cx| state.close_overlay(cx));

    // The T4 is the account's only runtime, so the account's burn rate is its own and
    // replaces the published figure; hardware that never ran keeps the published one.
    open_new_session(&mut app);
    pick(&mut app, Accelerator::T4, false);
    shows(&mut app, mock_rate(Accelerator::T4));
    pick(&mut app, Accelerator::A100, true);
    shows(&mut app, published_rate(Accelerator::A100, true));
}

#[gpui::test]
fn a_refresh_asked_for_during_a_listing_lists_once_more(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    let before = app.mock.runtime_listings();
    app.update(|state, cx| {
        // The first begins; it may read an account before the change behind the others.
        state.load_runtimes(cx);
        state.load_runtimes(cx);
        state.load_runtimes(cx);
    });
    assert_eq!(
        app.mock.runtime_listings(),
        before + 2,
        "the one under way, then one more for every request it could not answer"
    );
    assert!(app.read(|state, _| state.runtimes().ready().is_some()));
}

#[gpui::test]
fn the_periodic_listing_asks_for_no_other(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    let before = app.mock.runtime_listings();
    app.update(|state, cx| {
        state.load_runtimes(cx);
        // Polls that come due while a slow listing runs: it serves them.
        state.poll_runtimes(cx);
        state.poll_runtimes(cx);
    });
    assert_eq!(app.mock.runtime_listings(), before + 1);
    app.update(|state, cx| state.poll_runtimes(cx));
    assert_eq!(
        app.mock.runtime_listings(),
        before + 2,
        "with none under way it lists"
    );
}

fn h100_is_refused() -> Options {
    Options {
        failures: MockFailures {
            not_entitled: vec![Accelerator::H100],
            ..MockFailures::default()
        },
        ..Options::scenario(MockScenario::single())
    }
}

#[gpui::test]
fn a_suggested_name_follows_the_hardware_after_a_failed_attempt(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, h100_is_refused());
    open_new_session(&mut app);
    pick_hardware(&mut app, Accelerator::H100);
    let refused = name_field(&mut app);
    assert!(refused.starts_with("h100-"), "{refused}");
    app.click(NEW_SESSION_START);
    assert!(matches!(phase(&mut app), Some(ProvisionPhase::Failed(_))));

    app.click("new-session-change");
    assert_eq!(name_field(&mut app), refused);
    pick_hardware(&mut app, Accelerator::T4);
    let shown = name_field(&mut app);
    assert!(shown.starts_with("t4-"), "{shown}");

    app.click(NEW_SESSION_START);
    let Some(ProvisionPhase::Ready(outcome)) = phase(&mut app) else {
        panic!("expected a ready runtime, got {:?}", phase(&mut app));
    };
    assert_eq!(outcome.name, shown);
}

#[gpui::test]
fn a_typed_name_survives_a_hardware_change_after_a_failed_attempt(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, h100_is_refused());
    open_new_session(&mut app);
    pick_hardware(&mut app, Accelerator::H100);
    app.fill(|inputs| inputs.session_name.clone(), "my-experiment");
    app.click(NEW_SESSION_START);
    assert!(matches!(phase(&mut app), Some(ProvisionPhase::Failed(_))));

    app.click("new-session-change");
    pick_hardware(&mut app, Accelerator::T4);
    assert_eq!(name_field(&mut app), "my-experiment");

    app.fill(|inputs| inputs.session_name.clone(), "");
    app.click(NEW_SESSION_START);
    let Some(ProvisionPhase::Ready(outcome)) = phase(&mut app) else {
        panic!("expected a ready runtime, got {:?}", phase(&mut app));
    };
    assert!(outcome.name.starts_with("t4-"), "{}", outcome.name);
}

#[gpui::test]
fn an_emptied_name_field_after_a_failed_attempt_starts_under_a_suggested_name(
    cx: &mut TestAppContext,
) {
    let mut app = TestApp::with(cx, h100_is_refused());
    open_new_session(&mut app);
    pick_hardware(&mut app, Accelerator::H100);
    app.fill(|inputs| inputs.session_name.clone(), "my-experiment");
    app.click(NEW_SESSION_START);
    assert!(matches!(phase(&mut app), Some(ProvisionPhase::Failed(_))));

    app.click("new-session-change");
    app.set_failures(MockFailures::default());
    app.fill(|inputs| inputs.session_name.clone(), "");
    app.click(NEW_SESSION_START);
    let Some(ProvisionPhase::Ready(outcome)) = phase(&mut app) else {
        panic!("expected a ready runtime, got {:?}", phase(&mut app));
    };
    assert!(outcome.name.starts_with("h100-"), "{}", outcome.name);
}

#[gpui::test]
fn keep_plan_local_sync_and_vm_upload_work_from_session_card(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local_file = tmp.path().join("weights.bin");
    std::fs::write(&local_file, b"demo-weights").expect("write local upload file");
    let keep_dir = tmp.path().join("kept");

    let mut app = TestApp::new(cx);
    on_sessions(&mut app);

    app.click(&upload_selector("a100-7f3a"));
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::VmUpload(_))
    ));
    app.fill(
        |inputs| inputs.upload_local.clone(),
        &local_file.to_string_lossy(),
    );
    app.fill(
        |inputs| inputs.upload_remote.clone(),
        "/content/out/weights.bin",
    );
    app.click(VM_UPLOAD_SUBMIT);
    assert!(
        app.read(|state, _| state.overlay().is_none()),
        "upload dialog closes on success"
    );

    app.click(&keep_selector("a100-7f3a"));
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::KeepPlan(_))
    ));
    app.fill(|inputs| inputs.keep_paths.clone(), "/content/out");
    app.click(KEEP_LOCAL_TOGGLE);
    app.fill(
        |inputs| inputs.keep_local_folder.clone(),
        &keep_dir.to_string_lossy(),
    );
    app.click(KEEP_SUBMIT);
    assert!(
        app.read(|state, _| state.overlay().is_none()),
        "keep plan dialog closes on save"
    );
    assert!(app.shown(&keep_summary_selector("a100-7f3a")));
    assert!(app.shown(&sync_keep_selector("a100-7f3a")));

    app.click(&sync_keep_selector("a100-7f3a"));
    let report = app.read(|state, _| {
        state
            .runtimes()
            .ready()
            .and_then(|v| {
                v.runtimes
                    .iter()
                    .find(|r| r.id.name.as_deref() == Some("a100-7f3a"))
                    .and_then(|r| r.tracked.as_ref())
                    .and_then(|t| t.last_keep_sync.clone())
            })
    });
    let report = report.expect("last_keep_sync recorded");
    assert!(report.all_succeeded(), "{report:?}");
}

#[gpui::test]
fn drive_mount_is_opt_in_with_warning_and_handles_cancel_and_confirm(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);

    app.click(&keep_selector("a100-7f3a"));
    assert!(
        !app.shown(KEEP_DRIVE_WARNING),
        "Drive is off by default until opted in"
    );
    app.fill(|inputs| inputs.keep_paths.clone(), "/content/out");
    app.click(KEEP_DRIVE_TOGGLE);
    assert!(
        app.shown(KEEP_DRIVE_WARNING),
        "opting into Drive shows explicit exposure warning"
    );
    app.click(KEEP_SUBMIT);
    assert!(app.read(|state, _| state.overlay().is_none()));
    assert!(app.shown(&drive_mount_selector("a100-7f3a")));

    app.click(&drive_mount_selector("a100-7f3a"));
    let Some(Overlay::DriveMount(dialog)) = app.read(|state, _| state.overlay().cloned()) else {
        panic!("expected Drive mount dialog");
    };
    let DriveMountPhase::Consent { url } = dialog.phase else {
        panic!("expected consent URL phase, got {:?}", dialog.phase);
    };
    assert_eq!(app.opened_url().as_deref(), Some(url.as_str()));
    assert!(app.shown(KEEP_DRIVE_WARNING));
    assert!(app.shown(DRIVE_MOUNT_CONFIRM));

    app.click("overlay-cancel");
    assert!(app.read(|state, _| state.overlay().is_none()));
    let mounted_after_cancel = app.read(|state, _| {
        state
            .runtimes()
            .ready()
            .and_then(|v| {
                v.runtimes
                    .iter()
                    .find(|r| r.id.name.as_deref() == Some("a100-7f3a"))
                    .and_then(|r| r.tracked.as_ref())
                    .and_then(|t| t.keep_plan.as_ref())
                    .and_then(|p| p.drive.as_ref())
                    .map(|d| d.mounted)
            })
            .unwrap_or(false)
    });
    assert!(!mounted_after_cancel, "cancelling consent must not mount Drive");

    app.click(&drive_mount_selector("a100-7f3a"));
    app.click(DRIVE_MOUNT_CONFIRM);
    assert!(app.read(|state, _| state.overlay().is_none()));
    let mounted_after_confirm = app.read(|state, _| {
        state
            .runtimes()
            .ready()
            .and_then(|v| {
                v.runtimes
                    .iter()
                    .find(|r| r.id.name.as_deref() == Some("a100-7f3a"))
                    .and_then(|r| r.tracked.as_ref())
                    .and_then(|t| t.keep_plan.as_ref())
                    .and_then(|p| p.drive.as_ref())
                    .map(|d| d.mounted)
            })
            .unwrap_or(false)
    });
    assert!(mounted_after_confirm, "confirming consent mounts Drive");
    assert!(!app.shown(&drive_mount_selector("a100-7f3a")));
}

#[gpui::test]
fn pre_stop_keep_plan_failure_aborts_stop_until_user_forces_stop(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let keep_dir = tmp.path().join("kept");

    let mut app = TestApp::new(cx);
    on_sessions(&mut app);

    app.click(&keep_selector("a100-7f3a"));
    app.fill(|inputs| inputs.keep_paths.clone(), "/content/out");
    app.click(KEEP_LOCAL_TOGGLE);
    app.fill(
        |inputs| inputs.keep_local_folder.clone(),
        &keep_dir.to_string_lossy(),
    );
    app.click(KEEP_SUBMIT);
    assert!(app.read(|state, _| state.overlay().is_none()));

    app.set_failures(MockFailures {
        keep_sync_fails: true,
        ..MockFailures::default()
    });

    app.click(&stop_selector("a100-7f3a"));
    let Some(Overlay::Stop(prompt)) = app.read(|state, _| state.overlay().cloned()) else {
        panic!("expected stop confirmation dialog");
    };
    assert!(prompt.has_keep_plan && prompt.save_keep);
    assert!(app.shown(STOP_KEEP_TOGGLE));

    app.click(STOP_CONFIRM);
    let Some(Overlay::Stop(prompt)) = app.read(|state, _| state.overlay().cloned()) else {
        panic!("stop dialog must remain open on Keep Plan failure");
    };
    assert!(prompt.keep_failed && prompt.error.is_some());
    assert!(
        runtime_names(&mut app).contains(&"a100-7f3a".to_owned()),
        "runtime must stay running when Keep Plan fails before stop"
    );
    assert!(app.shown(STOP_FORCE_CONFIRM));

    app.click(STOP_FORCE_CONFIRM);
    assert!(app.read(|state, _| state.overlay().is_none()));
    assert!(
        !runtime_names(&mut app).contains(&"a100-7f3a".to_owned()),
        "forcing stop releases the runtime without saving"
    );
}

#[gpui::test]
fn open_in_colab_opens_the_runtime_url_without_creating_another_vm(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    let before_count = runtime_names(&mut app).len();

    app.click(&open_browser_selector("a100-7f3a"));
    let opened = app.opened_url().expect("the Colab runtime URL was opened");
    assert!(
        opened.starts_with("https://colab.research.google.com/notebooks/empty.ipynb?dbu="),
        "{opened}"
    );
    assert_eq!(app.mock.opened_urls(), vec![opened]);
    assert_eq!(
        runtime_names(&mut app).len(),
        before_count,
        "opening the attached URL must not allocate another VM"
    );
    let toast = app.cx.update(|_, cx| {
        i18n::tr_args(
            cx,
            i18n::keys::OPEN_BROWSER_DONE,
            &[("name", "a100-7f3a")],
        )
    });
    assert!(app.toasts().contains(&toast), "{:?}", app.toasts());
}

#[gpui::test]
fn restart_kernel_confirms_once_and_keeps_the_runtime(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    app.click(&restart_kernel_selector("a100-7f3a"));
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::RestartKernel(ref prompt)) if prompt.label == "a100-7f3a"
    ));
    assert!(
        app.mock.restarted_kernels().is_empty(),
        "kernel must not restart before confirmation"
    );
    assert!(app.shown(RESTART_KERNEL_CONFIRM));

    app.click(RESTART_KERNEL_CONFIRM);
    assert!(app.read(|state, _| state.overlay().is_none()));
    assert_eq!(app.mock.restarted_kernels(), vec!["a100-7f3a".to_owned()]);
    assert!(
        runtime_names(&mut app).contains(&"a100-7f3a".to_owned()),
        "restarting the kernel keeps the runtime alive"
    );

    app.set_failures(MockFailures {
        restart_kernel_fails: true,
        ..MockFailures::default()
    });
    app.click(&restart_kernel_selector("a100-7f3a"));
    app.click(RESTART_KERNEL_CONFIRM);
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::RestartKernel(ref prompt))
            if prompt.error.as_ref().is_some_and(|e| e.is(ErrorKind::ConnectionLost))
    ));
    assert!(app.shown("error-restart-kernel"));
}

#[gpui::test]
fn ssh_busy_offers_reset_ssh_recovery_in_forward_dialog_and_inactive_or_unnamed_actions_are_guarded(
    cx: &mut TestAppContext,
) {
    let options = Options {
        failures: MockFailures {
            ssh_busy_attempts: 5,
            ..MockFailures::default()
        },
        ..Options::default()
    };
    let mut app = TestApp::with(cx, options);
    on_sessions(&mut app);

    // Inactive account shows Switch instead of active-only controls.
    assert!(!app.shown(&open_browser_selector("cpu-0b5e")));
    assert!(!app.shown(&restart_kernel_selector("cpu-0b5e")));
    assert!(!app.shown(&ide_ssh_selector("cpu-0b5e")));
    assert!(app.shown(&switch_selector("cpu-0b5e")));

    // Unnamed runtime on the active account disables named-only controls.
    assert!(app.shown(&open_browser_selector("L4")));
    assert!(app.shown(&restart_kernel_selector("L4")));
    assert!(app.shown(&ide_ssh_selector("L4")));
    app.click(&open_browser_selector("L4"));
    assert!(app.opened_url().is_none());
    app.click(&restart_kernel_selector("L4"));
    assert!(app.read(|state, _| state.overlay().is_none()));
    app.click(&ide_ssh_selector("L4"));
    assert!(app.read(|state, _| state.overlay().is_none()));

    // Hitting HTTP 429 (`ErrorKind::SshBusy`) offers `Reset SSH sessions` recovery.
    app.click(&forward_selector("a100-7f3a"));
    app.fill(|inputs| inputs.forward_remote.clone(), "8000");
    app.click(FORWARD_SUBMIT);
    assert!(app.shown("error-forward"));
    assert!(app.shown(FORWARD_RESET_SSH));

    app.click(FORWARD_RESET_SSH);
    assert_eq!(app.mock.ssh_resets(), vec!["a100-7f3a".to_owned()]);
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::Forward(ref form)) if form.error.is_none()
    ));

    app.click(FORWARD_SUBMIT);
    assert!(
        app.read(|state, _| state.overlay().is_none()),
        "forwarding succeeds right after clearing stale SSH sessions"
    );
}

#[gpui::test]
fn plugin_run_form_configures_keep_plan_from_manifest_paths(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let keep_dir = tmp.path().join("plugin-kept");

    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    app.click(Route::Plugins.nav_selector());
    app.click("plugin-run comfyui");
    app.click(RUN_KEEP_CONFIGURE);
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::KeepPlan(_))
    ));
    let paths = app.read_root(|root, cx| root.inputs().keep_paths.read(cx).text().to_owned());
    assert_eq!(paths, "/content/ComfyUI/output");

    app.click(KEEP_LOCAL_TOGGLE);
    app.fill(
        |inputs| inputs.keep_local_folder.clone(),
        &keep_dir.to_string_lossy(),
    );
    app.click(KEEP_SUBMIT);
    assert!(app.read(|state, _| state.overlay().is_none()));
    let attached = app.read(|state, _| state.run_form().and_then(|f| f.keep_plan.clone()));
    let plan = attached.expect("Keep Plan attached to run form");
    assert_eq!(plan.paths, vec!["/content/ComfyUI/output".to_owned()]);
    assert!(plan.local.is_some());
}

#[gpui::test]
fn remote_ssh_dialog_shows_snippets_writes_isolated_temp_config_and_resets_ssh(
    cx: &mut TestAppContext,
) {
    let mut app = TestApp::new(cx);
    on_sessions(&mut app);
    app.click(&ide_ssh_selector("a100-7f3a"));
    assert!(app.shown(REMOTE_SSH_BODY));

    let temp_path = {
        let Some(Overlay::RemoteSsh(prompt)) = app.read(|state, _| state.overlay().cloned())
        else {
            panic!("expected RemoteSsh overlay");
        };
        let Load::Ready(info) = prompt.info else {
            panic!("expected loaded RemoteSsh info, got {:?}", prompt.info);
        };
        assert_eq!(info.host_alias, "colab-a100-7f3a");
        assert!(info.command.contains("ProxyCommand="));
        assert!(info.config_snippet.contains("--config"));
        assert!(!info.command.contains(".ssh/config"));
        assert!(!info.config_snippet.contains(".ssh/config"));
        assert!(prompt.temp_config_path.is_none());

        let body = app.bounds(REMOTE_SSH_BODY).expect("remote ssh body drawn");
        for _ in 0..10 {
            if app.bounds(REMOTE_SSH_WRITE_TEMP).unwrap().bottom() <= body.bottom() {
                break;
            }
            wheel(&mut app, body, -80.0);
        }
        app.click(REMOTE_SSH_WRITE_TEMP);
        let Some(Overlay::RemoteSsh(updated)) = app.read(|state, _| state.overlay().cloned())
        else {
            panic!("expected RemoteSsh overlay");
        };
        assert!(
            updated.temp_config_error.is_none(),
            "unexpected temp config error: {:?}",
            updated.temp_config_error
        );
        let written = updated.temp_config_path.expect("temp config written");
        assert!(app.shown(REMOTE_SSH_TEMP_COMMAND));
        assert!(!written.ends_with(".ssh/config"), "{written}");
        assert!(std::fs::read_to_string(&written).unwrap().contains("Host colab-a100-7f3a"));
        written
    };
    let _ = std::fs::remove_file(&temp_path);

    let body = app.bounds(REMOTE_SSH_BODY).expect("remote ssh body drawn");
    for _ in 0..10 {
        if app.bounds(REMOTE_SSH_RESET_SSH).unwrap().bottom() <= body.bottom() {
            break;
        }
        wheel(&mut app, body, -80.0);
    }
    app.click(REMOTE_SSH_RESET_SSH);
    assert_eq!(app.mock.ssh_resets(), vec!["a100-7f3a".to_owned()]);
}
