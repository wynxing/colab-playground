//! Machine-readable errors.
//!
//! Core never bakes user-facing prose into errors. Every [`Error`] carries an
//! [`ErrorKind`] that the UI maps to a localised message, a small set of named
//! parameters to interpolate into that message, and a free-form `detail` string
//! meant for logs and "show details" panes (usually the tail of a CLI's stderr).

use std::collections::BTreeMap;
use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What went wrong, independent of language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    /// The `colab` executable (or its helper interpreter) cannot be found.
    NotInstalled,
    /// The CLI has no usable credential and would start an interactive login.
    AuthRequired,
    /// Colab rejected the accelerator (HTTP 400): no entitlement or quota.
    NotEntitled,
    /// Colab refused the allocation (HTTP 412): too many sessions or a
    /// temporary capacity limit. Retrying later may help.
    CapacityRefused,
    /// Colab never answered the request for a runtime: `colab new` gave up
    /// after its own retries (each waits about two minutes for a reply).
    AllocationTimeout,
    /// A runtime already has its single allowed `colab ssh` connection (HTTP 429).
    SshBusy,
    /// The named session no longer exists (locally or on the server).
    SessionGone,
    /// The kernel connection dropped during `colab exec`.
    ConnectionLost,
    /// `colab exec` produced no output for longer than its idle timeout.
    ExecTimeout,
    /// The local end of a port forward is already taken. Param: `port`.
    PortInUse,
    /// The port is reserved (VM port 8080 belongs to Colab). Param: `port`.
    PortReserved,
    /// An argument failed validation. Params: `field`, optionally `value`.
    InvalidInput,
    /// No adopted account is active; adopt or add one first.
    NoActiveAccount,
    /// The credential in the CLI's slot is not saved as an account yet.
    ActiveAccountNotAdopted,
    /// The requested account is not the active one (CLI calls only run for the
    /// active account, to keep per-account state files consistent). Param: `account`.
    AccountNotActive,
    /// The account id is unknown. Param: `account`.
    AccountNotFound,
    /// The OS keyring backend is missing or locked.
    KeyringUnavailable,
    /// A previous add-account flow was interrupted; roll it back first.
    AddInProgress,
    /// The CLI still has a legacy plaintext token file; it would be migrated
    /// into the slot during login, so adding an account is refused.
    LegacyTokenPresent,
    /// The login was cancelled (by the user or because the CLI exited early).
    LoginCancelled,
    /// The authorisation code was rejected.
    LoginInvalidCode,
    /// The new credential could not be verified (no email, refresh failed).
    LoginVerificationFailed,
    /// The operation was cancelled through its [`crate::CancelToken`].
    Cancelled,
    /// The operation is not supported on this host (e.g. POSIX scripts on a
    /// Windows `Host::Local`).
    Unsupported,
    /// WSL is not installed or has no distribution.
    WslUnavailable,
    /// A command line would exceed the platform limit.
    CommandTooLong,
    /// The app already runs `max_concurrent_sessions` sessions on this
    /// account. Param: `limit`.
    SessionLimitReached,
    /// Another account operation holds the credential slot; retry shortly.
    Busy,
    /// Hardware never matched the requested spec within `max_attempts`.
    /// Params: `attempts`, `gpu`, `vram_mib`.
    ProvisionExhausted,
    /// A VM may have been left allocated and could not be verified as released.
    /// Param: `endpoint` (when known).
    OrphanSuspected,
    /// A plugin manifest is invalid. Params: `field`, `reason`.
    ManifestInvalid,
    /// A plugin with this id is already installed. Param: `plugin`.
    PluginExists,
    /// No plugin with this id. Param: `plugin`.
    PluginNotFound,
    /// A plugin run reported `failed`. Params: `stage`, `message`.
    PluginRunFailed,
    /// A plugin's endpoint did not answer its health probe.
    HealthCheckFailed,
    /// `git clone` failed. Param: `url`.
    GitFailed,
    /// Installing the CLI failed. Param: `exit_code`.
    InstallFailed,
    /// An HTTP request failed. Params: `status` (when there was one), `url`.
    Http,
    /// HTTP support was compiled out (`http` feature disabled).
    HttpUnavailable,
    /// A service is limiting requests (e.g. GitHub's API rate limit). Params: `status`, `url`.
    RateLimited,
    /// Output from the CLI or a remote script could not be parsed.
    Parse,
    /// Local file-system or process I/O failed.
    Io,
    /// The operation took too long.
    Timeout,
    /// The installed CLI's internals differ from what the helper expects
    /// (its Python client API changed).
    UnsupportedCliVersion,
    /// A downloaded update failed its signature or checksum check, so it was not installed.
    UpdateUnverified,
    /// The release has no download for this computer, or it is gone.
    UpdateUnavailable,
    /// An agent asked for something and was told no: a person refused it, let it lapse, or
    /// another call already had the window's attention. Nothing here is worth trying again.
    /// Params: `action`, `reason`.
    Denied,
    /// Anything else; see `detail`.
    Other,
}

