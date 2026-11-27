//! The VM-side watchdog: deployment and control snippets.
//!
//! `assets/watchdog.py` runs detached on the VM and releases the runtime at its
//! deadline (or when idle) even if the app is gone. These snippets are Python
//! code sent with `colab exec`; numeric values travel through `--env` (the
//! CLI quotes them), and the watchdog source is embedded as base64 so nothing
//! user-controlled is interpolated into code.
//!
//! Deadlines are sent as *relative* seconds and turned into an absolute time
//! on the VM clock; the VM replies with its own `now` so the app can map the
//! deadline back onto the local clock without trusting either clock's offset.

use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use crate::error::{Error, ErrorKind, Result};
use crate::guard::GuardPolicy;
use crate::sessions::WatchdogInfo;

pub const WATCHDOG_PY: &str = include_str!("../assets/watchdog.py");

/// Directory on the VM holding watchdog, heartbeat and plugin runs.
pub const VM_BASE: &str = "/root/.colab-playground";

/// Heartbeat file plugins touch to signal activity.
pub const HEARTBEAT_PATH: &str = "/root/.colab-playground/heartbeat";

/// Env var carrying the deadline in seconds from now (`none` = no deadline).
pub const ENV_DEADLINE: &str = "CPG_WD_DEADLINE_S";
/// Env var carrying the idle timeout in seconds (`none` = no idle rule).
pub const ENV_IDLE: &str = "CPG_WD_IDLE_S";
/// Env var carrying the wind-down grace period in seconds.
pub const ENV_GRACE: &str = "CPG_WD_GRACE_S";
/// Env var carrying the wind-down hooks (JSON); popped and piped to the
/// watchdog's stdin, never written to disk.
pub const ENV_HOOKS: &str = "CPG_WD_HOOKS";

/// A command the watchdog runs during wind-down (before the deadline) or periodically.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WatchdogHook {
    /// Program and arguments (no shell).
    pub argv: Vec<String>,
    /// Extra environment (kept in the watchdog's memory only).
    pub env: std::collections::BTreeMap<String, String>,
    /// Time limit in seconds (capped by the time left before the deadline).
    pub timeout_s: u64,
    /// Optional periodic interval in seconds (runs hook repeatedly while alive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_s: Option<u64>,
}

const STATUS_FN: &str = r#"
def _cpg_wd_status(cfg, alive):
    print("__CPG_RESULT__ " + json.dumps({
        "now": time.time(),
        "deadline_epoch": cfg.get("deadline_epoch"),
        "idle_timeout_s": cfg.get("idle_timeout_s"),
        "cancelled": bool(cfg.get("cancelled")),
        "runtime_addr": bool(cfg.get("runtime_addr")),
        "alive": alive,
    }))

def _cpg_wd_alive():
    try:
        pid = int(open(B + "/watchdog.pid").read().strip())
        with open("/proc/%d/stat" % pid) as fh:
            return fh.read().split(") ", 1)[1][:1] not in ("Z", "X")
    except Exception:
        return False

def _cpg_wd_write(cfg):
    tmp = B + "/watchdog.json.tmp"
    with open(tmp, "w") as fh:
        json.dump(cfg, fh)
    os.replace(tmp, B + "/watchdog.json")

def _cpg_num(key):
    v = os.environ.pop(key, "none")
    return None if v in ("", "none") else float(v)
"#;

fn header() -> String {
    format!(
        "import base64, json, os, subprocess, sys, time\nB = {VM_BASE:?}\nos.makedirs(B, exist_ok=True)\n{STATUS_FN}\n"
    )
}

