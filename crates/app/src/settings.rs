//! User settings, persisted as JSON in the OS config dir.
//!
//! Loading never fails: a missing file gives defaults, an unreadable or malformed file is moved
//! aside (`settings.json.corrupt`) and gives defaults, and a field with a bad value falls back
//! to that field's default without touching the others. New fields must carry
//! `#[serde(default, deserialize_with = "lenient")]` so old files keep loading.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use colab_playground_core::{Defaults, Toolchain, paths::write_atomic};
use serde::{Deserialize, Deserializer, Serialize};

/// Environment variable that replaces the config directory (tests point it at a tempdir).
pub const CONFIG_DIR_ENV: &str = "COLAB_PLAYGROUND_CONFIG_DIR";
pub const APP_DIR_NAME: &str = "colab-playground";
pub const SETTINGS_FILE: &str = "settings.json";
pub const CORRUPT_SUFFIX: &str = ".corrupt";

/// Declares [`LanguagePref`] from one list, so the settings file (serde) and the select read
/// the same value for each choice.
macro_rules! language_prefs {
    ($first:ident => $first_value:literal, $($variant:ident => $value:literal),+ $(,)?) => {
        /// The language the user picked; `System` resolves through the OS locale.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum LanguagePref {
            #[default]
            #[serde(rename = $first_value)]
            $first,
            $(
                #[serde(rename = $value)]
                $variant,
            )+
        }

        impl LanguagePref {
            /// Every choice, in the order the settings page offers them.
            pub const ALL: [LanguagePref; [$first_value, $($value),+].len()] =
                [LanguagePref::$first, $(LanguagePref::$variant),+];

            /// The stable value used in the settings file and as the select's value.
            pub fn value(self) -> &'static str {
                match self {
                    LanguagePref::$first => $first_value,
                    $(LanguagePref::$variant => $value,)+
                }
            }
        }
    };
}

language_prefs! {
    System => "system",
    EnUs => "en-US",
    ZhCn => "zh-CN",
    ZhTw => "zh-TW",
    JaJp => "ja-JP",
    KoKr => "ko-KR",
}

impl LanguagePref {
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|pref| pref.value() == value)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppearancePref {
    /// Follow the operating system, including live changes.
    #[default]
    System,
    Light,
    Dark,
}

/// Everything the user can set. Add fields with `#[serde(default, deserialize_with = "lenient")]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, deserialize_with = "lenient")]
    pub language: LanguagePref,
    #[serde(default, deserialize_with = "lenient")]
    pub appearance: AppearancePref,
    /// Policy defaults for new runtimes (core's `Defaults`; pushed to the backend). Read field by
    /// field: a bad value resets only that field, and the reset is reported.
    #[serde(default, skip_deserializing)]
    pub defaults: Defaults,
    /// WSL distribution the CLI runs in (Windows); `None` = the default distribution.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub wsl_distro: Option<String>,
    /// Path of `colab` on the host; `None` = found automatically.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub colab_path: Option<String>,
    /// Path of the CLI's Python interpreter; used only together with `colab_path`.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub colab_python: Option<String>,
    /// Check GitHub for a newer release automatically (once a day).
    #[serde(default, deserialize_with = "lenient")]
    pub check_updates: OnByDefault,
    /// When an update check last got an answer (seconds since the Unix epoch, backend clock).
    /// A check that failed is not recorded, so the next start tries again.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub update_checked_at: Option<u64>,
    /// The newest release a check found, while it is newer than this build.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub update_latest: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub update_dismissed: Option<String>,
    /// A release whose download failed its signature or checksum check. The app never points
    /// at its files again (a download by hand would skip the check), across restarts too.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub update_unverified: Option<String>,
    /// Runtimes and deployments that came up: the app asks for a star or feedback only after a
    /// few.
    #[serde(default, deserialize_with = "lenient")]
    pub ready_count: u32,
    /// How many times the app asked for a star or feedback.
    #[serde(default, deserialize_with = "lenient")]
    pub feedback_asked: u32,
    /// When it last asked (seconds since the Unix epoch, backend clock).
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub feedback_asked_at: Option<u64>,
    /// The user opened the project page or the issue form from the prompt, so it never asks
    /// again.
    #[serde(default, deserialize_with = "lenient")]
    pub feedback_answered: bool,
    /// Whether the local agent endpoint answers at all. Off unless the user turns it on: it is a
    /// way into this app from another program.
    #[serde(default, deserialize_with = "lenient")]
    pub mcp_enabled: bool,
    /// The loopback port it listens on.
    #[serde(default, deserialize_with = "lenient")]
    pub mcp_port: McpPort,
    /// Whether an agent is asked before a call that can spend more: opening a runtime, or moving
    /// its auto-stop (including turning that stop off). On unless the user turns it off. Stopping
    /// a runtime, and running a plugin on one that is already open, never ask either way.
    #[serde(default, deserialize_with = "lenient")]
    pub mcp_confirm: OnByDefault,
}

