//! Live RAM, GPU and disk from a Colab runtime proxy.
//!
//! The body is the JSON `colab-vscode` parses in `getResources`
//! (`ResourcesSchema`): `memory.{totalBytes,freeBytes}`, `disks[].filesystem`
//! and `gpus[]` with `gpuUtilization` as a fraction from 0 to 1. A missing
//! number stays unknown. Filling it in as 0 would show a runtime as idle when
//! the proxy simply omitted the field (`docs/DESIGN.md`, unknown usage).
//!
//! The proxy token and any URL that carries it stay in this module's stack
//! frame. They are never written onto an [`crate::Error`], a log line or
//! [`Debug`] of the result.

use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::error::{Error, ErrorKind, Result};
use crate::http::{HttpClient, HttpResponse};

/// How often a live reading is worth fetching again. Same order of magnitude as
/// colab-vscode's default `resource_poll_interval_ms` (10s).
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Header the runtime proxy expects. The value is the session token.
pub const PROXY_TOKEN_HEADER: &str = "X-Colab-Runtime-Proxy-Token";

const RESOURCES_PATH: &str = "api/colab/resources";

/// Why a live reading is not on screen. Never a stand-in for a measured 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceFault {
    /// The account's `sessions.json` has no proxy URL or token for this session.
    Unavailable,
    /// The stored token is past `token_expires_at`, or the proxy answered 401/403.
    Expired,
    /// The proxy could not be reached, or its body was not the resources shape.
    Unreachable,
}

/// One fetch. `Fresh` is the only variant that carries numbers.
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceStatus {
    Fresh(RuntimeResources),
    Unavailable,
    Expired,
    Unreachable,
}

impl ResourceStatus {
    pub fn fault(&self) -> Option<ResourceFault> {
        match self {
            ResourceStatus::Fresh(_) => None,
            ResourceStatus::Unavailable => Some(ResourceFault::Unavailable),
            ResourceStatus::Expired => Some(ResourceFault::Expired),
            ResourceStatus::Unreachable => Some(ResourceFault::Unreachable),
        }
    }
}

/// RAM from `memory`. Used bytes are `total - free` only when both are present
/// and free does not exceed total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryReading {
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

impl MemoryReading {
    pub fn used_bytes(self) -> Option<u64> {
        match (self.total_bytes, self.free_bytes) {
            (Some(total), Some(free)) if free <= total => Some(total - free),
            _ => None,
        }
    }
}

/// One GPU. Utilization fields are fractions in the 0–1 range the proxy sends,
/// not percents. Missing memory stays `None`, not 0.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuReading {
    pub name: Option<String>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub utilization: Option<f64>,
    pub memory_utilization: Option<f64>,
    pub ever_used: Option<bool>,
}

/// One filesystem, from `disks[].filesystem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskReading {
    pub label: Option<String>,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
}

/// What `GET /api/colab/resources` said, with omitted numbers left unknown.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeResources {
    pub memory: MemoryReading,
    pub gpus: Vec<GpuReading>,
    pub disks: Vec<DiskReading>,
}

/// VRAM across the GPUs that actually reported memory. A sum only happens when
/// every GPU reported both numbers, so a blank GPU cannot pull the total to 0.
#[derive(Debug, Clone, PartialEq)]
pub struct VramSummary {
    pub used_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub utilization: Option<f64>,
    pub name: Option<String>,
    pub count: usize,
}

