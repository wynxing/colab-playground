//! DirectBackend against the in-process Colab mock. No live Google calls.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use crate::backend::{Backend, RuntimeRef, StopOptions};
use crate::clock::{CancelToken, Clock, ManualClock};
use crate::defaults::Defaults;
use crate::error::ErrorKind;
use crate::provision::ProvisionSpec;
use crate::sessions::Accelerator;
use crate::tunnel::TunnelSpec;

use super::mock_colab::{self, MockColab, PROXY_TOKEN};
use super::protocol::{ConnectionInfo, Endpoints};
use super::transport::{AutoHttp, HttpExchange, StdWs, WsDial};
use super::vault::{self, FailVault, MemoryVault, TokenVault};
use super::{DirectBackend, DirectConfig, OAuthClient};

const CLIENT_SECRET: &str = "client-secret-zzz";
const ACCESS: &str = "access-aaa";
const REFRESH: &str = "refresh-rrr";
const PUBKEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKeyOnly";

struct Lab {
    mock: MockColab,
    backend: DirectBackend,
    dir: tempfile::TempDir,
    vault: Arc<MemoryVault>,
}

fn lab() -> Lab {
    let mock = MockColab::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let vault = Arc::new(MemoryVault::new());
    let backend = open_backend(&mock.origin, dir.path(), vault.clone(), PUBKEY);
    Lab {
        mock,
        backend,
        dir,
        vault,
    }
}

fn open_backend(
    origin: &str,
    dir: &Path,
    vault: Arc<dyn TokenVault>,
    pubkey: &str,
) -> DirectBackend {
    open_configured(origin, dir, vault, pubkey, StdWs::default(), None, None)
}

fn open_configured(
    origin: &str,
    dir: &Path,
    vault: Arc<dyn TokenVault>,
    pubkey: &str,
    ws: StdWs,
    ssh_identity: Option<PathBuf>,
    ssh_user: Option<String>,
) -> DirectBackend {
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::fixed());
    let http: Arc<dyn HttpExchange> = Arc::new(AutoHttp);
    let ws: Arc<dyn WsDial> = Arc::new(ws);
    DirectBackend::open(DirectConfig {
        clock,
        http,
        ws,
        vault,
        data_dir: Some(dir.to_owned()),
        endpoints: Endpoints::at(origin),
        oauth: Some(OAuthClient {
            client_id: "test-client".into(),
            client_secret: CLIENT_SECRET.into(),
        }),
        retry_delays: vec![Duration::ZERO, Duration::ZERO, Duration::ZERO],
        upload_chunk_bytes: 4,
        ssh_pubkey: Some(pubkey.to_owned()),
        ssh_identity,
        ssh_user,
        adc_path: None,
        defaults: Defaults::default(),
    })
    .expect("open direct backend")
}

fn ed25519_keypair(dir: &Path) -> (PathBuf, String) {
    let private = dir.join("id_ed25519");
    let status = Command::new("/usr/bin/ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(&private)
        .args(["-N", "", "-q"])
        .status()
        .expect("ssh-keygen");
    assert!(status.success(), "ssh-keygen failed");
    let public = fs::read_to_string(format!("{}.pub", private.display()))
        .expect("public key")
        .trim()
        .to_owned();
    (private, public)
}

fn spec(name: &str, accelerator: Accelerator, attempts: u32) -> ProvisionSpec {
    let mut spec = ProvisionSpec::new(name, accelerator, &Defaults::default());
    spec.max_attempts = attempts;
    spec.retry_delay = Duration::ZERO;
    spec
}

fn events() -> mpsc::Sender<crate::provision::ProvisionEvent> {
    let (tx, _rx) = mpsc::channel();
    tx
}

fn disk_text(dir: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = fs::read_to_string(&path) {
                out.push_str(&text);
            }
        }
    }
    out
}

fn assert_secrets_absent(lab: &Lab, extra: &[&str]) {
    let calls = lab.backend.recent_calls().join("\n");
    let files = disk_text(lab.dir.path());
    let debug = format!("{:?}", lab.backend);
    for secret in [PROXY_TOKEN, CLIENT_SECRET, ACCESS, REFRESH]
        .into_iter()
        .chain(extra.iter().copied())
    {
        assert!(!calls.contains(secret), "call log contains a secret");
        assert!(!files.contains(secret), "data dir contains a secret");
        assert!(!debug.contains(secret), "debug contains a secret");
    }
}

