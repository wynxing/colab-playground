//! Plugins: declarative manifests whose script runs on the Colab VM.
//!
//! A plugin never runs anything on the user's machine. It is a directory with
//! `plugin.toml`, one script (`.py` or `.sh`), and optional static assets that
//! the host uploads and starts detached on the VM. Manifests are validated
//! strictly (`api = 1` or `api = 2`, unknown fields rejected). `api = 1`
//! plugins keep working; the job-mode fields are optional additions on both.
//! Bundled plugins are embedded in the binary; user plugins are copied into
//! `<data>/plugins/<id>/` from a folder or a `git clone --depth 1` checkout
//! made by the *local* `git`. See `docs/PLUGINS.md` for the author guide.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::clock::CancelToken;
use crate::error::{Error, ErrorKind, Result};
use crate::host::{Host, is_env_key, tail};
use crate::sessions::Accelerator;

/// Original manifest API. Still accepted, including the optional v2 fields.
pub const API_VERSION: u32 = 1;
/// Manifest API that documents job mode, file inputs, secret slots and assets.
pub const API_VERSION_V2: u32 = 2;
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Largest accepted script (it travels inside a `colab exec` payload).
pub const MAX_SCRIPT_BYTES: u64 = 512 * 1024;
/// Largest single static asset, and how many a plugin may bundle.
pub const MAX_ASSET_BYTES: u64 = 512 * 1024;
pub const MAX_ASSETS: usize = 16;
/// Longest textarea, in bytes (not characters).
pub const MAX_TEXTAREA_BYTES: usize = 64 * 1024;
/// Caps for files the user picks. Checked before anything is read into memory.
pub const MAX_UPLOAD_FILES: usize = 256;
pub const MAX_UPLOAD_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_UPLOAD_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// `true` for the manifest versions this build loads.
pub fn supported_api(api: u32) -> bool {
    api == API_VERSION || api == API_VERSION_V2
}

/// Text with translations, keyed by locale (`en`, `zh-CN`, `zh-TW`, …).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalizedText(pub BTreeMap<String, String>);

impl LocalizedText {
    /// The text for `locale`: exact match, then same language, then `en`,
    /// then any. `None` only when empty.
    pub fn get(&self, locale: &str) -> Option<&str> {
        let lang = locale.split('-').next().unwrap_or(locale);
        self.0
            .get(locale)
            .or_else(|| {
                self.0
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(locale))
                    .map(|(_, v)| v)
            })
            .or_else(|| {
                self.0
                    .iter()
                    .find(|(k, _)| k.split('-').next() == Some(lang))
                    .map(|(_, v)| v)
            })
            .or_else(|| self.0.get("en"))
            .or_else(|| self.0.values().next())
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Default hardware for runs (user-editable in the run form).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionDefaults {
    #[serde(default)]
    pub accelerator: Option<Accelerator>,
    /// Minimum GPU memory (0 = any).
    #[serde(default)]
    pub min_vram_mib: u32,
    #[serde(default)]
    pub high_mem: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputKind {
    Text,
    Number,
    Bool,
    Select,
    /// A secret (never written to disk or logs by the host).
    Secret,
    /// A Hugging Face model repo id (`owner/name`) with host-side search.
    HfModel,
    /// A local file, uploaded to `$PLUGIN_RUN_DIR/inputs/<id>/<name>`.
    File,
    /// A local directory, uploaded under `$PLUGIN_RUN_DIR/inputs/<id>/`.
    Dir,
    /// A local `.ipynb`, uploaded like a file. The host does not open it.
    Notebook,
    /// Multi-line text (a prompt or a small config).
    Textarea,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSpec {
    /// Id: `[a-z][a-z0-9_-]*`, at most 32 chars. Reaches the script as `PLUGIN_INPUT_<ID>`.
    pub id: String,
    #[serde(rename = "type")]
    pub kind: InputKind,
    /// Must be non-empty.
    #[serde(default)]
    pub required: bool,
    /// Label (falls back to the id).
    #[serde(default)]
    pub label: LocalizedText,
    #[serde(default)]
    pub help: LocalizedText,
    /// Default value (not allowed for secrets).
    #[serde(default)]
    pub default: Option<InputValue>,
    /// Choices for `select`.
    #[serde(default)]
    pub options: Vec<String>,
    /// Lower bound for `number`.
    #[serde(default)]
    pub min: Option<f64>,
    /// Upper bound for `number`.
    #[serde(default)]
    pub max: Option<f64>,
}

impl InputSpec {
    pub fn env_key(&self) -> String {
        env_key(&self.id)
    }
}

/// `PLUGIN_INPUT_<ID>` with the id upper-cased and `-` mapped to `_`.
pub fn env_key(id: &str) -> String {
    format!("PLUGIN_INPUT_{}", id.to_ascii_uppercase().replace('-', "_"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputValue {
    Bool(bool),
    Number(f64),
    /// Text, select choice, secret or repo id.
    Text(String),
}

impl InputValue {
    pub fn to_env(&self) -> String {
        match self {
            InputValue::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
            InputValue::Number(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", *n as i64),
            InputValue::Number(n) => n.to_string(),
            InputValue::Text(s) => s.clone(),
        }
    }
}

/// How a run ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// A long-lived server. The runtime stays up after the script reports ready.
    #[default]
    Service,
    /// A batch job: on success, run the Keep Plan and stop a runtime this run opened.
    Job,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    /// Script file in the plugin directory (`.py` runs with the VM's Python,
    /// `.sh` with bash).
    pub script: String,
    /// The script keeps running after it reports ready (a server), so the app
    /// keeps reading its status. Not allowed together with [`RunMode::Job`].
    #[serde(default)]
    pub keep_alive: bool,
    /// `service` (default) or `job`.
    #[serde(default)]
    pub mode: RunMode,
    /// Job mode only: leave a runtime this run opened when the script fails,
    /// so the user can look. Default is to stop it.
    #[serde(default)]
    pub keep_on_failure: bool,
}

/// A progress stage shown in the run view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageSpec {
    /// Id the script reports in `status.json`.
    pub id: String,
    #[serde(default)]
    pub label: LocalizedText,
}

/// A port the host can forward once the run is ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSpec {
    pub id: String,
    /// Number input holding the VM port.
    #[serde(default)]
    pub port_input: Option<String>,
    /// Fixed VM port (instead of `port_input`).
    #[serde(default)]
    pub port: Option<u16>,
    /// Base path shown to the user (e.g. `/v1`).
    #[serde(default = "root_path")]
    pub path: String,
    /// Health path probed through the tunnel.
    #[serde(default)]
    pub health: Option<String>,
    /// `openai`, `http`, … (lower-case word; informs the endpoint card).
    #[serde(default = "http_kind")]
    pub kind: String,
    #[serde(default)]
    pub label: LocalizedText,
}

fn root_path() -> String {
    "/".into()
}
fn http_kind() -> String {
    "http".into()
}

/// A VM path whose contents the user may want to keep (hook for the
/// persistence feature; not acted upon yet).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeepSpec {
    /// Absolute path on the VM.
    pub path: String,
    #[serde(default)]
    pub label: LocalizedText,
}

/// A named keyring slot the plugin may receive (`PLUGIN_SECRET_<ID>`).
/// The value is stored per plugin and is never written to the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSlot {
    /// `[a-z][a-z0-9_]*`, at most 32 characters.
    pub id: String,
    /// When true, a run refuses to start until the user has saved a value.
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub label: LocalizedText,
}

/// A static file shipped with the plugin (not executed on this computer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetSpec {
    /// Relative path inside the plugin directory (`requirements.txt`, `workflows/a.json`).
    pub path: String,
}

/// One static file after it has been read (bytes only; nothing is executed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginAsset {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// The user's saved Hugging Face token, passed as `PLUGIN_HF_TOKEN`
    /// when one is stored (optional for the plugin).
    HfToken,
    /// Consent flag `PLUGIN_GCP_AUTH=1`. Not a stored secret and not a `colab` command.
    GcpAuth,
    /// The script may use Google Drive on the VM. The user must opt in on the run form.
    Drive,
}

impl Capability {
    /// Vault secret name holding the value. `None` for capabilities that are a
    /// consent flag rather than a stored secret.
    pub fn secret_name(self) -> Option<&'static str> {
        match self {
            Capability::HfToken => Some("hf_token"),
            Capability::GcpAuth | Capability::Drive => None,
        }
    }

    /// Environment variable the script receives, when the capability is granted.
    pub fn env_key(self) -> Option<&'static str> {
        match self {
            Capability::HfToken => Some("PLUGIN_HF_TOKEN"),
            Capability::GcpAuth => Some("PLUGIN_GCP_AUTH"),
            Capability::Drive => Some("PLUGIN_DRIVE"),
        }
    }

    /// The run form must record an explicit yes before this is granted.
    pub fn needs_consent(self) -> bool {
        matches!(self, Capability::GcpAuth | Capability::Drive)
    }
}

/// Explicit yes for the capabilities that touch Google. Both default to no.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CapabilityConsent {
    #[serde(default)]
    pub gcp_auth: bool,
    #[serde(default)]
    pub drive: bool,
}

