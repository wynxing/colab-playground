//! Colab Public API (v1beta) and the older tunnel control plane.
//!
//! Checked against `google-colab-cli` (`client.py`, the public-API migration note)
//! and `colab-vscode` (`colab-api.json`). CCU is only `GET /tun/m/ccu-info`.
//! The five `ErrorInfo.reason` strings are mapped when present; the public CLI
//! does not emit them today. Nothing in this module prints tokens or proxy URLs.

use serde_json::{Map, Value};

use crate::error::{Error, ErrorKind};
use crate::sessions::Accelerator;

pub const CLIENT_AGENT: &str = "colab-playground";
pub const REDIRECT_URI: &str = "https://sdk.cloud.google.com/applicationdefaultauthcode.html";
pub const OAUTH_SCOPES: &str = "openid https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/colaboratory https://www.googleapis.com/auth/drive.file";

/// Hosts the direct backend talks to. Tests point every field at one local server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub public_api: String,
    pub tunnel: String,
    pub token: String,
    pub userinfo: String,
    pub auth: String,
}

impl Endpoints {
    pub fn google() -> Self {
        Endpoints {
            public_api: "https://colaboratory.googleapis.com".into(),
            tunnel: "https://colab.research.google.com".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            userinfo: "https://www.googleapis.com/oauth2/v2/userinfo".into(),
            auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
        }
    }

    /// Every call goes to `origin` (no trailing slash), which is how the mock server is wired.
    pub fn at(origin: &str) -> Self {
        let origin = origin.trim_end_matches('/').to_owned();
        Endpoints {
            public_api: origin.clone(),
            tunnel: origin.clone(),
            token: format!("{origin}/token"),
            userinfo: format!("{origin}/userinfo"),
            auth: format!("{origin}/auth"),
        }
    }
}

/// Proxy coordinates for one runtime. Held in memory only; never written to the ledger.
#[derive(Clone)]
pub struct ConnectionInfo {
    pub url: String,
    pub token: String,
    pub expire_time: Option<String>,
    pub resource: String,
    pub endpoint: String,
}

impl std::fmt::Debug for ConnectionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionInfo")
            .field("resource", &self.resource)
            .field("endpoint", &self.endpoint)
            .field("expire_time", &self.expire_time)
            .field("url", &"<redacted>")
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A runtime as the direct client understands it, with no proxy secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedRuntime {
    pub resource: String,
    pub endpoint: String,
    pub accelerator: Accelerator,
    pub high_mem: bool,
    pub variant: String,
}

/// Failure of one Colab HTTP call, after XSSI stripping and `ErrorInfo` parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiFailure {
    pub kind: ErrorKind,
    pub retryable: bool,
    pub auth_denied: bool,
    pub already_exists: bool,
    pub status: u16,
    pub reason: Option<String>,
    pub limit: Option<u32>,
    pub detail: String,
}

impl ApiFailure {
    pub fn into_error(self) -> Error {
        let mut error = Error::new(self.kind).detail(self.detail);
        if self.status != 0 {
            error = error.with("status", self.status);
        }
        if let Some(reason) = self.reason {
            error = error.with("reason", reason);
        }
        if let Some(limit) = self.limit {
            error = error.with("limit", limit);
        }
        error
    }
}

/// Drops the Colab XSSI guard prefix `)]}'`.
pub fn strip_xssi(body: &str) -> &str {
    let body = body.trim_start_matches('\u{feff}');
    body.strip_prefix(")]}'\n")
        .or_else(|| body.strip_prefix(")]}'\r\n"))
        .or_else(|| body.strip_prefix(")]}'"))
        .unwrap_or(body)
        .trim_start()
}

/// Path and nothing else. Query strings can carry the proxy token, so they never go to logs.
pub fn path_only(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let path = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
    path.split('?').next().unwrap_or(path).to_owned()
}

