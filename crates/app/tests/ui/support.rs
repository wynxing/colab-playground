//! Headless harness for the real app views: replaces browser end-to-end tests.
//!
//! ```text
//! #[gpui::test]
//! fn opens_sessions(cx: &mut TestAppContext) {
//!     let mut app = TestApp::new(cx);              // mock backend, en-US, temp config dir
//!     app.click(Route::Sessions.nav_selector());   // a real mouse click on the Ely NavItem
//!     assert_eq!(app.route(), Route::Sessions);
//!     assert!(app.shown("page sessions Sessions"));
//! }
//! ```
//!
//! Elements are found by gpui debug selectors (`VisualTestContext::debug_bounds`): Ely's own
//! (`segment theme.dark`, `option-list`, ...) or the app's, set with `colab_playground::ui::tagged`.
//! After every input the harness settles: runs the executor until parked, refreshes the window
//! and parks again, so re-renders, observers and background tasks (settings saves, backend
//! calls) finish. Each app gets its own temporary config directory; the real config dir is
//! never touched.
//!
//! The backend is always core's headless `MockBackend` (a `ManualClock`, zero step delay so
//! virtual time only moves when a test calls [`TestApp::advance`]). There is no way to build a
//! real backend here. Failures can be set before start ([`Options::failures`]; `cli_missing`
//! only works that way) or flipped mid-test ([`TestApp::set_failures`]).

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use colab_playground::{
    backend::BackendSource,
    i18n::{self, Language},
    route::Route,
    settings::{Settings, SettingsStore},
    state::{AppState, StateOptions},
    ui::root::RootView,
};
use colab_playground_core::{
    ManualClock,
    backend::Backend,
    mock::{MockActiveRun, MockBackend, MockConfig, MockFailures, MockScenario},
};
use ely_gpui_component::theme::{ActiveTheme, Mode};
use gpui::{
    App, Bounds, Context, Entity, Modifiers, Pixels, Render, SharedString, Size, TestAppContext,
    VisualTestContext, Window, point, px,
};
use tempfile::TempDir;

/// The window size tests draw at, as the app opens by default.
pub const WINDOW: Size<Pixels> = colab_playground::ui::metrics::WINDOW_SIZE;

pub enum ConfigDir {
    /// A fresh temporary directory (the default).
    Temp,
    /// A temporary directory holding this `settings.json` text before the app starts.
    TempWith(String),
    /// This relative path under a temporary directory, not created yet (long paths, a folder
    /// that "Open folder" has to create).
    TempNested(String),
    /// No config directory: settings stay in memory.
    None,
}

pub struct Options {
    pub scenario: MockScenario,
    /// Failures switched on before the app starts.
    pub failures: MockFailures,
    pub os_locale: Option<String>,
    pub config: ConfigDir,
    /// Simulated active plugin runs present on runtimes before the app starts.
    pub active_runs: Vec<MockActiveRun>,
    /// Delay per simulated backend step (on the manual clock).
    pub step_delay: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scenario: MockScenario::demo(),
            failures: MockFailures::default(),
            os_locale: Some("en-US".into()),
            config: ConfigDir::Temp,
            active_runs: Vec::new(),
            step_delay: Duration::ZERO,
        }
    }
}

impl Options {
    pub fn scenario(scenario: MockScenario) -> Self {
        Self {
            scenario,
            ..Self::default()
        }
    }
}

pub struct TestApp<'a> {
    pub cx: &'a mut VisualTestContext,
    pub root: Entity<RootView>,
    pub state: Entity<AppState>,
    /// The simulated backend the app drives (shared with the app).
    pub mock: MockBackend,
    pub clock: ManualClock,
    /// Cancels a backend call at one of its later waits (see [`CancelSwitch`]).
    pub cancel_switch: CancelSwitch,
    config: Option<TempDir>,
    config_dir: Option<PathBuf>,
}

impl<'a> TestApp<'a> {
    pub fn new(cx: &'a mut TestAppContext) -> Self {
        Self::with(cx, Options::default())
    }

