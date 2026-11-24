//! Typed wrapper over the `colab` CLI.
//!
//! Failures are classified by [`classify_failure`] from the CLI's own messages
//! (see `google-colab-cli` sources), because exit codes are unreliable:
//! `colab stop` of a missing session exits 0, and `colab exec` exits 0 even
//! when the remote code raised.

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
    /// `colab download -s NAME REMOTE LOCAL`.
    Download {
        /// Session name.
        name: String,
        /// The path on the VM.
        remote: String,
        /// Where the file lands on this computer.
        local: String,
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

/// Extracts the JSON value printed after [`RESULT_MARKER`] (last one wins).
pub fn find_result(stdout: &str) -> Option<serde_json::Value> {
    stdout
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix(RESULT_MARKER))
        .and_then(|j| serde_json::from_str(j.trim()).ok())
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
    } else if has("Connection was lost") {
        ErrorKind::ConnectionLost
    } else if has("Timeout waiting for") {
        ErrorKind::ExecTimeout
    } else {
        ErrorKind::Other
    };
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
    /// The CLI had no local record (it prints "not found" and exits 0).
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
        // CLI-level failures (lost session, dropped connection, idle timeout)
        // exit non-zero; exceptions raised by `code` itself exit 0, so their
        // tracebacks are never misread as CLI failures.
        if !out.success() {
            return Err(classify_failure(&out));
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
        } else if out.success() && out.stdout.contains("not found") {
            Ok(StopOutcome::NotFound)
        } else {
            Err(classify_failure(&out))
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
            k(out(1, "", "RuntimeError: Connection was lost.")),
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
    }
}
