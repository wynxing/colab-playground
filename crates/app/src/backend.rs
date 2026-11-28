//! Which backend the UI talks to, and how it is built.
//!
//! `main` resolves the flags into a [`BackendSource`]; `AppState` turns that into an
//! `Arc<dyn Backend>` once the settings (defaults, WSL distro, `colab` path) are loaded. Tests
//! never come through here: the harness hands `AppState` a headless `MockBackend` directly.

use std::{fmt, sync::Arc, time::Duration};

use colab_playground_core::{
    Defaults, Host, Toolchain,
    backend::{Backend, BackendKind as Flavor},
    mock::{MockBackend, MockConfig, MockFailures, MockLoginFailure, MockScenario},
    real::{RealBackend, RealConfig},
    sessions::Accelerator,
};

use crate::platform;

/// Environment variable that picks the backend when no flag is given: `mock` or `real`.
pub const BACKEND_ENV: &str = "COLAB_PLAYGROUND_BACKEND";

/// How long each simulated step of the interactive mock takes (pacing for `--mock` runs).
pub const MOCK_STEP_DELAY: Duration = Duration::from_millis(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// Deterministic simulation; never touches WSL, the `colab` CLI or a Google account.
    Mock,
    /// The real `colab` CLI through the core crate's `Host`.
    Real,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    ConflictingFlags,
    InvalidEnv(String),
    UnknownScenario(String),
    UnknownFailure(String),
    UnknownArgument(String),
    MissingValue(&'static str),
    /// A simulation flag without the simulation (it would otherwise start the real backend).
    MockFlagWithoutMock(&'static str),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::ConflictingFlags => write!(f, "pass at most one of --mock and --real"),
            BackendError::InvalidEnv(value) => {
                write!(
                    f,
                    "{BACKEND_ENV}={value:?} is not one of \"mock\" or \"real\""
                )
            }
            BackendError::UnknownScenario(value) => write!(
                f,
                "--mock-scenario {value:?} is not one of {}",
                names(SCENARIOS, ", ")
            ),
            BackendError::UnknownFailure(value) => write!(
                f,
                "--mock-fail {value:?} is not one of {}",
                names(FAILURES, ", ")
            ),
            BackendError::UnknownArgument(arg) => write!(f, "unknown argument {arg:?}"),
            BackendError::MissingValue(flag) => write!(f, "{flag} needs a value"),
            BackendError::MockFlagWithoutMock(flag) => {
                write!(f, "{flag} only applies to the simulation; add --mock")
            }
        }
    }
}

impl std::error::Error for BackendError {}

impl BackendKind {
    /// Debug builds default to mock, release builds to real (a hard safety rule in
    /// `docs/DESIGN.md`).
    pub fn build_default() -> Self {
        if cfg!(debug_assertions) {
            BackendKind::Mock
        } else {
            BackendKind::Real
        }
    }

    /// A `--mock`/`--real` flag wins over `env` (the value of [`BACKEND_ENV`]), which wins over
    /// [`Self::build_default`]. Other arguments are ignored. Pure, so tests never touch the
    /// process environment.
    pub fn resolve<S: AsRef<str>>(args: &[S], env: Option<&str>) -> Result<Self, BackendError> {
        let mock = args.iter().any(|arg| arg.as_ref() == "--mock");
        let real = args.iter().any(|arg| arg.as_ref() == "--real");
        match (mock, real) {
            (true, true) => return Err(BackendError::ConflictingFlags),
            (true, false) => return Ok(BackendKind::Mock),
            (false, true) => return Ok(BackendKind::Real),
            (false, false) => {}
        }
        match env.map(str::trim) {
            None | Some("") => Ok(Self::build_default()),
            Some(value) if value.eq_ignore_ascii_case("mock") => Ok(BackendKind::Mock),
            Some(value) if value.eq_ignore_ascii_case("real") => Ok(BackendKind::Real),
            Some(value) => Err(BackendError::InvalidEnv(value.to_owned())),
        }
    }
}

/// The mock's starting world and injected failures, from `--mock-scenario` and `--mock-fail`.
#[derive(Clone, Debug, PartialEq)]
pub struct MockFlags {
    pub scenario: MockScenario,
    pub failures: MockFailures,
}

type StartWorld = fn() -> MockScenario;
type Inject = fn(&mut MockFailures);

const SCENARIOS: &[(&str, StartWorld)] = &[
    ("demo", MockScenario::demo),
    ("single", MockScenario::single),
    ("signed-out", MockScenario::signed_out),
];