impl ErrorKind {
    /// Stable snake_case identifier: the name serde writes for the kind.
    pub fn as_str(self) -> &'static str {
        serde::Serialize::serialize(&self, VariantName).unwrap_or("other")
    }

    pub fn default_next_step(self) -> NextStep {
        use ErrorKind as K;
        match self {
            K::NotInstalled => NextStep::InstallCli,
            K::AuthRequired | K::LoginVerificationFailed | K::LegacyTokenPresent => {
                NextStep::SignInAgain
            }
            K::LoginInvalidCode | K::LoginCancelled | K::AddInProgress => NextStep::TryAgain,
            K::NotEntitled | K::ProvisionExhausted | K::AllocationTimeout => {
                NextStep::ChooseOtherAccelerator
            }
            K::CapacityRefused
            | K::ConnectionLost
            | K::ExecTimeout
            | K::Busy
            | K::Timeout
            | K::RateLimited => NextStep::RetryLater,
            K::SshBusy => NextStep::CloseOtherConnection,
            K::SessionGone | K::AccountNotFound | K::PluginNotFound => NextStep::Refresh,
            K::PortInUse | K::PortReserved => NextStep::ChooseOtherPort,
            K::InvalidInput | K::ManifestInvalid => NextStep::CheckInput,
            K::NoActiveAccount | K::ActiveAccountNotAdopted => NextStep::AddAccount,
            K::AccountNotActive => NextStep::SwitchAccount,
            K::KeyringUnavailable => NextStep::UnlockKeyring,
            K::SessionLimitReached => NextStep::StopASession,
            K::OrphanSuspected => NextStep::ReviewRuntimes,
            K::WslUnavailable => NextStep::InstallWsl,
            K::UnsupportedCliVersion => NextStep::UpdateApp,
            K::InstallFailed | K::PluginRunFailed => NextStep::ViewLog,
            K::Http | K::HttpUnavailable | K::GitFailed | K::HealthCheckFailed => {
                NextStep::CheckNetwork
            }
            // For the update kinds another attempt would fetch the same files; the app offers
            // what is left itself. A denial is final by definition: a person already answered.
            K::PluginExists
            | K::Cancelled
            | K::UpdateUnverified
            | K::UpdateUnavailable
            | K::Denied => NextStep::None,
            K::Unsupported | K::CommandTooLong | K::Parse | K::Io | K::Other => {
                NextStep::ReportProblem
            }
        }
    }

    /// Parses the identifier produced by [`ErrorKind::as_str`] (used by the
    /// account helper, which reports kinds as strings). Unknown values map to
    /// [`ErrorKind::Other`].
    pub fn from_str_lossy(s: &str) -> ErrorKind {
        serde_json::from_value(serde_json::Value::String(s.to_owned())).unwrap_or(ErrorKind::Other)
    }
}

/// Serializer that answers with the name serde gives a unit variant, and refuses anything else.
struct VariantName;

type NoName = serde::de::value::Error;
type Unnamed = serde::ser::Impossible<&'static str, NoName>;

fn no_name<T>() -> Result<T, NoName> {
    Err(serde::ser::Error::custom("not a unit variant"))
}

macro_rules! refuse {
    ($($method:ident($($arg:ty),*) -> $ok:ty;)*) => {
        $(fn $method(self, $(_: $arg),*) -> Result<$ok, NoName> {
            no_name()
        })*
    };
}

