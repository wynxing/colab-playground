#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use colab_playground::{
    app,
    backend::{BACKEND_ENV, source_from_args, usage},
    platform,
    settings::{CONFIG_DIR_ENV, SettingsStore, config_dir},
    state::StateOptions,
};
use colab_playground_core::paths;

fn main() {
    let log = start_log();
    log::info!(
        "colab-playground {} starting (log: {})",
        env!("CARGO_PKG_VERSION"),
        match &log {
            Some(path) => path.display().to_string(),
            None => "nowhere".to_owned(),
        }
    );
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Every argument is checked before any backend exists: a typo never starts the real one.
    let source = match source_from_args(&args, std::env::var(BACKEND_ENV).ok().as_deref()) {
        Ok(source) => source,
        Err(error) => {
            log::error!("{error}");
            eprintln!("colab-playground: {error}\n{}", usage());
            std::process::exit(2);
        }
    };
    let store = SettingsStore::new(config_dir(std::env::var_os(CONFIG_DIR_ENV)));
    app::run(StateOptions {
        source,
        store,
        os_locale: platform::os_locale(),
    });
}

/// Sends the log to a file: a windowed build has no console, so without this a run that sticks
/// leaves nothing to read. What the app was doing — every command it ran, every agent call it took
/// — is what a person needs to tell a slow job from a stuck one. A log that cannot be opened is
/// not fatal; the app runs without one.
fn start_log() -> Option<PathBuf> {
    let dir = paths::default_log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("colab-playground.log");
    // The previous run's log is kept, once, and only when it has grown.
    let _ = paths::rotate_log(&path, paths::LOG_LIMIT);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("warn,colab_playground=info,colab_playground_core=info"),
    );
    builder.target(env_logger::Target::Pipe(Box::new(file)));
    builder.format(|buf, record| {
        use std::io::Write as _;
        writeln!(
            buf,
            "{} {:<5} {} {}",
            buf.timestamp_millis(),
            record.level(),
            record.target(),
            record.args()
        )
    });
    builder.init();
    Some(path)
}