/// A parsed, validated `plugin.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Must be [`API_VERSION`].
    pub api: u32,
    /// Id: `[a-z0-9][a-z0-9-]*`, at most 48 chars (also the directory name).
    pub id: String,
    pub version: String,
    /// Display name (`en` required).
    pub name: LocalizedText,
    #[serde(default)]
    pub description: LocalizedText,
    #[serde(default)]
    pub provision: ProvisionDefaults,
    #[serde(default)]
    pub inputs: Vec<InputSpec>,
    pub run: RunSpec,
    #[serde(default)]
    pub stages: Vec<StageSpec>,
    #[serde(default)]
    pub endpoints: Vec<EndpointSpec>,
    /// Paths worth keeping (persistence hook).
    #[serde(default)]
    pub keep: Vec<KeepSpec>,
    #[serde(default)]
    pub needs: Vec<Capability>,
    /// Named keyring slots beyond `hf_token`.
    #[serde(default)]
    pub secrets: Vec<SecretSlot>,
    /// Static files uploaded next to the script.
    #[serde(default)]
    pub assets: Vec<AssetSpec>,
}

fn invalid(field: &str, reason: &str) -> Error {
    Error::new(ErrorKind::ManifestInvalid)
        .with("field", field)
        .with("reason", reason)
}

fn is_slug(s: &str, max: usize, first_alpha_only: bool) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || (!first_alpha_only && c.is_ascii_digit()))
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn valid_locale(k: &str) -> bool {
    let mut parts = k.split('-');
    let lang = parts.next().unwrap_or_default();
    (2..=3).contains(&lang.len())
        && lang.chars().all(|c| c.is_ascii_lowercase())
        && parts.all(|p| (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn check_text(field: &str, t: &LocalizedText, need_en: bool) -> Result<()> {
    if need_en && t.0.get("en").is_none_or(|v| v.trim().is_empty()) {
        return Err(invalid(field, "an `en` entry is required"));
    }
    for (k, v) in &t.0 {
        if !valid_locale(k) {
            return Err(invalid(field, "invalid locale key"));
        }
        if v.len() > 4000 {
            return Err(invalid(field, "text too long"));
        }
    }
    Ok(())
}

fn check_vm_path(field: &str, p: &str) -> Result<()> {
    if !p.starts_with('/')
        || p.len() > 256
        || p.split('/').any(|c| c == "..")
        || p.chars().any(|c| c.is_control())
    {
        return Err(invalid(field, "must be an absolute VM path without `..`"));
    }
    Ok(())
}

/// Validates the script file name (relative, no traversal, `.py`/`.sh`).
pub fn check_script_name(script: &str) -> Result<()> {
    let ok_chars = script
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c));
    if script.is_empty()
        || script.len() > 128
        || script.starts_with('/')
        || !ok_chars
        || script
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(invalid(
            "run.script",
            "must be a relative path inside the plugin directory",
        ));
    }
    if !(script.ends_with(".py") || script.ends_with(".sh")) {
        return Err(invalid("run.script", "must end in .py or .sh"));
    }
    Ok(())
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest> {
        // Check the API version first so newer manifests get a precise error.
        let raw: toml::Table =
            toml::from_str(text).map_err(|e| invalid("toml", "syntax").detail(e.to_string()))?;
        match raw.get("api").and_then(|v| v.as_integer()) {
            Some(v) if u32::try_from(v).ok().is_some_and(supported_api) => {}
            Some(_) => return Err(invalid("api", "unsupported api version")),
            None => return Err(invalid("api", "missing")),
        }
        let m: Manifest =
            toml::from_str(text).map_err(|e| invalid("toml", "schema").detail(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    /// Checks every rule documented in `docs/PLUGINS.md`.
    pub fn validate(&self) -> Result<()> {
        if !supported_api(self.api) {
            return Err(invalid("api", "unsupported api version"));
        }
        if !is_slug(&self.id, 48, false) || self.id.contains('_') {
            return Err(invalid("id", "use lower-case letters, digits and '-'"));
        }
        if self.version.is_empty()
            || self.version.len() > 32
            || !self
                .version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".+-".contains(c))
        {
            return Err(invalid("version", "invalid version"));
        }
        check_text("name", &self.name, true)?;
        check_text("description", &self.description, false)?;
        check_script_name(&self.run.script)?;
        if self.run.mode == RunMode::Job && self.run.keep_alive {
            return Err(invalid(
                "run.keep_alive",
                "a job stops when the script finishes",
            ));
        }
        if self.run.mode != RunMode::Job && self.run.keep_on_failure {
            return Err(invalid(
                "run.keep_on_failure",
                "only a job can stay up after a failure",
            ));
        }
        if self.provision.min_vram_mib > 0
            && !self.provision.accelerator.is_some_and(|a| a.is_gpu())
        {
            return Err(invalid("provision.min_vram_mib", "needs a GPU accelerator"));
        }

        let mut ids = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for (i, input) in self.inputs.iter().enumerate() {
            let f = |name: &str| format!("inputs[{i}].{name}");
            if !is_slug(&input.id, 32, true) {
                return Err(invalid(&f("id"), "use [a-z][a-z0-9_-]*"));
            }
            if !ids.insert(input.id.clone()) {
                return Err(invalid(&f("id"), "duplicate id"));
            }
            let key = input.env_key();
            if !is_env_key(&key) || !keys.insert(key) {
                return Err(invalid(
                    &f("id"),
                    "environment name collides with another input",
                ));
            }
            check_text(&f("label"), &input.label, false)?;
            check_text(&f("help"), &input.help, false)?;
            match (input.kind, &input.default) {
                (
                    InputKind::Secret | InputKind::File | InputKind::Dir | InputKind::Notebook,
                    Some(_),
                ) => {
                    return Err(invalid(
                        &f("default"),
                        "secrets and local paths cannot have defaults",
                    ));
                }
                (InputKind::Number, Some(d)) if !matches!(d, InputValue::Number(_)) => {
                    return Err(invalid(&f("default"), "must be a number"));
                }
                (InputKind::Bool, Some(d)) if !matches!(d, InputValue::Bool(_)) => {
                    return Err(invalid(&f("default"), "must be true or false"));
                }
                (
                    InputKind::Text | InputKind::Textarea | InputKind::Select | InputKind::HfModel,
                    Some(d),
                ) if !matches!(d, InputValue::Text(_)) => {
                    return Err(invalid(&f("default"), "must be a string"));
                }
                _ => {}
            }
            if input.kind == InputKind::Select {
                if input.options.is_empty()
                    || input.options.iter().any(|o| o.is_empty() || o.len() > 128)
                {
                    return Err(invalid(&f("options"), "a select needs non-empty options"));
                }
                if let Some(InputValue::Text(d)) = &input.default
                    && !input.options.contains(d)
                {
                    return Err(invalid(&f("default"), "not one of the options"));
                }
            } else if !input.options.is_empty() {
                return Err(invalid(&f("options"), "only selects have options"));
            }
            if (input.min.is_some() || input.max.is_some()) && input.kind != InputKind::Number {
                return Err(invalid(&f("min"), "only numbers have bounds"));
            }
            if let (Some(InputValue::Number(d)), true) =
                (&input.default, input.kind == InputKind::Number)
                && (input.min.is_some_and(|m| *d < m) || input.max.is_some_and(|m| *d > m))
            {
                return Err(invalid(&f("default"), "outside min/max"));
            }
        }

        let mut stage_ids = BTreeSet::new();
        for (i, s) in self.stages.iter().enumerate() {
            if !is_slug(&s.id, 32, true) || !stage_ids.insert(s.id.clone()) {
                return Err(invalid(
                    &format!("stages[{i}].id"),
                    "invalid or duplicate id",
                ));
            }
            check_text(&format!("stages[{i}].label"), &s.label, false)?;
        }

        let mut ep_ids = BTreeSet::new();
        for (i, e) in self.endpoints.iter().enumerate() {
            let f = |name: &str| format!("endpoints[{i}].{name}");
            if !is_slug(&e.id, 32, true) || !ep_ids.insert(e.id.clone()) {
                return Err(invalid(&f("id"), "invalid or duplicate id"));
            }
            match (&e.port_input, e.port) {
                (Some(pi), None) => {
                    let ok = self
                        .inputs
                        .iter()
                        .any(|x| &x.id == pi && x.kind == InputKind::Number);
                    if !ok {
                        return Err(invalid(&f("port_input"), "must name a number input"));
                    }
                }
                (None, Some(p)) => {
                    if p == 0 || p == crate::cli::RESERVED_VM_PORT {
                        return Err(invalid(&f("port"), "port 0 and 8080 are not allowed"));
                    }
                }
                _ => {
                    return Err(invalid(
                        &f("port"),
                        "give exactly one of port_input or port",
                    ));
                }
            }
            if !e.path.starts_with('/') || e.path.chars().any(|c| c.is_whitespace()) {
                return Err(invalid(&f("path"), "must start with '/'"));
            }
            if e.health
                .as_ref()
                .is_some_and(|h| !h.starts_with('/') || h.chars().any(|c| c.is_whitespace()))
            {
                return Err(invalid(&f("health"), "must start with '/'"));
            }
            if e.kind.is_empty()
                || e.kind.len() > 16
                || !e.kind.chars().all(|c| c.is_ascii_lowercase() || c == '-')
            {
                return Err(invalid(&f("kind"), "lower-case word"));
            }
            check_text(&f("label"), &e.label, false)?;
        }

        for (i, k) in self.keep.iter().enumerate() {
            check_vm_path(&format!("keep[{i}].path"), &k.path)?;
            check_text(&format!("keep[{i}].label"), &k.label, false)?;
        }
        let needs: BTreeSet<_> = self.needs.iter().collect();
        if needs.len() != self.needs.len() {
            return Err(invalid("needs", "duplicate capability"));
        }

        let mut slot_ids = BTreeSet::new();
        let mut slot_keys = BTreeSet::new();
        for (i, slot) in self.secrets.iter().enumerate() {
            let f = |name: &str| format!("secrets[{i}].{name}");
            if !is_secret_slot(&slot.id) {
                return Err(invalid(&f("id"), "use [a-z][a-z0-9_]*"));
            }
            if !slot_ids.insert(slot.id.clone()) {
                return Err(invalid(&f("id"), "duplicate id"));
            }
            let key = secret_env_key(&slot.id);
            if !is_env_key(&key) || !slot_keys.insert(key) {
                return Err(invalid(&f("id"), "environment name collides"));
            }
            check_text(&f("label"), &slot.label, false)?;
        }

        let mut asset_paths = BTreeSet::new();
        for (i, asset) in self.assets.iter().enumerate() {
            check_asset_path(&format!("assets[{i}].path"), &asset.path)?;
            if asset.path == self.run.script {
                return Err(invalid(
                    &format!("assets[{i}].path"),
                    "an asset cannot be the script",
                ));
            }
            if !asset_paths.insert(asset.path.clone()) {
                return Err(invalid(&format!("assets[{i}].path"), "duplicate path"));
            }
        }
        if self.assets.len() > MAX_ASSETS {
            return Err(invalid("assets", "too many files"));
        }
        Ok(())
    }

    pub fn input(&self, id: &str) -> Option<&InputSpec> {
        self.inputs.iter().find(|i| i.id == id)
    }
}

/// A local file or directory to upload. The local path is an argument to
/// `colab upload`; it is never copied into script text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedUpload {
    pub local: PathBuf,
    /// Path relative to the run directory (`inputs/<id>/name`). Safe characters only.
    pub remote_rel: String,
    pub is_dir: bool,
}

/// Values for a run, after defaults and validation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedInputs {
    /// `PLUGIN_INPUT_*` pairs for `colab exec --env`.
    /// File inputs are not here yet: their VM path needs the run directory.
    pub env: Vec<(String, String)>,
    /// Secret values to redact from logs.
    pub secrets: Vec<String>,
    /// Final values by input id (secrets included; never persist this).
    /// File inputs store the VM-relative path, not the local path.
    pub values: BTreeMap<String, InputValue>,
    /// Local paths to upload before the script starts.
    pub uploads: Vec<StagedUpload>,
    /// `(PLUGIN_INPUT_*, remote_rel)` filled in once the run directory exists.
    pub vm_rels: Vec<(String, String)>,
}

