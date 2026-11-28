//! One local TCP server for the Colab calls the direct backend makes.
//! Nothing here contacts Google.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::sessions::Accelerator;

use super::transport::{self, read_frame, write_frame};

pub const PROXY_TOKEN: &str = "proxy-secret-token";

#[derive(Debug, Clone)]
pub struct Hit {
    pub method: String,
    pub path: String,
    pub bearer: String,
    pub has_proxy_header: bool,
    pub pubkey: String,
    pub status: u16,
}

#[derive(Clone)]
struct Vm {
    id: String,
    endpoint: String,
    accelerator: Accelerator,
    high_mem: bool,
    owner: String,
    token: String,
}

struct World {
    origin: String,
    public_denied: bool,
    tier: String,
    offers: Vec<(Accelerator, bool, bool)>,
    runtimes: Vec<Vm>,
    exhausted_left: u32,
    already_exists: bool,
    create_reason: Option<String>,
    quota_http: bool,
    defer_lro: bool,
    pending: BTreeMap<String, Value>,
    seq: u64,
    files: BTreeMap<String, Vec<u8>>,
    partials: BTreeMap<String, Vec<u8>>,
    chunks: Vec<i64>,
    ssh_status: u16,
    ssh_limit: u32,
    email: String,
    hits: Vec<Hit>,
    kernel_codes: Vec<String>,
}

impl World {
    fn new(origin: String) -> Self {
        World {
            origin,
            public_denied: false,
            tier: "SUBSCRIPTION_TIER_PRO".into(),
            offers: Accelerator::ALL
                .into_iter()
                .map(|accelerator| (accelerator, false, true))
                .collect(),
            runtimes: Vec::new(),
            exhausted_left: 0,
            already_exists: false,
            create_reason: None,
            quota_http: false,
            defer_lro: false,
            pending: BTreeMap::new(),
            seq: 0,
            files: BTreeMap::new(),
            partials: BTreeMap::new(),
            chunks: Vec::new(),
            ssh_status: 101,
            ssh_limit: 2,
            email: "ada@example.com".into(),
            hits: Vec::new(),
            kernel_codes: Vec::new(),
        }
    }

    fn note(&mut self, req: &Incoming, status: u16) {
        self.hits.push(Hit {
            method: req.method.clone(),
            path: req.path.clone(),
            bearer: req.bearer(),
            has_proxy_header: req.header("x-colab-runtime-proxy-token").is_some(),
            pubkey: req.header("x-colab-ssh-pubkey").unwrap_or("").to_owned(),
            status,
        });
    }

    fn count(&self, method: &str, path: &str) -> usize {
        self.hits
            .iter()
            .filter(|hit| hit.method == method && hit.path == path)
            .count()
    }

    fn push_vm(
        &mut self,
        owner: &str,
        accelerator: Accelerator,
        high_mem: bool,
        token: &str,
    ) -> Vm {
        self.seq += 1;
        let vm = Vm {
            id: format!("r-{}", self.seq),
            endpoint: format!("ep-{}", self.seq),
            accelerator,
            high_mem,
            owner: owner.to_owned(),
            token: token.to_owned(),
        };
        self.runtimes.push(vm.clone());
        vm
    }
}

/// Local stand-in for the public API, the tunnel, the runtime proxy, and the kernel socket.
pub struct MockColab {
    pub origin: String,
    world: Arc<Mutex<World>>,
}