impl serde::Serializer for VariantName {
    type Ok = &'static str;
    type Error = NoName;
    type SerializeSeq = Unnamed;
    type SerializeTuple = Unnamed;
    type SerializeTupleStruct = Unnamed;
    type SerializeTupleVariant = Unnamed;
    type SerializeMap = Unnamed;
    type SerializeStruct = Unnamed;
    type SerializeStructVariant = Unnamed;

    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<&'static str, NoName> {
        Ok(variant)
    }

    refuse! {
        serialize_bool(bool) -> &'static str;
        serialize_i8(i8) -> &'static str;
        serialize_i16(i16) -> &'static str;
        serialize_i32(i32) -> &'static str;
        serialize_i64(i64) -> &'static str;
        serialize_u8(u8) -> &'static str;
        serialize_u16(u16) -> &'static str;
        serialize_u32(u32) -> &'static str;
        serialize_u64(u64) -> &'static str;
        serialize_f32(f32) -> &'static str;
        serialize_f64(f64) -> &'static str;
        serialize_char(char) -> &'static str;
        serialize_str(&str) -> &'static str;
        serialize_bytes(&[u8]) -> &'static str;
        serialize_none() -> &'static str;
        serialize_unit() -> &'static str;
        serialize_unit_struct(&'static str) -> &'static str;
        serialize_seq(Option<usize>) -> Unnamed;
        serialize_tuple(usize) -> Unnamed;
        serialize_tuple_struct(&'static str, usize) -> Unnamed;
        serialize_tuple_variant(&'static str, u32, &'static str, usize) -> Unnamed;
        serialize_map(Option<usize>) -> Unnamed;
        serialize_struct(&'static str, usize) -> Unnamed;
        serialize_struct_variant(&'static str, u32, &'static str, usize) -> Unnamed;
    }

    fn serialize_some<T: ?Sized + serde::Serialize>(self, _: &T) -> Result<&'static str, NoName> {
        no_name()
    }

    fn serialize_newtype_struct<T: ?Sized + serde::Serialize>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<&'static str, NoName> {
        no_name()
    }

    fn serialize_newtype_variant<T: ?Sized + serde::Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<&'static str, NoName> {
        no_name()
    }
}

/// What the user can do about an error (the UI turns it into a button or hint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NextStep {
    /// Nothing to suggest (e.g. the user cancelled).
    None,
    /// Try the same thing again now.
    TryAgain,
    /// Wait and retry (capacity, network hiccup, busy).
    RetryLater,
    /// Switch to the account that owns the resource.
    SwitchAccount,
    /// Add or adopt an account.
    AddAccount,
    SignInAgain,
    /// Unlock or configure the OS keyring.
    UnlockKeyring,
    ChooseOtherAccelerator,
    /// Stop a running session first.
    StopASession,
    ChooseOtherPort,
    /// Close the other ssh connection to this runtime.
    CloseOtherConnection,
    InstallCli,
    InstallWsl,
    /// Update Colab Playground (the CLI changed underneath it).
    UpdateApp,
    CheckInput,
    CheckNetwork,
    /// Refresh the view; the thing is gone.
    Refresh,
    /// Review the runtime list (a VM may still be billing).
    ReviewRuntimes,
    ViewLog,
    /// Unexpected; report it.
    ReportProblem,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an agent's call was not carried out.
///
/// The four are different facts for the person looking at the window, and the same one for the
/// agent: the answer is final and the call is not repeated. Naming them apart is what lets the
/// window say "you refused it" where it would otherwise have to say "it failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Denial {
    /// The user said no.
    Refused,
    /// The dialog was closed without an answer.
    Dismissed,
    /// Nobody answered before the wait ran out.
    TimedOut,
    /// Another call was already waiting for the window's attention: the window asks about one
    /// thing at a time, and this is the second one.
    Busy,
}

impl Denial {
    /// The stable identifier the error carries as its `reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            Denial::Refused => "refused",
            Denial::Dismissed => "dismissed",
            Denial::TimedOut => "timed_out",
            Denial::Busy => "busy",
        }
    }
}

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error with a machine-readable kind, named parameters and a log detail.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub struct Error {
    pub kind: ErrorKind,
    /// Named values for the localised message (e.g. `port` → `"8080"`).
    pub params: BTreeMap<String, String>,
    /// Untranslated technical detail for logs; may be empty. Never contains secrets.
    pub detail: String,
    /// Suggested next step (defaults from the kind; may be overridden).
    pub next_step: NextStep,
}

impl Error {
    pub fn new(kind: ErrorKind) -> Self {
        Error {
            kind,
            params: BTreeMap::new(),
            detail: String::new(),
            next_step: kind.default_next_step(),
        }
    }

    pub fn with(mut self, key: &str, value: impl ToString) -> Self {
        self.params.insert(key.to_owned(), value.to_string());
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn next(mut self, step: NextStep) -> Self {
        self.next_step = step;
        self
    }

    /// Changes the kind and resets the next step to that kind's default.
    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self.next_step = kind.default_next_step();
        self
    }