impl RuntimeResources {
    pub fn vram(&self) -> VramSummary {
        let count = self.gpus.len();
        let known: Vec<&GpuReading> = self
            .gpus
            .iter()
            .filter(|gpu| gpu.memory_used_bytes.is_some() && gpu.memory_total_bytes.is_some())
            .collect();
        let (used_bytes, total_bytes, name) = if count > 0 && known.len() == count {
            // Every GPU in `known` has both numbers; the sum is not a fill-in for a blank one.
            let used: u64 = known.iter().filter_map(|gpu| gpu.memory_used_bytes).sum();
            let total: u64 = known.iter().filter_map(|gpu| gpu.memory_total_bytes).sum();
            let name = (count == 1)
                .then(|| self.gpus.first().and_then(|gpu| gpu.name.clone()))
                .flatten();
            (Some(used), Some(total), name)
        } else if known.len() == 1 {
            let gpu = known[0];
            (
                gpu.memory_used_bytes,
                gpu.memory_total_bytes,
                gpu.name.clone(),
            )
        } else {
            (
                None,
                None,
                self.gpus.iter().find_map(|gpu| gpu.name.clone()),
            )
        };
        let utilization = self.gpus.iter().find_map(|gpu| gpu.utilization);
        VramSummary {
            used_bytes,
            total_bytes,
            utilization,
            name,
            count,
        }
    }

    /// The filesystem a compact meter should show: the `kernel` one colab-vscode
    /// displays first, otherwise the first filesystem that reported a size.
    pub fn primary_disk(&self) -> Option<&DiskReading> {
        self.disks
            .iter()
            .find(|disk| {
                disk.label
                    .as_deref()
                    .is_some_and(|label| label.eq_ignore_ascii_case("kernel"))
            })
            .or_else(|| {
                self.disks
                    .iter()
                    .find(|disk| disk.used_bytes.is_some() || disk.total_bytes.is_some())
            })
            .or_else(|| self.disks.first())
    }
}

/// `used / total` as a fraction, or `None` when either side is unknown or the
/// total is 0. An unknown side is not treated as 0.
pub fn used_ratio(used: Option<u64>, total: Option<u64>) -> Option<f64> {
    match (used, total) {
        (Some(used), Some(total)) if total > 0 && used <= total => Some(used as f64 / total as f64),
        _ => None,
    }
}

/// Parses a resources body, including the `)]}'` prefix Google JSON sometimes
/// carries. A body that is not an object is an error. Omitted numbers are
/// unknown, not zero.
pub fn parse_resources(body: &str) -> Result<RuntimeResources> {
    let value: Value = serde_json::from_str(strip_xssi(body))
        .map_err(|error| Error::new(ErrorKind::Parse).detail(error.to_string()))?;
    let Some(obj) = value.as_object() else {
        return Err(Error::new(ErrorKind::Parse).detail("resources body is not an object"));
    };
    if let Some(disks) = obj.get("disks")
        && !disks.is_array()
        && !disks.is_null()
    {
        return Err(Error::new(ErrorKind::Parse).detail("disks is not a list"));
    }
    if let Some(gpus) = obj.get("gpus")
        && !gpus.is_array()
        && !gpus.is_null()
    {
        return Err(Error::new(ErrorKind::Parse).detail("gpus is not a list"));
    }
    let memory = obj
        .get("memory")
        .filter(|value| !value.is_null())
        .map(memory_of)
        .transpose()?
        .unwrap_or(MemoryReading {
            total_bytes: None,
            free_bytes: None,
        });
    let disks = obj
        .get("disks")
        .and_then(Value::as_array)
        .map(|disks| disks.iter().filter_map(disk_of).collect())
        .unwrap_or_default();
    let gpus = obj
        .get("gpus")
        .and_then(Value::as_array)
        .map(|gpus| gpus.iter().map(gpu_of).collect())
        .unwrap_or_default();
    Ok(RuntimeResources {
        memory,
        gpus,
        disks,
    })
}

/// Reads one session out of a CLI `sessions.json` and fetches its resources.
/// A missing file entry, a blank token and a past expiry do not call the proxy.
pub fn from_sessions_json(
    json: &str,
    session: &str,
    now: SystemTime,
    http: &dyn HttpClient,
) -> ResourceStatus {
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return ResourceStatus::Unavailable;
    };
    let Some(map) = value.as_object() else {
        return ResourceStatus::Unavailable;
    };
    let Some(record) = session_record(map, session) else {
        return ResourceStatus::Unavailable;
    };
    match proxy_of(record, now) {
        ProxyPick::Unavailable => ResourceStatus::Unavailable,
        ProxyPick::Expired => ResourceStatus::Expired,
        ProxyPick::Ready { url, token } => fetch_resources(http, &url, &token),
    }
}