/// The port the agent endpoint listens on unless the user changed it. A port of its own type so a
/// file that does not mention it keeps the default, the way [`OnByDefault`] keeps a switch on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpPort(pub u16);

impl Default for McpPort {
    fn default() -> Self {
        McpPort(8765)
    }
}

/// A switch that is on unless the user turned it off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OnByDefault(pub bool);

impl Default for OnByDefault {
    fn default() -> Self {
        OnByDefault(true)
    }
}

/// `defaults` from stored JSON, keeping every field that parses; returns the names of the
/// fields that did not (they take their default).
pub fn read_defaults(value: &serde_json::Value) -> (Defaults, Vec<String>) {
    if let Ok(defaults) = Defaults::deserialize(value) {
        return (defaults, Vec::new());
    }
    let serde_json::Value::Object(stored) = value else {
        log::warn!("settings: `defaults` is not an object; using the defaults");
        return (Defaults::default(), vec!["defaults".to_owned()]);
    };
    let mut kept = serde_json::Map::new();
    let mut reset = Vec::new();
    for (key, field) in stored {
        let mut candidate = kept.clone();
        candidate.insert(key.clone(), field.clone());
        if Defaults::deserialize(&serde_json::Value::Object(candidate)).is_ok() {
            kept.insert(key.clone(), field.clone());
        } else {
            log::warn!("settings: defaults.{key} = {field} is not valid; using its default");
            reset.push(key.clone());
        }
    }
    let defaults =
        Defaults::deserialize(&serde_json::Value::Object(kept)).unwrap_or_else(|error| {
            log::error!("settings: the kept defaults did not parse ({error})");
            Defaults::default()
        });
    (defaults, reset)
}

/// A field's value, or its default when the stored value does not parse.
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(&value).unwrap_or_else(|error| {
        log::warn!("settings: ignoring {value} ({error}); using the default");
        T::default()
    }))
}

impl Settings {
    /// Settings from a JSON document; fails only when it is not a JSON object.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        Self::from_json_reporting(text).map(|(settings, _)| settings)
    }

    /// Like [`Self::from_json`], also naming the `defaults` fields that were reset.
    pub fn from_json_reporting(text: &str) -> Result<(Self, Vec<String>), serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "the settings file is not a JSON object",
            ));
        }
        let (defaults, reset) = value.get("defaults").map(read_defaults).unwrap_or_default();
        let mut settings: Settings = serde_json::from_value(value)?;
        settings.defaults = defaults;
        Ok((settings, reset))
    }

    /// Where the real backend finds the CLI: a toolchain when a `colab` path is set. The
    /// interpreter path is optional; without it core derives the interpreter from `colab`'s
    /// shebang (`Toolchain::from_colab_path`). An interpreter path alone is ignored.
    pub fn host_setup(&self) -> crate::backend::HostSetup {
        let toolchain = match (&self.colab_path, &self.colab_python) {
            (Some(colab), Some(python)) => Some(Toolchain {
                colab: colab.clone(),
                helper_python: python.clone(),
            }),
            (Some(colab), None) => Some(Toolchain::from_colab_path(colab.clone())),
            (None, _) => None,
        };
        crate::backend::HostSetup {
            wsl_distro: self.wsl_distro.clone(),
            toolchain,
        }
    }

    /// Pretty JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("settings serialize to JSON");
        text.push('\n');
        text
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    /// No config directory could be determined; nothing is read or saved.
    NoConfigDir,
    Missing,
    Loaded,
    /// The file could not be read or parsed; defaults are in use.
    Corrupt {
        error: String,
        /// Where the bad file was moved, if moving it worked.
        backup: Option<PathBuf>,
    },
}

#[derive(Clone, Debug)]
pub struct Loaded {
    pub settings: Settings,
    pub outcome: LoadOutcome,
    /// `defaults` fields that held invalid values and went back to their default.
    pub reset_defaults: Vec<String>,
}

