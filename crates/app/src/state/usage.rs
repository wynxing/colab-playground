//! Compute-unit usage: polling at core's cadence, the cache and what the UI shows.
//!
//! A foreground loop wakes every [`POLL_CHECK`] and, when idle, asks the backend whether a
//! refresh is due (`usage_poll_due`, whose cadence comes from `Defaults`; no I/O) and, if so,
//! refreshes on the background executor. The backend's own clock (`now()`) comes back with
//! every result, so ages are measured on the same clock as `fetched_at`.

use std::time::{Duration, SystemTime};

use colab_playground_core::{
    Error, ErrorKind,
    accounts::AccountId,
    backend::AccountUsageView,
    guard::USAGE_STALE_AFTER,
    provision::ProvisionSpec,
    sessions::HardwareRate,
    usage::{RateReading, UsageSnapshot},
};
use gpui::{Context, Task};

use super::{AccountDialog, AppState, Op};
use crate::i18n::keys;

/// How often the poller asks whether a usage refresh is due (the cadence itself is a policy
/// value in `Defaults`).
pub const POLL_CHECK: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default)]
pub(crate) struct UsageCache {
    pub(crate) views: Vec<AccountUsageView>,
    /// What each hardware burned when it was last measured, as of the last refresh.
    pub(crate) rates: Vec<HardwareRate>,
    pub(crate) now: Option<SystemTime>,
    pub(crate) error: Option<Error>,
}

/// Usage totals across accounts; unknown accounts are counted, never added as zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSummary {
    /// Sum of known balances.
    pub balance_cu: f64,
    /// Sum of known burn rates.
    pub rate_cu_per_hour: f64,
    /// Sum of known running runtimes.
    pub runtimes: u32,
    pub known: usize,
    pub unknown: usize,
    pub stale: bool,
}

pub fn summarize<'a>(views: impl IntoIterator<Item = &'a AccountUsageView>) -> UsageSummary {
    let mut summary = UsageSummary::default();
    for view in views {
        match view.snapshot {
            Some(snapshot) => {
                summary.balance_cu += snapshot.balance_cu;
                summary.rate_cu_per_hour += snapshot.rate_cu_per_hour;
                summary.runtimes += snapshot.assignments;
                summary.known += 1;
                summary.stale |= snapshot.stale;
            }
            None => summary.unknown += 1,
        }
    }
    summary
}

/// Whether `snapshot` should carry the stale marker at `now`: a later fetch failed, or it is
/// older than core's `USAGE_STALE_AFTER`.
pub fn is_stale(snapshot: &UsageSnapshot, now: Option<SystemTime>) -> bool {
    match now {
        Some(now) => !snapshot.is_fresh(now, USAGE_STALE_AFTER),
        None => snapshot.stale,
    }
}

/// The note that explains a stale `snapshot`: a refresh failed, or (with a slow idle cadence
/// in `Defaults`) the values are only old. Saying a refresh failed when none did would not be
/// honest.
pub fn stale_note(snapshot: &UsageSnapshot) -> &'static str {
    if snapshot.stale {
        keys::USAGE_TIP_STALE
    } else {
        keys::USAGE_TIP_OLD
    }
}

fn needs_unmanaged_warning(view: &AccountUsageView) -> bool {
    view.snapshot.is_some_and(|s| s.assignments > 0)
        && (!view.ownership_verified || view.unmanaged_runtimes > 0)
}

pub(crate) fn start_poller(cx: &mut Context<AppState>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(POLL_CHECK).await;
            let alive = this.update(cx, |state, cx| state.poll_tick(cx)).is_ok();
            if !alive {
                break;
            }
        }
    })
}

impl AppState {
    pub fn usage(&self) -> &[AccountUsageView] {
        &self.usage.views
    }

    /// The backend's time when usage was last fetched.
    pub fn usage_now(&self) -> Option<SystemTime> {
        self.usage.now
    }

    /// Why the last refresh failed as a whole (per-account failures are in each view).
    pub fn usage_error(&self) -> Option<&Error> {
        self.usage.error.as_ref()
    }