/// Validates repo ids like `owner/name`.
pub fn is_repo_id(s: &str) -> bool {
    let mut parts = s.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && p.len() <= 96
                && !p.starts_with(['-', '.'])
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// Applies defaults, checks types and requirements, and maps inputs to env.
pub fn resolve_inputs(
    m: &Manifest,
    provided: &BTreeMap<String, InputValue>,
) -> Result<ResolvedInputs> {
    for k in provided.keys() {
        if m.input(k).is_none() {
            return Err(Error::invalid(k, "unknown input"));
        }
    }
    let mut out = ResolvedInputs::default();
    for spec in &m.inputs {
        let value = provided
            .get(&spec.id)
            .cloned()
            .or_else(|| spec.default.clone());
        let value = match (spec.kind, value) {
            (_, None) => None,
            (InputKind::Number, Some(InputValue::Text(t))) => Some(InputValue::Number(
                t.trim()
                    .parse::<f64>()
                    .map_err(|_| Error::invalid(&spec.id, "not a number"))?,
            )),
            (InputKind::Bool, Some(InputValue::Text(t))) => {
                Some(InputValue::Bool(matches!(t.as_str(), "1" | "true" | "yes")))
            }
            (_, Some(v)) => Some(v),
        };
        let empty = match &value {
            None => true,
            Some(InputValue::Text(t)) => t.trim().is_empty(),
            _ => false,
        };
        if empty {
            if spec.required {
                return Err(Error::invalid(&spec.id, "required"));
            }
            continue;
        }
        let value = value.unwrap_or(InputValue::Bool(false));
        match (spec.kind, &value) {
            (InputKind::Number, InputValue::Number(n)) => {
                if !n.is_finite()
                    || spec.min.is_some_and(|m| *n < m)
                    || spec.max.is_some_and(|m| *n > m)
                {
                    return Err(Error::invalid(&spec.id, "out of range"));
                }
            }
            (InputKind::Bool, InputValue::Bool(_)) => {}
            (InputKind::Select, InputValue::Text(t)) => {
                if !spec.options.contains(t) {
                    return Err(Error::invalid(&spec.id, "not one of the options"));
                }
            }
            (InputKind::HfModel, InputValue::Text(t)) => {
                if !is_repo_id(t.trim()) {
                    return Err(Error::invalid(&spec.id, "expected owner/name"));
                }
            }
            (InputKind::Text | InputKind::Secret, InputValue::Text(t)) => {
                if t.len() > 8192 || t.contains('\0') {
                    return Err(Error::invalid(&spec.id, "too long"));
                }
            }
            (InputKind::Textarea, InputValue::Text(t)) => {
                if t.len() > MAX_TEXTAREA_BYTES || t.contains('\0') {
                    return Err(Error::invalid(&spec.id, "too long"));
                }
            }
            (InputKind::File | InputKind::Dir | InputKind::Notebook, InputValue::Text(t)) => {
                let staged = stage_local(spec, t)?;
                out.vm_rels
                    .push((spec.env_key(), staged.remote_rel.clone()));
                out.values
                    .insert(spec.id.clone(), InputValue::Text(staged.remote_rel.clone()));
                out.uploads.push(staged);
                continue;
            }
            _ => return Err(Error::invalid(&spec.id, "wrong type")),
        }
        let env_value = match &value {
            InputValue::Text(t) if spec.kind == InputKind::HfModel => t.trim().to_owned(),
            v => v.to_env(),
        };
        if spec.kind == InputKind::Secret {
            out.secrets.push(env_value.clone());
        }
        out.env.push((spec.env_key(), env_value));
        out.values.insert(spec.id.clone(), value);
    }
    Ok(out)
}

/// Prefixes each file input's relative path with the run directory.
pub fn vm_env(run_dir: &str, rels: &[(String, String)]) -> Vec<(String, String)> {
    rels.iter()
        .map(|(key, rel)| (key.clone(), format!("{run_dir}/{rel}")))
        .collect()
}

/// An endpoint with its port resolved from the inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedEndpoint {
    pub id: String,
    pub remote_port: u16,
    pub path: String,
    pub health: Option<String>,
    /// Kind (`openai`, `http`, …).
    pub kind: String,
    pub label: LocalizedText,
}