/// `GET {base}/api/colab/resources` with the proxy token header.
/// Transport failures become [`ResourceStatus::Unreachable`] with the token and
/// the URL discarded, even when the HTTP error text contains them.
pub fn fetch_resources(http: &dyn HttpClient, base_url: &str, token: &str) -> ResourceStatus {
    if base_url.trim().is_empty() || token.is_empty() {
        return ResourceStatus::Unavailable;
    }
    let Some(url) = resources_url(base_url) else {
        return ResourceStatus::Unreachable;
    };
    match http.get(&url, &[(PROXY_TOKEN_HEADER, token)]) {
        Ok(response) => classify(response),
        Err(_) => ResourceStatus::Unreachable,
    }
}

fn classify(response: HttpResponse) -> ResourceStatus {
    match response.status {
        401 | 403 => ResourceStatus::Expired,
        200..=299 => match std::str::from_utf8(&response.body) {
            Ok(body) => parse_resources(body)
                .map(ResourceStatus::Fresh)
                .unwrap_or(ResourceStatus::Unreachable),
            Err(_) => ResourceStatus::Unreachable,
        },
        _ => ResourceStatus::Unreachable,
    }
}

fn resources_url(base: &str) -> Option<String> {
    let base = base.trim().trim_end_matches('/');
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return None;
    }
    Some(format!("{base}/{RESOURCES_PATH}"))
}

fn strip_xssi(body: &str) -> &str {
    let body = body.trim_start_matches('\u{feff}').trim_start();
    let rest = body.strip_prefix(")]}'").unwrap_or(body);
    rest.strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .unwrap_or(rest)
        .trim()
}

fn session_record<'a>(map: &'a serde_json::Map<String, Value>, session: &str) -> Option<&'a Value> {
    if let Some(record) = map.get(session) {
        return Some(record);
    }
    map.values()
        .find(|record| record.get("name").and_then(Value::as_str) == Some(session))
}

enum ProxyPick {
    Ready { url: String, token: String },
    Expired,
    Unavailable,
}

fn proxy_of(record: &Value, now: SystemTime) -> ProxyPick {
    let Some(record) = record.as_object() else {
        return ProxyPick::Unavailable;
    };
    let url = record
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let token = record
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if url.is_empty() || token.is_empty() {
        return ProxyPick::Unavailable;
    }
    if let Some(expires) = record
        .get("token_expires_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        && expires <= now
    {
        return ProxyPick::Expired;
    }
    ProxyPick::Ready {
        url: url.to_owned(),
        token: token.to_owned(),
    }
}

fn memory_of(value: &Value) -> Result<MemoryReading> {
    let Some(obj) = value.as_object() else {
        return Err(Error::new(ErrorKind::Parse).detail("memory is not an object"));
    };
    Ok(MemoryReading {
        total_bytes: opt_u64(obj.get("totalBytes")),
        free_bytes: opt_u64(obj.get("freeBytes")),
    })
}

fn disk_of(value: &Value) -> Option<DiskReading> {
    let filesystem = value.get("filesystem").filter(|inner| !inner.is_null());
    let filesystem = filesystem?.as_object()?;
    Some(DiskReading {
        label: filesystem
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_owned),
        total_bytes: opt_u64(filesystem.get("totalBytes")),
        used_bytes: opt_u64(filesystem.get("usedBytes")),
    })
}

fn gpu_of(value: &Value) -> GpuReading {
    let obj = value.as_object();
    let field = |name: &str| obj.and_then(|obj| obj.get(name));
    GpuReading {
        name: field("name").and_then(Value::as_str).map(str::to_owned),
        memory_used_bytes: opt_u64(field("memoryUsedBytes")),
        memory_total_bytes: opt_u64(field("memoryTotalBytes")),
        utilization: opt_f64(field("gpuUtilization")),
        memory_utilization: opt_f64(field("memoryUtilization")),
        ever_used: field("everUsed").and_then(Value::as_bool),
    }
}