    /// The app built from `options`, settled: what it loads on start has arrived.
    pub fn with(cx: &'a mut TestAppContext, options: Options) -> Self {
        let started = start(cx, options);
        let root_state = started.state.clone();
        // Opening the window runs the executor, so the start-up loads arrive here.
        let (root, cx) =
            cx.add_window_view(move |window, cx| RootView::new(root_state, window, cx));
        cx.simulate_resize(WINDOW);
        cx.update(|window, _| window.activate_window());
        let mut app = TestApp {
            cx,
            root,
            state: started.state,
            mock: started.mock,
            clock: started.clock,
            cancel_switch: started.cancel_switch,
            config: started.config,
            config_dir: started.config_dir,
        };
        app.settle();
        app
    }

    /// Moves virtual time on: the mock's clock and gpui's test clock together (so the usage
    /// poller wakes), then settles.
    pub fn advance(&mut self, by: Duration) {
        self.clock.advance(by);
        self.cx.executor().advance_clock(by);
        self.settle();
    }

    /// Replaces the mock's failure switches (all but `cli_missing` act at once).
    pub fn set_failures(&mut self, failures: MockFailures) {
        self.mock.set_failures(failures);
    }

    /// Types `text` into a text field the root view owns, as if pasted.
    pub fn fill(
        &mut self,
        field: impl Fn(&colab_playground::ui::Inputs) -> Entity<ely_gpui_component::forms::TextInput>,
        text: &str,
    ) {
        let input = self.root.read_with(self.cx, |root, _| field(root.inputs()));
        let text = text.to_owned();
        self.cx
            .update(|_, cx| input.update(cx, |input, cx| input.set_text(text, cx)));
        self.settle();
    }

    pub fn read_root<R>(&mut self, read: impl FnOnce(&RootView, &App) -> R) -> R {
        let root = self.root.clone();
        self.cx.update(|_, cx| read(root.read(cx), cx))
    }

    /// Resizes the window, then settles.
    pub fn resize(&mut self, window: Size<Pixels>) {
        self.cx.simulate_resize(window);
        self.settle();
    }

    /// The URL the app last asked the platform to open, if any.
    pub fn opened_url(&mut self) -> Option<String> {
        self.cx.opened_url()
    }

    /// Lets every pending render, observer and task finish.
    pub fn settle(&mut self) {
        settle(self.cx);
    }

    /// The bounds of the element carrying `selector` in the last frame.
    pub fn bounds(&mut self, selector: &str) -> Option<Bounds<Pixels>> {
        self.cx.debug_bounds(leak(selector))
    }

    /// Whether an element with `selector` was drawn in the last frame.
    pub fn shown(&mut self, selector: &str) -> bool {
        self.bounds(selector).is_some()
    }

    /// A left click in the middle of the element carrying `selector`, then settles.
    pub fn click(&mut self, selector: &str) {
        self.reveal(selector);
        let bounds = self
            .bounds(selector)
            .unwrap_or_else(|| panic!("no element with selector {selector:?} was drawn"));
        click_at(self.cx, bounds.center());
    }

