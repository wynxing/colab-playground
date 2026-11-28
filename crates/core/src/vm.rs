//! Python snippets executed on the VM with `colab exec`.
//!
//! Each snippet prints one `__CPG_RESULT__ <json>` line. Values reach the
//! snippets through `--env` (quoted by the CLI); only base64 text and crate
//! constants are interpolated into code. Snippets pop their `CPG_*` and
//! `PLUGIN_*` variables from the kernel environment so that inputs (secrets in
//! particular) do not linger in the long-lived kernel process.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::plugins::{InputValue, ResolvedEndpoint};
use crate::provision::Hardware;
use crate::watchdog::{HEARTBEAT_PATH, VM_BASE};

/// Probes the GPU name and total memory with `nvidia-smi`.
pub const PROBE_HARDWARE: &str = r#"import json, subprocess
_r = {"gpu": None, "vram_mib": None}
try:
    _o = subprocess.run(["nvidia-smi", "--query-gpu=name,memory.total", "--format=csv,noheader,nounits"],
                        capture_output=True, text=True, timeout=60).stdout.strip().splitlines()
    if _o:
        _n, _m = [x.strip() for x in _o[0].split(",")[:2]]
        _r = {"gpu": _n, "vram_mib": int(float(_m))}
except Exception as _e:
    _r["error"] = type(_e).__name__
print("__CPG_RESULT__ " + json.dumps(_r))
"#;

pub fn parse_hardware(v: &serde_json::Value) -> Hardware {
    Hardware {
        gpu: v.get("gpu").and_then(|g| g.as_str()).map(str::to_owned),
        vram_mib: v
            .get("vram_mib")
            .and_then(|m| m.as_u64())
            .map(|m| m.min(u64::from(u32::MAX)) as u32),
    }
}

pub const ENV_RUN_ID: &str = "CPG_RUN_ID";
pub const ENV_SCRIPT_NAME: &str = "CPG_SCRIPT_NAME";
/// Env var with the log byte offset for [`plugin_poll_snippet`].
pub const ENV_LOG_OFFSET: &str = "CPG_LOG_OFFSET";
/// Env var naming (comma-separated) the `PLUGIN_*` variables that hold
/// secrets. The launcher keeps those values in kernel memory only, so that
/// [`plugin_poll_snippet`] can scrub them from what it prints (and therefore
/// from the CLI's local history of executed cells).
pub const ENV_SECRET_KEYS: &str = "CPG_SECRET_KEYS";

pub const ENV_KEEP_PATHS: &str = "CPG_KEEP_PATHS";
pub const ENV_KEEP_ARCHIVE: &str = "CPG_KEEP_ARCHIVE";
pub const ENV_KEEP_REPO: &str = "CPG_KEEP_REPO";
pub const ENV_KEEP_SUBPATH: &str = "CPG_KEEP_SUBPATH";
pub const ENV_KEEP_MOUNT: &str = "CPG_KEEP_MOUNT";
pub const ENV_KEEP_DRIVE_DEST: &str = "CPG_KEEP_DRIVE_DEST";

pub fn run_dir(run_id: &str) -> String {
    format!("{VM_BASE}/runs/{run_id}")
}

/// Minimal run metadata persisted beside the script on the VM (`meta.json`).
/// Never stores secret inputs or host tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMeta {
    pub run_id: String,
    pub plugin: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, InputValue>,
    #[serde(default)]
    pub endpoints: Vec<ResolvedEndpoint>,
    #[serde(default)]
    pub keep_alive: bool,
    #[serde(default)]
    pub provisions: bool,
}