/// Code that installs and (re)starts the watchdog. Needs [`start_env`].
pub fn start_snippet() -> String {
    let b64 = B64.encode(WATCHDOG_PY);
    format!(
        r#"{header}
with open(B + "/watchdog.py", "wb") as fh:
    fh.write(base64.b64decode("{b64}"))
_dl = _cpg_num("{ENV_DEADLINE}")
_idle = _cpg_num("{ENV_IDLE}")
_grace = _cpg_num("{ENV_GRACE}")
_hooks = os.environ.pop("{ENV_HOOKS}", "[]")
_now = time.time()
_cfg = {{
    "deadline_epoch": (_now + _dl) if _dl is not None else None,
    "idle_timeout_s": _idle,
    "cancelled": False,
    "runtime_addr": os.environ.get("TBE_RUNTIME_ADDR", ""),
    "heartbeat": B + "/heartbeat",
    "log": B + "/watchdog.log",
    "poll_s": 15,
    "grace_s": _grace if _grace is not None else 300,
    "started": _now,
}}
try:
    os.kill(int(open(B + "/watchdog.pid").read().strip()), 15)
    time.sleep(0.5)
except Exception:
    pass
_cpg_wd_write(_cfg)
_p = subprocess.Popen([sys.executable, B + "/watchdog.py", "--config", B + "/watchdog.json", "--hooks-stdin"],
                      stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                      start_new_session=True, close_fds=True)
_p.stdin.write(_hooks.encode())
_p.stdin.close()
del _hooks
time.sleep(1.0)
_cpg_wd_status(_cfg, _cpg_wd_alive())
"#,
        header = header()
    )
}

pub fn start_env(
    policy: &GuardPolicy,
    grace: Duration,
    hooks: &[WatchdogHook],
) -> Vec<(String, String)> {
    let mut env = start_env_basic(policy);
    env.push((ENV_GRACE.into(), grace.as_secs().to_string()));
    env.push((
        ENV_HOOKS.into(),
        serde_json::to_string(hooks).unwrap_or_else(|_| "[]".into()),
    ));
    env
}

fn start_env_basic(policy: &GuardPolicy) -> Vec<(String, String)> {
    let secs = |d: Option<Duration>| {
        d.map(|d| d.as_secs().to_string())
            .unwrap_or_else(|| "none".into())
    };
    vec![
        (ENV_DEADLINE.into(), secs(policy.max_runtime)),
        (ENV_IDLE.into(), secs(policy.idle_timeout)),
    ]
}

/// Code that moves the deadline to `CPG_WD_DEADLINE_S` seconds from now (VM clock).
pub fn extend_snippet() -> String {
    format!(
        r#"{header}
_cfg = json.load(open(B + "/watchdog.json"))
_dl = _cpg_num("{ENV_DEADLINE}")
_cfg["deadline_epoch"] = (time.time() + _dl) if _dl is not None else None
_cfg["cancelled"] = False
_cpg_wd_write(_cfg)
_cpg_wd_status(_cfg, _cpg_wd_alive())
"#,
        header = header()
    )
}

/// Code that cancels the watchdog (it exits without releasing).
pub fn cancel_snippet() -> String {
    format!(
        r#"{header}
_cfg = json.load(open(B + "/watchdog.json"))
_cfg["cancelled"] = True
_cpg_wd_write(_cfg)
_cpg_wd_status(_cfg, _cpg_wd_alive())
"#,
        header = header()
    )
}