pub fn join(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

/// Maps an HTTP status and a JSON body (public API operation, or a tunnel error) to a kind.
pub fn classify(status: u16, body: &[u8]) -> ApiFailure {
    let text = String::from_utf8_lossy(body);
    let value: Value = serde_json::from_str(strip_xssi(&text)).unwrap_or(Value::Null);
    let reason = find_reason(&value);
    let code = find_code(&value).unwrap_or(0);
    let limit = find_limit(&value);
    let detail = reason
        .clone()
        .unwrap_or_else(|| find_message(&value).unwrap_or_else(|| format!("http {status}")));
    let mut failure = ApiFailure {
        kind: ErrorKind::Http,
        retryable: false,
        auth_denied: false,
        already_exists: false,
        status,
        reason: reason.clone(),
        limit,
        detail,
    };
    if let Some(reason) = reason.as_deref() {
        match reason {
            "RESOURCE_EXHAUSTED" => {
                failure.kind = ErrorKind::RateLimited;
                failure.retryable = true;
                return failure;
            }
            "TOO_MANY_ACTIVE_RUNTIMES" => {
                failure.kind = ErrorKind::SessionLimitReached;
                return failure;
            }
            "DENYLISTED" => {
                failure.kind = ErrorKind::NotEntitled;
                return failure;
            }
            "QUOTA_EXCEEDED_USAGE_TIME" | "FAILED_PRECONDITION" => {
                failure.kind = ErrorKind::CapacityRefused;
                return failure;
            }
            _ => {}
        }
    }
    if code == 8 {
        failure.kind = ErrorKind::RateLimited;
        failure.retryable = true;
        return failure;
    }
    if code == 9 {
        failure.kind = ErrorKind::CapacityRefused;
        return failure;
    }
    if code == 6 {
        failure.already_exists = true;
        failure.kind = ErrorKind::Other;
        return failure;
    }
    if code == 3 || status == 400 {
        failure.kind = ErrorKind::NotEntitled;
        return failure;
    }
    if code == 7 || status == 401 || status == 403 {
        failure.auth_denied = true;
        failure.kind = ErrorKind::AuthRequired;
        return failure;
    }
    if status == 404 {
        failure.kind = ErrorKind::SessionGone;
        return failure;
    }
    if status == 412 {
        failure.kind = ErrorKind::CapacityRefused;
        return failure;
    }
    if status == 429 && ssh_busy(&value) {
        failure.kind = ErrorKind::SshBusy;
        return failure;
    }
    if matches!(status, 429 | 502 | 503 | 504) {
        failure.kind = ErrorKind::RateLimited;
        failure.retryable = true;
        return failure;
    }
    failure
}

fn ssh_busy(value: &Value) -> bool {
    value
        .get("error")
        .and_then(|e| e.as_str())
        .is_some_and(|e| e == "already-active-session")
}

fn find_reason(value: &Value) -> Option<String> {
    if let Some(reason) = value.get("reason").and_then(|v| v.as_str()) {
        return Some(reason.to_owned());
    }
    if let Some(reason) = value.pointer("/error/reason").and_then(|v| v.as_str()) {
        return Some(reason.to_owned());
    }
    let details = value
        .pointer("/error/details")
        .and_then(|v| v.as_array())
        .or_else(|| value.get("details").and_then(|v| v.as_array()));
    if let Some(details) = details {
        for detail in details {
            if let Some(reason) = detail.get("reason").and_then(|v| v.as_str()) {
                return Some(reason.to_owned());
            }
        }
    }
    value
        .pointer("/error/status")
        .and_then(|v| v.as_str())
        .filter(|s| {
            matches!(
                *s,
                "RESOURCE_EXHAUSTED"
                    | "FAILED_PRECONDITION"
                    | "ALREADY_EXISTS"
                    | "PERMISSION_DENIED"
                    | "INVALID_ARGUMENT"
            )
        })
        .map(|s| s.to_owned())
}

fn find_code(value: &Value) -> Option<i64> {
    value
        .pointer("/error/code")
        .and_then(|v| v.as_i64())
        .or_else(|| value.get("code").and_then(|v| v.as_i64()))
}

fn find_message(value: &Value) -> Option<String> {
    value
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("message").and_then(|v| v.as_str()))
        .or_else(|| value.get("error").and_then(|v| v.as_str()))
        .map(|s| s.to_owned())
}