/// Code that writes the plugin script and non-secret `meta.json` into its run directory and
/// starts it detached (own session), passing `PLUGIN_*` variables only to the child.
pub fn plugin_launch_snippet(script: &[u8], meta_json: &[u8]) -> String {
    let b64 = B64.encode(script);
    let meta_b64 = B64.encode(meta_json);
    format!(
        r#"import base64, json, os, subprocess, sys
_rid = os.environ.pop("{ENV_RUN_ID}")
_name = os.environ.pop("{ENV_SCRIPT_NAME}")
_d = "{VM_BASE}/runs/" + _rid
os.makedirs(_d, exist_ok=True)
_meta_b64 = "{meta_b64}"
with open(_d + "/meta.json.tmp", "wb") as _fh:
    _fh.write(base64.b64decode(_meta_b64))
os.replace(_d + "/meta.json.tmp", _d + "/meta.json")
_path = _d + "/" + _name
with open(_path, "wb") as _fh:
    _fh.write(base64.b64decode("{b64}"))
_keys = [k for k in os.environ if k.startswith("PLUGIN_")]
_seckeys = [k for k in os.environ.pop("{ENV_SECRET_KEYS}", "").split(",") if k]
globals().setdefault("_cpg_redact", {{}})[_rid] = sorted(
    {{os.environ[k] for k in _seckeys if len(os.environ.get(k, "")) >= 4}}, key=len, reverse=True)
_env = dict(os.environ)
_env.update({{"PLUGIN_RUN_DIR": _d, "PLUGIN_STATUS_FILE": _d + "/status.json",
             "PLUGIN_HEARTBEAT": "{HEARTBEAT_PATH}", "PYTHONUNBUFFERED": "1"}})
for _k in _keys:
    os.environ.pop(_k, None)
_cmd = [sys.executable, "-u", _path] if _name.endswith(".py") else ["bash", _path]
_log = open(_d + "/run.log", "ab")
_p = subprocess.Popen(_cmd, cwd=_d, env=_env, stdin=subprocess.DEVNULL, stdout=_log,
                      stderr=subprocess.STDOUT, start_new_session=True, close_fds=True)
del _env
with open(_d + "/pid", "w") as _fh:
    _fh.write(str(_p.pid))
print("__CPG_RESULT__ " + json.dumps({{"pid": _p.pid, "run_dir": _d}}))
"#
    )
}

/// Log bytes one poll returns at most: what a poll prints comes back through one `colab exec`
/// and is recorded in the CLI's history.
const LOG_CHUNK: u64 = 16 * 1024;
/// The furthest the shown log may be behind the file. A script that writes faster than the
/// polls read is shown from this far back, so the end of the log is always within reach.
const LOG_BACKLOG: u64 = 16 * LOG_CHUNK;
/// Polls that read the whole backlog. A chunk cut short at a line end and the one after it
/// hold more than one chunk together, so twice the chunks in the backlog are enough.
pub const LOG_BACKLOG_READS: u32 = (2 * LOG_BACKLOG / LOG_CHUNK) as u32;

/// Code that reads `status.json`, whether the script is alive, and the log
/// from byte offset `CPG_LOG_OFFSET`.
pub fn plugin_poll_snippet() -> String {
    format!(
        r#"import json, os
_rid = os.environ.pop("{ENV_RUN_ID}")
_d = "{VM_BASE}/runs/" + _rid
_off = int(os.environ.pop("{ENV_LOG_OFFSET}", "0") or 0)
try:
    with open(_d + "/status.json") as _fh:
        _st = json.load(_fh)
except FileNotFoundError:
    _st = None
except Exception as _e:
    _st = {{"_unreadable": type(_e).__name__}}
_alive = False
try:
    _pid = int(open(_d + "/pid").read().strip())
    with open("/proc/%d/stat" % _pid) as _fh:
        _alive = _fh.read().split(") ", 1)[1][:1] not in ("Z", "X")
except Exception:
    _alive = False
_log, _new, _size, _skip = "", _off, 0, 0
try:
    with open(_d + "/run.log", "rb") as _fh:
        _size = os.fstat(_fh.fileno()).st_size
        if _off > _size:
            _off = 0
        if _size - _off > {LOG_BACKLOG}:
            _skip = _size - {LOG_BACKLOG} - _off
        _fh.seek(_off + _skip)
        _chunk = _fh.read({LOG_CHUNK})
        _size = max(_size, os.fstat(_fh.fileno()).st_size)
        _new = _off + _skip + len(_chunk)
        # Whole lines only where the chunk is cut out of the middle of the file.
        if _skip:
            _head = _chunk.find(b"\n") + 1
            _skip += _head
            _chunk = _chunk[_head:]
        _tail = _chunk.rfind(b"\n") + 1
        if _new < _size and _tail:
            _new -= len(_chunk) - _tail
            _chunk = _chunk[:_tail]
        _log = _chunk.decode("utf-8", "replace")
except FileNotFoundError:
    pass
# Scrub the run's secrets (kept in kernel memory by the launcher) before
# printing: printed output is recorded in the CLI's local history.
_sec = globals().get("_cpg_redact", {{}}).get(_rid, [])
def _cpg_scrub(v):
    if isinstance(v, str):
        for _s in _sec:
            v = v.replace(_s, "***")
        return v
    if isinstance(v, list):
        return [_cpg_scrub(x) for x in v]
    if isinstance(v, dict):
        return {{k: _cpg_scrub(x) for k, x in v.items()}}
    return v
print("__CPG_RESULT__ " + json.dumps({{"status": _cpg_scrub(_st), "alive": _alive, "log": _cpg_scrub(_log), "offset": _new, "size": _size, "skipped": _skip}}))
"#
    )
}