/// Code that reports the watchdog state (a missing control file reads as
/// cancelled, with no watchdog running).
pub fn status_snippet() -> String {
    format!(
        r#"{header}
try:
    _cfg = json.load(open(B + "/watchdog.json"))
except Exception:
    _cfg = {{"cancelled": True}}
_cpg_wd_status(_cfg, _cpg_wd_alive())
"#,
        header = header()
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct WatchdogReply {
    pub alive: bool,
    /// It knows `TBE_RUNTIME_ADDR` (without it the release cannot work).
    pub can_release: bool,
    /// State mapped onto the local clock. `info.armed` is `alive`,
    /// `can_release` and not cancelled: a cancelled watchdog keeps running
    /// until its next poll, then exits without releasing.
    pub info: WatchdogInfo,
}

/// Maps a snippet reply onto the local clock (`local_now` = when it arrived).
pub fn parse_reply(v: &serde_json::Value, local_now: SystemTime) -> Result<WatchdogReply> {
    let vm_now = v
        .get("now")
        .and_then(|n| n.as_f64())
        .ok_or_else(|| Error::new(ErrorKind::Parse).detail("watchdog reply without `now`"))?;
    let deadline = v.get("deadline_epoch").and_then(|d| d.as_f64()).map(|d| {
        let remaining = d - vm_now;
        if remaining >= 0.0 {
            local_now + Duration::from_secs_f64(remaining)
        } else {
            local_now
                .checked_sub(Duration::from_secs_f64(-remaining))
                .unwrap_or(local_now)
        }
    });
    let alive = v.get("alive").and_then(|a| a.as_bool()).unwrap_or(false);
    let can_release = v
        .get("runtime_addr")
        .and_then(|a| a.as_bool())
        .unwrap_or(false);
    let cancelled = v
        .get("cancelled")
        .and_then(|c| c.as_bool())
        .unwrap_or(false);
    Ok(WatchdogReply {
        alive,
        can_release,
        info: WatchdogInfo {
            deadline,
            idle_timeout: v
                .get("idle_timeout_s")
                .and_then(|i| i.as_f64())
                .map(|s| Duration::from_secs_f64(s.max(0.0))),
            cancelled,
            armed: alive && can_release && !cancelled,
        },
    })
}

/// The policy for starting a watchdog again with a deadline `runtime` from
/// now: the tracked session's idle rule (or, for a session without a ledger
/// entry, the idle rule the VM still has, `vm_idle`) and its other limits.
/// The tracked `max_runtime` is not used: it counts from the session start,
/// while the watchdog's deadline counts from its own start.
pub fn restart_policy(
    tracked: Option<&GuardPolicy>,
    vm_idle: Option<Duration>,
    runtime: Duration,
) -> GuardPolicy {
    let base = tracked.cloned().unwrap_or_else(|| GuardPolicy {
        idle_timeout: vm_idle,
        ..GuardPolicy::unlimited()
    });
    GuardPolicy {
        max_runtime: Some(runtime),
        ..base
    }
}

/// What the app records when a watchdog could not be started for `policy`:
/// not armed and without a deadline, so nothing reads as a stop on the
/// runtime; the app-side limit in the session's policy still applies while
/// the app runs.
pub fn start_failed(policy: &GuardPolicy) -> WatchdogInfo {
    WatchdogInfo {
        deadline: None,
        idle_timeout: policy.idle_timeout,
        cancelled: false,
        armed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_uses_relative_seconds() {
        let p = GuardPolicy {
            idle_timeout: Some(Duration::from_secs(900)),
            ..GuardPolicy::default()
        };
        let hook = WatchdogHook {
            argv: vec!["/usr/bin/true".into()],
            env: [("TOKEN".to_owned(), "x".to_owned())].into(),
            timeout_s: 60,
            interval_s: Some(900),
        };
        let env = start_env(&p, Duration::from_secs(300), std::slice::from_ref(&hook));
        assert_eq!(
            &env[..3],
            &[
                (ENV_DEADLINE.to_owned(), "7200".to_owned()),
                (ENV_IDLE.to_owned(), "900".to_owned()),
                (ENV_GRACE.to_owned(), "300".to_owned()),
            ]
        );
        let hooks: Vec<WatchdogHook> = serde_json::from_str(&env[3].1).unwrap();
        assert_eq!(hooks, vec![hook]);
        let plain: WatchdogHook =
            serde_json::from_str(r#"{"argv":["true"],"env":{},"timeout_s":10}"#).unwrap();
        assert_eq!(plain.interval_s, None);
        let none = start_env(&GuardPolicy::unlimited(), Duration::ZERO, &[]);
        assert_eq!(none[0].1, "none");
        assert_eq!(none[3].1, "[]");
        // Hooks travel on the watchdog's stdin, never in its config file.
        assert!(start_snippet().contains("--hooks-stdin"));
    }

    #[test]
    fn snippets_embed_only_base64_and_constants() {
        let s = start_snippet();
        assert!(s.contains("base64.b64decode(\""));
        assert!(s.contains("TBE_RUNTIME_ADDR"));
        assert!(s.contains(VM_BASE));
        for snippet in [
            start_snippet(),
            extend_snippet(),
            cancel_snippet(),
            status_snippet(),
        ] {
            assert!(snippet.contains("__CPG_RESULT__"));
        }
    }

    #[test]
    fn reply_maps_vm_deadline_to_local_clock() {
        let local = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let v = serde_json::json!({"now": 500.0, "deadline_epoch": 4100.0, "idle_timeout_s": null,
                                   "cancelled": false, "runtime_addr": true, "alive": true});
        let r = parse_reply(&v, local).unwrap();
        assert_eq!(r.info.deadline, Some(local + Duration::from_secs(3600)));
        assert!(r.alive && r.can_release);
        assert!(r.info.armed);
        assert!(parse_reply(&serde_json::json!({}), local).is_err());
    }

    #[test]
    fn armed_needs_a_running_releasing_uncancelled_watchdog() {
        let local = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let armed = |alive: bool, addr: bool, cancelled: bool| {
            let v = serde_json::json!({"now": 500.0, "deadline_epoch": 4100.0, "idle_timeout_s": 900.0,
                                       "cancelled": cancelled, "runtime_addr": addr, "alive": alive});
            parse_reply(&v, local).unwrap().info.armed
        };
        assert!(armed(true, true, false));
        // Right after a cancel the watchdog still runs until its next poll.
        assert!(!armed(true, true, true));
        // Exited (cancelled earlier, crashed, or never started).
        assert!(!armed(false, true, false));
        assert!(!armed(false, true, true));
        // Running but unable to release the runtime.
        assert!(!armed(true, false, false));
        // The status reply for a missing control file.
        let v = serde_json::json!({"now": 500.0, "deadline_epoch": null, "idle_timeout_s": null,
                                   "cancelled": true, "runtime_addr": false, "alive": false});
        let r = parse_reply(&v, local).unwrap();
        assert!(!r.info.armed && r.info.deadline.is_none());
    }

    #[test]
    fn a_restart_counts_its_deadline_from_now_and_keeps_the_idle_rule() {
        let tracked = GuardPolicy {
            // Elapsed time since the session start plus the last extension.
            max_runtime: Some(Duration::from_secs(5 * 3600)),
            idle_timeout: Some(Duration::from_secs(900)),
            max_spend_cu: Some(12.0),
            ..GuardPolicy::unlimited()
        };
        let p = restart_policy(
            Some(&tracked),
            Some(Duration::from_secs(60)),
            Duration::from_secs(3600),
        );
        assert_eq!(p.max_runtime, Some(Duration::from_secs(3600)));
        assert_eq!(p.idle_timeout, Some(Duration::from_secs(900)));
        assert_eq!(p.max_spend_cu, Some(12.0));
        let env = start_env(&p, Duration::from_secs(300), &[]);
        assert_eq!(env[0], (ENV_DEADLINE.to_owned(), "3600".to_owned()));
        assert_eq!(env[1], (ENV_IDLE.to_owned(), "900".to_owned()));

        // Without a ledger entry the VM's own idle rule is kept.
        let p = restart_policy(
            None,
            Some(Duration::from_secs(60)),
            Duration::from_secs(1800),
        );
        assert_eq!(p.max_runtime, Some(Duration::from_secs(1800)));
        assert_eq!(p.idle_timeout, Some(Duration::from_secs(60)));
        assert_eq!(p.max_spend_cu, None);

        let failed = start_failed(&tracked);
        assert!(!failed.armed && !failed.cancelled);
        assert_eq!(failed.deadline, None);
        assert_eq!(failed.enforced_deadline(), None);
        assert_eq!(failed.idle_timeout, Some(Duration::from_secs(900)));
    }
}