fn opt_u64(value: Option<&Value>) -> Option<u64> {
    let value = value.filter(|value| !value.is_null())?;
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    if let Some(number) = value.as_i64() {
        return (number >= 0).then_some(number as u64);
    }
    if let Some(text) = value.as_str() {
        return text.parse().ok();
    }
    None
}

fn opt_f64(value: Option<&Value>) -> Option<f64> {
    let value = value.filter(|value| !value.is_null())?;
    let number = if let Some(number) = value.as_f64() {
        number
    } else {
        let text = value.as_str()?;
        text.parse().ok()?
    };
    number.is_finite().then_some(number)
}

/// `YYYY-MM-DDTHH:MM:SS[.frac]Z` or an numeric offset, as pydantic writes
/// `token_expires_at`.
fn parse_rfc3339(input: &str) -> Option<SystemTime> {
    let (date, rest) = input.trim().split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i32 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || month_len(year, month) < day || day == 0 {
        return None;
    }
    let (time, offset) = split_offset(rest)?;
    let time = time.split('.').next().unwrap_or(time);
    let mut time_parts = time.split(':');
    let hour: u32 = time_parts.next()?.parse().ok()?;
    let minute: u32 = time_parts.next()?.parse().ok()?;
    let second: u32 = time_parts.next()?.parse().ok()?;
    if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let unix = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?
        .checked_sub(offset)?;
    if unix >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(unix as u64))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs((-unix) as u64))
    }
}

fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(time) = rest.strip_suffix('Z') {
        return Some((time, 0));
    }
    let index = rest.rfind(['+', '-'])?;
    let (time, offset) = rest.split_at(index);
    let sign: i64 = if offset.starts_with('+') { 1 } else { -1 };
    let (hours, minutes) = offset[1..].split_once(':')?;
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some((time, sign * (hours * 3_600 + minutes * 60)))
}

