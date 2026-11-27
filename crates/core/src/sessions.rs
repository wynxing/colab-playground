//! Session types and the app-side session ledger.
//!
//! The Colab CLI knows which runtimes exist; the ledger remembers what the app
//! itself provisioned (start time, guard policy, probed hardware, watchdog
//! deadline, integrated spend, why it was stopped) so the UI can show uptime,
//! estimated spend and time to auto-stop.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::accounts::AccountId;
use crate::clock::serde_secs;
use crate::error::{Error, Result};
use crate::guard::{Concern, GuardPolicy, SpendMeter};
use crate::usage::RateReading;

/// Hardware that can be requested from `colab new`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Accelerator {
    #[default]
    None,
    /// NVIDIA T4 (16 GB).
    T4,
    /// NVIDIA L4 (24 GB).
    L4,
    /// NVIDIA G4 (RTX PRO 6000 class).
    G4,
    /// NVIDIA A100 — Colab assigns 40 GB or 80 GB; it cannot be requested.
    A100,
    /// NVIDIA H100 (80 GB).
    H100,
    /// TPU v5e-1.
    #[serde(rename = "V5E1")]
    TpuV5e1,
    /// TPU v6e-1.
    #[serde(rename = "V6E1")]
    TpuV6e1,
}

impl Accelerator {
    /// Every accelerator, in UI order.
    pub const ALL: [Accelerator; 8] = [
        Accelerator::None,
        Accelerator::T4,
        Accelerator::L4,
        Accelerator::G4,
        Accelerator::A100,
        Accelerator::H100,
        Accelerator::TpuV5e1,
        Accelerator::TpuV6e1,
    ];

    /// The CLI's accelerator value (`NONE`, `A100`, `V5E1`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Accelerator::None => "NONE",
            Accelerator::T4 => "T4",
            Accelerator::L4 => "L4",
            Accelerator::G4 => "G4",
            Accelerator::A100 => "A100",
            Accelerator::H100 => "H100",
            Accelerator::TpuV5e1 => "V5E1",
            Accelerator::TpuV6e1 => "V6E1",
        }
    }

    /// Parses the `Hardware:` label of `colab sessions` (`CPU` means none).
    pub fn from_label(label: &str) -> Option<Accelerator> {
        let l = label.trim().to_ascii_uppercase();
        if l == "CPU" {
            return Some(Accelerator::None);
        }
        Accelerator::ALL.into_iter().find(|a| a.as_str() == l)
    }

    /// Flags for `colab new` (validated locally; never pass unknown values).
    pub fn new_flags(self) -> Vec<String> {
        match self {
            Accelerator::None => vec![],
            Accelerator::TpuV5e1 => vec!["--tpu".into(), "v5e1".into()],
            Accelerator::TpuV6e1 => vec!["--tpu".into(), "v6e1".into()],
            gpu => vec!["--gpu".into(), gpu.as_str().into()],
        }
    }

    pub fn is_gpu(self) -> bool {
        matches!(
            self,
            Accelerator::T4
                | Accelerator::L4
                | Accelerator::G4
                | Accelerator::A100
                | Accelerator::H100
        )
    }

    pub fn is_tpu(self) -> bool {
        matches!(self, Accelerator::TpuV5e1 | Accelerator::TpuV6e1)
    }

    /// False for accelerators with a single machine shape (`--high-mem` is ignored).
    pub fn supports_high_mem(self) -> bool {
        !matches!(
            self,
            Accelerator::L4 | Accelerator::TpuV5e1 | Accelerator::TpuV6e1
        )
    }
}

/// Validates a session name: 1–63 chars of `[A-Za-z0-9._-]`, starting
/// alphanumeric. Names end up in state files and ssh `ProxyCommand`s.
pub fn validate_session_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid("name", format!("invalid session name {name:?}")).with("value", name))
    }
}