fn find_limit(value: &Value) -> Option<u32> {
    value
        .get("limit")
        .and_then(|v| v.as_u64())
        .or_else(|| value.pointer("/error/limit").and_then(|v| v.as_u64()))
        .and_then(|n| u32::try_from(n).ok())
}

pub fn variant_of(accelerator: Accelerator) -> &'static str {
    if accelerator.is_tpu() {
        "VARIANT_TPU"
    } else if accelerator.is_gpu() {
        "VARIANT_GPU"
    } else {
        "VARIANT_CPU"
    }
}

pub fn tunnel_variant(accelerator: Accelerator) -> &'static str {
    if accelerator.is_tpu() {
        "TPU"
    } else if accelerator.is_gpu() {
        "GPU"
    } else {
        "DEFAULT"
    }
}

pub fn shape_of(accelerator: Accelerator, high_mem: bool) -> &'static str {
    if high_mem && accelerator.supports_high_mem() {
        "SHAPE_HIGHMEM"
    } else {
        "SHAPE_STANDARD"
    }
}

/// Tunnel assign query. High-mem-only accelerators ignore `shape=hm` (same as the CLI).
pub fn tunnel_shape(accelerator: Accelerator, high_mem: bool) -> &'static str {
    if high_mem && accelerator.supports_high_mem() {
        "hm"
    } else {
        ""
    }
}

pub fn accelerator_wire(accelerator: Accelerator) -> String {
    match accelerator {
        Accelerator::None => String::new(),
        other => other.as_str().to_owned(),
    }
}

pub fn parse_accelerator(label: &str) -> Accelerator {
    let label = label.trim();
    if label.is_empty() || label.eq_ignore_ascii_case("CPU") || label == "NONE" {
        return Accelerator::None;
    }
    Accelerator::from_label(label).unwrap_or(Accelerator::None)
}

pub fn high_mem_shape(shape: &str) -> bool {
    let shape = shape.trim();
    shape.eq_ignore_ascii_case("SHAPE_HIGHMEM")
        || shape.eq_ignore_ascii_case("HIGHMEM")
        || shape.eq_ignore_ascii_case("High-RAM")
        || shape == "1"
        || shape.eq_ignore_ascii_case("hm")
}

/// Runtime JSON from list, get, or a finished create operation.
pub fn parse_runtime(value: &Value) -> Option<ParsedRuntime> {
    let resource = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let spec = value.get("runtimeSpec").unwrap_or(value);
    let accelerator = parse_accelerator(
        spec.get("accelerator")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    let shape = spec.get("shape").and_then(|v| v.as_str()).unwrap_or("");
    let variant = spec
        .get("variant")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let info = value.get("connectionInfo").cloned().unwrap_or(Value::Null);
    let endpoint = info
        .get("endpoint")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("endpoint").and_then(|v| v.as_str()))
        .unwrap_or("")
        .trim_start_matches("runtimes/")
        .to_owned();
    if resource.is_empty() && endpoint.is_empty() {
        return None;
    }
    let resource = if resource.is_empty() {
        format!("runtimes/{endpoint}")
    } else if resource.starts_with("runtimes/") {
        resource
    } else {
        format!("runtimes/{resource}")
    };
    Some(ParsedRuntime {
        listed: ListedRuntime {
            resource: resource.clone(),
            endpoint,
            accelerator,
            high_mem: high_mem_shape(shape),
            variant,
        },
        connection: info
            .get("token")
            .and_then(|v| v.as_str())
            .map(|token| ConnectionInfo {
                url: info
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                token: token.to_owned(),
                expire_time: info
                    .get("expireTime")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned()),
                resource,
                endpoint: info
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            }),
    })
}

#[derive(Debug, Clone)]
pub struct ParsedRuntime {
    pub listed: ListedRuntime,
    pub connection: Option<ConnectionInfo>,
}

