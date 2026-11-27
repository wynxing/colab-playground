//! The Sessions page: every runtime on every saved account, grouped by account, with the
//! actions each allows; provisioning in progress; per-account listing errors; runtimes that
//! ended and why.

mod card;

use colab_playground_core::backend::Runtime;
use ely_gpui_component::{
    buttons::{Button, ButtonVariant, IconButton},
    feedback::{EmptyState, InlineMessage},
    layout::{Card, CardHeader, Page},
    primitives::{IconName, Severity},
    theme::{ActiveTheme, TextSize},
    typography::Caption,
};
use gpui::{AnyElement, App, Entity, FontWeight, IntoElement, ParentElement, Styled, div};

use crate::{
    errors, format,
    i18n::{keys, tr, tr_args, tr_count},
    route::Route,
    state::{AppState, EndReason, Load, Op, ProvisionPhase},
    ui::{
        error_view,
        parts::{busy_line, cancel_work_button, step_icon},
        progress, tagged,
    },
};

pub use card::{
    cancel_auto_stop_selector, drive_mount_selector, extend_selector, forward_selector,
    keep_selector, keep_summary_selector, runtime_selector, set_auto_stop_selector, stop_selector,
    switch_selector, sync_keep_selector, upload_selector,
};

pub const NEW_SESSION_SELECTOR: &str = "sessions-new";
/// Selector of the provisioning card shown while the dialog is hidden.
pub const PROVISIONING_SELECTOR: &str = "sessions-provisioning";
pub const ENDED_SELECTOR: &str = "sessions-ended";
/// Selector of the empty state when every account was listed and nothing runs.
pub const EMPTY_SELECTOR: &str = "sessions-empty";
/// Selector of the empty state when some accounts could not be listed.
pub const EMPTY_PARTIAL_SELECTOR: &str = "sessions-empty-partial";

pub fn render(state: &AppState, handle: &Entity<AppState>, cx: &App) -> Page {
    let new = handle.clone();
    let refresh = handle.clone();
    let ready = state.cli_installed() && state.has_saved_accounts();
    let page = super::page(Route::Sessions, keys::SESSIONS_SUBTITLE, cx).action(
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(tagged(
                "sessions-refresh",
                IconButton::new("sessions-refresh", IconName::RefreshCw)
                    .tooltip(tr(cx, keys::SESSIONS_REFRESH))
                    .variant(ButtonVariant::Ghost)
                    .disabled(!ready || state.is_busy(Op::Runtimes))
                    .on_click(move |_, _, cx| {
                        refresh.update(cx, |state, cx| state.after_runtime_change(cx))
                    }),
            ))
            .child(tagged(
                NEW_SESSION_SELECTOR,
                Button::new(NEW_SESSION_SELECTOR, tr(cx, keys::NEW_TITLE))
                    .icon(IconName::Plus)
                    .primary()
                    .disabled(!ready)
                    .on_click(move |_, _, cx| {
                        new.update(cx, |state, cx| state.open_new_session(cx))
                    }),
            )),
    );
    if !ready {
        let go = handle.clone();
        return page.child(
            EmptyState::new(
                "sessions-setup",
                IconName::Server,
                tr(cx, keys::SESSIONS_SETUP_TITLE),
            )
            .body(tr(cx, keys::SESSIONS_SETUP_BODY))
            .action(
                Button::new("sessions-setup", tr(cx, keys::SESSIONS_SETUP_ACTION)).on_click(
                    move |_, _, cx| go.update(cx, |state, cx| state.navigate(Route::Overview, cx)),
                ),
            ),
        );
    }
    let mut page = page.children(provisioning(state, handle, cx));
    if let Some(error) = state.guard_error() {
        page = page.child(InlineMessage::new(
            Severity::Warning,
            tr_args(
                cx,
                keys::GUARD_ROUND_FAILED,
                &[("why", &errors::explain(error, false, cx).message)],
            ),
        ));
    }
    page = match state.runtimes() {
        Load::Idle | Load::Loading => page.child(busy_line(
            "sessions-loading",
            tr(cx, keys::SESSIONS_LOADING),
        )),
        Load::Failed(error) => {
            let retry = error_view::retry(handle, AppState::load_runtimes);
            page.child(error_view::alert(
                "sessions",
                Some(tr(cx, keys::SESSIONS_FAILED)),
                error,
                Some(retry),
                handle,
                cx,
            ))
        }
        Load::Ready(view) => {
            for failure in &view.errors {
                let email = format::email(cx, state.email_of(&failure.account).as_deref());
                page = page.child(error_view::alert(
                    &format!("sessions-account-{}", failure.account),
                    Some(tr_args(
                        cx,
                        keys::SESSIONS_ACCOUNT_FAILED,
                        &[("email", &email)],
                    )),
                    &failure.error,
                    None,
                    handle,
                    cx,
                ));
            }
            if view.runtimes.is_empty() {
                let new = handle.clone();
                // "Nothing is running" only when every account was listed; otherwise it covers
                // just the accounts that were (the ones that were not are reported above).
                let (selector, title) = if view.errors.is_empty() {
                    (EMPTY_SELECTOR, keys::SESSIONS_EMPTY_TITLE)
                } else {
                    (EMPTY_PARTIAL_SELECTOR, keys::SESSIONS_EMPTY_PARTIAL_TITLE)
                };
                page.child(tagged(
                    selector,
                    EmptyState::new("sessions-empty", IconName::Server, tr(cx, title))
                        .body(tr(cx, keys::SESSIONS_EMPTY_BODY))
                        .action(
                            Button::new("sessions-empty-new", tr(cx, keys::NEW_TITLE))
                                .icon(IconName::Plus)
                                .on_click(move |_, _, cx| {
                                    new.update(cx, |state, cx| state.open_new_session(cx))
                                }),
                        ),
                ))
            } else {
                page.child(Caption::new(tr(cx, keys::SESSIONS_ESTIMATE_NOTE)))
                    .children(groups(&view.runtimes, state, handle, cx))
            }
        }
    };
    page.children(ended(state, handle, cx))
}