fn month_len(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 (Howard Hinnant).
fn days_from_civil(mut year: i32, month: u32, day: u32) -> i64 {
    year -= i32::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = (year - era * 400) as u64;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + u64::from(day_of_year);
    i64::from(era) * 146_097 + day_of_era as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountId;
    use crate::backend::{Backend, RuntimeRef};
    use crate::clock::CancelToken;
    use crate::mock::{MockBackend, MockConfig, MockScenario};
    use std::sync::Mutex;

    const TOKEN: &str = "super-secret-proxy-token";
    const SECRET_IN_URL: &str = "userinfo-secret";

    /// The body `colab-vscode`'s `getResources` unit test sends, plus an empty
    /// disk and an empty GPU that the vscode schema turns into zeros.
    fn vscode_body() -> String {
        r#"{
          "memory": { "totalBytes": 13605834752, "freeBytes": 12475244544 },
          "disks": [
            { "filesystem": { "label": "kernel", "totalBytes": 115658190848, "usedBytes": 22869635072 } },
            { "filesystem": {} }
          ],
          "gpus": [
            {
              "name": "Tesla T4",
              "memoryUsedBytes": 2869635072,
              "memoryTotalBytes": 13605834752,
              "gpuUtilization": 0.15,
              "memoryUtilization": 0.21,
              "everUsed": true
            },
            {}
          ]
        }"#
        .to_owned()
    }

    struct Scripted {
        status: u16,
        body: String,
        calls: Mutex<u32>,
        saw_token: Mutex<bool>,
    }

    impl HttpClient for Scripted {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse> {
            *self.calls.lock().expect("calls") += 1;
            let token = headers
                .iter()
                .find(|(name, _)| *name == PROXY_TOKEN_HEADER)
                .map(|(_, value)| *value);
            *self.saw_token.lock().expect("header") = token == Some(TOKEN);
            assert!(
                !url.contains(TOKEN) && !url.contains(SECRET_IN_URL),
                "the request URL must not carry the proxy secret"
            );
            Ok(HttpResponse {
                status: self.status,
                body: self.body.as_bytes().to_vec(),
            })
        }
    }

    struct Leaky;

    impl HttpClient for Leaky {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse> {
            let token = headers
                .iter()
                .find(|(name, _)| *name == PROXY_TOKEN_HEADER)
                .map(|(_, value)| *value)
                .unwrap_or("");
            Err(Error::new(ErrorKind::Http)
                .with("url", url)
                .detail(format!("{token} {SECRET_IN_URL}")))
        }
    }

    struct PanicHttp;

    impl HttpClient for PanicHttp {
        fn get(&self, _: &str, _: &[(&str, &str)]) -> Result<HttpResponse> {
            panic!("the proxy must not be called");
        }
    }

    fn assert_no_secret(status: &ResourceStatus) {
        let rendered = format!("{status:?}");
        assert!(!rendered.contains(TOKEN), "{rendered}");
        assert!(!rendered.contains(SECRET_IN_URL), "{rendered}");
    }

    fn sessions(expires: &str, url: &str) -> String {
        format!(
            r#"{{
                "llm": {{
                    "name": "llm",
                    "token": "{TOKEN}",
                    "url": "{url}",
                    "endpoint": "m-s-foo",
                    "token_expires_at": "{expires}",
                    "variant": "GPU",
                    "accelerator": "T4",
                    "machine_shape": "STANDARD"
                }}
            }}"#
        )
    }

    #[test]
    fn rfc3339_epoch_and_offset() {
        assert_eq!(
            parse_rfc3339("1970-01-01T00:00:00Z"),
            Some(SystemTime::UNIX_EPOCH)
        );
        assert_eq!(
            parse_rfc3339("1970-01-01T08:00:00+08:00"),
            Some(SystemTime::UNIX_EPOCH)
        );
        assert_eq!(
            parse_rfc3339("2000-01-01T00:00:00Z"),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(946_684_800))
        );
    }

    #[test]
    fn vscode_shape_keeps_present_numbers_and_does_not_invent_zeros() {
        let parsed = parse_resources(&format!(")]}}'\n{}", vscode_body())).unwrap();
        assert_eq!(parsed.memory.total_bytes, Some(13_605_834_752));
        assert_eq!(parsed.memory.free_bytes, Some(12_475_244_544));
        assert_eq!(parsed.memory.used_bytes(), Some(1_130_590_208));
        let kernel = parsed.primary_disk().unwrap();
        assert_eq!(kernel.label.as_deref(), Some("kernel"));
        assert_eq!(kernel.total_bytes, Some(115_658_190_848));
        assert_eq!(kernel.used_bytes, Some(22_869_635_072));
        let blank = &parsed.disks[1];
        assert_eq!(blank.total_bytes, None);
        assert_eq!(blank.used_bytes, None);
        assert_eq!(parsed.gpus[0].name.as_deref(), Some("Tesla T4"));
        assert_eq!(parsed.gpus[0].memory_used_bytes, Some(2_869_635_072));
        assert_eq!(parsed.gpus[0].memory_total_bytes, Some(13_605_834_752));
        assert_eq!(parsed.gpus[0].utilization, Some(0.15));
        assert_eq!(parsed.gpus[0].memory_utilization, Some(0.21));
        assert_eq!(parsed.gpus[0].ever_used, Some(true));
        assert_eq!(parsed.gpus[1].memory_used_bytes, None);
        assert_eq!(parsed.gpus[1].memory_total_bytes, None);
        let vram = parsed.vram();
        assert_eq!(vram.used_bytes, Some(2_869_635_072));
        assert_eq!(vram.total_bytes, Some(13_605_834_752));
        assert!(used_ratio(Some(0), Some(10)).is_some());
        assert_eq!(used_ratio(None, Some(10)), None);
    }

    #[test]
    fn sessions_fetch_sends_the_header_and_a_failure_keeps_the_token_out() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let http = Scripted {
            status: 200,
            body: format!(")]}}'\n{}", vscode_body()),
            calls: Mutex::new(0),
            saw_token: Mutex::new(false),
        };
        let status = from_sessions_json(
            &sessions("2999-01-01T00:00:00Z", "https://8080-m-s-foo.example/"),
            "llm",
            now,
            &http,
        );
        assert!(matches!(status, ResourceStatus::Fresh(_)));
        assert_eq!(*http.calls.lock().unwrap(), 1);
        assert!(*http.saw_token.lock().unwrap());
        assert_no_secret(&status);

        let leaked = from_sessions_json(
            &sessions(
                "2999-01-01T00:00:00.000000Z",
                &format!("https://{SECRET_IN_URL}@8080-m-s-foo.example"),
            ),
            "llm",
            now,
            &Leaky,
        );
        assert_eq!(leaked, ResourceStatus::Unreachable);
        assert_no_secret(&leaked);

        let denied = Scripted {
            status: 401,
            body: format!("token {TOKEN}"),
            calls: Mutex::new(0),
            saw_token: Mutex::new(false),
        };
        let status = from_sessions_json(
            &sessions("2999-01-01T00:00:00Z", "https://8080-m-s-foo.example"),
            "llm",
            now,
            &denied,
        );
        assert_eq!(status, ResourceStatus::Expired);
        assert_no_secret(&status);
    }

    #[test]
    fn expired_or_missing_proxy_does_not_call_and_is_not_a_zero_reading() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let expired = from_sessions_json(
            &sessions("2000-01-01T00:00:00Z", "https://8080-m-s-foo.example"),
            "llm",
            now,
            &PanicHttp,
        );
        assert_eq!(expired, ResourceStatus::Expired);
        let missing = from_sessions_json(
            r#"{"llm":{"name":"llm","token":"","url":"https://example"}}"#,
            "llm",
            now,
            &PanicHttp,
        );
        assert_eq!(missing, ResourceStatus::Unavailable);
        let absent = from_sessions_json("{}", "llm", now, &PanicHttp);
        assert_eq!(absent, ResourceStatus::Unavailable);
        let garbage = from_sessions_json("not-json", "llm", now, &PanicHttp);
        assert_eq!(garbage, ResourceStatus::Unavailable);
    }

    #[test]
    fn mock_demo_reports_live_numbers_and_a_fault_has_none() {
        let (config, _) = MockConfig::headless(MockScenario::demo());
        let backend = MockBackend::new(config);
        let account = AccountId::new("mock1").unwrap();
        let target = RuntimeRef {
            account: account.clone(),
            endpoint: String::new(),
            name: Some("a100-7f3a".into()),
        };
        let ResourceStatus::Fresh(reading) = backend
            .runtime_resources(&target, &CancelToken::new())
            .unwrap()
        else {
            panic!("named demo session should have a reading");
        };
        assert!(reading.memory.used_bytes().unwrap() > 0);
        assert!(reading.vram().used_bytes.unwrap() > 0);
        assert!(reading.primary_disk().unwrap().used_bytes.unwrap() > 0);
        let cpu = RuntimeRef {
            account: AccountId::new("mock2").unwrap(),
            endpoint: String::new(),
            name: Some("cpu-0b5e".into()),
        };
        let ResourceStatus::Fresh(cpu_reading) = backend
            .runtime_resources(&cpu, &CancelToken::new())
            .unwrap()
        else {
            panic!("cpu session still has RAM and disk");
        };
        assert!(cpu_reading.gpus.is_empty());
        assert!(cpu_reading.memory.used_bytes().unwrap() > 0);
        let unnamed = RuntimeRef {
            account,
            endpoint: String::new(),
            name: None,
        };
        assert_eq!(
            backend
                .runtime_resources(&unnamed, &CancelToken::new())
                .unwrap(),
            ResourceStatus::Unavailable
        );
        backend.set_resource_fault(Some(ResourceFault::Expired));
        assert_eq!(
            backend
                .runtime_resources(&target, &CancelToken::new())
                .unwrap(),
            ResourceStatus::Expired
        );
    }
}