/// Resolves endpoint ports from the final input values.
pub fn resolve_endpoints(
    m: &Manifest,
    values: &BTreeMap<String, InputValue>,
) -> Result<Vec<ResolvedEndpoint>> {
    m.endpoints
        .iter()
        .map(|e| {
            let port = match (&e.port_input, e.port) {
                (_, Some(p)) => p,
                (Some(pi), None) => match values.get(pi) {
                    Some(InputValue::Number(n))
                        if *n >= 1.0 && *n <= 65535.0 && n.fract() == 0.0 =>
                    {
                        *n as u16
                    }
                    _ => return Err(Error::invalid(pi, "invalid port")),
                },
                (None, None) => return Err(invalid("endpoints", "no port")),
            };
            if port == crate::cli::RESERVED_VM_PORT {
                return Err(Error::new(ErrorKind::PortReserved).with("port", port));
            }
            Ok(ResolvedEndpoint {
                id: e.id.clone(),
                remote_port: port,
                path: e.path.clone(),
                health: e.health.clone(),
                kind: e.kind.clone(),
                label: e.label.clone(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginSource {
    /// Embedded in the binary (cannot be removed, only disabled).
    Bundled,
    /// Installed under the data directory (or in memory for the mock).
    User,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPlugin {
    pub manifest: Manifest,
    pub script: Vec<u8>,
    /// Static files. Empty for the bundled plugins.
    pub assets: Vec<PluginAsset>,
    pub source: PluginSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub manifest: Manifest,
    pub source: PluginSource,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginOrigin {
    Folder {
        /// Path as given.
        path: String,
    },
    /// A Git URL (cloned with the local `git`).
    Git {
        /// URL as given.
        url: String,
    },
}

/// What the confirmation dialog shows before installing. Installing runs the
/// script on the user's paid runtime later; nothing runs locally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginPreview {
    pub manifest: Manifest,
    pub origin: PluginOrigin,
    pub script_bytes: u64,
    /// Static files staged with the script (still not executed here).
    pub assets: Vec<PluginAsset>,
    /// Version currently installed with the same id, if any.
    pub replaces_version: Option<String>,
    /// Staged copy of the files (manifest + script) to install from.
    pub staged_dir: PathBuf,
}

/// Bundled plugins: (manifest, script bytes).
pub fn bundled() -> Vec<(&'static str, &'static [u8])> {
    vec![
        (
            include_str!("../../../plugins/llama-cpp/plugin.toml"),
            include_bytes!("../../../plugins/llama-cpp/worker.py"),
        ),
        (
            include_str!("../../../plugins/comfyui/plugin.toml"),
            include_bytes!("../../../plugins/comfyui/worker.py"),
        ),
    ]
}

/// A plugin directory after validation: manifest, script bytes, static assets.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginBundle {
    pub manifest: Manifest,
    pub script: Vec<u8>,
    pub assets: Vec<PluginAsset>,
}

/// Reads and validates a plugin directory (no symlinked or oversized files,
/// script and assets confined to the directory). Nothing in the directory is executed.
pub fn load_dir(dir: &Path) -> Result<PluginBundle> {
    let manifest_path = dir.join("plugin.toml");
    let text = read_regular(&manifest_path, MAX_MANIFEST_BYTES, "plugin.toml")?;
    let manifest =
        Manifest::parse(&String::from_utf8(text).map_err(|_| invalid("toml", "not UTF-8"))?)?;
    let canon_dir = dir.canonicalize()?;
    let script = read_confined(
        &canon_dir,
        &dir.join(&manifest.run.script),
        MAX_SCRIPT_BYTES,
        "run.script",
    )?;
    let mut assets = Vec::with_capacity(manifest.assets.len());
    for spec in &manifest.assets {
        let bytes = read_confined(&canon_dir, &dir.join(&spec.path), MAX_ASSET_BYTES, "assets")?;
        assets.push(PluginAsset {
            path: spec.path.clone(),
            bytes,
        });
    }
    Ok(PluginBundle {
        manifest,
        script,
        assets,
    })
}

fn read_confined(canon_dir: &Path, path: &Path, max: u64, field: &str) -> Result<Vec<u8>> {
    let canon = path
        .canonicalize()
        .map_err(|_| invalid(field, "file not found"))?;
    if !canon.starts_with(canon_dir) {
        return Err(invalid(field, "outside the plugin directory"));
    }
    read_regular(path, max, field)
}

fn read_regular(path: &Path, max: u64, field: &str) -> Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| invalid(field, "file not found"))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(invalid(field, "must be a regular file (no symlinks)"));
    }
    if meta.len() > max {
        return Err(invalid(field, "file too large"));
    }
    Ok(std::fs::read(path)?)
}

/// Validates a Git URL: `https://…`, `ssh://…` or `user@host:path`.
pub fn validate_git_url(url: &str) -> Result<()> {
    let bad = || {
        Error::invalid("url", "use an https://, ssh:// or user@host:path Git URL")
            .with("value", url)
    };
    if url.is_empty() || url.len() > 512 || url.starts_with('-') || url.contains("::") {
        return Err(bad());
    }
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(bad());
    }
    let host_ok = |h: &str| {
        !h.is_empty()
            && !h.starts_with('-')
            && h.chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-:[]@_".contains(c))
    };
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("ssh://"))
    {
        let host = rest.split('/').next().unwrap_or_default();
        return if host_ok(host) && rest.len() > host.len() + 1 {
            Ok(())
        } else {
            Err(bad())
        };
    }
    if let Some((user_host, path)) = url.split_once(':')
        && let Some((user, host)) = user_host.split_once('@')
    {
        let user_ok = !user.is_empty()
            && user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
        if user_ok
            && host_ok(host)
            && !path.is_empty()
            && !path.starts_with('-')
            && !url.contains("://")
        {
            return Ok(());
        }
    }
    Err(bad())
}

/// `git clone` arguments (pure). Protocols that can run commands or read
/// local files are disabled; hooks are never cloned; symlinks are checked out
/// as plain files.
pub fn git_clone_args(url: &str, dest: &str) -> Result<Vec<String>> {
    validate_git_url(url)?;
    Ok([
        "-c",
        "protocol.ext.allow=never",
        "-c",
        "protocol.file.allow=never",
        "-c",
        "core.symlinks=false",
        "clone",
        "--depth",
        "1",
        "--no-tags",
        "--single-branch",
        "--",
        url,
        dest,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect())
}

/// Clones `url` with the **local** `git` into a new directory under `staging`
/// and returns that directory. A clone that does not finish (git fails, a
/// cancel, the timeout) leaves no directory behind.
pub fn clone_git(url: &str, staging: &Path, cancel: &CancelToken) -> Result<PathBuf> {
    let dest = staging.join(format!("{CHECKOUT_PREFIX}{}", crate::accounts::nonce()));
    std::fs::create_dir_all(staging)?;
    let args = git_clone_args(url, &dest.to_string_lossy())?;
    let env = vec![
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ("GIT_LFS_SKIP_SMUDGE".to_owned(), "1".to_owned()),
    ];
    let cloned = Host::Local
        .run_program("git", &args, &env, cancel, Some(Duration::from_secs(300)))
        .and_then(|out| {
            if out.success() {
                Ok(())
            } else {
                Err(Error::new(ErrorKind::GitFailed)
                    .with("url", url)
                    .detail(tail(&out.stderr, 600)))
            }
        });
    finished_checkout(dest, cloned)
}

/// A cancel or the timeout kills git in the middle of a clone, so every failure can leave a
/// partial checkout at `dest`, not only a non-zero exit.
fn finished_checkout(dest: PathBuf, cloned: Result<()>) -> Result<PathBuf> {
    if let Err(error) = cloned {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(error);
    }
    Ok(dest)
}

/// The clean-up deletes only directories with these prefixes: the path it is given comes
/// from the user's data folder or from a preview, which is plain data.
const STAGE_PREFIX: &str = "stage-";
const CHECKOUT_PREFIX: &str = "git-";

/// Nothing in `staging` outlives the install dialog that made it, so what is
/// found when the app starts is a leftover (the app quit or crashed with the
/// dialog open).
pub fn clear_staging(staging: &Path) {
    let Ok(entries) = std::fs::read_dir(staging) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(STAGE_PREFIX) || name.starts_with(CHECKOUT_PREFIX) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// For a preview that will not be installed (the dialog was closed).
pub fn discard_preview(preview: &PluginPreview) {
    let staged = preview
        .staged_dir
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(STAGE_PREFIX));
    if staged {
        let _ = std::fs::remove_dir_all(&preview.staged_dir);
    }
}

#[derive(Debug)]
pub struct PluginStore {
    dir: Option<PathBuf>,
    flags_path: Option<PathBuf>,
    plugins: Vec<LoadedPlugin>,
    flags: BTreeMap<String, bool>,
    /// Problems with installed plugins that failed to load (id/dir → error).
    pub load_errors: Vec<(String, Error)>,
}

impl PluginStore {
    /// Loads bundled plugins and, when `dir` is given, user plugins from it.
    pub fn load(dir: Option<&Path>, flags_path: Option<&Path>) -> Result<PluginStore> {
        let mut store = PluginStore {
            dir: dir.map(Path::to_owned),
            flags_path: flags_path.map(Path::to_owned),
            plugins: Vec::new(),
            flags: BTreeMap::new(),
            load_errors: Vec::new(),
        };
        for (toml, script) in bundled() {
            let manifest = Manifest::parse(toml)?;
            store.plugins.push(LoadedPlugin {
                manifest,
                script: script.to_vec(),
                assets: Vec::new(),
                source: PluginSource::Bundled,
            });
        }
        if let Some(path) = &store.flags_path
            && let Ok(text) = std::fs::read_to_string(path)
        {
            store.flags = serde_json::from_str(&text).unwrap_or_default();
        }
        if let Some(dir) = &store.dir
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            for d in dirs {
                let label = d
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match load_dir(&d) {
                    Ok(bundle) if bundle.manifest.id != label => {
                        store
                            .load_errors
                            .push((label, invalid("id", "does not match its directory")));
                    }
                    Ok(bundle) if store.get(&bundle.manifest.id).is_some() => {
                        store.load_errors.push((
                            label,
                            Error::new(ErrorKind::PluginExists).with("plugin", &bundle.manifest.id),
                        ));
                    }
                    Ok(bundle) => {
                        store.plugins.push(LoadedPlugin {
                            manifest: bundle.manifest,
                            script: bundle.script,
                            assets: bundle.assets,
                            source: PluginSource::User,
                        });
                    }
                    Err(e) => store.load_errors.push((label, e)),
                }
            }
        }
        Ok(store)
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|p| PluginInfo {
                manifest: p.manifest.clone(),
                source: p.source.clone(),
                enabled: self.enabled(&p.manifest.id),
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&LoadedPlugin> {
        self.plugins.iter().find(|p| p.manifest.id == id)
    }

    /// Enabled unless explicitly disabled.
    pub fn enabled(&self, id: &str) -> bool {
        self.flags.get(id).copied().unwrap_or(true)
    }

    /// Validates a folder and stages a copy of its manifest and script.
    pub fn inspect_folder(
        &self,
        path: &Path,
        staging: &Path,
        origin: PluginOrigin,
    ) -> Result<PluginPreview> {
        let bundle = load_dir(path)?;
        if self
            .get(&bundle.manifest.id)
            .is_some_and(|p| p.source == PluginSource::Bundled)
        {
            return Err(Error::new(ErrorKind::PluginExists).with("plugin", &bundle.manifest.id));
        }
        let toml = std::fs::read(path.join("plugin.toml"))?;
        let staged = staging.join(format!("{STAGE_PREFIX}{}", crate::accounts::nonce()));
        if let Err(error) = write_plugin_files(&staged, &bundle, &toml) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error);
        }
        Ok(PluginPreview {
            replaces_version: self
                .get(&bundle.manifest.id)
                .map(|p| p.manifest.version.clone()),
            script_bytes: bundle.script.len() as u64,
            assets: bundle.assets,
            manifest: bundle.manifest,
            origin,
            staged_dir: staged,
        })
    }

    /// Installs a previewed plugin (replacing a user plugin with the same id).
    /// The preview's staged copy is used up either way: a failed install
    /// needs a new preview.
    pub fn install(&mut self, preview: &PluginPreview) -> Result<PluginInfo> {
        let installed = self.install_staged(preview);
        discard_preview(preview);
        installed
    }

    fn install_staged(&mut self, preview: &PluginPreview) -> Result<PluginInfo> {
        let bundle = load_dir(&preview.staged_dir)?;
        if bundle.manifest != preview.manifest || bundle.assets != preview.assets {
            return Err(invalid("plugin.toml", "changed since it was previewed"));
        }
        if self
            .get(&bundle.manifest.id)
            .is_some_and(|p| p.source == PluginSource::Bundled)
        {
            return Err(Error::new(ErrorKind::PluginExists).with("plugin", &bundle.manifest.id));
        }
        if let Some(dir) = &self.dir {
            let target = dir.join(&bundle.manifest.id);
            let toml = std::fs::read(preview.staged_dir.join("plugin.toml"))?;
            let tmp = dir.join(format!(".{}.new", bundle.manifest.id));
            let _ = std::fs::remove_dir_all(&tmp);
            write_plugin_files(&tmp, &bundle, &toml)?;
            let _ = std::fs::remove_dir_all(&target);
            std::fs::rename(&tmp, &target)?;
        }
        let manifest = bundle.manifest;
        self.plugins.retain(|p| p.manifest.id != manifest.id);
        self.plugins.push(LoadedPlugin {
            manifest: manifest.clone(),
            script: bundle.script,
            assets: bundle.assets,
            source: PluginSource::User,
        });
        self.flags.remove(&manifest.id);
        self.save_flags()?;
        Ok(PluginInfo {
            manifest,
            source: PluginSource::User,
            enabled: true,
        })
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        if self.get(id).is_none() {
            return Err(Error::new(ErrorKind::PluginNotFound).with("plugin", id));
        }
        self.flags.insert(id.to_owned(), enabled);
        self.save_flags()
    }

    /// Removes a user plugin (bundled ones can only be disabled).
    pub fn remove(&mut self, id: &str) -> Result<()> {
        match self.get(id).map(|p| p.source.clone()) {
            None => Err(Error::new(ErrorKind::PluginNotFound).with("plugin", id)),
            Some(PluginSource::Bundled) => Err(Error::invalid(
                "plugin",
                "bundled plugins can only be disabled",
            )),
            Some(PluginSource::User) => {
                if let Some(dir) = &self.dir
                    && is_slug(id, 48, false)
                {
                    let _ = std::fs::remove_dir_all(dir.join(id));
                }
                self.plugins.retain(|p| p.manifest.id != id);
                self.flags.remove(id);
                self.save_flags()
            }
        }
    }

    fn save_flags(&self) -> Result<()> {
        if let Some(path) = &self.flags_path {
            crate::paths::write_atomic(
                path,
                serde_json::to_string_pretty(&self.flags)?.as_bytes(),
            )?;
        }
        Ok(())
    }
}

fn write_plugin_files(dir: &Path, bundle: &PluginBundle, toml: &[u8]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("plugin.toml"), toml)?;
    let script_path = dir.join(&bundle.manifest.run.script);
    if let Some(parent) = script_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(script_path, &bundle.script)?;
    for asset in &bundle.assets {
        let path = dir.join(&asset.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &asset.bytes)?;
    }
    Ok(())
}

