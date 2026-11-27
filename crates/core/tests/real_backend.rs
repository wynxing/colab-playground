//! `RealBackend` against the fake Colab toolchain (`tests/fake/`), unix only.
//!
//! Every test builds an isolated world in a temporary directory: the fake
//! `colab`, the fake CLI interpreter (which runs the real
//! `account_helper.py` with fake keyring/google-auth modules) and a fake
//! `ssh`, all given by absolute path; `HOME` and the data directories point
//! into the temp dir. No real `colab`, keyring or network is ever used.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use colab_playground_core::accounts::{Account, AccountId, Helper};
use colab_playground_core::backend::{
    Backend, PluginRunEvent, RunTarget, Runtime, RuntimeRef, StopOptions,
};
use colab_playground_core::cli::Cli;
use colab_playground_core::guard::GuardPolicy;
use colab_playground_core::http::NoHttp;
use colab_playground_core::paths::HostPath;
use colab_playground_core::plugins::InputValue;
use colab_playground_core::provision::{ProvisionEvent, ProvisionSpec, WATCHDOG_ATTEMPTS};
use colab_playground_core::real::{RealBackend, RealConfig};
use colab_playground_core::sessions::{Accelerator, StopReason, WatchdogInfo};
use colab_playground_core::tunnel::{TunnelEvent, TunnelSpec, TunnelState};
use colab_playground_core::vm::RunState;
use colab_playground_core::{CancelToken, Defaults, ErrorKind, Host, ManualClock, Toolchain};
use serde_json::{Value, json};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    cfg: RealConfig,
    b: RealBackend,
}

fn nc() -> CancelToken {
    CancelToken::new()
}

fn token(name: &str) -> String {
    json!({
        "token": format!("fake-access-{name}"),
        "refresh_token": format!("rt-{name}"),
        "client_id": "fake-client",
        "client_secret": "fake-secret",
        "token_uri": "https://oauth2.googleapis.com/token",
    })
    .to_string()
}

fn install(src: &Path, dst: &Path, exec: bool) {
    std::fs::copy(src, dst).unwrap();
    if exec {
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let fake = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fake");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    install(&fake.join("colab.py"), &bin.join("colab"), true);
    install(&fake.join("python_shim.py"), &bin.join("fake-python"), true);
    install(&fake.join("ssh.py"), &bin.join("ssh"), true);
    install(
        &fake.join("fake_common.py"),
        &bin.join("fake_common.py"),
        false,
    );
    let state = root.join("state");
    let home = root.join("home");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let keyring = json!({ "google-colab-cli\u{0}oauth_token": token("alice") });
    std::fs::write(state.join("keyring.json"), keyring.to_string()).unwrap();
    let server = json!({
        "accounts": {
            "alice": {"balance": 50.0, "assignments": {}},
            "bob": {"balance": 80.0, "assignments": {}}
        },
        "knobs": {}
    });
    std::fs::write(state.join("server.json"), server.to_string()).unwrap();
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let cfg = RealConfig {
        host: Host::Local,
        toolchain: Some(Toolchain {
            colab: s(&bin.join("colab")),
            helper_python: s(&bin.join("fake-python")),
        }),
        local_dir: root.join("local"),
        host_dir: Some(HostPath::new(s(&root.join("hostdata")))),
        base_env: vec![
            ("HOME".into(), s(&home)),
            ("CPG_FAKE_STATE".into(), s(&state)),
            ("CPG_SSH".into(), s(&bin.join("ssh"))),
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
        ],
        clock: Arc::new(ManualClock::fixed()),
        http: Arc::new(NoHttp),
        defaults: Defaults::default(),
        run_poll_interval: Duration::from_secs(1),
        guard_keep_timeout: Duration::from_secs(45),
    };
    let b = RealBackend::new(cfg.clone()).unwrap();
    Fixture {
        _tmp: tmp,
        root,
        state,
        cfg,
        b,
    }
}

impl Fixture {
    fn server(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.state.join("server.json")).unwrap())
            .unwrap()
    }

    fn edit_server(&self, f: impl FnOnce(&mut Value)) {
        let mut v = self.server();
        f(&mut v);
        std::fs::write(self.state.join("server.json"), v.to_string()).unwrap();
    }

    fn knob(&self, k: &str, v: Value) {
        self.edit_server(|s| s["knobs"][k] = v);
    }

    fn assignments(&self, account: &str) -> serde_json::Map<String, Value> {
        self.server()["accounts"][account]["assignments"]
            .as_object()
            .cloned()
            .unwrap_or_default()
    }

    fn slot(&self) -> String {
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(self.state.join("keyring.json")).unwrap(),
        )
        .unwrap();
        v["google-colab-cli\u{0}oauth_token"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.state.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn history_path(&self, session: &str) -> PathBuf {
        self.root
            .join("home/.config/colab-cli/history")
            .join(format!("{session}.jsonl"))
    }

    fn history(&self, session: &str) -> String {
        std::fs::read_to_string(self.history_path(session)).unwrap_or_default()
    }

    fn helper(&self) -> Helper {
        let tc = self.cfg.toolchain.clone().unwrap();
        Helper::new(Host::Local, tc.helper_python, self.cfg.base_env.clone())
    }

    fn state_file(&self, id: &AccountId) -> PathBuf {
        self.root
            .join("hostdata")
            .join("accounts")
            .join(id.as_str())
            .join("sessions.json")
    }

    fn adopt_alice(&self) -> Account {
        let v = self.b.accounts(&nc()).unwrap();
        assert!(v.unadopted);
        assert_eq!(v.active_email.as_deref(), Some("alice@example.com"));
        self.b.adopt_active_account(&nc()).unwrap()
    }

    fn add_bob(&self) -> Account {
        let flow = self.b.begin_add_account(&nc()).unwrap();
        flow.submit_code("good-code", &nc()).unwrap()
    }

    /// Every `colab exec` that started the watchdog (only the start sends hooks).
    fn watchdog_starts(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter_map(|c| {
                let argv: Vec<String> = serde_json::from_value(c["argv"].clone()).ok()?;
                argv.iter()
                    .any(|a| a.starts_with("CPG_WD_HOOKS="))
                    .then_some(argv)
            })
            .collect()
    }

    fn listed_watchdog(&self, name: &str) -> (Runtime, WatchdogInfo) {
        let rt = self
            .b
            .runtimes(&nc())
            .unwrap()
            .runtimes
            .into_iter()
            .find(|r| r.id.name.as_deref() == Some(name))
            .unwrap();
        let wd = rt.tracked.as_ref().unwrap().watchdog.clone().unwrap();
        (rt, wd)
    }

    fn provision(
        &self,
        name: &str,
        accel: Accelerator,
    ) -> colab_playground_core::provision::ProvisionOutcome {
        let (tx, _rx) = channel();
        self.b
            .provision(
                ProvisionSpec::new(name, accel, &Defaults::default()),
                tx,
                &nc(),
            )
            .unwrap()
    }
}

fn wait_for<T>(rx: &Receiver<T>, secs: u64, mut pred: impl FnMut(&T) -> bool) -> Option<T> {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if let Ok(v) = rx.recv_timeout(Duration::from_millis(200))
            && pred(&v)
        {
            return Some(v);
        }
    }
    None
}

#[test]
fn cli_version_and_missing_toolchain() {
    let f = fixture();
    let tc = f.cfg.toolchain.clone().unwrap();
    let cli = Cli::new(Host::Local, tc.colab, f.cfg.base_env.clone());
    assert_eq!(cli.version(&nc()).unwrap(), "0.8.0-fake");

    let mut cfg = f.cfg.clone();
    cfg.toolchain = Some(Toolchain {
        colab: "/nonexistent/colab".into(),
        helper_python: "/nonexistent/python".into(),
    });
    cfg.local_dir = f.root.join("local-missing");
    let b = RealBackend::new(cfg).unwrap();
    assert!(b.accounts(&nc()).unwrap_err().is(ErrorKind::NotInstalled));
}

#[test]
fn login_rollback_cancel_drop_and_success() {
    let f = fixture();
    let alice = f.adopt_alice();
    assert_eq!(alice.email.as_deref(), Some("alice@example.com"));

    let flow = f.b.begin_add_account(&nc()).unwrap();
    assert!(flow.auth_url().starts_with("https://accounts.google.com/"));
    let err = flow.submit_code("wrong-code", &nc()).unwrap_err();
    assert!(err.is(ErrorKind::LoginInvalidCode), "{err}");
    assert_eq!(
        f.slot(),
        token("alice"),
        "the previous credential is restored"
    );
    let v = f.b.accounts(&nc()).unwrap();
    assert_eq!(v.active.as_ref(), Some(&alice.id));
    assert!(!v.recovered_pending_add);

    f.b.begin_add_account(&nc()).unwrap().cancel().unwrap();
    assert_eq!(f.slot(), token("alice"));
    {
        let _abandoned = f.b.begin_add_account(&nc()).unwrap();
    }
    assert_eq!(f.slot(), token("alice"));

    let bob = f.add_bob();
    assert_eq!(bob.email.as_deref(), Some("bob@example.com"));
    let v = f.b.accounts(&nc()).unwrap();
    assert_eq!(v.active.as_ref(), Some(&bob.id));
    assert_eq!(v.accounts.len(), 2);
    // No token ever reached the app-side data directory.
    let accounts_json = std::fs::read_to_string(f.cfg.local_dir.join("accounts.json")).unwrap();
    assert!(!accounts_json.contains("fake-access") && !accounts_json.contains("rt-"));
}

#[test]
fn per_account_state_files_switching_and_cross_account_stop() {
    let f = fixture();
    let alice = f.adopt_alice();
    let bob = f.add_bob();
    f.provision("bob-t4", Accelerator::T4);
    let impact = f.b.switch_impact(&alice.id).unwrap();
    assert_eq!(impact.sessions_left_running.len(), 1);
    f.b.switch_account(&alice.id, &nc()).unwrap();
    f.provision("alice-cpu", Accelerator::None);
    // `colab sessions` under alice did not prune bob's state file.
    assert!(
        std::fs::read_to_string(f.state_file(&bob.id))
            .unwrap()
            .contains("bob-t4")
    );
    // Every account-touching call carried the active account's own --config.
    let ids: BTreeMap<&str, &AccountId> = [("alice", &alice.id), ("bob", &bob.id)]
        .into_iter()
        .collect();
    for call in f.calls() {
        let argv: Vec<String> = serde_json::from_value(call["argv"].clone()).unwrap();
        if argv[0] == "whoami" || argv[0] == "version" {
            continue;
        }
        assert_eq!(argv[0], "--config", "{argv:?}");
        let who = call["account"].as_str().unwrap();
        assert_eq!(
            argv[1],
            f.state_file(ids[who]).to_string_lossy(),
            "{argv:?}"
        );
    }
    // Runtimes of every account, managed or not, can be listed and stopped.
    f.edit_server(|s| {
        s["accounts"]["alice"]["assignments"]["browser-1"] =
            json!({"accelerator": "L4", "variant": "GPU", "shape": "STANDARD"});
    });
    let rt = f.b.runtimes(&nc()).unwrap();
    assert!(rt.errors.is_empty(), "{:?}", rt.errors);
    assert_eq!(rt.runtimes.len(), 3);
    let bobs = rt.runtimes.iter().find(|r| r.id.account == bob.id).unwrap();
    assert!(!bobs.on_active_account && bobs.managed);
    let err =
        f.b.extend_session(&bobs.id, Duration::from_secs(3600), &nc())
            .unwrap_err();
    assert!(err.is(ErrorKind::AccountNotActive));
    assert_eq!(err.param("account"), Some(bob.id.as_str()));
    f.b.stop_session(&bobs.id, StopOptions::default(), &nc())
        .unwrap();
    assert!(f.assignments("bob").is_empty());
    let unmanaged = rt
        .runtimes
        .iter()
        .find(|r| r.id.endpoint == "browser-1")
        .unwrap();
    assert!(!unmanaged.managed && unmanaged.id.name.is_none());
    f.b.stop_session(&unmanaged.id, StopOptions::default(), &nc())
        .unwrap();
    assert_eq!(f.assignments("alice").len(), 1);
}