impl MockColab {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock colab");
        let port = listener.local_addr().expect("mock port").port();
        let origin = format!("http://127.0.0.1:{port}");
        let world = Arc::new(Mutex::new(World::new(origin.clone())));
        let shared = world.clone();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { break };
                let world = shared.clone();
                thread::spawn(move || serve(stream, world));
            }
        });
        MockColab { origin, world }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn set_public_denied(&self, denied: bool) {
        self.lock().public_denied = denied;
    }

    pub fn set_exhausted(&self, left: u32) {
        self.lock().exhausted_left = left;
    }

    pub fn set_already_exists(&self, yes: bool) {
        self.lock().already_exists = yes;
    }

    pub fn set_create_reason(&self, reason: &str) {
        self.lock().create_reason = Some(reason.to_owned());
    }

    pub fn set_quota_http(&self, yes: bool) {
        self.lock().quota_http = yes;
    }

    pub fn set_defer_lro(&self, yes: bool) {
        self.lock().defer_lro = yes;
    }

    pub fn set_ineligible(&self, accelerator: Accelerator) {
        for offer in &mut self.lock().offers {
            if offer.0 == accelerator {
                offer.2 = false;
            }
        }
    }

    pub fn set_ssh_status(&self, status: u16, limit: u32) {
        let mut world = self.lock();
        world.ssh_status = status;
        world.ssh_limit = limit;
    }

    pub fn insert_runtime(
        &self,
        owner: &str,
        id: &str,
        endpoint: &str,
        accelerator: Accelerator,
        token: &str,
    ) {
        self.lock().runtimes.push(Vm {
            id: id.to_owned(),
            endpoint: endpoint.to_owned(),
            accelerator,
            high_mem: false,
            owner: owner.to_owned(),
            token: token.to_owned(),
        });
    }

    pub fn count(&self, method: &str, path: &str) -> usize {
        self.lock().count(method, path)
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.lock().hits.clone()
    }

    pub fn kernel_codes(&self) -> Vec<String> {
        self.lock().kernel_codes.clone()
    }

    pub fn chunks(&self) -> Vec<i64> {
        self.lock().chunks.clone()
    }

    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.lock().files.get(path).cloned()
    }

    pub fn partial(&self, path: &str) -> Option<Vec<u8>> {
        self.lock().partials.get(path).cloned()
    }
}

struct Incoming {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    pending: Vec<u8>,
    stream: TcpStream,
}

impl Incoming {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
    }

    fn bearer(&self) -> String {
        self.header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_owned()
    }

    fn query_param(&self, name: &str) -> Option<String> {
        for pair in self.query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            if key == name {
                return Some(decode_component(value));
            }
        }
        None
    }
}

fn serve(stream: TcpStream, world: Arc<Mutex<World>>) {
    let _ = stream.set_nodelay(true);
    let Ok(mut req) = read_incoming(stream) else {
        return;
    };
    let upgrade = req
        .header("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    if upgrade {
        let action = {
            let mut world = world.lock().unwrap_or_else(|error| error.into_inner());
            ws_action(&mut world, &req)
        };
        match action {
            WsAction::Reject(reply) => {
                let _ = write_reply(&mut req.stream, &reply);
            }
            WsAction::Kernel => kernel_session(&mut req, &world),
            WsAction::Ssh => ssh_session(&mut req),
        }
        return;
    }
    let reply = {
        let mut world = world.lock().unwrap_or_else(|error| error.into_inner());
        route(&mut world, &req)
    };
    let _ = write_reply(&mut req.stream, &reply);
}

enum WsAction {
    Reject(Reply),
    Kernel,
    Ssh,
}

fn ws_action(world: &mut World, req: &Incoming) -> WsAction {
    if req.path == "/colab/ssh" {
        let pubkey = req.header("x-colab-ssh-pubkey").unwrap_or("");
        if pubkey.starts_with("ssh-rsa ") || world.ssh_status != 101 {
            let status = if pubkey.starts_with("ssh-rsa ") {
                400
            } else {
                world.ssh_status
            };
            let body = if status == 429 {
                json!({"error": "already-active-session", "limit": world.ssh_limit}).to_string()
            } else {
                format!("http {status}")
            };
            world.note(req, status);
            return WsAction::Reject(Reply {
                status,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: body.into_bytes(),
            });
        }
        world.note(req, 101);
        return WsAction::Ssh;
    }
    world.note(req, 101);
    WsAction::Kernel
}

fn route(world: &mut World, req: &Incoming) -> Reply {
    let bearer = req.bearer();
    if world.public_denied && (req.path.starts_with("/v1beta/") || req.path.starts_with("/v1/")) {
        world.note(req, 403);
        return json_reply(
            403,
            br#"{"error":{"code":7,"status":"PERMISSION_DENIED","message":"denied"}}"#.to_vec(),
        );
    }
    let reply = dispatch(world, req, &bearer);
    world.note(req, reply.status);
    reply
}