fn is_secret_slot(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn check_asset_path(field: &str, path: &str) -> Result<()> {
    let ok_chars = path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c));
    if path.is_empty()
        || path.len() > 128
        || path.starts_with('/')
        || !ok_chars
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(invalid(
            field,
            "must be a relative path inside the plugin directory",
        ));
    }
    Ok(())
}

/// `PLUGIN_SECRET_<ID>` for a named slot.
pub fn secret_env_key(id: &str) -> String {
    format!("PLUGIN_SECRET_{}", id.to_ascii_uppercase())
}

/// Keyring name for one plugin's slot.
///
/// `p{len:02}_{plugin}_{slot}`, with `-` in the plugin id written as `_`.
/// `{len}` is the plugin id's length before that rewrite, so `train` +
/// `civitai_token` and `train-civitai` + `token` do not share a name (the old
/// `p_<plugin>_<slot>` spelling did). Fits
/// [`crate::accounts::validate_secret_name`]: plugin ids are at most 48
/// characters and slots at most 32, so the name stays within 96.
pub fn secret_keyring_name(plugin_id: &str, slot_id: &str) -> String {
    format!(
        "p{:02}_{}_{}",
        plugin_id.len(),
        plugin_id.replace('-', "_"),
        slot_id
    )
}

fn safe_leaf(name: &str) -> Result<String> {
    if name.is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(Error::invalid("path", "the file name is not usable"));
    }
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_matches('.').to_owned();
    if safe.is_empty() || safe.chars().all(|c| c == '_') {
        return Err(Error::invalid("path", "the file name is not usable"));
    }
    Ok(safe)
}

fn path_leaf(path: &str) -> Result<&str> {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    let leaf = trimmed.rsplit(['/', '\\']).next().unwrap_or("");
    if leaf.is_empty() || leaf == "." || leaf == ".." {
        return Err(Error::invalid("path", "the file name is not usable"));
    }
    Ok(leaf)
}

fn stage_local(spec: &InputSpec, raw: &str) -> Result<StagedUpload> {
    let text = raw.trim();
    if text.is_empty()
        || text.len() > 4096
        || text.contains('\0')
        || text.contains('\n')
        || text.starts_with('-')
    {
        return Err(Error::invalid(&spec.id, "the local path is not usable"));
    }
    let remote_rel = match spec.kind {
        InputKind::Dir => format!("inputs/{}", spec.id),
        InputKind::File | InputKind::Notebook => {
            let leaf = path_leaf(text)?;
            if spec.kind == InputKind::Notebook && !leaf.to_ascii_lowercase().ends_with(".ipynb") {
                return Err(Error::invalid(&spec.id, "expected a .ipynb file"));
            }
            let safe = safe_leaf(leaf)?;
            format!("inputs/{}/{safe}", spec.id)
        }
        _ => return Err(Error::invalid(&spec.id, "wrong type")),
    };
    Ok(StagedUpload {
        local: PathBuf::from(text),
        remote_rel,
        is_dir: spec.kind == InputKind::Dir,
    })
}

/// Expands staged directories into individual files. Symlinks are rejected.
/// File bytes are not read here and nothing is executed.
pub fn expand_uploads(staged: &[StagedUpload]) -> Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    let mut total = 0u64;
    for item in staged {
        if item.is_dir {
            walk_dir(&item.local, &item.remote_rel, &mut out, &mut total)?;
        } else {
            push_file(&item.local, &item.remote_rel, &mut out, &mut total)?;
        }
    }
    Ok(out)
}

fn push_file(
    local: &Path,
    remote_rel: &str,
    out: &mut Vec<(PathBuf, String)>,
    total: &mut u64,
) -> Result<()> {
    let meta = std::fs::symlink_metadata(local).map_err(|_| {
        Error::invalid("path", "file not found").with("path", local.display().to_string())
    })?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(
            Error::invalid("path", "must be a regular file (no symlinks)")
                .with("path", local.display().to_string()),
        );
    }
    if meta.len() > MAX_UPLOAD_FILE_BYTES {
        return Err(
            Error::invalid("path", "file too large").with("path", local.display().to_string())
        );
    }
    *total = total.saturating_add(meta.len());
    if *total > MAX_UPLOAD_TOTAL_BYTES || out.len() >= MAX_UPLOAD_FILES {
        return Err(Error::invalid("path", "too many or too large files"));
    }
    out.push((local.to_owned(), remote_rel.to_owned()));
    Ok(())
}

