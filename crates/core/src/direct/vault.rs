//! Per-account token vault.
//!
//! The OS store is `secret-tool` (service `colab-playground-direct`). The `keyring`
//! crate is not a dependency: its Linux backend needs D-Bus and fails to link here.
//! Tests use [`MemoryVault`] and never call `secret-tool`. Values are not logged.

#[cfg(test)]
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::Mutex;

use crate::accounts::AccountId;
use crate::error::{Error, ErrorKind, Result};

pub const SERVICE: &str = "colab-playground-direct";

pub fn account_item(id: &AccountId) -> String {
    format!("account:{id}")
}

pub fn secret_item(name: &str) -> String {
    format!("secret:{name}")
}

pub trait TokenVault: Send + Sync {
    fn store(&self, item: &str, secret: &str) -> Result<()>;
    fn lookup(&self, item: &str) -> Result<Option<String>>;
    fn delete(&self, item: &str) -> Result<()>;
}

/// Process-local vault. `Debug` does not print the secrets.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryVault {
    items: Mutex<BTreeMap<String, String>>,
}

#[cfg(test)]
impl MemoryVault {
    pub fn new() -> Self {
        MemoryVault::default()
    }
}

#[cfg(test)]
impl std::fmt::Debug for MemoryVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len();
        f.debug_struct("MemoryVault")
            .field("items", &count)
            .finish()
    }
}

#[cfg(test)]
impl TokenVault for MemoryVault {
    fn store(&self, item: &str, secret: &str) -> Result<()> {
        self.items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(item.to_owned(), secret.to_owned());
        Ok(())
    }

    fn lookup(&self, item: &str) -> Result<Option<String>> {
        Ok(self
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(item)
            .cloned())
    }

    fn delete(&self, item: &str) -> Result<()> {
        self.items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(item);
        Ok(())
    }
}

/// Vault that always fails. Used to prove a locked keyring surfaces `KeyringUnavailable`.
#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FailVault;

#[cfg(test)]
impl TokenVault for FailVault {
    fn store(&self, _item: &str, _secret: &str) -> Result<()> {
        Err(keyring_down())
    }

    fn lookup(&self, _item: &str) -> Result<Option<String>> {
        Err(keyring_down())
    }

    fn delete(&self, _item: &str) -> Result<()> {
        Err(keyring_down())
    }
}

fn keyring_down() -> Error {
    Error::new(ErrorKind::KeyringUnavailable).detail("secret store is unavailable")
}

/// `secret-tool` backed by the desktop secret service. A missing binary is `KeyringUnavailable`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SecretToolVault;

impl SecretToolVault {
    pub fn new() -> Self {
        SecretToolVault
    }
}

impl TokenVault for SecretToolVault {
    fn store(&self, item: &str, secret: &str) -> Result<()> {
        let mut child = Command::new("secret-tool")
            .args([
                "store",
                "--label=colab-playground-direct",
                "service",
                SERVICE,
                "account",
                item,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| keyring_down())?;
        {
            let stdin = child.stdin.as_mut().ok_or_else(keyring_down)?;
            stdin
                .write_all(secret.as_bytes())
                .map_err(|_| keyring_down())?;
        }
        let status = child.wait().map_err(|_| keyring_down())?;
        if status.success() {
            Ok(())
        } else {
            Err(keyring_down())
        }
    }

    fn lookup(&self, item: &str) -> Result<Option<String>> {
        let output = Command::new("secret-tool")
            .args(["lookup", "service", SERVICE, "account", item])
            .output()
            .map_err(|_| keyring_down())?;
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(Some(text))
            }
        } else if output.status.code() == Some(1) {
            Ok(None)
        } else {
            Err(keyring_down())
        }
    }

    fn delete(&self, item: &str) -> Result<()> {
        let output = Command::new("secret-tool")
            .args(["clear", "service", SERVICE, "account", item])
            .output()
            .map_err(|_| keyring_down())?;
        if output.status.success() || output.status.code() == Some(1) {
            Ok(())
        } else {
            Err(keyring_down())
        }
    }
}
