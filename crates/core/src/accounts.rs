//! Accounts: non-secret metadata plus the keyring helper protocol.
//!
//! The CLI keeps exactly one OAuth token in the OS keyring
//! (`google-colab-cli` / `oauth_token`). Switching accounts means copying
//! another token into that slot. That copying is done by
//! `assets/account_helper.py`, which runs with the CLI's own interpreter (so it
//! has `keyring` and `google-auth`) and keeps saved tokens in the same keyring
//! (`colab-playground` / `account:<id>`). **Tokens never leave that Python
//! process**: this module only ever sees the helper's non-secret JSON.
//!
//! Adding an account is `begin-add` (back up the active token, verified by
//! read-back, then clear the slot) → `colab whoami` login → `finish-add`
//! (verify and save the new token) or `rollback-add` (restore the backup).
//! A pending backup is never overwritten, and `status` reports it so an
//! interrupted add can be rolled back on the next start.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::cli::find_result;
use crate::clock::{CancelToken, serde_secs};
use crate::error::{Error, ErrorKind, Result};
use crate::host::{Host, ScriptRequest, tail};
use crate::usage::{Usage, UsageSnapshot};

/// Source of the account helper, embedded at build time.
pub const ACCOUNT_HELPER_PY: &str = include_str!("../assets/account_helper.py");

/// A short opaque account id: `[a-z0-9][a-z0-9-]{0,31}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AccountId(String);

impl AccountId {
    pub fn new(s: impl Into<String>) -> Result<AccountId> {
        let s = s.into();
        let ok = !s.is_empty()
            && s.len() <= 32
            && s.chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if ok {
            Ok(AccountId(s))
        } else {
            Err(Error::invalid(
                "account",
                format!("invalid account id {s:?}"),
            ))
        }
    }