fn walk_dir(
    dir: &Path,
    remote_rel: &str,
    out: &mut Vec<(PathBuf, String)>,
    total: &mut u64,
) -> Result<()> {
    let meta = std::fs::symlink_metadata(dir).map_err(|_| {
        Error::invalid("path", "file not found").with("path", dir.display().to_string())
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Error::invalid("path", "must be a directory (no symlinks)")
            .with("path", dir.display().to_string()));
    }
    let entries = std::fs::read_dir(dir).map_err(|e| {
        Error::new(ErrorKind::Io)
            .with("path", dir.display().to_string())
            .detail(e.to_string())
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::new(ErrorKind::Io).detail(e.to_string()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let safe = safe_leaf(&name)?;
        let child_rel = format!("{remote_rel}/{safe}");
        let child_meta = std::fs::symlink_metadata(entry.path()).map_err(|e| {
            Error::new(ErrorKind::Io)
                .with("path", entry.path().display().to_string())
                .detail(e.to_string())
        })?;
        if child_meta.file_type().is_symlink() {
            return Err(
                Error::invalid("path", "must be a regular file (no symlinks)")
                    .with("path", entry.path().display().to_string()),
            );
        }
        if child_meta.is_dir() {
            walk_dir(&entry.path(), &child_rel, out, total)?;
        } else if child_meta.is_file() {
            push_file(&entry.path(), &child_rel, out, total)?;
        }
    }
    Ok(())
}

/// Writes asset bytes under `scratch` so they can be uploaded. Does not execute them.
pub fn materialize_assets(assets: &[PluginAsset], scratch: &Path) -> Result<Vec<StagedUpload>> {
    let mut staged = Vec::with_capacity(assets.len());
    for asset in assets {
        check_asset_path("assets", &asset.path)?;
        if asset.bytes.len() as u64 > MAX_ASSET_BYTES {
            return Err(invalid("assets", "file too large"));
        }
        let dest = scratch.join(&asset.path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &asset.bytes)?;
        staged.push(StagedUpload {
            local: dest,
            remote_rel: format!("assets/{}", asset.path),
            is_dir: false,
        });
    }
    Ok(staged)
}

/// Host capabilities and named slots, as env pairs plus the secret values to mask.
/// `lookup` reads the keyring. A missing optional secret is omitted.
pub fn capability_env(
    manifest: &Manifest,
    consent: &CapabilityConsent,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<(Vec<(String, String)>, Vec<String>)> {
    let mut env = Vec::new();
    let mut secrets = Vec::new();
    for need in &manifest.needs {
        match need {
            Capability::HfToken => {
                if let Some(value) = need.secret_name().and_then(&lookup) {
                    if let Some(key) = need.env_key() {
                        env.push((key.to_owned(), value.clone()));
                    }
                    secrets.push(value);
                }
            }
            Capability::GcpAuth => {
                if !consent.gcp_auth {
                    return Err(Error::invalid(
                        "gcp_auth",
                        "this plugin needs Google Cloud auth, which you have not allowed",
                    ));
                }
                if let Some(key) = need.env_key() {
                    env.push((key.to_owned(), "1".to_owned()));
                }
            }
            Capability::Drive => {
                if !consent.drive {
                    return Err(Error::invalid(
                        "drive",
                        "this plugin needs Google Drive, which you have not allowed",
                    ));
                }
                if let Some(key) = need.env_key() {
                    env.push((key.to_owned(), "1".to_owned()));
                }
            }
        }
    }
    for slot in &manifest.secrets {
        let name = secret_keyring_name(&manifest.id, &slot.id);
        match lookup(&name) {
            Some(value) => {
                env.push((secret_env_key(&slot.id), value.clone()));
                secrets.push(value);
            }
            None if slot.required => {
                return Err(Error::invalid(
                    &slot.id,
                    "save this secret in Plugins before starting",
                ));
            }
            None => {}
        }
    }
    Ok((env, secrets))
}

/// Why a plugin run stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    Succeeded,
    Failed,
    Cancelled,
}

/// What to do with the runtime after the script stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterRun {
    /// Leave the runtime as it is.
    Leave,
    /// Run the Keep Plan already attached to the session, and leave it up.
    SyncKeep,
    /// Stop the runtime. `save_keep` runs the Keep Plan first.
    Stop { save_keep: bool },
}

/// What happens to the runtime after the script stops.
///
/// Job mode ignores `stop_on_failure`. A runtime this run opened stops on
/// failure unless `keep_on_failure` is set, and stops after a successful run
/// once the Keep Plan has been saved. An existing session is never stopped.
/// A service still uses `stop_on_failure`.
pub fn after_run(
    mode: RunMode,
    provisioned: bool,
    end: RunEnd,
    stop_on_failure: bool,
    keep_on_failure: bool,
) -> AfterRun {
    match end {
        RunEnd::Cancelled if provisioned => AfterRun::Stop { save_keep: false },
        RunEnd::Cancelled => AfterRun::Leave,
        RunEnd::Succeeded if mode == RunMode::Job && provisioned => {
            AfterRun::Stop { save_keep: true }
        }
        RunEnd::Succeeded if mode == RunMode::Job => AfterRun::SyncKeep,
        RunEnd::Succeeded => AfterRun::Leave,
        RunEnd::Failed if mode == RunMode::Job && keep_on_failure => AfterRun::Leave,
        RunEnd::Failed if mode == RunMode::Job && provisioned => {
            AfterRun::Stop { save_keep: false }
        }
        RunEnd::Failed if mode == RunMode::Job => AfterRun::Leave,
        RunEnd::Failed if provisioned && stop_on_failure => AfterRun::Stop { save_keep: false },
        RunEnd::Failed => AfterRun::Leave,
    }
}

pub fn redact_json(value: &mut serde_json::Value, secrets: &[String]) {
    match value {
        serde_json::Value::String(s) => *s = redact(s, secrets),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| redact_json(v, secrets)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|v| redact_json(v, secrets)),
        _ => {}
    }
}

/// True when `text` contains a secret that [`redact`] would replace.
pub fn contains_secret(text: &str, secrets: &[String]) -> bool {
    secrets
        .iter()
        .any(|s| s.len() >= 4 && text.contains(s.as_str()))
}