/// Code that sends `SIGTERM` to the run's process group.
pub fn plugin_kill_snippet() -> String {
    format!(
        r#"import json, os, signal
_d = "{VM_BASE}/runs/" + os.environ.pop("{ENV_RUN_ID}")
_ok = False
try:
    os.killpg(int(open(_d + "/pid").read().strip()), signal.SIGTERM)
    _ok = True
except Exception:
    pass
print("__CPG_RESULT__ " + json.dumps({{"signalled": _ok}}))
"#
    )
}

/// Tail bytes read on run discovery (two chunks, enough to populate the log tail cleanly).
const DISCOVER_TAIL: u64 = 2 * LOG_CHUNK;

/// Code that scans `/root/.colab-playground/runs` for live processes that wrote `meta.json`.
pub fn plugin_discover_snippet() -> String {
    format!(
        r#"import json, os
_base = "{VM_BASE}/runs"
_runs = []
_redact = globals().get("_cpg_redact", {{}})
def _cpg_scrub(v, _sec):
    if isinstance(v, str):
        for _s in _sec:
            v = v.replace(_s, "***")
        return v
    if isinstance(v, list):
        return [_cpg_scrub(x, _sec) for x in v]
    if isinstance(v, dict):
        return {{k: _cpg_scrub(x, _sec) for k, x in v.items()}}
    return v
def _mtime(n):
    try:
        return os.path.getmtime(os.path.join(_base, n))
    except OSError:
        return 0
if os.path.isdir(_base):
    _entries = sorted(os.listdir(_base), key=_mtime, reverse=True)
    for _rid in _entries:
        _d = os.path.join(_base, _rid)
        if not os.path.isdir(_d):
            continue
        _alive = False
        try:
            _pid = int(open(_d + "/pid").read().strip())
            with open("/proc/%d/stat" % _pid) as _fh:
                _alive = _fh.read().split(") ", 1)[1][:1] not in ("Z", "X")
        except Exception:
            _alive = False
        if not _alive:
            continue
        try:
            with open(_d + "/meta.json") as _fh:
                _meta = json.load(_fh)
        except Exception:
            continue
        if not isinstance(_meta, dict) or _meta.get("run_id") != _rid:
            continue
        try:
            with open(_d + "/status.json") as _fh:
                _st = json.load(_fh)
        except Exception:
            _st = None
        _log, _new, _size, _skip = "", 0, 0, 0
        try:
            with open(_d + "/run.log", "rb") as _fh:
                _size = os.fstat(_fh.fileno()).st_size
                _off = max(0, _size - {DISCOVER_TAIL})
                _skip = _off
                _fh.seek(_off)
                _chunk = _fh.read({DISCOVER_TAIL})
                _size = max(_size, os.fstat(_fh.fileno()).st_size)
                if _skip:
                    _head = _chunk.find(b"\n") + 1
                    _skip += _head
                    _chunk = _chunk[_head:]
                _tail = _chunk.rfind(b"\n") + 1
                _new = _skip + _tail if _tail else _skip + len(_chunk)
                if _tail and _tail < len(_chunk):
                    _chunk = _chunk[:_tail]
                _log = _chunk.decode("utf-8", "replace")
        except FileNotFoundError:
            pass
        _sec = _redact.get(_rid, [])
        _runs.append({{
            "meta": _meta,
            "status": _cpg_scrub(_st, _sec),
            "alive": _alive,
            "log": _cpg_scrub(_log, _sec),
            "offset": _new,
            "size": _size,
            "skipped": _skip,
        }})
print("__CPG_RESULT__ " + json.dumps({{"runs": _runs}}))
"#
    )
}

/// Code that lists the files under a path on the VM, with their sizes.
pub fn vm_ls_snippet(path: &str) -> String {
    let quoted = serde_json::to_string(path).unwrap_or_else(|_| "\"\"".to_owned());
    format!(
        r#"import json, os
_path = {quoted}
_files = []
for _root, _dirs, _names in os.walk(_path):
    for _name in _names:
        _p = os.path.join(_root, _name)
        try:
            _size = os.path.getsize(_p)
        except OSError:
            _size = 0
        _files.append({{"path": _p, "size": _size}})
print("__CPG_RESULT__ " + json.dumps({{"files": _files}}))
"#
    )
}