#[test]
fn provision_retries_to_80_gb_and_enforces_entitlement_and_cap() {
    let f = fixture();
    f.adopt_alice();
    let mut spec = ProvisionSpec::new("big", Accelerator::A100, &Defaults::default());
    spec.min_vram_mib = Some(70_000);
    let (tx, rx) = channel();
    let out = f.b.provision(spec, tx, &nc()).unwrap();
    assert_eq!(out.attempts, 2);
    assert_eq!(out.hardware.vram_mib, Some(81_920));
    assert!(out.watchdog.is_some());
    let events: Vec<ProvisionEvent> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Rejected { attempt: 1, .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::WatchdogStarted { .. }))
    );
    let mine = f.assignments("alice");
    assert_eq!(mine.len(), 1, "the 40 GB VM was released");
    let only = mine.values().next().unwrap();
    assert_eq!(only["vram"], 81_920);
    assert_eq!(only["watchdog"]["grace_s"], 300.0);

    f.knob("deny", json!(["H100"]));
    let (tx, _rx) = channel();
    let err =
        f.b.provision(
            ProvisionSpec::new("h", Accelerator::H100, &Defaults::default()),
            tx,
            &nc(),
        )
        .unwrap_err();
    assert!(err.is(ErrorKind::NotEntitled));
    assert_eq!(f.assignments("alice").len(), 1);

    f.provision("t4a", Accelerator::T4);
    let (tx, _rx) = channel();
    let err =
        f.b.provision(
            ProvisionSpec::new("t4b", Accelerator::T4, &Defaults::default()),
            tx,
            &nc(),
        )
        .unwrap_err();
    assert!(err.is(ErrorKind::SessionLimitReached));
}

/// A request Colab never answers fails once (the CLI already retried), and a
/// cancel kills the waiting `colab new` at once and stops the VM the request
/// got anyway.
#[test]
fn a_request_colab_never_answers_fails_once_and_a_cancel_kills_it() {
    let f = fixture();
    f.adopt_alice();
    f.knob("no_answer", json!(["A100"]));
    let spec = |name| ProvisionSpec::new(name, Accelerator::A100, &Defaults::default());
    let (tx, _rx) = channel();
    let err = f.b.provision(spec("na"), tx, &nc()).unwrap_err();
    assert!(err.is(ErrorKind::AllocationTimeout), "{err}");
    let news = f
        .calls()
        .iter()
        .filter(|c| {
            c["argv"]
                .as_array()
                .is_some_and(|a| a.contains(&json!("new")))
        })
        .count();
    assert_eq!(news, 1, "not retried");
    assert!(f.assignments("alice").is_empty());

    f.knob("no_answer_hangs", json!(true));
    let cancel = CancelToken::new();
    let canceller = {
        let cancel = cancel.clone();
        let server = f.state.join("server.json");
        std::thread::spawn(move || {
            // Cancel once the hanging request has got its VM.
            let end = Instant::now() + Duration::from_secs(30);
            while Instant::now() < end {
                let s: Value = std::fs::read_to_string(&server)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default();
                if s["accounts"]["alice"]["assignments"]
                    .as_object()
                    .is_some_and(|a| !a.is_empty())
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            cancel.cancel();
        })
    };
    let started = Instant::now();
    let (tx, rx) = channel();
    let err = f.b.provision(spec("nb"), tx, &cancel).unwrap_err();
    canceller.join().unwrap();
    assert!(err.is(ErrorKind::Cancelled), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the cancel waited for `colab new`: {:?}",
        started.elapsed()
    );
    let events: Vec<ProvisionEvent> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::OrphanDetected { released: true, .. })),
        "{events:?}"
    );
    assert!(
        f.assignments("alice").is_empty(),
        "the VM the request got anyway was left: {:?}",
        f.assignments("alice")
    );
}

/// Setting an auto-stop after a cancel puts it back on the runtime: the
/// watchdog is started again the way provisioning starts it.
#[test]
fn set_auto_stop_after_cancel_starts_the_watchdog_again() {
    let f = fixture();
    f.adopt_alice();
    let mut spec = ProvisionSpec::new("wd", Accelerator::T4, &Defaults::default());
    spec.guard.idle_timeout = Some(Duration::from_secs(900));
    let (tx, _rx) = channel();
    f.b.provision(spec, tx, &nc()).unwrap();
    let vm = |f: &Fixture| f.assignments("alice").values().next().unwrap()["watchdog"].clone();
    assert_eq!(f.watchdog_starts().len(), 1);
    let (rt, wd) = f.listed_watchdog("wd");
    assert!(wd.armed && !wd.cancelled);

    // The cancel reply still shows the watchdog running (it exits at its
    // next poll), yet it is not armed any more.
    f.b.cancel_auto_stop(&rt.id, &nc()).unwrap();
    let (rt, wd) = f.listed_watchdog("wd");
    assert!(!wd.armed && wd.cancelled);
    assert_eq!(rt.auto_stop_at, None);
    assert_eq!(vm(&f)["running"], false);

    let hour = Duration::from_secs(3600);
    let info = f.b.set_auto_stop(&rt.id, hour, &nc()).unwrap();
    assert!(info.armed && !info.cancelled && info.deadline.is_some());
    assert_eq!(info.idle_timeout, Some(Duration::from_secs(900)));
    let starts = f.watchdog_starts();
    assert_eq!(starts.len(), 2, "the watchdog was started again");
    for env in [
        "CPG_WD_DEADLINE_S=3600",
        "CPG_WD_IDLE_S=900",
        "CPG_WD_GRACE_S=300",
    ] {
        assert!(starts[1].contains(&env.to_owned()), "{:?}", starts[1]);
    }
    assert_eq!(vm(&f)["running"], true);
    assert_eq!(vm(&f)["cancelled"], false);
    assert_eq!(vm(&f)["idle_timeout_s"], 900.0);
    let (rt, wd) = f.listed_watchdog("wd");
    assert_eq!(wd, info, "the listing shows the last known state");
    let tracked = rt.tracked.clone().unwrap();
    assert_eq!(
        tracked.started_at() + tracked.policy.max_runtime.unwrap(),
        f.b.now() + hour
    );
    assert!(rt.auto_stop_at.is_some());

    // A running watchdog only gets the new deadline.
    assert!(f.b.set_auto_stop(&rt.id, 2 * hour, &nc()).unwrap().armed);
    assert_eq!(f.watchdog_starts().len(), 2);

    // One that exited without a cancel is started again too.
    f.edit_server(|s| {
        for a in s["accounts"]["alice"]["assignments"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            a["watchdog"]["running"] = json!(false);
        }
    });
    assert!(f.b.set_auto_stop(&rt.id, hour, &nc()).unwrap().armed);
    assert_eq!(f.watchdog_starts().len(), 3);

    // It cannot be started: the call says the stop is not on the runtime and
    // the app-side limit still moves to the new time.
    f.b.cancel_auto_stop(&rt.id, &nc()).unwrap();
    f.knob("watchdog_start_fails", json!(true));
    let half = hour / 2;
    let info = f.b.set_auto_stop(&rt.id, half, &nc()).unwrap();
    assert!(!info.armed && !info.cancelled);
    assert_eq!(info.deadline, None);
    assert_eq!(
        f.watchdog_starts().len(),
        3 + WATCHDOG_ATTEMPTS as usize,
        "tried like provisioning tries"
    );
    let (rt, wd) = f.listed_watchdog("wd");
    assert_eq!(wd, info);
    let tracked = rt.tracked.clone().unwrap();
    assert_eq!(
        tracked.started_at() + tracked.policy.max_runtime.unwrap(),
        f.b.now() + half
    );
    assert_eq!(rt.auto_stop_at, Some(f.b.now() + half));

    // Once it can start, trying again puts the stop back on the runtime.
    f.knob("watchdog_start_fails", json!(false));
    assert!(f.b.set_auto_stop(&rt.id, hour, &nc()).unwrap().armed);
    assert_eq!(vm(&f)["running"], true);
}

/// Extend adds to the stop the runtime has when the call is made, so two in a
/// row with no listing in between add twice.
#[test]
fn extend_adds_to_the_stop_the_session_has_at_that_moment() {
    let f = fixture();
    f.adopt_alice();
    let spec = ProvisionSpec::new("ext", Accelerator::T4, &Defaults::default());
    let (tx, _rx) = channel();
    f.b.provision(spec, tx, &nc()).unwrap();
    let (rt, _) = f.listed_watchdog("ext");
    let left = rt.auto_stop_at.unwrap().duration_since(f.b.now()).unwrap();
    // The fake VM's clock runs while the backend's stands still, and the
    // deadline is sent in whole seconds.
    let close = |actual: Duration, expected: Duration| {
        assert!(
            actual.abs_diff(expected) <= Duration::from_secs(30),
            "{actual:?}, expected about {expected:?}"
        );
    };

    let hour = Duration::from_secs(3600);
    let set = f.b.extend_session(&rt.id, hour, &nc()).unwrap();
    assert!(set.watchdog.armed);
    close(set.runtime, left + hour);
    let set = f.b.extend_session(&rt.id, hour, &nc()).unwrap();
    close(set.runtime, left + 2 * hour);
    assert_eq!(
        f.watchdog_starts().len(),
        1,
        "the running watchdog was moved"
    );
    let (rt, wd) = f.listed_watchdog("ext");
    assert_eq!(wd, set.watchdog);
    close(
        rt.auto_stop_at.unwrap().duration_since(f.b.now()).unwrap(),
        left + 2 * hour,
    );

    // Without an auto-stop there is nothing to add to.
    f.b.cancel_auto_stop(&rt.id, &nc()).unwrap();
    let set = f.b.extend_session(&rt.id, hour, &nc()).unwrap();
    assert!(set.watchdog.armed);
    assert_eq!(set.runtime, hour);
    assert_eq!(f.watchdog_starts().len(), 2);
}

