//! Where data lives.
//!
//! Two file systems are involved and are kept apart by type:
//!
//! * **App-side** files ([`PathBuf`]): `accounts.json`, the session ledger,
//!   installed plugins. Read and written directly by Rust.
//! * **Host-side** files ([`HostPath`]): the per-account CLI state files and
//!   scratch files, which the `colab` CLI (inside WSL on Windows) must read.
//!   They are only touched through [`crate::Host`] scripts.
//!
//! On Linux and macOS both live on the same disk, but the types still differ
//! so code written on one platform does the right thing on the other.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::accounts::AccountId;

/// A POSIX path on the host where the `colab` CLI runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostPath(pub String);

impl HostPath {
    pub fn new(p: impl Into<String>) -> Self {
        HostPath(p.into())
    }

    pub fn join(&self, seg: &str) -> HostPath {
        if self.0.ends_with('/') {
            HostPath(format!("{}{seg}", self.0))
        } else {
            HostPath(format!("{}/{seg}", self.0))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The parent directory (`/` for top-level paths).
    pub fn parent(&self) -> HostPath {
        match self.0.trim_end_matches('/').rsplit_once('/') {
            Some(("", _)) | None => HostPath("/".into()),
            Some((p, _)) => HostPath(p.to_owned()),
        }
    }
}

impl std::fmt::Display for HostPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataLayout {
    pub local: PathBuf,
    /// Host-side data directory (inside WSL on Windows).
    pub host: HostPath,
}

impl DataLayout {
    pub fn new(local: impl Into<PathBuf>, host: HostPath) -> Self {
        DataLayout {
            local: local.into(),
            host,
        }
    }

    /// `<local>/accounts.json`: non-secret account metadata.
    pub fn accounts_json(&self) -> PathBuf {
        self.local.join("accounts.json")
    }

    /// `<local>/ledger.json`: sessions the app provisioned.
    pub fn ledger_json(&self) -> PathBuf {
        self.local.join("ledger.json")
    }

    /// `<local>/plugins/`: user-installed plugins, one directory per id.
    pub fn plugins_dir(&self) -> PathBuf {
        self.local.join("plugins")
    }

    /// `<local>/plugins.json`: enabled/disabled flags.
    pub fn plugins_json(&self) -> PathBuf {
        self.local.join("plugins.json")
    }

    /// `<local>/staging/`: temporary plugin checkouts.
    pub fn staging_dir(&self) -> PathBuf {
        self.local.join("staging")
    }

    /// `<host>/accounts/<id>/sessions.json`: the account's `--config` state file.
    pub fn account_state(&self, id: &AccountId) -> HostPath {
        self.host
            .join("accounts")
            .join(id.as_str())
            .join("sessions.json")
    }

    /// `<host>/tmp`: scratch space for throw-away state files.
    pub fn host_scratch(&self) -> HostPath {
        self.host.join("tmp")
    }

    /// `<local>/ssh/<account>-<session>.conf`: isolated SSH config written only on explicit request.
    pub fn ssh_config_file(&self, account: &AccountId, session: &str) -> PathBuf {
        self.local
            .join("ssh")
            .join(format!("{}-{session}.conf", account.as_str()))
    }
}

/// The path the `colab` CLI has to be given for a file on this computer.
///
/// The CLI runs inside a Linux namespace on Windows: a Windows path handed to it as-is is one long
/// file name, and the file lands in the CLI's working directory instead of where it was asked for.
/// On Linux and macOS the app and the CLI share a file system, so the path stands as it is. `None`
/// for a path the CLI cannot write at all (a Windows share), which the caller reports rather than
/// writing somewhere no one will look.
pub fn host_path_for_local(local: &Path) -> Option<HostPath> {
    let text = local.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        match text.split_once(':') {
            Some((drive, rest)) if drive.len() == 1 && rest.starts_with('/') => Some(
                HostPath::new(format!("/mnt/{}{rest}", drive.to_ascii_lowercase())),
            ),
            // A share (`//server/...`) or a path with no drive has no place in the CLI's namespace.
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        Some(HostPath::new(text))
    }
}

/// The default app-side data directory (`<OS data dir>/colab-playground`).
/// Only used by the real app; tests always pass explicit directories.
pub fn default_local_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("colab-playground"))
}