/// Replaces every occurrence of a secret in `text` with `***`.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut ordered: Vec<&String> = secrets.iter().filter(|s| s.len() >= 4).collect();
    // Longest first, so a secret that contains another is replaced whole.
    ordered.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_owned();
    for s in ordered {
        out = out.replace(s.as_str(), "***");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
api = 1
id = "demo"
version = "0.1.0"
[name]
en = "Demo"
zh-CN = "演示"
[run]
script = "main.py"
"#;

    fn with(extra: &str) -> String {
        format!("{MINIMAL}\n{extra}")
    }

    /// Inserts top-level keys before the first table.
    fn top(keys: &str) -> String {
        MINIMAL.replace("[name]", &format!("{keys}\n[name]"))
    }

    #[test]
    fn bundled_llama_manifest_is_valid() {
        let store = PluginStore::load(None, None).unwrap();
        let p = store.get("llama-cpp").expect("bundled plugin");
        assert_eq!(p.source, PluginSource::Bundled);
        assert!(p.manifest.needs.contains(&Capability::HfToken));
        assert!(p.manifest.keep.is_empty());
        assert!(!p.script.is_empty());
        assert_eq!(
            p.manifest.name.get("zh-TW").map(|s| !s.is_empty()),
            Some(true)
        );
        for tag in ["ja-JP", "ko-KR"] {
            assert_ne!(
                p.manifest.name.get(tag),
                p.manifest.name.get("en"),
                "the app asks for {tag}, which the manifest's bare language key answers"
            );
        }
        let ep = &p.manifest.endpoints[0];
        assert_eq!(ep.port_input.as_deref(), Some("port"));
    }

    #[test]
    fn bundled_comfy_manifest_is_valid() {
        let store = PluginStore::load(None, None).unwrap();
        let p = store.get("comfyui").expect("bundled plugin");
        assert_eq!(p.source, PluginSource::Bundled);
        assert!(p.manifest.needs.contains(&Capability::HfToken));
        assert!(!p.script.is_empty());
        assert_eq!(p.manifest.stages.len(), 5);
        assert!(p.manifest.run.keep_alive, "the server keeps running");
        let ep = &p.manifest.endpoints[0];
        assert_eq!(ep.port_input.as_deref(), Some("port"));
        assert_eq!(p.manifest.keep[0].path, "/content/ComfyUI/output");
    }

    #[test]
    fn localized_fallbacks() {
        let m = Manifest::parse(MINIMAL).unwrap();
        assert_eq!(m.name.get("zh-CN"), Some("演示"));
        assert_eq!(m.name.get("zh-TW"), Some("演示"), "same language before en");
        assert_eq!(m.name.get("fr"), Some("Demo"));
    }

    #[test]
    fn a_server_may_say_it_keeps_running() {
        let text = MINIMAL.replace(
            "script = \"main.py\"",
            "script = \"main.py\"
keep_alive = true",
        );
        let manifest = Manifest::parse(&text).expect("a manifest with keep_alive");
        manifest.validate().expect("keep_alive is valid");
        assert!(manifest.run.keep_alive);
        let bad = MINIMAL.replace(
            "script = \"main.py\"",
            "script = \"main.py\"
keep_alive = \"yes\"",
        );
        let err = Manifest::parse(&bad).expect_err("keep_alive must be a boolean");
        assert!(err.is(ErrorKind::ManifestInvalid), "{err}");
    }

    #[test]
    fn strict_validation() {
        let bad = [
            MINIMAL.replace("api = 1", "api = 3"),
            MINIMAL.replace("api = 1\n", ""),
            MINIMAL.replace("id = \"demo\"", "id = \"../x\""),
            MINIMAL.replace("en = \"Demo\"", "fr = \"Demo\""),
            MINIMAL.replace("main.py", "../main.py"),
            MINIMAL.replace("main.py", "main.exe"),
            top("unknown = 1"),
            with("[[inputs]]\nid = \"a\"\ntype = \"secret\"\ndefault = \"x\""),
            with("[[inputs]]\nid = \"q\"\ntype = \"select\"\noptions = [\"a\"]\ndefault = \"b\""),
            with(
                "[[inputs]]\nid = \"a-b\"\ntype = \"text\"\n[[inputs]]\nid = \"a_b\"\ntype = \"text\"",
            ),
            with(
                "[[inputs]]\nid = \"p\"\ntype = \"text\"\n[[endpoints]]\nid = \"e\"\nport_input = \"p\"",
            ),
            with("[[endpoints]]\nid = \"e\"\nport = 8080"),
            with("[[keep]]\npath = \"relative/path\""),
            top("needs = [\"aws_keys\"]"),
            with("[provision]\nmin_vram_mib = 1000"),
        ];
        for (i, text) in bad.iter().enumerate() {
            let err = Manifest::parse(text).expect_err(&format!("case {i} should fail"));
            assert!(err.is(ErrorKind::ManifestInvalid), "case {i}: {err}");
        }
        assert_eq!(
            Manifest::parse(&MINIMAL.replace("api = 1", "api = 3"))
                .unwrap_err()
                .param("field"),
            Some("api")
        );
        let v2 = Manifest::parse(&MINIMAL.replace("api = 1", "api = 2")).unwrap();
        assert_eq!(v2.api, 2);
        assert_eq!(v2.run.mode, RunMode::Service);
        let ok = top("needs = [\"hf_token\"]")
            + "\n[[keep]]\npath = \"/content/out\"\n[keep.label]\nen = \"Outputs\"\n";
        assert!(Manifest::parse(&top("needs = [\"hf_token\", \"hf_token\"]")).is_err());
        let m = Manifest::parse(&ok).unwrap();
        assert_eq!(m.keep[0].path, "/content/out");
    }

    #[test]
    fn inputs_resolve_to_env_and_secrets() {
        let m = Manifest::parse(&with(
            r#"
[[inputs]]
id = "repo"
type = "hf-model"
required = true
[[inputs]]
id = "quant"
type = "select"
options = ["auto", "Q8_0"]
default = "auto"
[[inputs]]
id = "port"
type = "number"
default = 8000
min = 1
max = 65535
[[inputs]]
id = "api-key"
type = "secret"
[[inputs]]
id = "mtp"
type = "bool"
default = true
[[endpoints]]
id = "api"
port_input = "port"
path = "/v1"
health = "/health"
kind = "openai"
"#,
        ))
        .unwrap();
        let mut given = BTreeMap::new();
        assert!(
            resolve_inputs(&m, &given)
                .unwrap_err()
                .is(ErrorKind::InvalidInput)
        );
        given.insert(
            "repo".into(),
            InputValue::Text(" unsloth/Qwen3-0.6B-GGUF ".into()),
        );
        given.insert("api-key".into(), InputValue::Text("sk-secret".into()));
        given.insert("port".into(), InputValue::Text("8001".into()));
        let r = resolve_inputs(&m, &given).unwrap();
        let env: BTreeMap<_, _> = r.env.iter().cloned().collect();
        assert_eq!(env["PLUGIN_INPUT_REPO"], "unsloth/Qwen3-0.6B-GGUF");
        assert_eq!(env["PLUGIN_INPUT_QUANT"], "auto");
        assert_eq!(env["PLUGIN_INPUT_PORT"], "8001");
        assert_eq!(env["PLUGIN_INPUT_API_KEY"], "sk-secret");
        assert_eq!(env["PLUGIN_INPUT_MTP"], "1");
        assert_eq!(r.secrets, vec!["sk-secret".to_owned()]);
        let eps = resolve_endpoints(&m, &r.values).unwrap();
        assert_eq!(eps[0].remote_port, 8001);
        given.insert("port".into(), InputValue::Number(70000.0));
        assert!(resolve_inputs(&m, &given).is_err());
        given.insert("port".into(), InputValue::Number(8000.0));
        given.insert("repo".into(), InputValue::Text("not a repo".into()));
        assert!(resolve_inputs(&m, &given).is_err());
        given.insert("bogus".into(), InputValue::Bool(true));
        assert!(resolve_inputs(&m, &given).is_err());
        assert_eq!(redact("key=sk-secret!", &r.secrets), "key=***!");
        let mut v = serde_json::json!({"a": ["x sk-secret", {"b": "sk-secret"}], "n": 1});
        redact_json(&mut v, &r.secrets);
        assert_eq!(v, serde_json::json!({"a": ["x ***", {"b": "***"}], "n": 1}));
        assert!(contains_secret("..sk-secret..", &r.secrets));
        assert!(!contains_secret("nothing", &r.secrets));
        assert!(
            !contains_secret("ab", &["ab".to_owned()]),
            "too short to redact"
        );
    }

    #[test]
    fn job_manifest_accepts_files_secrets_assets_and_rejects_bad_combinations() {
        let text = r#"
api = 2
id = "train"
version = "0.1.0"
needs = ["hf_token", "gcp_auth", "drive"]
[name]
en = "Train"
[run]
script = "main.py"
mode = "job"
keep_on_failure = true
[[inputs]]
id = "data"
type = "file"
required = true
[[inputs]]
id = "folder"
type = "dir"
[[inputs]]
id = "book"
type = "notebook"
[[inputs]]
id = "prompt"
type = "textarea"
[[secrets]]
id = "civitai_token"
required = true
[secrets.label]
en = "Civitai"
[[assets]]
path = "requirements.txt"
[[assets]]
path = "workflows/demo.json"
[[keep]]
path = "/content/out"
"#;
        let m = Manifest::parse(text).unwrap();
        assert_eq!(m.run.mode, RunMode::Job);
        assert!(m.run.keep_on_failure);
        assert!(m.needs.contains(&Capability::GcpAuth));
        assert_eq!(m.assets.len(), 2);
        assert_eq!(
            secret_keyring_name(&m.id, "civitai_token"),
            "p05_train_civitai_token"
        );

        let both = text.replace("mode = \"job\"", "mode = \"job\"\nkeep_alive = true");
        assert!(Manifest::parse(&both).is_err());
        let service_keep = MINIMAL.replace(
            "script = \"main.py\"",
            "script = \"main.py\"\nkeep_on_failure = true",
        );
        assert!(Manifest::parse(&service_keep).is_err());
        let bad_slot = text.replace("id = \"civitai_token\"", "id = \"Civitai\"");
        assert!(Manifest::parse(&bad_slot).is_err());
        let linked = text.replace("path = \"requirements.txt\"", "path = \"../secret\"");
        assert!(Manifest::parse(&linked).is_err());

        let mut given = BTreeMap::new();
        given.insert("data".into(), InputValue::Text("/tmp/My Notes.txt".into()));
        given.insert("book".into(), InputValue::Text("/tmp/run.ipynb".into()));
        given.insert(
            "prompt".into(),
            InputValue::Text("line one\nline two".into()),
        );
        let resolved = resolve_inputs(&m, &given).unwrap();
        let rels: BTreeMap<_, _> = resolved.vm_rels.iter().cloned().collect();
        assert_eq!(rels["PLUGIN_INPUT_DATA"], "inputs/data/My_Notes.txt");
        assert_eq!(rels["PLUGIN_INPUT_BOOK"], "inputs/book/run.ipynb");
        assert!(!resolved.env.iter().any(|(_, v)| v.contains("/tmp/")));
        assert_eq!(
            resolved
                .env
                .iter()
                .find(|(k, _)| k == "PLUGIN_INPUT_PROMPT")
                .map(|(_, v)| v.as_str()),
            Some("line one\nline two")
        );
        let vm = vm_env("/root/.colab-playground/runs/r1", &resolved.vm_rels);
        assert!(
            vm.iter()
                .any(|(_, v)| v == "/root/.colab-playground/runs/r1/inputs/data/My_Notes.txt")
        );
        given.insert("book".into(), InputValue::Text("/tmp/notes.txt".into()));
        assert!(resolve_inputs(&m, &given).is_err());
        given.insert("book".into(), InputValue::Text("/tmp/run.ipynb".into()));
        given.insert(
            "data".into(),
            InputValue::Text("$(touch /tmp/pwn).txt".into()),
        );
        let sneaky = resolve_inputs(&m, &given).unwrap();
        assert!(
            sneaky
                .uploads
                .iter()
                .all(|u| !u.remote_rel.contains("$(") && !u.remote_rel.contains("touch"))
        );

        assert!(
            capability_env(&m, &CapabilityConsent::default(), |_| None)
                .unwrap_err()
                .is(ErrorKind::InvalidInput)
        );
        let consent = CapabilityConsent {
            gcp_auth: true,
            drive: true,
        };
        let err = capability_env(&m, &consent, |_| None).unwrap_err();
        assert!(err.is(ErrorKind::InvalidInput));
        let (env, secrets) = capability_env(&m, &consent, |name| {
            (name == "p05_train_civitai_token" || name == "hf_token").then(|| format!("sek-{name}"))
        })
        .unwrap();
        let joined = env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("PLUGIN_GCP_AUTH=1"));
        assert!(joined.contains("PLUGIN_DRIVE=1"));
        assert!(joined.contains("PLUGIN_SECRET_CIVITAI_TOKEN=sek-p05_train_civitai_token"));
        let shown = redact(&joined, &secrets);
        assert!(!shown.contains("sek-"));
        assert!(shown.contains("***"));
    }

    #[test]
    fn secret_slot_names_do_not_share_the_old_keyring_slot() {
        let old = |plugin: &str, slot: &str| format!("p_{}_{}", plugin.replace('-', "_"), slot);
        assert_eq!(
            old("train-civitai", "token"),
            old("train", "civitai_token"),
            "the old encoding gave both plugins the same keyring slot"
        );
        let later = secret_keyring_name("train-civitai", "token");
        let earlier = secret_keyring_name("train", "civitai_token");
        assert_ne!(later, earlier);
        assert_eq!(later, "p13_train_civitai_token");
        assert_eq!(earlier, "p05_train_civitai_token");
        for name in [&later, &earlier] {
            assert!(
                crate::accounts::validate_secret_name(name).is_ok(),
                "{name}"
            );
            assert!(name.len() <= 96, "{name}");
        }
    }

    #[test]
    fn a_job_stops_the_runtime_it_opened_and_a_failure_can_keep_it() {
        assert_eq!(
            after_run(RunMode::Job, true, RunEnd::Succeeded, true, false),
            AfterRun::Stop { save_keep: true }
        );
        assert_eq!(
            after_run(RunMode::Job, false, RunEnd::Succeeded, true, false),
            AfterRun::SyncKeep,
            "an existing runtime is not stopped"
        );
        assert_eq!(
            after_run(RunMode::Job, true, RunEnd::Failed, true, true),
            AfterRun::Leave
        );
        assert_eq!(
            after_run(RunMode::Job, true, RunEnd::Failed, true, false),
            AfterRun::Stop { save_keep: false }
        );
        assert_eq!(
            after_run(RunMode::Job, true, RunEnd::Failed, false, false),
            AfterRun::Stop { save_keep: false },
            "a job ignores the old stop-on-failure switch"
        );
        assert_eq!(
            after_run(RunMode::Job, false, RunEnd::Failed, true, false),
            AfterRun::Leave,
            "an existing session is never stopped when a job fails"
        );
        assert_eq!(
            after_run(RunMode::Service, true, RunEnd::Failed, true, false),
            AfterRun::Stop { save_keep: false }
        );
        assert_eq!(
            after_run(RunMode::Service, true, RunEnd::Failed, false, false),
            AfterRun::Leave,
            "a service still uses stop-on-failure"
        );
        assert_eq!(
            after_run(RunMode::Service, true, RunEnd::Succeeded, true, false),
            AfterRun::Leave
        );
        assert_eq!(
            after_run(RunMode::Service, true, RunEnd::Cancelled, false, false),
            AfterRun::Stop { save_keep: false }
        );
    }

    #[test]
    fn uploads_reject_symlinks_and_assets_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, "hello").unwrap();
        let staged = vec![StagedUpload {
            local: file.clone(),
            remote_rel: "inputs/data/notes.txt".into(),
            is_dir: false,
        }];
        let expanded = expand_uploads(&staged).unwrap();
        assert_eq!(expanded[0].1, "inputs/data/notes.txt");

        let link = tmp.path().join("link.txt");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&file, &link).unwrap();
            let bad = vec![StagedUpload {
                local: link,
                remote_rel: "inputs/data/link.txt".into(),
                is_dir: false,
            }];
            assert!(expand_uploads(&bad).is_err());
        }

        let manifest = r#"