#[test]
fn staged_plugin_copies_do_not_outlive_their_preview() {
    let mut f = fixture();
    let src = f.root.join("plugin-src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("plugin.toml"),
        "api = 1\nid = \"demo\"\nversion = \"0.1.0\"\n[name]\nen = \"Demo\"\n[run]\nscript = \"main.py\"\n",
    )
    .unwrap();
    std::fs::write(src.join("main.py"), "print('hi')\n").unwrap();
    let staging = f.cfg.local_dir.join("staging");
    let staged = || -> Vec<PathBuf> {
        std::fs::read_dir(&staging)
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    };

    // Closing the confirmation drops the copy.
    let preview = f.b.inspect_plugin_folder(&src).unwrap();
    assert_eq!(staged(), std::slice::from_ref(&preview.staged_dir));
    f.b.discard_plugin_preview(&preview);
    assert!(staged().is_empty());

    // So does an install that fails.
    let preview = f.b.inspect_plugin_folder(&src).unwrap();
    std::fs::remove_file(preview.staged_dir.join("main.py")).unwrap();
    assert!(f.b.install_plugin(&preview).is_err());
    assert!(staged().is_empty());

    // What a run that did not get to either left behind goes at the next start.
    f.b.inspect_plugin_folder(&src).unwrap();
    std::fs::create_dir_all(staging.join("git-0123456789abcdef").join(".git")).unwrap();
    assert_eq!(staged().len(), 2);
    f.b = RealBackend::new(f.cfg.clone()).unwrap();
    assert!(staged().is_empty());

    let preview = f.b.inspect_plugin_folder(&src).unwrap();
    f.b.install_plugin(&preview).unwrap();
    assert!(staged().is_empty());
    assert!(f.b.plugins().iter().any(|p| p.manifest.id == "demo"));
}

#[test]
fn plugin_run_passes_secrets_only_via_env_and_redacts_them() {
    let f = fixture();
    f.adopt_alice();
    f.b.set_secret("hf_token", "hf_SECRET123", &nc()).unwrap();
    assert!(f.b.has_secret("hf_token", &nc()).unwrap());
    let mut spec = f.b.plugin_run_template("llama-cpp").unwrap();
    spec.inputs.insert(
        "repo".into(),
        InputValue::Text("unsloth/Qwen3-0.6B-GGUF".into()),
    );
    spec.inputs
        .insert("api_key".into(), InputValue::Text("sk-SECRET456".into()));
    let (tx, rx) = channel();
    let out = f.b.run_plugin(spec, tx, &nc()).unwrap();
    assert_eq!(out.status.state, RunState::Ready);
    assert_eq!(out.endpoints[0].remote_port, 8000);
    let events: Vec<PluginRunEvent> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PluginRunEvent::Launched { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PluginRunEvent::Log { .. }))
    );
    let text = serde_json::to_string(&events).unwrap();
    assert!(
        !text.contains("hf_SECRET123") && !text.contains("sk-SECRET456"),
        "secrets leaked into events"
    );
    let run = &f.server()["runs"][&out.run_id];
    assert_eq!(run["env"]["PLUGIN_HF_TOKEN"], "hf_SECRET123");
    assert_eq!(run["env"]["PLUGIN_INPUT_API_KEY"], "sk-SECRET456");
    assert_eq!(run["env"]["PLUGIN_INPUT_REPO"], "unsloth/Qwen3-0.6B-GGUF");
    // Check the secret *values*: the env var name CPG_SECRET_KEYS legitimately
    // appears in recorded cells.
    let leaked = |text: &str| text.contains("hf_SECRET123") || text.contains("sk-SECRET456");
    for entry in walk(&f.cfg.local_dir) {
        let content = std::fs::read(&entry).unwrap_or_default();
        let content = String::from_utf8_lossy(&content);
        assert!(!leaked(&content), "secret written to {}", entry.display());
    }
    // The CLI recorded the launch cell with its --env prelude; it was scrubbed.
    let history = f.history(&out.session);
    assert!(
        history.contains("PLUGIN_HF_TOKEN") && history.contains("***"),
        "{history}"
    );
    assert!(
        !leaked(&history),
        "secret left in the CLI history: {history}"
    );
    // The status polls (whose printed log carried the secret) were recorded
    // and scrubbed too, not dropped.
    assert!(history.contains("poll 0 (debug secret=***)"), "{history}");
    assert!(
        history.contains("--query-gpu=name,memory.total"),
        "other cells kept"
    );
    let mode = std::fs::metadata(f.history_path(&out.session))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // A failing stage stops the session provisioned for the run.
    f.knob("plugin_fail_stage", json!("download"));
    let before = f.assignments("alice").len();
    let mut spec = f.b.plugin_run_template("llama-cpp").unwrap();
    spec.inputs.insert(
        "repo".into(),
        InputValue::Text("unsloth/Qwen3-0.6B-GGUF".into()),
    );
    if let RunTarget::Provision(p) = &mut spec.target {
        p.accelerator = Accelerator::T4;
    }
    let (tx, _rx) = channel();
    let err = f.b.run_plugin(spec, tx, &nc()).unwrap_err();
    assert!(err.is(ErrorKind::PluginRunFailed));
    assert_eq!(err.param("stage"), Some("download"));
    assert!(!leaked(err.param("message").unwrap_or_default()));
    let failed_session = f
        .calls()
        .iter()
        .rev()
        .find_map(|c| {
            let argv: Vec<String> = serde_json::from_value(c["argv"].clone()).ok()?;
            let i = argv.iter().position(|a| a == "exec")?;
            (argv.get(i + 1)? == "-s").then(|| argv[i + 2].clone())
        })
        .unwrap();
    assert_ne!(failed_session, out.session);
    let failed_history = f.history(&failed_session);
    assert!(failed_history.contains("download"), "{failed_history}");
    assert!(
        !leaked(&failed_history),
        "secret left after a failed run: {failed_history}"
    );
    assert_eq!(
        f.assignments("alice").len(),
        before,
        "no billed VM left behind"
    );
}

#[test]
fn plugin_run_shows_the_whole_log_without_waiting_for_what_is_already_written() {
    // `backlog` lines are written by each step of the script on top of the one a poll returns.
    let run = |backlog: u64, fail: bool| {
        let f = fixture();
        f.adopt_alice();
        f.knob("plugin_log_backlog", json!(backlog));
        if fail {
            f.knob("plugin_fail_stage", json!("download"));
            f.knob("plugin_log_skipped", json!(3000));
        }
        let mut spec = f.b.plugin_run_template("llama-cpp").unwrap();
        spec.inputs.insert(
            "repo".into(),
            InputValue::Text("unsloth/Qwen3-0.6B-GGUF".into()),
        );
        let (tx, rx) = channel();
        let before = f.cfg.clock.now();
        match f.b.run_plugin(spec, tx, &nc()) {
            Ok(out) => assert!(!fail && out.status.state == RunState::Ready),
            Err(e) => assert!(fail && e.is(ErrorKind::PluginRunFailed), "{e:?}"),
        }
        let took = f.cfg.clock.now().duration_since(before).unwrap();
        let lines: Vec<String> = rx
            .try_iter()
            .filter_map(|e| match e {
                PluginRunEvent::Log { lines } => Some(lines),
                PluginRunEvent::LogSkipped { path } => Some(vec![format!("not shown, see {path}")]),
                _ => None,
            })
            .flatten()
            .collect();
        (lines, took)
    };
    let (short, usual) = run(0, false);
    assert_eq!(short.len(), 6, "{short:?}");

    let whole = |steps: usize| -> Vec<String> {
        (0..steps)
            .flat_map(|step| {
                std::iter::once(format!("poll {step} (debug secret=)"))
                    .chain((0..3).map(move |i| format!("poll {step} line {i}")))
            })
            .collect()
    };
    let (lines, took) = run(3, false);
    assert_eq!(lines, whole(6), "the lines written last are shown too");
    assert_eq!(took, usual, "no wait before reading what is already there");
    let (lines, _) = run(3, true);
    assert!(
        lines[0].starts_with("not shown, see /root/.colab-playground/runs/")
            && lines[0].ends_with("/run.log"),
        "{}",
        lines[0]
    );
    let mut rest = whole(3);
    rest.push("exiting after the failure".into());
    assert_eq!(lines[1..], rest, "a failed run shows its last lines");
}

#[test]
fn plugin_run_cancelled_while_its_last_log_lines_are_read_ends_as_cancelled() {
    let f = fixture();
    f.adopt_alice();
    f.knob("plugin_log_backlog", json!(6));
    let mut spec = f.b.plugin_run_template("llama-cpp").unwrap();
    spec.inputs.insert(
        "repo".into(),
        InputValue::Text("unsloth/Qwen3-0.6B-GGUF".into()),
    );
    let (tx, rx) = channel();
    let cancel = CancelToken::new();
    let watcher = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            for event in rx {
                // The first line read after the script reported that it is ready.
                if matches!(&event, PluginRunEvent::Log { lines } if lines.iter().any(|l| l == "poll 5 line 0"))
                {
                    cancel.cancel();
                }
            }
        })
    };
    let err = f.b.run_plugin(spec, tx, &cancel).unwrap_err();
    watcher.join().unwrap();
    assert!(err.is(ErrorKind::Cancelled), "{err:?}");
    assert!(
        f.assignments("alice").is_empty(),
        "the runtime started for the run is stopped"
    );
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p))
            } else {
                out.push(p)
            }
        }
    }
    out
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A switch that cannot start, because the active account is still at work, changes
/// nothing: the port forwards stay open.
#[test]
fn a_switch_that_cannot_start_leaves_the_tunnels_open() {
    let f = fixture();
    let alice = f.adopt_alice();
    let bob = f.add_bob();
    f.provision("tun", Accelerator::T4);
    let (tx, rx) = channel();
    f.b.open_tunnel(
        TunnelSpec {
            account: bob.id.clone(),
            session: "tun".into(),
            local_port: free_port(),
            remote_port: 8000,
            health_path: None,
        },
        tx,
    )
    .unwrap();
    let up = |e: &TunnelEvent| {
        matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Up,
                ..
            }
        )
    };
    assert!(wait_for(&rx, 60, up).is_some(), "tunnel never came up");

    // A request Colab does not answer keeps the account busy until it is cancelled.
    f.knob("no_answer", json!(["A100"]));
    f.knob("no_answer_hangs", json!(true));
    let cancel = CancelToken::new();
    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| {
            let (tx, _rx) = channel();
            let spec = ProvisionSpec::new("busy", Accelerator::A100, &Defaults::default());
            f.b.provision(spec, tx, &cancel)
        });
        let end = Instant::now() + Duration::from_secs(30);
        while f.assignments("bob").len() < 2 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        let err = f.b.switch_account(&alice.id, &nc()).unwrap_err();
        cancel.cancel();
        assert!(
            waiting
                .join()
                .unwrap()
                .unwrap_err()
                .is(ErrorKind::Cancelled)
        );
        assert!(err.is(ErrorKind::Busy), "{err}");
    });
    assert_eq!(
        f.b.tunnels().len(),
        1,
        "the refused switch closed the tunnel"
    );
    assert_eq!(f.b.accounts(&nc()).unwrap().active, Some(bob.id.clone()));

    f.b.switch_account(&alice.id, &nc()).unwrap();
    assert!(f.b.tunnels().is_empty());
}

