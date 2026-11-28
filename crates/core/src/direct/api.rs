//! One account's calls to the public API, with the tunnel as a fallback.
//!
//! Retries cover `RESOURCE_EXHAUSTED` and HTTP 429/502/503/504. Terminal reasons
//! (`FAILED_PRECONDITION`, `TOO_MANY_ACTIVE_RUNTIMES`, `DENYLISTED`,
//! `QUOTA_EXCEEDED_USAGE_TIME`) are returned on the first response.
//! Call records are `METHOD /path STATUS` with the query stripped.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::clock::{CancelToken, Clock};
use crate::error::{Error, ErrorKind, Result};
use crate::sessions::Accelerator;

use super::protocol::{self, ApiFailure, CLIENT_AGENT, Endpoints, ParsedRuntime};
use super::transport::{HttpExchange, HttpRequest, HttpResponse};

const CALL_RING: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Api {
    pub http: Arc<dyn HttpExchange>,
    pub clock: Arc<dyn Clock>,
    pub endpoints: Endpoints,
    pub retry_delays: Vec<Duration>,
    calls: Arc<Mutex<VecDeque<String>>>,
}

impl Api {
    pub fn new(
        http: Arc<dyn HttpExchange>,
        clock: Arc<dyn Clock>,
        endpoints: Endpoints,
        retry_delays: Vec<Duration>,
        calls: Arc<Mutex<VecDeque<String>>>,
    ) -> Self {
        Api {
            http,
            clock,
            endpoints,
            retry_delays,
            calls,
        }
    }

    pub fn record(&self, method: &str, path: &str, status: u16) {
        let url = format!("http://direct.local{path}");
        self.note(method, &url, status);
    }

    pub fn recent_calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    fn note(&self, method: &str, url: &str, status: u16) {
        let line = format!("{method} {} {status}", protocol::path_only(url));
        log::debug!("direct: {line}");
        let mut calls = self.calls.lock().unwrap_or_else(|error| error.into_inner());
        if calls.len() >= CALL_RING {
            calls.pop_front();
        }
        calls.push_back(line);
    }