/// Runtimes grouped by account, the active account first.
fn groups(
    runtimes: &[Runtime],
    state: &AppState,
    handle: &Entity<AppState>,
    cx: &App,
) -> Vec<AnyElement> {
    let theme = cx.theme();
    // By account, not by email: two accounts can both have none.
    let mut accounts: Vec<Vec<&Runtime>> = Vec::new();
    for runtime in runtimes {
        match accounts
            .iter_mut()
            .find(|list| list[0].id.account == runtime.id.account)
        {
            Some(list) => list.push(runtime),
            None => accounts.push(vec![runtime]),
        }
    }
    accounts.sort_by_key(|list| !list[0].on_active_account);
    accounts
        .into_iter()
        .map(|list| {
            let active = list[0].on_active_account;
            let email = format::email(cx, list[0].account_email.as_deref());
            let heading = if active {
                tr_args(cx, keys::SESSIONS_GROUP_ACTIVE, &[("email", &email)])
            } else {
                tr_args(cx, keys::SESSIONS_GROUP, &[("email", &email)])
            };
            let count = tr_count(
                cx,
                list.len(),
                keys::SESSIONS_COUNT_ONE,
                keys::SESSIONS_COUNT_MANY,
                &[],
            );
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .child(
                            div()
                                .text_size(theme.text_size(TextSize::Base))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(heading),
                        )
                        .child(Caption::new(count)),
                )
                .children(
                    list.into_iter()
                        .map(|runtime| card::render(runtime, state, handle, cx)),
                )
                .into_any_element()
        })
        .collect()
}

/// Provisioning under way while the dialog is hidden: progress, Show and Cancel.
fn provisioning(state: &AppState, handle: &Entity<AppState>, cx: &App) -> Option<AnyElement> {
    let session = state.new_session()?;
    if session.phase != ProvisionPhase::Running || state.overlay().is_some() {
        return None;
    }
    let cancelling = state.is_cancelling_provision();
    let (show, cancel) = (handle.clone(), handle.clone());
    Some(
        tagged(
            PROVISIONING_SELECTOR,
            Card::new()
                .header(
                    CardHeader::new(tr_args(
                        cx,
                        keys::SESSIONS_STARTING,
                        &[("name", &session.spec.name)],
                    ))
                    .description(progress::headline(&session.progress, cx)),
                )
                .child(progress::render(&session.progress, true, cx))
                .children(progress::no_answer_note(
                    &session.progress,
                    state.backend_now(),
                    cx,
                ))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("provisioning-show", tr(cx, keys::ACTION_SHOW))
                                .variant(ButtonVariant::Ghost)
                                .on_click(move |_, _, cx| {
                                    show.update(cx, |state, cx| state.open_new_session(cx))
                                }),
                        )
                        .child(cancel_work_button(
                            "provisioning-cancel",
                            cancelling,
                            move |_, cx| cancel.update(cx, |state, cx| state.cancel_provision(cx)),
                            cx,
                        )),
                ),
        )
        .w_full()
        .into_any_element(),
    )
}

/// Runtimes that ended while the app watched, with the reason.
fn ended(state: &AppState, handle: &Entity<AppState>, cx: &App) -> Option<AnyElement> {
    let ended = state.ended();
    if ended.is_empty() {
        return None;
    }
    let theme = cx.theme();
    let clear = handle.clone();
    let rows = ended.iter().map(|item| {
        let email = format::email(cx, item.email.as_deref());
        let (icon, color, why) = match &item.reason {
            EndReason::User => (
                IconName::Square,
                theme.colors.fg_muted,
                tr(cx, keys::ENDED_USER),
            ),
            EndReason::Guard(concern) => (
                IconName::Shield,
                theme.colors.danger,
                tr_args(
                    cx,
                    keys::ENDED_GUARD,
                    &[("why", &format::concern(cx, concern))],
                ),
            ),
            EndReason::Vanished => (
                IconName::CircleStop,
                theme.colors.fg_muted,
                tr(cx, keys::ENDED_VANISHED),
            ),
        };
        div()
            .flex()
            .items_start()
            .gap_2()
            .py_1()
            .child(div().pt_0p5().child(step_icon(icon, color)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(theme.text_size(TextSize::Sm))
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr_args(
                                cx,
                                keys::ENDED_LINE,
                                &[("name", &item.label), ("email", &email)],
                            )),
                    )
                    .child(Caption::new(why)),
            )
    });
    Some(
        tagged(
            ENDED_SELECTOR,
            Card::new()
                .header(
                    CardHeader::new(tr(cx, keys::ENDED_TITLE)).action(
                        Button::new("ended-clear", tr(cx, keys::ACTION_CLEAR))
                            .variant(ButtonVariant::Ghost)
                            .on_click(move |_, _, cx| {
                                clear.update(cx, |state, cx| state.clear_ended(cx))
                            }),
                    ),
                )
                .child(div().flex().flex_col().children(rows)),
        )
        .w_full()
        .into_any_element(),
    )
}