#[test]
fn tunnel_uses_per_account_proxy_command_and_never_auto_creates() {
    let f = fixture();
    let alice = f.adopt_alice();
    f.provision("tun1", Accelerator::T4);
    let port = free_port();
    let spec = TunnelSpec {
        account: alice.id.clone(),
        session: "tun1".into(),
        local_port: port,
        remote_port: 8000,
        health_path: None,
    };
    let (tx, rx) = channel();
    let id = f.b.open_tunnel(spec.clone(), tx).unwrap();
    assert!(
        wait_for(&rx, 60, |e| matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Up,
                ..
            }
        ))
        .is_some(),
        "tunnel never came up"
    );
    let calls = std::fs::read_to_string(f.state.join("ssh_calls.jsonl")).unwrap();
    let expected = format!(
        "ProxyCommand={} --config {} ssh --proxy-mode -s tun1",
        f.cfg.toolchain.as_ref().unwrap().colab,
        f.state_file(&alice.id).display()
    );
    assert!(
        calls.contains(&serde_json::to_string(&expected).unwrap()),
        "{calls}"
    );
    assert!(calls.contains(&format!("\"127.0.0.1:{port}:127.0.0.1:8000\"")));
    f.b.close_tunnel(id).unwrap();
    assert!(f.b.tunnels().is_empty());

    // After the session is gone, a reconnect must not let --proxy-mode create a VM.
    let rt = f.b.runtimes(&nc()).unwrap();
    f.b.stop_session(&rt.runtimes[0].id, StopOptions::default(), &nc())
        .unwrap();
    let (tx, rx) = channel();
    f.b.open_tunnel(spec, tx).unwrap();
    let failed = wait_for(&rx, 30, |e| {
        matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Failed { .. },
                ..
            }
        )
    });
    match failed {
        Some(TunnelEvent::State {
            state: TunnelState::Failed { error },
            ..
        }) => assert!(error.is(ErrorKind::SessionGone), "{error}"),
        other => panic!("{other:?}"),
    }
    assert!(
        f.server().get("autocreated").is_none(),
        "a runtime was auto-created"
    );
    assert!(f.assignments("alice").is_empty());
}

#[test]
fn tunnel_reports_429_and_port_in_use() {
    let f = fixture();
    let alice = f.adopt_alice();
    f.provision("busy", Accelerator::T4);
    f.knob("ssh_busy", json!(true));
    let (tx, rx) = channel();
    let spec = TunnelSpec {
        account: alice.id.clone(),
        session: "busy".into(),
        local_port: free_port(),
        remote_port: 8000,
        health_path: None,
    };
    f.b.open_tunnel(spec.clone(), tx).unwrap();
    match wait_for(&rx, 90, |e| {
        matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Failed { .. },
                ..
            }
        )
    }) {
        Some(TunnelEvent::State {
            state: TunnelState::Failed { error },
            ..
        }) => assert!(error.is(ErrorKind::SshBusy), "{error}"),
        other => panic!("{other:?}"),
    }
    f.knob("ssh_busy", json!(false));
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (tx, rx) = channel();
    let spec = TunnelSpec {
        local_port: taken.local_addr().unwrap().port(),
        ..spec
    };
    f.b.open_tunnel(spec, tx).unwrap();
    match wait_for(&rx, 30, |e| {
        matches!(
            e,
            TunnelEvent::State {
                state: TunnelState::Failed { .. },
                ..
            }
        )
    }) {
        Some(TunnelEvent::State {
            state: TunnelState::Failed { error },
            ..
        }) => assert!(error.is(ErrorKind::PortInUse), "{error}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn usage_guard_stop_and_vanished_sessions() {
    let f = fixture();
    let alice = f.adopt_alice();
    let views = f.b.refresh_usage(&[], &nc()).unwrap();
    let snap = views[0].snapshot.unwrap();
    assert_eq!(snap.balance_cu, 50.0);
    assert!(!snap.stale);

    let mut spec = ProvisionSpec::new("floor", Accelerator::T4, &Defaults::default());
    spec.guard = GuardPolicy {
        min_balance_cu: Some(40.0),
        ..GuardPolicy::unlimited()
    };
    let (tx, _rx) = channel();
    f.b.provision(spec, tx, &nc()).unwrap();
    let r = f.b.guard_poll(&nc()).unwrap();
    assert_eq!(r.decisions.len(), 1);
    assert!(!r.decisions[0].stopped);
    let rates = f.b.measured_rates();
    assert_eq!(rates.len(), 1, "{rates:?}");
    assert!(rates[0].matches(Accelerator::T4, false));
    assert_eq!(rates[0].reading.cu_per_hour, 1.76);
    f.edit_server(|s| s["accounts"]["alice"]["balance"] = json!(30.0));
    let r = f.b.guard_poll(&nc()).unwrap();
    assert!(r.decisions[0].stopped, "{:?}", r.decisions);
    assert!(f.assignments("alice").is_empty());
    let rt = f.b.runtimes(&nc()).unwrap();
    assert!(rt.runtimes.is_empty());

    f.provision("vanish", Accelerator::T4);
    f.edit_server(|s| s["accounts"]["alice"]["assignments"] = json!({}));
    let r = f.b.guard_poll(&nc()).unwrap();
    assert_eq!(r.vanished.len(), 1);
    assert_eq!(
        r.vanished[0],
        RuntimeRef {
            account: alice.id.clone(),
            endpoint: r.vanished[0].endpoint.clone(),
            name: Some("vanish".into())
        }
    );
    let _ = StopReason::Vanished;
}

/// Keeps the ledger's next write in the returned file. The ledger is replaced through a
/// temporary file: as a link it sends that write to the returned file, and the ledger is
/// that link until a second write replaces it.
fn keep_next_ledger_write(f: &Fixture, to: &str) -> (PathBuf, PathBuf) {
    let ledger = f.root.join("local").join("ledger.json");
    let write = f.root.join(to);
    let temporary = ledger.with_file_name(format!(".ledger.json.{}.tmp", std::process::id()));
    std::os::unix::fs::symlink(&write, &temporary).unwrap();
    (ledger, write)
}

fn ledger_entries(file: &Path) -> Vec<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
}

#[test]
fn a_usage_round_writes_the_ledger_once() {
    // What runs, whether it ended before the round, whether the round is the guard's,
    // and how many rates the round measures: only a session running alone is measured.
    for (sessions, ended, guard, measured) in [
        (&["alone"][..], false, true, 1),
        (&["first", "second"][..], false, true, 0),
        (&["alone"][..], true, true, 0),
        (&["alone"][..], false, false, 1),
    ] {
        let f = fixture();
        f.adopt_alice();
        for name in sessions {
            f.provision(name, Accelerator::T4);
        }
        if ended {
            f.edit_server(|s| s["accounts"]["alice"]["assignments"] = json!({}));
        }
        let round = || {
            if guard {
                f.b.guard_poll(&nc()).unwrap();
            } else {
                f.b.refresh_usage(&[], &nc()).unwrap();
            }
        };
        let case = format!("{sessions:?}, ended: {ended}, guard round: {guard}");

        let (ledger, write) = keep_next_ledger_write(&f, "write.json");
        round();
        assert!(write.exists(), "nothing was written ({case})");
        assert!(ledger.is_symlink(), "written more than once ({case})");
        let saved = RealBackend::new(f.cfg.clone()).unwrap().measured_rates();
        assert_eq!(saved.len(), measured, "{saved:?} ({case})");
        if guard && !ended {
            let entries = ledger_entries(&write);
            assert_eq!(entries.len(), sessions.len(), "{entries:?} ({case})");
            for e in &entries {
                assert_eq!(e["spend"]["rate_known"], true, "{e} ({case})");
            }
        }

        // The backend's clock stands still, so the same round again reads what is saved.
        let (_, again) = keep_next_ledger_write(&f, "again.json");
        round();
        assert!(!again.exists(), "written with nothing new ({case})");
    }
}

/// Starts a session the guard stops in its next round: the balance is under its floor.
fn session_under_its_floor(f: &Fixture) {
    f.adopt_alice();
    let mut spec = ProvisionSpec::new("floor", Accelerator::T4, &Defaults::default());
    spec.guard = GuardPolicy {
        min_balance_cu: Some(40.0),
        ..GuardPolicy::unlimited()
    };
    let (tx, _rx) = channel();
    f.b.provision(spec, tx, &nc()).unwrap();
    f.edit_server(|s| s["accounts"]["alice"]["balance"] = json!(30.0));
}

#[test]
fn a_guard_stop_saves_what_the_round_read_before_it_stops() {
    let f = fixture();
    session_under_its_floor(&f);

    let (ledger, write) = keep_next_ledger_write(&f, "write.json");
    let r = f.b.guard_poll(&nc()).unwrap();
    assert!(r.decisions[0].stopped, "{:?}", r.decisions);
    let before = &ledger_entries(&write)[0];
    assert!(before["stopped"].is_null(), "{before}");
    assert_eq!(before["spend"]["rate_known"], true, "{before}");
    assert!(!before["measured_rate"].is_null(), "{before}");
    assert!(!ledger.is_symlink());
    let after = &ledger_entries(&ledger)[0];
    assert!(!after["stopped"].is_null(), "{after}");
}

#[test]
fn a_failed_stop_record_write_is_made_by_the_next_guard_round() {
    let f = fixture();
    session_under_its_floor(&f);
    let ledger = f.cfg.local_dir.join("ledger.json");
    let before = std::fs::read_to_string(&ledger).unwrap();
    // A folder where the file was makes every write fail.
    std::fs::remove_file(&ledger).unwrap();
    std::fs::create_dir(&ledger).unwrap();
    let r = f.b.guard_poll(&nc()).unwrap();
    assert!(r.decisions[0].stopped, "{:?}", r.decisions);
    std::fs::remove_dir(&ledger).unwrap();
    std::fs::write(&ledger, &before).unwrap();

    // No session is left for this round to read.
    assert!(f.b.guard_poll(&nc()).unwrap().decisions.is_empty());
    let e = &ledger_entries(&ledger)[0];
    assert!(!e["stopped"].is_null(), "{e}");
    assert_eq!(e["spend"]["rate_known"], true, "{e}");
}

#[test]
fn usage_reconciles_before_counting_managed_sessions() {
    for guard in [false, true] {
        let f = fixture();
        f.adopt_alice();
        f.provision("gone", Accelerator::T4);
        f.edit_server(|s| {
            s["accounts"]["alice"]["assignments"] = json!({
                "m-browser": {"accelerator": "T4", "high_mem": false}
            })
        });
        let views = if guard {
            let report = f.b.guard_poll(&nc()).unwrap();
            assert_eq!(report.vanished.len(), 1);
            report.usage
        } else {
            f.b.refresh_usage(&[], &nc()).unwrap()
        };
        assert!(views[0].ownership_verified);
        assert_eq!(views[0].unmanaged_runtimes, 1);
    }
}

#[test]
fn ownership_is_unverified_when_listing_fails_and_recovers_on_a_new_reading() {
    let f = fixture();
    f.adopt_alice();
    f.provision("managed", Accelerator::T4);
    f.knob("cli_api_broken", json!(true));
    for views in [
        f.b.refresh_usage(&[], &nc()).unwrap(),
        f.b.guard_poll(&nc()).unwrap().usage,
    ] {
        assert!(views[0].snapshot.is_some_and(|s| !s.stale));
        assert!(!views[0].ownership_verified);
    }
    f.knob("cli_api_broken", json!(false));
    f.b.runtimes(&nc()).unwrap();
    assert!(
        !f.b.usage()[0].ownership_verified,
        "listing does not rewrite a reading"
    );
    assert!(f.b.refresh_usage(&[], &nc()).unwrap()[0].ownership_verified);
}