    /// Scrolls the page (mouse wheel over the page area) until the element carrying `selector`
    /// is inside the window.
    pub fn reveal(&mut self, selector: &str) {
        for _ in 0..30 {
            let Some(bounds) = self.bounds(selector) else {
                return;
            };
            let viewport = self.cx.update(|window, _| window.viewport_size());
            let margin = px(24.0);
            let above = bounds.top() < px(0.0);
            let below = bounds.bottom() > viewport.height - margin;
            if !above && !below {
                return;
            }
            let Some(page) = self.bounds(colab_playground::ui::root::PAGE_SELECTOR) else {
                return;
            };
            let target = page.top() + page.size.height / 2.0;
            let delta = target - bounds.center().y;
            self.cx.simulate_event(gpui::ScrollEvent {
                position: page.center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.0), delta)),
                modifiers: Modifiers::none(),
                touch_phase: gpui::TouchPhase::Moved,
                source: gpui::ScrollSource::Pointer,
            });
            self.settle();
        }
    }

    /// Opens the `Select` inside `selector` with a click and clicks its `index`th of `count`
    /// options.
    pub fn pick_option(&mut self, selector: &str, index: usize, count: usize) {
        assert!(index < count, "option {index} of {count}");
        self.click(selector);
        let list = self
            .bounds("option-list")
            .unwrap_or_else(|| panic!("clicking {selector:?} opened no option list"));
        let row = list.size.height / count as f32;
        let at = point(
            list.origin.x + list.size.width / 2.0,
            list.origin.y + row * (index as f32 + 0.5),
        );
        click_at(self.cx, at);
    }

    /// Keystrokes such as `"tab"`, `"enter"` or `"secondary-z"`, then settles.
    pub fn press(&mut self, keystrokes: &str) {
        self.cx.simulate_keystrokes(keystrokes);
        self.settle();
    }

    /// Text typed into the focused field, then settles.
    pub fn type_text(&mut self, text: &str) {
        self.cx.simulate_input(text);
        self.settle();
    }

    pub fn read<R>(&mut self, read: impl FnOnce(&AppState, &App) -> R) -> R {
        let state = self.state.clone();
        self.cx.update(|_, cx| read(state.read(cx), cx))
    }

    /// Changes the app state as a view would, then settles.
    pub fn update<R>(
        &mut self,
        update: impl FnOnce(&mut AppState, &mut Context<AppState>) -> R,
    ) -> R {
        let state = self.state.clone();
        let result = self.cx.update(|_, cx| state.update(cx, update));
        self.settle();
        result
    }

    /// Applies `update` to the app state and draws a frame without running the executor, so
    /// a state that pending work (a listing in flight) ends at the next settle can be seen.
    pub fn update_and_draw<R>(
        &mut self,
        update: impl FnOnce(&mut AppState, &mut Context<AppState>) -> R,
    ) -> R {
        let state = self.state.clone();
        let result = self.cx.update(|_, cx| state.update(cx, update));
        self.draw_now();
        result
    }

    /// Draws a frame now, without running the executor first.
    pub fn draw_now(&mut self) {
        self.cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
    }

    pub fn route(&mut self) -> Route {
        self.read(|state, _| state.route())
    }

    pub fn language(&mut self) -> Language {
        self.cx.update(|_, cx| i18n::current(cx))
    }

    pub fn mode(&mut self) -> Mode {
        self.cx.update(|_, cx| cx.theme().mode())
    }

    /// A message of the app's catalog in the shown language.
    pub fn text(&mut self, key: &str) -> SharedString {
        self.cx.update(|_, cx| i18n::tr(cx, key))
    }

    pub fn toasts(&mut self) -> Vec<SharedString> {
        let toaster = self.read(|state, _| state.toaster().clone());
        self.cx.update(|_, cx| {
            toaster
                .read(cx)
                .shown()
                .into_iter()
                .map(|(title, _)| title)
                .collect()
        })
    }

    /// Toast titles with their severity, oldest first.
    pub fn toasts_with_severity(
        &mut self,
    ) -> Vec<(
        SharedString,
        Option<ely_gpui_component::primitives::Severity>,
    )> {
        let toaster = self.read(|state, _| state.toaster().clone());
        self.cx.update(|_, cx| toaster.read(cx).shown())
    }

    pub fn config_dir(&self) -> Option<&Path> {
        self.config_dir.as_deref()
    }

    pub fn settings_file(&self) -> Option<PathBuf> {
        self.config_dir().map(|dir| dir.join("settings.json"))
    }

    /// What is on disk now, read the way the app reads it at startup.
    pub fn saved_settings(&self) -> Option<Settings> {
        let file = self.settings_file()?;
        let text = std::fs::read_to_string(file).ok()?;
        Some(Settings::from_json(&text).expect("the app writes valid settings"))
    }
}

/// The app's state started from `options`, with no window and nothing run yet.
struct Started {
    state: Entity<AppState>,
    mock: MockBackend,
    clock: ManualClock,
    cancel_switch: CancelSwitch,
    config: Option<TempDir>,
    config_dir: Option<PathBuf>,
}

