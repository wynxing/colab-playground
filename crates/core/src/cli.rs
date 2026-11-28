//! Typed wrapper over the `colab` CLI.
//!
//! Failures are classified by [`classify_failure`] from the CLI's own messages
//! (see `google-colab-cli` sources), because exit codes vary across versions:
//! in `0.8.0`, `colab stop` of a missing session and `colab exec` after a
//! remote Python exception exit `0`, whereas post-`0.8.0` exits `1` (or
//! propagates `SystemExit` codes).

use std::time::Duration;

use crate::clock::CancelToken;
use crate::error::{Error, ErrorKind, Result};
use crate::host::{
    Host, Line, LineAction, ScriptOutput, ScriptRequest, StdinMode, Stream, is_env_key, tail,
};
use crate::paths::HostPath;
use crate::sessions::{Accelerator, ServerSession, validate_session_name};
use crate::usage::Usage;

/// Prefix of the single JSON result line printed by remote snippets.
pub const RESULT_MARKER: &str = "__CPG_RESULT__ ";

/// First line of every `colab exec` payload. If the CLI unexpectedly starts
/// its OAuth prompt, `input()` consumes this harmless line instead of code.
pub const EXEC_PREAMBLE: &str = "# colab-playground\n";

/// The CLI prints this on stderr right before reading an authorisation code.
pub const AUTH_PROMPT_MARKER: &str = "To authorize colab-cli";

/// VM port used by Colab itself; never forward or bind it.
pub const RESERVED_VM_PORT: u16 = 8080;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    /// `colab sessions`.
    Sessions,
    /// `colab usage`.
    Usage,
    /// `colab new -s NAME [--gpu G|--tpu T] [--high-mem]`.
    New {
        name: String,
        accelerator: Accelerator,
        /// Request the high-RAM shape (ignored where unsupported).
        high_mem: bool,
    },
    /// `colab exec -s NAME --timeout S [--env K=V]...` (code on stdin).
    Exec {
        name: String,
        /// Environment for the remote kernel.
        env: Vec<(String, String)>,
        /// Idle timeout in seconds (0 = none).
        timeout_secs: u32,
    },
    /// `colab stop -s NAME`.
    Stop {
        /// Session name.
        name: String,
    },
    /// `colab url -s NAME`.
    Url {
        /// Session name.
        name: String,
    },
    /// `colab restart-kernel -s NAME`.
    RestartKernel {
        /// Session name.
        name: String,
    },
    /// `colab ssh --force --proxy-mode -s NAME`.
    SshReset {
        /// Session name.
        name: String,
    },
    /// `colab download -s NAME REMOTE LOCAL`.
    Download {
        /// Session name.
        name: String,
        /// The path on the VM.
        remote: String,
        /// Where the file lands on this computer.
        local: String,
    },
    /// `colab ls -s NAME PATH`.
    Ls {
        /// Session name.
        name: String,
        /// Directory or file path on the VM.
        path: String,
    },
    /// `colab upload -s NAME LOCAL REMOTE`.
    Upload {
        /// Session name.
        name: String,
        /// Local file path on the host.
        local: String,
        /// Target path on the VM.
        remote: String,
    },
    /// `colab drivemount -s NAME PATH`.
    DriveMount {
        /// Session name.
        name: String,
        /// Mount path on the VM (e.g. `/content/drive`).
        path: String,
    },
    /// `colab version`.
    Version,
    /// `colab whoami` (triggers login when no credential exists).
    Whoami,
}

/// Builds the argument vector (without the program) for `cmd`.
pub fn build_argv(state_file: Option<&HostPath>, cmd: &CliCommand) -> Result<Vec<String>> {
    let mut argv = Vec::new();
    if let Some(state) = state_file {
        argv.push("--config".to_owned());
        argv.push(state.as_str().to_owned());
    }
    match cmd {
        CliCommand::Sessions => argv.push("sessions".into()),
        CliCommand::Usage => argv.push("usage".into()),
        CliCommand::Version => argv.push("version".into()),
        CliCommand::Whoami => argv.push("whoami".into()),
        CliCommand::New {
            name,
            accelerator,
            high_mem,
        } => {
            validate_session_name(name)?;
            argv.extend(["new".into(), "-s".into(), name.clone()]);
            argv.extend(accelerator.new_flags());
            if *high_mem && accelerator.supports_high_mem() {
                argv.push("--high-mem".into());
            }
        }
        CliCommand::Exec {
            name,
            env,
            timeout_secs,
        } => {
            validate_session_name(name)?;
            argv.extend([
                "exec".into(),
                "-s".into(),
                name.clone(),
                "--timeout".into(),
                timeout_secs.to_string(),
            ]);
            for (k, v) in env {
                if !is_env_key(k) {
                    return Err(Error::invalid(
                        "env",
                        format!("invalid environment key {k:?}"),
                    ));
                }
                if v.contains('\0') {
                    return Err(Error::invalid("env", format!("NUL byte in value of {k}")));
                }
                argv.push("--env".into());
                argv.push(format!("{k}={v}"));
            }
        }
        CliCommand::Stop { name } => {
            validate_session_name(name)?;
            argv.extend(["stop".into(), "-s".into(), name.clone()]);
        }
        CliCommand::Url { name } => {
            validate_session_name(name)?;
            argv.extend(["url".into(), "-s".into(), name.clone()]);
        }
        CliCommand::RestartKernel { name } => {
            validate_session_name(name)?;
            argv.extend(["restart-kernel".into(), "-s".into(), name.clone()]);
        }
        CliCommand::SshReset { name } => {
            validate_session_name(name)?;
            argv.extend([
                "ssh".into(),
                "--force".into(),
                "--proxy-mode".into(),
                "-s".into(),
                name.clone(),
            ]);
        }
        CliCommand::Download {
            name,
            remote,
            local,
        } => {
            validate_session_name(name)?;
            if remote.is_empty() || local.is_empty() {
                return Err(Error::invalid("download", "the path may not be empty"));
            }
            argv.extend([
                "download".into(),
                "-s".into(),
                name.clone(),
                remote.clone(),
                local.clone(),
            ]);
        }
        CliCommand::Ls { name, path } => {
            validate_session_name(name)?;
            if path.trim().is_empty() || path.contains('\0') {
                return Err(Error::invalid("ls", "the path may not be empty"));
            }
            argv.extend(["ls".into(), "-s".into(), name.clone(), path.clone()]);
        }
        CliCommand::Upload {
            name,
            local,
            remote,
        } => {
            validate_session_name(name)?;
            if local.is_empty() || remote.is_empty() {
                return Err(Error::invalid("upload", "the path may not be empty"));
            }
            argv.extend([
                "upload".into(),
                "-s".into(),
                name.clone(),
                local.clone(),
                remote.clone(),
            ]);
        }
        CliCommand::DriveMount { name, path } => {
            validate_session_name(name)?;
            crate::keep::validate_vm_path("drive_mount_path", path)?;
            argv.extend(["drivemount".into(), "-s".into(), name.clone(), path.clone()]);
        }
    }
    Ok(argv)
}