#[test]
fn ownership_is_verified_without_live_ledger_entries_even_if_listing_fails() {
    for stopped in [false, true] {
        let f = fixture();
        f.adopt_alice();
        if stopped {
            f.provision("ended", Accelerator::T4);
            let target = f.b.runtimes(&nc()).unwrap().runtimes[0].id.clone();
            f.b.stop_session(&target, StopOptions::default(), &nc())
                .unwrap();
        }
        f.edit_server(|s| {
            s["accounts"]["alice"]["assignments"] = json!({
                "m-browser": {"accelerator": "T4"}
            });
        });
        f.knob("cli_api_broken", json!(true));
        let views = f.b.refresh_usage(&[], &nc()).unwrap();
        assert!(views[0].ownership_verified);
        assert_eq!(views[0].unmanaged_runtimes, 1);
        assert!(!views[0].snapshot.unwrap().stale);
    }
}

#[test]
fn runtime_listing_survives_ledger_write_failure() {
    let f = fixture();
    f.adopt_alice();
    f.provision("ended", Accelerator::T4);
    f.provision("running", Accelerator::T4);
    let running_endpoint =
        f.b.runtimes(&nc())
            .unwrap()
            .runtimes
            .into_iter()
            .find(|r| r.id.name.as_deref() == Some("running"))
            .unwrap()
            .id
            .endpoint;
    f.edit_server(|s| {
        s["accounts"]["alice"]["assignments"]
            .as_object_mut()
            .unwrap()
            .retain(|endpoint, _| endpoint == &running_endpoint);
    });
    let ledger_file = f.cfg.local_dir.join("ledger.json");
    std::fs::remove_file(&ledger_file).unwrap();
    std::fs::create_dir(&ledger_file).unwrap();
    let view = f.b.runtimes(&nc()).unwrap();
    assert_eq!(view.runtimes.len(), 1);
    assert_eq!(view.runtimes[0].id.name.as_deref(), Some("running"));
}

#[test]
fn endpoint_backfill_is_persisted_and_missing_names_do_not_end_sessions() {
    let mut f = fixture();
    let account = f.adopt_alice();
    f.provision("managed", Accelerator::T4);
    let state_file = f.state_file(&account.id);
    let state = std::fs::read_to_string(&state_file).unwrap();
    let ledger_file = f.cfg.local_dir.join("ledger.json");
    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_file).unwrap()).unwrap();
    ledger[0]["endpoint"] = Value::Null;
    std::fs::write(&ledger_file, ledger.to_string()).unwrap();
    f.b = RealBackend::new(f.cfg.clone()).unwrap();

    for contents in [None, Some("not json"), Some("{}")] {
        if let Some(contents) = contents {
            std::fs::write(&state_file, contents).unwrap();
        } else {
            std::fs::remove_file(&state_file).unwrap();
        }
        let views = f.b.refresh_usage(&[], &nc()).unwrap();
        assert!(!views[0].ownership_verified);
        let ledger: Value =
            serde_json::from_str(&std::fs::read_to_string(&ledger_file).unwrap()).unwrap();
        assert!(ledger[0]["endpoint"].is_null());
        assert!(ledger[0]["stopped"].is_null());
    }
    std::fs::write(&state_file, state).unwrap();
    assert!(f.b.runtimes(&nc()).unwrap().runtimes[0].managed);
    let ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_file).unwrap()).unwrap();
    assert!(ledger[0]["endpoint"].is_string());
    f.b = RealBackend::new(f.cfg.clone()).unwrap();
    std::fs::remove_file(&state_file).unwrap();
    assert!(f.b.refresh_usage(&[], &nc()).unwrap()[0].ownership_verified);
    f.edit_server(|s| s["accounts"]["alice"]["assignments"] = json!({}));
    assert_eq!(f.b.guard_poll(&nc()).unwrap().vanished.len(), 1);
}

#[test]
fn a_session_without_an_endpoint_ends_once_nothing_listed_could_be_it() {
    let mut f = fixture();
    let account = f.adopt_alice();
    f.provision("managed", Accelerator::T4);
    let state_file = f.state_file(&account.id);
    let ledger_file = f.cfg.local_dir.join("ledger.json");
    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_file).unwrap()).unwrap();
    ledger[0]["endpoint"] = Value::Null;
    std::fs::write(&ledger_file, ledger.to_string()).unwrap();
    f.b = RealBackend::new(f.cfg.clone()).unwrap();
    std::fs::remove_file(&state_file).unwrap();

    // Its runtime is still listed and unnamed, so it could be the session.
    assert!(!f.b.refresh_usage(&[], &nc()).unwrap()[0].ownership_verified);
    f.edit_server(|s| s["accounts"]["alice"]["assignments"] = json!({}));
    let report = f.b.guard_poll(&nc()).unwrap();
    assert_eq!(report.vanished.len(), 1);
    assert_eq!(report.vanished[0].name.as_deref(), Some("managed"));
    assert!(report.usage[0].ownership_verified);
}

#[test]
fn a_stop_without_an_endpoint_suspects_an_orphan_and_releases_nothing() {
    let mut f = fixture();
    let account = f.adopt_alice();
    f.provision("managed", Accelerator::T4);
    let state_file = f.state_file(&account.id);
    let ledger_file = f.cfg.local_dir.join("ledger.json");
    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_file).unwrap()).unwrap();
    ledger[0]["endpoint"] = Value::Null;
    std::fs::write(&ledger_file, ledger.to_string()).unwrap();
    f.b = RealBackend::new(f.cfg.clone()).unwrap();
    std::fs::remove_file(&state_file).unwrap();

    let target = RuntimeRef {
        account: account.id.clone(),
        endpoint: String::new(),
        name: Some("managed".into()),
    };
    let err =
        f.b.stop_session(&target, StopOptions::default(), &nc())
            .unwrap_err();
    assert!(err.is(ErrorKind::OrphanSuspected), "{err}");
    assert_eq!(f.assignments("alice").len(), 1);
}

#[test]
fn keyring_and_cli_api_failures_are_typed() {
    let f = fixture();
    f.adopt_alice();
    f.knob("cli_api_broken", json!(true));
    let rt = f.b.runtimes(&nc()).unwrap();
    assert!(rt.errors[0].error.is(ErrorKind::UnsupportedCliVersion));
    f.knob("cli_api_broken", json!(false));
    f.knob("keyring_locked", json!(true));
    assert!(
        f.b.accounts(&nc())
            .unwrap_err()
            .is(ErrorKind::KeyringUnavailable)
    );
}

/// A usage poll refreshes an expired token over HTTP, so a switch or a removal
/// can land before it writes the token back.
#[test]
fn a_token_refreshed_during_a_switch_or_removal_is_not_written_over_it() {
    let f = fixture();
    let alice = f.adopt_alice();
    f.add_bob();
    f.b.switch_account(&alice.id, &nc()).unwrap();
    let path = f.state.join("keyring.json");
    let keyring =
        || -> Value { serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap() };
    let slot = "google-colab-cli\u{0}oauth_token".to_owned();
    let vault = format!("colab-playground\u{0}account:{}", alice.id.as_str());
    let fresh = || -> Value { serde_json::from_str(&token("alice")).unwrap() };
    let expire = |keys: &[&String]| {
        let mut k = keyring();
        let mut t = fresh();
        t["expired"] = json!(true);
        for key in keys {
            k[key.as_str()] = json!(t.to_string());
        }
        std::fs::write(&path, k.to_string()).unwrap();
    };
    let read = |key: &String| -> Option<Value> {
        keyring()[key.as_str()]
            .as_str()
            .map(|s| serde_json::from_str(s).unwrap())
    };
    let poll = || {
        let r = f
            .helper()
            .usage_many(std::slice::from_ref(&alice.id), &nc())
            .unwrap();
        assert!(r[0].1.is_ok(), "{r:?}");
    };

    // Nothing lands meanwhile: both copies get the refreshed token.
    expire(&[&slot, &vault]);
    poll();
    assert_eq!(read(&slot), Some(fresh()));
    assert_eq!(read(&vault), Some(fresh()));

    expire(&[&slot, &vault]);
    f.knob(
        "keyring_during_refresh",
        json!({ slot.clone(): token("bob") }),
    );
    poll();
    assert_eq!(f.slot(), token("bob"), "the switch to bob stands");
    assert_eq!(read(&vault), Some(fresh()));

    expire(&[&vault]);
    f.knob("keyring_during_refresh", json!({ vault.clone(): null }));
    poll();
    assert_eq!(read(&vault), None, "a removed account is not saved again");
    assert_eq!(f.slot(), token("bob"));
}

#[test]
fn history_redaction_scrubs_only_secret_values() {
    let f = fixture();
    let helper = f.helper();
    let secret = "hf_Ab'c\"d\\é";
    let missing = helper
        .history_redact("nothing-here", &[secret.to_owned()], &nc())
        .unwrap();
    assert!(!missing.present);
    assert_eq!(missing.redacted_lines, 0);

    let path = f.history_path("sess-1");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Line 1: the CLI's own encoding (repr inside the prelude, then JSON).
    let prelude = format!(
        "import os\nos.environ['PLUGIN_HF_TOKEN'] = {}\nprint('ok')",
        python_repr(secret)
    );
    let cell = json!({"event_type": "execution", "code": prelude, "outputs": [{"text": format!("token {secret}\n")}]});
    let unrelated =
        r#"{"event_type": "session_created", "endpoint": "gpu-t4-s-1",   "spacing": "kept"}"#;
    let raw = format!("not json at all {secret} tail");
    std::fs::write(&path, format!("{cell}\n{unrelated}\n{raw}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let r = helper
        .history_redact("sess-1", &[secret.to_owned(), "abc".to_owned()], &nc())
        .unwrap();
    assert!(r.present);
    assert_eq!(r.redacted_lines, 2);
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(!text.contains("hf_Ab"), "{text}");
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(
        first["code"],
        "import os\nos.environ['PLUGIN_HF_TOKEN'] = '***'\nprint('ok')"
    );
    assert_eq!(first["outputs"][0]["text"], "token ***\n");
    assert_eq!(
        lines[1], unrelated,
        "lines without secrets are left byte-for-byte"
    );
    assert_eq!(lines[2], "not json at all *** tail");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(text.ends_with('\n'));
}

/// Python's `repr()` of a str, for the characters used in the test above.
fn python_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[test]
fn switch_impact_lists_unmanaged_runtimes_via_the_helper() {
    let f = fixture();
    let alice = f.adopt_alice();
    let bob = f.add_bob();
    f.provision("bob-t4", Accelerator::T4);
    f.edit_server(|s| {
        s["accounts"]["bob"]["assignments"]["browser-9"] =
            json!({"accelerator": "L4", "variant": "GPU", "shape": "STANDARD"});
    });
    let impact = f.b.switch_impact(&alice.id).unwrap();
    assert_eq!(impact.from.as_ref(), Some(&bob.id));
    assert_eq!(impact.sessions_left_running.len(), 1);
    let unmanaged = impact.unmanaged_left_running.unwrap();
    assert_eq!(unmanaged.len(), 1);
    assert_eq!(unmanaged[0].endpoint, "browser-9");
    assert_eq!(unmanaged[0].account, bob.id);
    let same = f.b.switch_impact(&bob.id).unwrap();
    assert_eq!(same.unmanaged_left_running, Some(vec![]));
}

#[test]
fn switch_impact_never_reports_unlisted_runtimes_as_none() {
    let f = fixture();
    let alice = f.adopt_alice();
    let bob = f.add_bob();
    let left = |to: &AccountId| f.b.switch_impact(to).unwrap().unmanaged_left_running;
    // The helper answers, but reports that bob's runtimes could not be listed.
    f.knob("cli_api_broken", json!(true));
    assert_eq!(left(&alice.id), None, "no list and no usage");
    assert_eq!(left(&bob.id), Some(vec![]), "staying leaves nothing behind");

    f.b.refresh_usage(&[], &nc()).unwrap();
    assert_eq!(
        left(&alice.id),
        None,
        "a usage reading of zero is not a list"
    );

    // Usage that shows the runtime does not stand in for the list either.
    f.edit_server(|s| {
        s["accounts"]["bob"]["assignments"]["browser-9"] =
            json!({"accelerator": "L4", "variant": "GPU", "shape": "STANDARD"});
    });
    let usage = f.b.refresh_usage(&[], &nc()).unwrap();
    let bobs = usage.iter().find(|u| u.account == bob.id).unwrap();
    assert_eq!(bobs.unmanaged_runtimes, 1);
    assert_eq!(left(&alice.id), None);

    // The helper failing outright, before it answers for any account.
    f.knob("cli_api_broken", json!(false));
    f.knob("keyring_locked", json!(true));
    assert_eq!(left(&alice.id), None);

    f.knob("keyring_locked", json!(false));
    let listed = left(&alice.id).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].endpoint, "browser-9");
}