    /// Shorthand for [`ErrorKind::InvalidInput`] with a `field` parameter.
    pub fn invalid(field: &str, detail: impl Into<String>) -> Self {
        Error::new(ErrorKind::InvalidInput)
            .with("field", field)
            .detail(detail)
    }

    pub fn cancelled() -> Self {
        Error::new(ErrorKind::Cancelled)
    }

    /// An agent's call the app will not carry out.
    ///
    /// It carries the tool's name, so a client with more than one call in flight can tell which
    /// was answered, and the reason, which the window says in its own words.
    pub fn denied(action: &str, reason: Denial) -> Self {
        Error::new(ErrorKind::Denied)
            .with("action", action)
            .with("reason", reason.as_str())
            .detail(format!("{action} was {reason}"))
    }

    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    pub fn is(&self, kind: ErrorKind) -> bool {
        self.kind == kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)?;
        if !self.params.is_empty() {
            let parts: Vec<String> = self
                .params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            write!(f, " ({})", parts.join(", "))?;
        }
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        Ok(())
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Error::new(kind)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(ErrorKind::Io).detail(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::new(ErrorKind::Parse).detail(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips_through_strings() {
        for kind in [
            ErrorKind::NotEntitled,
            ErrorKind::SshBusy,
            ErrorKind::LoginInvalidCode,
            ErrorKind::AllocationTimeout,
        ] {
            assert_eq!(ErrorKind::from_str_lossy(kind.as_str()), kind);
        }
        assert_eq!(ErrorKind::from_str_lossy("nope"), ErrorKind::Other);
        assert_eq!(
            ErrorKind::ActiveAccountNotAdopted.as_str(),
            "active_account_not_adopted"
        );
        assert_eq!(ErrorKind::Io.as_str(), "io");
        assert!(serde::Serialize::serialize("io", VariantName).is_err());
    }

    #[test]
    fn errors_carry_a_next_step() {
        assert_eq!(
            Error::new(ErrorKind::NotEntitled).next_step,
            NextStep::ChooseOtherAccelerator
        );
        assert_eq!(
            Error::new(ErrorKind::AccountNotActive).next_step,
            NextStep::SwitchAccount
        );
        assert_eq!(
            Error::new(ErrorKind::NotInstalled).next_step,
            NextStep::InstallCli
        );
        let e = Error::new(ErrorKind::Other).next(NextStep::RetryLater);
        assert_eq!(e.next_step, NextStep::RetryLater);
        assert_eq!(
            e.with_kind(ErrorKind::AuthRequired).next_step,
            NextStep::SignInAgain
        );
        let json = serde_json::to_value(Error::new(ErrorKind::SshBusy)).unwrap();
        assert_eq!(json["next_step"], "close_other_connection");
        assert_eq!(json["kind"], "ssh_busy");
        for kind in [
            ErrorKind::UnsupportedCliVersion,
            ErrorKind::SessionLimitReached,
            ErrorKind::Busy,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
        }
    }

    #[test]
    fn display_includes_params_and_detail() {
        let e = Error::new(ErrorKind::PortInUse)
            .with("port", 8000)
            .detail("bind failed");
        assert_eq!(e.to_string(), "port_in_use (port=8000): bind failed");
        assert_eq!(e.param("port"), Some("8000"));
    }

    #[test]
    fn a_denial_says_which_call_and_why_and_offers_no_next_step() {
        let denied = Error::denied("session_start", Denial::TimedOut);
        assert_eq!(denied.kind, ErrorKind::Denied);
        assert_eq!(denied.kind.as_str(), "denied");
        assert_eq!(denied.param("action"), Some("session_start"));
        assert_eq!(denied.param("reason"), Some("timed_out"));
        assert_eq!(
            denied.next_step,
            NextStep::None,
            "the answer is final: asking again is wrong"
        );
        // The reason travels as a stable name, not as prose.
        let json = serde_json::to_value(&denied).unwrap();
        assert_eq!(json["params"]["reason"], "timed_out");
        for (denial, name) in [
            (Denial::Refused, "refused"),
            (Denial::Dismissed, "dismissed"),
            (Denial::TimedOut, "timed_out"),
            (Denial::Busy, "busy"),
        ] {
            assert_eq!(denial.as_str(), name);
            assert_eq!(serde_json::to_value(denial).unwrap(), name);
        }
    }
}