impl Loaded {
    fn fresh(outcome: LoadOutcome) -> Self {
        Loaded {
            settings: Settings::default(),
            outcome,
            reset_defaults: Vec::new(),
        }
    }
}

/// The config directory: `override_dir` (the value of [`CONFIG_DIR_ENV`]) when set and
/// non-empty, otherwise `<OS config dir>/colab-playground`. Pure, so tests never read the
/// process environment.
pub fn config_dir(override_dir: Option<OsString>) -> Option<PathBuf> {
    match override_dir {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => dirs::config_dir().map(|dir| dir.join(APP_DIR_NAME)),
    }
}

/// A `file://` URL for `dir`, which is created first if missing, for `App::open_url` to show in
/// the system's file manager. Blocking: call it from a background task.
pub fn folder_url(dir: &Path) -> Result<String, String> {
    fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let absolute =
        std::path::absolute(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    url::Url::from_directory_path(&absolute)
        .map(String::from)
        .map_err(|()| format!("{} cannot be written as a URL", absolute.display()))
}

#[derive(Clone, Debug)]
pub struct SettingsStore {
    dir: Option<PathBuf>,
}

impl SettingsStore {
    /// A store in `dir`; `None` keeps settings in memory only.
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn file(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join(SETTINGS_FILE))
    }

    /// Reads the settings, tolerating a missing or corrupt file.
    pub fn load(&self) -> Loaded {
        let Some(file) = self.file() else {
            log::warn!("settings: no config directory; settings will not be saved");
            return Loaded::fresh(LoadOutcome::NoConfigDir);
        };
        let failure = match fs::read_to_string(&file) {
            Ok(text) => match Settings::from_json_reporting(&text) {
                Ok((settings, reset_defaults)) => {
                    log::info!("settings: loaded {}", file.display());
                    return Loaded {
                        settings,
                        outcome: LoadOutcome::Loaded,
                        reset_defaults,
                    };
                }
                Err(error) => error.to_string(),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                log::info!("settings: {} does not exist yet", file.display());
                return Loaded::fresh(LoadOutcome::Missing);
            }
            Err(error) => error.to_string(),
        };
        let mut backup_name = file.clone().into_os_string();
        backup_name.push(CORRUPT_SUFFIX);
        let backup = PathBuf::from(backup_name);
        let backup = match fs::rename(&file, &backup) {
            Ok(()) => Some(backup),
            Err(error) => {
                log::error!("settings: could not move {} aside: {error}", file.display());
                None
            }
        };
        log::warn!(
            "settings: {} is unreadable ({failure}); using defaults",
            file.display()
        );
        Loaded::fresh(LoadOutcome::Corrupt {
            error: failure,
            backup,
        })
    }

    /// Writes the settings atomically. A store without a directory writes nothing. Blocking:
    /// call it from a background task.
    pub fn save(&self, settings: &Settings) -> io::Result<()> {
        let Some(file) = self.file() else {
            return Ok(());
        };
        write_atomic(&file, settings.to_json().as_bytes())?;
        log::info!("settings: saved {}", file.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        AppearancePref, LanguagePref, LoadOutcome, McpPort, SETTINGS_FILE, Settings, SettingsStore,
        config_dir,
    };

    fn store() -> (tempfile::TempDir, SettingsStore) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = SettingsStore::new(Some(dir.path().join("nested")));
        (dir, store)
    }

    #[test]
    fn the_agent_endpoint_starts_off_on_its_own_port() {
        let plain = Settings::from_json(r#"{"language":"zh-CN"}"#).expect("loads");
        assert!(!plain.mcp_enabled, "off until the user asks for it");
        assert_eq!(plain.mcp_port, McpPort(8765));
        let asked = Settings::from_json(r#"{"mcp_enabled":true,"mcp_port":9000}"#).expect("loads");
        assert!(asked.mcp_enabled);
        assert_eq!(asked.mcp_port, McpPort(9000));
        let broken =
            Settings::from_json(r#"{"mcp_enabled":"yes","mcp_port":"nine"}"#).expect("loads");
        assert!(
            !broken.mcp_enabled,
            "a value the field cannot hold resets that field only"
        );
        assert_eq!(broken.mcp_port, McpPort(8765));
    }

    #[test]
    fn asking_before_an_agent_spends_stays_on_unless_it_is_turned_off() {
        let plain = Settings::from_json("{}").expect("loads");
        assert!(
            plain.mcp_confirm.0,
            "a file that never mentioned it still asks"
        );
        let off = Settings::from_json(r#"{"mcp_confirm":false}"#).expect("loads");
        assert!(!off.mcp_confirm.0);
        let broken = Settings::from_json(r#"{"mcp_confirm":"no"}"#).expect("loads");
        assert!(
            broken.mcp_confirm.0,
            "a value the field cannot hold keeps the safe default"
        );
    }

    #[test]
    fn the_override_wins_and_empty_means_unset() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(
            config_dir(Some(dir.path().as_os_str().to_owned())),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(config_dir(Some("".into())), config_dir(None));
    }

    #[test]
    fn a_missing_file_gives_defaults_and_a_save_round_trips() {
        let (_dir, store) = store();
        let loaded = store.load();
        assert_eq!(loaded.outcome, LoadOutcome::Missing);
        assert_eq!(loaded.settings, Settings::default());
        let settings = Settings {
            language: LanguagePref::ZhTw,
            appearance: AppearancePref::Dark,
            defaults: colab_playground_core::Defaults {
                max_attempts: 5,
                idle_timeout: Some(std::time::Duration::from_secs(900)),
                ..Default::default()
            },
            wsl_distro: Some("Debian".into()),
            ..Settings::default()
        };
        store.save(&settings).expect("saved");
        let text = fs::read_to_string(store.file().expect("a file")).expect("written");
        assert!(
            text.contains("\"zh-TW\"") && text.contains("\"dark\"") && text.contains("Debian"),
            "{text}"
        );
        let again = store.load();
        assert_eq!(again.outcome, LoadOutcome::Loaded);
        assert_eq!(again.settings, settings);
        assert!(again.reset_defaults.is_empty());
    }

    #[test]
    fn a_bad_default_resets_only_itself_and_is_reported() {
        let text = r#"{"defaults": {"max_attempts": "lots", "retry_delay": 45, "high_mem": true}}"#;
        let (settings, reset) = Settings::from_json_reporting(text).expect("an object");
        assert_eq!(reset, ["max_attempts"]);
        let core = colab_playground_core::Defaults::default();
        assert_eq!(settings.defaults.max_attempts, core.max_attempts);
        assert_eq!(
            settings.defaults.retry_delay,
            std::time::Duration::from_secs(45)
        );
        assert!(settings.defaults.high_mem);
        let (_, reset) = Settings::from_json_reporting(r#"{"defaults": 3}"#).expect("an object");
        assert_eq!(reset, ["defaults"]);
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_and_gives_defaults() {
        let (_dir, store) = store();
        let file = store.file().expect("a file");
        fs::create_dir_all(file.parent().expect("a parent")).expect("dir");
        fs::write(&file, "{ not json").expect("written");
        let loaded = store.load();
        assert_eq!(loaded.settings, Settings::default());
        let LoadOutcome::Corrupt { backup, .. } = loaded.outcome else {
            panic!("expected a corrupt outcome, got {:?}", loaded.outcome);
        };
        let backup = backup.expect("moved aside");
        assert_eq!(fs::read_to_string(backup).expect("kept"), "{ not json");
        assert!(
            !file.exists(),
            "the bad file no longer shadows the defaults"
        );
    }

    #[test]
    fn a_bad_field_falls_back_alone_and_unknown_fields_are_ignored() {
        let settings = Settings::from_json(r#"{"language":"fr-FR","appearance":"dark","later":1}"#)
            .expect("an object");
        assert_eq!(settings.language, LanguagePref::System);
        assert_eq!(settings.appearance, AppearancePref::Dark);
        assert!(Settings::from_json("[1,2]").is_err());
        assert_eq!(
            Settings::from_json("{}").expect("an object"),
            Settings::default()
        );
    }

    #[test]
    fn a_store_without_a_directory_keeps_settings_in_memory() {
        let store = SettingsStore::new(None);
        assert_eq!(store.load().outcome, LoadOutcome::NoConfigDir);
        store.save(&Settings::default()).expect("nothing to write");
        assert!(store.file().is_none());
        assert_eq!(SETTINGS_FILE, "settings.json");
    }

    #[test]
    fn language_values_round_trip() {
        for pref in LanguagePref::ALL {
            assert_eq!(LanguagePref::from_value(pref.value()), Some(pref));
            assert_eq!(
                serde_json::to_value(pref).expect("serialises"),
                pref.value(),
                "the settings file and the select agree on {pref:?}"
            );
        }
        assert_eq!(LanguagePref::from_value("de-DE"), None);
    }
}