    fn send_once(
        &self,
        method: &str,
        url: &str,
        bearer: Option<&str>,
        extra: &[(String, String)],
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<HttpResponse> {
        let mut headers = vec![
            ("Accept".into(), "application/json".into()),
            ("X-Colab-Client-Agent".into(), CLIENT_AGENT.into()),
        ];
        if let Some(kind) = content_type {
            headers.push(("Content-Type".into(), kind.into()));
        }
        if let Some(bearer) = bearer {
            headers.push(("Authorization".into(), format!("Bearer {bearer}")));
        }
        headers.extend(extra.iter().cloned());
        let response = self.http.exchange(&HttpRequest {
            method: method.into(),
            url: url.into(),
            headers,
            body,
            timeout: TIMEOUT,
        })?;
        self.note(method, url, response.status);
        Ok(response)
    }

    fn call(
        &self,
        method: &str,
        url: &str,
        bearer: Option<&str>,
        body: Vec<u8>,
        content_type: Option<&str>,
        cancel: &CancelToken,
    ) -> std::result::Result<HttpResponse, ApiFailure> {
        let mut delays = self.retry_delays.clone();
        loop {
            if cancel.is_cancelled() {
                return Err(cancelled_failure());
            }
            let response = self
                .send_once(method, url, bearer, &[], body.clone(), content_type)
                .map_err(transport_failure)?;
            if response.status < 400 {
                return Ok(response);
            }
            let failure = protocol::classify(response.status, &response.body);
            if failure.retryable && !delays.is_empty() {
                let delay = delays.remove(0);
                if let Err(error) = self.clock.sleep(delay, cancel) {
                    return Err(error_failure(error));
                }
                continue;
            }
            return Err(failure);
        }
    }

    pub fn subscription(
        &self,
        bearer: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<Option<String>, ApiFailure> {
        let url = protocol::join(&self.endpoints.public_api, "/v1beta/subscription");
        let response = self.call("GET", &url, Some(bearer), Vec::new(), None, cancel)?;
        protocol::parse_subscription_tier(&response.body).map_err(error_failure)
    }

    pub fn specs(
        &self,
        bearer: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<Vec<crate::backend::RuntimeOffer>, ApiFailure> {
        let url = protocol::join(&self.endpoints.public_api, "/v1beta/runtimespecs");
        let response = self.call("GET", &url, Some(bearer), Vec::new(), None, cancel)?;
        protocol::parse_specs(&response.body)
            .map(|(_, offers)| offers)
            .map_err(error_failure)
    }

    pub fn list_runtimes(
        &self,
        bearer: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<Vec<ParsedRuntime>, ApiFailure> {
        let url = protocol::join(&self.endpoints.public_api, "/v1beta/runtimes");
        match self.call("GET", &url, Some(bearer), Vec::new(), None, cancel) {
            Ok(response) => protocol::parse_runtime_list(&response.body).map_err(error_failure),
            Err(failure) if failure.auth_denied => self.tunnel_assignments(bearer, cancel),
            Err(failure) => Err(failure),
        }
    }

    pub fn usage(
        &self,
        bearer: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<crate::usage::Usage, ApiFailure> {
        let url = format!(
            "{}?authuser=0",
            protocol::join(&self.endpoints.tunnel, "/tun/m/ccu-info")
        );
        let response = self.call("GET", &url, Some(bearer), Vec::new(), None, cancel)?;
        protocol::parse_usage(&response.body).map_err(error_failure)
    }

    pub fn create_runtime(
        &self,
        bearer: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> std::result::Result<ParsedRuntime, ApiFailure> {
        match self.create_public(bearer, accelerator, high_mem, cancel) {
            Err(failure) if failure.auth_denied => {
                self.tunnel_assign(bearer, accelerator, high_mem, cancel)
            }
            Err(failure) if failure.already_exists => {
                self.adopt_existing(bearer, accelerator, high_mem, cancel)
            }
            other => other,
        }
    }

    fn create_public(
        &self,
        bearer: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> std::result::Result<ParsedRuntime, ApiFailure> {
        let runtime_id = format!("r-{}", runtime_suffix());
        let request_id = runtime_suffix();
        let url = format!(
            "{}?runtimeId={}&requestId={}",
            protocol::join(&self.endpoints.public_api, "/v1beta/runtimes"),
            protocol::encode(&runtime_id),
            protocol::encode(&request_id)
        );
        let body = json!({
            "runtimeSpec": {
                "variant": protocol::variant_of(accelerator),
                "accelerator": protocol::accelerator_wire(accelerator),
                "shape": protocol::shape_of(accelerator, high_mem),
            }
        });
        let mut delays = self.retry_delays.clone();
        loop {
            if cancel.is_cancelled() {
                return Err(cancelled_failure());
            }
            let response = self
                .send_once(
                    "POST",
                    &url,
                    Some(bearer),
                    &[],
                    serde_json::to_vec(&body).unwrap_or_default(),
                    Some("application/json"),
                )
                .map_err(transport_failure)?;
            if response.status >= 400 {
                let failure = protocol::classify(response.status, &response.body);
                if failure.auth_denied || failure.already_exists || !failure.retryable {
                    return Err(failure);
                }
                if delays.is_empty() {
                    return Err(failure);
                }
                let delay = delays.remove(0);
                self.clock.sleep(delay, cancel).map_err(error_failure)?;
                continue;
            }
            let mut operation = protocol::parse_operation(&response.body).map_err(error_failure)?;
            if !operation.done && !operation.name.is_empty() {
                operation = self.poll_operation(bearer, &operation.name, cancel)?;
            }
            if let Some(failure) = operation.failure() {
                if failure.retryable && !delays.is_empty() {
                    let delay = delays.remove(0);
                    self.clock.sleep(delay, cancel).map_err(error_failure)?;
                    continue;
                }
                return Err(failure);
            }
            let response = operation.response.unwrap_or(Value::Null);
            return protocol::parse_runtime(&response).ok_or_else(|| {
                error_failure(Error::new(ErrorKind::Parse).detail("create returned no runtime"))
            });
        }
    }

    fn poll_operation(
        &self,
        bearer: &str,
        name: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<protocol::Operation, ApiFailure> {
        let url = protocol::join(&self.endpoints.public_api, &format!("/v1/{name}:wait"));
        for _ in 0..5 {
            if cancel.is_cancelled() {
                return Err(cancelled_failure());
            }
            let response = self.call("GET", &url, Some(bearer), Vec::new(), None, cancel)?;
            let operation = protocol::parse_operation(&response.body).map_err(error_failure)?;
            if operation.done || operation.error.is_some() {
                return Ok(operation);
            }
            self.clock
                .sleep(Duration::from_millis(10), cancel)
                .map_err(error_failure)?;
        }
        Err(error_failure(
            Error::new(ErrorKind::Timeout).detail("runtime create operation did not finish"),
        ))
    }

    fn adopt_existing(
        &self,
        bearer: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> std::result::Result<ParsedRuntime, ApiFailure> {
        let list = self.list_runtimes(bearer, cancel)?;
        let found = list.into_iter().find(|runtime| {
            runtime.listed.accelerator == accelerator && runtime.listed.high_mem == high_mem
        });
        found.ok_or_else(|| {
            error_failure(
                Error::new(ErrorKind::Other)
                    .detail("create reported ALREADY_EXISTS but no runtime matched"),
            )
        })
    }

    pub fn delete_runtime(
        &self,
        bearer: &str,
        resource: &str,
        endpoint: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<(), ApiFailure> {
        let id = resource.trim_start_matches("runtimes/");
        let url = format!(
            "{}?allowMissing=true",
            protocol::join(
                &self.endpoints.public_api,
                &format!("/v1beta/runtimes/{id}")
            )
        );
        match self.call("DELETE", &url, Some(bearer), Vec::new(), None, cancel) {
            Ok(_) => Ok(()),
            Err(failure) if failure.auth_denied => self.tunnel_unassign(bearer, endpoint, cancel),
            Err(failure) if failure.kind == ErrorKind::SessionGone => Ok(()),
            Err(failure) => Err(failure),
        }
    }

    fn tunnel_assignments(
        &self,
        bearer: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<Vec<ParsedRuntime>, ApiFailure> {
        let url = format!(
            "{}?authuser=0",
            protocol::join(&self.endpoints.tunnel, "/tun/m/assignments")
        );
        let response = self.call("GET", &url, Some(bearer), Vec::new(), None, cancel)?;
        let value = protocol::parse_json(&response.body).map_err(error_failure)?;
        let items = value
            .get("assignments")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(items
            .iter()
            .filter_map(protocol::parse_assignment)
            .collect())
    }

    fn tunnel_assign(
        &self,
        bearer: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> std::result::Result<ParsedRuntime, ApiFailure> {
        let query = format!(
            "authuser=0&nbh=&variant={}&accelerator={}&shape={}",
            protocol::encode(protocol::tunnel_variant(accelerator)),
            protocol::encode(&protocol::accelerator_wire(accelerator)),
            protocol::encode(protocol::tunnel_shape(accelerator, high_mem))
        );
        let url = format!(
            "{}?{query}",
            protocol::join(&self.endpoints.tunnel, "/tun/m/assign")
        );
        let probe = self.call("GET", &url, Some(bearer), Vec::new(), None, cancel)?;
        let token = protocol::xsrf_token(&probe.headers, &probe.body).ok_or_else(|| {
            error_failure(Error::new(ErrorKind::Parse).detail("tunnel assign had no xsrf token"))
        })?;
        let response = self
            .send_once(
                "POST",
                &url,
                Some(bearer),
                &[("X-Goog-Colab-Token".into(), token)],
                Vec::new(),
                None,
            )
            .map_err(transport_failure)?;
        if response.status >= 400 {
            return Err(protocol::classify(response.status, &response.body));
        }
        let value = protocol::parse_json(&response.body).map_err(error_failure)?;
        protocol::parse_assignment(&value).ok_or_else(|| {
            error_failure(Error::new(ErrorKind::Parse).detail("tunnel assign had no endpoint"))
        })
    }

    fn tunnel_unassign(
        &self,
        bearer: &str,
        endpoint: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<(), ApiFailure> {
        if endpoint.is_empty() {
            return Err(error_failure(
                Error::new(ErrorKind::SessionGone).detail("runtime has no endpoint"),
            ));
        }
        let url = format!(
            "{}?authuser=0",
            protocol::join(
                &self.endpoints.tunnel,
                &format!("/tun/m/unassign/{}", protocol::encode(endpoint))
            )
        );
        let probe = self.call("GET", &url, Some(bearer), Vec::new(), None, cancel)?;
        let token = protocol::xsrf_token(&probe.headers, &probe.body).unwrap_or_default();
        let response = self
            .send_once(
                "POST",
                &url,
                Some(bearer),
                &[("X-Goog-Colab-Token".into(), token)],
                Vec::new(),
                None,
            )
            .map_err(transport_failure)?;
        if response.status >= 400 && response.status != 404 {
            return Err(protocol::classify(response.status, &response.body));
        }
        Ok(())
    }

    pub fn contents(
        &self,
        base: &str,
        proxy_token: &str,
        method: &str,
        remote: &str,
        body: Option<Value>,
        cancel: &CancelToken,
    ) -> std::result::Result<HttpResponse, ApiFailure> {
        let _ = cancel;
        let path = remote.trim_start_matches('/');
        let url = format!(
            "{}/api/contents/{}?authuser=0&colab-runtime-proxy-token={}",
            base.trim_end_matches('/'),
            encode_path(path),
            protocol::encode(proxy_token)
        );
        let raw = body
            .as_ref()
            .map(|value| serde_json::to_vec(value).unwrap_or_default())
            .unwrap_or_default();
        let content_type = body.as_ref().map(|_| "application/json");
        let extra = [("X-Colab-Runtime-Proxy-Token".into(), proxy_token.to_owned())];
        let response = self
            .send_once(method, &url, None, &extra, raw, content_type)
            .map_err(transport_failure)?;
        if response.status >= 400 {
            return Err(protocol::classify(response.status, &response.body));
        }
        Ok(response)
    }

    pub fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<TokenBundle, ApiFailure> {
        let form = [
            protocol::form_pair("grant_type", "authorization_code"),
            protocol::form_pair("code", code),
            protocol::form_pair("client_id", client_id),
            protocol::form_pair("client_secret", client_secret),
            protocol::form_pair("redirect_uri", protocol::REDIRECT_URI),
        ]
        .join("&");
        self.token_call(form, cancel)
    }

    pub fn refresh(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<TokenBundle, ApiFailure> {
        let form = [
            protocol::form_pair("grant_type", "refresh_token"),
            protocol::form_pair("refresh_token", refresh_token),
            protocol::form_pair("client_id", client_id),
            protocol::form_pair("client_secret", client_secret),
        ]
        .join("&");
        let mut bundle = self.token_call(form, cancel)?;
        if bundle.refresh_token.is_empty() {
            bundle.refresh_token = refresh_token.to_owned();
        }
        Ok(bundle)
    }

    fn token_call(
        &self,
        form: String,
        cancel: &CancelToken,
    ) -> std::result::Result<TokenBundle, ApiFailure> {
        let response = self.call(
            "POST",
            &self.endpoints.token,
            None,
            form.into_bytes(),
            Some("application/x-www-form-urlencoded"),
            cancel,
        )?;
        let value = protocol::parse_json(&response.body).map_err(error_failure)?;
        let access = value
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        if access.is_empty() {
            return Err(error_failure(
                Error::new(ErrorKind::LoginVerificationFailed)
                    .detail("token response had no access token"),
            ));
        }
        let refresh = value
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let expires_in = value
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);
        Ok(TokenBundle {
            access_token: access,
            refresh_token: refresh,
            expires_in,
        })
    }

    pub fn userinfo(
        &self,
        access: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<String, ApiFailure> {
        let response = self.call(
            "GET",
            &self.endpoints.userinfo,
            Some(access),
            Vec::new(),
            None,
            cancel,
        )?;
        let value = protocol::parse_json(&response.body).map_err(error_failure)?;
        value
            .get("email")
            .and_then(|v| v.as_str())
            .filter(|email| !email.is_empty())
            .map(|email| email.to_owned())
            .ok_or_else(|| {
                error_failure(
                    Error::new(ErrorKind::LoginVerificationFailed).detail("userinfo had no email"),
                )
            })
    }

    pub fn kernel_http(
        &self,
        base: &str,
        proxy_token: &str,
        path: &str,
        method: &str,
        cancel: &CancelToken,
    ) -> std::result::Result<(), ApiFailure> {
        let _ = cancel;
        let url = format!(
            "{base}{path}?colab-runtime-proxy-token={}",
            protocol::encode(proxy_token)
        );
        let response = self
            .send_once(
                method,
                &url,
                None,
                &[("X-Colab-Runtime-Proxy-Token".into(), proxy_token.to_owned())],
                Vec::new(),
                None,
            )
            .map_err(transport_failure)?;
        if response.status >= 400 {
            Err(protocol::classify(response.status, &response.body))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone)]
pub struct TokenBundle {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

pub fn runtime_suffix() -> String {
    let first = crate::accounts::nonce();
    let second = crate::accounts::nonce();
    let mixed = format!("{first}{second}");
    format!(
        "{}-{}-{}-{}-{}",
        &mixed[0..8],
        &mixed[8..12],
        &mixed[12..16],
        &mixed[16..20],
        &mixed[20..32]
    )
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(protocol::encode)
        .collect::<Vec<_>>()
        .join("/")
}

fn cancelled_failure() -> ApiFailure {
    error_failure(Error::cancelled())
}

fn error_failure(error: Error) -> ApiFailure {
    ApiFailure {
        kind: error.kind,
        retryable: false,
        auth_denied: false,
        already_exists: false,
        status: 0,
        reason: None,
        limit: None,
        detail: error.detail,
    }
}

fn transport_failure(error: Error) -> ApiFailure {
    error_failure(error)
}