fn dispatch(world: &mut World, req: &Incoming, bearer: &str) -> Reply {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1beta/subscription") => {
            json_reply(200, json!({"subscriptionTier": world.tier}).to_string().into_bytes())
        }
        ("GET", "/v1beta/runtimespecs") => json_reply(200, specs_body(world).into_bytes()),
        ("GET", "/v1beta/runtimes") => json_reply(200, list_body(world, bearer).into_bytes()),
        ("POST", "/v1beta/runtimes") => create(world, req, bearer),
        ("GET", "/tun/m/ccu-info") => json_reply(200, ccu_body().into_bytes()),
        ("GET", "/tun/m/assignments") => json_reply(200, assignments_body(world, bearer).into_bytes()),
        ("GET", "/tun/m/assign") => Reply {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("X-Goog-Colab-Token".into(), "xsrf-1".into()),
            ],
            body: b"{}".to_vec(),
        },
        ("POST", "/tun/m/assign") => assign(world, req, bearer),
        ("GET", path) if path.starts_with("/tun/m/unassign/") => Reply {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("X-Goog-Colab-Token".into(), "xsrf-1".into()),
            ],
            body: b"{}".to_vec(),
        },
        ("POST", path) if path.starts_with("/tun/m/unassign/") => {
            let endpoint = decode_component(path.trim_start_matches("/tun/m/unassign/"));
            world.runtimes.retain(|vm| vm.endpoint != endpoint);
            json_reply(200, b"{}".to_vec())
        }
        ("POST", "/token") => json_reply(
            200,
            json!({"access_token": "access-aaa", "refresh_token": "refresh-rrr", "expires_in": 3600})
                .to_string()
                .into_bytes(),
        ),
        ("GET", "/userinfo") => {
            json_reply(200, json!({"email": world.email}).to_string().into_bytes())
        }
        ("POST", "/api/kernels/restart") => Reply {
            status: 204,
            headers: Vec::new(),
            body: Vec::new(),
        },
        ("GET", path) if path.starts_with("/v1/operations/") && path.ends_with(":wait") => {
            let name = path.trim_start_matches("/v1/").trim_end_matches(":wait");
            let response = world.pending.get(name).cloned().unwrap_or(Value::Null);
            json_reply(
                200,
                json!({"name": name, "done": true, "response": response})
                    .to_string()
                    .into_bytes(),
            )
        }
        ("DELETE", path) if path.starts_with("/v1beta/runtimes/") => {
            let id = path.trim_start_matches("/v1beta/runtimes/");
            world.runtimes.retain(|vm| !(vm.id == id && vm.owner == bearer));
            json_reply(200, b"{}".to_vec())
        }
        _ if req.path.starts_with("/api/contents/") => contents(world, req),
        _ => json_reply(404, br#"{"error":"not found"}"#.to_vec()),
    }
}

fn create(world: &mut World, req: &Incoming, bearer: &str) -> Reply {
    if world.exhausted_left > 0 {
        world.exhausted_left -= 1;
        return op_error(8, "RESOURCE_EXHAUSTED");
    }
    if world.quota_http {
        return json_reply(
            429,
            json!({
                "error": {
                    "code": 8,
                    "message": "quota",
                    "details": [{"reason": "QUOTA_EXCEEDED_USAGE_TIME"}]
                }
            })
            .to_string()
            .into_bytes(),
        );
    }
    if world.already_exists {
        return op_error(6, "ALREADY_EXISTS");
    }
    if let Some(reason) = world.create_reason.clone() {
        let code = if reason == "TOO_MANY_ACTIVE_RUNTIMES" || reason == "FAILED_PRECONDITION" {
            9
        } else {
            3
        };
        return op_error(code, &reason);
    }
    let value: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let label = value
        .pointer("/runtimeSpec/accelerator")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let accelerator = if label.is_empty() {
        Accelerator::None
    } else {
        Accelerator::from_label(label).unwrap_or(Accelerator::None)
    };
    let shape = value
        .pointer("/runtimeSpec/shape")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let vm = world.push_vm(
        bearer,
        accelerator,
        shape.eq_ignore_ascii_case("SHAPE_HIGHMEM"),
        PROXY_TOKEN,
    );
    let body = runtime_json(world, &vm);
    if world.defer_lro {
        let name = format!("operations/op-{}", vm.id);
        world.pending.insert(name.clone(), body);
        return json_reply(
            200,
            json!({"name": name, "done": false})
                .to_string()
                .into_bytes(),
        );
    }
    json_reply(
        200,
        json!({"name": format!("operations/{}", vm.id), "done": true, "response": body})
            .to_string()
            .into_bytes(),
    )
}

fn assign(world: &mut World, req: &Incoming, bearer: &str) -> Reply {
    let accelerator = req
        .query_param("accelerator")
        .as_deref()
        .and_then(Accelerator::from_label)
        .unwrap_or(Accelerator::A100);
    let vm = world.push_vm(bearer, accelerator, false, PROXY_TOKEN);
    json_reply(200, assignment_json(world, &vm).to_string().into_bytes())
}