/// The files a listing snippet reported.
pub fn parse_vm_files(value: &serde_json::Value) -> Vec<crate::backend::VmFile> {
    value
        .get("files")
        .and_then(|v| v.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|f| {
                    Some(crate::backend::VmFile {
                        path: f.get("path")?.as_str()?.to_owned(),
                        size: f.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Archives existing files under `CPG_KEEP_PATHS` into `CPG_KEEP_ARCHIVE` (`.tar.gz`).
pub fn keep_archive_snippet() -> String {
    format!(
        r#"import json, os, tarfile
_paths = json.loads(os.environ.pop("{ENV_KEEP_PATHS}", "[]"))
_out = os.environ.pop("{ENV_KEEP_ARCHIVE}")
os.makedirs(os.path.dirname(_out), exist_ok=True)
_tmp = _out + ".tmp"
_files = 0
_bytes = 0
_seen = set()
with tarfile.open(_tmp, "w:gz") as _tar:
    for _p in _paths:
        if not os.path.exists(_p):
            continue
        if os.path.isfile(_p):
            _arc = _p.lstrip("/") or os.path.basename(_p)
            if _p not in _seen:
                _seen.add(_p)
                _tar.add(_p, arcname=_arc, recursive=False)
                _files += 1
                try:
                    _bytes += os.path.getsize(_p)
                except OSError:
                    pass
        else:
            for _root, _dirs, _names in os.walk(_p):
                for _name in sorted(_names):
                    _full = os.path.join(_root, _name)
                    if os.path.isfile(_full) and _full not in _seen:
                        _seen.add(_full)
                        _arc = _full.lstrip("/") or _name
                        _tar.add(_full, arcname=_arc, recursive=False)
                        _files += 1
                        try:
                            _bytes += os.path.getsize(_full)
                        except OSError:
                            pass
if _files == 0:
    try:
        os.remove(_tmp)
    except OSError:
        pass
    print("__CPG_RESULT__ " + json.dumps({{"empty": True, "files": 0, "bytes": 0}}))
else:
    os.replace(_tmp, _out)
    print("__CPG_RESULT__ " + json.dumps({{"empty": False, "archive": _out, "files": _files, "bytes": os.path.getsize(_out)}}))
"#
    )
}

/// Removes a temporary Keep Plan archive from the VM after download.
pub fn keep_remove_snippet() -> String {
    format!(
        r#"import json, os
_p = os.environ.pop("{ENV_KEEP_ARCHIVE}", "")
_removed = False
if _p:
    try:
        os.remove(_p)
        _removed = True
    except OSError:
        pass
print("__CPG_RESULT__ " + json.dumps({{"removed": _removed}}))
"#
    )
}

/// Uploads `CPG_KEEP_PATHS` to `CPG_KEEP_REPO` using `HF_TOKEN` from memory only.
pub fn keep_hf_upload_snippet() -> String {
    format!(
        r#"import json, os, shutil, subprocess, tempfile
_paths = json.loads(os.environ.pop("{ENV_KEEP_PATHS}", "[]"))
_repo = os.environ.pop("{ENV_KEEP_REPO}", "")
_sub = os.environ.pop("{ENV_KEEP_SUBPATH}", "").strip("/")
_tok = os.environ.pop("HF_TOKEN", "")
def _scrub(msg):
    return msg.replace(_tok, "***") if _tok else msg
_files = 0
_bytes = 0
_items = []
for _p in _paths:
    if not os.path.exists(_p):
        continue
    if os.path.isfile(_p):
        _items.append((_p, os.path.basename(_p)))
        _files += 1
        try:
            _bytes += os.path.getsize(_p)
        except OSError:
            pass
    else:
        for _root, _dirs, _names in os.walk(_p):
            for _name in sorted(_names):
                _full = os.path.join(_root, _name)
                if os.path.isfile(_full):
                    _rel = os.path.relpath(_full, _p)
                    _base = os.path.basename(_p.rstrip("/")) or "keep"
                    _items.append((_full, os.path.join(_base, _rel)))
                    _files += 1
                    try:
                        _bytes += os.path.getsize(_full)
                    except OSError:
                        pass
if not _tok:
    print("__CPG_RESULT__ " + json.dumps({{"ok": False, "files": 0, "bytes": 0, "error": "HF_TOKEN is missing"}}))
elif _files == 0:
    print("__CPG_RESULT__ " + json.dumps({{"ok": False, "files": 0, "bytes": 0, "error": "no files found at keep paths"}}))
else:
    _stage = tempfile.mkdtemp(prefix="cpg-hf-")
    try:
        _err = None
        for _src, _rel in _items:
            _dst = os.path.join(_stage, _rel)
            os.makedirs(os.path.dirname(_dst), exist_ok=True)
            shutil.copy2(_src, _dst)
        try:
            from huggingface_hub import HfApi
            HfApi(token=_tok).upload_folder(
                folder_path=_stage,
                repo_id=_repo,
                path_in_repo=_sub or None,
                commit_message="colab-playground keep plan sync",
            )
        except ImportError:
            _cmd = ["hf", "upload", _repo, _stage] + ([_sub] if _sub else [])
            _env = dict(os.environ)
            _env["HF_TOKEN"] = _tok
            _r = subprocess.run(_cmd, env=_env, capture_output=True, text=True, timeout=600)
            if _r.returncode != 0:
                _err = _scrub((_r.stderr or _r.stdout or "hf upload failed").strip())
        if _err:
            print("__CPG_RESULT__ " + json.dumps({{"ok": False, "files": 0, "bytes": 0, "error": _err}}))
        else:
            print("__CPG_RESULT__ " + json.dumps({{"ok": True, "files": _files, "bytes": _bytes}}))
    except Exception as _e:
        print("__CPG_RESULT__ " + json.dumps({{"ok": False, "files": 0, "bytes": 0, "error": _scrub(f"{{type(_e).__name__}}: {{_e}}")}}))
    finally:
        shutil.rmtree(_stage, ignore_errors=True)
"#
    )
}

/// Copies `CPG_KEEP_PATHS` into a verified Google Drive mount at `CPG_KEEP_MOUNT`.
pub fn keep_drive_copy_snippet() -> String {
    format!(
        r#"import json, os, shutil
_paths = json.loads(os.environ.pop("{ENV_KEEP_PATHS}", "[]"))
_mount = os.environ.pop("{ENV_KEEP_MOUNT}", "/content/drive")
_dest = os.environ.pop("{ENV_KEEP_DRIVE_DEST}", "MyDrive/colab-playground").strip("/")
_files = 0
_bytes = 0
if not os.path.ismount(_mount):
    print("__CPG_RESULT__ " + json.dumps({{"ok": False, "files": 0, "bytes": 0, "error": "Google Drive is not mounted at " + _mount}}))
else:
    _target = os.path.join(_mount, _dest)
    try:
        for _p in _paths:
            if not os.path.exists(_p):
                continue
            if os.path.isfile(_p):
                _dst = os.path.join(_target, os.path.basename(_p))
                os.makedirs(os.path.dirname(_dst), exist_ok=True)
                shutil.copy2(_p, _dst)
                _files += 1
                _bytes += os.path.getsize(_dst)
            else:
                _base = os.path.basename(_p.rstrip("/")) or "keep"
                for _root, _dirs, _names in os.walk(_p):
                    for _name in sorted(_names):
                        _full = os.path.join(_root, _name)
                        if os.path.isfile(_full):
                            _rel = os.path.relpath(_full, _p)
                            _dst = os.path.join(_target, _base, _rel)
                            os.makedirs(os.path.dirname(_dst), exist_ok=True)
                            shutil.copy2(_full, _dst)
                            _files += 1
                            _bytes += os.path.getsize(_dst)
        if _files == 0:
            print("__CPG_RESULT__ " + json.dumps({{"ok": False, "files": 0, "bytes": 0, "error": "no files found at keep paths"}}))
        else:
            print("__CPG_RESULT__ " + json.dumps({{"ok": True, "files": _files, "bytes": _bytes, "destination": _target}}))
    except Exception as _e:
        print("__CPG_RESULT__ " + json.dumps({{"ok": False, "files": 0, "bytes": 0, "error": f"{{type(_e).__name__}}: {{_e}}"}}))
"#
    )
}

/// Checks whether `CPG_KEEP_MOUNT` is currently an active mount point on the VM.
pub fn keep_check_drive_mount_snippet() -> String {
    format!(
        r#"import json, os
_mount = os.environ.pop("{ENV_KEEP_MOUNT}", "/content/drive")
print("__CPG_RESULT__ " + json.dumps({{"mounted": os.path.ismount(_mount), "path": _mount}}))
"#
    )
}

/// Builds VM watchdog wind-down and periodic hooks for the VM-side targets of `plan`.
pub fn watchdog_hooks_for_keep_plan(
    plan: &crate::keep::KeepPlan,
    session: &str,
    hf_token: Option<&str>,
) -> Vec<crate::watchdog::WatchdogHook> {
    if plan.paths.is_empty() {
        return Vec::new();
    }
    let paths_json = serde_json::to_string(&plan.paths).unwrap_or_else(|_| "[]".into());
    let mut hooks = Vec::new();
    if let Some(hf) = &plan.hf
        && let Some(token) = hf_token.filter(|t| !t.trim().is_empty())
    {
        let mut env = std::collections::BTreeMap::new();
        env.insert(ENV_KEEP_PATHS.into(), paths_json.clone());
        env.insert(ENV_KEEP_REPO.into(), hf.repo.clone());
        env.insert(
            ENV_KEEP_SUBPATH.into(),
            hf.path_in_repo.clone().unwrap_or_default(),
        );
        env.insert("HF_TOKEN".into(), token.to_owned());
        hooks.push(crate::watchdog::WatchdogHook {
            argv: vec!["python3".into(), "-c".into(), keep_hf_upload_snippet()],
            env,
            timeout_s: 240,
            interval_s: hf.interval_minutes.map(|m| u64::from(m) * 60),
        });
    }
    if let Some(drive) = &plan.drive
        && drive.consented
        && drive.mounted
    {
        let mut env = std::collections::BTreeMap::new();
        env.insert(ENV_KEEP_PATHS.into(), paths_json);
        env.insert(ENV_KEEP_MOUNT.into(), drive.mount_path.clone());
        env.insert(
            ENV_KEEP_DRIVE_DEST.into(),
            format!("{}/{}", drive.dest_subpath.trim_end_matches('/'), session),
        );
        hooks.push(crate::watchdog::WatchdogHook {
            argv: vec!["python3".into(), "-c".into(), keep_drive_copy_snippet()],
            env,
            timeout_s: 240,
            interval_s: None,
        });
    }
    hooks
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// No `status.json` yet.
    #[default]
    Starting,
    Running,
    /// Done; endpoints may be tunnelled.
    Ready,
    Failed,
}

/// Parsed `status.json` (all text is untrusted plugin output; display only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunStatus {
    pub state: RunState,
    /// Current stage id (should match a manifest stage).
    #[serde(default)]
    pub stage: Option<String>,
    /// Progress of the stage, 0.0–1.0.
    #[serde(default)]
    pub progress: Option<f64>,
    /// Human-readable message (from the plugin, not localised).
    #[serde(default)]
    pub message: Option<String>,
    /// Arbitrary outputs (e.g. `model_id`).
    #[serde(default)]
    pub outputs: serde_json::Map<String, serde_json::Value>,
    /// Optional live counters. Missing or non-numeric fields are ignored.
    #[serde(default, deserialize_with = "de_metrics")]
    pub metrics: RunMetrics,
}

/// Numbers a script may report in `status.json` for the run page.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunMetrics {
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub loss: Option<f64>,
    #[serde(default)]
    pub tok_per_s: Option<f64>,
}

fn de_metrics<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<RunMetrics, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(metrics_from_value(&value))
}

fn metrics_from_value(value: &serde_json::Value) -> RunMetrics {
    let Some(obj) = value.as_object() else {
        return RunMetrics::default();
    };
    let number = |key: &str| {
        obj.get(key)
            .and_then(|v| v.as_f64())
            .filter(|n| n.is_finite())
    };
    RunMetrics {
        step: number("step"),
        loss: number("loss"),
        tok_per_s: number("tok_per_s"),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunPoll {
    /// `status.json` content, if present and valid.
    pub status: Option<RunStatus>,
    pub alive: bool,
    /// New log text since the previous offset.
    pub log: String,
    /// Offset to send next time.
    pub offset: u64,
    /// Size of the log file; more than `offset` while there is more to read.
    pub size: u64,
    /// Bytes left out before `log`, because the log grew faster than it was read.
    pub skipped: u64,
}

pub fn parse_poll(v: &serde_json::Value) -> Result<RunPoll> {
    let status = match v.get("status") {
        None | Some(serde_json::Value::Null) => None,
        Some(s) if s.get("_unreadable").is_some() => None,
        Some(s) => Some(parse_status(s)?),
    };
    let number = |key: &str| v.get(key).and_then(|n| n.as_u64()).unwrap_or(0);
    Ok(RunPoll {
        status,
        alive: v.get("alive").and_then(|a| a.as_bool()).unwrap_or(false),
        log: v
            .get("log")
            .and_then(|l| l.as_str())
            .unwrap_or_default()
            .to_owned(),
        offset: number("offset"),
        size: number("size"),
        skipped: number("skipped"),
    })
}

/// One live run entry returned by [`plugin_discover_snippet`].
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredVmRun {
    pub meta: RunMeta,
    pub status: Option<RunStatus>,
    pub alive: bool,
    pub log: String,
    pub offset: u64,
    pub size: u64,
    pub skipped: u64,
}

fn is_safe_run_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && !id.contains("..")
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('\0')
}

pub fn parse_discovered(v: &serde_json::Value) -> Vec<DiscoveredVmRun> {
    let Some(runs) = v.get("runs").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    let number =
        |obj: &serde_json::Value, key: &str| obj.get(key).and_then(|n| n.as_u64()).unwrap_or(0);
    runs.iter()
        .filter_map(|item| {
            let alive = item.get("alive").and_then(|a| a.as_bool()).unwrap_or(false);
            if !alive {
                return None;
            }
            let meta: RunMeta = serde_json::from_value(item.get("meta")?.clone()).ok()?;
            if !is_safe_run_id(&meta.run_id) || meta.plugin.is_empty() {
                return None;
            }
            let status = match item.get("status") {
                None | Some(serde_json::Value::Null) => None,
                Some(s) => parse_status(s).ok(),
            };
            Some(DiscoveredVmRun {
                meta,
                status,
                alive,
                log: item
                    .get("log")
                    .and_then(|l| l.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                offset: number(item, "offset"),
                size: number(item, "size"),
                skipped: number(item, "skipped"),
            })
        })
        .collect()
}

/// Parses a `status.json` object strictly enough to render it safely.
pub fn parse_status(v: &serde_json::Value) -> Result<RunStatus> {
    let mut s: RunStatus = serde_json::from_value(v.clone())
        .map_err(|e| Error::new(ErrorKind::Parse).detail(format!("status.json: {e}")))?;
    s.progress = s
        .progress
        .filter(|p| p.is_finite())
        .map(|p| p.clamp(0.0, 1.0));
    if let Some(m) = s.message.as_mut() {
        truncate(m, 2000);
    }
    Ok(s)
}

fn truncate(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_parsing() {
        let hw =
            parse_hardware(&serde_json::json!({"gpu": "NVIDIA A100-SXM4-80GB", "vram_mib": 81920}));
        assert_eq!(hw.vram_mib, Some(81920));
        assert_eq!(
            parse_hardware(&serde_json::json!({"gpu": null, "vram_mib": null})),
            Hardware::default()
        );
    }

    #[test]
    fn launch_snippet_embeds_base64_only() {
        let s = plugin_launch_snippet(
            b"print('\"; import os; os.system(\"x\")')",
            br#"{"run_id":"r1","plugin":"p1"}"#,
        );
        assert!(!s.contains("os.system"));
        assert!(s.contains("start_new_session=True"));
        assert!(s.contains("os.environ.pop(_k, None)"));
        assert!(s.contains("meta.json"));
        assert!(s.contains(ENV_SECRET_KEYS) && s.contains("_cpg_redact"));
        let p = plugin_poll_snippet();
        assert!(p.contains("_cpg_scrub(_log)") && p.contains("_cpg_scrub(_st)"));
        let d = plugin_discover_snippet();
        assert!(d.contains("meta.json") && d.contains("_cpg_scrub(_log"));
    }

    #[test]
    fn discover_parsing_skips_dead_or_invalid_entries() {
        let parsed = parse_discovered(&serde_json::json!({
            "runs": [
                {
                    "meta": {"run_id": "r1", "plugin": "llama-cpp", "keep_alive": true},
                    "status": {"state": "running", "stage": "download"},
                    "alive": true,
                    "log": "line1\n",
                    "offset": 6,
                    "size": 6,
                    "skipped": 0
                },
                {
                    "meta": {"run_id": "r2", "plugin": "llama-cpp"},
                    "alive": false,
                    "log": ""
                },
                {
                    "meta": {"run_id": "", "plugin": "llama-cpp"},
                    "alive": true
                }
            ]
        }));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].meta.run_id, "r1");
        assert!(parsed[0].meta.keep_alive);
        assert_eq!(parsed[0].offset, 6);
    }

    #[test]
    fn poll_parsing_and_status_sanitising() {
        let v = serde_json::json!({
            "status": {"state": "running", "stage": "download", "progress": 3.0, "message": "x",
                       "outputs": {"model_id": "m"}},
            "alive": true, "log": "line\n", "offset": 5
        });
        let p = parse_poll(&v).unwrap();
        let s = p.status.unwrap();
        assert_eq!(s.state, RunState::Running);
        assert_eq!(s.progress, Some(1.0));
        assert_eq!(s.outputs["model_id"], "m");
        assert_eq!(s.metrics, RunMetrics::default());
        let with = parse_poll(&serde_json::json!({
            "status": {"state": "running", "metrics": {"step": 12, "loss": 0.5, "tok_per_s": 18.2, "note": "ignored"}},
            "alive": true
        }))
        .unwrap();
        let metrics = with.status.unwrap().metrics;
        assert_eq!(metrics.step, Some(12.0));
        assert_eq!(metrics.loss, Some(0.5));
        assert_eq!(metrics.tok_per_s, Some(18.2));
        let junk = parse_poll(&serde_json::json!({
            "status": {"state": "running", "metrics": "nope"},
            "alive": true
        }))
        .unwrap();
        assert_eq!(junk.status.unwrap().metrics, RunMetrics::default());
        assert_eq!((p.offset, p.size, p.skipped), (5, 0, 0));
        let p =
            parse_poll(&serde_json::json!({"alive": true, "offset": 5, "size": 9, "skipped": 2}))
                .unwrap();
        assert_eq!((p.offset, p.size, p.skipped), (5, 9, 2));
        let p = parse_poll(&serde_json::json!({"status": null, "alive": false})).unwrap();
        assert!(p.status.is_none() && !p.alive);
        assert!(parse_poll(&serde_json::json!({"status": {"state": "bogus"}})).is_err());
    }

    #[test]
    fn the_files_of_a_listing_are_read() {
        let value = serde_json::json!({
            "files": [
                {"path": "/content/ComfyUI/output/a.png", "size": 1234},
                {"path": "/content/ComfyUI/output/b.png", "size": 0},
            ]
        });
        let files = parse_vm_files(&value);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "/content/ComfyUI/output/a.png");
        assert_eq!(files[0].size, 1234);
        assert_eq!(files[1].size, 0);
    }

    #[test]
    fn a_listing_without_files_is_empty() {
        assert!(parse_vm_files(&serde_json::json!({})).is_empty());
        assert!(parse_vm_files(&serde_json::json!({"files": "no"})).is_empty());
    }

    #[test]
    fn the_listing_snippet_asks_for_the_files_of_a_path() {
        let snippet = vm_ls_snippet("/content/ComfyUI/output");
        assert!(snippet.contains("/content/ComfyUI/output"));
        assert!(snippet.contains("os.walk"));
    }

    #[test]
    fn parse_discovered_rejects_unsafe_run_ids() {
        for bad in ["", ".", "..", "../evil", "a/b", "a\\b", "a\0b"] {
            let parsed = parse_discovered(&serde_json::json!({
                "runs": [{
                    "alive": true,
                    "meta": {"run_id": bad, "plugin": "comfyui"}
                }]
            }));
            assert!(parsed.is_empty(), "expected {bad:?} to be rejected");
        }
        assert!(plugin_discover_snippet().contains("_meta.get(\"run_id\") != _rid"));
    }

    #[test]
    fn keep_snippets_and_hooks_respect_secrets_and_drive_consent() {
        use crate::keep::{DriveKeepTarget, HfKeepTarget, KeepPlan};

        assert!(keep_archive_snippet().contains("tarfile"));
        let hf = keep_hf_upload_snippet();
        assert!(hf.contains("os.environ.pop(\"HF_TOKEN\""));
        assert!(hf.contains("_scrub"));
        assert!(keep_drive_copy_snippet().contains("os.path.ismount"));
        assert!(keep_check_drive_mount_snippet().contains("os.path.ismount"));

        let mut plan = KeepPlan {
            paths: vec!["/content/out".into()],
            local: None,
            hf: Some(HfKeepTarget {
                repo: "alice/ckpt".into(),
                path_in_repo: Some("run1".into()),
                interval_minutes: Some(15),
            }),
            drive: Some(DriveKeepTarget {
                mount_path: "/content/drive".into(),
                dest_subpath: "MyDrive/out".into(),
                consented: false,
                mounted: true,
            }),
        };
        let hooks = watchdog_hooks_for_keep_plan(&plan, "sess-1", Some("hf_secret"));
        assert_eq!(hooks.len(), 1, "unconsented Drive must never become a hook");
        assert_eq!(hooks[0].interval_s, Some(900));
        assert_eq!(
            hooks[0].env.get("HF_TOKEN").map(String::as_str),
            Some("hf_secret")
        );

        plan.drive.as_mut().unwrap().consented = true;
        let hooks = watchdog_hooks_for_keep_plan(&plan, "sess-1", Some("hf_secret"));
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[1].interval_s, None);
    }
}