/// Overriding only the `colab` path derives the interpreter from its
/// shebang (no Python is run), and bad paths say what to fix.
#[test]
fn colab_path_override_derives_the_interpreter() {
    let tmp = tempfile::tempdir().unwrap();
    // Canonical, because the launcher form resolves symlinks and macOS keeps its temp
    // directories behind one (/var -> /private/var).
    let root = tmp.path().canonicalize().unwrap();
    let root = root.as_path();
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let exe = |p: &Path, text: &str| {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let resolve = |p: &Path| Toolchain::from_colab_path(s(p)).resolve(&Host::Local, &[], &nc());
    let reason = |p: &Path| resolve(p).unwrap_err().param("reason").map(str::to_owned);

    assert_eq!(
        reason(&root.join("missing/colab")).as_deref(),
        Some("not_found")
    );
    let plain = root.join("plain/colab");
    std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
    std::fs::write(&plain, "#!/usr/bin/python3\n").unwrap();
    assert_eq!(reason(&plain).as_deref(), Some("not_executable"));
    let shell = root.join("shell/colab");
    exe(&shell, "#!/bin/sh\necho hi\n");
    assert_eq!(reason(&shell).as_deref(), Some("no_python_shebang"));

    // A uv/pip entry script whose interpreter is missing, then present.
    let venv_python = root.join("venv/bin/python");
    let entry = root.join("venv/bin/colab");
    exe(&entry, &format!("#!{}\nimport sys\n", s(&venv_python)));
    let err = resolve(&entry).unwrap_err();
    assert_eq!(err.param("reason"), Some("interpreter_missing"));
    assert_eq!(err.param("interpreter"), Some(s(&venv_python).as_str()));
    exe(&venv_python, "#!/bin/sh\nexit 0\n");
    assert_eq!(resolve(&entry).unwrap().helper_python, s(&venv_python));

    // Through a symlink (as ~/.local/bin/colab is), and the /bin/sh launcher form.
    let link = root.join("bin/colab");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&entry, &link).unwrap();
    assert_eq!(resolve(&link).unwrap().helper_python, s(&venv_python));
    let launcher = root.join("venv/bin/colab-launcher");
    exe(
        &launcher,
        "#!/bin/sh\n'''exec' \"$(dirname -- \"$(realpath -- \"$0\")\")\"/'python' \"$0\" \"$@\"\n' '''\n",
    );
    let t = resolve(&launcher).unwrap();
    assert_eq!(t.helper_python, s(&root.join("venv/bin").join("python")));
    assert_eq!(t.colab, s(&launcher));
}