/// Parses `colab sessions` output. Lines that are not session lines (banners,
/// "Pruned N stale local session(s).", "No active sessions…") are ignored.
pub fn parse_sessions(stdout: &str) -> Vec<ServerSession> {
    stdout.lines().filter_map(parse_session_line).collect()
}

fn parse_session_line(line: &str) -> Option<ServerSession> {
    let line = line.trim();
    let rest = line.strip_prefix('[')?;
    let (name, rest) = rest.split_once("] ")?;
    let mut parts = rest.split(" | ");
    let endpoint = parts.next()?.trim();
    if endpoint.is_empty() || endpoint.contains(' ') {
        return None;
    }
    let mut s = ServerSession {
        name: if name == "?" {
            None
        } else {
            Some(name.to_owned())
        },
        endpoint: endpoint.to_owned(),
        hardware: String::new(),
        accelerator: None,
        shape: String::new(),
        variant: String::new(),
        status: None,
    };
    let mut saw_hardware = false;
    for part in parts {
        let (k, v) = part.split_once(": ")?;
        let v = v.trim().to_owned();
        match k.trim() {
            "Hardware" => {
                saw_hardware = true;
                s.accelerator = Accelerator::from_label(&v);
                s.hardware = v;
            }
            "Shape" => s.shape = v,
            "Variant" => s.variant = v,
            "Status" => s.status = Some(v),
            _ => {}
        }
    }
    saw_hardware.then_some(s)
}

pub fn parse_usage(stdout: &str) -> Result<Usage> {
    let mut balance = None;
    let mut rate = None;
    let mut count = None;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("Current balance:") {
            balance = v
                .trim()
                .trim_end_matches("compute units")
                .trim()
                .parse::<f64>()
                .ok();
        } else if let Some(v) = line.strip_prefix("Usage rate:") {
            rate = v.trim().trim_end_matches("/hr").trim().parse::<f64>().ok();
        } else if let Some(v) = line.strip_prefix("Active assignments:") {
            count = v.trim().parse::<f64>().ok().map(|c| c.max(0.0) as u32);
        }
    }
    match (balance, rate, count) {
        (Some(b), Some(r), Some(c)) if b.is_finite() && r.is_finite() => Ok(Usage {
            balance_cu: b,
            rate_cu_per_hour: r,
            assignments: c,
        }),
        _ => Err(Error::new(ErrorKind::Parse).detail(format!(
            "unexpected `colab usage` output: {}",
            tail(stdout, 300)
        ))),
    }
}

/// A command as a log line: what it is and what it is about, and never a value that could be a
/// secret — an environment entry's value is where tokens travel.
fn describe(cmd: &CliCommand) -> String {
    match cmd {
        CliCommand::Sessions => "sessions".to_owned(),
        CliCommand::Usage => "usage".to_owned(),
        CliCommand::New { .. } => "new".to_owned(),
        CliCommand::Version => "version".to_owned(),
        CliCommand::Whoami => "whoami".to_owned(),
        CliCommand::Stop { name } => format!("stop -s {name}"),
        CliCommand::Url { name } => format!("url -s {name}"),
        CliCommand::RestartKernel { name } => format!("restart-kernel -s {name}"),
        CliCommand::SshReset { name } => format!("ssh --force --proxy-mode -s {name}"),
        CliCommand::Download { name, remote, .. } => format!("download -s {name} {remote} <here>"),
        CliCommand::Ls { name, path } => format!("ls -s {name} {path}"),
        CliCommand::Upload { name, remote, .. } => format!("upload -s {name} <here> {remote}"),
        CliCommand::DriveMount { name, path } => format!("drivemount -s {name} {path}"),
        CliCommand::Exec {
            name,
            env,
            timeout_secs,
        } => {
            let keys: Vec<&str> = env.iter().map(|(key, _)| key.as_str()).collect();
            format!(
                "exec -s {name} --timeout {timeout_secs} (env {})",
                if keys.is_empty() {
                    "none".to_owned()
                } else {
                    keys.join(",")
                }
            )
        }
    }
}

/// Parses `colab ls` output (`name/` for directories, `name` for files).
pub fn parse_ls(stdout: &str) -> Vec<crate::backend::VmEntry> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with("[colab]")
                && !line.starts_with("A new version of colab-cli")
        })
        .map(|line| {
            if let Some(dir) = line.strip_suffix('/') {
                crate::backend::VmEntry {
                    name: dir.to_owned(),
                    is_dir: true,
                }
            } else {
                crate::backend::VmEntry {
                    name: line.to_owned(),
                    is_dir: false,
                }
            }
        })
        .collect()
}

/// Extracts the JSON value printed after [`RESULT_MARKER`] (last one wins).
pub fn find_result(stdout: &str) -> Option<serde_json::Value> {
    stdout
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix(RESULT_MARKER))
        .and_then(|j| serde_json::from_str(j.trim()).ok())
}

/// Parses `"N of M [SSH] sessions in use"` from a `colab ssh` HTTP 429 message.
pub(crate) fn parse_ssh_session_limit(text: &str) -> Option<u32> {
    let (before, _) = text.split_once(" sessions in use")?;
    let before = before.strip_suffix(" SSH").unwrap_or(before);
    let mut rev = before.split_whitespace().rev();
    let limit = rev.next()?.parse::<u32>().ok()?;
    let of = rev.next()?;
    let used = rev.next()?.parse::<u32>().ok()?;
    (of == "of" && used > 0 && limit > 0).then_some(limit)
}