    /// A fresh random-looking id (`a` + 10 hex digits).
    pub fn generate() -> AccountId {
        AccountId(format!("a{}", &nonce()[..10]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AccountId {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        AccountId::new(s)
    }
}

impl From<AccountId> for String {
    fn from(id: AccountId) -> String {
        id.0
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A process-unique hex string (time, pid and a counter mixed together).
/// Not cryptographic; used for scratch names and account ids.
pub fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut x = t ^ (u64::from(std::process::id()) << 32) ^ c.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // splitmix64 finaliser for well-spread digits
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    format!("{x:016x}")
}

/// Non-secret metadata of a saved account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// Opaque id (keyring key and state-file directory).
    pub id: AccountId,
    /// Verified Google account email.
    pub email: Option<String>,
    #[serde(with = "serde_secs")]
    pub created_at: SystemTime,
    #[serde(with = "serde_secs")]
    pub updated_at: SystemTime,
    #[serde(default)]
    pub usage: Option<UsageSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountsView {
    pub accounts: Vec<Account>,
    /// The saved account whose token is in the CLI slot.
    pub active: Option<AccountId>,
    /// The CLI slot holds a credential.
    pub has_token: bool,
    /// The slot's credential is not saved as an account yet (offer "adopt").
    pub unadopted: bool,
    /// Email of the slot's credential when known (also for unadopted ones).
    pub active_email: Option<String>,
    /// An interrupted add-account flow was found and rolled back.
    pub recovered_pending_add: bool,
}

impl AccountsView {
    pub fn active_account(&self) -> Option<&Account> {
        let id = self.active.as_ref()?;
        self.accounts.iter().find(|a| &a.id == id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AccountsFile {
    version: u32,
    #[serde(default)]
    accounts: Vec<Account>,
}

/// `accounts.json` on the app side (no secrets).
#[derive(Debug)]
pub struct AccountStore {
    path: Option<PathBuf>,
    accounts: Vec<Account>,
}

impl AccountStore {
    pub fn load(path: &Path) -> Result<AccountStore> {
        let accounts = match std::fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Vec::new(),
            Ok(s) => {
                let f: AccountsFile = serde_json::from_str(&s)?;
                if f.version != 1 {
                    return Err(Error::new(ErrorKind::Parse)
                        .detail(format!("unsupported accounts.json version {}", f.version)));
                }
                f.accounts
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(AccountStore {
            path: Some(path.to_owned()),
            accounts,
        })
    }

    pub fn in_memory() -> AccountStore {
        AccountStore {
            path: None,
            accounts: Vec::new(),
        }
    }

    pub fn all(&self) -> &[Account] {
        &self.accounts
    }

    pub fn ids(&self) -> Vec<AccountId> {
        self.accounts.iter().map(|a| a.id.clone()).collect()
    }

    pub fn get(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.iter().find(|a| &a.id == id)
    }

    /// Finds an account by (case-insensitive) email.
    pub fn by_email(&self, email: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| {
            a.email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(email))
        })
    }

    /// Inserts or replaces an account and saves.
    pub fn upsert(&mut self, account: Account) -> Result<()> {
        match self.accounts.iter_mut().find(|a| a.id == account.id) {
            Some(a) => *a = account,
            None => self.accounts.push(account),
        }
        self.save()
    }

    /// Removes an account and saves.
    pub fn remove(&mut self, id: &AccountId) -> Result<()> {
        self.accounts.retain(|a| &a.id != id);
        self.save()
    }

    pub fn set_usage(&mut self, id: &AccountId, sample: UsageSnapshot) -> Result<()> {
        if let Some(a) = self.accounts.iter_mut().find(|a| &a.id == id) {
            a.usage = Some(sample);
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if let Some(path) = &self.path {
            let f = AccountsFile {
                version: 1,
                accounts: self.accounts.clone(),
            };
            crate::paths::write_atomic(path, serde_json::to_string_pretty(&f)?.as_bytes())?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperStatus {
    /// The slot holds a credential.
    pub has_token: bool,
    /// Its verified email (may be absent offline).
    #[serde(default)]
    pub email: Option<String>,
    /// The saved account it matches (same refresh token).
    #[serde(default)]
    pub matches: Option<AccountId>,
    /// A `begin-add` backup is pending.
    #[serde(default)]
    pub pending_add: bool,
    /// The CLI's legacy plaintext token file exists.
    #[serde(default)]
    pub legacy_token: bool,
}

#[derive(Debug, Clone)]
pub struct Helper {
    host: Host,
    python: String,
    base_env: Vec<(String, String)>,
    source: String,
}

/// Starts the helper: a fixed loader reads `<len>\n<source>` from stdin and
/// executes it, leaving the rest of stdin for values such as secrets (which
/// therefore never appear in argv or the environment).
const RUN_HELPER: &str = r#"exec "$CPG_PY" -c 'import sys
n = int(sys.stdin.buffer.readline())
src = sys.stdin.buffer.read(n)
exec(compile(src, "account_helper.py", "exec"), {"__name__": "__main__"})' "$@""#;

impl Helper {
    /// A helper using `python` (the CLI's interpreter) on `host`.
    pub fn new(host: Host, python: impl Into<String>, base_env: Vec<(String, String)>) -> Self {
        Helper {
            host,
            python: python.into(),
            base_env,
            source: ACCOUNT_HELPER_PY.to_owned(),
        }
    }

    /// The request for `op args…` (pure; the helper source goes on stdin).
    pub fn request(&self, op: &str, args: &[String]) -> ScriptRequest {
        self.request_with_input(op, args, &[])
    }

    /// Like [`Helper::request`] with `input` after the source on stdin.
    pub fn request_with_input(&self, op: &str, args: &[String], input: &[u8]) -> ScriptRequest {
        let mut stdin = format!("{}\n", self.source.len()).into_bytes();
        stdin.extend_from_slice(self.source.as_bytes());
        stdin.extend_from_slice(input);
        let mut req = ScriptRequest::new(RUN_HELPER)
            .arg(op)
            .args(args.iter().cloned())
            .stdin_bytes(stdin)
            .timeout(Duration::from_secs(120));
        req.env = self.base_env.clone();
        req.env.push(("CPG_PY".into(), self.python.clone()));
        req
    }

    /// Runs one operation and returns its JSON object (errors mapped to [`Error`]).
    pub fn call(
        &self,
        op: &str,
        args: &[String],
        cancel: &CancelToken,
    ) -> Result<serde_json::Value> {
        self.call_with_input(op, args, &[], cancel)
    }

    /// Runs one operation with extra stdin input. Stdout (which may hold a
    /// secret for `secret-get`) is never copied into errors.
    pub fn call_with_input(
        &self,
        op: &str,
        args: &[String],
        input: &[u8],
        cancel: &CancelToken,
    ) -> Result<serde_json::Value> {
        let out = self
            .host
            .run_quiet(self.request_with_input(op, args, input), cancel)?;
        let Some(v) = find_result(&out.stdout) else {
            let kind = if matches!(out.code, Some(126) | Some(127)) {
                ErrorKind::NotInstalled
            } else {
                ErrorKind::Other
            };
            return Err(Error::new(kind)
                .detail(format!("account helper failed: {}", tail(&out.stderr, 600))));
        };
        if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
            return Ok(v);
        }
        Err(helper_error(&v))
    }

    fn ids(ids: &[AccountId]) -> Vec<String> {
        ids.iter().map(|i| i.as_str().to_owned()).collect()
    }

    /// `status <known ids…>`.
    pub fn status(&self, known: &[AccountId], cancel: &CancelToken) -> Result<HelperStatus> {
        let v = self.call("status", &Self::ids(known), cancel)?;
        Ok(serde_json::from_value(v)?)
    }

    /// `adopt <id>`: saves the slot's credential as account `id`; returns its email.
    pub fn adopt(&self, id: &AccountId, cancel: &CancelToken) -> Result<Option<String>> {
        let v = self.call("adopt", &[id.as_str().to_owned()], cancel)?;
        Ok(v.get("email").and_then(|e| e.as_str()).map(str::to_owned))
    }

    /// `use <id> <known ids…>`: puts account `id` into the slot (saving the
    /// current, possibly refreshed, credential back to its account first).
    pub fn use_account(
        &self,
        id: &AccountId,
        known: &[AccountId],
        cancel: &CancelToken,
    ) -> Result<Option<String>> {
        let mut args = vec![id.as_str().to_owned()];
        args.extend(Self::ids(known));
        let v = self.call("use", &args, cancel)?;
        Ok(v.get("email").and_then(|e| e.as_str()).map(str::to_owned))
    }

    /// `remove <id> <known ids…>`: deletes a saved credential (never the active one).
    pub fn remove(&self, id: &AccountId, known: &[AccountId], cancel: &CancelToken) -> Result<()> {
        let mut args = vec![id.as_str().to_owned()];
        args.extend(Self::ids(known));
        self.call("remove", &args, cancel).map(|_| ())
    }

    /// `usage <id>…`: compute-unit usage of any saved accounts, without
    /// switching, in one helper invocation. Each account succeeds or fails
    /// on its own.
    pub fn usage_many(
        &self,
        ids: &[AccountId],
        cancel: &CancelToken,
    ) -> Result<Vec<(AccountId, Result<Usage>)>> {
        let v = self.call("usage", &Self::ids(ids), cancel)?;
        let mut out = Vec::new();
        for (id, item) in per_account(&v) {
            let id = AccountId::new(id)?;
            let res = if item.get("ok").and_then(|o| o.as_bool()) == Some(true) {
                let num = |k: &str| item.get(k).and_then(|x| x.as_f64());
                match (num("balance"), num("rate"), num("assignments")) {
                    (Some(b), Some(r), Some(a)) if b.is_finite() && r.is_finite() => Ok(Usage {
                        balance_cu: b,
                        rate_cu_per_hour: r,
                        assignments: a.max(0.0) as u32,
                    }),
                    _ => Err(Error::new(ErrorKind::Parse).detail("helper usage output")),
                }
            } else {
                Err(helper_error(item))
            };
            out.push((id, res));
        }
        Ok(out)
    }

    /// `secret-set <name>`: stores an app secret (value via stdin).
    pub fn secret_set(&self, name: &str, value: &str, cancel: &CancelToken) -> Result<()> {
        validate_secret_name(name)?;
        self.call_with_input("secret-set", &[name.to_owned()], value.as_bytes(), cancel)
            .map(|_| ())
    }

    /// `secret-get <name>`: the secret's value. Never log it.
    pub fn secret_get(&self, name: &str, cancel: &CancelToken) -> Result<Option<String>> {
        validate_secret_name(name)?;
        let v = self.call("secret-get", &[name.to_owned()], cancel)?;
        Ok(v.get("value").and_then(|x| x.as_str()).map(str::to_owned))
    }

    /// `secret-has <name>`.
    pub fn secret_has(&self, name: &str, cancel: &CancelToken) -> Result<bool> {
        validate_secret_name(name)?;
        let v = self.call("secret-has", &[name.to_owned()], cancel)?;
        Ok(v.get("present").and_then(|x| x.as_bool()).unwrap_or(false))
    }

    /// `secret-delete <name>`.
    pub fn secret_delete(&self, name: &str, cancel: &CancelToken) -> Result<()> {
        validate_secret_name(name)?;
        self.call("secret-delete", &[name.to_owned()], cancel)
            .map(|_| ())
    }

    /// `history-redact <session>`: replaces `secrets` (sent as JSON on stdin,
    /// never in argv or env) with `***` in the CLI's history file for that
    /// session (`~/.config/colab-cli/history/<session>.jsonl` on the host),
    /// rewriting it atomically with mode 0600. Values shorter than 4
    /// characters are left alone. A missing file is not an error.
    pub fn history_redact(
        &self,
        session: &str,
        secrets: &[String],
        cancel: &CancelToken,
    ) -> Result<HistoryRedaction> {
        crate::sessions::validate_session_name(session)?;
        let input = serde_json::to_vec(secrets)?;
        let v = self.call_with_input("history-redact", &[session.to_owned()], &input, cancel)?;
        Ok(HistoryRedaction {
            present: v.get("present").and_then(|p| p.as_bool()).unwrap_or(false),
            redacted_lines: v
                .get("redacted_lines")
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32,
        })
    }

    /// `assignments <id>…`: runtimes of saved accounts, listed through the
    /// CLI's own API client without switching. Each account succeeds or
    /// fails on its own (`UnsupportedCliVersion` if the CLI internals changed).
    pub fn assignments(
        &self,
        ids: &[AccountId],
        cancel: &CancelToken,
    ) -> Result<Vec<(AccountId, Result<Vec<RemoteAssignment>>)>> {
        let v = self.call("assignments", &Self::ids(ids), cancel)?;
        let mut out = Vec::new();
        for (id, item) in per_account(&v) {
            let id = AccountId::new(id)?;
            let res = if item.get("ok").and_then(|o| o.as_bool()) == Some(true) {
                let list = item
                    .get("assignments")
                    .and_then(|a| a.as_array())
                    .cloned()
                    .unwrap_or_default();
                list.iter()
                    .map(RemoteAssignment::from_json)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| {
                        Error::new(ErrorKind::UnsupportedCliVersion).detail("assignment fields")
                    })
            } else {
                Err(helper_error(item))
            };
            out.push((id, res));
        }
        Ok(out)
    }

    /// `unassign <id> <endpoint>`: releases a runtime of any saved account.
    pub fn unassign(&self, id: &AccountId, endpoint: &str, cancel: &CancelToken) -> Result<()> {
        self.call(
            "unassign",
            &[id.as_str().to_owned(), endpoint.to_owned()],
            cancel,
        )
        .map(|_| ())
    }

    /// `begin-add <known ids…>`: backs up and clears the slot. Returns whether
    /// a credential was backed up.
    pub fn begin_add(&self, known: &[AccountId], cancel: &CancelToken) -> Result<bool> {
        let v = self.call("begin-add", &Self::ids(known), cancel)?;
        Ok(v.get("had_token")
            .and_then(|b| b.as_bool())
            .unwrap_or(false))
    }

    /// `finish-add <id>`: verifies the new slot credential, saves it as `id`,
    /// drops the backup. Returns the verified email.
    pub fn finish_add(&self, id: &AccountId, cancel: &CancelToken) -> Result<String> {
        let v = self.call("finish-add", &[id.as_str().to_owned()], cancel)?;
        v.get("email")
            .and_then(|e| e.as_str())
            .map(str::to_owned)
            .ok_or_else(|| Error::new(ErrorKind::LoginVerificationFailed).detail("no email"))
    }

    /// `rollback-add`: restores the backup (or clears the slot if there was
    /// none). Returns whether a pending add was rolled back.
    pub fn rollback_add(&self, cancel: &CancelToken) -> Result<bool> {
        let v = self.call("rollback-add", &[], cancel)?;
        Ok(v.get("restored").and_then(|b| b.as_bool()).unwrap_or(false))
    }

    /// `list <ids…>`: which ids have a stored credential.
    pub fn stored(
        &self,
        ids: &[AccountId],
        cancel: &CancelToken,
    ) -> Result<Vec<(AccountId, bool)>> {
        let v = self.call("list", &Self::ids(ids), cancel)?;
        let mut out = Vec::new();
        for (id, item) in per_account(&v) {
            if let Some(stored) = item.get("stored").and_then(|s| s.as_bool()) {
                out.push((AccountId::new(id)?, stored));
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRedaction {
    /// The history file existed.
    pub present: bool,
    /// Lines that contained a secret and were rewritten.
    pub redacted_lines: u32,
}

/// A runtime as listed by the CLI's API client (no secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAssignment {
    pub endpoint: String,
    /// Parsed accelerator (`None` if the CLI reported something unknown).
    pub accelerator: Option<crate::sessions::Accelerator>,
    /// Raw accelerator name (`NONE`, `A100`, …).
    pub accelerator_label: String,
    /// `DEFAULT` / `GPU` / `TPU`.
    pub variant: String,
    pub high_mem: bool,
}

impl RemoteAssignment {
    fn from_json(v: &serde_json::Value) -> Option<RemoteAssignment> {
        let endpoint = v.get("endpoint")?.as_str()?.to_owned();
        let label = v.get("accelerator")?.as_str()?.to_owned();
        Some(RemoteAssignment {
            endpoint,
            accelerator: crate::sessions::Accelerator::from_label(if label == "NONE" {
                "CPU"
            } else {
                &label
            }),
            accelerator_label: label,
            variant: v
                .get("variant")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            high_mem: v
                .get("shape")
                .and_then(|x| x.as_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("HIGH_RAM")),
        })
    }
}

fn helper_error(reply: &serde_json::Value) -> Error {
    let text = |key: &str| reply.get("error")?.get(key)?.as_str();
    let error = Error::new(ErrorKind::from_str_lossy(text("kind").unwrap_or("other")))
        .detail(text("detail").unwrap_or_default());
    match text("field") {
        Some(field) => error.with("field", field),
        None => error,
    }
}

fn per_account(reply: &serde_json::Value) -> impl Iterator<Item = (&str, &serde_json::Value)> {
    let items = reply.get("accounts").and_then(|a| a.as_array());
    items
        .into_iter()
        .flatten()
        .filter_map(|item| Some((item.get("id")?.as_str()?, item)))
}

/// App secret names: `[a-z][a-z0-9_]*`, at most 96 chars (e.g. `hf_token`,
/// `p05_train_civitai_token`).
pub fn validate_secret_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 96
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::invalid("secret", "invalid secret name"))
    }
}

pub fn classify_login_failure(stdout: &str, stderr: &str) -> Error {
    let all = format!("{stdout}\n{stderr}");
    let kind = if all.contains("invalid_grant")
        || all.contains("InvalidGrantError")
        || all.contains("Malformed auth code")
    {
        ErrorKind::LoginInvalidCode
    } else if all.contains("Could not save the OAuth token to the OS keyring")
        || all.contains("Could not read the OAuth token from the OS keyring")
    {
        ErrorKind::KeyringUnavailable
    } else {
        // EOF on stdin, Ctrl-C, or the process was killed.
        ErrorKind::LoginCancelled
    };
    Error::new(kind).detail(tail(stderr, 600))
}

/// Extracts the authorisation URL from a stderr line of `colab whoami`.
pub fn auth_url_from_line(line: &str) -> Option<String> {
    let t = line.trim();
    (t.starts_with("https://") && !t.contains(' ')).then(|| t.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_ids() {
        assert!(AccountId::new("a1b2-c3").is_ok());
        for bad in ["", "A1", "-a", "a_b", "a/b", &"a".repeat(33)] {
            assert!(AccountId::new(bad).is_err(), "{bad}");
        }
        let g = AccountId::generate();
        assert!(AccountId::new(g.as_str()).is_ok());
        assert_ne!(AccountId::generate(), AccountId::generate());
        assert!(serde_json::from_str::<AccountId>("\"BAD\"").is_err());
    }

    #[test]
    fn store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.json");
        let mut s = AccountStore::load(&path).unwrap();
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let id = AccountId::new("a1").unwrap();
        s.upsert(Account {
            id: id.clone(),
            email: Some("A@x.com".into()),
            created_at: t,
            updated_at: t,
            usage: None,
        })
        .unwrap();
        let s = AccountStore::load(&path).unwrap();
        assert_eq!(s.by_email("a@X.com").unwrap().id, id);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("token"));
    }

    #[test]
    fn helper_request_feeds_source_on_stdin() {
        let h = Helper::new(Host::Wsl { distro: None }, "/venv/bin/python", vec![]);
        let req = h.request("use", &["a1".into()]);
        assert_eq!(req.args, ["use", "a1"]);
        let crate::host::StdinMode::Bytes(stdin) = &req.stdin else {
            panic!("stdin")
        };
        let (len, rest) = std::str::from_utf8(stdin)
            .unwrap()
            .split_once('\n')
            .unwrap();
        assert_eq!(len.parse::<usize>().unwrap(), ACCOUNT_HELPER_PY.len());
        assert!(rest.starts_with("#!"));
        let req = h.request_with_input("secret-set", &["hf_token".into()], b"hf_secret");
        let crate::host::StdinMode::Bytes(stdin) = &req.stdin else {
            panic!("stdin")
        };
        assert!(stdin.ends_with(b"hf_secret"));
        assert!(!req.args.iter().any(|a| a.contains("hf_secret")));
        assert!(!req.env.iter().any(|(_, v)| v.contains("hf_secret")));
        assert!(validate_secret_name("hf_token").is_ok());
        assert!(validate_secret_name("HF").is_err());
        assert!(
            req.env
                .contains(&("CPG_PY".into(), "/venv/bin/python".into()))
        );
        // Must fit through wsl.exe: the helper source is not on the command line.
        assert!(h.host.invocation(&req, true).is_ok());
    }

    #[test]
    fn history_redaction_sends_secrets_on_stdin_only() {
        let h = Helper::new(Host::Wsl { distro: None }, "/venv/bin/python", vec![]);
        let secrets = vec!["hf_TOPSECRET".to_owned(), "sk-'quoted\"".to_owned()];
        let input = serde_json::to_vec(&secrets).unwrap();
        let req = h.request_with_input("history-redact", &["llama-cpp-1a2b".into()], &input);
        assert_eq!(req.args, ["history-redact", "llama-cpp-1a2b"]);
        let crate::host::StdinMode::Bytes(stdin) = &req.stdin else {
            panic!("stdin")
        };
        assert!(stdin.ends_with(&input));
        assert!(
            !req.args
                .iter()
                .chain(req.env.iter().map(|(_, v)| v))
                .any(|a| a.contains("TOPSECRET"))
        );
        // Session names are validated before anything runs.
        let err = h
            .history_redact("../etc", &secrets, &CancelToken::new())
            .unwrap_err();
        assert!(err.is(ErrorKind::InvalidInput));
    }

    #[test]
    fn login_failure_and_url() {
        assert!(
            classify_login_failure(
                "",
                "oauthlib.oauth2.rfc6749.errors.InvalidGrantError: (invalid_grant) Bad Request"
            )
            .is(ErrorKind::LoginInvalidCode)
        );
        assert!(classify_login_failure("", "EOFError").is(ErrorKind::LoginCancelled));
        assert_eq!(
            auth_url_from_line("  https://accounts.google.com/o/oauth2/auth?x=1  ").as_deref(),
            Some("https://accounts.google.com/o/oauth2/auth?x=1")
        );
        assert_eq!(
            auth_url_from_line("To authorize colab-cli, visit this URL"),
            None
        );
    }
}