/// The real launch and poll snippets, run in one Python process standing in
/// for the kernel (globals persist between cells, as in Colab): a secret the
/// script prints or puts into status.json never appears in the poll output.
#[test]
fn poll_snippet_scrubs_secrets_kept_in_kernel_memory() {
    use colab_playground_core::{vm, watchdog};
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("vm").to_string_lossy().into_owned();
    let script = br#"import json, os, time
secret = os.environ["PLUGIN_INPUT_API_KEY"]
print("leaking " + secret, flush=True)
st = os.environ["PLUGIN_STATUS_FILE"]
with open(st + ".tmp", "w") as fh:
    json.dump({"state": "running", "stage": "s", "message": "m " + secret, "outputs": {"k": [secret]}}, fh)
os.replace(st + ".tmp", st)
time.sleep(30)
"#;
    let write = |name: &str, text: String| {
        let p = tmp.path().join(name);
        std::fs::write(&p, text.replace(watchdog::VM_BASE, &base)).unwrap();
        p.to_string_lossy().into_owned()
    };
    let launch = write("launch.py", vm::plugin_launch_snippet(script));
    let poll = write("poll.py", vm::plugin_poll_snippet());
    let kill = write("kill.py", vm::plugin_kill_snippet());
    let driver = write(
        "driver.py",
        r#"import contextlib, io, json, os, sys, time
g = {"__name__": "__main__"}
os.environ.update({"PLUGIN_INPUT_API_KEY": "sk-VMSECRET789", "PLUGIN_INPUT_REPO": "a/b",
                   "CPG_RUN_ID": "r1", "CPG_SCRIPT_NAME": "w.py", "CPG_SECRET_KEYS": "PLUGIN_INPUT_API_KEY"})
exec(open(sys.argv[1]).read(), g)
assert "PLUGIN_INPUT_API_KEY" not in os.environ, "secret left in the kernel env"
result = None
for _ in range(100):
    time.sleep(0.1)
    os.environ.update({"CPG_RUN_ID": "r1", "CPG_LOG_OFFSET": "0"})
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        exec(open(sys.argv[2]).read(), g)
    result = buf.getvalue()
    if "leaking" in result and '"message"' in result:
        break
os.environ["CPG_RUN_ID"] = "r1"
with contextlib.redirect_stdout(io.StringIO()):
    exec(open(sys.argv[3]).read(), g)
print("POLL " + result.strip())
"#
        .to_owned(),
    );
    let out = Host::Local
        .run_program(
            "python3",
            &["-B".to_owned(), driver, launch, poll, kill],
            &[("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned())],
            &nc(),
            Some(Duration::from_secs(60)),
        )
        .unwrap();
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    let line = out.stdout.lines().find(|l| l.starts_with("POLL ")).unwrap();
    assert!(!out.stdout.contains("sk-VMSECRET789"), "{}", out.stdout);
    assert!(
        line.contains("leaking ***") && line.contains("m ***"),
        "{line}"
    );
    let v: Value = serde_json::from_str(line.trim_start_matches("POLL __CPG_RESULT__ ")).unwrap();
    assert_eq!(v["status"]["outputs"]["k"][0], "***");
}

#[test]
fn poll_snippet_reads_a_fast_growing_log_in_whole_lines_up_to_its_end() {
    use colab_playground_core::{vm, watchdog};
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("vm").to_string_lossy().into_owned();
    let poll = tmp.path().join("poll.py");
    std::fs::write(
        &poll,
        vm::plugin_poll_snippet().replace(watchdog::VM_BASE, &base),
    )
    .unwrap();
    let driver = tmp.path().join("driver.py");
    std::fs::write(
        &driver,
        r#"import contextlib, io, json, os, sys
poll_code, base, reads = open(sys.argv[1]).read(), sys.argv[2], int(sys.argv[3])
os.makedirs(base + "/runs/r1")
path = base + "/runs/r1/run.log"

def poll(offset):
    os.environ.update({"CPG_RUN_ID": "r1", "CPG_LOG_OFFSET": str(offset)})
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        exec(poll_code, {"__name__": "__main__"})
    return json.loads(buf.getvalue().split("__CPG_RESULT__ ", 1)[1])

def read_all(offset):
    """What the app shows from `offset` on: the text, the bytes left out, the next offset."""
    text, skipped = "", 0
    for n in range(reads + 1):
        r = poll(offset)
        assert len(r["log"].encode()) <= 16384, "one poll stays small"
        assert n == 0 or not r["skipped"], "bytes are left out once, not in the middle"
        text, skipped, offset = text + r["log"], skipped + r["skipped"], r["offset"]
        if offset >= r["size"]:
            assert r["size"] == os.path.getsize(path)
            return text, skipped, offset
        assert r["log"].endswith("\n"), "a line is not split between two polls"
    raise AssertionError("the end of the log was not reached in %d polls" % reads)

def write(mode, first, count):
    with open(path, mode) as fh:
        for i in range(first, first + count):
            fh.write("line %06d %s\u00e9\n" % (i, "x" * (i % 97)))

# More than one poll holds, less than the app keeps: all of it, with nothing left out.
write("w", 0, 2000)
size = os.path.getsize(path)
assert 16384 * 4 < size < 16384 * 16
text, skipped, offset = read_all(0)
assert (text, skipped, offset) == (open(path, encoding="utf-8").read(), 0, size)
assert poll(offset)["log"] == "" and poll(offset)["offset"] == offset

# Far more than the app keeps: the middle is left out and counted, the end is all there.
write("a", 2000, 20000)
with open(path, "a") as fh:
    fh.write("Traceback: the error, no newline after it")
whole = open(path, "rb").read()
text, skipped, end = read_all(offset)
assert skipped > 0 and end == len(whole)
shown = whole[offset + skipped:].decode()
assert text == shown and shown.startswith("line ") and shown.endswith("no newline after it")
assert len(shown.encode()) > 16384 * 15

# A log that became shorter than the offset is read from its start.
write("w", 0, 3)
assert read_all(end)[0] == open(path, encoding="utf-8").read()
print("OK")
"#,
    )
    .unwrap();
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let out = Host::Local
        .run_program(
            "python3",
            &[
                "-B".to_owned(),
                s(&driver),
                s(&poll),
                base,
                vm::LOG_BACKLOG_READS.to_string(),
            ],
            &[("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned())],
            &nc(),
            Some(Duration::from_secs(60)),
        )
        .unwrap();
    assert!(
        out.success() && out.stdout.contains("OK"),
        "{}\n{}",
        out.stdout,
        out.stderr
    );
}

#[test]
fn worker_quant_fitting_matches_the_rust_vectors() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let worker = root.join("../../plugins/llama-cpp/worker.py");
    let vectors = root.join("tests/data/quant_fit_vectors.json");
    // -B / PYTHONDONTWRITEBYTECODE: never leave __pycache__ inside the repo.
    let args = vec![
        "-B".to_owned(),
        worker.to_string_lossy().into_owned(),
        "--fit-self-test".to_owned(),
        vectors.to_string_lossy().into_owned(),
    ];
    let env = vec![("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned())];
    let out = Host::Local
        .run_program("python3", &args, &env, &nc(), Some(Duration::from_secs(60)))
        .unwrap();
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    assert!(!out.stdout.contains("FAIL"), "{}", out.stdout);
}

#[test]
fn probe_uses_configured_colab_path_instead_of_path_binary() {
    let f = fixture();
    let expected = f.cfg.toolchain.clone().unwrap();
    let report = f.b.probe(&nc()).unwrap();
    assert_eq!(report.colab.as_deref(), Some(expected.colab.as_str()));
    assert_eq!(report.colab_version.as_deref(), Some("0.8.0-fake"));
    assert_eq!(
        report.helper_python.as_deref(),
        Some(expected.helper_python.as_str())
    );
    assert!(report.keyring_ok);

    // macOS rejects nested `#!` scripts, so test `Toolchain::from_colab_path` with the
    // `#!/bin/sh` + sibling `python` launcher layout produced by `uv` and `pip`.
    let custom = f.root.join("custom-cli");
    std::fs::create_dir_all(&custom).unwrap();
    // `RESOLVE_SCRIPT` resolves symlinks with `readlink -f` (e.g. `/var` -> `/private/var`).
    let custom = std::fs::canonicalize(&custom).unwrap();
    let fake = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fake");
    let custom_py = custom.join("python");
    install(&fake.join("python_shim.py"), &custom_py, true);
    install(
        &fake.join("fake_common.py"),
        &custom.join("fake_common.py"),
        false,
    );
    let custom_colab = custom.join("colab");
    std::fs::write(
        &custom_colab,
        format!("#!/bin/sh\nexec python3 \"{}\" \"$@\"\n", expected.colab),
    )
    .unwrap();
    std::fs::set_permissions(&custom_colab, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut cfg = f.cfg.clone();
    cfg.local_dir = f.root.join("local-custom");
    cfg.toolchain = Some(Toolchain::from_colab_path(
        custom_colab.to_string_lossy().into_owned(),
    ));
    let b2 = RealBackend::new(cfg).unwrap();
    let r2 = b2.probe(&nc()).unwrap();
    assert_eq!(
        r2.colab.as_deref(),
        Some(custom_colab.to_string_lossy().as_ref())
    );
    assert_eq!(r2.colab_version.as_deref(), Some("0.8.0-fake"));
    assert_eq!(
        r2.helper_python.as_deref(),
        Some(custom_py.to_string_lossy().as_ref())
    );
    assert!(r2.keyring_ok);

    let cancelled = CancelToken::new();
    cancelled.cancel();
    assert!(f.b.probe(&cancelled).unwrap_err().is(ErrorKind::Cancelled));
}

#[test]
fn probe_blames_the_configured_path_when_nothing_runs_there() {
    let f = fixture();
    let mut cfg = f.cfg.clone();
    cfg.local_dir = f.root.join("local-missing");
    let mut toolchain = cfg.toolchain.clone().unwrap();
    toolchain.colab = f.root.join("no-such-colab").to_string_lossy().into_owned();
    cfg.toolchain = Some(toolchain);
    let backend = RealBackend::new(cfg).unwrap();
    // `NotInstalled` would make the checklist offer an install that cannot help.
    let error = backend.probe(&nc()).unwrap_err();
    assert!(error.is(ErrorKind::InvalidInput), "{error}");
}

#[test]
fn vm_list_dir_and_vm_upload_roundtrip_and_errors() {
    let f = fixture();
    let _alice = f.adopt_alice();
    f.provision("files-rt", Accelerator::T4);
    let rt = f
        .b
        .runtimes(&nc())
        .unwrap()
        .runtimes
        .into_iter()
        .find(|r| r.id.name.as_deref() == Some("files-rt"))
        .unwrap()
        .id;

    let local_file = f.root.join("lora.safetensors");
    std::fs::write(&local_file, b"fake-lora-weights").unwrap();
    f.b.vm_upload(
        &rt,
        &local_file.to_string_lossy(),
        "content/weights/lora.safetensors",
        &nc(),
    )
    .unwrap();

    let entries = f.b.vm_list_dir(&rt, "content/weights", &nc()).unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.name == "lora.safetensors" && !e.is_dir),
        "{entries:?}"
    );

    let missing_local = f
        .b
        .vm_upload(
            &rt,
            &f.root.join("does-not-exist.bin").to_string_lossy(),
            "content/missing.bin",
            &nc(),
        )
        .unwrap_err();
    assert!(missing_local.is(ErrorKind::Io), "{missing_local}");

    let missing_remote = f.b.vm_list_dir(&rt, "content/no-such-dir", &nc()).unwrap_err();
    assert!(!missing_remote.is(ErrorKind::SessionGone), "{missing_remote}");
}

#[test]
fn keep_plan_local_sync_and_pre_stop_never_discards_on_failure() {
    use colab_playground_core::keep::{KeepPlan, KeepTargetId, LocalKeepTarget};

    let f = fixture();
    f.adopt_alice();
    f.provision("keep-local", Accelerator::T4);
    let rt = f
        .b
        .runtimes(&nc())
        .unwrap()
        .runtimes
        .into_iter()
        .find(|r| r.id.name.as_deref() == Some("keep-local"))
        .unwrap()
        .id;

    let save_dir = f.root.join("saved-local");
    let plan = KeepPlan {
        paths: vec!["/content/out".into()],
        local: Some(LocalKeepTarget {
            folder: save_dir.to_string_lossy().into_owned(),
        }),
        hf: None,
        drive: None,
    };
    f.b.set_keep_plan(&rt, Some(plan), &nc()).unwrap();

    // No files on VM yet: save must fail, and pre-stop save must abort stop without wiping the VM.
    assert!(f.b.sync_keep(&rt, None, &nc()).is_err());
    let stop_err = f
        .b
        .stop_session(
            &rt,
            StopOptions {
                reason: StopReason::User,
                save_keep: true,
                force_on_keep_error: false,
                ..StopOptions::default()
            },
            &nc(),
        )
        .unwrap_err();
    assert!(!stop_err.is(ErrorKind::OrphanSuspected), "{stop_err}");
    assert_eq!(
        f.assignments("alice").len(),
        1,
        "runtime must stay alive when pre-stop Keep Plan fails"
    );

    // Populate simulated VM output file and retry save + pre-stop stop.
    f.edit_server(|s| {
        let eps = s["accounts"]["alice"]["assignments"]
            .as_object_mut()
            .unwrap();
        let (_, a) = eps.iter_mut().next().unwrap();
        a["vm_files"] = json!({"/content/out/result.png": "png-bytes"});
    });
    let report = f.b.sync_keep(&rt, None, &nc()).unwrap();
    assert!(report.all_succeeded());
    assert!(
        report
            .results
            .iter()
            .any(|(id, r)| *id == KeepTargetId::Local && r.error.is_none()),
        "{report:?}"
    );
    let downloaded: Vec<_> = std::fs::read_dir(&save_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(!downloaded.is_empty(), "archive must exist on local disk");
    assert!(
        std::fs::metadata(&downloaded[0]).unwrap().len() > 0,
        "downloaded archive must be non-empty"
    );

    // Pre-stop save succeeds and then releases the VM.
    f.b.stop_session(
        &rt,
        StopOptions {
            reason: StopReason::User,
            save_keep: true,
            force_on_keep_error: false,
            ..StopOptions::default()
        },
        &nc(),
    )
    .unwrap();
    assert!(f.assignments("alice").is_empty());
}

#[test]
fn keep_plan_hf_upload_isolates_secret_and_arms_watchdog_hooks() {
    use colab_playground_core::keep::{HfKeepTarget, KeepPlan};

    let f = fixture();
    f.adopt_alice();
    let secret = "hf_SUPERSECRET_ISSUE19_TOKEN";
    f.b.set_secret("hf_token", secret, &nc()).unwrap();
    f.provision("keep-hf", Accelerator::T4);
    let rt = f
        .b
        .runtimes(&nc())
        .unwrap()
        .runtimes
        .into_iter()
        .find(|r| r.id.name.as_deref() == Some("keep-hf"))
        .unwrap()
        .id;

    let plan = KeepPlan {
        paths: vec!["/content/checkpoints".into()],
        local: None,
        hf: Some(HfKeepTarget {
            repo: "alice/test-checkpoints".into(),
            path_in_repo: Some("run-1".into()),
            interval_minutes: Some(15),
        }),
        drive: None,
    };
    f.b.set_keep_plan(&rt, Some(plan), &nc()).unwrap();

    // Watchdog must be restarted with the HF wind-down hook, and CLI history must be scrubbed.
    let starts = f.watchdog_starts();
    assert!(starts.len() >= 2, "watchdog restarted with keep hooks: {starts:?}");
    let last_hooks = starts
        .last()
        .unwrap()
        .iter()
        .find_map(|a| a.strip_prefix("CPG_WD_HOOKS="))
        .unwrap();
    assert!(last_hooks.contains("alice/test-checkpoints"), "{last_hooks}");
    assert!(!f.history("keep-hf").contains(secret), "history leaked HF token!");

    f.edit_server(|s| {
        let eps = s["accounts"]["alice"]["assignments"]
            .as_object_mut()
            .unwrap();
        let (_, a) = eps.iter_mut().next().unwrap();
        a["vm_files"] = json!({"/content/checkpoints/step-100.safetensors": "ckpt-bytes"});
    });
    let report = f.b.sync_keep(&rt, None, &nc()).unwrap();
    assert!(report.all_succeeded());
    assert!(!f.history("keep-hf").contains(secret), "sync leaked HF token!");

    // Deleting hf_token makes HF sync fail clearly without leaking or claiming Ok.
    f.b.delete_secret("hf_token", &nc()).unwrap();
    let err = f.b.sync_keep(&rt, None, &nc()).unwrap_err();
    assert!(!err.detail.contains(secret));
}

#[test]
fn keep_plan_drive_mount_requires_explicit_consent_and_handles_auth_flow() {
    use colab_playground_core::backend::DriveMountStart;
    use colab_playground_core::keep::{DriveKeepTarget, KeepPlan};

    let f = fixture();
    f.adopt_alice();
    f.provision("keep-drive", Accelerator::T4);
    let rt = f
        .b
        .runtimes(&nc())
        .unwrap()
        .runtimes
        .into_iter()
        .find(|r| r.id.name.as_deref() == Some("keep-drive"))
        .unwrap()
        .id;

    // Drive with consented = false must refuse to sync.
    let unconsented = KeepPlan {
        paths: vec!["/content/out".into()],
        local: None,
        hf: None,
        drive: Some(DriveKeepTarget {
            mount_path: "/content/drive".into(),
            dest_subpath: "MyDrive/colab-playground".into(),
            consented: false,
            mounted: false,
        }),
    };
    f.b.set_keep_plan(&rt, Some(unconsented), &nc()).unwrap();
    let err = f.b.sync_keep(&rt, None, &nc()).unwrap_err();
    assert!(err.is(ErrorKind::InvalidInput), "{err}");

    // Interactive Drive mount consent flow: cancel leaves it unmounted, confirm mounts it.
    match f.b.begin_drive_mount(&rt, "/content/drive", &nc()).unwrap() {
        DriveMountStart::NeedsConsent(flow) => {
            assert!(flow.auth_url().starts_with("https://"));
            flow.cancel().unwrap();
        }
        DriveMountStart::Mounted => panic!("expected consent URL on first mount"),
    }
    let (_, _) = f.listed_watchdog("keep-drive");
    let rt_view = f
        .b
        .runtimes(&nc())
        .unwrap()
        .runtimes
        .into_iter()
        .find(|r| r.id.name.as_deref() == Some("keep-drive"))
        .unwrap();
    assert!(
        !rt_view
            .tracked
            .as_ref()
            .and_then(|t| t.keep_plan.as_ref())
            .and_then(|p| p.drive.as_ref())
            .is_some_and(|d| d.mounted)
    );

    match f.b.begin_drive_mount(&rt, "/content/drive", &nc()).unwrap() {
        DriveMountStart::NeedsConsent(flow) => {
            flow.confirm(&nc()).unwrap();
        }
        DriveMountStart::Mounted => panic!("expected consent URL"),
    }

    f.edit_server(|s| {
        let eps = s["accounts"]["alice"]["assignments"]
            .as_object_mut()
            .unwrap();
        let (_, a) = eps.iter_mut().next().unwrap();
        a["vm_files"] = json!({"/content/out/a.txt": "hello"});
    });
    let report = f.b.sync_keep(&rt, None, &nc()).unwrap();
    assert!(report.all_succeeded());
}

#[test]
fn keep_plan_drive_silent_exit_zero_failure_never_marks_mounted_and_force_stop_releases() {
    use colab_playground_core::backend::DriveMountStart;
    use colab_playground_core::keep::{KeepPlan, LocalKeepTarget};

    let f = fixture();
    f.adopt_alice();
    f.provision("drive-fail", Accelerator::T4);
    let rt = f
        .b
        .runtimes(&nc())
        .unwrap()
        .runtimes
        .into_iter()
        .find(|r| r.id.name.as_deref() == Some("drive-fail"))
        .unwrap()
        .id;

    // Even if `colab drivemount` exits 0 with its propagation failure message, verify ismount catches it.
    f.knob("drive_mount_fails", json!(true));
    let err = match f.b.begin_drive_mount(&rt, "/content/drive", &nc()).unwrap() {
        DriveMountStart::NeedsConsent(flow) => flow.confirm(&nc()).unwrap_err(),
        DriveMountStart::Mounted => panic!("must not report mounted on silent failure"),
    };
    assert!(!err.detail.is_empty(), "{err}");
    let listed = f
        .b
        .runtimes(&nc())
        .unwrap()
        .runtimes
        .into_iter()
        .find(|r| r.id.name.as_deref() == Some("drive-fail"))
        .unwrap();
    assert!(
        !listed
            .tracked
            .as_ref()
            .and_then(|t| t.keep_plan.as_ref())
            .and_then(|p| p.drive.as_ref())
            .is_some_and(|d| d.mounted)
    );

    // With force_on_keep_error = true, stop still proceeds after Keep Plan failure.
    let save_dir = f.root.join("force-stop-dir");
    f.b.set_keep_plan(
        &rt,
        Some(KeepPlan {
            paths: vec!["/content/missing".into()],
            local: Some(LocalKeepTarget {
                folder: save_dir.to_string_lossy().into_owned(),
            }),
            hf: None,
            drive: None,
        }),
        &nc(),
    )
    .unwrap();
    f.b.stop_session(
        &rt,
        StopOptions {
            reason: StopReason::User,
            save_keep: true,
            force_on_keep_error: true,
            ..StopOptions::default()
        },
        &nc(),
    )
    .unwrap();
    assert!(f.assignments("alice").is_empty());
}

#[test]
fn watchdog_script_runs_periodic_and_idle_winddown_hooks() {
    use colab_playground_core::watchdog::{WATCHDOG_PY, WatchdogHook};
    use std::io::Write;
    use std::process::{Command, Stdio};

    let tmp = tempfile::tempdir().unwrap();
    let wd_py = tmp.path().join("watchdog.py");
    std::fs::write(&wd_py, WATCHDOG_PY).unwrap();
    let cfg_path = tmp.path().join("watchdog.json");
    let log_path = tmp.path().join("watchdog.log");
    let marker = tmp.path().join("periodic.txt");
    let idle_marker = tmp.path().join("idle.txt");

    // Start a tiny stdlib HTTP server to accept POST /unassign when idle triggers.
    let driver = tmp.path().join("run_wd.py");
    std::fs::write(
        &driver,
        r#"import http.server, json, os, subprocess, sys, threading, time
tmp, wd_py = sys.argv[1], sys.argv[2]
hits = []
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        hits.append(self.path)
        self.send_response(200)
        self.end_headers()
    def log_message(self, *a): pass
srv = http.server.HTTPServer(("127.0.0.1", 0), H)
t = threading.Thread(target=srv.serve_forever, daemon=True)
t.start()
port = srv.server_address[1]
cfg = {
    "deadline_epoch": None,
    "idle_timeout_s": 4.0,
    "cancelled": False,
    "runtime_addr": f"127.0.0.1:{port}",
    "heartbeat": os.path.join(tmp, "heartbeat"),
    "log": os.path.join(tmp, "watchdog.log"),
    "poll_s": 0.2,
    "grace_s": 10,
    "started": time.time() - 1.5,
}
cfg_path = os.path.join(tmp, "watchdog.json")
with open(cfg_path, "w") as fh:
    json.dump(cfg, fh)
hooks = json.loads(sys.stdin.read())
p = subprocess.Popen(
    [sys.executable, "-B", wd_py, "--config", cfg_path, "--hooks-stdin"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
)
out, err = p.communicate(json.dumps(hooks).encode(), timeout=30)
srv.shutdown()
assert p.returncode == 0, (p.returncode, out, err)
assert "/unassign" in hits, hits
print("OK")
"#,
    )
    .unwrap();

    let hooks = vec![
        WatchdogHook {
            argv: vec![
                "python3".into(),
                "-c".into(),
                format!(
                    "import pathlib; p = pathlib.Path({:?}); p.write_text((p.read_text() if p.exists() else '') + 'periodic\\n')",
                    marker.to_string_lossy(),
                ),
            ],
            env: BTreeMap::new(),
            timeout_s: 15,
            interval_s: Some(1),
        },
        WatchdogHook {
            argv: vec![
                "python3".into(),
                "-c".into(),
                format!(
                    "import pathlib; pathlib.Path({:?}).write_text('idle')",
                    idle_marker.to_string_lossy(),
                ),
            ],
            env: BTreeMap::new(),
            timeout_s: 15,
            interval_s: None,
        },
    ];
    let mut child = Command::new("python3")
        .args(["-B", driver.to_str().unwrap(), tmp.path().to_str().unwrap(), wd_py.to_str().unwrap()])
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&hooks).unwrap().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let wd_log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(out.status.success() && stdout.contains("OK"), "stdout={stdout}\nstderr={stderr}\ncfg={cfg_path:?}\nlog={wd_log:?}");
    assert!(marker.exists(), "periodic hook must have executed");
    assert!(idle_marker.exists(), "wind-down hook must have run before idle release");
    let hook0_pos = wd_log.find("hook 0").expect("hook 0 must be logged");
    let winddown_pos = wd_log.find("wind-down started").expect("wind-down must be logged");
    assert!(hook0_pos < winddown_pos, "periodic hook 0 must run before wind-down: {wd_log}");
    let hook1_pos = wd_log.find("hook 1").expect("hook 1 must be logged");
    assert!(hook1_pos > winddown_pos, "wind-down-only hook 1 must only run during wind-down: {wd_log}");
}

#[test]
fn guard_poll_cancels_hanging_keep_sync_and_still_stops_runtime() {
    use colab_playground_core::guard::GuardPolicy;
    use colab_playground_core::keep::{KeepPlan, LocalKeepTarget};
    use std::time::Instant;

    let f = fixture();
    f.adopt_alice();
    let mut cfg = f.cfg.clone();
    cfg.guard_keep_timeout = Duration::from_millis(300);
    let backend = RealBackend::new(cfg).unwrap();
    let mut spec = ProvisionSpec::new("guard-keep-timeout", Accelerator::T4, &Defaults::default());
    spec.guard = GuardPolicy {
        min_balance_cu: Some(40.0),
        ..GuardPolicy::unlimited()
    };
    let (tx, _rx) = channel();
    backend.provision(spec, tx, &nc()).unwrap();
    let rt = backend
        .runtimes(&nc())
        .unwrap()
        .runtimes
        .into_iter()
        .find(|r| r.id.name.as_deref() == Some("guard-keep-timeout"))
        .unwrap()
        .id;

    let save_dir = f.root.join("guard-timeout-save");
    backend
        .set_keep_plan(
            &rt,
            Some(KeepPlan {
                paths: vec!["/content/out".into()],
                local: Some(LocalKeepTarget {
                    folder: save_dir.to_string_lossy().into_owned(),
                }),
                hf: None,
                drive: None,
            }),
            &nc(),
        )
        .unwrap();

    // Drop balance below min_balance_cu so guard_poll issues Decision::Stop, and make Keep sync hang.
    f.edit_server(|s| s["accounts"]["alice"]["balance"] = json!(30.0));
    f.knob("keep_sync_hangs", json!(true));

    let started = Instant::now();
    let report = backend.guard_poll(&nc()).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "guard_poll took too long when Keep Plan hung: {:?}",
        started.elapsed()
    );
    assert_eq!(report.decisions.len(), 1, "{report:?}");
    assert!(report.decisions[0].stopped, "runtime must still be stopped: {report:?}");
    assert!(f.assignments("alice").is_empty());
}

#[test]
fn unconsented_drive_target_is_skipped_by_default_sync_and_stop_session() {
    use colab_playground_core::keep::{DriveKeepTarget, KeepPlan, KeepTargetId, LocalKeepTarget};

    let f = fixture();
    f.adopt_alice();
    f.provision("unconsented-drive", Accelerator::T4);
    let rt = f
        .b
        .runtimes(&nc())
        .unwrap()
        .runtimes
        .into_iter()
        .find(|r| r.id.name.as_deref() == Some("unconsented-drive"))
        .unwrap()
        .id;

    f.edit_server(|s| {
        let eps = s["accounts"]["alice"]["assignments"]
            .as_object_mut()
            .unwrap();
        let (_, a) = eps.iter_mut().next().unwrap();
        a["vm_files"] = json!({"/content/out/a.txt": "hello"});
    });

    // Local + unconsented Drive: default sync_keep only syncs Local; explicit Drive still fails.
    let save_dir = f.root.join("unconsented-local-save");
    f.b.set_keep_plan(
        &rt,
        Some(KeepPlan {
            paths: vec!["/content/out".into()],
            local: Some(LocalKeepTarget {
                folder: save_dir.to_string_lossy().into_owned(),
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
    assert!(
        f.b.sync_keep(&rt, Some(vec![KeepTargetId::Drive]), &nc())
            .is_err()
    );
    let report = f.b.sync_keep(&rt, None, &nc()).unwrap();
    assert!(report.all_succeeded());
    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].0, KeepTargetId::Local);

    // Only unconsented Drive: has_enabled_target() is false, so stop_session(save_keep: true)
    // must not attempt Drive sync or abort stop.
    f.b.set_keep_plan(
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
    f.b.stop_session(
        &rt,
        StopOptions {
            reason: StopReason::User,
            save_keep: true,
            force_on_keep_error: false,
            ..StopOptions::default()
        },
        &nc(),
    )
    .unwrap();
    assert!(f.assignments("alice").is_empty());
}