#[test]
fn oauth_debug_redacts_the_client_secret() {
    let shown = format!(
        "{:?}",
        OAuthClient {
            client_id: "test-client".into(),
            client_secret: CLIENT_SECRET.into(),
        }
    );
    assert!(shown.contains("test-client"));
    assert!(!shown.contains(CLIENT_SECRET));
    let connection = format!(
        "{:?}",
        ConnectionInfo {
            url: "http://proxy.example/secret".into(),
            token: PROXY_TOKEN.into(),
            expire_time: None,
            resource: "runtimes/r-1".into(),
            endpoint: "ep-1".into(),
        }
    );
    assert!(!connection.contains(PROXY_TOKEN));
    assert!(!connection.contains("proxy.example"));
}

#[test]
fn public_create_probes_gpu_transfers_files_and_stops() {
    let lab = lab();
    lab.mock.set_defer_lro(true);
    let cancel = CancelToken::new();
    lab.backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .expect("seed");
    let outcome = lab
        .backend
        .provision(spec("a100-1", Accelerator::A100, 3), events(), &cancel)
        .expect("provision");
    assert_eq!(outcome.hardware.vram_mib, Some(81920));
    assert_eq!(outcome.hardware.gpu.as_deref(), Some("NVIDIA A100"));
    assert!(outcome.watchdog.is_some_and(|watchdog| watchdog.armed));
    assert_eq!(lab.mock.count("POST", "/v1beta/runtimes"), 1);
    assert_eq!(lab.mock.count("GET", "/v1/operations/op-r-1:wait"), 1);
    let codes = lab.mock.kernel_codes();
    assert!(codes.iter().any(|code| code.contains("nvidia-smi")));
    assert!(codes.iter().any(|code| code.contains("TBE_RUNTIME_ADDR")));
    assert!(codes.iter().all(|code| !code.contains(PROXY_TOKEN)));

    let local = lab.dir.path().join("upload.bin");
    fs::write(&local, b"0123456789").unwrap();
    let target = RuntimeRef {
        account: lab.backend.accounts(&cancel).unwrap().active.unwrap(),
        endpoint: String::new(),
        name: Some("a100-1".into()),
    };
    lab.backend
        .vm_upload(&target, local.to_str().unwrap(), "tmp/hello.txt", &cancel)
        .expect("upload");
    assert_eq!(lab.mock.chunks(), vec![1, 2, -1]);
    assert_eq!(
        lab.mock.file("tmp/hello.txt").as_deref(),
        Some(b"0123456789".as_slice())
    );
    assert!(lab.mock.partial("tmp/hello.txt.partial").is_none());
    let downloaded = lab.dir.path().join("down.bin");
    lab.backend
        .vm_download(
            &target,
            "tmp/hello.txt",
            downloaded.to_str().unwrap(),
            &cancel,
        )
        .expect("download");
    assert_eq!(fs::read(downloaded).unwrap(), b"0123456789");

    let usage = lab.backend.refresh_usage(&[], &cancel).expect("usage");
    assert_eq!(
        usage[0].snapshot.as_ref().map(|item| item.balance_cu),
        Some(12.5)
    );

    lab.backend
        .stop_session(&target, StopOptions::default(), &cancel)
        .expect("stop");
    assert_eq!(lab.mock.count("DELETE", "/v1beta/runtimes/r-1"), 1);
    assert_secrets_absent(&lab, &[]);
}

#[test]
fn denied_public_api_falls_back_to_the_tunnel() {
    let lab = lab();
    lab.mock.set_public_denied(true);
    let cancel = CancelToken::new();
    lab.backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .unwrap();
    let outcome = lab
        .backend
        .provision(spec("a100-2", Accelerator::A100, 1), events(), &cancel)
        .expect("tunnel provision");
    assert_eq!(outcome.hardware.gpu.as_deref(), Some("NVIDIA A100"));
    assert!(lab.mock.count("GET", "/tun/m/assign") >= 1);
    assert!(lab.mock.count("POST", "/tun/m/assign") >= 1);
    assert_eq!(lab.mock.count("POST", "/v1beta/runtimes"), 1);
    let posts = lab.mock.hits();
    assert!(
        posts
            .iter()
            .any(|hit| hit.path == "/v1beta/runtimes" && hit.status == 403)
    );
}

#[test]
fn ineligible_shape_is_refused_before_create() {
    let lab = lab();
    lab.mock.set_ineligible(Accelerator::H100);
    let cancel = CancelToken::new();
    lab.backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .unwrap();
    let catalog = lab.backend.hardware_catalog(&cancel).unwrap();
    assert!(catalog.blocks(Accelerator::H100, false));
    assert!(!catalog.blocks(Accelerator::A100, false));
    let error = lab
        .backend
        .provision(spec("h100-1", Accelerator::H100, 1), events(), &cancel)
        .expect_err("ineligible");
    assert_eq!(error.kind, ErrorKind::NotEntitled);
    assert_eq!(lab.mock.count("POST", "/v1beta/runtimes"), 0);
}