api = 1
id = "demo"
version = "0.1.0"
[name]
en = "Demo"
[run]
script = "main.py"
[[assets]]
path = "requirements.txt"
[[assets]]
path = "workflows/demo.json"
"#;
        std::fs::write(tmp.path().join("plugin.toml"), manifest).unwrap();
        std::fs::write(tmp.path().join("main.py"), "print('hi')\n").unwrap();
        std::fs::create_dir(tmp.path().join("workflows")).unwrap();
        std::fs::write(tmp.path().join("requirements.txt"), "torch\n").unwrap();
        std::fs::write(tmp.path().join("workflows").join("demo.json"), "{}\n").unwrap();
        let bundle = load_dir(tmp.path()).unwrap();
        assert_eq!(bundle.assets.len(), 2);
        assert_eq!(bundle.assets[0].bytes, b"torch\n");

        #[cfg(unix)]
        {
            let sneaky = tempfile::tempdir().unwrap();
            std::fs::write(sneaky.path().join("plugin.toml"), manifest).unwrap();
            std::fs::write(sneaky.path().join("main.py"), "print('hi')\n").unwrap();
            std::fs::create_dir(sneaky.path().join("workflows")).unwrap();
            std::fs::write(sneaky.path().join("requirements.txt"), "torch\n").unwrap();
            std::os::unix::fs::symlink(
                sneaky.path().join("requirements.txt"),
                sneaky.path().join("workflows").join("demo.json"),
            )
            .unwrap();
            assert!(load_dir(sneaky.path()).is_err());
        }
    }

    #[test]
    fn git_urls() {
        for ok in [
            "https://github.com/u/p.git",
            "ssh://git@github.com/u/p",
            "git@github.com:u/p.git",
        ] {
            assert!(validate_git_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "--upload-pack=x",
            "ext::sh -c touch% /tmp/pwned",
            "file:///etc",
            "/local/path",
            "https://",
            "http://example.com/p",
            "git@host:-oProxyCommand=x",
            "https://host/p p",
        ] {
            assert!(validate_git_url(bad).is_err(), "{bad}");
        }
        let args = git_clone_args("https://github.com/u/p.git", "/tmp/x").unwrap();
        let dd = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(&args[dd + 1..], ["https://github.com/u/p.git", "/tmp/x"]);
        assert!(args.contains(&"protocol.ext.allow=never".to_owned()));
    }

    #[test]
    fn folder_install_enable_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("plugin.toml"), MINIMAL).unwrap();
        std::fs::write(src.join("main.py"), "print('hi')\n").unwrap();
        let data = tmp.path().join("data");
        let mut store = PluginStore::load(
            Some(&data.join("plugins")),
            Some(&data.join("plugins.json")),
        )
        .unwrap();
        let preview = store
            .inspect_folder(
                &src,
                &data.join("staging"),
                PluginOrigin::Folder {
                    path: src.display().to_string(),
                },
            )
            .unwrap();
        assert_eq!(preview.manifest.id, "demo");
        assert_eq!(preview.replaces_version, None);
        assert!(preview.staged_dir.exists());
        store.install(&preview).unwrap();
        assert!(!preview.staged_dir.exists(), "the staged copy is used up");
        store.set_enabled("demo", false).unwrap();
        let reloaded = PluginStore::load(
            Some(&data.join("plugins")),
            Some(&data.join("plugins.json")),
        )
        .unwrap();
        assert!(reloaded.get("demo").is_some());
        assert!(!reloaded.enabled("demo"));
        assert!(reloaded.enabled("llama-cpp"));
        let mut reloaded = reloaded;
        reloaded.remove("demo").unwrap();
        assert!(reloaded.remove("llama-cpp").is_err());
        assert!(!data.join("plugins").join("demo").exists());
    }

    fn folder_and_store(tmp: &Path, manifest: &str) -> (PathBuf, PathBuf, PluginStore) {
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("plugin.toml"), manifest).unwrap();
        std::fs::write(src.join("main.py"), "print('hi')\n").unwrap();
        let data = tmp.join("data");
        let store = PluginStore::load(
            Some(&data.join("plugins")),
            Some(&data.join("plugins.json")),
        )
        .unwrap();
        (src, data.join("staging"), store)
    }

    fn folder_origin(src: &Path) -> PluginOrigin {
        PluginOrigin::Folder {
            path: src.display().to_string(),
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn a_failed_install_leaves_no_staged_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, staging, mut store) = folder_and_store(tmp.path(), MINIMAL);

        // The staged manifest no longer is the one that was previewed.
        let preview = store
            .inspect_folder(&src, &staging, folder_origin(&src))
            .unwrap();
        std::fs::write(
            preview.staged_dir.join("plugin.toml"),
            MINIMAL.replace("0.1.0", "0.2.0"),
        )
        .unwrap();
        let error = store.install(&preview).unwrap_err();
        assert!(error.is(ErrorKind::ManifestInvalid), "{error:?}");
        assert_eq!(entries(&staging), Vec::<String>::new());

        // The id belongs to a bundled plugin.
        let mut preview = store
            .inspect_folder(&src, &staging, folder_origin(&src))
            .unwrap();
        let bundled = MINIMAL.replace("id = \"demo\"", "id = \"llama-cpp\"");
        std::fs::write(preview.staged_dir.join("plugin.toml"), &bundled).unwrap();
        preview.manifest = Manifest::parse(&bundled).unwrap();
        let error = store.install(&preview).unwrap_err();
        assert!(error.is(ErrorKind::PluginExists), "{error:?}");
        assert_eq!(entries(&staging), Vec::<String>::new());

        // The staged script cannot be read any more.
        let preview = store
            .inspect_folder(&src, &staging, folder_origin(&src))
            .unwrap();
        std::fs::remove_file(preview.staged_dir.join("main.py")).unwrap();
        assert!(store.install(&preview).is_err());
        assert_eq!(entries(&staging), Vec::<String>::new());
        assert!(store.get("demo").is_none());
    }

    #[test]
    fn a_discarded_preview_leaves_no_staged_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, staging, store) = folder_and_store(tmp.path(), MINIMAL);
        let preview = store
            .inspect_folder(&src, &staging, folder_origin(&src))
            .unwrap();
        let other = store
            .inspect_folder(&src, &staging, folder_origin(&src))
            .unwrap();
        discard_preview(&preview);
        assert!(!preview.staged_dir.exists());
        assert!(other.staged_dir.exists(), "only its own copy goes");

        // A preview that names some other directory removes nothing.
        let mut elsewhere = other.clone();
        elsewhere.staged_dir = src.clone();
        discard_preview(&elsewhere);
        assert!(src.join("plugin.toml").exists());
    }

    #[test]
    fn leftovers_in_staging_are_cleared() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, staging, store) = folder_and_store(tmp.path(), MINIMAL);
        clear_staging(&staging); // nothing there yet: not an error
        store
            .inspect_folder(&src, &staging, folder_origin(&src))
            .unwrap();
        std::fs::create_dir_all(staging.join("git-0123456789abcdef").join(".git")).unwrap();
        std::fs::create_dir_all(staging.join("kept")).unwrap();
        assert_eq!(entries(&staging).len(), 3);
        clear_staging(&staging);
        assert_eq!(entries(&staging), ["kept"]);
    }

    #[test]
    fn an_unfinished_checkout_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        for stopped in [
            Error::cancelled(),
            Error::new(ErrorKind::Timeout),
            Error::new(ErrorKind::GitFailed),
        ] {
            let dest = tmp.path().join("git-partial");
            std::fs::create_dir_all(dest.join(".git")).unwrap();
            let kind = stopped.kind;
            let error = finished_checkout(dest.clone(), Err(stopped)).unwrap_err();
            assert_eq!(error.kind, kind);
            assert!(!dest.exists(), "{kind:?}");
        }
        let dest = tmp.path().join("git-complete");
        std::fs::create_dir_all(&dest).unwrap();
        assert_eq!(finished_checkout(dest.clone(), Ok(())).unwrap(), dest);
        assert!(dest.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_scripts_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("plugin.toml"), MINIMAL).unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("secret.txt"), tmp.path().join("main.py"))
            .unwrap();
        assert!(
            load_dir(tmp.path())
                .unwrap_err()
                .is(ErrorKind::ManifestInvalid)
        );
    }
}