fn failure_error(kind: ErrorKind, out: &ScriptOutput) -> Error {
    let detail = if out.stderr.trim().is_empty() {
        &out.stdout
    } else {
        &out.stderr
    };
    let mut e = Error::new(kind).detail(tail(detail, 800));
    if let Some(code) = out.code {
        e = e.with("exit_code", code);
    }
    e
}

fn is_local_not_installed(colab: &str, out: &ScriptOutput) -> bool {
    if !matches!(out.code, Some(126 | 127)) {
        return false;
    }
    let stderr = &out.stderr;
    let mentions_exec =
        stderr.contains("bash: line 1:") || (!colab.is_empty() && stderr.contains(colab));
    let missing_or_denied = stderr.contains("No such file")
        || stderr.contains("not found")
        || stderr.contains("Permission denied")
        || stderr.contains("cannot execute");
    mentions_exec && missing_or_denied
}

fn is_local_cli_exec_failure(out: &ScriptOutput) -> bool {
    if out.code.is_none() {
        return true;
    }
    let stderr = &out.stderr;
    stderr.contains("Usage: colab")
        || stderr.contains("No such command")
        || stderr.contains("No such option")
        || stderr.contains("Invalid value for")
        || stderr.contains("Missing option")
        || stderr.contains("[colab] Invalid --env")
        || stderr.contains("colab_cli/")
        || stderr.contains(r"colab_cli\")
        || stderr.contains("No module named 'colab_cli'")
        || stderr.contains("ColabRequestError")
        || out.stdout.contains("[colab] Error: No input provided")
}

fn classify_exec_failure(colab: &str, out: &ScriptOutput) -> Option<Error> {
    if is_local_not_installed(colab, out) {
        return Some(failure_error(ErrorKind::NotInstalled, out));
    }
    let all = format!("{}\n{}", out.stdout, out.stderr);
    let has = |s: &str| all.contains(s);
    let kind = if has("Could not read the OAuth token from the OS keyring") {
        Some(ErrorKind::KeyringUnavailable)
    } else if has(AUTH_PROMPT_MARKER) || has("Enter the authorization code") {
        Some(ErrorKind::AuthRequired)
    } else if has("appears to be lost") || (has("ession '") && has("' not found")) {
        Some(ErrorKind::SessionGone)
    } else if has("Connection was lost")
        || has("Kernel dead before")
        || has("Kernel restarting before")
    {
        Some(ErrorKind::ConnectionLost)
    } else if has("Timeout waiting for") {
        Some(ErrorKind::ExecTimeout)
    } else if is_local_cli_exec_failure(out) {
        Some(ErrorKind::Other)
    } else {
        None
    };
    kind.map(|k| failure_error(k, out))
}

pub fn classify_failure(out: &ScriptOutput) -> Error {
    let all = format!("{}\n{}", out.stdout, out.stderr);
    let has = |s: &str| all.contains(s);
    let kind = if matches!(out.code, Some(126) | Some(127))
        && (has("No such file")
            || has("not found")
            || has("Permission denied")
            || has("cannot execute"))
    {
        ErrorKind::NotInstalled
    } else if has("Could not read the OAuth token from the OS keyring") {
        ErrorKind::KeyringUnavailable
    } else if has(AUTH_PROMPT_MARKER) || has("Enter the authorization code") {
        ErrorKind::AuthRequired
    } else if has("Backend rejected accelerator") {
        ErrorKind::NotEntitled
    } else if has("Allocation refused") {
        ErrorKind::CapacityRefused
    } else if has("(HTTP 429)") {
        ErrorKind::SshBusy
    } else if has("appears to be lost") || (has("ession '") && has("' not found")) {
        ErrorKind::SessionGone
    } else if has("Connection was lost")
        || has("Kernel dead before")
        || has("Kernel restarting before")
    {
        ErrorKind::ConnectionLost
    } else if has("Timeout waiting for") {
        ErrorKind::ExecTimeout
    } else if (has("Download failed:") || has("Upload failed:"))
        && (has("ReadTimeout")
            || has("ConnectTimeout")
            || has("Read timed out")
            || has("read timeout=")
            || has("connect timeout="))
    {
        ErrorKind::Timeout
    } else if has("Local file '") && has("' not found.")
        || has("File or directory not found:")
        || has("Upload failed:")
        || has("Download failed:")
    {
        ErrorKind::Io
    } else {
        ErrorKind::Other
    };
    let mut e = failure_error(kind, out);
    if kind == ErrorKind::SshBusy
        && let Some(limit) = parse_ssh_session_limit(&all)
    {
        e = e.with("limit", limit);
    }
    e
}

/// Whether `colab new` died of the `requests` read timeout: Colab never
/// answered the assignment request, and the CLI's own retries (each waiting
/// about two minutes) ran out. The CLI prints uncaught exceptions as a rich
/// traceback that wraps long lines, so this matches the exception line's
/// start (`ReadTimeout: …`, or `requests.exceptions.ReadTimeout: …` in a plain
/// traceback), never the message after it.
fn allocation_read_timed_out(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("ReadTimeout:") || line.starts_with("requests.exceptions.ReadTimeout:")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// "Session terminated."
    Terminated,
    /// The CLI had no local record (it prints "not found" and exits 0 in 0.8.0 or 1 in newer versions).
    NotFound,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecOutput {
    pub stdout: String,
    /// Remote stderr (tracebacks land here).
    pub stderr: String,
    /// The value after [`RESULT_MARKER`], if printed.
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct Cli {
    pub host: Host,
    /// Absolute path of `colab` on the host.
    pub colab: String,
    /// Environment added to every invocation (tests isolate `HOME` here).
    pub base_env: Vec<(String, String)>,
}

const RUN_COLAB: &str = r#"exec "$CPG_COLAB" "$@""#;

impl Cli {
    pub fn new(host: Host, colab: impl Into<String>, base_env: Vec<(String, String)>) -> Self {
        Cli {
            host,
            colab: colab.into(),
            base_env,
        }
    }

    pub fn request(&self, state: Option<&HostPath>, cmd: &CliCommand) -> Result<ScriptRequest> {
        let mut req = ScriptRequest::new(RUN_COLAB).args(build_argv(state, cmd)?);
        req.env = self.base_env.clone();
        req.env.push(("CPG_COLAB".into(), self.colab.clone()));
        Ok(req)
    }

    /// Runs `cmd`. An unexpected OAuth prompt kills the process and yields
    /// `AuthRequired` (or `KeyringUnavailable` if the keyring could not be read).
    /// Non-zero exits are *not* turned into errors here.
    pub fn run(
        &self,
        state: Option<&HostPath>,
        cmd: &CliCommand,
        stdin: StdinMode,
        timeout: Option<Duration>,
        cancel: &CancelToken,
        on_line: &mut dyn FnMut(&Line),
    ) -> Result<ScriptOutput> {
        // Two lines around every command, because a command that started and never finished is how
        // a stuck run is told apart from a slow one.
        let described = describe(cmd);
        log::info!("cli: {described} (timeout {timeout:?})");
        let mut req = self.request(state, cmd)?;
        req.stdin = stdin;
        req.timeout = timeout;
        let interactive = matches!(cmd, CliCommand::Whoami);
        let out = self.host.run(req, cancel, &mut |line| {
            on_line(line);
            if !interactive
                && line.stream == Stream::Stderr
                && line.text.contains(AUTH_PROMPT_MARKER)
            {
                LineAction::Kill
            } else {
                LineAction::Continue
            }
        })?;
        log::info!(
            "cli: {described} -> {}",
            match out.code {
                Some(code) => format!("exit {code}"),
                None => "no exit code".to_owned(),
            }
        );
        if out.killed {
            let mut e = classify_failure(&out);
            if !matches!(e.kind, ErrorKind::KeyringUnavailable) {
                e = e.with_kind(ErrorKind::AuthRequired);
            }
            return Err(e);
        }
        Ok(out)
    }

    fn run_simple(
        &self,
        state: Option<&HostPath>,
        cmd: &CliCommand,
        timeout: u64,
        cancel: &CancelToken,
    ) -> Result<ScriptOutput> {
        self.run(
            state,
            cmd,
            StdinMode::Null,
            Some(Duration::from_secs(timeout)),
            cancel,
            &mut |_| {},
        )
    }

    pub fn sessions(&self, state: &HostPath, cancel: &CancelToken) -> Result<Vec<ServerSession>> {
        let out = self.run_simple(Some(state), &CliCommand::Sessions, 120, cancel)?;
        if !out.success() {
            return Err(classify_failure(&out));
        }
        Ok(parse_sessions(&out.stdout))
    }

    pub fn usage(&self, state: &HostPath, cancel: &CancelToken) -> Result<Usage> {
        let out = self.run_simple(Some(state), &CliCommand::Usage, 120, cancel)?;
        if !out.success() {
            return Err(classify_failure(&out));
        }
        parse_usage(&out.stdout)
    }

    /// `colab new`. Never pass a name that already exists: the CLI silently
    /// overwrites the local record and orphans the previous VM.
    pub fn new_session(
        &self,
        state: &HostPath,
        name: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<()> {
        let cmd = CliCommand::New {
            name: name.to_owned(),
            accelerator,
            high_mem,
        };
        let out = self.run_simple(Some(state), &cmd, 900, cancel)?;
        if out.success() && out.stdout.contains("Session READY") {
            Ok(())
        } else {
            let mut e = classify_failure(&out);
            if out.success() && e.kind == ErrorKind::Other {
                e = e.detail(format!(
                    "`colab new` did not report READY: {}",
                    tail(&out.stdout, 300)
                ));
            } else if e.kind == ErrorKind::Other && allocation_read_timed_out(&out.stderr) {
                e = e.with_kind(ErrorKind::AllocationTimeout);
            }
            Err(e)
        }
    }

    /// `colab exec` with `code` on stdin (prefixed by [`EXEC_PREAMBLE`]).
    ///
    /// Fails only for CLI-level problems (session gone, connection lost, idle
    /// timeout, auth); a Python exception in `code` returns `Ok` with the
    /// traceback in `stderr` and no `result`.
    pub fn exec(
        &self,
        state: &HostPath,
        name: &str,
        env: &[(String, String)],
        code: &str,
        timeout_secs: u32,
        cancel: &CancelToken,
    ) -> Result<ExecOutput> {
        let cmd = CliCommand::Exec {
            name: name.to_owned(),
            env: env.to_vec(),
            timeout_secs,
        };
        let stdin = StdinMode::Bytes(format!("{EXEC_PREAMBLE}{code}").into_bytes());
        let overall = Duration::from_secs(u64::from(timeout_secs.max(30)) * 4 + 120);
        let out = self.run(Some(state), &cmd, stdin, Some(overall), cancel, &mut |_| {})?;
        // CLI-level failures (lost session, dropped connection, idle timeout,
        // local usage/CLI errors) fail here; exceptions or `SystemExit` raised
        // by remote `code` exit 0 in 0.8.0 and non-zero in newer CLIs, and are
        // returned as `ExecOutput` without a `result`.
        if !out.success() {
            if let Some(err) = classify_exec_failure(&self.colab, &out) {
                return Err(err);
            }
            return Ok(ExecOutput {
                result: None,
                stdout: out.stdout,
                stderr: out.stderr,
            });
        }
        Ok(ExecOutput {
            result: find_result(&out.stdout),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    /// Like [`Cli::exec`] but requires the [`RESULT_MARKER`] line.
    pub fn exec_json(
        &self,
        state: &HostPath,
        name: &str,
        env: &[(String, String)],
        code: &str,
        timeout_secs: u32,
        cancel: &CancelToken,
    ) -> Result<serde_json::Value> {
        let out = self.exec(state, name, env, code, timeout_secs, cancel)?;
        out.result.ok_or_else(|| {
            Error::new(ErrorKind::Parse)
                .detail(format!("remote snippet failed: {}", tail(&out.stderr, 600)))
        })
    }

    pub fn stop(&self, state: &HostPath, name: &str, cancel: &CancelToken) -> Result<StopOutcome> {
        let out = self.run_simple(
            Some(state),
            &CliCommand::Stop {
                name: name.to_owned(),
            },
            300,
            cancel,
        )?;
        if out.success() && out.stdout.contains("Session terminated") {
            Ok(StopOutcome::Terminated)
        } else {
            let err = classify_failure(&out);
            if err.kind == ErrorKind::SessionGone && matches!(out.code, Some(0 | 1)) {
                Ok(StopOutcome::NotFound)
            } else {
                Err(err)
            }
        }
    }

    /// `colab download -s NAME REMOTE LOCAL`: a file comes to this computer.
    pub fn download(
        &self,
        state: &HostPath,
        name: &str,
        remote: &str,
        local: &str,
        cancel: &CancelToken,
    ) -> Result<()> {
        let out = self.run_simple(
            Some(state),
            &CliCommand::Download {
                name: name.to_owned(),
                remote: remote.to_owned(),
                local: local.to_owned(),
            },
            900,
            cancel,
        )?;
        if !out.success() {
            return Err(classify_failure(&out));
        }
        Ok(())
    }

    /// `colab ls -s NAME PATH`: lists entries in a VM directory.
    pub fn ls(
        &self,
        state: &HostPath,
        name: &str,
        path: &str,
        cancel: &CancelToken,
    ) -> Result<Vec<crate::backend::VmEntry>> {
        let out = self.run_simple(
            Some(state),
            &CliCommand::Ls {
                name: name.to_owned(),
                path: path.to_owned(),
            },
            120,
            cancel,
        )?;
        if !out.success() {
            return Err(classify_failure(&out));
        }
        Ok(parse_ls(&out.stdout))
    }

    /// `colab upload -s NAME LOCAL REMOTE`: sends a local file to the VM.
    pub fn upload(
        &self,
        state: &HostPath,
        name: &str,
        local: &str,
        remote: &str,
        cancel: &CancelToken,
    ) -> Result<()> {
        let out = self.run_simple(
            Some(state),
            &CliCommand::Upload {
                name: name.to_owned(),
                local: local.to_owned(),
                remote: remote.to_owned(),
            },
            900,
            cancel,
        )?;
        if !out.success() {
            return Err(classify_failure(&out));
        }
        Ok(())
    }

    pub fn url(&self, state: &HostPath, name: &str, cancel: &CancelToken) -> Result<String> {
        let out = self.run_simple(
            Some(state),
            &CliCommand::Url {
                name: name.to_owned(),
            },
            60,
            cancel,
        )?;
        if !out.success() {
            return Err(classify_failure(&out));
        }
        out.stdout
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("https://") || line.starts_with("http://"))
            .map(str::to_owned)
            .ok_or_else(|| Error::new(ErrorKind::Parse).detail(tail(&out.stdout, 200)))
    }

    pub fn restart_kernel(
        &self,
        state: &HostPath,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<()> {
        let out = self.run_simple(
            Some(state),
            &CliCommand::RestartKernel {
                name: name.to_owned(),
            },
            120,
            cancel,
        )?;
        if !out.success() {
            return Err(classify_failure(&out));
        }
        Ok(())
    }

    pub fn reset_ssh(&self, state: &HostPath, name: &str, cancel: &CancelToken) -> Result<()> {
        let out = self.run_simple(
            Some(state),
            &CliCommand::SshReset {
                name: name.to_owned(),
            },
            60,
            cancel,
        )?;
        if out.success() {
            return Ok(());
        }
        // `google-colab-cli` 0.8.0 has no `ssh --force` flag yet; fall back to
        // the exact `sshd` child cleanup used by post-0.8.0 `_killStaleSshd`.
        if out.code == Some(2)
            && out.stderr.contains("--force")
            && (out.stderr.contains("No such option") || out.stderr.contains("unrecognized"))
        {
            self.exec(
                state,
                name,
                &[],
                "import subprocess; subprocess.run(['sh', '-c', 'pkill -P $(pgrep -o -x sshd) 2>/dev/null || true'])",
                30,
                cancel,
            )?;
            return Ok(());
        }
        // In post-0.8.0, `_killStaleSshd` runs before `_resolve_pubkey`; when no public key exists
        // yet in `~/.ssh/`, the CLI exits 2 after already killing stale sshd sessions.
        if out.code == Some(2)
            && out.stderr.contains("no SSH public key found")
            && out.stderr.contains("--force: killing stale SSH sessions")
            && !out.stderr.contains("--force: failed to kill sshd")
        {
            return Ok(());
        }
        Err(classify_failure(&out))
    }
    pub fn version(&self, cancel: &CancelToken) -> Result<String> {
        let out = self.run_simple(None, &CliCommand::Version, 60, cancel)?;
        if !out.success() {
            return Err(classify_failure(&out));
        }
        out.stdout
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("Version:")
                    .map(|v| v.trim().to_owned())
            })
            .ok_or_else(|| Error::new(ErrorKind::Parse).detail(tail(&out.stdout, 200)))
    }

    /// Releases an assignment that has no local record, by endpoint.
    ///
    /// Writes a throw-away state file holding a placeholder record for the
    /// endpoint (no token expiry, so the CLI refreshes it from the server's
    /// assignment list) and runs `colab stop` on it. Returns `true` when the
    /// CLI reported "Session terminated"; `false` means the endpoint is not
    /// assigned to the active account.
    pub fn reap_endpoint(
        &self,
        scratch: &HostPath,
        endpoint: &str,
        cancel: &CancelToken,
    ) -> Result<bool> {
        if endpoint.is_empty()
            || !endpoint
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
        {
            return Err(Error::invalid(
                "endpoint",
                format!("invalid endpoint {endpoint:?}"),
            ));
        }
        let placeholder = format!("cpg-reap-{}", crate::accounts::nonce());
        let state = scratch.join(&format!("{placeholder}.json"));
        let record = serde_json::json!({
            placeholder.clone(): {
                "name": placeholder,
                "token": "placeholder",
                "url": "http://127.0.0.1:9/",
                "endpoint": endpoint,
            }
        });
        let fs = crate::hostfs::HostFs::new(&self.host, &self.base_env);
        fs.write(&state, record.to_string().as_bytes(), cancel)?;
        let result = self.stop(&state, &placeholder, cancel);
        let _ = fs.remove(&state, cancel);
        Ok(matches!(result?, StopOutcome::Terminated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(code: i32, stdout: &str, stderr: &str) -> ScriptOutput {
        ScriptOutput {
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
            killed: false,
        }
    }

    #[test]
    fn a_file_is_downloaded_from_the_runtime() {
        let state = HostPath::new("/d/accounts/a1/sessions.json");
        let argv = build_argv(
            Some(&state),
            &CliCommand::Download {
                name: "comfyui-1a2b".into(),
                remote: "/content/ComfyUI/output/apple.png".into(),
                local: "/home/u/Downloads/apple.png".into(),
            },
        )
        .unwrap();
        assert_eq!(
            argv,
            [
                "--config",
                "/d/accounts/a1/sessions.json",
                "download",
                "-s",
                "comfyui-1a2b",
                "/content/ComfyUI/output/apple.png",
                "/home/u/Downloads/apple.png",
            ]
        );
        let bad = build_argv(
            None,
            &CliCommand::Download {
                name: "comfyui-1a2b".into(),
                remote: String::new(),
                local: "/tmp/x".into(),
            },
        );
        assert!(bad.is_err(), "a path may not be empty");
        let bad = build_argv(
            None,
            &CliCommand::Download {
                name: "../evil".into(),
                remote: "/content/x".into(),
                local: "/tmp/x".into(),
            },
        );
        assert!(bad.is_err(), "the session name is checked");
    }

    #[test]
    fn argv_puts_config_first_and_validates() {
        let state = HostPath::new("/d/accounts/a1/sessions.json");
        let argv = build_argv(
            Some(&state),
            &CliCommand::New {
                name: "llm".into(),
                accelerator: Accelerator::A100,
                high_mem: true,
            },
        )
        .unwrap();
        assert_eq!(
            argv,
            [
                "--config",
                "/d/accounts/a1/sessions.json",
                "new",
                "-s",
                "llm",
                "--gpu",
                "A100",
                "--high-mem"
            ]
        );
        let argv = build_argv(
            Some(&state),
            &CliCommand::New {
                name: "x".into(),
                accelerator: Accelerator::L4,
                high_mem: true,
            },
        )
        .unwrap();
        assert!(!argv.contains(&"--high-mem".to_owned()));
        let argv = build_argv(
            None,
            &CliCommand::Exec {
                name: "s".into(),
                env: vec![("PLUGIN_INPUT_REPO".into(), "a/b c".into())],
                timeout_secs: 60,
            },
        )
        .unwrap();
        assert_eq!(
            argv,
            [
                "exec",
                "-s",
                "s",
                "--timeout",
                "60",
                "--env",
                "PLUGIN_INPUT_REPO=a/b c"
            ]
        );
        assert!(build_argv(None, &CliCommand::Stop { name: "a b".into() }).is_err());
        assert!(
            build_argv(
                None,
                &CliCommand::Exec {
                    name: "s".into(),
                    env: vec![("1A".into(), "x".into())],
                    timeout_secs: 1
                }
            )
            .is_err()
        );
        assert_eq!(
            build_argv(Some(&state), &CliCommand::Url { name: "llm".into() }).unwrap(),
            ["--config", "/d/accounts/a1/sessions.json", "url", "-s", "llm"]
        );
        assert_eq!(
            build_argv(
                Some(&state),
                &CliCommand::RestartKernel { name: "llm".into() }
            )
            .unwrap(),
            [
                "--config",
                "/d/accounts/a1/sessions.json",
                "restart-kernel",
                "-s",
                "llm"
            ]
        );
        assert_eq!(
            build_argv(Some(&state), &CliCommand::SshReset { name: "llm".into() }).unwrap(),
            [
                "--config",
                "/d/accounts/a1/sessions.json",
                "ssh",
                "--force",
                "--proxy-mode",
                "-s",
                "llm"
            ]
        );
        assert!(build_argv(None, &CliCommand::Url { name: "../bad".into() }).is_err());
        assert!(build_argv(None, &CliCommand::RestartKernel { name: "".into() }).is_err());
        assert!(build_argv(None, &CliCommand::SshReset { name: "a b".into() }).is_err());
    }

    #[test]
    fn sessions_parser_handles_orphans_status_and_noise() {
        let text = "\
A new version of colab-cli is available
[colab] Pruned 1 stale local session(s).
[llm] gpu-a100-s-abc123 | Hardware: A100 | Shape: High-RAM | Variant: GPU
[?] m-s-xyz | Hardware: CPU | Shape: Standard | Variant: DEFAULT
[tpu1] t-1 | Hardware: V5E1 | Shape: Standard | Variant: TPU | Status: BUSY (exec(stdin))
[colab] No active sessions found on server.
[bad] has space | Hardware: CPU";
        let s = parse_sessions(text);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].name.as_deref(), Some("llm"));
        assert_eq!(s[0].accelerator, Some(Accelerator::A100));
        assert!(s[0].high_mem());
        assert_eq!(s[1].name, None);
        assert_eq!(s[1].accelerator, Some(Accelerator::None));
        assert_eq!(s[2].status.as_deref(), Some("BUSY (exec(stdin))"));
        assert!(parse_sessions("[colab] No active sessions found on server.").is_empty());
    }

    #[test]
    fn usage_parser() {
        let u = parse_usage(
            "Current balance: 87.50 compute units\nUsage rate: 11.77/hr\nActive assignments: 1\n",
        )
        .unwrap();
        assert_eq!(u.balance_cu, 87.5);
        assert_eq!(u.rate_cu_per_hour, 11.77);
        assert_eq!(u.assignments, 1);
        assert!(
            parse_usage("[colab] Warning: failed to fetch compute-unit info")
                .unwrap_err()
                .is(ErrorKind::Parse)
        );
    }

    #[test]
    fn result_marker() {
        let v = find_result("noise\n__CPG_RESULT__ {\"gpu\": \"A100\", \"vram_mib\": 81920}\n")
            .unwrap();
        assert_eq!(v["vram_mib"], 81920);
        assert!(find_result("Traceback ...").is_none());
    }

    #[test]
    fn failure_classification() {
        let k = |o: ScriptOutput| classify_failure(&o).kind;
        assert_eq!(
            k(out(
                127,
                "",
                "bash: line 1: /x/colab: No such file or directory"
            )),
            ErrorKind::NotInstalled
        );
        assert_eq!(
            k(out(
                1,
                "",
                "[colab] Backend rejected accelerator 'H100'. You may not have quota"
            )),
            ErrorKind::NotEntitled
        );
        assert_eq!(
            k(out(
                1,
                "",
                "[colab] Allocation refused (precondition failed)."
            )),
            ErrorKind::CapacityRefused
        );
        assert_eq!(
            k(out(
                255,
                "",
                "[colab] Already-active SSH session (HTTP 429): another `colab ssh` is connected"
            )),
            ErrorKind::SshBusy
        );
        assert_eq!(
            k(out(
                1,
                "[colab] Session 'x' appears to be lost (404/401). Cleaning up.",
                ""
            )),
            ErrorKind::SessionGone
        );
        assert_eq!(
            k(out(1, "[colab] Session 'x' not found.", "")),
            ErrorKind::SessionGone
        );
        assert_eq!(
            k(out(
                255,
                "",
                "[colab] SSH session limit reached (HTTP 429): 5 of 5 SSH sessions in use on this runtime."
            )),
            ErrorKind::SshBusy
        );
        assert_eq!(
            classify_failure(&out(
                255,
                "",
                "[colab] SSH session limit reached (HTTP 429): 5 of 5 SSH sessions in use on this runtime."
            ))
            .param("limit"),
            Some("5")
        );
        assert_eq!(
            classify_failure(&out(
                255,
                "",
                "[colab] Already-active SSH session (HTTP 429): retry 1 of 2"
            ))
            .param("limit"),
            None
        );
        assert_eq!(
            k(out(1, "", "RuntimeError: Connection was lost.")),
            ErrorKind::ConnectionLost
        );
        assert_eq!(
            k(out(1, "", "RuntimeError: Kernel dead before the code finished.")),
            ErrorKind::ConnectionLost
        );
        assert_eq!(
            k(out(1, "", "RuntimeError: Kernel restarting before the code finished.")),
            ErrorKind::ConnectionLost
        );
        assert_eq!(
            k(out(
                1,
                "",
                "TimeoutError: Timeout waiting for reply: no message"
            )),
            ErrorKind::ExecTimeout
        );
        assert_eq!(
            k(out(
                1,
                "",
                "TimeoutError: Timeout waiting for output: no message from the kernel for 30s. Use --timeout to allow longer silences."
            )),
            ErrorKind::ExecTimeout
        );
        assert_eq!(
            k(out(
                1,
                "[colab] Download failed: HTTPSConnectionPool(host='x', port=443): Read timed out. (read timeout=300)",
                ""
            )),
            ErrorKind::Timeout
        );
        assert_eq!(
            k(out(
                1,
                "[colab] Upload failed: HTTPSConnectionPool(host='x', port=443): Max retries exceeded (Caused by ConnectTimeoutError('connect timeout=30'))",
                ""
            )),
            ErrorKind::Timeout
        );
        assert_eq!(
            k(out(
                1,
                "[colab] Download failed: timed out waiting for user",
                ""
            )),
            ErrorKind::Io
        );
        assert_eq!(
            k(out(
                1,
                "",
                "To authorize colab-cli, visit this URL in any browser:"
            )),
            ErrorKind::AuthRequired
        );
        assert_eq!(k(out(1, "", "boom")), ErrorKind::Other);
        assert_eq!(
            classify_failure(&out(3, "", "boom")).param("exit_code"),
            Some("3")
        );
    }

    #[test]
    fn local_cli_exec_failures_are_told_apart_from_remote_errors() {
        let local = |code, stdout, stderr| is_local_cli_exec_failure(&out(code, stdout, stderr));
        assert!(is_local_cli_exec_failure(&ScriptOutput {
            code: None,
            stdout: String::new(),
            stderr: String::new(),
            killed: false,
        }));
        assert!(local(2, "", "Usage: colab exec [OPTIONS]"));
        assert!(local(2, "", "No such command 'bad'."));
        assert!(local(2, "", "No such option: --bad"));
        assert!(local(2, "", "Invalid value for '--timeout': bad"));
        assert!(local(2, "", "Missing option '-s'."));
        assert!(local(2, "", "[colab] Invalid --env value 'BAD'"));
        assert!(local(1, "[colab] Error: No input provided.", ""));
        assert!(local(1, "", "File \"/venv/colab_cli/runtime.py\", line 1"));
        assert!(local(1, "", r#"File "C:\venv\colab_cli\runtime.py", line 1"#));
        assert!(local(1, "", "No module named 'colab_cli'"));
        assert!(local(1, "", "ColabRequestError: Bad Gateway"));
        assert!(!local(2, "", "Usage: foo [OPTIONS]"));
        assert!(!local(1, "Usage: colab exec", ""));
        assert!(!local(1, "", "remote mentioned colab_cli in output"));
        assert!(!local(1, "File \"/venv/colab_cli/runtime.py\"", ""));
        assert!(!local(1, "", "Traceback:\nValueError: bad value"));
        assert!(!local(2, "", ""));
    }

    #[test]
    fn exec_failure_classification_ignores_remote_and_non_exec_errors() {
        let exec_kind = |code, stdout, stderr| {
            classify_exec_failure("/x/colab", &out(code, stdout, stderr)).map(|e| e.kind)
        };
        assert_eq!(exec_kind(127, "", "not found"), None);
        assert_eq!(exec_kind(127, "", "No such file or directory"), None);
        assert_eq!(exec_kind(126, "", "Permission denied"), None);
        assert_eq!(exec_kind(1, "", "(HTTP 429)"), None);
        assert_eq!(exec_kind(1, "", "Backend rejected accelerator"), None);
        assert_eq!(exec_kind(1, "", "Allocation refused"), None);
        assert_eq!(exec_kind(1, "[colab] Download failed: ReadTimeout", ""), None);
        assert_eq!(exec_kind(2, "", "Usage: foo"), None);
        assert_eq!(exec_kind(1, "", "Traceback:\nValueError: bad"), None);

        assert_eq!(
            exec_kind(127, "", "bash: line 1: /x/colab: No such file or directory"),
            Some(ErrorKind::NotInstalled)
        );
        assert_eq!(
            exec_kind(126, "", "/x/colab: Permission denied"),
            Some(ErrorKind::NotInstalled)
        );
        assert_eq!(
            exec_kind(1, "", "Could not read the OAuth token from the OS keyring"),
            Some(ErrorKind::KeyringUnavailable)
        );
        assert_eq!(
            exec_kind(1, "", "To authorize colab-cli, visit this URL:"),
            Some(ErrorKind::AuthRequired)
        );
        assert_eq!(
            exec_kind(1, "[colab] Session 'x' not found.", ""),
            Some(ErrorKind::SessionGone)
        );
        assert_eq!(
            exec_kind(1, "", "RuntimeError: Kernel dead before the code finished."),
            Some(ErrorKind::ConnectionLost)
        );
        assert_eq!(
            exec_kind(1, "", "TimeoutError: Timeout waiting for output: 30s"),
            Some(ErrorKind::ExecTimeout)
        );
        assert_eq!(
            exec_kind(2, "", "Usage: colab exec [OPTIONS]"),
            Some(ErrorKind::Other)
        );
        assert_eq!(
            exec_kind(2, "", "[colab] Invalid --env value 'BAD'"),
            Some(ErrorKind::Other)
        );
        assert_eq!(
            exec_kind(1, "", "File \"/venv/colab_cli/runtime.py\", line 1"),
            Some(ErrorKind::Other)
        );
    }

    /// How google-colab-cli 0.8.0 (Typer with rich) prints the read timeout that ends a
    /// `colab new` Colab never answered: the message wraps, the exception line starts it.
    const RICH_READ_TIMEOUT: &str = "\
│ ❱ 303 │   │   │   │   raise                                                  │
│   304 │   │   │   delay = backoff * (2**attempt)                             │
╰──────────────────────────────────────────────────────────────────────────────╯
ReadTimeout: HTTPSConnectionPool(host='colab.research.google.com', port=443):
Read timed out. (read timeout=120)";

    #[test]
    fn a_read_timeout_is_told_apart_from_code_that_mentions_it() {
        assert!(allocation_read_timed_out(RICH_READ_TIMEOUT));
        assert!(allocation_read_timed_out(
            "Traceback (most recent call last):\n  File \"client.py\", line 303, in assign\n    raise\nrequests.exceptions.ReadTimeout: HTTPSConnectionPool(host='colab.research.google.com', port=443): Read timed out. (read timeout=120)"
        ));
        // Source lines shown in a traceback name the exception without being it.
        assert!(!allocation_read_timed_out(
            "│   296 │   │   except (ColabRequestError, requests.exceptions.ReadTimeout) as e: │\nColabRequestError: Failed to issue request POST https://colab.research.google.com/tun/m/assign: Bad Gateway"
        ));
        assert!(!allocation_read_timed_out(
            "urllib3.exceptions.ReadTimeoutError: HTTPSConnectionPool(host='x', port=443)"
        ));
        assert_eq!(
            classify_failure(&out(
                1,
                "[colab] Creating session 'a'...",
                RICH_READ_TIMEOUT
            ))
            .kind,
            ErrorKind::Other,
            "only `colab new` turns it into AllocationTimeout"
        );
        assert!(
            classify_failure(&out(
                1,
                "[colab] Local file '/tmp/missing' not found.",
                ""
            ))
            .kind
                == ErrorKind::Io
        );
        assert_eq!(
            classify_failure(&out(1, "[colab] Upload failed: permission denied", "")).kind,
            ErrorKind::Io
        );
        assert_eq!(
            classify_failure(&out(1, "[colab] Download failed: bad path", "")).kind,
            ErrorKind::Io
        );
        assert_eq!(
            classify_failure(&out(1, "[colab] Error: File or directory not found: x", "")).kind,
            ErrorKind::Io
        );
        assert_eq!(
            parse_ls("A new version of colab-cli is available\nsubdir/\nmodel.safetensors\n[colab] Note\n"),
            vec![
                crate::backend::VmEntry {
                    name: "subdir".into(),
                    is_dir: true,
                },
                crate::backend::VmEntry {
                    name: "model.safetensors".into(),
                    is_dir: false,
                },
            ]
        );
        let state = HostPath::new("/d/accounts/a1/sessions.json");
        assert_eq!(
            build_argv(
                Some(&state),
                &CliCommand::Ls {
                    name: "s1".into(),
                    path: "content/out".into(),
                }
            )
            .unwrap(),
            ["--config", "/d/accounts/a1/sessions.json", "ls", "-s", "s1", "content/out"]
        );
        assert_eq!(
            build_argv(
                Some(&state),
                &CliCommand::Upload {
                    name: "s1".into(),
                    local: "/tmp/a.bin".into(),
                    remote: "content/a.bin".into(),
                }
            )
            .unwrap(),
            [
                "--config",
                "/d/accounts/a1/sessions.json",
                "upload",
                "-s",
                "s1",
                "/tmp/a.bin",
                "content/a.bin",
            ]
        );
        assert_eq!(
            build_argv(
                Some(&state),
                &CliCommand::DriveMount {
                    name: "s1".into(),
                    path: "/content/drive".into(),
                }
            )
            .unwrap(),
            [
                "--config",
                "/d/accounts/a1/sessions.json",
                "drivemount",
                "-s",
                "s1",
                "/content/drive",
            ]
        );
        assert!(
            build_argv(
                None,
                &CliCommand::DriveMount {
                    name: "s1".into(),
                    path: "relative/drive".into(),
                }
            )
            .is_err()
        );
    }
}
