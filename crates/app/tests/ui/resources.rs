//! Live RAM, VRAM and disk on session cards and a running Run page.
//! A failed poll keeps the last numbers and says expired or unknown.

use std::time::Duration;

use colab_playground::{
    route::Route,
    state::{PluginView, RunPhase},
    ui::{
        pages::plugins::{RUN_RESOURCES, RUN_RESOURCES_STALE},
        resources::{resources_selector, resources_stale_selector, resources_vram_none_selector},
    },
};
use colab_playground_core::{
    mock::{MockActiveRun, MockScenario},
    plugins::{LocalizedText, ResolvedEndpoint},
    resources::ResourceFault,
    vm::RunState,
};
use gpui::TestAppContext;

use crate::support::{Options, TestApp};

fn used_ram(app: &mut TestApp<'_>, name: &str) -> Option<u64> {
    app.read(|state, _| {
        let runtime = state
            .runtimes()
            .ready()?
            .runtimes
            .iter()
            .find(|runtime| runtime.id.name.as_deref() == Some(name))?;
        state
            .resource_view(&runtime.id)?
            .reading
            .as_ref()?
            .memory
            .used_bytes()
    })
}

#[gpui::test]
fn session_cards_show_ram_vram_and_disk_and_a_fault_keeps_them(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    app.click(Route::Sessions.nav_selector());
    app.reveal(&resources_selector("a100-7f3a"));
    assert!(app.shown(&resources_selector("a100-7f3a")));
    assert!(!app.shown(&resources_stale_selector("a100-7f3a")));
    app.reveal(&resources_vram_none_selector("cpu-0b5e"));
    assert!(
        app.shown(&resources_vram_none_selector("cpu-0b5e")),
        "a CPU runtime says it has no GPU"
    );
    let used = used_ram(&mut app, "a100-7f3a").expect("a100 has a reading");
    assert!(used > 0, "used RAM is a real number, not a stand-in zero");
    app.read(|state, _| {
        let unnamed = state
            .runtimes()
            .ready()
            .unwrap()
            .runtimes
            .iter()
            .find(|runtime| runtime.id.name.is_none())
            .expect("demo has an unnamed runtime");
        let view = state
            .resource_view(&unnamed.id)
            .expect("unnamed runtime was fetched");
        assert!(view.reading.is_none());
        assert_eq!(view.fault, Some(ResourceFault::Unavailable));
    });

    app.mock.set_resource_fault(Some(ResourceFault::Expired));
    app.advance(Duration::from_secs(15));
    app.reveal(&resources_stale_selector("a100-7f3a"));
    assert!(app.shown(&resources_stale_selector("a100-7f3a")));
    assert_eq!(used_ram(&mut app, "a100-7f3a"), Some(used));
}

#[gpui::test]
fn a_running_plugin_shows_resources_until_the_fetch_fails(cx: &mut TestAppContext) {
    let options = Options {
        scenario: MockScenario::demo(),
        active_runs: vec![MockActiveRun {
            account: 0,
            session: "a100-7f3a".into(),
            run_id: "resources-1".into(),
            plugin: "comfyui".into(),
            stage: Some("download".into()),
            state: RunState::Running,
            log_tail: vec!["downloading checkpoint...".into()],
            keep_alive: true,
            endpoints: vec![ResolvedEndpoint {
                id: "ui".into(),
                remote_port: 8188,
                path: "/".into(),
                health: Some("/".into()),
                kind: "comfyui".into(),
                label: LocalizedText::default(),
            }],
            manual_stages: false,
        }],
        ..Options::default()
    };
    let mut app = TestApp::with(cx, options);
    let used = used_ram(&mut app, "a100-7f3a").expect("session reading before the run page");
    assert!(used > 0);
    app.click(Route::Plugins.nav_selector());
    app.update(|state, cx| state.show_plugin_run(cx));
    app.settle();
    assert_eq!(app.read(|state, _| state.plugin_view()), PluginView::Run);
    assert!(matches!(
        app.read(|state, _| state.plugin_run().map(|run| run.phase.clone())),
        Some(RunPhase::Ready(_))
    ));
    app.reveal(RUN_RESOURCES);
    assert!(app.shown(RUN_RESOURCES));
    assert!(!app.shown(RUN_RESOURCES_STALE));

    app.mock
        .set_resource_fault(Some(ResourceFault::Unreachable));
    app.advance(Duration::from_secs(15));
    app.reveal(RUN_RESOURCES_STALE);
    assert!(app.shown(RUN_RESOURCES_STALE));
    assert_eq!(used_ram(&mut app, "a100-7f3a"), Some(used));
}