    pub fn usage_for(&self, id: &AccountId) -> Option<&AccountUsageView> {
        self.usage.views.iter().find(|view| &view.account == id)
    }

    pub fn active_usage(&self) -> Option<&AccountUsageView> {
        let id = self.accounts.ready()?.active.as_ref()?;
        self.usage_for(id)
    }

    pub fn usage_summary(&self) -> UsageSummary {
        let saved: Vec<AccountId> = self
            .accounts
            .ready()
            .map(|view| view.accounts.iter().map(|a| a.id.clone()).collect())
            .unwrap_or_default();
        let mut summary = summarize(
            self.usage
                .views
                .iter()
                .filter(|view| saved.contains(&view.account)),
        );
        let missing = saved
            .iter()
            .filter(|id| self.usage_for(id).is_none())
            .count();
        summary.unknown += missing;
        summary
    }

    /// Saved accounts with no usage snapshot yet (not loaded, or every fetch failed), by email
    /// (`None` when the account has none): totals do not count them.
    pub fn uncounted_accounts(&self) -> Vec<Option<String>> {
        self.accounts
            .ready()
            .map(|view| {
                view.accounts
                    .iter()
                    .filter(|account| {
                        self.usage_for(&account.id)
                            .is_none_or(|usage| usage.snapshot.is_none())
                    })
                    .map(|account| account.email.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Accounts with runtimes this app does not manage, and how many.
    pub fn unmanaged(&self) -> Vec<(Option<String>, Option<u32>, bool)> {
        self.usage
            .views
            .iter()
            .filter(|view| needs_unmanaged_warning(view))
            .map(|view| {
                (
                    view.email.clone(),
                    view.ownership_verified.then_some(view.unmanaged_runtimes),
                    view.snapshot
                        .is_some_and(|s| is_stale(&s, self.usage_now())),
                )
            })
            .collect()
    }

    /// Fetches usage: always when `manual`, otherwise only if core says a poll is due. The
    /// runtimes are listed once it is back; `manual` is the user asking for what is there now,
    /// so a listing under way then, which may have begun before the click, is followed by one
    /// more.
    pub fn refresh_usage(&mut self, manual: bool, cx: &mut Context<Self>) {
        self.fetch_usage(manual, manual, cx);
    }

    /// Fetches usage after a change. The caller asks for the listing that change needs, so the
    /// one after usage asks for no other.
    pub(crate) fn refresh_usage_after_change(&mut self, cx: &mut Context<Self>) {
        self.fetch_usage(true, false, cx);
    }

    /// What a runtime with `spec`'s hardware burned when it was last measured, and the
    /// backend's clock to tell how long ago that was.
    pub fn measured_rate(&self, spec: &ProvisionSpec) -> Option<(RateReading, SystemTime)> {
        let rate = self
            .usage
            .rates
            .iter()
            .find(|rate| rate.matches(spec.accelerator, spec.high_mem))?;
        Some((rate.reading, self.usage.now?))
    }

    fn fetch_usage(&mut self, manual: bool, relist: bool, cx: &mut Context<Self>) {
        if !self.cli_installed() {
            return;
        }
        if !manual
            && let Ok(backend) = &self.backend
            && !backend.usage_poll_due()
        {
            let now = backend.now();
            let stale_changed = self
                .usage
                .views
                .iter()
                .filter_map(|view| view.snapshot.as_ref())
                .any(|snapshot| {
                    is_stale(snapshot, Some(now)) != is_stale(snapshot, self.usage.now)
                });
            self.usage.now = Some(now);
            self.check_auto_stop_soon(cx);
            if stale_changed {
                cx.notify();
            }
            return;
        }
        // While the app manages runtimes, each due poll is also a guard round (core's
        // `guard_poll` fetches usage for their accounts and stops what crossed a limit).
        let guard = self.has_managed_runtimes();
        self.run_backend(
            Op::Usage,
            cx,
            move |backend, cancel| {
                if guard {
                    match backend.guard_poll(cancel) {
                        Ok(report) => {
                            let covered: std::collections::BTreeSet<AccountId> = report
                                .decisions
                                .iter()
                                .map(|d| d.session.account.clone())
                                .chain(report.vanished.iter().map(|v| v.account.clone()))
                                .collect();
                            let uncovered: Vec<AccountId> = report
                                .usage
                                .iter()
                                .map(|u| u.account.clone())
                                .filter(|id| !covered.contains(id))
                                .collect();
                            // Asked again on purpose: the round just fetched the accounts
                            // with sessions, so this is whether one of the others is due.
                            let views =
                                if !uncovered.is_empty() && (manual || backend.usage_poll_due()) {
                                    backend.refresh_usage(&uncovered, cancel)?
                                } else {
                                    report.usage.clone()
                                };
                            Ok((
                                views,
                                Some(Ok(report)),
                                backend.measured_rates(),
                                backend.now(),
                            ))
                        }
                        Err(error) => {
                            let views = backend.refresh_usage(&[], cancel)?;
                            Ok((
                                views,
                                Some(Err(error)),
                                backend.measured_rates(),
                                backend.now(),
                            ))
                        }
                    }
                } else {
                    let views = backend.refresh_usage(&[], cancel)?;
                    Ok((views, None, backend.measured_rates(), backend.now()))
                }
            },
            move |state, result, cx| match result {
                Ok((views, report, rates, now)) => {
                    state.usage.views = views;
                    state.usage.rates = rates;
                    state.usage.now = Some(now);
                    state.usage.error = None;
                    match report {
                        Some(Ok(report)) => state.apply_guard(report, cx),
                        Some(Err(error)) if error.is(ErrorKind::Busy) => {}
                        Some(Err(error)) => {
                            log::warn!("guard: the round failed: {error}");
                            state.sessions.guard_error = Some(error);
                        }
                        None => {}
                    }
                    state.check_auto_stop_soon(cx);
                    if relist {
                        state.load_runtimes(cx);
                    } else {
                        state.poll_runtimes(cx);
                    }
                    cx.notify();
                }
                Err(error) if error.is(ErrorKind::Busy) && !manual => {
                    log::info!("usage: the account slot is busy; trying again later");
                }
                Err(error) => {
                    log::warn!("usage: refresh failed: {error}");
                    state.usage.error = Some(error);
                }
            },
        );
    }

    /// One poller wake: refreshes when nothing would be disturbed and core says it is due.
    fn poll_tick(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.update_prompt().map(|prompt| &prompt.phase),
            Some(super::UpdatePhase::Restarting)
        ) {
            // About to restart into an update: start nothing new it would have to wait for.
            self.settle_pending_restart(cx);
            return;
        }
        if self.is_provisioning() || self.ops.is_running(Op::PluginRun) {
            // A provisioning progress shows how long Colab has not answered.
            cx.notify();
        }
        // The update check needs no account; its own daily cadence decides whether it asks.
        self.maybe_check_updates(cx);
        let signing_in = matches!(self.dialog, Some(AccountDialog::Add(_)));
        let has_accounts = self
            .accounts
            .ready()
            .is_some_and(|view| !view.accounts.is_empty());
        if signing_in || !has_accounts || self.ops.pausing_poll() {
            return;
        }
        self.refresh_usage(false, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use colab_playground_core::{backend::usage_view, usage::AccountUsage};

    #[test]
    fn unknown_ownership_only_warns_when_the_reading_has_runtimes() {
        let mut view = usage_view(
            &AccountId::new("account").unwrap(),
            None,
            &AccountUsage::default(),
            0,
        );
        assert!(!needs_unmanaged_warning(&view));
        for stale in [false, true] {
            view.snapshot = Some(UsageSnapshot {
                balance_cu: 10.0,
                rate_cu_per_hour: 0.0,
                assignments: 0,
                fetched_at: SystemTime::UNIX_EPOCH,
                stale,
            });
            assert!(!view.ownership_verified);
            assert!(!needs_unmanaged_warning(&view));
            view.snapshot.as_mut().unwrap().assignments = 1;
            assert!(needs_unmanaged_warning(&view));
        }
        view.ownership_verified = true;
        assert!(!needs_unmanaged_warning(&view));
        view.unmanaged_runtimes = 1;
        assert!(needs_unmanaged_warning(&view));
    }
}