pub fn parse_runtime_list(body: &[u8]) -> Result<Vec<ParsedRuntime>, Error> {
    let value = parse_json(body)?;
    let items = value
        .get("runtimes")
        .and_then(|v| v.as_array())
        .or_else(|| value.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(items.iter().filter_map(parse_runtime).collect())
}

pub fn parse_operation(body: &[u8]) -> Result<Operation, Error> {
    let value = parse_json(body)?;
    if value.get("error").is_some() && value.get("done").is_none() && value.get("name").is_none() {
        let failure = classify(200, body);
        return Err(failure.into_error());
    }
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let done = value.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
    let error = value.get("error").cloned();
    let response = value
        .get("response")
        .cloned()
        .or_else(|| value.get("runtime").cloned());
    Ok(Operation {
        name,
        done,
        error,
        response,
    })
}

#[derive(Debug, Clone)]
pub struct Operation {
    pub name: String,
    pub done: bool,
    pub error: Option<Value>,
    pub response: Option<Value>,
}

impl Operation {
    pub fn failure(&self) -> Option<ApiFailure> {
        self.error.as_ref().map(|error| {
            let body = Value::Object({
                let mut map = Map::new();
                map.insert("error".into(), error.clone());
                map
            });
            let bytes = serde_json::to_vec(&body).unwrap_or_default();
            classify(200, &bytes)
        })
    }
}

pub fn parse_json(body: &[u8]) -> Result<Value, Error> {
    let text = String::from_utf8_lossy(body);
    serde_json::from_str(strip_xssi(&text)).map_err(|error| {
        Error::new(ErrorKind::Parse).detail(format!("colab response was not json: {error}"))
    })
}

pub fn parse_specs(
    body: &[u8],
) -> Result<(Option<String>, Vec<crate::backend::RuntimeOffer>), Error> {
    let value = parse_json(body)?;
    let items = value
        .get("runtimeSpecs")
        .or_else(|| value.get("runtime_specs"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let offers = items
        .iter()
        .filter_map(|spec| {
            let accelerator = match spec.get("accelerator").and_then(|v| v.as_str()) {
                Some(label) => parse_accelerator(label),
                None => match spec.get("variant").and_then(|v| v.as_str()) {
                    Some("VARIANT_CPU" | "CPU" | "DEFAULT") => Accelerator::None,
                    _ => return None,
                },
            };
            let shape = spec.get("shape").and_then(|v| v.as_str()).unwrap_or("");
            let eligible = spec
                .get("eligible")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            Some(crate::backend::RuntimeOffer {
                accelerator,
                high_mem: high_mem_shape(shape),
                eligible,
            })
        })
        .collect();
    Ok((None, offers))
}

pub fn parse_subscription_tier(body: &[u8]) -> Result<Option<String>, Error> {
    let value = parse_json(body)?;
    Ok(value
        .get("subscriptionTier")
        .or_else(|| value.get("tier"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned()))
}

pub fn parse_usage(body: &[u8]) -> Result<crate::usage::Usage, Error> {
    let value = parse_json(body)?;
    let balance = value
        .get("currentBalance")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| Error::new(ErrorKind::Parse).detail("ccu-info has no currentBalance"))?;
    let rate = value
        .get("consumptionRateHourly")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let assignments = value
        .get("assignmentsCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    Ok(crate::usage::Usage {
        balance_cu: balance,
        rate_cu_per_hour: rate,
        assignments,
    })
}

/// Assignment JSON from the tunnel (`/tun/m/assign` or `/tun/m/assignments`).
pub fn parse_assignment(value: &Value) -> Option<ParsedRuntime> {
    let endpoint = value
        .get("endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    if endpoint.is_empty() {
        return None;
    }
    let accelerator = parse_accelerator(
        value
            .get("accelerator")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    let shape = value.get("shape").and_then(|v| v.as_str()).unwrap_or("");
    let variant = value
        .get("variant")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let resource = value
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|n| !n.is_empty())
        .map(|n| {
            if n.starts_with("runtimes/") {
                n.to_owned()
            } else {
                format!("runtimes/{n}")
            }
        })
        .unwrap_or_else(|| format!("runtimes/{endpoint}"));
    let token = value
        .get("token")
        .or_else(|| value.pointer("/connectionInfo/token"))
        .and_then(|v| v.as_str());
    let url = value
        .get("url")
        .or_else(|| value.pointer("/connectionInfo/url"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    Some(ParsedRuntime {
        listed: ListedRuntime {
            resource: resource.clone(),
            endpoint: endpoint.clone(),
            accelerator,
            high_mem: high_mem_shape(shape),
            variant,
        },
        connection: token.map(|token| ConnectionInfo {
            url,
            token: token.to_owned(),
            expire_time: value
                .get("expireTime")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned()),
            resource,
            endpoint,
        }),
    })
}

pub fn xsrf_token(headers: &[(String, String)], body: &[u8]) -> Option<String> {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("x-goog-colab-token") && !value.is_empty() {
            return Some(value.clone());
        }
    }
    let value = parse_json(body).ok()?;
    value
        .get("token")
        .or_else(|| value.get("xsrfToken"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
}

pub fn auth_url(endpoints: &Endpoints, client_id: &str) -> String {
    let mut url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&access_type=offline&prompt=consent&scope={}",
        endpoints.auth,
        encode(client_id),
        encode(REDIRECT_URI),
        encode(OAUTH_SCOPES),
    );
    let _ = &mut url;
    url
}

pub fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub fn form_pair(key: &str, value: &str) -> String {
    format!("{}={}", encode(key), encode(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation(code: i64, reason: &str) -> Vec<u8> {
        format!(
            r#"{{"error":{{"code":{code},"message":"nope","details":[{{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"{reason}"}}]}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn reason_strings_map_and_terminal_ones_do_not_retry() {
        let cases = [
            ("RESOURCE_EXHAUSTED", ErrorKind::RateLimited, true),
            ("FAILED_PRECONDITION", ErrorKind::CapacityRefused, false),
            (
                "TOO_MANY_ACTIVE_RUNTIMES",
                ErrorKind::SessionLimitReached,
                false,
            ),
            ("DENYLISTED", ErrorKind::NotEntitled, false),
            (
                "QUOTA_EXCEEDED_USAGE_TIME",
                ErrorKind::CapacityRefused,
                false,
            ),
        ];
        for (reason, kind, retryable) in cases {
            let failure = classify(429, &operation(8, reason));
            assert_eq!(failure.kind, kind, "{reason}");
            assert_eq!(failure.retryable, retryable, "{reason}");
            assert_eq!(failure.reason.as_deref(), Some(reason));
        }
    }

    #[test]
    fn http_status_fallbacks_match_the_cli() {
        assert!(classify(503, b"{}").retryable);
        assert_eq!(classify(412, b"{}").kind, ErrorKind::CapacityRefused);
        assert!(!classify(412, b"{}").retryable);
        assert!(classify(403, b"{}").auth_denied);
        assert_eq!(classify(400, b"{}").kind, ErrorKind::NotEntitled);
        let busy = classify(429, br#"{"error":"already-active-session","limit":5}"#);
        assert_eq!(busy.kind, ErrorKind::SshBusy);
        assert_eq!(busy.limit, Some(5));
        assert!(!busy.retryable);
        assert!(classify(200, &operation(6, "ALREADY_EXISTS")).already_exists);
    }

    #[test]
    fn xssi_prefix_is_removed_before_json() {
        let body =
            b")]}'\n{\"currentBalance\":1.5,\"consumptionRateHourly\":0.25,\"assignmentsCount\":2}";
        let usage = parse_usage(body).unwrap();
        assert_eq!(usage.balance_cu, 1.5);
        assert_eq!(usage.assignments, 2);
    }

    #[test]
    fn logged_path_drops_the_query() {
        let path =
            path_only("http://127.0.0.1:9/api/contents/a?colab-runtime-proxy-token=secret-token");
        assert_eq!(path, "/api/contents/a");
        assert!(!path.contains("secret-token"));
    }
}
