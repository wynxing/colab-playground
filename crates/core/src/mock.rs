//! A deterministic, clock-driven simulation of the whole backend.
//!
//! The mock keeps a small world model (accounts with balances that burn down
//! while runtimes exist, managed and unmanaged runtimes on several accounts,
//! tunnels, plugin runs) and reuses the real provisioning loop, guard, plugin
//! validation and quantisation fitting. Time comes from the injected clock and
//! every simulated step waits `step_delay` on it:
//!
//! * headless UI tests: [`MockConfig::headless`] uses a [`ManualClock`], so
//!   everything completes instantly while virtual time advances;
//! * interactive `--mock` runs: [`MockConfig::interactive`] uses the system
//!   clock with a human-paced delay.
//!
//! [`MockFailures`] injects each failure the UI must handle.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use crate::accounts::{Account, AccountId, AccountsView};
use crate::backend::*;
use crate::cli::StopOutcome;
use crate::clock::{CancelToken, Clock, ManualClock, SystemClock};
use crate::defaults::Defaults;
use crate::error::{Error, ErrorKind, Result};
use crate::guard::{Decision, GuardPolicy, SpendMeter, evaluate};
use crate::hf::HfModel;
use crate::host::{Host, Line, ProbeReport, Stream};
use crate::plugins::{
    AfterRun, InputKind, PluginInfo, PluginOrigin, PluginPreview, PluginStore, ResolvedEndpoint,
    RunEnd, RunMode, capability_env, discard_preview, expand_uploads, materialize_assets, redact,
    resolve_endpoints, resolve_inputs, vm_env,
};
use crate::provision::{
    self, Hardware, ProvisionEvent, ProvisionOps, ProvisionOutcome, ProvisionSpec,
};
use crate::quant::{FitInput, GgufFile, GgufListing, fit_auto, group};
use crate::sessions::{
    Accelerator, HardwareRate, Ledger, ServerSession, StopReason, TrackedSession, WatchdogInfo,
};
use crate::tunnel::{TunnelEvent, TunnelId, TunnelInfo, TunnelSpec, TunnelState};
use crate::updater::{Installed, SelfUpdate, Unsupported, UpdateProgress};
use crate::usage::{AccountUsage, RateReading, Usage, poll_due};
use crate::vm::{RunMeta, RunState, RunStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockLoginFailure {
    CancelledBeforeUrl,
    InvalidCode,
    VerificationFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockFailures {
    /// `colab` is not installed (until `install_cli` succeeds).
    pub cli_missing: bool,
    pub install_fails: bool,
    /// A runtime whose directory listing comes back empty, so a save has nothing to copy.
    pub vm_listing_empty: bool,
    pub keyring_unavailable: bool,
    pub login: Option<MockLoginFailure>,
    /// Accelerators the accounts are not entitled to.
    pub not_entitled: Vec<Accelerator>,
    /// Accelerators Colab never answers a request for: `colab new` waits
    /// [`ALLOCATION_HANG`] and fails with `AllocationTimeout`. Cancelled during
    /// that wait, the request still gets an unnamed VM that provisioning must
    /// clean up.
    pub allocation_hangs: Vec<Accelerator>,
    /// GPU memory of successive A100 allocations (the last value repeats).
    pub a100_vram_sequence: Vec<u32>,
    /// The first N tunnel connections fail with HTTP 429 (`u32::MAX`: always).
    pub ssh_busy_attempts: u32,
    /// A plugin run fails at this stage id.
    pub plugin_fail_stage: Option<String>,
    /// Usage fetches fail (snapshots turn stale).
    pub usage_fetch_fails: bool,
    /// The CLI's credential slot holds a signed-in Google account that is not
    /// saved yet, so `accounts()` reports `unadopted` (and no active account)
    /// until `adopt_active_account`. Applied at construction and whenever the
    /// switch is turned on with [`MockBackend::set_failures`]; turning it off
    /// signs that credential out.
    pub unadopted_slot: bool,
    /// An add-account flow was interrupted (as after a crash): the credential
    /// slot is empty with a backup pending, and the next `accounts()` restores
    /// it and reports `recovered_pending_add` once. Applied at construction and
    /// whenever the switch is turned on.
    pub interrupted_add: bool,
    /// The VM watchdog cannot be started again: setting an auto-stop on a
    /// runtime whose watchdog is not running (after a cancel) leaves the stop
    /// app-side only (`WatchdogInfo::armed` is false). Watchdogs started by
    /// provisioning, and extending one that runs, are not affected.
    pub watchdog_restart_fails: bool,
    /// The update check finds a newer release (the next patch version).
    pub update_available: bool,
    /// The update check fails (as if GitHub could not be reached).
    pub update_check_fails: bool,
    /// This copy cannot replace itself: the folder holding it is not writable.
    pub update_unsupported: bool,
    /// The update's download fails halfway (as if the connection dropped).
    pub update_download_fails: bool,
    /// The downloaded update fails its checksum check, so it is not installed.
    pub update_unverified: bool,
    /// Putting the update in place fails; the old version stays.
    pub update_install_fails: bool,
    /// A completed update asks to be started again, as a real one does
    /// (`Installed::relaunch`); `relaunch` then only counts. Off, the update reports that
    /// nothing was installed, so the interactive demo keeps running.
    pub update_relaunch: bool,
    /// Starting the updated version fails.
    pub update_relaunch_fails: bool,
    /// `switch_impact` cannot list the unmanaged runtimes either.
    pub runtimes_listing_fails: bool,
    /// Accounts (by email) whose runtimes cannot be listed: listings leave their runtimes out
    /// and report each account in `RuntimesView::errors`. Leaving such an account affects
    /// `switch_impact` as `runtimes_listing_fails` does.
    pub unlisted_accounts: Vec<String>,
    /// Keep Plan sync fails (simulated upload/archive/copy failure).
    pub keep_sync_fails: bool,
    /// `begin_drive_mount` / `confirm` fails without mounting Drive.
    pub drive_mount_fails: bool,
    /// `restart_kernel` fails with `ConnectionLost`.
    pub restart_kernel_fails: bool,
}

impl Default for MockFailures {
    fn default() -> Self {
        MockFailures {
            cli_missing: false,
            install_fails: false,
            vm_listing_empty: false,
            keyring_unavailable: false,
            login: None,
            not_entitled: vec![],
            allocation_hangs: vec![],
            a100_vram_sequence: vec![40_960, 81_920],
            ssh_busy_attempts: 0,
            plugin_fail_stage: None,
            usage_fetch_fails: false,
            unadopted_slot: false,
            interrupted_add: false,
            watchdog_restart_fails: false,
            update_available: false,
            update_check_fails: false,
            update_unsupported: false,
            update_download_fails: false,
            update_unverified: false,
            update_install_fails: false,
            update_relaunch: false,
            update_relaunch_fails: false,
            runtimes_listing_fails: false,
            unlisted_accounts: vec![],
            keep_sync_fails: false,
            drive_mount_fails: false,
            restart_kernel_fails: false,
        }
    }
}

/// How long a simulated `colab new` waits for an answer that never comes
/// ([`MockFailures::allocation_hangs`]): four two-minute tries and the CLI's
/// backoff between them.
pub const ALLOCATION_HANG: Duration = Duration::from_secs(4 * 120 + 70);

#[derive(Debug, Clone, PartialEq)]
pub struct MockAccount {
    pub email: String,
    /// Starting balance in compute units.
    pub balance_cu: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MockRuntime {
    /// Index into [`MockScenario::accounts`].
    pub account: usize,
    /// Local name (`None` = created in the browser, unmanaged).
    pub name: Option<String>,
    pub accelerator: Accelerator,
    /// Tracked by the app (ledger entry with the default guard policy).
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MockScenario {
    /// Saved accounts; the first is active. Empty = signed out.
    pub accounts: Vec<MockAccount>,
    pub runtimes: Vec<MockRuntime>,
}

impl MockScenario {
    /// Two accounts; on the active one two managed sessions (A100 + T4) and a
    /// runtime opened in the browser; one runtime on the second account.
    pub fn demo() -> Self {
        MockScenario {
            accounts: vec![
                MockAccount {
                    email: "ada@example.com".into(),
                    balance_cu: 87.5,
                },
                MockAccount {
                    email: "lin@example.com".into(),
                    balance_cu: 240.0,
                },
            ],
            runtimes: vec![
                MockRuntime {
                    account: 0,
                    name: Some("a100-7f3a".into()),
                    accelerator: Accelerator::A100,
                    managed: true,
                },
                MockRuntime {
                    account: 0,
                    name: Some("t4-91c2".into()),
                    accelerator: Accelerator::T4,
                    managed: true,
                },
                MockRuntime {
                    account: 0,
                    name: None,
                    accelerator: Accelerator::L4,
                    managed: false,
                },
                MockRuntime {
                    account: 1,
                    name: Some("cpu-0b5e".into()),
                    accelerator: Accelerator::None,
                    managed: true,
                },
            ],
        }
    }

    pub fn single() -> Self {
        MockScenario {
            accounts: vec![MockAccount {
                email: "ada@example.com".into(),
                balance_cu: 100.0,
            }],
            runtimes: vec![],
        }
    }

    /// No account yet (first run).
    pub fn signed_out() -> Self {
        MockScenario {
            accounts: vec![],
            runtimes: vec![],
        }
    }
}

/// An active plugin run seeded on a mock runtime before startup (simulates restarting the app
/// while a run is still in progress on the VM).
#[derive(Debug, Clone, PartialEq)]
pub struct MockActiveRun {
    pub account: usize,
    pub session: String,
    pub run_id: String,
    pub plugin: String,
    pub stage: Option<String>,
    pub state: RunState,
    pub log_tail: Vec<String>,
    pub keep_alive: bool,
    pub endpoints: Vec<ResolvedEndpoint>,
    /// When true, `reattach_run` waits for [`MockBackend::advance_active_run`] between stages
    /// (and holds `keep_alive` until cancelled or advanced again), for deterministic UI tests.
    pub manual_stages: bool,
}

#[derive(Clone)]
pub struct MockConfig {
    pub clock: Arc<dyn Clock>,
    /// Wait per simulated step (on `clock`).
    pub step_delay: Duration,
    pub failures: MockFailures,
    pub scenario: MockScenario,
    pub defaults: Defaults,
    pub active_runs: Vec<MockActiveRun>,
}

impl MockConfig {
    /// Instant: manual clock (returned for advancing), 5 s of virtual time per step.
    pub fn headless(scenario: MockScenario) -> (MockConfig, ManualClock) {
        let clock = ManualClock::fixed();
        (
            MockConfig {
                clock: Arc::new(clock.clone()),
                step_delay: Duration::from_secs(5),
                failures: MockFailures::default(),
                scenario,
                defaults: Defaults::default(),
                active_runs: Vec::new(),
            },
            clock,
        )
    }

    /// Human-paced: system clock, `step_delay` per step.
    pub fn interactive(step_delay: Duration) -> MockConfig {
        MockConfig {
            clock: Arc::new(SystemClock),
            step_delay,
            failures: MockFailures::default(),
            scenario: MockScenario::demo(),
            defaults: Defaults::default(),
            active_runs: Vec::new(),
        }
    }

    pub fn with_active_run(mut self, run: MockActiveRun) -> Self {
        self.active_runs.push(run);
        self
    }
}

/// Simulated hourly rate of an accelerator (CU/h).
pub fn mock_rate(a: Accelerator) -> f64 {
    match a {
        Accelerator::None => 0.08,
        Accelerator::T4 => 1.76,
        Accelerator::L4 => 4.82,
        Accelerator::G4 => 8.2,
        Accelerator::A100 => 11.77,
        Accelerator::H100 => 18.3,
        Accelerator::TpuV5e1 => 3.9,
        Accelerator::TpuV6e1 => 6.4,
    }
}

fn mock_gpu(a: Accelerator, vram: Option<u32>) -> Option<String> {
    match a {
        Accelerator::T4 => Some("Tesla T4".into()),
        Accelerator::L4 => Some("NVIDIA L4".into()),
        Accelerator::G4 => Some("NVIDIA RTX PRO 6000 Blackwell Server Edition".into()),
        Accelerator::A100 => Some(
            if vram.unwrap_or(0) > 50_000 {
                "NVIDIA A100-SXM4-80GB"
            } else {
                "NVIDIA A100-SXM4-40GB"
            }
            .into(),
        ),
        Accelerator::H100 => Some("NVIDIA H100 80GB HBM3".into()),
        _ => None,
    }
}

fn mock_vram(a: Accelerator) -> Option<u32> {
    match a {
        Accelerator::T4 => Some(15_360),
        Accelerator::L4 => Some(23_034),
        Accelerator::G4 => Some(97_887),
        Accelerator::H100 => Some(81_559),
        Accelerator::A100 => Some(40_960),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct MAccount {
    id: AccountId,
    email: String,
    created_at: SystemTime,
    balance: f64,
}

#[derive(Debug, Clone)]
struct MRuntime {
    account: AccountId,
    endpoint: String,
    name: Option<String>,
    accelerator: Accelerator,
    high_mem: bool,
    vram_mib: Option<u32>,
    /// Its VM watchdog is running (a cancelled one exits).
    watchdog_alive: bool,
    vm_files: BTreeMap<String, Vec<u8>>,
    drive_mounts: BTreeSet<String>,
    uploads: Vec<String>,
}

impl MRuntime {
    fn is(&self, account: &AccountId, name: &str) -> bool {
        &self.account == account && self.name.as_deref() == Some(name)
    }

    fn hardware(&self) -> String {
        if self.accelerator == Accelerator::None {
            "CPU".into()
        } else {
            self.accelerator.as_str().into()
        }
    }
}

#[derive(Clone)]
struct MActiveRun {
    account: AccountId,
    session: String,
    meta: RunMeta,
    status: Option<RunStatus>,
    log_tail: Vec<String>,
    step_rx: Option<Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>>,
}

struct World {
    cli_installed: bool,
    accounts: Vec<MAccount>,
    active: Option<AccountId>,
    runtimes: Vec<MRuntime>,
    balance_at: SystemTime,
    ledger: Ledger,
    usage: BTreeMap<AccountId, AccountUsage>,
    tunnels: BTreeMap<TunnelId, TunnelInfo>,
    tunnel_events: BTreeMap<TunnelId, Events<TunnelEvent>>,
    next_tunnel: u64,
    ssh_busy_left: u32,
    plugins: PluginStore,
    active_runs: Vec<MActiveRun>,
    run_steps: BTreeMap<String, std::sync::mpsc::Sender<()>>,
    secrets: BTreeMap<String, String>,
    /// Env pairs of the latest plugin launch (file inputs are VM paths).
    last_plugin_env: Vec<(String, String)>,
    a100_attempt: usize,
    defaults: Defaults,
    reserved: u32,
    next_id: u32,
    failures: MockFailures,
    /// The slot holds this not-yet-saved credential (email).
    unadopted_email: Option<String>,
    /// An add-account backup is pending: the account that was in the slot.
    pending_add: Option<Option<AccountId>>,
    login_live: bool,
    update_checks: u32,
    runtime_listings: u32,
    /// The files that were downloaded, as `remote -> local`.
    downloads: Vec<String>,
    update_installs: u32,
    update_relaunches: u32,
    opened_urls: Vec<String>,
    restarted_kernels: Vec<String>,
    ssh_resets: Vec<String>,
}

/// Email of the credential `MockFailures::unadopted_slot` puts in the slot.
pub const MOCK_UNADOPTED_EMAIL: &str = "pat@example.com";

impl World {
    /// Applies switches that model state (rather than being read on use)
    /// when they change from `old`.
    fn apply_switches(&mut self, old: &MockFailures) {
        let new = self.failures.clone();
        if new.cli_missing != old.cli_missing {
            self.cli_installed = !new.cli_missing;
        }
        self.ssh_busy_left = new.ssh_busy_attempts;
        if new.a100_vram_sequence != old.a100_vram_sequence {
            self.a100_attempt = 0;
        }
        if new.unadopted_slot && !old.unadopted_slot {
            self.active = None;
            self.unadopted_email = Some(MOCK_UNADOPTED_EMAIL.to_owned());
        } else if !new.unadopted_slot && old.unadopted_slot {
            self.unadopted_email = None;
        }
        if new.interrupted_add && !old.interrupted_add && self.pending_add.is_none() {
            let previous = self.active.take();
            self.pending_add = Some(previous);
            self.tunnels.clear();
        }
    }

    /// Restores the account backed up by a pending add (no-op without one).
    fn rollback_add(&mut self) -> bool {
        match self.pending_add.take() {
            Some(previous) => {
                self.active = previous;
                true
            }
            None => false,
        }
    }

    fn adopt_unadopted(&mut self, now: SystemTime) -> Option<AccountId> {
        let email = self.unadopted_email.take()?;
        let id = match self.accounts.iter().find(|a| a.email == email) {
            Some(a) => a.id.clone(),
            None => {
                let id = self.fresh_account_id();
                self.accounts.push(MAccount {
                    id: id.clone(),
                    email,
                    created_at: now,
                    balance: 100.0,
                });
                id
            }
        };
        self.active = Some(id.clone());
        Some(id)
    }

    fn fresh_account_id(&self) -> AccountId {
        let mut n = self.accounts.len() + 1;
        loop {
            let id = AccountId::new(format!("mock{n}")).unwrap_or_else(|_| AccountId::generate());
            if !self.accounts.iter().any(|a| a.id == id) {
                return id;
            }
            n += 1;
        }
    }

    fn taken_names(&self) -> Vec<String> {
        let mut taken: Vec<String> = self
            .runtimes
            .iter()
            .filter_map(|r| r.name.clone())
            .collect();
        taken.extend(self.ledger.names());
        taken
    }

    fn runtime(&self, account: &AccountId, name: &str) -> Option<&MRuntime> {
        self.runtimes.iter().find(|r| r.is(account, name))
    }

    fn runtime_mut(&mut self, account: &AccountId, name: &str) -> Option<&mut MRuntime> {
        self.runtimes.iter_mut().find(|r| r.is(account, name))
    }

    fn unmanaged_on(&self, account: &AccountId) -> Vec<RuntimeRef> {
        self.runtimes
            .iter()
            .filter(|r| &r.account == account)
            .filter(|r| {
                r.name
                    .as_ref()
                    .is_none_or(|n| self.ledger.live(account, n).is_none())
            })
            .map(|r| RuntimeRef {
                account: r.account.clone(),
                endpoint: r.endpoint.clone(),
                name: r.name.clone(),
            })
            .collect()
    }

    fn listing_fails_for(&self, account: &AccountId) -> bool {
        self.failures.runtimes_listing_fails
            || self
                .email(account)
                .is_some_and(|email| self.failures.unlisted_accounts.contains(&email))
    }

    /// Burns balances up to `now`.
    fn settle(&mut self, now: SystemTime) {
        let hours = now
            .duration_since(self.balance_at)
            .unwrap_or_default()
            .as_secs_f64()
            / 3600.0;
        if hours > 0.0 {
            for a in &mut self.accounts {
                let rate: f64 = self
                    .runtimes
                    .iter()
                    .filter(|r| r.account == a.id)
                    .map(|r| mock_rate(r.accelerator))
                    .sum();
                a.balance = (a.balance - rate * hours).max(0.0);
            }
            self.balance_at = now;
        }
    }

    fn usage_of(&self, id: &AccountId) -> Option<Usage> {
        let a = self.accounts.iter().find(|a| &a.id == id)?;
        let rts: Vec<&MRuntime> = self.runtimes.iter().filter(|r| &r.account == id).collect();
        Some(Usage {
            balance_cu: a.balance,
            rate_cu_per_hour: rts.iter().map(|r| mock_rate(r.accelerator)).sum(),
            assignments: rts.len() as u32,
        })
    }

    fn live_names(&self, id: &AccountId) -> BTreeSet<String> {
        self.ledger.live_of(id).map(|e| e.name.clone()).collect()
    }

    fn endpoint(&mut self) -> String {
        self.next_id += 1;
        format!("m-mock-{:04x}", self.next_id * 7919 % 65536)
    }

    fn email(&self, id: &AccountId) -> Option<String> {
        self.accounts
            .iter()
            .find(|a| &a.id == id)
            .map(|a| a.email.clone())
    }

    fn account_views(&self) -> Vec<Account> {
        self.accounts
            .iter()
            .map(|a| Account {
                id: a.id.clone(),
                email: Some(a.email.clone()),
                created_at: a.created_at,
                updated_at: a.created_at,
                usage: self.usage.get(&a.id).and_then(|u| u.snapshot),
            })
            .collect()
    }

    fn reconcile(&mut self, id: &AccountId, now: SystemTime) -> (bool, Vec<RuntimeRef>) {
        if self.listing_fails_for(id) {
            return (false, Vec::new());
        }
        let runtimes = || self.runtimes.iter().filter(|r| &r.account == id);
        let listed: Vec<&str> = runtimes().map(|r| r.endpoint.as_str()).collect();
        let names: Vec<(&str, &str)> = runtimes()
            .filter_map(|r| Some((r.endpoint.as_str(), r.name.as_deref()?)))
            .collect();
        let outcome = self.ledger.reconcile(id, |_| true, &listed, &names, now);
        let vanished = outcome
            .vanished
            .into_iter()
            .map(|(name, endpoint)| RuntimeRef {
                account: id.clone(),
                endpoint,
                name: Some(name),
            })
            .collect();
        (!outcome.unsettled, vanished)
    }

    fn record_usage(&mut self, id: &AccountId, now: SystemTime, verified: bool) {
        let live = self.live_names(id);
        let fails = self.failures.usage_fetch_fails;
        let reading = self.usage_of(id);
        let entry = self.usage.entry(id.clone()).or_default();
        match (fails, reading) {
            (false, Some(u)) => {
                entry.record_success(u, now, &live);
                entry.ownership_verified = live.is_empty() || verified;
                if verified && let Some(name) = u.sole_session(&live) {
                    let reading = RateReading {
                        cu_per_hour: u.rate_cu_per_hour,
                        at: now,
                    };
                    let _ = self
                        .ledger
                        .update(id, name, |e| e.measured_rate = Some(reading));
                }
            }
            _ => entry.record_failure(
                Error::new(ErrorKind::Http).detail("simulated usage failure"),
                now,
            ),
        }
    }

    fn usage_views(&self) -> Vec<AccountUsageView> {
        self.accounts
            .iter()
            .map(|a| {
                let u = self.usage.get(&a.id).cloned().unwrap_or_default();
                let known = self.ledger.live_of(&a.id).count();
                usage_view(&a.id, Some(a.email.clone()), &u, known)
            })
            .collect()
    }

    fn set_tunnel(
        &mut self,
        id: TunnelId,
        state: TunnelState,
        events: Option<&Events<TunnelEvent>>,
    ) {
        if let Some(t) = self.tunnels.get_mut(&id) {
            t.state = state.clone();
        }
        if let Some(tx) = events {
            let _ = tx.send(TunnelEvent::State { id, state });
        }
    }

    fn close_session_tunnels(&mut self, account: &AccountId, name: &str) {
        let ids: Vec<TunnelId> = self
            .tunnels
            .values()
            .filter(|t| &t.spec.account == account && t.spec.session == name)
            .map(|t| t.id)
            .collect();
        for id in ids {
            self.tunnels.remove(&id);
            self.tunnel_events.remove(&id);
        }
    }
}

/// How many steps a simulated server runs before it is over.
const LIVE_RUN_STEPS: u32 = 100;
/// How long a simulated server waits between two steps.
const LIVE_STEP_MILLIS: u64 = 5;

#[derive(Clone)]
pub struct MockBackend {
    cfg: Arc<MockConfig>,
    world: Arc<Mutex<World>>,
}

impl std::fmt::Debug for MockBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockBackend").finish_non_exhaustive()
    }
}

impl MockBackend {
    pub fn new(cfg: MockConfig) -> MockBackend {
        let now = cfg.clock.now();
        let accounts: Vec<MAccount> = cfg
            .scenario
            .accounts
            .iter()
            .enumerate()
            .map(|(i, a)| MAccount {
                id: AccountId::new(format!("mock{}", i + 1))
                    .unwrap_or_else(|_| AccountId::generate()),
                email: a.email.clone(),
                created_at: now - Duration::from_secs(86_400 * (30 + i as u64)),
                balance: a.balance_cu,
            })
            .collect();
        let mut world = World {
            cli_installed: !cfg.failures.cli_missing,
            active: accounts.first().map(|a| a.id.clone()),
            accounts,
            runtimes: vec![],
            balance_at: now,
            ledger: Ledger::in_memory(),
            usage: BTreeMap::new(),
            tunnels: BTreeMap::new(),
            tunnel_events: BTreeMap::new(),
            next_tunnel: 0,
            ssh_busy_left: cfg.failures.ssh_busy_attempts,
            plugins: PluginStore::load(None, None).expect("bundled plugins are valid"),
            active_runs: Vec::new(),
            run_steps: BTreeMap::new(),
            secrets: BTreeMap::new(),
            last_plugin_env: Vec::new(),
            a100_attempt: 0,
            defaults: cfg.defaults.clone(),
            reserved: 0,
            next_id: 0,
            failures: cfg.failures.clone(),
            unadopted_email: None,
            pending_add: None,
            login_live: false,
            update_checks: 0,
            runtime_listings: 0,
            downloads: Vec::new(),
            update_installs: 0,
            update_relaunches: 0,
            opened_urls: Vec::new(),
            restarted_kernels: Vec::new(),
            ssh_resets: Vec::new(),
        };
        let off = MockFailures {
            cli_missing: cfg.failures.cli_missing,
            ..MockFailures::default()
        };
        world.apply_switches(&off);
        for (i, r) in cfg.scenario.runtimes.iter().enumerate() {
            let Some(account) = world.accounts.get(r.account).map(|a| a.id.clone()) else {
                continue;
            };
            let endpoint = world.endpoint();
            let started = now - Duration::from_secs(600 * (i as u64 + 1));
            let policy = world.defaults.guard_policy();
            let watchdog = match (r.managed, &r.name) {
                (true, Some(_)) => policy.max_runtime.map(|d| WatchdogInfo {
                    deadline: Some(started + d),
                    idle_timeout: policy.idle_timeout,
                    cancelled: false,
                    armed: true,
                }),
                _ => None,
            };
            world.runtimes.push(MRuntime {
                account: account.clone(),
                endpoint: endpoint.clone(),
                name: r.name.clone(),
                accelerator: r.accelerator,
                high_mem: false,
                vram_mib: mock_vram(r.accelerator),
                watchdog_alive: watchdog.is_some(),
                vm_files: BTreeMap::new(),
                drive_mounts: BTreeSet::new(),
                uploads: Vec::new(),
            });
            if let (true, Some(name)) = (r.managed, &r.name) {
                let _ = world.ledger.upsert(TrackedSession {
                    name: name.clone(),
                    account: account.clone(),
                    endpoint: Some(endpoint),
                    accelerator: r.accelerator,
                    high_mem: false,
                    gpu: mock_gpu(r.accelerator, mock_vram(r.accelerator)),
                    vram_mib: mock_vram(r.accelerator),
                    watchdog,
                    policy,
                    spend: SpendMeter::new(started),
                    stopped: None,
                    measured_rate: None,
                    keep_plan: None,
                    last_keep_sync: None,
                });
            }
        }
        let backend = MockBackend {
            cfg: Arc::new(cfg.clone()),
            world: Arc::new(Mutex::new(world)),
        };
        for run in &cfg.active_runs {
            backend.seed_active_run(run.clone());
        }
        backend
    }

    /// How many update checks were made (the simulation never contacts GitHub).
    pub fn update_checks(&self) -> u32 {
        self.w().update_checks
    }

    /// How many times the runtimes were listed for the app (`Backend::runtimes`).
    pub fn runtime_listings(&self) -> u32 {
        self.w().runtime_listings
    }

    /// How many simulated updates completed (nothing is ever downloaded or replaced).
    pub fn update_installs(&self) -> u32 {
        self.w().update_installs
    }

    /// How many times the app asked to start an updated version (nothing is started).
    pub fn update_relaunches(&self) -> u32 {
        self.w().update_relaunches
    }

    pub fn opened_urls(&self) -> Vec<String> {
        self.w().opened_urls.clone()
    }

    pub fn restarted_kernels(&self) -> Vec<String> {
        self.w().restarted_kernels.clone()
    }

    pub fn ssh_resets(&self) -> Vec<String> {
        self.w().ssh_resets.clone()
    }

    /// Replaces the failure switches at runtime (UI tests flip them mid-test).
    /// Every switch takes effect immediately: switches read on use apply to
    /// the next call; state switches (`cli_missing`, `unadopted_slot`,
    /// `interrupted_add`, `ssh_busy_attempts`, `a100_vram_sequence`) are
    /// applied when they change.
    pub fn set_failures(&self, failures: MockFailures) {
        let mut w = self.w();
        let old = std::mem::replace(&mut w.failures, failures);
        w.apply_switches(&old);
    }

    /// Adds an unmanaged runtime (as if opened in the browser) to `account`.
    pub fn add_unmanaged_runtime(&self, account: &AccountId, accelerator: Accelerator) -> String {
        let now = self.cfg.clock.now();
        let mut w = self.w();
        w.settle(now);
        let endpoint = w.endpoint();
        w.runtimes.push(MRuntime {
            account: account.clone(),
            endpoint: endpoint.clone(),
            name: None,
            accelerator,
            high_mem: false,
            vram_mib: mock_vram(accelerator),
            watchdog_alive: false,
            vm_files: BTreeMap::new(),
            drive_mounts: BTreeSet::new(),
            uploads: Vec::new(),
        });
        endpoint
    }

    /// Seeds an active plugin run on an existing mock runtime (simulates an app restart mid-run).
    pub fn seed_active_run(&self, run: MockActiveRun) {
        let mut w = self.w();
        let Some(account) = w.accounts.get(run.account).map(|a| a.id.clone()) else {
            return;
        };
        let status = Some(RunStatus {
            state: run.state,
            stage: run.stage.clone(),
            progress: Some(if run.state == RunState::Ready {
                1.0
            } else {
                0.5
            }),
            message: Some(match run.state {
                RunState::Ready => "ready (simulated)".to_owned(),
                _ => format!("simulated {}", run.stage.as_deref().unwrap_or("starting")),
            }),
            outputs: Default::default(),
            metrics: Default::default(),
        });
        let step_rx = if run.manual_stages {
            let (tx, rx) = std::sync::mpsc::channel();
            w.run_steps.insert(run.run_id.clone(), tx);
            Some(Arc::new(Mutex::new(Some(rx))))
        } else {
            None
        };
        w.active_runs.push(MActiveRun {
            account,
            session: run.session,
            meta: RunMeta {
                run_id: run.run_id,
                plugin: run.plugin,
                inputs: BTreeMap::new(),
                endpoints: run.endpoints,
                keep_alive: run.keep_alive,
                provisions: false,
            },
            status,
            log_tail: run.log_tail,
            step_rx,
        });
    }

    /// Advances a seeded run created with `manual_stages: true` by one step (next stage,
    /// `ready`, or exits a `keep_alive` server).
    pub fn advance_active_run(&self, run_id: &str) {
        if let Some(tx) = self.w().run_steps.get(run_id) {
            let _ = tx.send(());
        }
    }

    /// Pushes a simulated tunnel state or health change to the app's open tunnel stream.
    pub fn emit_tunnel_event(&self, event: TunnelEvent) {
        let mut w = self.w();
        match &event {
            TunnelEvent::State { id, state } => {
                if let Some(t) = w.tunnels.get_mut(id) {
                    t.state = state.clone();
                }
            }
            TunnelEvent::Health { id, healthy } => {
                if let Some(t) = w.tunnels.get_mut(id) {
                    t.healthy = Some(*healthy);
                }
            }
        }
        let id = match &event {
            TunnelEvent::State { id, .. } | TunnelEvent::Health { id, .. } => *id,
        };
        if let Some(tx) = w.tunnel_events.get(&id) {
            let _ = tx.send(event);
        }
    }

    fn w(&self) -> MutexGuard<'_, World> {
        self.world.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now_(&self) -> SystemTime {
        self.cfg.clock.now()
    }

    fn step(&self, cancel: &CancelToken) -> Result<()> {
        self.cfg.clock.sleep(self.cfg.step_delay, cancel)
    }

    fn ready(&self) -> Result<()> {
        let w = self.w();
        if !w.cli_installed {
            return Err(Error::new(ErrorKind::NotInstalled).with("program", "colab"));
        }
        if w.failures.keyring_unavailable {
            return Err(Error::new(ErrorKind::KeyringUnavailable));
        }
        Ok(())
    }

    fn require_active(&self, account: &AccountId) -> Result<()> {
        self.ready()?;
        if self.w().active.as_ref() != Some(account) {
            return Err(Error::new(ErrorKind::AccountNotActive).with("account", account));
        }
        Ok(())
    }

    fn active(&self) -> Result<AccountId> {
        self.ready()?;
        self.w()
            .active
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::NoActiveAccount))
    }

    fn view(&self) -> AccountsView {
        let w = self.w();
        AccountsView {
            accounts: w.account_views(),
            active: w.active.clone(),
            has_token: w.active.is_some() || w.unadopted_email.is_some(),
            unadopted: w.unadopted_email.is_some(),
            active_email: w
                .active
                .as_ref()
                .and_then(|a| w.email(a))
                .or_else(|| w.unadopted_email.clone()),
            recovered_pending_add: false,
        }
    }

    fn stop_inner(&self, target: &RuntimeRef, reason: StopReason) -> Result<()> {
        // Like the real backend: without an endpoint it cannot tell that the runtime is gone.
        if target.endpoint.is_empty() {
            return Err(
                Error::new(ErrorKind::OrphanSuspected).detail("the session's runtime is not known")
            );
        }
        let now = self.now_();
        let mut w = self.w();
        w.settle(now);
        let before = w.runtimes.len();
        w.runtimes
            .retain(|r| !(r.account == target.account && r.endpoint == target.endpoint));
        if w.runtimes.len() == before {
            return Err(Error::new(ErrorKind::SessionGone).with("endpoint", &target.endpoint));
        }
        let name = target.name.clone().or_else(|| {
            w.ledger
                .live_by_endpoint(&target.account, &target.endpoint)
                .map(|e| e.name.clone())
        });
        if let Some(n) = name {
            w.close_session_tunnels(&target.account, &n);
            let _ = w.ledger.mark_stopped(&target.account, &n, now, reason);
        }
        Ok(())
    }

    /// Keep Plan and teardown after a plugin script stops.
    /// A stop uses a fresh cancel token: the user's cancel must not skip giving the runtime back.
    fn apply_after(
        &self,
        action: AfterRun,
        target: &RuntimeRef,
        cancel: &CancelToken,
    ) -> Result<()> {
        match action {
            AfterRun::Leave => Ok(()),
            AfterRun::SyncKeep => {
                let has_plan = target.name.as_ref().is_some_and(|name| {
                    self.w()
                        .ledger
                        .live(&target.account, name)
                        .and_then(|e| e.keep_plan.as_ref())
                        .is_some_and(|plan| plan.has_enabled_target())
                });
                if !has_plan {
                    return Ok(());
                }
                self.sync_keep(target, None, cancel).map(|_| ())
            }
            AfterRun::Stop { save_keep } => self.stop_session(
                target,
                StopOptions {
                    reason: StopReason::Provision,
                    save_keep,
                    force_on_keep_error: false,
                    keep_timeout: None,
                },
                &CancelToken::new(),
            ),
        }
    }
}

struct MockOps<'a> {
    m: &'a MockBackend,
    account: AccountId,
    /// Watchdog starts are restarts (`MockFailures::watchdog_restart_fails` applies).
    restart: bool,
}

impl ProvisionOps for MockOps<'_> {
    fn sessions(&self, _: &CancelToken) -> Result<Vec<ServerSession>> {
        let w = self.m.w();
        Ok(w.runtimes
            .iter()
            .filter(|r| r.account == self.account)
            .map(|r| ServerSession {
                name: r.name.clone(),
                endpoint: r.endpoint.clone(),
                hardware: r.hardware(),
                accelerator: Some(r.accelerator),
                shape: if r.high_mem {
                    "High-RAM".into()
                } else {
                    "Standard".into()
                },
                variant: if r.accelerator.is_gpu() {
                    "GPU"
                } else if r.accelerator.is_tpu() {
                    "TPU"
                } else {
                    "DEFAULT"
                }
                .into(),
                status: None,
            })
            .collect())
    }

    fn create(
        &self,
        name: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<()> {
        let hangs = self.m.w().failures.allocation_hangs.contains(&accelerator);
        if hangs {
            if let Err(e) = self.m.cfg.clock.sleep(ALLOCATION_HANG, cancel) {
                // Killed mid-request, which Colab still answered with a VM.
                self.m.add_unmanaged_runtime(&self.account, accelerator);
                return Err(e);
            }
            return Err(Error::new(ErrorKind::AllocationTimeout).detail(
                "ReadTimeout: HTTPSConnectionPool(host='colab.research.google.com', port=443): Read timed out. (read timeout=120)",
            ));
        }
        self.m.step(cancel)?;
        let now = self.m.now_();
        let mut w = self.m.w();
        if w.failures.not_entitled.contains(&accelerator) {
            return Err(
                Error::new(ErrorKind::NotEntitled).with("accelerator", accelerator.as_str())
            );
        }
        w.settle(now);
        let vram = if accelerator == Accelerator::A100 {
            let seq = w.failures.a100_vram_sequence.clone();
            let v = seq
                .get(w.a100_attempt)
                .or(seq.last())
                .copied()
                .unwrap_or(40_960);
            w.a100_attempt += 1;
            Some(v)
        } else {
            mock_vram(accelerator)
        };
        let endpoint = w.endpoint();
        w.runtimes.push(MRuntime {
            account: self.account.clone(),
            endpoint,
            name: Some(name.to_owned()),
            accelerator,
            high_mem,
            vram_mib: vram,
            watchdog_alive: false,
            vm_files: BTreeMap::new(),
            drive_mounts: BTreeSet::new(),
            uploads: Vec::new(),
        });
        Ok(())
    }

    fn probe(&self, name: &str, cancel: &CancelToken) -> Result<Hardware> {
        self.m.step(cancel)?;
        let w = self.m.w();
        let r = w
            .runtime(&self.account, name)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        Ok(Hardware {
            gpu: mock_gpu(r.accelerator, r.vram_mib),
            vram_mib: r.vram_mib,
        })
    }

    fn stop(&self, name: &str, _: &CancelToken) -> Result<StopOutcome> {
        let now = self.m.now_();
        let mut w = self.m.w();
        w.settle(now);
        let before = w.runtimes.len();
        w.runtimes.retain(|r| !r.is(&self.account, name));
        Ok(if w.runtimes.len() < before {
            StopOutcome::Terminated
        } else {
            StopOutcome::NotFound
        })
    }

    fn reap(&self, endpoint: &str, _: &CancelToken) -> Result<bool> {
        let mut w = self.m.w();
        let before = w.runtimes.len();
        w.runtimes
            .retain(|r| !(r.account == self.account && r.endpoint == endpoint));
        Ok(w.runtimes.len() < before)
    }

    fn start_watchdog(
        &self,
        name: &str,
        policy: &GuardPolicy,
        cancel: &CancelToken,
    ) -> Result<Option<WatchdogInfo>> {
        self.m.step(cancel)?;
        let now = self.m.now_();
        let mut w = self.m.w();
        if self.restart && w.failures.watchdog_restart_fails {
            return Err(Error::new(ErrorKind::Other).detail("the watchdog process is not running"));
        }
        let rt = w
            .runtime_mut(&self.account, name)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        rt.watchdog_alive = true;
        Ok(Some(WatchdogInfo {
            deadline: policy.max_runtime.map(|d| now + d),
            idle_timeout: policy.idle_timeout,
            cancelled: false,
            armed: true,
        }))
    }
}

struct MockLoginFlow {
    m: MockBackend,
    url: String,
    finished: bool,
}

impl Drop for MockLoginFlow {
    /// Like the real flow: anything but success restores the previous account.
    fn drop(&mut self) {
        if !self.finished {
            let mut w = self.m.w();
            w.rollback_add();
            w.login_live = false;
        }
    }
}

impl LoginFlow for MockLoginFlow {
    fn auth_url(&self) -> &str {
        &self.url
    }

    fn submit_code(mut self: Box<Self>, code: &str, cancel: &CancelToken) -> Result<Account> {
        if code.trim().is_empty() {
            return Err(Error::invalid("code", "paste the code exactly as shown"));
        }
        self.m.step(cancel)?;
        match self.m.w().failures.login {
            Some(MockLoginFailure::InvalidCode) => {
                return Err(Error::new(ErrorKind::LoginInvalidCode));
            }
            Some(MockLoginFailure::VerificationFailed) => {
                return Err(Error::new(ErrorKind::LoginVerificationFailed));
            }
            _ => {}
        }
        let now = self.m.now_();
        let mut w = self.m.w();
        let id = w.fresh_account_id();
        let email = format!("user{}@example.com", w.accounts.len() + 1);
        w.accounts.push(MAccount {
            id: id.clone(),
            email: email.clone(),
            created_at: now,
            balance: 100.0,
        });
        w.active = Some(id.clone());
        w.pending_add = None;
        w.login_live = false;
        w.tunnels.clear();
        drop(w);
        self.finished = true;
        Ok(Account {
            id,
            email: Some(email),
            created_at: now,
            updated_at: now,
            usage: None,
        })
    }

    fn cancel(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

struct MockDriveMountFlow {
    m: MockBackend,
    target: RuntimeRef,
    mount_path: String,
    url: String,
}

impl DriveMountFlow for MockDriveMountFlow {
    fn auth_url(&self) -> &str {
        &self.url
    }

    fn confirm(self: Box<Self>, cancel: &CancelToken) -> Result<()> {
        self.m.step(cancel)?;
        self.m.finish_drive_mount(&self.target, &self.mount_path)
    }

    fn cancel(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

fn catalog() -> Vec<(&'static str, f64)> {
    // (repo, billions of parameters) for synthetic file sizes
    vec![
        ("unsloth/Qwen3-0.6B-GGUF", 0.6),
        ("unsloth/Qwen3-8B-GGUF", 8.2),
        ("bartowski/ScarletKc_ScarletKc-2.0-GGUF", 27.0),
        ("unsloth/gpt-oss-20b-GGUF", 20.9),
        ("bartowski/Llama-3.3-70B-Instruct-GGUF", 70.6),
    ]
}

fn mock_listing(repo: &str) -> GgufListing {
    let params = catalog()
        .into_iter()
        .find(|(r, _)| *r == repo)
        .map(|(_, p)| p)
        .unwrap_or(7.0);
    let name = repo
        .rsplit('/')
        .next()
        .unwrap_or("model")
        .trim_end_matches("-GGUF");
    let bits = [
        ("BF16", 16.0),
        ("Q8_0", 8.5),
        ("Q6_K", 6.56),
        ("Q5_K_M", 5.69),
        ("Q4_K_M", 4.85),
        ("IQ4_XS", 4.25),
        ("Q3_K_M", 3.91),
    ];
    let files: Vec<GgufFile> = bits
        .iter()
        .map(|(q, b)| GgufFile {
            path: format!("{name}-{q}.gguf"),
            size: (params * 1e9 * b / 8.0) as u64,
        })
        .collect();
    group(&files)
}

impl MockBackend {
    fn move_auto_stop(
        &self,
        target: &RuntimeRef,
        runtime_from_time_left: impl FnOnce(Duration) -> Duration,
        cancel: &CancelToken,
    ) -> Result<Extension> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        self.step(cancel)?;
        let gone = || Error::new(ErrorKind::SessionGone).with("session", &name);
        // Like the real backend: the runtime's own state decides between
        // moving the deadline of a running watchdog and starting it again.
        let (alive, tracked, vm_idle, stop_at) = {
            let w = self.w();
            let rt = w.runtime(&target.account, &name).ok_or_else(gone)?;
            let e = w.ledger.live(&target.account, &name).ok_or_else(gone)?;
            (
                rt.watchdog_alive,
                e.policy.clone(),
                e.watchdog.as_ref().and_then(|wd| wd.idle_timeout),
                e.auto_stop_at(),
            )
        };
        let time_left = stop_at
            .and_then(|at| at.duration_since(self.now_()).ok())
            .unwrap_or_default();
        let runtime = runtime_from_time_left(time_left);
        let info = if alive {
            WatchdogInfo {
                deadline: Some(self.now_() + runtime),
                idle_timeout: vm_idle,
                cancelled: false,
                armed: true,
            }
        } else {
            let policy = crate::watchdog::restart_policy(Some(&tracked), vm_idle, runtime);
            let ops = MockOps {
                m: self,
                account: target.account.clone(),
                restart: true,
            };
            match provision::start_watchdog(&ops, &name, &policy) {
                Ok(Some(info)) if info.armed => info,
                _ => crate::watchdog::start_failed(&policy),
            }
        };
        let now = self.now_();
        let found = self.w().ledger.update(&target.account, &name, |e| {
            let elapsed = now.duration_since(e.spend.started_at).unwrap_or_default();
            e.policy.max_runtime = Some(elapsed + runtime);
            e.watchdog = Some(info.clone());
        })?;
        if !found {
            return Err(gone());
        }
        Ok(Extension {
            runtime,
            watchdog: info,
        })
    }

    fn finish_drive_mount(&self, target: &RuntimeRef, mount_path: &str) -> Result<()> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        let mut w = self.w();
        if w.failures.drive_mount_fails {
            return Err(
                Error::new(ErrorKind::Other).detail("Drive credentials could not be propagated")
            );
        }
        let rt = w
            .runtime_mut(&target.account, &name)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone).with("session", &name))?;
        rt.drive_mounts.insert(mount_path.to_owned());
        let _ = w.ledger.update(&target.account, &name, |e| {
            let mut plan = e.keep_plan.clone().unwrap_or_default();
            let mut drive = plan.drive.unwrap_or_default();
            drive.mount_path = mount_path.to_owned();
            drive.consented = true;
            drive.mounted = true;
            plan.drive = Some(drive);
            e.keep_plan = Some(plan);
        });
        Ok(())
    }
}

impl Backend for MockBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Mock
    }

    fn defaults(&self) -> Defaults {
        self.w().defaults.clone()
    }

    fn set_defaults(&self, defaults: Defaults) {
        self.w().defaults = defaults;
    }

    fn now(&self) -> SystemTime {
        self.now_()
    }

    fn probe(&self, cancel: &CancelToken) -> Result<ProbeReport> {
        self.step(cancel)?;
        let w = self.w();
        let colab = w
            .cli_installed
            .then(|| "/home/demo/.local/bin/colab".to_owned());
        let wsl = matches!(Host::detect(), Host::Wsl { .. });
        Ok(ProbeReport {
            wsl_installed: wsl,
            wsl_distros: if wsl { vec!["Ubuntu".into()] } else { vec![] },
            bash: Some("/usr/bin/bash".into()),
            ssh: Some("/usr/bin/ssh".into()),
            ssh_keygen: Some("/usr/bin/ssh-keygen".into()),
            git: Some("/usr/bin/git".into()),
            uv: Some("/home/demo/.local/bin/uv".into()),
            python3: Some("/usr/bin/python3".into()),
            colab_version: colab.as_ref().map(|_| "0.8.0".to_owned()),
            helper_python: colab
                .as_ref()
                .map(|_| "/home/demo/.local/share/uv/tools/google-colab-cli/bin/python".to_owned()),
            keyring_ok: colab.is_some() && !w.failures.keyring_unavailable,
            ssh_key_present: true,
            home: Some("/home/demo".into()),
            colab,
            errors: vec![],
        })
    }

    fn install_cli(
        &self,
        method: InstallMethod,
        log: Events<Line>,
        cancel: &CancelToken,
    ) -> Result<()> {
        let lines = [
            format!(
                "$ {}",
                match method {
                    InstallMethod::Uv => "uv tool install google-colab-cli",
                    InstallMethod::InstallUvThenCli =>
                        "curl -LsSf https://astral.sh/uv/install.sh | sh",
                    InstallMethod::PipUser => "python3 -m pip install --user google-colab-cli",
                }
            ),
            "Resolved 42 packages in 1.2s".into(),
            "Prepared 42 packages in 3.4s".into(),
            "Installed 42 packages in 0.8s".into(),
        ];
        for l in lines {
            self.step(cancel)?;
            let _ = log.send(Line {
                stream: Stream::Stdout,
                text: l,
            });
        }
        if self.w().failures.install_fails {
            let _ = log.send(Line {
                stream: Stream::Stderr,
                text: "error: failed to download google-colab-cli".into(),
            });
            return Err(Error::new(ErrorKind::InstallFailed).with("exit_code", 2));
        }
        let _ = log.send(Line {
            stream: Stream::Stdout,
            text: "Installed 1 executable: colab".into(),
        });
        let mut w = self.w();
        w.cli_installed = true;
        w.failures.cli_missing = false;
        Ok(())
    }

    fn accounts(&self, cancel: &CancelToken) -> Result<AccountsView> {
        self.ready()?;
        self.step(cancel)?;
        // Like the real backend: an interrupted add (no live flow) is rolled back.
        let recovered = {
            let mut w = self.w();
            !w.login_live && w.rollback_add()
        };
        let mut view = self.view();
        view.recovered_pending_add = recovered;
        Ok(view)
    }

    fn adopt_active_account(&self, _: &CancelToken) -> Result<Account> {
        self.ready()?;
        let now = self.now_();
        let mut w = self.w();
        w.adopt_unadopted(now);
        let id = w
            .active
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::AuthRequired))?;
        w.account_views()
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| Error::new(ErrorKind::AccountNotFound))
    }

    fn begin_add_account(&self, cancel: &CancelToken) -> Result<Box<dyn LoginFlow>> {
        self.ready()?;
        self.step(cancel)?;
        let now = self.now_();
        {
            let mut w = self.w();
            if w.pending_add.is_some() {
                return Err(Error::new(ErrorKind::AddInProgress));
            }
            if w.failures.login == Some(MockLoginFailure::CancelledBeforeUrl) {
                return Err(Error::new(ErrorKind::LoginCancelled));
            }
            // Like the real backend: save an unsaved credential first, then
            // back up and clear the slot while the flow is alive.
            w.adopt_unadopted(now);
            let previous = w.active.take();
            w.pending_add = Some(previous);
            w.login_live = true;
            w.tunnels.clear();
        }
        Ok(Box::new(MockLoginFlow {
            finished: false,
            m: self.clone(),
            url: "https://accounts.google.com/o/oauth2/auth?client_id=mock&redirect_uri=https%3A%2F%2Fsdk.cloud.google.com%2Fapplicationdefaultauthcode.html".into(),
        }))
    }

    fn switch_impact(&self, to: &AccountId) -> Result<SwitchImpact> {
        let w = self.w();
        if !w.accounts.iter().any(|a| &a.id == to) {
            return Err(Error::new(ErrorKind::AccountNotFound).with("account", to));
        }
        let from = w.active.clone();
        let switching = from.as_ref() != Some(to);
        let unmanaged_left_running = match (&from, switching) {
            (Some(f), true) if w.listing_fails_for(f) => None,
            (Some(f), true) => Some(w.unmanaged_on(f)),
            _ => Some(vec![]),
        };
        Ok(SwitchImpact {
            from: from.clone(),
            to: to.clone(),
            tunnels_closing: if switching {
                w.tunnels.values().cloned().collect()
            } else {
                vec![]
            },
            sessions_left_running: from
                .as_ref()
                .filter(|_| switching)
                .map(|from| w.ledger.live_of(from).cloned().collect())
                .unwrap_or_default(),
            unmanaged_left_running,
        })
    }

    fn switch_account(&self, to: &AccountId, cancel: &CancelToken) -> Result<AccountsView> {
        self.ready()?;
        if !self.w().accounts.iter().any(|a| &a.id == to) {
            return Err(Error::new(ErrorKind::AccountNotFound).with("account", to));
        }
        self.step(cancel)?;
        let mut w = self.w();
        if w.active.as_ref() != Some(to) {
            w.tunnels.clear();
            w.active = Some(to.clone());
        }
        drop(w);
        Ok(self.view())
    }

    fn remove_account(&self, id: &AccountId, _: &CancelToken) -> Result<()> {
        self.ready()?;
        let mut w = self.w();
        if w.active.as_ref() == Some(id) {
            return Err(Error::invalid(
                "account",
                "switch to another account before removing this one",
            ));
        }
        if !w.live_names(id).is_empty() {
            return Err(Error::invalid("account", "stop its sessions first")
                .next(crate::error::NextStep::StopASession));
        }
        w.accounts.retain(|a| &a.id != id);
        w.usage.remove(id);
        Ok(())
    }

    fn refresh_usage(
        &self,
        accounts: &[AccountId],
        cancel: &CancelToken,
    ) -> Result<Vec<AccountUsageView>> {
        self.ready()?;
        self.step(cancel)?;
        let now = self.now_();
        let mut w = self.w();
        w.settle(now);
        let ids: Vec<AccountId> = if accounts.is_empty() {
            w.accounts.iter().map(|a| a.id.clone()).collect()
        } else {
            accounts.to_vec()
        };
        for id in ids {
            let (verified, _) = w.reconcile(&id, now);
            w.record_usage(&id, now, verified);
        }
        Ok(w.usage_views())
    }

    fn usage(&self) -> Vec<AccountUsageView> {
        self.w().usage_views()
    }

    fn measured_rates(&self) -> Vec<HardwareRate> {
        self.w().ledger.measured_rates()
    }

    fn usage_poll_due(&self) -> bool {
        let now = self.now_();
        let w = self.w();
        w.accounts.iter().any(|a| {
            let has = w.runtimes.iter().any(|r| r.account == a.id);
            poll_due(
                w.usage.get(&a.id).and_then(|u| u.last_attempt),
                now,
                has,
                &w.defaults,
            )
        })
    }

    fn runtimes(&self, cancel: &CancelToken) -> Result<RuntimesView> {
        self.ready()?;
        self.step(cancel)?;
        let now = self.now_();
        let mut w = self.w();
        w.runtime_listings += 1;
        if w.failures.runtimes_listing_fails {
            return Err(Error::new(ErrorKind::Http).detail("simulated runtime listing failure"));
        }
        w.settle(now);
        let mut view = RuntimesView::default();
        let ids: Vec<_> = w.accounts.iter().map(|a| a.id.clone()).collect();
        for id in ids {
            w.reconcile(&id, now);
        }
        for account in &w.accounts {
            if w.failures.unlisted_accounts.contains(&account.email) {
                view.errors.push(AccountError {
                    account: account.id.clone(),
                    error: Error::new(ErrorKind::Http).detail("simulated account listing failure"),
                });
            }
        }
        for r in &w.runtimes {
            if view.errors.iter().any(|failed| failed.account == r.account) {
                continue;
            }
            let tracked = r
                .name
                .as_ref()
                .and_then(|n| w.ledger.live(&r.account, n).cloned());
            let rate = r.name.as_ref().and_then(|n| {
                w.usage
                    .get(&r.account)
                    .and_then(|u| u.attribution.rate_of(n))
            });
            view.runtimes.push(Runtime {
                id: RuntimeRef {
                    account: r.account.clone(),
                    endpoint: r.endpoint.clone(),
                    name: r.name.clone(),
                },
                account_email: w.email(&r.account),
                on_active_account: w.active.as_ref() == Some(&r.account),
                accelerator: Some(r.accelerator),
                hardware: r.hardware(),
                high_mem: r.high_mem,
                managed: tracked.is_some(),
                estimated_rate_cu_per_hour: rate.filter(|_| tracked.is_some()),
                estimated_spend_cu: tracked.as_ref().and_then(|t| t.spend.spend()),
                auto_stop_at: tracked.as_ref().and_then(|t| t.auto_stop_at()),
                tunnels: w
                    .tunnels
                    .values()
                    .filter(|t| {
                        t.spec.account == r.account && Some(&t.spec.session) == r.name.as_ref()
                    })
                    .map(|t| t.id)
                    .collect(),
                tracked,
            });
        }
        view.runtimes.sort_by_key(|r| !r.on_active_account);
        Ok(view)
    }

    fn new_session_name(&self, prefix: Option<&str>) -> String {
        let w = self.w();
        let taken = w.taken_names();
        let p = prefix
            .map(str::to_owned)
            .unwrap_or_else(|| accelerator_prefix(w.defaults.accelerator));
        generate_session_name(&p, &taken)
    }

    fn provision(
        &self,
        spec: ProvisionSpec,
        events: Events<ProvisionEvent>,
        cancel: &CancelToken,
    ) -> Result<ProvisionOutcome> {
        let account = self.active()?;
        let limit = {
            let mut w = self.w();
            let mut l = w.defaults.session_limit();
            l.reserved = w.reserved;
            w.reserved += 1;
            l
        };
        let ops = MockOps {
            m: self,
            account: account.clone(),
            restart: false,
        };
        let r = provision::provision(&ops, &spec, limit, self.cfg.clock.as_ref(), cancel, &|e| {
            let _ = events.send(e);
        });
        let mut w = self.w();
        w.reserved = w.reserved.saturating_sub(1);
        let outcome = r?;
        let endpoint = w
            .runtime(&account, &outcome.name)
            .map(|r| r.endpoint.clone());
        w.ledger.upsert(TrackedSession {
            name: outcome.name.clone(),
            account,
            endpoint,
            accelerator: spec.accelerator,
            high_mem: spec.high_mem,
            gpu: outcome.hardware.gpu.clone(),
            vram_mib: outcome.hardware.vram_mib,
            policy: spec.guard.clone(),
            spend: SpendMeter::new(outcome.started_at),
            watchdog: outcome.watchdog.clone(),
            stopped: None,
            measured_rate: None,
            keep_plan: None,
            last_keep_sync: None,
        })?;
        Ok(outcome)
    }

    fn stop_session(
        &self,
        target: &RuntimeRef,
        options: StopOptions,
        cancel: &CancelToken,
    ) -> Result<()> {
        self.ready()?;
        self.step(cancel)?;
        if options.save_keep {
            let has_plan = target.name.as_ref().is_some_and(|name| {
                self.w()
                    .ledger
                    .live(&target.account, name)
                    .and_then(|e| e.keep_plan.as_ref())
                    .is_some_and(|p| p.has_enabled_target())
            });
            if has_plan
                && let Err(err) = self.sync_keep(target, None, cancel)
                && !options.force_on_keep_error
            {
                return Err(err);
            }
        }
        self.stop_inner(target, options.reason)
    }

    fn vm_ls(&self, _target: &RuntimeRef, path: &str, cancel: &CancelToken) -> Result<Vec<VmFile>> {
        self.ready()?;
        self.step(cancel)?;
        if self.w().failures.vm_listing_empty {
            return Ok(Vec::new());
        }
        Ok(vec![
            VmFile {
                path: format!("{path}/image.png"),
                size: 12_345,
            },
            VmFile {
                path: format!("{path}/other.png"),
                size: 6_789,
            },
        ])
    }

    fn vm_list_dir(
        &self,
        target: &RuntimeRef,
        path: &str,
        cancel: &CancelToken,
    ) -> Result<Vec<VmEntry>> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .as_deref()
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        self.step(cancel)?;
        let w = self.w();
        let rt = w
            .runtime(&target.account, name)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone).with("session", name))?;
        if w.failures.vm_listing_empty {
            return Err(
                Error::new(ErrorKind::Io).detail(format!("File or directory not found: {path}"))
            );
        }
        let norm = format!("/{}", path.trim_start_matches('/'));
        if rt.vm_files.contains_key(&norm) {
            let base = norm.rsplit('/').next().unwrap_or(&norm).to_owned();
            return Ok(vec![VmEntry {
                name: base,
                is_dir: false,
            }]);
        }
        let prefix = if norm == "/" {
            "/".to_owned()
        } else {
            format!("{}/", norm.trim_end_matches('/'))
        };
        let mut dirs = BTreeSet::new();
        let mut files = BTreeSet::new();
        for key in rt.vm_files.keys() {
            if let Some(rest) = key.strip_prefix(&prefix) {
                if let Some((dir, _)) = rest.split_once('/') {
                    dirs.insert(dir.to_owned());
                } else if !rest.is_empty() {
                    files.insert(rest.to_owned());
                }
            }
        }
        let mut entries: Vec<VmEntry> = dirs
            .into_iter()
            .map(|name| VmEntry { name, is_dir: true })
            .collect();
        entries.extend(files.into_iter().map(|name| VmEntry {
            name,
            is_dir: false,
        }));
        Ok(entries)
    }

    fn vm_download(
        &self,
        _target: &RuntimeRef,
        remote: &str,
        local: &str,
        cancel: &CancelToken,
    ) -> Result<()> {
        self.ready()?;
        self.step(cancel)?;
        let mut w = self.w();
        w.downloads.push(format!("{remote} -> {local}"));
        Ok(())
    }

    fn vm_upload(
        &self,
        target: &RuntimeRef,
        local: &str,
        remote: &str,
        cancel: &CancelToken,
    ) -> Result<()> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .as_deref()
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        self.step(cancel)?;
        let bytes = std::fs::read(local).map_err(|e| {
            Error::new(ErrorKind::Io)
                .with("path", local)
                .detail(e.to_string())
        })?;
        let mut w = self.w();
        let rt = w
            .runtime_mut(&target.account, name)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone).with("session", name))?;
        let norm = format!("/{}", remote.trim_start_matches('/'));
        rt.vm_files.insert(norm, bytes);
        rt.uploads.push(format!("{local} -> {remote}"));
        Ok(())
    }

    fn set_keep_plan(
        &self,
        target: &RuntimeRef,
        plan: Option<crate::keep::KeepPlan>,
        cancel: &CancelToken,
    ) -> Result<()> {
        if let Some(ref p) = plan {
            p.validate()?;
        }
        self.require_active(&target.account)?;
        let name = target
            .name
            .as_deref()
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        self.step(cancel)?;
        let mut w = self.w();
        if w.runtime(&target.account, name).is_none() {
            return Err(Error::new(ErrorKind::SessionGone).with("session", name));
        }
        let found = w
            .ledger
            .update(&target.account, name, |e| e.keep_plan = plan.clone())?;
        if !found {
            return Err(Error::new(ErrorKind::SessionGone).with("session", name));
        }
        Ok(())
    }

    fn sync_keep(
        &self,
        target: &RuntimeRef,
        targets: Option<Vec<crate::keep::KeepTargetId>>,
        cancel: &CancelToken,
    ) -> Result<crate::keep::KeepSyncReport> {
        use crate::keep::{
            KeepSyncReport, KeepTargetId, KeepTargetResult, ensure_report_succeeded,
        };

        self.require_active(&target.account)?;
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        self.step(cancel)?;
        let now = self.now_();
        let (plan, vm_empty, sync_fails, has_hf_token, drive_mounts, vm_count, vm_bytes) = {
            let w = self.w();
            let rt = w
                .runtime(&target.account, &name)
                .ok_or_else(|| Error::new(ErrorKind::SessionGone).with("session", &name))?;
            let plan = w
                .ledger
                .live(&target.account, &name)
                .and_then(|e| e.keep_plan.clone())
                .ok_or_else(|| Error::invalid("keep_plan", "no Keep Plan configured"))?;
            let count = rt.vm_files.len() as u64;
            let bytes: u64 = rt.vm_files.values().map(|v| v.len() as u64).sum();
            (
                plan,
                w.failures.vm_listing_empty,
                w.failures.keep_sync_fails,
                w.secrets
                    .get("hf_token")
                    .is_some_and(|t| !t.trim().is_empty()),
                rt.drive_mounts.clone(),
                count.max(1),
                bytes.max(64),
            )
        };
        if plan.paths.is_empty() {
            return Err(Error::invalid("keep_paths", "no VM paths configured"));
        }
        plan.validate()?;
        let selected: Vec<KeepTargetId> = match targets {
            Some(list) if !list.is_empty() => list,
            _ => {
                let mut list = Vec::new();
                if plan.local.is_some() {
                    list.push(KeepTargetId::Local);
                }
                if plan.hf.is_some() {
                    list.push(KeepTargetId::HuggingFace);
                }
                if plan.drive.as_ref().is_some_and(|d| d.consented) {
                    list.push(KeepTargetId::Drive);
                }
                list
            }
        };
        if selected.is_empty() {
            return Err(Error::invalid("keep_plan", "no Keep Plan targets enabled"));
        }
        let mut results = Vec::new();
        for id in selected {
            let res = match id {
                KeepTargetId::Local => {
                    let Some(local) = &plan.local else {
                        results.push((
                            id,
                            KeepTargetResult {
                                files: 0,
                                bytes: 0,
                                destination: String::new(),
                                error: Some(Error::invalid(
                                    "local_folder",
                                    "local Keep Plan target is not configured",
                                )),
                            },
                        ));
                        continue;
                    };
                    let stamp = now
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let local_file = std::path::PathBuf::from(&local.folder)
                        .join(format!("{name}-{stamp}.tar.gz"));
                    let dest = local_file.to_string_lossy().into_owned();
                    if sync_fails || vm_empty {
                        KeepTargetResult {
                            files: 0,
                            bytes: 0,
                            destination: dest,
                            error: Some(
                                Error::new(ErrorKind::Io)
                                    .detail("simulated Keep Plan sync failure"),
                            ),
                        }
                    } else if let Err(e) = std::fs::create_dir_all(&local.folder)
                        .and_then(|()| std::fs::write(&local_file, b"mock-keep-archive"))
                    {
                        KeepTargetResult {
                            files: 0,
                            bytes: 0,
                            destination: dest,
                            error: Some(Error::from(e)),
                        }
                    } else {
                        KeepTargetResult {
                            files: vm_count,
                            bytes: vm_bytes,
                            destination: dest,
                            error: None,
                        }
                    }
                }
                KeepTargetId::HuggingFace => {
                    let Some(hf) = &plan.hf else {
                        results.push((
                            id,
                            KeepTargetResult {
                                files: 0,
                                bytes: 0,
                                destination: String::new(),
                                error: Some(Error::invalid(
                                    "hf_repo",
                                    "Hugging Face Keep Plan target is not configured",
                                )),
                            },
                        ));
                        continue;
                    };
                    let dest = match &hf.path_in_repo {
                        Some(sub) if !sub.trim().is_empty() => {
                            format!("{}/{}", hf.repo, sub.trim())
                        }
                        _ => hf.repo.clone(),
                    };
                    if !has_hf_token {
                        KeepTargetResult {
                            files: 0,
                            bytes: 0,
                            destination: dest,
                            error: Some(Error::invalid("hf_token", "no Hugging Face token saved")),
                        }
                    } else if sync_fails || vm_empty {
                        KeepTargetResult {
                            files: 0,
                            bytes: 0,
                            destination: dest,
                            error: Some(
                                Error::new(ErrorKind::Other)
                                    .detail("simulated Hugging Face upload failure"),
                            ),
                        }
                    } else {
                        KeepTargetResult {
                            files: vm_count,
                            bytes: vm_bytes,
                            destination: dest,
                            error: None,
                        }
                    }
                }
                KeepTargetId::Drive => {
                    let Some(drive) = &plan.drive else {
                        results.push((
                            id,
                            KeepTargetResult {
                                files: 0,
                                bytes: 0,
                                destination: String::new(),
                                error: Some(Error::invalid(
                                    "drive",
                                    "Google Drive Keep Plan target is not configured",
                                )),
                            },
                        ));
                        continue;
                    };
                    let dest = format!(
                        "{}/{}/{}",
                        drive.mount_path.trim_end_matches('/'),
                        drive.dest_subpath.trim_end_matches('/'),
                        name
                    );
                    if !drive.consented {
                        KeepTargetResult {
                            files: 0,
                            bytes: 0,
                            destination: dest,
                            error: Some(Error::invalid(
                                "drive_consent",
                                "Google Drive mount requires explicit user consent",
                            )),
                        }
                    } else if !drive.mounted || !drive_mounts.contains(&drive.mount_path) {
                        KeepTargetResult {
                            files: 0,
                            bytes: 0,
                            destination: dest,
                            error: Some(Error::invalid(
                                "drive_mount",
                                "Google Drive is not mounted on this runtime",
                            )),
                        }
                    } else if sync_fails || vm_empty {
                        KeepTargetResult {
                            files: 0,
                            bytes: 0,
                            destination: dest,
                            error: Some(
                                Error::new(ErrorKind::Io)
                                    .detail("simulated Google Drive copy failure"),
                            ),
                        }
                    } else {
                        KeepTargetResult {
                            files: vm_count,
                            bytes: vm_bytes,
                            destination: dest,
                            error: None,
                        }
                    }
                }
            };
            results.push((id, res));
        }
        let report = KeepSyncReport { at: now, results };
        let _ = self.w().ledger.update(&target.account, &name, |e| {
            e.last_keep_sync = Some(report.clone())
        });
        ensure_report_succeeded(&report)?;
        Ok(report)
    }

    fn begin_drive_mount(
        &self,
        target: &RuntimeRef,
        mount_path: &str,
        cancel: &CancelToken,
    ) -> Result<DriveMountStart> {
        crate::keep::validate_vm_path("drive_mount_path", mount_path)?;
        self.require_active(&target.account)?;
        let name = target
            .name
            .as_deref()
            .ok_or_else(|| Error::new(ErrorKind::SessionGone))?;
        self.step(cancel)?;
        let already_mounted = {
            let w = self.w();
            let rt = w
                .runtime(&target.account, name)
                .ok_or_else(|| Error::new(ErrorKind::SessionGone).with("session", name))?;
            rt.drive_mounts.contains(mount_path)
        };
        if already_mounted {
            self.finish_drive_mount(target, mount_path)?;
            return Ok(DriveMountStart::Mounted);
        }
        Ok(DriveMountStart::NeedsConsent(Box::new(
            MockDriveMountFlow {
                m: self.clone(),
                target: target.clone(),
                mount_path: mount_path.to_owned(),
                url: "https://accounts.google.com/o/oauth2/auth?client_id=mock-drive&scope=drive"
                    .into(),
            },
        )))
    }

    fn extend_session(
        &self,
        target: &RuntimeRef,
        by: Duration,
        cancel: &CancelToken,
    ) -> Result<Extension> {
        self.move_auto_stop(target, |left| left + by, cancel)
    }

    fn set_auto_stop(
        &self,
        target: &RuntimeRef,
        runtime: Duration,
        cancel: &CancelToken,
    ) -> Result<WatchdogInfo> {
        self.move_auto_stop(target, |_| runtime, cancel)
            .map(|set| set.watchdog)
    }

    fn cancel_auto_stop(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<()> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        self.step(cancel)?;
        let mut w = self.w();
        // The cancelled watchdog exits without releasing the runtime.
        if let Some(rt) = w.runtime_mut(&target.account, &name) {
            rt.watchdog_alive = false;
        }
        w.ledger.update(&target.account, &name, |e| {
            e.policy.max_runtime = None;
            if let Some(wd) = e.watchdog.as_mut() {
                wd.cancelled = true;
                wd.armed = false;
            }
        })?;
        Ok(())
    }

    fn guard_poll(&self, cancel: &CancelToken) -> Result<GuardReport> {
        self.ready()?;
        self.step(cancel)?;
        let now = self.now_();
        let mut report = GuardReport::default();
        let live: Vec<TrackedSession> = {
            let mut w = self.w();
            w.settle(now);
            let live: Vec<TrackedSession> = w.ledger.all_live().cloned().collect();
            let ids: BTreeSet<AccountId> = live.iter().map(|e| e.account.clone()).collect();
            for id in &ids {
                let (verified, vanished) = w.reconcile(id, now);
                report.vanished.extend(vanished);
                w.record_usage(id, now, verified);
            }
            w.ledger
                .all_live()
                .filter(|e| ids.contains(&e.account))
                .cloned()
                .collect()
        };
        for e in live {
            if report
                .vanished
                .iter()
                .any(|v| v.account == e.account && v.name.as_deref() == Some(&e.name))
            {
                continue;
            }
            let (decision, target) = {
                let mut w = self.w();
                let fresh = !w.failures.usage_fetch_fails;
                let u = w.usage.get(&e.account).cloned().unwrap_or_default();
                let rate = if fresh {
                    u.attribution.rate_of(&e.name)
                } else {
                    None
                };
                let mut meter = e.spend.clone();
                meter.record(now, rate);
                let _ = w
                    .ledger
                    .update(&e.account, &e.name, |x| x.spend = meter.clone());
                let target = RuntimeRef {
                    account: e.account.clone(),
                    endpoint: e.endpoint.clone().unwrap_or_default(),
                    name: Some(e.name.clone()),
                };
                (
                    evaluate(&e.policy, now, u.snapshot.as_ref(), &meter),
                    target,
                )
            };
            let (mut stopped, mut error) = (false, None);
            if let Decision::Stop(concern) = &decision {
                let save_keep = e.keep_plan.as_ref().is_some_and(|p| p.has_enabled_target());
                match self.stop_session(
                    &target,
                    StopOptions {
                        reason: StopReason::Guard {
                            concern: concern.clone(),
                        },
                        save_keep,
                        force_on_keep_error: true,
                        keep_timeout: Some(Duration::from_secs(45)),
                    },
                    cancel,
                ) {
                    Ok(()) => stopped = true,
                    Err(err) => error = Some(err),
                }
            }
            report.decisions.push(GuardDecision {
                session: target,
                decision,
                stopped,
                error,
            });
        }
        report.usage = self.w().usage_views();
        Ok(report)
    }

    fn open_tunnel(&self, spec: TunnelSpec, events: Events<TunnelEvent>) -> Result<TunnelId> {
        spec.validate()?;
        self.require_active(&spec.account)?;
        let mut w = self.w();
        if !w.runtime(&spec.account, &spec.session).is_some() {
            return Err(Error::new(ErrorKind::SessionGone).with("session", &spec.session));
        }
        if w.tunnels
            .values()
            .any(|t| t.spec.local_port == spec.local_port && !t.state.is_terminal())
        {
            return Err(Error::new(ErrorKind::PortInUse).with("port", spec.local_port));
        }
        w.next_tunnel += 1;
        let id = TunnelId(w.next_tunnel);
        w.tunnels.insert(
            id,
            TunnelInfo {
                id,
                spec,
                state: TunnelState::Starting,
                healthy: None,
            },
        );
        w.set_tunnel(id, TunnelState::Starting, Some(&events));
        if w.ssh_busy_left > 0 {
            w.ssh_busy_left = w
                .ssh_busy_left
                .saturating_sub(if w.ssh_busy_left == u32::MAX { 0 } else { 1 });
            w.set_tunnel(
                id,
                TunnelState::Failed {
                    error: Error::new(ErrorKind::SshBusy),
                },
                Some(&events),
            );
        } else {
            w.set_tunnel(id, TunnelState::Up, Some(&events));
            if let Some(t) = w.tunnels.get_mut(&id) {
                t.healthy = Some(true);
            }
            let _ = events.send(TunnelEvent::Health { id, healthy: true });
            w.tunnel_events.insert(id, events);
        }
        Ok(id)
    }

    fn close_tunnel(&self, id: TunnelId) -> Result<()> {
        let mut w = self.w();
        w.tunnel_events.remove(&id);
        w.tunnels
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| Error::invalid("tunnel", "unknown tunnel"))
    }

    fn tunnels(&self) -> Vec<TunnelInfo> {
        self.w().tunnels.values().cloned().collect()
    }

    fn open_in_browser(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<String> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        self.step(cancel)?;
        let mut w = self.w();
        let rt = w
            .runtime(&target.account, &name)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone).with("session", &name))?;
        let url = format!(
            "https://colab.research.google.com/notebooks/empty.ipynb?dbu=%2Ftun%2Fm%2F{}#datalabBackendUrl=https://colab.research.google.com/tun/m/{}",
            rt.endpoint, rt.endpoint
        );
        w.opened_urls.push(url.clone());
        Ok(url)
    }

    fn restart_kernel(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<()> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        self.step(cancel)?;
        let mut w = self.w();
        if w.failures.restart_kernel_fails {
            return Err(Error::new(ErrorKind::ConnectionLost));
        }
        w.runtime(&target.account, &name)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone).with("session", &name))?;
        w.restarted_kernels.push(name);
        Ok(())
    }

    fn reset_ssh_sessions(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<()> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        self.step(cancel)?;
        let mut w = self.w();
        w.runtime(&target.account, &name)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone).with("session", &name))?;
        w.close_session_tunnels(&target.account, &name);
        w.ssh_busy_left = 0;
        w.ssh_resets.push(name);
        Ok(())
    }

    fn remote_ssh(&self, target: &RuntimeRef, cancel: &CancelToken) -> Result<RemoteSsh> {
        self.require_active(&target.account)?;
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        self.step(cancel)?;
        let w = self.w();
        w.runtime(&target.account, &name)
            .ok_or_else(|| Error::new(ErrorKind::SessionGone).with("session", &name))?;
        let state = crate::paths::HostPath::new(format!(
            "/home/demo/.local/share/colab-playground/accounts/{}/sessions.json",
            target.account.as_str()
        ));
        crate::tunnel::remote_ssh_info(&Host::Local, "/home/demo/.local/bin/colab", &state, &name)
    }

    fn write_temp_ssh_config(
        &self,
        target: &RuntimeRef,
        cancel: &CancelToken,
    ) -> Result<std::path::PathBuf> {
        let name = target
            .name
            .clone()
            .ok_or_else(|| Error::invalid("name", "the session has no local name"))?;
        let info = self.remote_ssh(target, cancel)?;
        let path = std::env::temp_dir()
            .join("colab-playground-mock-ssh")
            .join(format!("{}-{name}.conf", target.account.as_str()));
        crate::paths::write_atomic(&path, format!("{}\n", info.config_snippet).as_bytes())?;
        Ok(path)
    }

    fn plugins(&self) -> Vec<PluginInfo> {
        self.w().plugins.list()
    }

    fn plugin_run_template(&self, plugin: &str) -> Result<PluginRunSpec> {
        let w = self.w();
        let info = w
            .plugins
            .list()
            .into_iter()
            .find(|p| p.manifest.id == plugin)
            .ok_or_else(|| Error::new(ErrorKind::PluginNotFound).with("plugin", plugin))?;
        let taken = w.taken_names();
        Ok(run_template(&info, &w.defaults, &taken))
    }

    fn inspect_plugin_folder(&self, path: &Path) -> Result<PluginPreview> {
        let staging =
            std::env::temp_dir().join(format!("colab-playground-mock-{}", std::process::id()));
        self.w().plugins.inspect_folder(
            path,
            &staging,
            PluginOrigin::Folder {
                path: path.display().to_string(),
            },
        )
    }

    fn inspect_plugin_git(&self, url: &str, _: &CancelToken) -> Result<PluginPreview> {
        crate::plugins::validate_git_url(url)?;
        Err(Error::new(ErrorKind::GitFailed)
            .with("url", url)
            .detail("Git installs are not simulated; use a folder"))
    }

    fn install_plugin(&self, preview: &PluginPreview) -> Result<PluginInfo> {
        self.w().plugins.install(preview)
    }

    fn discard_plugin_preview(&self, preview: &PluginPreview) {
        discard_preview(preview);
    }

    fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.w().plugins.set_enabled(id, enabled)
    }

    fn remove_plugin(&self, id: &str) -> Result<()> {
        self.w().plugins.remove(id)
    }

    fn run_plugin(
        &self,
        spec: PluginRunSpec,
        events: Events<PluginRunEvent>,
        cancel: &CancelToken,
    ) -> Result<PluginRunOutcome> {
        let account = self.active()?;
        let (plugin, enabled) = {
            let w = self.w();
            let p = w.plugins.get(&spec.plugin).cloned().ok_or_else(|| {
                Error::new(ErrorKind::PluginNotFound).with("plugin", &spec.plugin)
            })?;
            (p, w.plugins.enabled(&spec.plugin))
        };
        if !enabled {
            return Err(
                Error::invalid("plugin", "the plugin is disabled").with("plugin", &spec.plugin)
            );
        }
        let resolved = resolve_inputs(&plugin.manifest, &spec.inputs)?;
        let endpoints = resolve_endpoints(&plugin.manifest, &resolved.values)?;
        let saved = self.w().secrets.clone();
        let (cap_env, cap_secrets) = capability_env(&plugin.manifest, &spec.consent, |name| {
            saved.get(name).cloned()
        })?;
        let mut env = resolved.env.clone();
        env.extend(cap_env);
        let mut secrets = resolved.secrets.clone();
        secrets.extend(cap_secrets);
        let mut secret_keys: Vec<String> = plugin
            .manifest
            .inputs
            .iter()
            .filter(|input| input.kind == InputKind::Secret)
            .map(|input| input.env_key())
            .collect();
        for (key, _) in &env {
            if (key == "PLUGIN_HF_TOKEN" || key.starts_with("PLUGIN_SECRET_"))
                && !secret_keys.iter().any(|existing| existing == key)
            {
                secret_keys.push(key.clone());
            }
        }
        env.push((crate::vm::ENV_SECRET_KEYS.to_owned(), secret_keys.join(",")));
        let (session, provisioned) = match &spec.target {
            RunTarget::Existing { name } => {
                if !self.w().runtime(&account, name).is_some() {
                    return Err(Error::new(ErrorKind::SessionGone).with("session", name));
                }
                (name.clone(), false)
            }
            RunTarget::Provision(p) => {
                // Forwarded as they happen, like the real backend, so the progress shows.
                let (tx, rx) = std::sync::mpsc::channel();
                let ev = events.clone();
                let fwd = std::thread::spawn(move || {
                    for e in rx {
                        let _ = ev.send(PluginRunEvent::Provision(e));
                    }
                });
                let r = self.provision(p.clone(), tx, cancel);
                let _ = fwd.join();
                (r?.name, true)
            }
        };
        let endpoint_of = |m: &MockBackend| {
            m.w()
                .runtime(&account, &session)
                .map(|r| r.endpoint.clone())
        };
        if let Some(mut plan) = spec.keep_plan {
            let extra: Vec<String> = plugin
                .manifest
                .keep
                .iter()
                .map(|k| k.path.clone())
                .collect();
            plan.merge_paths(&extra);
            let ep = endpoint_of(self).unwrap_or_default();
            self.set_keep_plan(
                &RuntimeRef {
                    account: account.clone(),
                    endpoint: ep,
                    name: Some(session.clone()),
                },
                Some(plan),
                cancel,
            )?;
        }
        let target_of = |m: &MockBackend| RuntimeRef {
            account: account.clone(),
            endpoint: endpoint_of(m).unwrap_or_default(),
            name: Some(session.clone()),
        };
        let finish = |m: &MockBackend, end: RunEnd| -> Result<()> {
            let action = crate::plugins::after_run(
                spec.mode,
                provisioned,
                end,
                spec.stop_on_failure,
                spec.keep_on_failure,
            );
            m.apply_after(action, &target_of(m), cancel)
        };
        let run_id = format!("{}-{}", plugin.manifest.id, &crate::accounts::nonce()[..8]);
        let run_dir = crate::vm::run_dir(&run_id);
        let uploaded = (|| -> Result<()> {
            let mut staged = resolved.uploads.clone();
            let scratch = if plugin.assets.is_empty() {
                None
            } else {
                Some(
                    tempfile::tempdir()
                        .map_err(|e| Error::new(ErrorKind::Io).detail(e.to_string()))?,
                )
            };
            if let Some(dir) = scratch.as_ref() {
                staged.extend(materialize_assets(&plugin.assets, dir.path())?);
            }
            let files = expand_uploads(&staged)?;
            for (local, rel) in files {
                let local_str = local
                    .to_str()
                    .ok_or_else(|| Error::invalid("path", "the local path is not usable"))?;
                self.vm_upload(
                    &target_of(self),
                    local_str,
                    &format!("{run_dir}/{rel}"),
                    cancel,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = uploaded {
            let end = if error.kind == ErrorKind::Cancelled {
                RunEnd::Cancelled
            } else {
                RunEnd::Failed
            };
            let _ = finish(self, end);
            return Err(error);
        }
        env.extend(vm_env(&run_dir, &resolved.vm_rels));
        self.w().last_plugin_env = env;
        let _ = events.send(PluginRunEvent::Launched {
            session: session.clone(),
            run_id: run_id.clone(),
        });
        let vram = self
            .w()
            .runtime(&account, &session)
            .and_then(|r| r.vram_mib);
        let fail_stage = self.w().failures.plugin_fail_stage.clone();
        let stages: Vec<String> = plugin
            .manifest
            .stages
            .iter()
            .map(|s| s.id.clone())
            .collect();
        for (i, stage) in stages.iter().enumerate() {
            if self.step(cancel).is_err() {
                let _ = finish(self, RunEnd::Cancelled);
                return Err(Error::cancelled());
            }
            let status = RunStatus {
                state: RunState::Running,
                stage: Some(stage.clone()),
                progress: Some((i as f64 + 0.5) / stages.len().max(1) as f64),
                message: Some(format!("simulated {stage}")),
                outputs: Default::default(),
                metrics: Default::default(),
            };
            let _ = events.send(PluginRunEvent::Status(status));
            let leaked = secrets.first().cloned().unwrap_or_default();
            let _ = events.send(PluginRunEvent::Log {
                lines: vec![redact(
                    &format!("[mock] {stage}: working {leaked}"),
                    &secrets,
                )],
            });
            if fail_stage.as_deref() == Some(stage.as_str()) {
                let failed = RunStatus {
                    state: RunState::Failed,
                    stage: Some(stage.clone()),
                    progress: None,
                    message: Some(format!("simulated failure in {stage}")),
                    outputs: Default::default(),
                    metrics: Default::default(),
                };
                let _ = events.send(PluginRunEvent::Status(failed));
                let _ = finish(self, RunEnd::Failed);
                return Err(Error::new(ErrorKind::PluginRunFailed)
                    .with("stage", stage)
                    .with("message", format!("simulated failure in {stage}")));
            }
        }
        let mut outputs = serde_json::Map::new();
        if let Some(crate::plugins::InputValue::Text(repo)) = resolved.values.get("repo") {
            let listing = mock_listing(repo);
            let input = FitInput {
                vram_mib: vram.unwrap_or(16_384),
                ctx: 0,
                model_ctx: Some(131_072),
                kv_bytes_per_token: Some(65_536),
                mmproj_bytes: 0,
                kv: None,
            };
            if let Ok(choice) = fit_auto(&listing, &input) {
                outputs.insert("quant".into(), choice.quant.clone().into());
                outputs.insert("n_ctx".into(), choice.ctx.into());
                outputs.insert("kv".into(), choice.kv.as_str().into());
            }
            outputs.insert(
                "model_id".into(),
                repo.rsplit('/')
                    .next()
                    .unwrap_or(repo)
                    .trim_end_matches("-GGUF")
                    .to_ascii_lowercase()
                    .into(),
            );
        }
        if let Some(v) = vram {
            outputs.insert("vram_total_mib".into(), v.into());
        }
        outputs.insert("tok_per_s".into(), 64.2.into());
        let metrics = if spec.mode == RunMode::Job {
            crate::vm::RunMetrics {
                step: Some(3.0),
                loss: Some(0.42),
                tok_per_s: Some(12.5),
            }
        } else {
            crate::vm::RunMetrics::default()
        };
        let status = RunStatus {
            state: RunState::Ready,
            stage: stages.last().cloned(),
            progress: Some(1.0),
            message: Some("ready (simulated)".into()),
            outputs,
            metrics,
        };
        let _ = events.send(PluginRunEvent::Status(status.clone()));
        if plugin.manifest.run.keep_alive {
            // A server stays running: the run ends when it is cancelled.
            for _ in 0..LIVE_RUN_STEPS {
                if cancel.is_cancelled() || self.step(cancel).is_err() {
                    let _ = finish(self, RunEnd::Cancelled);
                    return Err(Error::cancelled());
                }
                std::thread::sleep(Duration::from_millis(LIVE_STEP_MILLIS));
            }
        }
        finish(self, RunEnd::Succeeded)?;
        Ok(PluginRunOutcome {
            session,
            account,
            run_id,
            status,
            endpoints,
        })
    }

    fn discover_runs(
        &self,
        targets: &[RuntimeRef],
        _cancel: &CancelToken,
    ) -> Result<Vec<DiscoveredRun>> {
        self.ready()?;
        let active = self.active()?;
        let w = self.w();
        let found = w
            .active_runs
            .iter()
            .filter(|run| {
                run.account == active
                    && targets.iter().any(|t| {
                        t.account == run.account && t.name.as_deref() == Some(&run.session)
                    })
            })
            .map(|run| DiscoveredRun {
                account: run.account.clone(),
                session: run.session.clone(),
                meta: run.meta.clone(),
                status: run.status.clone(),
                log_tail: run.log_tail.clone(),
                log_skipped: false,
                log_offset: run.log_tail.len() as u64,
            })
            .collect();
        Ok(found)
    }

    fn reattach_run(
        &self,
        run: DiscoveredRun,
        events: Events<PluginRunEvent>,
        cancel: &CancelToken,
    ) -> Result<PluginRunOutcome> {
        let account = run.account.clone();
        let session = run.session.clone();
        let run_id = run.meta.run_id.clone();
        let endpoints = run.meta.endpoints.clone();
        let (step_rx, stages, keep_alive) = {
            let w = self.w();
            let step_rx = w
                .active_runs
                .iter()
                .find(|r| r.meta.run_id == run_id)
                .and_then(|r| r.step_rx.as_ref())
                .and_then(|slot| slot.lock().ok().and_then(|mut guard| guard.take()));
            match w.plugins.get(&run.meta.plugin) {
                Some(p) => (
                    step_rx,
                    p.manifest
                        .stages
                        .iter()
                        .map(|s| s.id.clone())
                        .collect::<Vec<_>>(),
                    p.manifest.run.keep_alive || run.meta.keep_alive,
                ),
                None => (step_rx, Vec::new(), run.meta.keep_alive),
            }
        };
        let manual_stages = step_rx.is_some();
        let wait_step = |cancel: &CancelToken| -> Result<()> {
            if let Some(rx) = &step_rx {
                match rx.try_recv() {
                    Ok(()) => return Ok(()),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return Err(Error::cancelled());
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return self.step(cancel);
                    }
                }
            }
            self.step(cancel)
        };
        let _ = events.send(PluginRunEvent::Launched {
            session: session.clone(),
            run_id: run_id.clone(),
        });
        let finish_entry = |m: &MockBackend| {
            let mut w = m.w();
            w.run_steps.remove(&run_id);
            w.active_runs.retain(|r| r.meta.run_id != run_id);
        };
        let already_ready = run
            .status
            .as_ref()
            .is_some_and(|s| s.state == RunState::Ready);
        let final_status = if already_ready {
            run.status.unwrap_or_else(|| RunStatus {
                state: RunState::Ready,
                stage: stages.last().cloned(),
                progress: Some(1.0),
                message: Some("ready (simulated)".into()),
                outputs: Default::default(),
                metrics: Default::default(),
            })
        } else {
            let start_idx = run
                .status
                .as_ref()
                .and_then(|s| s.stage.as_deref())
                .and_then(|cur| stages.iter().position(|st| st == cur).map(|i| i + 1))
                .unwrap_or(0);
            for (i, stage) in stages.iter().enumerate().skip(start_idx) {
                if wait_step(cancel).is_err() || self.step(cancel).is_err() {
                    finish_entry(self);
                    return Err(Error::cancelled());
                }
                let status = RunStatus {
                    state: RunState::Running,
                    stage: Some(stage.clone()),
                    progress: Some((i as f64 + 0.5) / stages.len().max(1) as f64),
                    message: Some(format!("simulated {stage}")),
                    outputs: Default::default(),
                    metrics: Default::default(),
                };
                let _ = events.send(PluginRunEvent::Status(status));
                let _ = events.send(PluginRunEvent::Log {
                    lines: vec![format!("[mock] {stage}: working")],
                });
            }
            if wait_step(cancel).is_err() {
                finish_entry(self);
                return Err(Error::cancelled());
            }
            let status = RunStatus {
                state: RunState::Ready,
                stage: stages.last().cloned(),
                progress: Some(1.0),
                message: Some("ready (simulated)".into()),
                outputs: Default::default(),
                metrics: Default::default(),
            };
            let _ = events.send(PluginRunEvent::Status(status.clone()));
            status
        };
        if keep_alive {
            for _ in 0..LIVE_RUN_STEPS {
                if cancel.is_cancelled() || self.step(cancel).is_err() {
                    finish_entry(self);
                    return Err(Error::cancelled());
                }
                if !manual_stages {
                    std::thread::sleep(Duration::from_millis(LIVE_STEP_MILLIS));
                }
            }
        }
        finish_entry(self);
        Ok(PluginRunOutcome {
            session,
            account,
            run_id,
            status: final_status,
            endpoints,
        })
    }

    fn set_secret(&self, name: &str, value: &str, _: &CancelToken) -> Result<()> {
        crate::accounts::validate_secret_name(name)?;
        if value.trim().is_empty() {
            return Err(Error::invalid("secret", "empty secret"));
        }
        self.w().secrets.insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    fn has_secret(&self, name: &str, _: &CancelToken) -> Result<bool> {
        crate::accounts::validate_secret_name(name)?;
        Ok(self.w().secrets.contains_key(name))
    }

    fn delete_secret(&self, name: &str, _: &CancelToken) -> Result<()> {
        crate::accounts::validate_secret_name(name)?;
        self.w().secrets.remove(name);
        Ok(())
    }

    fn mcp_token(&self, _: &CancelToken) -> Result<String> {
        let name = crate::mcp::TOKEN_SECRET;
        crate::accounts::validate_secret_name(name)?;
        let existing = self.w().secrets.get(name).cloned();
        if let Some(token) = existing {
            return Ok(token);
        }
        let token = crate::mcp::new_token();
        self.w().secrets.insert(name.to_owned(), token.clone());
        Ok(token)
    }

    fn rotate_mcp_token(&self, cancel: &CancelToken) -> Result<String> {
        self.delete_secret(crate::mcp::TOKEN_SECRET, cancel)?;
        self.mcp_token(cancel)
    }

    fn hf_search(&self, query: &str, limit: u32, cancel: &CancelToken) -> Result<Vec<HfModel>> {
        self.step(cancel)?;
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return Ok(vec![]);
        }
        Ok(catalog()
            .into_iter()
            .filter(|(r, _)| r.to_ascii_lowercase().contains(&q))
            .take(limit as usize)
            .enumerate()
            .map(|(i, (r, _))| HfModel {
                id: r.into(),
                downloads: 100_000 / (i as u64 + 1),
                likes: 500 / (i as u64 + 1),
                gated: false,
            })
            .collect())
    }

    fn hf_files(&self, repo: &str, cancel: &CancelToken) -> Result<GgufListing> {
        if !crate::plugins::is_repo_id(repo) {
            return Err(Error::invalid("repo", "expected owner/name").with("value", repo));
        }
        self.step(cancel)?;
        Ok(mock_listing(repo))
    }

    fn check_for_update(&self, cancel: &CancelToken) -> Result<crate::update::UpdateStatus> {
        cancel.check()?;
        // Never touches the network: the answer comes from the failure switches.
        let mut w = self.w();
        w.update_checks += 1;
        if w.failures.update_check_fails {
            return Err(Error::new(ErrorKind::Http).detail("simulated update check failure"));
        }
        if !w.failures.update_available {
            return Ok(crate::update::UpdateStatus::UpToDate);
        }
        let current = crate::update::Version::parse(crate::update::CURRENT_VERSION).unwrap_or(
            crate::update::Version {
                major: 0,
                minor: 0,
                patch: 0,
                pre: Vec::new(),
            },
        );
        let next = crate::update::Version {
            major: current.major,
            minor: current.minor,
            patch: current.patch + 1,
            pre: Vec::new(),
        }
        .to_string();
        let page = crate::update::this_app_release_page(&next).unwrap_or_default();
        Ok(crate::update::UpdateStatus::Available(
            crate::update::Release {
                tag: format!("v{next}"),
                version: next,
                page,
            },
        ))
    }

    fn self_update(&self) -> SelfUpdate {
        if self.w().failures.update_unsupported {
            SelfUpdate::Unsupported(Unsupported::NotWritable {
                folder: mock_app_folder().to_owned(),
            })
        } else {
            SelfUpdate::Supported
        }
    }

    fn install_update(
        &self,
        release: &crate::update::Release,
        events: Events<UpdateProgress>,
        cancel: &CancelToken,
    ) -> Result<Installed> {
        // Never touches the network or a file: each phase is a paced step, and failures come
        // from the switches.
        let failures = self.w().failures.clone();
        if failures.update_unsupported {
            return Err(Error::new(ErrorKind::Unsupported).detail("simulated: not writable"));
        }
        let _ = events.send(UpdateProgress::Preparing);
        self.step(cancel)?;
        for step in 0..=MOCK_UPDATE_STEPS {
            let _ = events.send(UpdateProgress::Downloading {
                received: MOCK_UPDATE_SIZE * step / MOCK_UPDATE_STEPS,
                total: Some(MOCK_UPDATE_SIZE),
            });
            if failures.update_download_fails && step == MOCK_UPDATE_STEPS / 2 {
                return Err(Error::new(ErrorKind::Http)
                    .with("url", &release.page)
                    .detail("simulated: the connection dropped"));
            }
            if step < MOCK_UPDATE_STEPS {
                self.step(cancel)?;
            }
        }
        let _ = events.send(UpdateProgress::Verifying);
        self.step(cancel)?;
        if failures.update_unverified {
            return Err(Error::new(ErrorKind::UpdateUnverified)
                .detail("simulated: the download's SHA-256 differs from SHA256SUMS"));
        }
        // The point of no return, as in the real backend: a cancel from here on does not apply.
        cancel.commit()?;
        let _ = events.send(UpdateProgress::Installing);
        self.step(cancel)?;
        if failures.update_install_fails {
            return Err(Error::new(ErrorKind::Io)
                .with("path", mock_app_folder())
                .detail("simulated: access denied"));
        }
        self.w().update_installs += 1;
        Ok(Installed {
            version: release.version.clone(),
            relaunch: failures.update_relaunch.then(|| crate::updater::Relaunch {
                program: std::path::PathBuf::from("colab-playground (simulated)"),
                args: Vec::new(),
                launcher: false,
            }),
        })
    }

    fn relaunch(&self, installed: &Installed) -> Result<()> {
        let _ = installed;
        let mut w = self.w();
        w.update_relaunches += 1;
        if w.failures.update_relaunch_fails {
            return Err(Error::new(ErrorKind::Io).detail("simulated: the program did not start"));
        }
        Ok(())
    }
}

/// The folder the simulation says holds the app (named when it "cannot write" there).
pub fn mock_app_folder() -> &'static str {
    match crate::host::Platform::current() {
        Some(crate::host::Platform::Windows) => r"C:\Program Files\Colab Playground",
        Some(crate::host::Platform::MacOs) => "/Applications",
        _ => "/opt/colab-playground",
    }
}

/// The simulated download: its size and the steps it takes (long enough to watch).
const MOCK_UPDATE_SIZE: u64 = 31_457_280;
const MOCK_UPDATE_STEPS: u64 = 16;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn mock(scenario: MockScenario) -> (MockBackend, ManualClock) {
        let (cfg, clock) = MockConfig::headless(scenario);
        (MockBackend::new(cfg), clock)
    }

    fn nc() -> CancelToken {
        CancelToken::new()
    }

    #[test]
    fn the_endpoint_token_is_made_once_and_can_be_rotated() {
        let (m, _) = mock(MockScenario::demo());
        let first = m.mcp_token(&nc()).expect("a token");
        assert!(first.len() >= 32, "{first}: 128 bits of hex");
        assert_eq!(
            m.mcp_token(&nc()).expect("the same token"),
            first,
            "made once, so an agent keeps working across restarts"
        );
        assert!(
            m.has_secret(crate::mcp::TOKEN_SECRET, &nc())
                .expect("stored"),
            "it is kept in the app's own secret store"
        );
        let second = m.rotate_mcp_token(&nc()).expect("a new token");
        assert_ne!(second, first, "rotating replaces it");
        assert_eq!(
            m.mcp_token(&nc()).expect("the new token"),
            second,
            "and the new one is what is stored"
        );
    }

    #[test]
    fn the_files_of_a_run_are_listed_and_downloaded() {
        let (m, _) = mock(MockScenario::demo());
        let target = RuntimeRef {
            account: AccountId::new("mock1").unwrap(),
            name: Some("demo".into()),
            endpoint: "ep".into(),
        };
        let files = m
            .vm_ls(&target, "/content/out", &nc())
            .expect("the files are listed");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "/content/out/image.png");
        m.vm_download(&target, "/content/out/image.png", "/tmp/image.png", &nc())
            .expect("the file comes over");
        let downloads = m.w().downloads.clone();
        assert_eq!(downloads, ["/content/out/image.png -> /tmp/image.png"]);
    }

    #[test]
    fn reconciliation_precedes_usage_and_respects_listing_failures() {
        for guard in [false, true] {
            for per_account in [false, true] {
                let (m, _) = mock(MockScenario::demo());
                let (account, email) = {
                    let mut w = m.w();
                    let account = w.active.clone().unwrap();
                    let email = w.email(&account).unwrap();
                    let endpoint = w
                        .ledger
                        .entries()
                        .iter()
                        .find(|e| e.account == account)
                        .unwrap()
                        .endpoint
                        .clone()
                        .unwrap();
                    w.runtimes.retain(|r| r.endpoint != endpoint);
                    (account, email)
                };
                m.set_failures(MockFailures {
                    runtimes_listing_fails: !per_account,
                    unlisted_accounts: if per_account { vec![email] } else { vec![] },
                    ..Default::default()
                });
                let refresh = || {
                    if guard {
                        m.guard_poll(&nc()).unwrap().usage
                    } else {
                        m.refresh_usage(&[], &nc()).unwrap()
                    }
                };
                let views = refresh();
                let view = views.iter().find(|v| v.account == account).unwrap();
                assert!(!view.snapshot.unwrap().stale);
                assert!(!view.ownership_verified);
                assert_eq!(m.w().live_names(&account).len(), 2);
                m.set_failures(MockFailures::default());
                let views = refresh();
                let view = views.iter().find(|v| v.account == account).unwrap();
                assert!(view.ownership_verified);
                assert_eq!(view.unmanaged_runtimes, 1);
                assert_eq!(m.w().live_names(&account).len(), 1);
            }
        }
    }

    #[test]
    fn reconciliation_backfills_but_does_not_invent_missing_identities() {
        let (m, _) = mock(MockScenario::demo());
        let mut w = m.w();
        let account = w.active.clone().unwrap();
        let entry = w
            .ledger
            .entries()
            .iter()
            .find(|e| e.account == account)
            .unwrap()
            .clone();
        w.ledger
            .update(&account, &entry.name, |e| e.endpoint = None)
            .unwrap();
        assert!(w.reconcile(&account, m.now_()).0);
        assert_eq!(
            w.ledger.live(&account, &entry.name).unwrap().endpoint,
            entry.endpoint
        );
        w.ledger
            .update(&account, &entry.name, |e| e.endpoint = None)
            .unwrap();
        w.runtimes
            .retain(|r| r.endpoint != entry.endpoint.as_deref().unwrap());
        assert!(!w.reconcile(&account, m.now_()).0);
        assert!(w.ledger.live(&account, &entry.name).is_some());
    }

    #[test]
    fn a_session_without_an_endpoint_ends_once_no_runtime_could_be_it() {
        let (m, _) = mock(MockScenario::demo());
        let (active, other) = {
            let w = m.w();
            (w.accounts[0].id.clone(), w.accounts[1].id.clone())
        };
        let lose = |account: &AccountId, name: &str| {
            let mut w = m.w();
            let endpoint = w
                .ledger
                .live(account, name)
                .unwrap()
                .endpoint
                .clone()
                .unwrap();
            w.ledger
                .update(account, name, |e| e.endpoint = None)
                .unwrap();
            w.runtimes.retain(|r| r.endpoint != endpoint);
        };
        let verified = |views: &[AccountUsageView], account: &AccountId| {
            views
                .iter()
                .find(|v| &v.account == account)
                .unwrap()
                .ownership_verified
        };

        lose(&other, "cpu-0b5e");
        assert!(m.remove_account(&other, &nc()).is_err());
        let report = m.guard_poll(&nc()).unwrap();
        let cpu = RuntimeRef {
            account: other.clone(),
            endpoint: String::new(),
            name: Some("cpu-0b5e".into()),
        };
        assert_eq!(report.vanished, [cpu]);
        assert!(verified(&report.usage, &other));
        m.remove_account(&other, &nc()).unwrap();

        lose(&active, "t4-91c2");
        let views = m.refresh_usage(&[], &nc()).unwrap();
        assert!(m.w().ledger.live(&active, "t4-91c2").is_some());
        assert!(!verified(&views, &active), "the browser's L4 could be it");
        let browser = {
            let w = m.w();
            let r = w
                .runtimes
                .iter()
                .find(|r| r.account == active && r.name.is_none())
                .unwrap();
            RuntimeRef {
                account: active.clone(),
                endpoint: r.endpoint.clone(),
                name: None,
            }
        };
        m.stop_session(&browser, StopOptions::default(), &nc())
            .unwrap();
        let views = m.refresh_usage(&[], &nc()).unwrap();
        assert!(m.w().ledger.live(&active, "t4-91c2").is_none());
        assert!(verified(&views, &active));
    }

    #[test]
    fn a_guard_stop_without_an_endpoint_suspects_an_orphan_and_keeps_the_session() {
        let (m, clock) = mock(MockScenario::demo());
        let (account, endpoint, limit) = {
            let mut w = m.w();
            let account = w.active.clone().unwrap();
            let entry = w.ledger.live(&account, "t4-91c2").unwrap().clone();
            let endpoint = entry.endpoint.unwrap();
            w.ledger
                .update(&account, "t4-91c2", |e| e.endpoint = None)
                .unwrap();
            let runtime = w
                .runtimes
                .iter_mut()
                .find(|r| r.endpoint == endpoint)
                .unwrap();
            runtime.name = None;
            (account, endpoint, entry.policy.max_runtime.unwrap())
        };
        clock.advance(limit);
        let report = m.guard_poll(&nc()).unwrap();
        let decision = report
            .decisions
            .iter()
            .find(|d| d.session.name.as_deref() == Some("t4-91c2"))
            .unwrap();
        assert!(matches!(decision.decision, Decision::Stop(_)));
        assert!(!decision.stopped);
        let error = decision.error.as_ref().unwrap();
        assert!(error.is(ErrorKind::OrphanSuspected), "{error:?}");
        assert!(m.w().ledger.live(&account, "t4-91c2").is_some());
        let listed = m.runtimes(&nc()).unwrap().runtimes;
        assert!(listed.iter().any(|r| r.id.endpoint == endpoint));
    }

    #[test]
    fn ownership_without_live_ledger_entries_does_not_need_a_listing() {
        for stopped in [false, true] {
            let mut scenario = if stopped {
                MockScenario::demo()
            } else {
                MockScenario::single()
            };
            if !stopped {
                scenario
                    .runtimes
                    .push(MockScenario::demo().runtimes[2].clone());
            }
            let (m, _) = mock(scenario);
            let account = {
                let mut w = m.w();
                let account = w.active.clone().unwrap();
                let names = w.live_names(&account);
                for name in names {
                    w.ledger
                        .mark_stopped(&account, &name, m.now_(), StopReason::User)
                        .unwrap();
                }
                account
            };
            m.set_failures(MockFailures {
                runtimes_listing_fails: true,
                ..Default::default()
            });
            let views = m.refresh_usage(&[], &nc()).unwrap();
            let view = views.iter().find(|v| v.account == account).unwrap();
            assert!(view.ownership_verified);
            assert_eq!(view.unmanaged_runtimes, view.snapshot.unwrap().assignments);
            assert!(view.unmanaged_runtimes > 0);
        }
    }

    #[test]
    fn demo_lists_two_accounts_managed_unmanaged_and_other_account_runtimes() {
        let (m, _) = mock(MockScenario::demo());
        let v = m.runtimes(&nc()).unwrap();
        assert_eq!(v.runtimes.len(), 4);
        assert_eq!(
            v.runtimes
                .iter()
                .filter(|r| r.on_active_account && r.managed)
                .count(),
            2
        );
        let unmanaged = v.runtimes.iter().find(|r| !r.managed).unwrap();
        assert!(unmanaged.id.name.is_none());
        let other = v.runtimes.iter().find(|r| !r.on_active_account).unwrap();
        // Anything listed can be stopped, on any account.
        m.stop_session(&unmanaged.id, StopOptions::default(), &nc())
            .unwrap();
        m.stop_session(&other.id, StopOptions::default(), &nc())
            .unwrap();
        assert_eq!(m.runtimes(&nc()).unwrap().runtimes.len(), 2);
        // Exec-style operations on another account need a switch.
        let err = m
            .extend_session(&other.id, Duration::from_secs(60), &nc())
            .unwrap_err();
        assert!(err.is(ErrorKind::AccountNotActive));
        assert_eq!(err.next_step, crate::error::NextStep::SwitchAccount);
    }

    #[test]
    fn balance_burns_and_attribution_splits_two_sessions() {
        let (m, clock) = mock(MockScenario::demo());
        let first = m.refresh_usage(&[], &nc()).unwrap();
        let a = &first[0];
        let s = a.snapshot.unwrap();
        assert!((s.rate_cu_per_hour - (11.77 + 1.76 + 4.82)).abs() < 1e-9);
        assert_eq!(a.unmanaged_runtimes, 1, "the browser runtime is flagged");
        assert_eq!(a.session_rates.len(), 2);
        clock.advance(Duration::from_secs(3600));
        let later = m.refresh_usage(&[], &nc()).unwrap();
        assert!(later[0].snapshot.unwrap().balance_cu < s.balance_cu - 18.0);
        assert!(later[0].history.len() >= 2);
        assert!(later[0].eta.is_some());
        let f = MockFailures {
            usage_fetch_fails: true,
            ..Default::default()
        };
        m.set_failures(f);
        let stale = m.refresh_usage(&[], &nc()).unwrap();
        assert!(stale[0].snapshot.unwrap().stale);
    }

    #[test]
    fn a_runtime_alone_on_its_account_measures_its_hardware() {
        let (m, clock) = mock(MockScenario::single());
        let start = |accelerator| {
            let spec = ProvisionSpec {
                accelerator,
                min_vram_mib: None,
                ..m.defaults().provision_spec(m.new_session_name(None))
            };
            let (tx, _rx) = mpsc::channel();
            m.provision(spec, tx, &nc()).unwrap().name
        };
        let rate = |accelerator| {
            m.measured_rates()
                .into_iter()
                .find(|r| r.matches(accelerator, false))
                .map(|r| r.reading)
        };
        assert!(m.measured_rates().is_empty());
        let t4 = start(Accelerator::T4);
        m.refresh_usage(&[], &nc()).unwrap();
        let measured = rate(Accelerator::T4).unwrap();
        assert_eq!(measured.cu_per_hour, mock_rate(Accelerator::T4));
        // A guard round reads usage too, and keeps what it measured.
        clock.advance(Duration::from_secs(60));
        m.guard_poll(&nc()).unwrap();
        assert!(rate(Accelerator::T4).unwrap().at > measured.at);

        // With two runtimes the account rate is the sum: neither is measured.
        start(Accelerator::A100);
        let before = rate(Accelerator::T4);
        clock.advance(Duration::from_secs(60));
        m.guard_poll(&nc()).unwrap();
        assert_eq!(rate(Accelerator::T4), before);
        assert_eq!(rate(Accelerator::A100), None);

        // The stopped session's reading stays; the one left alone is measured.
        let t4 = m
            .runtimes(&nc())
            .unwrap()
            .runtimes
            .into_iter()
            .find(|r| r.id.name.as_deref() == Some(&t4))
            .unwrap();
        m.stop_session(&t4.id, StopOptions::default(), &nc())
            .unwrap();
        m.guard_poll(&nc()).unwrap();
        assert_eq!(rate(Accelerator::T4), before);
        assert_eq!(
            rate(Accelerator::A100).unwrap().cu_per_hour,
            mock_rate(Accelerator::A100)
        );
    }

    #[test]
    fn provision_retries_to_80_gb_and_respects_the_session_cap() {
        let (m, _) = mock(MockScenario::single());
        let mut spec = m.defaults().provision_spec(m.new_session_name(None));
        spec.min_vram_mib = Some(70_000);
        let (tx, rx) = mpsc::channel();
        let out = m.provision(spec, tx, &nc()).unwrap();
        assert_eq!(out.attempts, 2);
        assert_eq!(out.hardware.vram_mib, Some(81_920));
        assert!(
            rx.try_iter()
                .any(|e| matches!(e, ProvisionEvent::Rejected { .. }))
        );
        let spec2 = m.defaults().provision_spec(m.new_session_name(Some("t4")));
        let (tx, _rx) = mpsc::channel();
        m.provision(
            ProvisionSpec {
                accelerator: Accelerator::T4,
                ..spec2
            },
            tx,
            &nc(),
        )
        .unwrap();
        let spec3 = m.defaults().provision_spec(m.new_session_name(None));
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.provision(spec3, tx, &nc())
                .unwrap_err()
                .is(ErrorKind::SessionLimitReached)
        );
    }

    #[test]
    fn guard_warns_then_stops_as_usage_burns() {
        let (m, clock) = mock(MockScenario::single());
        let mut d = m.defaults();
        d.max_spend_cu = Some(10.0);
        d.max_runtime = None;
        m.set_defaults(d);
        let spec = ProvisionSpec {
            accelerator: Accelerator::H100,
            ..m.defaults().provision_spec("h100-test")
        };
        let (tx, _rx) = mpsc::channel();
        m.provision(spec, tx, &nc()).unwrap();
        m.guard_poll(&nc()).unwrap();
        clock.advance(Duration::from_secs(27 * 60)); // ≈ 8.2 CU at 18.3 CU/h
        let r = m.guard_poll(&nc()).unwrap();
        assert!(
            matches!(r.decisions[0].decision, Decision::Warn(_)),
            "{:?}",
            r.decisions
        );
        clock.advance(Duration::from_secs(10 * 60));
        let r = m.guard_poll(&nc()).unwrap();
        assert!(r.decisions[0].stopped, "{:?}", r.decisions);
        assert!(m.runtimes(&nc()).unwrap().runtimes.is_empty());
    }

    #[test]
    fn login_flows_and_failures() {
        let (m, _) = mock(MockScenario::single());
        let flow = m.begin_add_account(&nc()).unwrap();
        assert!(flow.auth_url().starts_with("https://accounts.google.com/"));
        let acct = flow.submit_code("4/0Abc", &nc()).unwrap();
        assert_eq!(m.accounts(&nc()).unwrap().active, Some(acct.id));
        let mut f = MockFailures {
            login: Some(MockLoginFailure::InvalidCode),
            ..Default::default()
        };
        m.set_failures(f.clone());
        assert!(
            m.begin_add_account(&nc())
                .unwrap()
                .submit_code("bad", &nc())
                .unwrap_err()
                .is(ErrorKind::LoginInvalidCode)
        );
        f.login = Some(MockLoginFailure::CancelledBeforeUrl);
        m.set_failures(f);
        assert!(
            m.begin_add_account(&nc())
                .err()
                .unwrap()
                .is(ErrorKind::LoginCancelled)
        );
    }

    #[test]
    fn cli_missing_install_and_failures() {
        let (cfg, _) = MockConfig::headless(MockScenario::signed_out());
        let mut cfg = cfg;
        cfg.failures.cli_missing = true;
        cfg.failures.install_fails = true;
        let m = MockBackend::new(cfg);
        assert!(m.accounts(&nc()).unwrap_err().is(ErrorKind::NotInstalled));
        assert!(m.probe(&nc()).unwrap().colab.is_none());
        let (tx, rx) = mpsc::channel();
        assert!(
            m.install_cli(InstallMethod::Uv, tx, &nc())
                .unwrap_err()
                .is(ErrorKind::InstallFailed)
        );
        assert!(rx.try_iter().count() >= 4);
        m.set_failures(MockFailures::default());
        let (tx, _rx) = mpsc::channel();
        m.install_cli(InstallMethod::Uv, tx, &nc()).unwrap();
        let v = m.accounts(&nc()).unwrap();
        assert!(v.accounts.is_empty() && v.active.is_none());
    }

    /// How long [`HeldRetry`] holds on its own, and how long the test waits for an event in
    /// that time: well inside it, so nothing sent after the hold can be taken for one.
    const HELD_AT_MOST: Duration = Duration::from_secs(60);
    const TOLD_WITHIN: Duration = Duration::from_secs(10);

    /// A clock that holds the wait before a retry until the test lets it go.
    struct HeldRetry {
        inner: ManualClock,
        held: Duration,
        waiting: Mutex<mpsc::Sender<()>>,
        go: Mutex<mpsc::Receiver<()>>,
    }

    impl Clock for HeldRetry {
        fn now(&self) -> SystemTime {
            self.inner.now()
        }

        fn sleep(&self, duration: Duration, cancel: &CancelToken) -> Result<()> {
            if duration == self.held {
                let _ = self.waiting.lock().unwrap().send(());
                // Never forever: the test lets it go, or this gives up.
                let _ = self.go.lock().unwrap().recv_timeout(HELD_AT_MOST);
            }
            self.inner.sleep(duration, cancel)
        }
    }

    #[test]
    fn a_server_run_reports_ready_and_keeps_running() {
        let (m, _) = mock(MockScenario::single());
        let spec = m.plugin_run_template("comfyui").expect("the plugin");
        assert!(spec.keep_alive, "the ComfyUI plugin is a server");
        let (tx, rx) = mpsc::channel();
        let cancel = CancelToken::new();
        std::thread::scope(|scope| {
            let run = scope.spawn(|| m.run_plugin(spec, tx, &cancel));
            let mut ready = false;
            let until = std::time::Instant::now() + TOLD_WITHIN;
            while std::time::Instant::now() < until {
                let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) else {
                    continue;
                };
                if let PluginRunEvent::Status(status) = event
                    && status.state == RunState::Ready
                {
                    ready = true;
                    break;
                }
            }
            assert!(ready, "the server reports ready");
            assert!(!run.is_finished(), "and keeps running");
            cancel.cancel();
            assert!(
                run.join().unwrap().is_err(),
                "the run ends when it is cancelled"
            );
        });
    }

    #[test]
    fn a_plugin_run_reports_its_provisioning_while_it_happens() {
        let (cfg, clock) = MockConfig::headless(MockScenario::single());
        let held = Duration::from_secs(77);
        let (waiting, at_the_wait) = mpsc::channel();
        let (go, let_go) = mpsc::channel();
        let m = MockBackend::new(MockConfig {
            clock: Arc::new(HeldRetry {
                inner: clock,
                held,
                waiting: Mutex::new(waiting),
                go: Mutex::new(let_go),
            }),
            ..cfg
        });
        let mut spec = m.plugin_run_template("llama-cpp").unwrap();
        spec.inputs.insert(
            "repo".into(),
            crate::plugins::InputValue::Text("bartowski/ScarletKc_ScarletKc-2.0-GGUF".into()),
        );
        if let RunTarget::Provision(p) = &mut spec.target {
            p.min_vram_mib = Some(70_000);
            p.retry_delay = held;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let run = scope.spawn(|| m.run_plugin(spec, tx, &nc()));
            at_the_wait
                .recv_timeout(TOLD_WITHIN)
                .expect("the first GPU is too small, so the run waits to ask again");
            // Still waiting to ask again, and what happened so far is already out.
            let until = std::time::Instant::now() + TOLD_WITHIN;
            let next = || {
                let left = until.saturating_duration_since(std::time::Instant::now());
                rx.recv_timeout(left).ok()
            };
            let told = std::iter::from_fn(next).any(|event| {
                matches!(
                    event,
                    PluginRunEvent::Provision(ProvisionEvent::Retrying { .. })
                )
            });
            go.send(()).unwrap();
            assert!(told, "the retry is only reported once provisioning is over");
            assert_eq!(run.join().unwrap().unwrap().status.state, RunState::Ready);
        });
    }

    #[test]
    fn quick_deploy_reaches_a_ready_endpoint_and_tunnel() {
        let (m, _) = mock(MockScenario::single());
        let mut spec = m.plugin_run_template("llama-cpp").unwrap();
        spec.inputs.insert(
            "repo".into(),
            crate::plugins::InputValue::Text("bartowski/ScarletKc_ScarletKc-2.0-GGUF".into()),
        );
        if let RunTarget::Provision(p) = &mut spec.target {
            p.min_vram_mib = Some(70_000);
        }
        let (tx, rx) = mpsc::channel();
        let out = m.run_plugin(spec, tx, &nc()).unwrap();
        assert_eq!(out.status.state, RunState::Ready);
        assert_eq!(
            out.status.outputs["quant"], "Q8_0",
            "80 GB A100 after a retry gets Q8_0"
        );
        assert!(rx.try_iter().any(|e| matches!(
            e,
            PluginRunEvent::Provision(ProvisionEvent::Retrying { .. })
        )));
        let ep = &out.endpoints[0];
        let (tx, trx) = mpsc::channel();
        let id = m
            .open_tunnel(
                TunnelSpec {
                    account: out.account.clone(),
                    session: out.session.clone(),
                    local_port: ep.remote_port,
                    remote_port: ep.remote_port,
                    health_path: ep.health.clone(),
                },
                tx,
            )
            .unwrap();
        assert!(trx.try_iter().any(|e| matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Up,
                ..
            }
        )));
        assert_eq!(m.tunnels()[0].id, id);
    }

    #[test]
    fn plugin_failure_at_a_stage_stops_the_fresh_session_and_429_is_reported() {
        let (cfg, _) = MockConfig::headless(MockScenario::single());
        let mut cfg = cfg;
        cfg.failures.plugin_fail_stage = Some("download".into());
        cfg.failures.ssh_busy_attempts = 1;
        let m = MockBackend::new(cfg);
        let mut spec = m.plugin_run_template("llama-cpp").unwrap();
        spec.inputs.insert(
            "repo".into(),
            crate::plugins::InputValue::Text("unsloth/Qwen3-0.6B-GGUF".into()),
        );
        let (tx, _rx) = mpsc::channel();
        let err = m.run_plugin(spec, tx, &nc()).unwrap_err();
        assert!(err.is(ErrorKind::PluginRunFailed));
        assert_eq!(err.param("stage"), Some("download"));
        assert!(
            m.runtimes(&nc()).unwrap().runtimes.is_empty(),
            "no billed VM left behind"
        );
        // 429 on the first tunnel attempt.
        let spec = m.defaults().provision_spec("t");
        let (tx, _rx) = mpsc::channel();
        m.provision(
            ProvisionSpec {
                accelerator: Accelerator::T4,
                ..spec
            },
            tx,
            &nc(),
        )
        .unwrap();
        let acct = m.accounts(&nc()).unwrap().active.unwrap();
        let (tx, trx) = mpsc::channel();
        m.open_tunnel(
            TunnelSpec {
                account: acct,
                session: "t".into(),
                local_port: 18001,
                remote_port: 8000,
                health_path: None,
            },
            tx,
        )
        .unwrap();
        assert!(trx.try_iter().any(|e| matches!(e, TunnelEvent::State { state: TunnelState::Failed { ref error }, .. } if error.is(ErrorKind::SshBusy))));
    }

    fn write_fixture(dir: &std::path::Path, toml: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), toml).unwrap();
        std::fs::write(dir.join("main.py"), "print('hi')\n").unwrap();
        std::fs::write(dir.join("requirements.txt"), "torch\n").unwrap();
    }

    #[test]
    fn a_job_stops_the_runtime_it_opened_after_keep_and_a_failure_can_keep_it() {
        let (m, _) = mock(MockScenario::single());
        let dir = tempfile::tempdir().unwrap();
        write_fixture(
            dir.path(),
            r#"
api = 2
id = "trainjob"
version = "0.1.0"
needs = ["gcp_auth"]
[name]
en = "Train"
[run]
script = "main.py"
mode = "job"
keep_on_failure = true
[[inputs]]
id = "data"
type = "file"
required = true
[[secrets]]
id = "civitai_token"
required = true
[secrets.label]
en = "Civitai"
[[assets]]
path = "requirements.txt"
[[stages]]
id = "fit"
[[keep]]
path = "/content/out"
"#,
        );
        let preview = m.inspect_plugin_folder(dir.path()).unwrap();
        m.install_plugin(&preview).unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "rows").unwrap();
        let mut spec = m.plugin_run_template("trainjob").unwrap();
        spec.inputs.insert(
            "data".into(),
            crate::plugins::InputValue::Text(file.display().to_string()),
        );
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.run_plugin(spec.clone(), tx, &nc())
                .unwrap_err()
                .is(ErrorKind::InvalidInput),
            "gcp auth needs an explicit yes"
        );
        assert!(m.runtimes(&nc()).unwrap().runtimes.is_empty());
        spec.consent.gcp_auth = true;
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.run_plugin(spec.clone(), tx, &nc())
                .unwrap_err()
                .is(ErrorKind::InvalidInput),
            "a required secret slot must be saved first"
        );
        assert!(m.runtimes(&nc()).unwrap().runtimes.is_empty());
        m.set_secret("p_trainjob_civitai_token", "sek-civitai-token", &nc())
            .unwrap();
        let keep_dir = tempfile::tempdir().unwrap();
        spec.keep_plan = Some(crate::keep::KeepPlan {
            paths: vec!["/content/out".into()],
            local: Some(crate::keep::LocalKeepTarget {
                folder: keep_dir.path().display().to_string(),
            }),
            ..Default::default()
        });
        let (tx, rx) = mpsc::channel();
        let out = m.run_plugin(spec, tx, &nc()).unwrap();
        assert_eq!(out.status.state, RunState::Ready);
        assert_eq!(out.status.metrics.step, Some(3.0));
        assert_eq!(out.status.metrics.loss, Some(0.42));
        assert_eq!(out.status.metrics.tok_per_s, Some(12.5));
        assert!(
            m.runtimes(&nc()).unwrap().runtimes.is_empty(),
            "a finished job stops the runtime it opened"
        );
        assert!(
            m.w()
                .ledger
                .entries()
                .iter()
                .any(|entry| entry.name == out.session && entry.last_keep_sync.is_some()),
            "the Keep Plan runs before that stop"
        );
        let logs: Vec<String> = rx
            .try_iter()
            .filter_map(|event| match event {
                PluginRunEvent::Log { lines } => Some(lines.join("\n")),
                _ => None,
            })
            .collect();
        assert!(logs.iter().all(|line| !line.contains("sek-civitai")));
        assert!(logs.iter().any(|line| line.contains("***")));

        m.set_failures(MockFailures {
            plugin_fail_stage: Some("fit".into()),
            ..MockFailures::default()
        });
        let mut again = m.plugin_run_template("trainjob").unwrap();
        again.consent.gcp_auth = true;
        again.inputs.insert(
            "data".into(),
            crate::plugins::InputValue::Text(file.display().to_string()),
        );
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.run_plugin(again, tx, &nc())
                .unwrap_err()
                .is(ErrorKind::PluginRunFailed)
        );
        assert_eq!(
            m.runtimes(&nc()).unwrap().runtimes.len(),
            1,
            "keep_on_failure leaves the runtime up"
        );
    }

    #[test]
    fn a_service_uploads_files_without_putting_the_local_path_in_the_environment() {
        let (m, _) = mock(MockScenario::single());
        let dir = tempfile::tempdir().unwrap();
        write_fixture(
            dir.path(),
            r#"
api = 1
id = "copyin"
version = "0.1.0"
[name]
en = "Copy"
[run]
script = "main.py"
[[inputs]]
id = "data"
type = "file"
required = true
[[assets]]
path = "requirements.txt"
[[stages]]
id = "copy"
"#,
        );
        let preview = m.inspect_plugin_folder(dir.path()).unwrap();
        m.install_plugin(&preview).unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "rows").unwrap();
        let local = file.display().to_string();
        let mut spec = m.plugin_run_template("copyin").unwrap();
        spec.inputs.insert(
            "data".into(),
            crate::plugins::InputValue::Text(local.clone()),
        );
        let (tx, _rx) = mpsc::channel();
        let out = m.run_plugin(spec, tx, &nc()).unwrap();
        assert_eq!(out.status.state, RunState::Ready);
        let uploads = m
            .w()
            .runtimes
            .iter()
            .find(|runtime| runtime.name.as_deref() == Some(out.session.as_str()))
            .expect("a service leaves its runtime up")
            .uploads
            .clone();
        assert!(
            uploads
                .iter()
                .any(|line| line.contains("inputs/data/notes.txt"))
        );
        assert!(
            uploads
                .iter()
                .any(|line| line.contains("assets/requirements.txt"))
        );
        let env = m.w().last_plugin_env.clone();
        assert!(env.iter().any(|(key, value)| {
            key == "PLUGIN_INPUT_DATA"
                && value.ends_with("inputs/data/notes.txt")
                && !value.contains(&local)
        }));
        assert!(env.iter().all(|(_, value)| !value.contains(&local)));
    }

    #[test]
    fn switch_impact_lists_tunnels_and_sessions() {
        let (m, _) = mock(MockScenario::demo());
        let accts = m.accounts(&nc()).unwrap();
        let other = accts.accounts[1].id.clone();
        let (tx, _rx) = mpsc::channel();
        m.open_tunnel(
            TunnelSpec {
                account: accts.accounts[0].id.clone(),
                session: "a100-7f3a".into(),
                local_port: 18000,
                remote_port: 8000,
                health_path: None,
            },
            tx,
        )
        .unwrap();
        let impact = m.switch_impact(&other).unwrap();
        assert_eq!(impact.tunnels_closing.len(), 1);
        assert_eq!(impact.sessions_left_running.len(), 2);
        m.switch_account(&other, &nc()).unwrap();
        assert!(m.tunnels().is_empty());
    }

    #[test]
    fn pending_add_clears_the_slot_and_every_non_success_restores_it() {
        let (m, _) = mock(MockScenario::demo());
        let original = m.accounts(&nc()).unwrap().active.unwrap();
        // While a flow is alive the slot is empty; cancel restores it.
        let flow = m.begin_add_account(&nc()).unwrap();
        let mid = m.accounts(&nc()).unwrap();
        assert_eq!(mid.active, None);
        assert!(!mid.has_token && !mid.recovered_pending_add);
        assert!(
            m.begin_add_account(&nc())
                .err()
                .unwrap()
                .is(ErrorKind::AddInProgress)
        );
        flow.cancel().unwrap();
        assert_eq!(m.accounts(&nc()).unwrap().active.as_ref(), Some(&original));
        // Dropping the flow restores it too.
        drop(m.begin_add_account(&nc()).unwrap());
        assert_eq!(m.accounts(&nc()).unwrap().active.as_ref(), Some(&original));
        // A rejected code restores it.
        m.set_failures(MockFailures {
            login: Some(MockLoginFailure::InvalidCode),
            ..Default::default()
        });
        let err = m
            .begin_add_account(&nc())
            .unwrap()
            .submit_code("x", &nc())
            .unwrap_err();
        assert!(err.is(ErrorKind::LoginInvalidCode));
        assert_eq!(m.accounts(&nc()).unwrap().active.as_ref(), Some(&original));
        // Success replaces it.
        m.set_failures(MockFailures::default());
        let added = m
            .begin_add_account(&nc())
            .unwrap()
            .submit_code("4/ok", &nc())
            .unwrap();
        let v = m.accounts(&nc()).unwrap();
        assert_eq!(v.active, Some(added.id));
        assert_eq!(v.accounts.len(), 3);
    }

    #[test]
    fn unadopted_slot_and_interrupted_add_scenarios() {
        let (cfg, _) = MockConfig::headless(MockScenario::single());
        let mut cfg = cfg;
        cfg.failures.unadopted_slot = true;
        let m = MockBackend::new(cfg);
        let v = m.accounts(&nc()).unwrap();
        assert!(v.unadopted && v.has_token && v.active.is_none());
        assert_eq!(v.active_email.as_deref(), Some(MOCK_UNADOPTED_EMAIL));
        let adopted = m.adopt_active_account(&nc()).unwrap();
        assert_eq!(adopted.email.as_deref(), Some(MOCK_UNADOPTED_EMAIL));
        let v = m.accounts(&nc()).unwrap();
        assert!(!v.unadopted && v.active == Some(adopted.id.clone()));

        // An interrupted add is rolled back on the next accounts() and reported once.
        m.set_failures(MockFailures {
            interrupted_add: true,
            ..Default::default()
        });
        let v = m.accounts(&nc()).unwrap();
        assert!(v.recovered_pending_add);
        assert_eq!(v.active, Some(adopted.id.clone()));
        assert!(!m.accounts(&nc()).unwrap().recovered_pending_add);

        let (cfg, _) = MockConfig::headless(MockScenario::single());
        let mut cfg = cfg;
        cfg.failures.interrupted_add = true;
        let m = MockBackend::new(cfg);
        let v = m.accounts(&nc()).unwrap();
        assert!(v.recovered_pending_add && v.active.is_some());
    }

    #[test]
    fn every_failure_switch_flips_at_runtime() {
        let (m, _) = mock(MockScenario::single());
        let on = |f: MockFailures| m.set_failures(f);
        let off = || m.set_failures(MockFailures::default());
        let d = MockFailures::default;

        on(MockFailures {
            cli_missing: true,
            ..d()
        });
        assert!(m.accounts(&nc()).unwrap_err().is(ErrorKind::NotInstalled));
        assert!(m.probe(&nc()).unwrap().colab.is_none());
        off();
        assert!(m.accounts(&nc()).is_ok());
        assert!(m.probe(&nc()).unwrap().colab.is_some());

        on(MockFailures {
            install_fails: true,
            ..d()
        });
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.install_cli(InstallMethod::Uv, tx, &nc())
                .unwrap_err()
                .is(ErrorKind::InstallFailed)
        );
        off();

        on(MockFailures {
            keyring_unavailable: true,
            ..d()
        });
        assert!(
            m.accounts(&nc())
                .unwrap_err()
                .is(ErrorKind::KeyringUnavailable)
        );
        assert!(!m.probe(&nc()).unwrap().keyring_ok);
        off();

        on(MockFailures {
            usage_fetch_fails: true,
            ..d()
        });
        m.refresh_usage(&[], &nc()).unwrap();
        off();
        assert!(
            !m.refresh_usage(&[], &nc()).unwrap()[0]
                .snapshot
                .unwrap()
                .stale
        );
        on(MockFailures {
            usage_fetch_fails: true,
            ..d()
        });
        assert!(
            m.refresh_usage(&[], &nc()).unwrap()[0]
                .snapshot
                .unwrap()
                .stale
        );
        off();

        on(MockFailures {
            login: Some(MockLoginFailure::VerificationFailed),
            ..d()
        });
        let err = m
            .begin_add_account(&nc())
            .unwrap()
            .submit_code("c", &nc())
            .unwrap_err();
        assert!(err.is(ErrorKind::LoginVerificationFailed));
        off();

        on(MockFailures {
            not_entitled: vec![Accelerator::T4],
            ..d()
        });
        let spec = ProvisionSpec {
            accelerator: Accelerator::T4,
            ..m.defaults().provision_spec("t4-x")
        };
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.provision(spec.clone(), tx, &nc())
                .unwrap_err()
                .is(ErrorKind::NotEntitled)
        );
        off();

        on(MockFailures {
            allocation_hangs: vec![Accelerator::T4],
            ..d()
        });
        let (tx, _rx) = mpsc::channel();
        assert!(
            m.provision(spec.clone(), tx, &nc())
                .unwrap_err()
                .is(ErrorKind::AllocationTimeout)
        );
        assert!(m.runtimes(&nc()).unwrap().runtimes.is_empty());
        off();
        let (tx, _rx) = mpsc::channel();
        m.provision(spec, tx, &nc()).unwrap();

        let acct = m.accounts(&nc()).unwrap().active.unwrap();
        let tspec = |port| TunnelSpec {
            account: acct.clone(),
            session: "t4-x".into(),
            local_port: port,
            remote_port: 8000,
            health_path: None,
        };
        on(MockFailures {
            ssh_busy_attempts: u32::MAX,
            ..d()
        });
        let (tx, trx) = mpsc::channel();
        m.open_tunnel(tspec(18100), tx).unwrap();
        assert!(trx.try_iter().any(|e| matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Failed { .. },
                ..
            }
        )));
        off();
        let (tx, trx) = mpsc::channel();
        m.open_tunnel(tspec(18101), tx).unwrap();
        assert!(trx.try_iter().any(|e| matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Up,
                ..
            }
        )));

        on(MockFailures {
            plugin_fail_stage: Some("plan".into()),
            ..d()
        });
        let mut run = m.plugin_run_template("llama-cpp").unwrap();
        run.inputs.insert(
            "repo".into(),
            crate::plugins::InputValue::Text("unsloth/Qwen3-0.6B-GGUF".into()),
        );
        run.target = RunTarget::Existing {
            name: "t4-x".into(),
        };
        let (tx, _rx) = mpsc::channel();
        assert_eq!(
            m.run_plugin(run.clone(), tx, &nc())
                .unwrap_err()
                .param("stage"),
            Some("plan")
        );
        off();
        let (tx, _rx) = mpsc::channel();
        assert_eq!(
            m.run_plugin(run, tx, &nc()).unwrap().status.state,
            RunState::Ready
        );

        // The A100 sequence restarts when it changes.
        on(MockFailures {
            a100_vram_sequence: vec![81_920],
            ..d()
        });
        m.stop_session(
            &m.runtimes(&nc()).unwrap().runtimes[0].id,
            StopOptions::default(),
            &nc(),
        )
        .unwrap();
        let (tx, _rx) = mpsc::channel();
        let out = m
            .provision(m.defaults().provision_spec("big"), tx, &nc())
            .unwrap();
        assert_eq!(out.hardware.vram_mib, Some(81_920));

        // A cancelled watchdog cannot be started again while the switch is on.
        let big = m
            .runtimes(&nc())
            .unwrap()
            .runtimes
            .into_iter()
            .find(|r| r.id.name.as_deref() == Some("big"))
            .unwrap()
            .id;
        let hour = Duration::from_secs(3600);
        m.cancel_auto_stop(&big, &nc()).unwrap();
        on(MockFailures {
            watchdog_restart_fails: true,
            ..d()
        });
        assert!(!m.set_auto_stop(&big, hour, &nc()).unwrap().armed);
        off();
        assert!(m.set_auto_stop(&big, hour, &nc()).unwrap().armed);
    }

    fn tracked_watchdog(m: &MockBackend, name: &str) -> (Runtime, WatchdogInfo) {
        let rt = m
            .runtimes(&nc())
            .unwrap()
            .runtimes
            .into_iter()
            .find(|r| r.id.name.as_deref() == Some(name))
            .unwrap();
        let wd = rt.tracked.as_ref().unwrap().watchdog.clone().unwrap();
        (rt, wd)
    }

    #[test]
    fn setting_an_auto_stop_after_a_cancel_starts_the_watchdog_again() {
        let (m, _) = mock(MockScenario::single());
        let mut d = m.defaults();
        d.idle_timeout = Some(Duration::from_secs(900));
        m.set_defaults(d);
        let (tx, _rx) = mpsc::channel();
        let spec = ProvisionSpec {
            accelerator: Accelerator::T4,
            ..m.defaults().provision_spec("t4-wd")
        };
        m.provision(spec, tx, &nc()).unwrap();
        let (rt, wd) = tracked_watchdog(&m, "t4-wd");
        assert!(wd.armed && !wd.cancelled && wd.deadline.is_some());
        assert!(rt.auto_stop_at.is_some());

        m.cancel_auto_stop(&rt.id, &nc()).unwrap();
        let (rt, wd) = tracked_watchdog(&m, "t4-wd");
        assert!(!wd.armed && wd.cancelled);
        assert_eq!(rt.auto_stop_at, None, "it bills until stopped");

        // "Set auto-stop": the watchdog runs on the runtime again.
        let hour = Duration::from_secs(3600);
        let info = m.set_auto_stop(&rt.id, hour, &nc()).unwrap();
        assert!(info.armed && !info.cancelled);
        assert_eq!(info.idle_timeout, Some(Duration::from_secs(900)));
        let (rt, wd) = tracked_watchdog(&m, "t4-wd");
        assert_eq!(wd, info, "the listing shows the last known state");
        assert_eq!(rt.auto_stop_at, info.deadline);

        // The restart fails: the call reports it and keeps the app-side limit.
        m.cancel_auto_stop(&rt.id, &nc()).unwrap();
        m.set_failures(MockFailures {
            watchdog_restart_fails: true,
            ..MockFailures::default()
        });
        let before = m.now();
        let info = m.set_auto_stop(&rt.id, hour, &nc()).unwrap();
        assert!(!info.armed && !info.cancelled);
        assert_eq!(info.deadline, None);
        assert_eq!(info.idle_timeout, Some(Duration::from_secs(900)));
        let (rt, wd) = tracked_watchdog(&m, "t4-wd");
        assert_eq!(wd, info);
        let tracked = rt.tracked.as_ref().unwrap();
        let app_stop = tracked.started_at() + tracked.policy.max_runtime.unwrap();
        assert!(
            app_stop >= before + hour,
            "the app-side limit is the new time"
        );
        assert_eq!(rt.auto_stop_at, Some(app_stop));

        // Trying again once it can start works; after that the running
        // watchdog is only moved, so the switch no longer matters.
        m.set_failures(MockFailures::default());
        assert!(m.set_auto_stop(&rt.id, hour, &nc()).unwrap().armed);
        m.set_failures(MockFailures {
            watchdog_restart_fails: true,
            ..MockFailures::default()
        });
        let info = m.set_auto_stop(&rt.id, hour * 2, &nc()).unwrap();
        assert!(info.armed);
        assert_eq!(info.deadline, Some(m.now() + hour * 2));
    }

    #[test]
    fn extending_adds_to_the_stop_the_session_has_at_that_moment() {
        let (m, clock) = mock(MockScenario::single());
        let (tx, _rx) = mpsc::channel();
        let spec = ProvisionSpec {
            accelerator: Accelerator::T4,
            ..m.defaults().provision_spec("t4-ext")
        };
        m.provision(spec, tx, &nc()).unwrap();
        let (rt, _) = tracked_watchdog(&m, "t4-ext");
        let first_stop = rt.auto_stop_at.unwrap();
        let hour = Duration::from_secs(3600);
        let left = |m: &MockBackend, stop: SystemTime| stop.duration_since(m.now()).unwrap();

        // Two in a row, with no listing in between: the second adds to the first.
        let set = m.extend_session(&rt.id, hour, &nc()).unwrap();
        assert_eq!(set.watchdog.deadline, Some(first_stop + hour));
        assert_eq!(set.runtime, left(&m, first_stop + hour));
        let set = m.extend_session(&rt.id, hour, &nc()).unwrap();
        assert_eq!(set.watchdog.deadline, Some(first_stop + 2 * hour));
        assert_eq!(set.runtime, left(&m, first_stop + 2 * hour));
        let (rt, wd) = tracked_watchdog(&m, "t4-ext");
        assert_eq!(wd, set.watchdog);
        assert_eq!(rt.auto_stop_at, Some(first_stop + 2 * hour));

        // Setting an auto-stop counts from now, whatever time was left.
        let info = m.set_auto_stop(&rt.id, hour, &nc()).unwrap();
        let set_stop = m.now() + hour;
        assert_eq!(info.deadline, Some(set_stop));
        clock.advance(hour / 2);
        let set = m.extend_session(&rt.id, hour, &nc()).unwrap();
        assert_eq!(set.watchdog.deadline, Some(set_stop + hour));

        // A stop that has passed, or none at all, leaves nothing to add to.
        clock.advance(3 * hour);
        let set = m.extend_session(&rt.id, hour, &nc()).unwrap();
        assert_eq!(set.runtime, hour);
        assert_eq!(set.watchdog.deadline, Some(m.now() + hour));
        m.cancel_auto_stop(&rt.id, &nc()).unwrap();
        let set = m.extend_session(&rt.id, hour, &nc()).unwrap();
        assert_eq!(set.runtime, hour);
        assert_eq!(set.watchdog.deadline, Some(m.now() + hour));
    }

    #[test]
    fn switch_impact_reports_unmanaged_runtimes_left_running() {
        let (m, _) = mock(MockScenario::demo());
        let accts = m.accounts(&nc()).unwrap();
        let impact = m.switch_impact(&accts.accounts[1].id).unwrap();
        let unmanaged = impact.unmanaged_left_running.unwrap();
        assert_eq!(unmanaged.len(), 1);
        assert_eq!(unmanaged[0].account, accts.accounts[0].id);
        assert!(unmanaged[0].name.is_none());
        let same = m.switch_impact(&accts.accounts[0].id).unwrap();
        assert_eq!(same.unmanaged_left_running, Some(vec![]));
    }

    #[test]
    fn switch_impact_never_reports_unlisted_runtimes_as_none() {
        let idle = |email: &str| MockAccount {
            email: email.into(),
            balance_cu: 100.0,
        };
        let (m, _) = mock(MockScenario {
            accounts: vec![idle("ada@example.com"), idle("lin@example.com")],
            runtimes: vec![],
        });
        let accts = m.accounts(&nc()).unwrap();
        let (ada, lin) = (&accts.accounts[0].id, &accts.accounts[1].id);
        let left = |to: &AccountId| m.switch_impact(to).unwrap().unmanaged_left_running;
        m.refresh_usage(&[], &nc()).unwrap();
        assert_eq!(left(lin), Some(vec![]), "listed, and nothing runs");

        m.set_failures(MockFailures {
            unlisted_accounts: vec!["ada@example.com".into()],
            ..MockFailures::default()
        });
        assert_eq!(left(lin), None, "a usage reading of zero is not a list");
        assert_eq!(left(ada), Some(vec![]), "staying leaves nothing behind");

        // Usage that shows the runtime does not stand in for the list either.
        let endpoint = m.add_unmanaged_runtime(ada, Accelerator::T4);
        m.refresh_usage(&[], &nc()).unwrap();
        assert_eq!(m.usage()[0].unmanaged_runtimes, 1);
        assert_eq!(left(lin), None);
        m.set_failures(MockFailures {
            runtimes_listing_fails: true,
            ..MockFailures::default()
        });
        assert_eq!(left(lin), None);

        m.set_failures(MockFailures::default());
        let listed = left(lin).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].endpoint, endpoint);
    }

    #[test]
    fn listing_failure_switches() {
        let (m, _) = mock(MockScenario::demo());
        m.set_failures(MockFailures {
            unlisted_accounts: vec!["lin@example.com".into()],
            ..MockFailures::default()
        });
        let v = m.runtimes(&nc()).unwrap();
        assert_eq!(v.errors.len(), 1, "the unlisted account is reported");
        assert!(
            v.runtimes
                .iter()
                .all(|r| r.account_email.as_deref() != Some("lin@example.com")),
            "and its runtimes are left out"
        );
        assert_eq!(v.runtimes.len(), 3);
        m.set_failures(MockFailures {
            runtimes_listing_fails: true,
            ..MockFailures::default()
        });
        assert!(m.runtimes(&nc()).is_err());
    }

    #[test]
    fn cancelling_provisioning_leaves_no_vm() {
        // Real clock with short steps, so the asynchronous cancel lands during
        // the retry wait (a manual clock would finish before it arrives).
        let mut cfg = MockConfig::interactive(Duration::from_millis(20));
        cfg.scenario = MockScenario::single();
        let m = MockBackend::new(cfg);
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        let (tx, rx) = mpsc::channel();
        let spec = ProvisionSpec {
            min_vram_mib: Some(70_000),
            retry_delay: Duration::from_secs(30),
            ..m.defaults().provision_spec("x")
        };
        // Cancel as soon as the first rejection is reported.
        let h = std::thread::spawn(move || {
            for e in rx {
                if matches!(e, ProvisionEvent::Rejected { .. }) {
                    c2.cancel();
                }
            }
        });
        let r = m.provision(spec, tx, &cancel);
        h.join().unwrap();
        assert!(r.unwrap_err().is(ErrorKind::Cancelled));
        assert!(m.runtimes(&nc()).unwrap().runtimes.is_empty());
    }

    #[test]
    fn vm_list_dir_and_vm_upload_work_and_record_calls() {
        let (m, _) = mock(MockScenario::demo());
        let target = RuntimeRef {
            account: AccountId::new("mock1").unwrap(),
            name: Some("a100-7f3a".into()),
            endpoint: "ep".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("asset.safetensors");
        std::fs::write(&file, b"weights").unwrap();
        m.vm_upload(
            &target,
            &file.to_string_lossy(),
            "content/asset.safetensors",
            &nc(),
        )
        .expect("upload succeeds");
        let entries = m
            .vm_list_dir(&target, "content", &nc())
            .expect("list succeeds");
        assert!(
            entries
                .iter()
                .any(|e| e.name == "asset.safetensors" && !e.is_dir),
            "{entries:?}"
        );
    }

    #[test]
    fn keep_plan_pre_stop_failure_keeps_runtime_and_drive_requires_consent() {
        use crate::keep::{DriveKeepTarget, KeepPlan, LocalKeepTarget};

        let (m, _) = mock(MockScenario::single());
        let (tx, _rx) = mpsc::channel();
        let spec = m.defaults().provision_spec("keep-mock");
        m.provision(spec, tx, &nc()).unwrap();
        let rt = m.runtimes(&nc()).unwrap().runtimes[0].id.clone();

        let dir = tempfile::tempdir().unwrap();
        m.set_keep_plan(
            &rt,
            Some(KeepPlan {
                paths: vec!["/content/out".into()],
                local: Some(LocalKeepTarget {
                    folder: dir.path().to_string_lossy().into_owned(),
                }),
                hf: None,
                drive: Some(DriveKeepTarget {
                    mount_path: "/content/drive".into(),
                    dest_subpath: "MyDrive/out".into(),
                    consented: false,
                    mounted: false,
                }),
            }),
            &nc(),
        )
        .unwrap();

        // Explicit Drive sync fails when unconsented, while default sync skips unconsented Drive
        // and syncs enabled Local target cleanly.
        assert!(
            m.sync_keep(&rt, Some(vec![crate::keep::KeepTargetId::Drive]), &nc())
                .is_err()
        );
        let report = m.sync_keep(&rt, None, &nc()).unwrap();
        assert!(report.all_succeeded());
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].0, crate::keep::KeepTargetId::Local);

        // If only an unconsented Drive target exists, stop_session must not abort on it.
        m.set_keep_plan(
            &rt,
            Some(KeepPlan {
                paths: vec!["/content/out".into()],
                local: None,
                hf: None,
                drive: Some(DriveKeepTarget {
                    mount_path: "/content/drive".into(),
                    dest_subpath: "MyDrive/out".into(),
                    consented: false,
                    mounted: false,
                }),
            }),
            &nc(),
        )
        .unwrap();
        assert!(m.sync_keep(&rt, None, &nc()).is_err());
        m.stop_session(
            &rt,
            StopOptions {
                reason: StopReason::User,
                save_keep: true,
                force_on_keep_error: false,
                keep_timeout: None,
            },
            &nc(),
        )
        .unwrap();
        assert!(m.runtimes(&nc()).unwrap().runtimes.is_empty());
    }

    #[test]
    fn session_controls_open_url_restart_kernel_reset_ssh_and_remote_ssh() {
        let (m, _) = mock(MockScenario::demo());
        let runtimes = m.runtimes(&nc()).unwrap().runtimes;
        let a100 = runtimes
            .iter()
            .find(|r| r.id.name.as_deref() == Some("a100-7f3a"))
            .unwrap()
            .id
            .clone();
        let other = runtimes
            .iter()
            .find(|r| r.id.name.as_deref() == Some("cpu-0b5e"))
            .unwrap()
            .id
            .clone();

        let url = m.open_in_browser(&a100, &nc()).unwrap();
        assert!(url.contains(&a100.endpoint), "{url}");
        assert_eq!(m.opened_urls(), vec![url]);
        assert!(
            m.open_in_browser(&other, &nc())
                .unwrap_err()
                .is(ErrorKind::AccountNotActive)
        );

        m.restart_kernel(&a100, &nc()).unwrap();
        assert_eq!(m.restarted_kernels(), vec!["a100-7f3a".to_owned()]);
        m.set_failures(MockFailures {
            restart_kernel_fails: true,
            ..MockFailures::default()
        });
        assert!(
            m.restart_kernel(&a100, &nc())
                .unwrap_err()
                .is(ErrorKind::ConnectionLost)
        );

        m.set_failures(MockFailures {
            ssh_busy_attempts: u32::MAX,
            ..MockFailures::default()
        });
        let (tx, rx) = mpsc::channel();
        let spec = TunnelSpec {
            account: a100.account.clone(),
            session: "a100-7f3a".into(),
            local_port: 18000,
            remote_port: 8000,
            health_path: None,
        };
        let _ = m.open_tunnel(spec.clone(), tx).unwrap();
        assert!(rx.try_iter().any(|e| matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Failed { ref error },
                ..
            } if error.is(ErrorKind::SshBusy)
        )));

        m.reset_ssh_sessions(&a100, &nc()).unwrap();
        assert_eq!(m.ssh_resets(), vec!["a100-7f3a".to_owned()]);
        let (tx2, rx2) = mpsc::channel();
        let id = m.open_tunnel(spec, tx2).unwrap();
        assert!(rx2.try_iter().any(|e| matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Up,
                ..
            }
        )));
        m.close_tunnel(id).unwrap();

        let info = m.remote_ssh(&a100, &nc()).unwrap();
        assert_eq!(info.host_alias, "colab-a100-7f3a");
        assert!(info.command.contains("ProxyCommand="));
        assert!(info.config_snippet.contains("--config"));
        let temp = m.write_temp_ssh_config(&a100, &nc()).unwrap();
        let written = std::fs::read_to_string(&temp).unwrap();
        assert!(written.contains("Host colab-a100-7f3a"));
        let _ = std::fs::remove_file(&temp);
    }
}