#[test]
fn resource_exhausted_is_retried_until_the_runtime_exists() {
    let lab = lab();
    lab.mock.set_exhausted(2);
    let cancel = CancelToken::new();
    lab.backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .unwrap();
    lab.backend
        .provision(spec("a100-3", Accelerator::A100, 1), events(), &cancel)
        .expect("retried create");
    assert_eq!(lab.mock.count("POST", "/v1beta/runtimes"), 3);
}

#[test]
fn terminal_create_errors_are_not_retried() {
    let cases = [
        ("denylisted", "DENYLISTED", ErrorKind::NotEntitled, false),
        (
            "precondition",
            "FAILED_PRECONDITION",
            ErrorKind::CapacityRefused,
            false,
        ),
        (
            "too-many",
            "TOO_MANY_ACTIVE_RUNTIMES",
            ErrorKind::SessionLimitReached,
            false,
        ),
    ];
    for (name, reason, kind, quota) in cases {
        let lab = lab();
        if quota {
            lab.mock.set_quota_http(true);
        } else {
            lab.mock.set_create_reason(reason);
        }
        let cancel = CancelToken::new();
        lab.backend
            .seed_account("ada@example.com", ACCESS, REFRESH, true)
            .unwrap();
        let error = lab
            .backend
            .provision(spec(name, Accelerator::A100, 1), events(), &cancel)
            .expect_err(reason);
        assert_eq!(error.kind, kind, "{reason}");
        assert_eq!(lab.mock.count("POST", "/v1beta/runtimes"), 1, "{reason}");
    }

    let lab = lab();
    lab.mock.set_quota_http(true);
    let cancel = CancelToken::new();
    lab.backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .unwrap();
    let error = lab
        .backend
        .provision(spec("quota-1", Accelerator::A100, 1), events(), &cancel)
        .expect_err("quota");
    assert_eq!(error.kind, ErrorKind::CapacityRefused);
    assert_eq!(
        error.params.get("reason").map(String::as_str),
        Some("QUOTA_EXCEEDED_USAGE_TIME")
    );
    assert_eq!(lab.mock.count("POST", "/v1beta/runtimes"), 1);
}

#[test]
fn already_exists_adopts_the_listed_runtime() {
    let lab = lab();
    lab.mock.set_already_exists(true);
    let cancel = CancelToken::new();
    lab.backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .unwrap();
    lab.mock.insert_runtime(
        ACCESS,
        "r-existing",
        "ep-existing",
        Accelerator::A100,
        PROXY_TOKEN,
    );
    let outcome = lab
        .backend
        .provision(spec("a100-4", Accelerator::A100, 1), events(), &cancel)
        .expect("adopt");
    assert_eq!(outcome.hardware.gpu.as_deref(), Some("NVIDIA A100"));
    assert_eq!(lab.mock.count("POST", "/v1beta/runtimes"), 1);
    assert!(lab.mock.count("GET", "/v1beta/runtimes") >= 1);
}