fn start(cx: &mut TestAppContext, options: Options) -> Started {
    cx.update(ely_gpui_component::init_for_tests);
    let config = match &options.config {
        ConfigDir::None => None,
        ConfigDir::Temp | ConfigDir::TempWith(_) | ConfigDir::TempNested(_) => {
            Some(tempfile::tempdir().expect("a temporary config dir"))
        }
    };
    if let (ConfigDir::TempWith(text), Some(dir)) = (&options.config, &config) {
        std::fs::write(dir.path().join("settings.json"), text).expect("seeded settings");
    }
    let config_dir = config.as_ref().map(|dir| match &options.config {
        ConfigDir::TempNested(sub) => dir.path().join(sub),
        _ => dir.path().to_path_buf(),
    });
    let store = SettingsStore::new(config_dir.clone());
    let (mut mock_config, clock) = MockConfig::headless(options.scenario);
    mock_config.step_delay = options.step_delay;
    let cancel_switch = CancelSwitch::default();
    mock_config.clock = Arc::new(SwitchedClock {
        inner: clock.clone(),
        switch: cancel_switch.clone(),
    });
    mock_config.failures = options.failures;
    mock_config.active_runs = options.active_runs;
    let mock = MockBackend::new(mock_config);
    let source = BackendSource::Mock(Arc::new(mock.clone()) as Arc<dyn Backend>);
    let state = cx.update(|cx| {
        AppState::init(
            StateOptions {
                source,
                store,
                os_locale: options.os_locale,
            },
            cx,
        )
    });
    Started {
        state,
        mock,
        clock,
        cancel_switch,
        config,
        config_dir,
    }
}

/// The app's state just after launch, without a window: the environment check, accounts and
/// runtimes are all still pending until `cx.run_until_parked()`. For start-up behaviour a
/// window would hide (opening one runs the executor).
pub struct Launch {
    pub state: Entity<AppState>,
    pub mock: MockBackend,
    _config: Option<TempDir>,
}

impl Launch {
    pub fn new(cx: &mut TestAppContext, options: Options) -> Self {
        let started = start(cx, options);
        Launch {
            state: started.state,
            mock: started.mock,
            _config: started.config,
        }
    }
}

/// A bare window over `build`, after Ely's test init: for testing one Ely component or a
/// scratch view without the whole app.
pub fn mount<V: Render + 'static>(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut Window, &mut Context<V>) -> V + 'static,
) -> (Entity<V>, &mut VisualTestContext) {
    cx.update(ely_gpui_component::init_for_tests);
    let (view, cx) = cx.add_window_view(build);
    cx.update(|window, _| window.activate_window());
    settle(cx);
    (view, cx)
}

/// Runs until parked, refreshes the window and runs until parked again (Ely's pattern).
pub fn settle(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

/// A left click at `at`, then settles.
pub fn click_at(cx: &mut VisualTestContext, at: gpui::Point<Pixels>) {
    cx.simulate_click(at, Modifiers::none());
    settle(cx);
}

/// `debug_bounds` takes `&'static str`; selectors built at run time are leaked (tests only).
fn leak(selector: &str) -> &'static str {
    Box::leak(selector.to_owned().into_boxed_str())
}

/// Cancels the backend call that makes the `n`th wait on the mock's clock after
/// [`CancelSwitch::arm`], as if the user pressed Cancel at that moment. Headless backend calls
/// run to completion in one go, so a click cannot land in the middle of one; the call's own
/// token reaches `Clock::sleep`, which is where this flips it.
#[derive(Clone, Default)]
pub struct CancelSwitch(Arc<std::sync::atomic::AtomicUsize>);

impl CancelSwitch {
    /// Cancels at the `n`th wait from now (1 = the next one).
    pub fn arm(&self, n: usize) {
        self.0.store(n, std::sync::atomic::Ordering::SeqCst);
    }
}

struct SwitchedClock {
    inner: ManualClock,
    switch: CancelSwitch,
}

impl colab_playground_core::clock::Clock for SwitchedClock {
    fn now(&self) -> std::time::SystemTime {
        colab_playground_core::clock::Clock::now(&self.inner)
    }

    fn sleep(
        &self,
        duration: Duration,
        cancel: &colab_playground_core::CancelToken,
    ) -> colab_playground_core::Result<()> {
        use std::sync::atomic::Ordering;
        let left = self.switch.0.load(Ordering::SeqCst);
        if left > 0 {
            self.switch.0.store(left - 1, Ordering::SeqCst);
            // As for a real click, a cancel after the call's point of no return does not apply.
            if left == 1 && cancel.cancel() {
                return Err(colab_playground_core::Error::cancelled());
            }
        }
        colab_playground_core::clock::Clock::sleep(&self.inner, duration, cancel)
    }
}