/// One line of `colab sessions` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSession {
    /// Local name, `None` for `[?]` (an assignment without local state, e.g.
    /// created in the browser or by another account's state file).
    pub name: Option<String>,
    pub endpoint: String,
    /// `Hardware:` label as printed (`CPU`, `A100`, …).
    pub hardware: String,
    /// Parsed accelerator, when recognised.
    pub accelerator: Option<Accelerator>,
    /// `Shape:` label (`Standard` / `High-RAM`).
    pub shape: String,
    /// `Variant:` label (`DEFAULT` / `GPU` / `TPU`).
    pub variant: String,
    /// `Status:` label when present.
    pub status: Option<String>,
}

impl ServerSession {
    pub fn high_mem(&self) -> bool {
        self.shape.eq_ignore_ascii_case("High-RAM")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    User,
    Guard {
        concern: Concern,
    },
    /// Provisioning rejected or cancelled this runtime.
    Provision,
    /// It disappeared (VM watchdog, Colab idle reclaim, or stopped elsewhere).
    Vanished,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopRecord {
    /// When the app noticed or performed the stop.
    #[serde(with = "serde_secs")]
    pub at: SystemTime,
    pub reason: StopReason,
}

/// What the app knows about the VM-side watchdog of a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchdogInfo {
    /// Absolute deadline (local clock) the watchdog was given, if any. Empty
    /// when a start of the watchdog failed, because nothing on the runtime
    /// enforces a deadline then.
    #[serde(default, with = "serde_secs::option")]
    pub deadline: Option<SystemTime>,
    #[serde(default, with = "crate::clock::serde_dur::option")]
    pub idle_timeout: Option<Duration>,
    #[serde(default)]
    pub cancelled: bool,
    /// The stop is armed on the runtime: the last reply from the VM (start,
    /// extend, cancel or status) showed the watchdog running, not cancelled
    /// and able to release the runtime. Only then is the runtime released at
    /// `deadline` (or when idle) while the app is closed. A watchdog that
    /// exits later still reads as armed until the next reply. `false` when not
    /// confirmed, which includes entries saved before this field existed.
    #[serde(default)]
    pub armed: bool,
}

impl WatchdogInfo {
    /// The deadline the runtime itself enforces: `deadline` while the
    /// watchdog is armed and not cancelled.
    pub fn enforced_deadline(&self) -> Option<SystemTime> {
        self.deadline.filter(|_| self.armed && !self.cancelled)
    }
}

/// What a runtime with this hardware burned when it was last measured.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HardwareRate {
    pub accelerator: Accelerator,
    /// False where the accelerator has a single machine shape.
    pub high_mem: bool,
    pub reading: RateReading,
}

impl HardwareRate {
    /// Whether a runtime requested with these options gets this hardware.
    pub fn matches(&self, accelerator: Accelerator, high_mem: bool) -> bool {
        self.accelerator == accelerator
            && self.high_mem == (high_mem && accelerator.supports_high_mem())
    }
}

/// A session the app provisioned (persisted in the ledger).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackedSession {
    pub name: String,
    pub account: AccountId,
    /// Server endpoint, once known.
    #[serde(default)]
    pub endpoint: Option<String>,
    pub accelerator: Accelerator,
    pub high_mem: bool,
    #[serde(default)]
    pub gpu: Option<String>,
    /// Probed GPU memory in MiB.
    #[serde(default)]
    pub vram_mib: Option<u32>,
    pub policy: GuardPolicy,
    /// Spend integration (its `started_at` is the session start).
    pub spend: SpendMeter,
    /// VM watchdog state, if one was started.
    #[serde(default)]
    pub watchdog: Option<WatchdogInfo>,
    #[serde(default)]
    pub stopped: Option<StopRecord>,
    /// The account's burn rate at the last reading where this session was
    /// the account's only runtime: what its hardware burns, not an estimate.
    #[serde(default)]
    pub measured_rate: Option<RateReading>,
    /// Per-session Keep Plan (paths and persistence targets).
    #[serde(default)]
    pub keep_plan: Option<crate::keep::KeepPlan>,
    /// Result of the most recent Keep Plan sync attempt.
    #[serde(default)]
    pub last_keep_sync: Option<crate::keep::KeepSyncReport>,
}