#[test]
fn another_accounts_runtime_can_be_extended_forwarded_and_stopped() {
    let mock = MockColab::start();
    assert_ne!(
        mock.sshd_port(),
        0,
        "local sshd did not start: {}",
        mock.sshd_error()
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let (identity, pubkey) = ed25519_keypair(dir.path());
    let vault = Arc::new(MemoryVault::new());
    let backend = open_configured(
        &mock.origin,
        dir.path(),
        vault,
        &pubkey,
        StdWs::default(),
        Some(identity),
        Some(mock_colab::login_user()),
    );
    let cancel = CancelToken::new();
    let active = backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .unwrap();
    let other = backend
        .seed_account("bea@example.com", "access-bbb", "refresh-bbb", false)
        .unwrap();
    mock.insert_runtime(
        "access-bbb",
        "r-b",
        "ep-b",
        Accelerator::A100,
        "proxy-b-secret",
    );
    backend
        .remember_runtime(
            &other,
            "bee-1",
            "runtimes/r-b",
            "ep-b",
            Accelerator::A100,
            "proxy-b-secret",
            &mock.origin,
        )
        .unwrap();
    let target = RuntimeRef {
        account: other.clone(),
        endpoint: "ep-b".into(),
        name: Some("bee-1".into()),
    };
    let extension = backend
        .extend_session(&target, Duration::from_secs(90), &cancel)
        .expect("extend");
    assert!(extension.watchdog.armed);
    assert!(
        mock.kernel_codes()
            .iter()
            .all(|code| !code.contains("proxy-b-secret"))
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("vm port");
    let remote_port = listener.local_addr().unwrap().port();
    let (got_tx, got_rx) = mpsc::channel();
    thread::spawn(move || {
        // The port check opens one empty connection. Keep accepting until the marker arrives.
        let marker = b"to-the-vm-port";
        loop {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 64];
            while buf.len() < marker.len() {
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
            }
            if buf.is_empty() {
                continue;
            }
            let _ = sock.write_all(b"vm-reply-bytes");
            let _ = got_tx.send(buf);
            return;
        }
    });
    let local_port = free_port();
    let (tx, _rx) = mpsc::channel();
    backend
        .open_tunnel(
            TunnelSpec {
                account: other.clone(),
                session: "bee-1".into(),
                local_port,
                remote_port,
                health_path: None,
            },
            tx,
        )
        .expect("tunnel");
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", local_port)).expect("local forward");
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    stream.write_all(b"to-the-vm-port").unwrap();
    let mut reply = [0u8; 32];
    let n = stream.read(&mut reply).expect("reply from the vm port");
    assert_eq!(&reply[..n], b"vm-reply-bytes");
    let arrived = got_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("vm got bytes");
    assert_eq!(arrived, b"to-the-vm-port");

    backend
        .stop_session(&target, StopOptions::default(), &cancel)
        .expect("stop other");
    let delete = mock
        .hits()
        .into_iter()
        .find(|hit| hit.method == "DELETE" && hit.path == "/v1beta/runtimes/r-b")
        .expect("delete");
    assert_eq!(delete.bearer, "access-bbb");
    assert_eq!(backend.accounts(&cancel).unwrap().active, Some(active));
    let calls = backend.recent_calls().join("\n");
    let files = disk_text(dir.path());
    for secret in ["access-bbb", "refresh-bbb", "proxy-b-secret", PROXY_TOKEN] {
        assert!(!calls.contains(secret), "call log contains a secret");
        assert!(!files.contains(secret), "data dir contains a secret");
        assert!(!format!("{backend:?}").contains(secret));
    }
}

#[test]
fn listed_runtime_without_a_ledger_row_can_be_stopped() {
    let lab = lab();
    let cancel = CancelToken::new();
    let active = lab
        .backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .unwrap();
    let other = lab
        .backend
        .seed_account("bea@example.com", "access-bbb", "refresh-bbb", false)
        .unwrap();
    lab.mock.insert_runtime(
        "access-bbb",
        "r-listed",
        "ep-listed",
        Accelerator::None,
        "proxy-b-secret",
    );
    let view = lab.backend.runtimes(&cancel).expect("list");
    assert!(view.runtimes.iter().any(|runtime| {
        runtime.id.endpoint == "ep-listed"
            && runtime.id.account == other
            && runtime.id.name.is_none()
    }));
    lab.backend
        .stop_session(
            &RuntimeRef {
                account: other,
                endpoint: "ep-listed".into(),
                name: None,
            },
            StopOptions::default(),
            &cancel,
        )
        .expect("stop from the list");
    let delete = lab
        .mock
        .hits()
        .into_iter()
        .find(|hit| hit.method == "DELETE" && hit.path == "/v1beta/runtimes/r-listed")
        .expect("delete");
    assert_eq!(delete.bearer, "access-bbb");
    assert_eq!(lab.backend.accounts(&cancel).unwrap().active, Some(active));
    assert_secrets_absent(&lab, &["access-bbb", "refresh-bbb", "proxy-b-secret"]);
}

#[test]
fn https_proxy_kernel_handshake_reaches_the_runtime() {
    let api = MockColab::start();
    let tls = MockColab::start_tls();
    let dir = tempfile::tempdir().expect("tempdir");
    let vault = Arc::new(MemoryVault::new());
    let backend = open_configured(
        &api.origin,
        dir.path(),
        vault,
        PUBKEY,
        StdWs {
            extra_roots: vec![tls.cert_der()],
        },
        None,
        None,
    );
    let cancel = CancelToken::new();
    let account = backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .unwrap();
    backend
        .remember_runtime(
            &account,
            "tls-1",
            "runtimes/r-tls",
            "ep-tls",
            Accelerator::None,
            PROXY_TOKEN,
            &tls.origin,
        )
        .unwrap();
    let output = backend
        .execute_code(
            &RuntimeRef {
                account,
                endpoint: "ep-tls".into(),
                name: Some("tls-1".into()),
            },
            "print('wss-handshake')",
            &cancel,
        )
        .expect("wss kernel");
    assert!(
        tls.kernel_codes()
            .iter()
            .any(|code| code.contains("wss-handshake")),
        "cell did not reach the https proxy"
    );
    assert!(!output.contains(PROXY_TOKEN));
    let calls = backend.recent_calls().join("\n");
    let files = disk_text(dir.path());
    assert!(!calls.contains(PROXY_TOKEN), "call log contains a secret");
    assert!(!files.contains(PROXY_TOKEN), "data dir contains a secret");
    assert!(!format!("{backend:?}").contains(PROXY_TOKEN));
}

#[test]
fn ssh_busy_is_not_retried_and_rsa_is_refused_locally() {
    let lab = lab();
    lab.mock.set_ssh_status(429, 2);
    let account = lab
        .backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .unwrap();
    lab.backend
        .remember_runtime(
            &account,
            "ssh-1",
            "runtimes/r-ssh",
            "ep-ssh",
            Accelerator::None,
            PROXY_TOKEN,
            &lab.mock.origin,
        )
        .unwrap();
    let (tx, _rx) = mpsc::channel();
    let error = lab
        .backend
        .open_tunnel(
            TunnelSpec {
                account: account.clone(),
                session: "ssh-1".into(),
                local_port: free_port(),
                remote_port: 2222,
                health_path: None,
            },
            tx,
        )
        .expect_err("busy");
    assert_eq!(error.kind, ErrorKind::SshBusy);
    assert_eq!(error.params.get("limit").map(String::as_str), Some("2"));
    let ssh = lab
        .mock
        .hits()
        .into_iter()
        .find(|hit| hit.path == "/colab/ssh")
        .expect("ssh hit");
    assert_eq!(ssh.status, 429);
    assert!(ssh.has_proxy_header);
    assert!(ssh.pubkey.starts_with("ssh-ed25519 "));

    let mock = MockColab::start();
    let dir = tempfile::tempdir().unwrap();
    let vault = Arc::new(MemoryVault::new());
    let backend = open_backend(
        &mock.origin,
        dir.path(),
        vault,
        "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCTest",
    );
    let account = backend
        .seed_account("ada@example.com", ACCESS, REFRESH, true)
        .unwrap();
    backend
        .remember_runtime(
            &account,
            "ssh-2",
            "runtimes/r-2",
            "ep-2",
            Accelerator::None,
            PROXY_TOKEN,
            &mock.origin,
        )
        .unwrap();
    let (tx, _rx) = mpsc::channel();
    let error = backend
        .open_tunnel(
            TunnelSpec {
                account,
                session: "ssh-2".into(),
                local_port: free_port(),
                remote_port: 2222,
                health_path: None,
            },
            tx,
        )
        .expect_err("rsa");
    assert_eq!(error.kind, ErrorKind::InvalidInput);
    assert_eq!(mock.count("GET", "/colab/ssh"), 0);
}

#[test]
fn login_stores_only_the_refresh_token_in_the_vault() {
    let lab = lab();
    let cancel = CancelToken::new();
    let flow = lab.backend.begin_add_account(&cancel).unwrap();
    assert!(flow.auth_url().contains("test-client"));
    assert!(!flow.auth_url().contains(CLIENT_SECRET));
    let account = flow.submit_code("paste-code", &cancel).expect("login");
    assert_eq!(account.email.as_deref(), Some("ada@example.com"));
    let stored = lab
        .vault
        .lookup(&vault::account_item(&account.id))
        .unwrap()
        .expect("vault item");
    assert!(stored.contains(REFRESH));
    assert!(!stored.contains(ACCESS));
    assert!(!stored.contains(CLIENT_SECRET));
    assert_secrets_absent(&lab, &[]);
}

#[test]
fn a_locked_vault_does_not_save_the_account() {
    let mock = MockColab::start();
    let dir = tempfile::tempdir().unwrap();
    let backend = open_backend(&mock.origin, dir.path(), Arc::new(FailVault), PUBKEY);
    let cancel = CancelToken::new();
    let flow = backend.begin_add_account(&cancel).unwrap();
    let error = flow.submit_code("paste-code", &cancel).expect_err("vault");
    assert_eq!(error.kind, ErrorKind::KeyringUnavailable);
    assert!(backend.accounts(&cancel).unwrap().accounts.is_empty());
    let files = disk_text(dir.path());
    assert!(!files.contains(ACCESS));
    assert!(!files.contains(REFRESH));
    assert!(!files.contains(CLIENT_SECRET));
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}