const FAILURES: &[(&str, Inject)] = &[
    ("cli-missing", |f| f.cli_missing = true),
    ("install-fails", |f| f.install_fails = true),
    ("keyring", |f| f.keyring_unavailable = true),
    ("usage-fails", |f| f.usage_fetch_fails = true),
    ("login-invalid-code", |f| {
        f.login = Some(MockLoginFailure::InvalidCode)
    }),
    ("login-unverified", |f| {
        f.login = Some(MockLoginFailure::VerificationFailed)
    }),
    ("not-entitled", |f| {
        f.not_entitled = vec![Accelerator::A100, Accelerator::H100]
    }),
    ("no-answer", |f| {
        f.allocation_hangs = vec![Accelerator::A100]
    }),
    ("ssh-busy", |f| f.ssh_busy_attempts = 1),
    ("plugin-fails", |f| {
        f.plugin_fail_stage = Some("download".into())
    }),
    ("watchdog-restart-fails", |f| {
        f.watchdog_restart_fails = true
    }),
    ("update-available", |f| f.update_available = true),
    ("update-check-fails", |f| f.update_check_fails = true),
    ("update-unsupported", |f| f.update_unsupported = true),
    ("update-download-fails", |f| f.update_download_fails = true),
    ("update-unverified", |f| f.update_unverified = true),
    ("update-install-fails", |f| f.update_install_fails = true),
    ("runtimes-fail", |f| f.runtimes_listing_fails = true),
];

fn names<T>(table: &[(&str, T)], separator: &str) -> String {
    let names: Vec<&str> = table.iter().map(|(name, _)| *name).collect();
    names.join(separator)
}

impl MockFlags {
    /// Reads `--mock-scenario NAME` (default `demo`) and `--mock-fail a,b`, as `--flag value`
    /// or `--flag=value`.
    pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Self, BackendError> {
        let mut flags = MockFlags {
            scenario: MockScenario::demo(),
            failures: MockFailures::default(),
        };
        if let Some(name) = flag_value(args, "--mock-scenario") {
            let Some((_, scenario)) = SCENARIOS.iter().find(|(known, _)| *known == name) else {
                return Err(BackendError::UnknownScenario(name));
            };
            flags.scenario = scenario();
        }
        if let Some(list) = flag_value(args, "--mock-fail") {
            for name in list
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                let Some((_, inject)) = FAILURES.iter().find(|(known, _)| *known == name) else {
                    return Err(BackendError::UnknownFailure(name.to_owned()));
                };
                inject(&mut flags.failures);
            }
        }
        Ok(flags)
    }
}

/// How to call the program, printed with any argument error.
pub fn usage() -> String {
    format!(
        "usage: colab-playground [--real | --mock [--mock-scenario {}] [--mock-fail NAME[,NAME...]]]\n  --mock-fail names: {}",
        names(SCENARIOS, "|"),
        names(FAILURES, ", ")
    )
}

const VALUE_FLAGS: [&str; 2] = ["--mock-scenario", "--mock-fail"];

/// Rejects anything but the known flags, and a value flag without a value. Runs before any
/// backend is chosen, so a mistyped `--mock` can never start the real backend.
pub fn check_args<S: AsRef<str>>(args: &[S]) -> Result<(), BackendError> {
    let mut iter = args.iter().map(AsRef::as_ref);
    while let Some(arg) = iter.next() {
        if arg == "--mock" || arg == "--real" {
            continue;
        }
        if platform::is_launcher_argument(arg) {
            continue;
        }
        if let Some(&flag) = VALUE_FLAGS.iter().find(|flag| **flag == arg) {
            match iter.next() {
                Some(value) if !value.trim().is_empty() && !value.starts_with("--") => continue,
                _ => return Err(BackendError::MissingValue(flag)),
            }
        }
        if let Some(&flag) = VALUE_FLAGS
            .iter()
            .find(|flag| arg.starts_with(&format!("{flag}=")))
        {
            if arg[flag.len() + 1..].trim().is_empty() {
                return Err(BackendError::MissingValue(flag));
            }
            continue;
        }
        return Err(BackendError::UnknownArgument(arg.to_owned()));
    }
    Ok(())
}