/// The directory the app's own log goes in (`<local data dir>/colab-playground/logs`). Local, not
/// roaming: a log is about this machine.
pub fn default_log_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("colab-playground").join("logs"))
}

/// How large a log file may grow before it is set aside at the start of the next run.
pub const LOG_LIMIT: u64 = 4 * 1024 * 1024;

/// Sets a log aside when it has grown past `limit`, keeping the one before it as `<name>.1.log`.
/// Nothing here is fatal: a log that cannot be rotated is no reason not to start.
pub fn rotate_log(path: &Path, limit: u64) -> std::io::Result<Option<PathBuf>> {
    if !std::fs::metadata(path).is_ok_and(|meta| meta.len() >= limit) {
        return Ok(None);
    }
    let aside = path.with_extension("1.log");
    let _ = std::fs::remove_file(&aside);
    std::fs::rename(path, &aside)?;
    Ok(Some(aside))
}

/// Writes `bytes` to `path` atomically (temporary file + rename), creating
/// parent directories. Files are owner-only on unix.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI writes a downloaded file, so it has to be told where in its own namespace: a
    /// Windows path reaches it as one long file name and the file lands in its working directory.
    #[test]
    fn a_local_path_is_given_to_the_cli_in_its_own_namespace() {
        #[cfg(windows)]
        {
            assert_eq!(
                host_path_for_local(Path::new(r"E:\Users\wynn\a file.png"))
                    .expect("a drive path")
                    .as_str(),
                "/mnt/e/Users/wynn/a file.png"
            );
            assert!(
                host_path_for_local(Path::new(r"\\server\share\a.png")).is_none(),
                "a share has no place in the CLI's namespace"
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                host_path_for_local(Path::new("/home/u/a file.png"))
                    .expect("a path")
                    .as_str(),
                "/home/u/a file.png"
            );
        }
    }
    /// A log that has grown is set aside, and the one before it is kept — an install that runs for
    /// months cannot fill the disk.
    #[test]
    fn a_log_is_set_aside_once_it_has_grown() {
        let dir = std::env::temp_dir().join(format!("cpg-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory to log in");
        let path = dir.join("colab-playground.log");
        std::fs::write(&path, b"small").expect("a log");
        assert_eq!(
            rotate_log(&path, 1024).expect("a look"),
            None,
            "a small log stays"
        );
        std::fs::write(&path, vec![b'x'; 2048]).expect("a big log");
        let aside = rotate_log(&path, 1024).expect("a look").expect("set aside");
        assert!(!path.exists(), "the log starts again");
        assert_eq!(std::fs::read(&aside).expect("the old log")[0], b'x');
        std::fs::write(&path, vec![b'y'; 2048]).expect("another big log");
        let again = rotate_log(&path, 1024).expect("a look").expect("set aside");
        assert_eq!(again, aside, "there is one kept log, not a shelf of them");
        assert_eq!(std::fs::read(&aside).expect("the newest old log")[0], b'y');
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_paths_join_with_slashes() {
        let h = HostPath::new("/home/u/.local/share/colab-playground");
        let id = AccountId::new("a1b2").unwrap();
        let layout = DataLayout::new("C:\\data", h.clone());
        assert_eq!(
            layout.account_state(&id).as_str(),
            "/home/u/.local/share/colab-playground/accounts/a1b2/sessions.json"
        );
        assert_eq!(HostPath::new("/x/").join("y").as_str(), "/x/y");
        assert_eq!(
            layout.account_state(&id).parent().as_str(),
            "/home/u/.local/share/colab-playground/accounts/a1b2"
        );
        assert_eq!(HostPath::new("/x").parent().as_str(), "/");
    }

    #[test]
    fn atomic_write_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("f.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
    }
}