impl TrackedSession {
    pub fn started_at(&self) -> SystemTime {
        self.spend.started_at
    }

    /// Uptime at `now` (frozen at the stop time once stopped).
    pub fn uptime(&self, now: SystemTime) -> Duration {
        let end = self.stopped.as_ref().map(|s| s.at).unwrap_or(now);
        end.duration_since(self.started_at()).unwrap_or_default()
    }

    /// Earliest of the app-side runtime limit and the deadline the runtime's
    /// watchdog enforces.
    pub fn auto_stop_at(&self) -> Option<SystemTime> {
        let app = self.policy.max_runtime.map(|d| self.started_at() + d);
        let vm = self
            .watchdog
            .as_ref()
            .and_then(WatchdogInfo::enforced_deadline);
        match (app, vm) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Persistent list of tracked sessions (`<data>/ledger.json`).
#[derive(Debug)]
pub struct Ledger {
    path: Option<PathBuf>,
    entries: Vec<TrackedSession>,
    /// The entries differ from the file: [`Ledger::save_changes`] writes them.
    unsaved: bool,
}

impl Ledger {
    pub fn load(path: &Path) -> Result<Ledger> {
        let entries = match std::fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Vec::new(),
            Ok(s) => serde_json::from_str(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Ledger {
            path: Some(path.to_owned()),
            entries,
            unsaved: false,
        })
    }

    /// A ledger that is never written to disk.
    pub fn in_memory() -> Ledger {
        Ledger {
            path: None,
            entries: Vec::new(),
            unsaved: false,
        }
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> &[TrackedSession] {
        &self.entries
    }

    /// The latest rate measured for each hardware, over running and stopped
    /// sessions of every account.
    pub fn measured_rates(&self) -> Vec<HardwareRate> {
        let mut latest: Vec<HardwareRate> = Vec::new();
        for e in &self.entries {
            let Some(reading) = e.measured_rate else {
                continue;
            };
            match latest
                .iter_mut()
                .find(|r| r.matches(e.accelerator, e.high_mem))
            {
                Some(r) if reading.at > r.reading.at => r.reading = reading,
                Some(_) => {}
                None => latest.push(HardwareRate {
                    accelerator: e.accelerator,
                    high_mem: e.high_mem && e.accelerator.supports_high_mem(),
                    reading,
                }),
            }
        }
        latest
    }

    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    pub fn all_live(&self) -> impl DoubleEndedIterator<Item = &TrackedSession> {
        self.entries.iter().filter(|e| e.stopped.is_none())
    }

    pub fn live_of<'a>(
        &'a self,
        account: &AccountId,
    ) -> impl DoubleEndedIterator<Item = &'a TrackedSession> {
        self.all_live().filter(move |e| &e.account == account)
    }

    /// The live (not stopped) entry for `account`/`name`.
    pub fn live(&self, account: &AccountId, name: &str) -> Option<&TrackedSession> {
        self.live_of(account).rev().find(|e| e.name == name)
    }

    pub fn live_by_endpoint(&self, account: &AccountId, endpoint: &str) -> Option<&TrackedSession> {
        self.live_of(account)
            .rev()
            .find(|e| e.endpoint.as_deref() == Some(endpoint))
    }

    pub fn live_mut(&mut self, account: &AccountId, name: &str) -> Option<&mut TrackedSession> {
        self.entries
            .iter_mut()
            .rev()
            .find(|e| &e.account == account && e.name == name && e.stopped.is_none())
    }

    /// Adds an entry (replacing a live one with the same key).
    pub fn upsert(&mut self, entry: TrackedSession) -> Result<()> {
        if let Some(e) = self.live_mut(&entry.account.clone(), &entry.name.clone()) {
            *e = entry;
        } else {
            self.entries.push(entry);
        }
        self.trim();
        self.save()
    }

    pub fn update(
        &mut self,
        account: &AccountId,
        name: &str,
        f: impl FnOnce(&mut TrackedSession),
    ) -> Result<bool> {
        let found = self.change(account, name, f);
        if found {
            self.save()?;
        }
        Ok(found)
    }

    /// [`Ledger::update`] without the write, for a round that changes several entries:
    /// it ends with [`Ledger::save_changes`] and so writes the ledger once, or not at all
    /// when `f` left every entry as it was.
    pub fn change(
        &mut self,
        account: &AccountId,
        name: &str,
        f: impl FnOnce(&mut TrackedSession),
    ) -> bool {
        let Some(e) = self.live_mut(account, name) else {
            return false;
        };
        let before = e.clone();
        f(e);
        let changed = *e != before;
        self.unsaved |= changed;
        true
    }

    /// Writes the ledger if a [`Ledger::change`], or a write that failed, is waiting for it.
    pub fn save_changes(&mut self) -> Result<()> {
        if self.unsaved { self.save() } else { Ok(()) }
    }

    pub fn mark_stopped(
        &mut self,
        account: &AccountId,
        name: &str,
        at: SystemTime,
        reason: StopReason,
    ) -> Result<bool> {
        self.update(account, name, |e| {
            e.stopped = Some(StopRecord { at, reason })
        })
    }

    /// Applies a listing of `account`'s runtimes to its live entries: see [`reconcile`].
    pub fn reconcile(
        &mut self,
        account: &AccountId,
        judged: impl Fn(&TrackedSession) -> bool,
        listed: &[&str],
        names: &[(&str, &str)],
        now: SystemTime,
    ) -> Reconciliation {
        let live: Vec<TrackedSession> = self.live_of(account).cloned().collect();
        let outcome = reconcile(&live, judged, listed, names);
        // A failed save keeps the change in memory and the next save writes it, so the
        // round goes on: a session that ended is still reported.
        for (name, endpoint) in &outcome.confirmed {
            let _ = self.update(account, name, |e| e.endpoint = Some(endpoint.clone()));
        }
        for (name, _) in &outcome.vanished {
            let _ = self.mark_stopped(account, name, now, StopReason::Vanished);
        }
        outcome
    }

    /// Keeps at most 200 stopped entries, the one holding a hardware's latest
    /// measured rate among them however old it is.
    fn trim(&mut self) {
        let stopped = self.entries.iter().filter(|e| e.stopped.is_some()).count();
        if stopped > 200 {
            let latest = self.measured_rates();
            let mut drop = stopped - 200;
            self.entries.retain(|e| {
                let holds_rate = e.measured_rate.is_some_and(|reading| {
                    latest
                        .iter()
                        .any(|r| r.matches(e.accelerator, e.high_mem) && r.reading == reading)
                });
                if drop > 0 && e.stopped.is_some() && !holds_rate {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Writes the ledger (atomic replace).
    pub fn save(&mut self) -> Result<()> {
        // A write that fails leaves the entries unsaved, whoever asked for it.
        self.unsaved = true;
        if let Some(path) = &self.path {
            crate::paths::write_atomic(
                path,
                serde_json::to_string_pretty(&self.entries)?.as_bytes(),
            )?;
        }
        self.unsaved = false;
        Ok(())
    }
}

/// What a listing of one account's runtimes says about its live sessions.
#[derive(Debug, Default, PartialEq)]
pub struct Reconciliation {
    /// Sessions that had no endpoint and whose runtime the listing confirms: (name, endpoint).
    pub confirmed: Vec<(String, String)>,
    /// Sessions whose runtime has ended: (name, endpoint), the endpoint empty when it was
    /// never known.
    pub vanished: Vec<(String, String)>,
    /// A session without an endpoint is left, and a listed runtime no session claims could
    /// be its own.
    pub unsettled: bool,
}

/// Pure: depends only on its arguments.
///
/// `live` are the account's live entries, of which only the `judged` ones can be confirmed
/// or end; `listed` are the endpoints of its runtimes; `names` pairs an endpoint with the
/// session name recorded for it, and may be out of date.
pub fn reconcile(
    live: &[TrackedSession],
    judged: impl Fn(&TrackedSession) -> bool,
    listed: &[&str],
    names: &[(&str, &str)],
) -> Reconciliation {
    let mut outcome = Reconciliation::default();
    let mut unmatched = Vec::new();
    let mut claimed: Vec<&str> = Vec::new();
    for e in live {
        if !judged(e) {
            claimed.extend(e.endpoint.as_deref());
            continue;
        }
        match &e.endpoint {
            Some(endpoint) if listed.contains(&endpoint.as_str()) => claimed.push(endpoint),
            Some(endpoint) => outcome.vanished.push((e.name.clone(), endpoint.clone())),
            None => {
                // `names` may still hold the record of an earlier session with this name,
                // so only an endpoint the listing confirms is taken from it, and an
                // unlisted one never ends the session.
                let confirmed = names
                    .iter()
                    .find(|(endpoint, name)| *name == e.name && listed.contains(endpoint));
                match confirmed {
                    Some((endpoint, _)) => {
                        claimed.push(endpoint);
                        outcome
                            .confirmed
                            .push((e.name.clone(), (*endpoint).to_owned()));
                    }
                    None => unmatched.push(e.name.clone()),
                }
            }
        }
    }
    // A session without an endpoint can only be a listed runtime that no other live
    // session claims; with none left, its runtime has ended.
    if listed.iter().all(|endpoint| claimed.contains(endpoint)) {
        outcome
            .vanished
            .extend(unmatched.into_iter().map(|name| (name, String::new())));
    } else {
        outcome.unsettled = !unmatched.is_empty();
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accelerator_labels_and_flags() {
        assert_eq!(Accelerator::from_label("CPU"), Some(Accelerator::None));
        assert_eq!(Accelerator::from_label("a100"), Some(Accelerator::A100));
        assert_eq!(Accelerator::from_label("V5E1"), Some(Accelerator::TpuV5e1));
        assert_eq!(Accelerator::from_label("B200"), None);
        assert_eq!(Accelerator::A100.new_flags(), vec!["--gpu", "A100"]);
        assert_eq!(Accelerator::TpuV6e1.new_flags(), vec!["--tpu", "v6e1"]);
        assert!(Accelerator::None.new_flags().is_empty());
        assert!(!Accelerator::L4.supports_high_mem());
        assert_eq!(
            serde_json::to_string(&Accelerator::TpuV5e1).unwrap(),
            "\"V5E1\""
        );
        assert_eq!(
            serde_json::to_string(&Accelerator::A100).unwrap(),
            "\"A100\""
        );
    }

    #[test]
    fn session_names() {
        assert!(validate_session_name("llm-1.a_b").is_ok());
        for bad in ["", "-x", "a b", "a;b", "é", &"x".repeat(64)] {
            assert!(validate_session_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ledger_round_trip_and_auto_stop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut ledger = Ledger::load(&path).unwrap();
        let acct = AccountId::new("a1").unwrap();
        ledger
            .upsert(TrackedSession {
                name: "s".into(),
                account: acct.clone(),
                endpoint: None,
                accelerator: Accelerator::A100,
                high_mem: false,
                gpu: None,
                vram_mib: None,
                policy: GuardPolicy::default(),
                spend: SpendMeter::new(t0),
                watchdog: Some(WatchdogInfo {
                    deadline: Some(t0 + Duration::from_secs(3600)),
                    idle_timeout: None,
                    cancelled: false,
                    armed: true,
                }),
                stopped: None,
                measured_rate: None,
                keep_plan: None,
                last_keep_sync: None,
            })
            .unwrap();
        let again = Ledger::load(&path).unwrap();
        let e = again.live(&acct, "s").unwrap();
        assert_eq!(e.auto_stop_at(), Some(t0 + Duration::from_secs(3600)));
        assert_eq!(
            e.uptime(t0 + Duration::from_secs(10)),
            Duration::from_secs(10)
        );
    }

    fn live(name: &str, endpoint: Option<&str>) -> TrackedSession {
        TrackedSession {
            name: name.into(),
            account: AccountId::new("a1").unwrap(),
            endpoint: endpoint.map(str::to_owned),
            accelerator: Accelerator::None,
            high_mem: false,
            gpu: None,
            vram_mib: None,
            policy: GuardPolicy::default(),
            spend: SpendMeter::new(SystemTime::UNIX_EPOCH),
            watchdog: None,
            stopped: None,
            measured_rate: None,
            keep_plan: None,
            last_keep_sync: None,
        }
    }

    #[test]
    fn a_failed_write_is_made_by_the_next_round_that_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // The ledger's folder cannot be created while a file has its name.
        let folder = dir.path().join("local");
        let path = folder.join("ledger.json");
        let mut ledger = Ledger::load(&path).unwrap();
        std::fs::write(&folder, "").unwrap();
        assert!(ledger.upsert(live("s", None)).is_err());

        std::fs::remove_file(&folder).unwrap();
        let account = AccountId::new("a1").unwrap();
        assert!(ledger.change(&account, "s", |_| {}));
        ledger.save_changes().unwrap();
        assert!(Ledger::load(&path).unwrap().live(&account, "s").is_some());

        std::fs::remove_file(&path).unwrap();
        assert!(ledger.change(&account, "s", |_| {}));
        ledger.save_changes().unwrap();
        assert!(!path.exists(), "written with nothing to save");
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn measured_rates_keep_the_latest_reading_of_each_hardware() {
        let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        let entry = |name: &str, accelerator, high_mem, rate: Option<(f64, u64)>| TrackedSession {
            accelerator,
            high_mem,
            measured_rate: rate.map(|(cu_per_hour, at)| RateReading {
                cu_per_hour,
                at: t(at),
            }),
            ..live(name, None)
        };
        let mut ledger = Ledger::in_memory();
        for e in [
            entry("old", Accelerator::T4, false, Some((1.9, 10))),
            entry("new", Accelerator::T4, false, Some((1.76, 20))),
            entry("big", Accelerator::T4, true, Some((2.0, 5))),
            entry("l4", Accelerator::L4, true, Some((4.82, 30))),
            entry("never", Accelerator::A100, false, None),
        ] {
            ledger.upsert(e).unwrap();
        }
        let a1 = AccountId::new("a1").unwrap();
        ledger
            .mark_stopped(&a1, "new", t(40), StopReason::User)
            .unwrap();
        let rates = ledger.measured_rates();
        let rate = |accelerator, high_mem| {
            rates
                .iter()
                .find(|r| r.matches(accelerator, high_mem))
                .map(|r| r.reading.cu_per_hour)
        };
        assert_eq!(rates.len(), 3);
        assert_eq!(rate(Accelerator::T4, false), Some(1.76));
        assert_eq!(rate(Accelerator::T4, true), Some(2.0));
        // An L4 has one machine shape: the high-RAM option changes nothing.
        assert_eq!(rate(Accelerator::L4, false), Some(4.82));
        assert_eq!(rate(Accelerator::L4, true), Some(4.82));
        assert_eq!(rate(Accelerator::A100, false), None);
    }

    #[test]
    fn a_measured_rate_outlives_the_trimming_of_old_sessions() {
        let a1 = AccountId::new("a1").unwrap();
        let mut ledger = Ledger::in_memory();
        let mut stop = |entry: TrackedSession| {
            let name = entry.name.clone();
            ledger.upsert(entry).unwrap();
            ledger
                .mark_stopped(&a1, &name, SystemTime::UNIX_EPOCH, StopReason::User)
                .unwrap();
        };
        stop(TrackedSession {
            accelerator: Accelerator::T4,
            measured_rate: Some(RateReading {
                cu_per_hour: 1.76,
                at: SystemTime::UNIX_EPOCH,
            }),
            ..live("measured", None)
        });
        for i in 0..250 {
            stop(live(&format!("s{i}"), None));
        }
        assert!(ledger.entries().iter().all(|e| e.name != "s0"), "trimmed");
        let rates = ledger.measured_rates();
        assert_eq!(rates.len(), 1);
        assert!(rates[0].matches(Accelerator::T4, false));
    }

    #[test]
    fn a_session_ends_when_its_endpoint_is_no_longer_listed() {
        let entries = [live("kept", Some("e1")), live("gone", Some("e2"))];
        let outcome = reconcile(&entries, |_| true, &["e1"], &[]);
        assert_eq!(outcome.vanished, pairs(&[("gone", "e2")]));
        assert!(outcome.confirmed.is_empty() && !outcome.unsettled);
    }

    #[test]
    fn a_session_without_an_endpoint_takes_only_a_listed_one_recorded_for_its_name() {
        let entries = [live("new", None), live("stale", None)];
        let names = [("e0", "new"), ("e1", "new"), ("e9", "stale")];
        let outcome = reconcile(&entries, |_| true, &["e1", "e2"], &names);
        assert_eq!(outcome.confirmed, pairs(&[("new", "e1")]));
        assert!(outcome.vanished.is_empty(), "e2 could be the other session");
        assert!(outcome.unsettled);
    }

    #[test]
    fn a_session_without_an_endpoint_ends_once_every_listed_runtime_is_claimed() {
        let entries = [live("a", Some("e1")), live("b", None), live("later", None)];
        let judged = |e: &TrackedSession| e.name != "later";
        let outcome = reconcile(&entries, judged, &["e1"], &[]);
        assert_eq!(outcome.vanished, pairs(&[("b", "")]));
        assert!(!outcome.unsettled);

        let entries = [live("b", None), live("later", Some("e1"))];
        let outcome = reconcile(&entries, judged, &["e1"], &[]);
        assert_eq!(
            outcome.vanished,
            pairs(&[("b", "")]),
            "a session that is not judged still claims its runtime"
        );
    }

    #[test]
    fn reconciling_the_ledger_records_what_the_listing_says() {
        let acct = AccountId::new("a1").unwrap();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut ledger = Ledger::in_memory();
        for entry in [live("gone", Some("e2")), live("new", None)] {
            ledger.upsert(entry).unwrap();
        }
        ledger.reconcile(&acct, |_| true, &["e1"], &[("e1", "new")], t0);
        assert_eq!(ledger.live_by_endpoint(&acct, "e1").unwrap().name, "new");
        assert!(ledger.live(&acct, "gone").is_none());
        assert_eq!(
            ledger.entries()[0].stopped,
            Some(StopRecord {
                at: t0,
                reason: StopReason::Vanished
            })
        );
    }

    #[test]
    fn only_an_armed_watchdog_counts_as_a_stop_on_the_runtime() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let wd = WatchdogInfo {
            deadline: Some(t0 + Duration::from_secs(600)),
            idle_timeout: None,
            cancelled: false,
            armed: true,
        };
        assert_eq!(wd.enforced_deadline(), wd.deadline);
        let dead = WatchdogInfo {
            armed: false,
            ..wd.clone()
        };
        assert_eq!(dead.enforced_deadline(), None);
        let cancelled = WatchdogInfo {
            cancelled: true,
            ..wd.clone()
        };
        assert_eq!(cancelled.enforced_deadline(), None);

        // The earlier watchdog deadline counts only while it is armed; the
        // app-side limit (2 h by default) remains either way.
        let mut s = TrackedSession {
            name: "s".into(),
            account: AccountId::new("a1").unwrap(),
            endpoint: None,
            accelerator: Accelerator::T4,
            high_mem: false,
            gpu: None,
            vram_mib: None,
            policy: GuardPolicy::default(),
            spend: SpendMeter::new(t0),
            watchdog: Some(wd),
            stopped: None,
            measured_rate: None,
            keep_plan: None,
            last_keep_sync: None,
        };
        assert_eq!(s.auto_stop_at(), Some(t0 + Duration::from_secs(600)));
        s.watchdog = Some(dead);
        assert_eq!(s.auto_stop_at(), Some(t0 + Duration::from_secs(7200)));

        // Ledgers written before `armed` existed load as not armed.
        let old: WatchdogInfo =
            serde_json::from_str(r#"{"deadline": 1000600, "cancelled": false}"#).unwrap();
        assert!(!old.armed);
    }
}