fn contents(world: &mut World, req: &Incoming) -> Reply {
    let path = req
        .path
        .trim_start_matches("/api/contents/")
        .trim_start_matches('/');
    let path = path
        .split('/')
        .map(decode_component)
        .collect::<Vec<_>>()
        .join("/");
    match req.method.as_str() {
        "PUT" => {
            let value: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let chunk = value
                .get("chunk")
                .and_then(|item| item.as_i64())
                .unwrap_or(-1);
            let encoded = value
                .get("content")
                .and_then(|item| item.as_str())
                .unwrap_or("");
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
                .unwrap_or_default();
            world.chunks.push(chunk);
            let partial = format!("{path}.partial");
            if chunk == 1 {
                world.partials.insert(partial, bytes);
            } else if chunk > 1 {
                world.partials.entry(partial).or_default().extend(bytes);
            } else {
                let mut data = world.partials.remove(&partial).unwrap_or_default();
                data.extend(bytes);
                world.files.insert(path, data);
            }
            json_reply(200, b"{}".to_vec())
        }
        "DELETE" => {
            world.files.remove(&path);
            json_reply(200, b"{}".to_vec())
        }
        _ => {
            let Some(bytes) = world.files.get(&path) else {
                return json_reply(404, br#"{"error":"missing"}"#.to_vec());
            };
            let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
            json_reply(
                200,
                json!({
                    "name": path,
                    "path": path,
                    "type": "file",
                    "format": "base64",
                    "content": encoded,
                    "size": bytes.len(),
                })
                .to_string()
                .into_bytes(),
            )
        }
    }
}

fn specs_body(world: &World) -> String {
    let specs: Vec<_> = world
        .offers
        .iter()
        .map(|(accelerator, high_mem, eligible)| {
            json!({
                "accelerator": accelerator.as_str(),
                "shape": if *high_mem { "SHAPE_HIGHMEM" } else { "SHAPE_STANDARD" },
                "variant": if accelerator.is_tpu() {
                    "VARIANT_TPU"
                } else if accelerator.is_gpu() {
                    "VARIANT_GPU"
                } else {
                    "VARIANT_CPU"
                },
                "eligible": eligible,
            })
        })
        .collect();
    json!({"runtimeSpecs": specs}).to_string()
}

fn list_body(world: &World, bearer: &str) -> String {
    let runtimes: Vec<_> = world
        .runtimes
        .iter()
        .filter(|vm| vm.owner == bearer)
        .map(|vm| runtime_json(world, vm))
        .collect();
    json!({"runtimes": runtimes}).to_string()
}

fn assignments_body(world: &World, bearer: &str) -> String {
    let assignments: Vec<_> = world
        .runtimes
        .iter()
        .filter(|vm| vm.owner == bearer)
        .map(|vm| assignment_json(world, vm))
        .collect();
    json!({"assignments": assignments}).to_string()
}

fn runtime_json(world: &World, vm: &Vm) -> Value {
    json!({
        "name": format!("runtimes/{}", vm.id),
        "runtimeSpec": {
            "accelerator": vm.accelerator.as_str(),
            "shape": if vm.high_mem { "SHAPE_HIGHMEM" } else { "SHAPE_STANDARD" },
            "variant": if vm.accelerator.is_tpu() {
                "VARIANT_TPU"
            } else if vm.accelerator.is_gpu() {
                "VARIANT_GPU"
            } else {
                "VARIANT_CPU"
            },
        },
        "connectionInfo": {
            "token": vm.token,
            "url": world.origin,
            "endpoint": vm.endpoint,
            "expireTime": "2099-01-01T00:00:00Z",
        }
    })
}

fn assignment_json(world: &World, vm: &Vm) -> Value {
    json!({
        "name": format!("runtimes/{}", vm.id),
        "endpoint": vm.endpoint,
        "accelerator": vm.accelerator.as_str(),
        "variant": if vm.accelerator.is_gpu() { "GPU" } else { "DEFAULT" },
        "shape": "",
        "token": vm.token,
        "url": world.origin,
    })
}

fn ccu_body() -> String {
    ")]}'\n{\"currentBalance\":12.5,\"consumptionRateHourly\":1.0,\"assignmentsCount\":1}".into()
}

fn op_error(code: i64, reason: &str) -> Reply {
    json_reply(
        200,
        json!({
            "name": "operations/fail",
            "done": true,
            "error": {
                "code": code,
                "message": reason,
                "status": reason,
                "details": [{"reason": reason}],
            }
        })
        .to_string()
        .into_bytes(),
    )
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn json_reply(status: u16, body: Vec<u8>) -> Reply {
    Reply {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body,
    }
}

struct Conn {
    stream: TcpStream,
    pending: Vec<u8>,
    pending_pos: usize,
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending_pos < self.pending.len() {
            let n = (self.pending.len() - self.pending_pos).min(buf.len());
            buf[..n].copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
            self.pending_pos += n;
            return Ok(n);
        }
        self.stream.read(buf)
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

fn kernel_session(req: &mut Incoming, world: &Arc<Mutex<World>>) {
    let key = req.header("sec-websocket-key").unwrap_or("").to_owned();
    if write_handshake(&mut req.stream, &key).is_err() {
        return;
    }
    let mut conn = Conn {
        stream: req.stream.try_clone().expect("clone mock socket"),
        pending: std::mem::take(&mut req.pending),
        pending_pos: 0,
    };
    let _ = conn.stream.set_read_timeout(Some(Duration::from_secs(5)));
    loop {
        let Ok((opcode, payload)) = read_frame(&mut conn) else {
            break;
        };
        if opcode == 0x8 {
            break;
        }
        if opcode == 0x9 {
            let _ = write_frame(&mut conn, 0xA, &payload, false);
            continue;
        }
        if opcode != 0x1 {
            continue;
        }
        let value: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
        let kind = value
            .pointer("/header/msg_type")
            .and_then(|item| item.as_str())
            .unwrap_or("");
        if kind != "execute_request" {
            continue;
        }
        let code = value
            .pointer("/content/code")
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .to_owned();
        world
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .kernel_codes
            .push(code.clone());
        let mut text = String::new();
        if code.contains("nvidia-smi") {
            text.push_str("CPG_VRAM 81920\nCPG_GPU NVIDIA A100\n");
        }
        if code.contains("TBE_RUNTIME_ADDR") || code.contains("cpg-watchdog-cancel") {
            text.push_str("CPG_WATCHDOG armed\n");
        }
        let stream_msg = json!({"header": {"msg_type": "stream"}, "content": {"text": text}});
        let reply = json!({"header": {"msg_type": "execute_reply"}, "content": {"status": "ok"}});
        if write_frame(&mut conn, 0x1, stream_msg.to_string().as_bytes(), false).is_err() {
            break;
        }
        let _ = write_frame(&mut conn, 0x1, reply.to_string().as_bytes(), false);
        break;
    }
}

fn ssh_session(req: &mut Incoming) {
    let key = req.header("sec-websocket-key").unwrap_or("").to_owned();
    if write_handshake(&mut req.stream, &key).is_err() {
        return;
    }
    let mut conn = Conn {
        stream: req.stream.try_clone().expect("clone mock socket"),
        pending: std::mem::take(&mut req.pending),
        pending_pos: 0,
    };
    let _ = conn.stream.set_read_timeout(Some(Duration::from_secs(5)));
    loop {
        let Ok((opcode, payload)) = read_frame(&mut conn) else {
            break;
        };
        if opcode == 0x8 {
            break;
        }
        if opcode == 0x9 {
            let _ = write_frame(&mut conn, 0xA, &payload, false);
            continue;
        }
        if write_frame(&mut conn, opcode, &payload, false).is_err() {
            break;
        }
    }
}

fn write_handshake(stream: &mut TcpStream, key: &str) -> std::io::Result<()> {
    let accept = transport::accept_key(key);
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nContent-Length: 0\r\n\r\n"
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

fn read_incoming(mut stream: TcpStream) -> std::io::Result<Incoming> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if buf.len() > 65_536 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers",
            ));
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "headers",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("/");
    let (path, query) = target
        .split_once('?')
        .map(|(path, query)| (path.to_owned(), query.to_owned()))
        .unwrap_or_else(|| (target.to_owned(), String::new()));
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let len = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut rest = buf[header_end + 4..].to_vec();
    while rest.len() < len {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        rest.extend_from_slice(&tmp[..n]);
    }
    let pending = if rest.len() > len {
        rest[len..].to_vec()
    } else {
        Vec::new()
    };
    rest.truncate(len);
    Ok(Incoming {
        method,
        path,
        query,
        headers,
        body: rest,
        pending,
        stream,
    })
}

fn write_reply(stream: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    let reason = match reply.status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reason,
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        head.push_str(name);
        head.push(':');
        head.push(' ');
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&reply.body)?;
    stream.flush()
}

fn decode_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