/// The backend the command line (and [`BACKEND_ENV`]) asks for, after checking every
/// argument. Simulation flags without the simulation are an error.
pub fn source_from_args<S: AsRef<str>>(
    args: &[S],
    env: Option<&str>,
) -> Result<BackendSource, BackendError> {
    check_args(args)?;
    match BackendKind::resolve(args, env)? {
        BackendKind::Mock => Ok(BackendSource::interactive_mock(MockFlags::parse(args)?)),
        BackendKind::Real => {
            for flag in VALUE_FLAGS {
                if flag_value(args, flag).is_some() {
                    return Err(BackendError::MockFlagWithoutMock(flag));
                }
            }
            Ok(BackendSource::Real)
        }
    }
}

fn flag_value<S: AsRef<str>>(args: &[S], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut iter = args.iter().map(AsRef::as_ref);
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().map(str::to_owned);
        }
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(value.to_owned());
        }
    }
    None
}

#[derive(Clone)]
pub enum BackendSource {
    /// Build a `RealBackend` from the settings (rebuilt when the WSL distro or `colab` path
    /// changes).
    Real,
    /// A ready simulation (the interactive one for `--mock`, a headless one in tests).
    Mock(Arc<dyn Backend>),
}

impl BackendSource {
    /// The interactive simulation for `--mock`: system clock, human-paced steps.
    pub fn interactive_mock(flags: MockFlags) -> Self {
        let mut config = MockConfig::interactive(MOCK_STEP_DELAY);
        config.scenario = flags.scenario;
        config.failures = flags.failures;
        BackendSource::Mock(Arc::new(MockBackend::new(config)))
    }

    pub fn flavor(&self) -> Flavor {
        match self {
            BackendSource::Real => Flavor::Real,
            BackendSource::Mock(_) => Flavor::Mock,
        }
    }
}

/// Where the real backend finds the CLI (from Settings).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostSetup {
    /// WSL distribution (Windows); `None` = the default one.
    pub wsl_distro: Option<String>,
    /// `colab` and its Python interpreter; `None` = found automatically.
    pub toolchain: Option<Toolchain>,
    /// Talk to Colab's public API instead of the CLI. The CLI stays available.
    pub direct: bool,
}

/// Builds the backend for `source`. A mock is returned as it is, with `defaults` pushed in.
pub fn build(
    source: &BackendSource,
    host: &HostSetup,
    defaults: &Defaults,
) -> colab_playground_core::Result<Arc<dyn Backend>> {
    match source {
        BackendSource::Mock(mock) => {
            mock.set_defaults(defaults.clone());
            Ok(mock.clone())
        }
        BackendSource::Real if host.direct => Ok(Arc::new(
            colab_playground_core::direct::DirectBackend::production(defaults.clone(), None, None)?,
        )),
        BackendSource::Real => {
            let mut config = RealConfig::for_app(defaults.clone())?;
            if let Host::Wsl { distro } = &mut config.host {
                distro.clone_from(&host.wsl_distro);
            }
            config.toolchain.clone_from(&host.toolchain);
            Ok(Arc::new(RealBackend::new(config)?))
        }
    }
}

/// Whether the shell shows the "Demo data" badge (the product principle in `docs/DESIGN.md`).
pub fn shows_demo_badge(flavor: Flavor) -> bool {
    flavor == Flavor::Mock
}

#[cfg(test)]
mod tests {
    use colab_playground_core::{backend::BackendKind as Flavor, mock::MockScenario};

    use super::{BackendError, BackendKind, MockFlags, shows_demo_badge};

    #[test]
    fn flags_win_over_env_and_env_over_the_build_default() {
        let none: [&str; 0] = [];
        assert_eq!(
            BackendKind::resolve(&none, None),
            Ok(BackendKind::build_default())
        );
        assert_eq!(
            BackendKind::resolve(&none, Some("")),
            Ok(BackendKind::build_default())
        );
        assert_eq!(
            BackendKind::resolve(&none, Some("real")),
            Ok(BackendKind::Real)
        );
        assert_eq!(
            BackendKind::resolve(&none, Some(" MOCK ")),
            Ok(BackendKind::Mock)
        );
        assert_eq!(
            BackendKind::resolve(&["--mock"], Some("real")),
            Ok(BackendKind::Mock)
        );
        assert_eq!(
            BackendKind::resolve(&["--real"], Some("mock")),
            Ok(BackendKind::Real)
        );
        assert_eq!(
            BackendKind::resolve(&["--mock-scenario", "single", "--mock"], None),
            Ok(BackendKind::Mock)
        );
    }

    #[test]
    fn unknown_arguments_and_missing_values_are_errors() {
        use super::{BackendSource, check_args, source_from_args};
        assert_eq!(
            check_args(&["--mok"]),
            Err(BackendError::UnknownArgument("--mok".into())),
            "a typo of --mock is not ignored"
        );
        assert_eq!(
            check_args(&["--mock", "stray"]),
            Err(BackendError::UnknownArgument("stray".into()))
        );
        assert_eq!(
            check_args(&["--mock", "--mock-scenario"]),
            Err(BackendError::MissingValue("--mock-scenario"))
        );
        assert_eq!(
            check_args(&["--mock-scenario", "--mock"]),
            Err(BackendError::MissingValue("--mock-scenario")),
            "a flag is not a value"
        );
        assert_eq!(
            check_args(&["--mock", "--mock-fail="]),
            Err(BackendError::MissingValue("--mock-fail"))
        );
        assert!(
            check_args(&["--mock", "--mock-scenario", "single", "--mock-fail=keyring"]).is_ok()
        );
        assert!(check_args::<&str>(&[]).is_ok());
        assert!(check_args(&["--real"]).is_ok());
        let flags = MockFlags::parse(&["--mock", "--mock-fail", "watchdog-restart-fails"]).unwrap();
        assert!(flags.failures.watchdog_restart_fails);
        let flags =
            MockFlags::parse(&["--mock", "--mock-fail=update-available,update-check-fails"])
                .unwrap();
        assert!(flags.failures.update_available && flags.failures.update_check_fails);
        let flags = MockFlags::parse(&[
            "--mock",
            "--mock-fail=update-unsupported,update-download-fails,update-unverified,update-install-fails",
        ])
        .unwrap();
        let f = &flags.failures;
        assert!(
            f.update_unsupported
                && f.update_download_fails
                && f.update_unverified
                && f.update_install_fails
        );
        assert!(super::usage().contains("update-available"));
        assert!(super::usage().contains("watchdog-restart-fails"));

        // Simulation flags without the simulation would start the real backend.
        assert!(matches!(
            source_from_args(&["--mock-scenario", "single"], Some("real")),
            Err(BackendError::MockFlagWithoutMock("--mock-scenario"))
        ));
        assert!(matches!(
            source_from_args(&["--mok"], None),
            Err(BackendError::UnknownArgument(_))
        ));
        assert!(matches!(
            source_from_args(&["--mock", "--mock-scenario", "nope"], None),
            Err(BackendError::UnknownScenario(_))
        ));
        assert!(matches!(
            source_from_args(&["--real"], None),
            Ok(BackendSource::Real)
        ));
        assert!(matches!(
            source_from_args(&["--mock-scenario", "single"], Some("mock")),
            Ok(BackendSource::Mock(_))
        ));
    }

    #[test]
    fn conflicting_flags_and_unknown_env_values_fail() {
        assert_eq!(
            BackendKind::resolve(&["--mock", "--real"], None),
            Err(BackendError::ConflictingFlags)
        );
        assert_eq!(
            BackendKind::resolve(&[] as &[&str], Some("fake")),
            Err(BackendError::InvalidEnv("fake".into()))
        );
    }

    #[test]
    fn debug_builds_default_to_mock() {
        if cfg!(debug_assertions) {
            assert_eq!(BackendKind::build_default(), BackendKind::Mock);
        }
    }

    #[test]
    fn mock_scenarios_and_failures_parse() {
        let none: [&str; 0] = [];
        assert_eq!(
            MockFlags::parse(&none).map(|flags| flags.scenario),
            Ok(MockScenario::demo())
        );
        let flags = MockFlags::parse(&[
            "--mock",
            "--mock-scenario=signed-out",
            "--mock-fail",
            "cli-missing, keyring",
        ])
        .expect("valid flags");
        assert_eq!(flags.scenario, MockScenario::signed_out());
        assert!(flags.failures.cli_missing && flags.failures.keyring_unavailable);
        assert!(!flags.failures.install_fails);
        assert_eq!(
            MockFlags::parse(&["--mock-scenario", "busy"]).map(|flags| flags.scenario),
            Err(BackendError::UnknownScenario("busy".into()))
        );
        assert_eq!(
            MockFlags::parse(&["--mock-fail=nope"]).map(|flags| flags.scenario),
            Err(BackendError::UnknownFailure("nope".into()))
        );
    }

    #[test]
    fn only_the_simulation_carries_the_demo_badge() {
        assert!(shows_demo_badge(Flavor::Mock));
        assert!(!shows_demo_badge(Flavor::Real));
    }
}
