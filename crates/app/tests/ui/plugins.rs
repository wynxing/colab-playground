//! Plugins over the headless mock: quick deploy to a ready endpoint, a run failing at a stage,
//! installing from a fixture folder (with the confirmation), and secrets staying secret.

use std::path::PathBuf;

use colab_playground::{
    i18n::keys,
    route::Route,
    settings::LanguagePref,
    state::{
        InstallPrompt, Load, Op, Overlay, PluginView, RUN_LOG_LINES_KEPT, RunPhase, TargetLost,
    },
    ui::{
        overlays::INSTALL_CONFIRM,
        pages::{
            overview::{INSTALL_SELECTOR, deploy_selector},
            plugins::{
                ENDPOINT_SELECTOR, INSTALL_CHECK, RUN_START, RUN_STOP, hf_result_selector,
                plugin_selector,
            },
        },
    },
};
use colab_playground_core::{
    CancelToken, ErrorKind,
    backend::{Backend, PluginRunEvent, RuntimeRef},
    mock::{MockFailures, MockRuntime, MockScenario},
    sessions::Accelerator,
    tunnel::TunnelState,
};
use gpui::TestAppContext;

use crate::support::{Options, TestApp};

const REPO: &str = "unsloth/Qwen3-0.6B-GGUF";

fn run_phase(app: &mut TestApp<'_>) -> Option<RunPhase> {
    app.read(|state, _| state.plugin_run().map(|run| run.phase.clone()))
}

/// Quick deploy from the Overview: the run form opens pre-filled.
fn open_deploy_form(app: &mut TestApp<'_>) {
    assert!(
        app.shown(&deploy_selector("llama-cpp")),
        "a Deploy a model card"
    );
    app.click(&deploy_selector("llama-cpp"));
    assert_eq!(app.route(), Route::Plugins);
    assert_eq!(app.read(|state, _| state.plugin_view()), PluginView::Form);
    let quant = app.read(|state, _| {
        state.run_form().and_then(|form| {
            form.values
                .get(colab_playground_core::quant::INPUT_QUANT)
                .cloned()
        })
    });
    assert_eq!(
        quant,
        Some(colab_playground_core::plugins::InputValue::Text(
            colab_playground_core::quant::QUANT_AUTO.into()
        )),
        "manifest defaults are filled in"
    );
}

#[gpui::test]
fn quick_deploy_runs_to_a_ready_endpoint(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    open_deploy_form(&mut app);

    // Search, pick a result, see which quantisation fits.
    app.fill(|inputs| inputs.hf_query.clone(), "qwen3");
    app.click("hf-search");
    assert!(app.shown(&hf_result_selector(REPO)), "search results");
    app.click(&hf_result_selector(REPO));
    let repo = app.read(|state, _| state.run_form().and_then(|f| f.hf.repo.clone()));
    assert_eq!(repo.as_deref(), Some(REPO));
    assert!(app.shown("quant-preview 40960"), "an A100 can be 40 GB…");
    assert!(app.shown("quant-preview 81920"), "…or 80 GB");

    app.click(RUN_START);
    let Some(RunPhase::Ready(outcome)) = run_phase(&mut app) else {
        panic!("expected a ready run, got {:?}", run_phase(&mut app));
    };
    assert_eq!(app.read(|state, _| state.plugin_view()), PluginView::Run);
    assert!(!outcome.endpoints.is_empty());
    let endpoint = &outcome.endpoints[0];
    assert!(app.shown(&format!("{ENDPOINT_SELECTOR} {}", endpoint.id)));
    let forwards: Vec<(u16, TunnelState)> = app.read(|state, _| {
        state
            .tunnels()
            .map(|t| (t.spec.remote_port, t.state.clone()))
            .collect()
    });
    assert_eq!(
        forwards,
        [(endpoint.remote_port, TunnelState::Up)],
        "the endpoint is forwarded"
    );
    let stages = app.read(|state, _| state.plugin_run().and_then(|r| r.status.clone()));
    assert_eq!(
        stages.map(|s| s.state),
        Some(colab_playground_core::vm::RunState::Ready)
    );
}

/// A ComfyUI endpoint says how to point an agent at it.
#[gpui::test]
fn a_comfyui_endpoint_shows_how_to_point_an_agent_at_it(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    app.click(Route::Plugins.nav_selector());
    assert!(
        app.shown(&plugin_selector("comfyui")),
        "the ComfyUI plugin is listed"
    );
    app.click("plugin-run comfyui");
    assert_eq!(app.read(|state, _| state.plugin_view()), PluginView::Form);
    app.click(RUN_START);
    let Some(RunPhase::Ready(outcome)) = run_phase(&mut app) else {
        panic!("expected a ready run, got {:?}", run_phase(&mut app));
    };
    let endpoint = &outcome.endpoints[0];
    assert!(
        app.shown(&format!("endpoint-mcp {}", endpoint.id)),
        "the endpoint says how to drive it with an agent"
    );
    app.reveal("run-save-/content/ComfyUI/output");
    assert!(
        app.shown("run-save-/content/ComfyUI/output"),
        "the files the run produced can be saved to this computer"
    );
}

/// The run's log as the app keeps it: how many lines, the oldest and the newest.
fn log_ends(app: &mut TestApp<'_>) -> (usize, String, String) {
    app.read(|state, _| {
        let log = &state.plugin_run().expect("a run").log;
        let mut lines = log.lines();
        let oldest = lines.next().unwrap_or_default().to_owned();
        let newest = lines.next_back().unwrap_or_default().to_owned();
        (log.len(), oldest, newest)
    })
}

#[gpui::test]
fn a_long_log_keeps_its_newest_lines_whatever_adds_them(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    open_deploy_form(&mut app);
    app.fill(|inputs| inputs.hf_query.clone(), "qwen3");
    app.click("hf-search");
    app.click(&hf_result_selector(REPO));
    app.click(RUN_START);
    assert!(matches!(run_phase(&mut app), Some(RunPhase::Ready(_))));

    // One event with more lines than are kept, as a poll of short lines brings.
    let batch = RUN_LOG_LINES_KEPT + 3;
    app.update(|state, cx| {
        let lines = (0..batch).map(|i| format!("line {i}")).collect();
        state.apply_run_event(PluginRunEvent::Log { lines }, cx);
    });
    assert_eq!(
        log_ends(&mut app),
        (
            RUN_LOG_LINES_KEPT,
            "line 3".to_owned(),
            format!("line {}", batch - 1)
        )
    );

    // The line that says part of the log was left out takes the place of the oldest one.
    app.update(|state, cx| {
        let path = "/root/run.log".to_owned();
        state.apply_run_event(PluginRunEvent::LogSkipped { path }, cx);
    });
    let (len, oldest, newest) = log_ends(&mut app);
    assert_eq!((len, oldest.as_str()), (RUN_LOG_LINES_KEPT, "line 4"));
    assert!(newest.contains("/root/run.log"), "{newest}");
}

#[gpui::test]
fn a_failed_stage_says_whether_the_runtime_still_bills(cx: &mut TestAppContext) {
    let options = Options {
        failures: MockFailures {
            plugin_fail_stage: Some("download".into()),
            ..MockFailures::default()
        },
        ..Options::scenario(MockScenario::single())
    };
    let mut app = TestApp::with(cx, options);
    open_deploy_form(&mut app);
    app.fill(
        |inputs| inputs.field("repo").expect("the model field"),
        REPO,
    );
    app.click(RUN_START);
    let Some(RunPhase::Failed(_)) = run_phase(&mut app) else {
        panic!("expected a failure, got {:?}", run_phase(&mut app));
    };
    assert!(app.shown("error-run"));
    let left = app.mock.runtimes(&CancelToken::new()).expect("listing");
    assert!(
        left.runtimes.is_empty(),
        "stop on failure is on by default, so nothing bills: {left:?}"
    );

    // Again, keeping the session: it is still billing and Stop is right there.
    app.click("run-again");
    app.update(|state, cx| state.edit_run_form(|form| form.stop_on_failure = false, cx));
    app.fill(
        |inputs| inputs.field("repo").expect("the model field"),
        REPO,
    );
    app.click(RUN_START);
    assert!(matches!(run_phase(&mut app), Some(RunPhase::Failed(_))));
    let left = app.mock.runtimes(&CancelToken::new()).expect("listing");
    assert_eq!(left.runtimes.len(), 1, "the session was kept");
    assert!(app.shown(RUN_STOP), "with Stop right there");
    app.click(RUN_STOP);
    app.click(colab_playground::ui::overlays::STOP_CONFIRM);
    let left = app.mock.runtimes(&CancelToken::new()).expect("listing");
    assert!(left.runtimes.is_empty());
}

#[gpui::test]
fn a_runtime_is_only_called_stopped_when_its_account_was_listed(cx: &mut TestAppContext) {
    let options = Options {
        failures: MockFailures {
            plugin_fail_stage: Some("download".into()),
            ..MockFailures::default()
        },
        ..Options::scenario(MockScenario::single())
    };
    let mut app = TestApp::with(cx, options);
    open_deploy_form(&mut app);
    app.fill(
        |inputs| inputs.field("repo").expect("the model field"),
        REPO,
    );
    app.click(RUN_START);
    assert!(matches!(run_phase(&mut app), Some(RunPhase::Failed(_))));
    assert!(
        app.shown("run-session-stopped"),
        "the listing no longer has it"
    );
    // The run's account cannot be listed now: whether its runtime is gone is not known.
    app.set_failures(MockFailures {
        unlisted_accounts: vec!["ada@example.com".into()],
        ..MockFailures::default()
    });
    app.update(|state, cx| state.load_runtimes(cx));
    assert!(!app.shown("run-session-stopped"));
    assert!(app.shown(colab_playground::ui::pages::plugins::RUN_SESSION_UNKNOWN_SELECTOR));
}

const FIXTURE_MANIFEST: &str = r#"api = 1
id = "hello-web"
version = "0.2.0"
needs = ["hf_token"]

[name]
en = "Hello web server"
zh-CN = "你好网页服务"
zh-TW = "你好網頁服務"

[description]
en = "Serves a page that says hello."

[[inputs]]
id = "greeting"
type = "text"
default = "hello"
[inputs.label]
en = "Greeting"

[run]
script = "run.sh"

[[stages]]
id = "start"
[stages.label]
en = "Start"

[[endpoints]]
id = "web"
port = 9000
[endpoints.label]
en = "Web page"
"#;

#[gpui::test]
fn a_plugin_from_a_folder_is_installed_after_confirming(cx: &mut TestAppContext) {
    let fixture = tempfile::tempdir().expect("a fixture folder");
    std::fs::write(fixture.path().join("plugin.toml"), FIXTURE_MANIFEST).expect("manifest");
    std::fs::write(fixture.path().join("run.sh"), "#!/bin/bash\necho hello\n").expect("script");
    let mut app = TestApp::new(cx);
    app.click(Route::Plugins.nav_selector());
    assert!(
        app.shown(&plugin_selector("llama-cpp")),
        "the built-in plugin is listed"
    );

    let folder = fixture.path().display().to_string();
    app.fill(|inputs| inputs.plugin_source.clone(), &folder);
    app.click(INSTALL_CHECK);
    let Some(Overlay::InstallPlugin(InstallPrompt::Preview(preview))) =
        app.read(|state, _| state.overlay().cloned())
    else {
        panic!(
            "expected the confirmation, got {:?}",
            app.read(|state, _| state.overlay().cloned())
        );
    };
    assert_eq!(preview.manifest.id, "hello-web");
    assert!(
        app.read(|state, _| state.plugin("hello-web").is_none()),
        "nothing is installed before confirming"
    );
    assert!(app.shown(INSTALL_CONFIRM));

    app.click(INSTALL_CONFIRM);
    assert!(app.read(|state, _| state.overlay().is_none()));
    assert!(app.read(|state, _| state.plugin("hello-web").is_some()));
    assert!(app.shown(&plugin_selector("hello-web")));
    let installed = app.cx.update(|_, cx| {
        colab_playground::i18n::tr_args(cx, keys::PLUGIN_INSTALLED, &[("name", "Hello web server")])
    });
    assert!(app.toasts().contains(&installed), "{:?}", app.toasts());

    // It can be removed (after confirming); built-in plugins cannot.
    app.click("plugin-remove hello-web");
    app.click(colab_playground::ui::overlays::REMOVE_CONFIRM);
    assert!(app.read(|state, _| state.plugin("hello-web").is_none()));
}

/// The copies the mock backend has staged of the plugin `id`. Tests share its staging folder,
/// so a test looks for an id that only it installs.
fn staged_copies(id: &str) -> Vec<PathBuf> {
    let staging =
        std::env::temp_dir().join(format!("colab-playground-mock-{}", std::process::id()));
    let Ok(entries) = std::fs::read_dir(staging) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| {
            std::fs::read_to_string(dir.join("plugin.toml"))
                .is_ok_and(|manifest| manifest.contains(&format!("id = \"{id}\"")))
        })
        .collect()
}

#[gpui::test]
fn closing_the_install_confirmation_drops_the_staged_copy(cx: &mut TestAppContext) {
    const ID: &str = "hello-closed";
    let fixture = tempfile::tempdir().expect("a fixture folder");
    std::fs::write(
        fixture.path().join("plugin.toml"),
        FIXTURE_MANIFEST.replace("hello-web", ID),
    )
    .expect("manifest");
    std::fs::write(fixture.path().join("run.sh"), "#!/bin/bash\necho hello\n").expect("script");
    let mut app = TestApp::new(cx);
    app.click(Route::Plugins.nav_selector());
    let folder = fixture.path().display().to_string();

    let check = |app: &mut TestApp| {
        app.fill(|inputs| inputs.plugin_source.clone(), &folder);
        app.click(INSTALL_CHECK);
        let Some(Overlay::InstallPlugin(InstallPrompt::Preview(preview))) =
            app.read(|state, _| state.overlay().cloned())
        else {
            panic!("expected the confirmation");
        };
        assert_eq!(staged_copies(ID), std::slice::from_ref(&preview.staged_dir));
        preview.staged_dir.clone()
    };

    check(&mut app);
    app.press("escape");
    assert!(app.read(|state, _| state.overlay().is_none()));
    assert!(staged_copies(ID).is_empty(), "Escape leaves nothing staged");

    check(&mut app);
    app.update(|state, cx| state.close_overlay(cx));
    assert!(
        staged_copies(ID).is_empty(),
        "neither does Cancel or the close button"
    );
    assert!(app.read(|state, _| state.plugin(ID).is_none()));

    // Closed before the check has returned: the copy it then stages has no dialog to go to.
    app.update(|state, cx| {
        state.inspect_plugin(folder.clone(), cx);
        state.close_overlay(cx);
    });
    assert!(app.read(|state, _| state.overlay().is_none()));
    assert!(staged_copies(ID).is_empty());

    // The quit confirmation takes the place of the dialog.
    check(&mut app);
    app.update(|state, cx| state.request_quit(cx));
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::Quit(_))
    ));
    assert!(staged_copies(ID).is_empty());
    app.update(|state, cx| state.close_overlay(cx));

    // A failed install has used the copy up as well.
    let staged = check(&mut app);
    std::fs::remove_file(staged.join("run.sh")).expect("the staged script");
    app.click(INSTALL_CONFIRM);
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::InstallPlugin(InstallPrompt::Failed(_)))
    ));
    assert!(staged_copies(ID).is_empty());
}

#[gpui::test]
fn closing_the_install_dialog_stops_its_check(cx: &mut TestAppContext) {
    let fixture = tempfile::tempdir().expect("a fixture folder");
    std::fs::write(fixture.path().join("plugin.toml"), FIXTURE_MANIFEST).expect("manifest");
    std::fs::write(fixture.path().join("run.sh"), "#!/bin/bash\necho hello\n").expect("script");
    let folder = fixture.path().display().to_string();
    let mut app = TestApp::new(cx);
    app.click(Route::Plugins.nav_selector());

    // Headless calls finish as soon as the app settles, so the dialog is closed in the same
    // step that opened it, while its check has not answered.
    let source = folder.clone();
    app.update(|state, cx| {
        state.inspect_plugin(source.clone(), cx);
        assert!(matches!(
            state.overlay(),
            Some(Overlay::InstallPlugin(InstallPrompt::Checking))
        ));
        state.close_overlay(cx);
        assert!(state.is_cancelling(Op::InspectPlugin), "the check goes on");
        // Until it has stopped, no other check starts: its answer would fill that dialog.
        state.inspect_plugin(source, cx);
        assert!(state.overlay().is_none());
    });
    assert!(app.read(|state, _| !state.is_busy(Op::InspectPlugin)));
    assert!(
        app.read(|state, _| state.overlay().is_none()),
        "the stopped check opened the dialog again"
    );

    app.fill(|inputs| inputs.plugin_source.clone(), &folder);
    app.click(INSTALL_CHECK);
    assert!(matches!(
        app.read(|state, _| state.overlay().cloned()),
        Some(Overlay::InstallPlugin(InstallPrompt::Preview(_)))
    ));
}

#[gpui::test]
fn secrets_never_show_up_anywhere(cx: &mut TestAppContext) {
    const SECRET: &str = "sk-very-secret-4711";
    const TOKEN: &str = "hf_token-secret-0815";
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));

    // The Hugging Face token: saved from the masked field, which is then emptied.
    app.click(Route::Plugins.nav_selector());
    app.fill(|inputs| inputs.hf_token.clone(), TOKEN);
    app.click("hf-token-save");
    assert_eq!(
        app.read(|state, _| state.hf_token().ready().copied()),
        Some(true)
    );
    let field = app.read_root(|root, cx| root.inputs().hf_token.read(cx).text().to_owned());
    assert!(field.is_empty(), "the field is cleared");

    // A secret run input.
    open_deploy_form_from_plugins(&mut app);
    app.fill(
        |inputs| inputs.field("repo").expect("the model field"),
        REPO,
    );
    app.fill(
        |inputs| inputs.field("api_key").expect("the key field"),
        SECRET,
    );
    app.click(RUN_START);
    assert!(matches!(run_phase(&mut app), Some(RunPhase::Ready(_))));

    let field = app.read_root(|root, cx| {
        root.inputs()
            .field("api_key")
            .map(|f| f.read(cx).text().to_owned())
            .unwrap_or_default()
    });
    assert!(
        field.is_empty(),
        "the secret field is cleared after starting"
    );
    let seen = app.read(|state, _| {
        format!(
            "{:?}\n{:?}\n{:?}",
            state.plugin_run(),
            state.run_form(),
            state.tunnels().collect::<Vec<_>>()
        )
    });
    for secret in [SECRET, TOKEN] {
        assert!(
            !seen.contains(secret),
            "{secret} leaked into state or events"
        );
        assert!(
            !app.toasts().iter().any(|t| t.contains(secret)),
            "{secret} leaked into a toast"
        );
        if let Some(file) = app.settings_file() {
            let text = std::fs::read_to_string(file).unwrap_or_default();
            assert!(!text.contains(secret), "{secret} leaked into settings.json");
        }
    }

    // Running again asks for the secret again.
    app.click("run-again");
    let field = app.read_root(|root, cx| {
        root.inputs()
            .field("api_key")
            .map(|f| f.read(cx).text().to_owned())
            .unwrap_or_default()
    });
    assert!(field.is_empty());
}

#[gpui::test]
fn a_rejected_deploy_keeps_the_secret_typed(cx: &mut TestAppContext) {
    const SECRET: &str = "sk-very-secret-4711";
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    app.click(Route::Plugins.nav_selector());
    open_deploy_form_from_plugins(&mut app);
    let key = |app: &mut TestApp<'_>| {
        app.read_root(|root, cx| {
            root.inputs()
                .field("api_key")
                .map(|f| f.read(cx).text().to_owned())
                .unwrap_or_default()
        })
    };
    let rejected = |app: &mut TestApp<'_>| {
        app.read(|state, _| {
            state.plugin_view() == PluginView::Form
                && state.plugin_run().is_none()
                && state
                    .run_form()
                    .and_then(|form| form.error.as_ref())
                    .map(|e| e.kind)
                    == Some(ErrorKind::InvalidInput)
        })
    };

    // The required model is missing.
    app.fill(|inputs| inputs.field("repo").expect("the model field"), "");
    app.fill(
        |inputs| inputs.field("api_key").expect("the key field"),
        SECRET,
    );
    app.click(RUN_START);
    assert!(rejected(&mut app), "the form stays open with the error");
    assert_eq!(key(&mut app), SECRET, "the secret is still in its field");

    // A number that does not parse.
    app.fill(
        |inputs| inputs.field("repo").expect("the model field"),
        REPO,
    );
    app.fill(
        |inputs| inputs.field("ctx").expect("the context field"),
        "1.2.3",
    );
    app.click(RUN_START);
    assert!(rejected(&mut app), "the form stays open with the error");
    assert_eq!(key(&mut app), SECRET, "the secret is still in its field");

    app.fill(|inputs| inputs.field("ctx").expect("the context field"), "");
    app.click(RUN_START);
    assert!(matches!(run_phase(&mut app), Some(RunPhase::Ready(_))));
    assert!(
        key(&mut app).is_empty(),
        "the secret field is cleared once the run started"
    );
}

fn open_deploy_form_from_plugins(app: &mut TestApp<'_>) {
    app.click("plugin-run llama-cpp");
    assert_eq!(app.read(|state, _| state.plugin_view()), PluginView::Form);
}

#[gpui::test]
fn cancelling_a_deployment_leaves_nothing_billing(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    open_deploy_form(&mut app);
    app.fill(
        |inputs| inputs.field("repo").expect("the model field"),
        REPO,
    );
    // Wait 2 of the run is the hardware probe of its fresh session: a VM exists by then.
    app.cancel_switch.arm(2);
    app.click(RUN_START);
    assert_eq!(run_phase(&mut app), Some(RunPhase::Cancelled));
    let provision = app.read(|state, _| {
        state
            .plugin_run()
            .map(|run| run.provision.events.clone())
            .unwrap_or_default()
    });
    assert!(
        provision.iter().any(|e| matches!(
            e,
            colab_playground_core::provision::ProvisionEvent::Releasing { .. }
        )),
        "the session it allocated was released: {provision:?}"
    );
    let left = app.mock.runtimes(&CancelToken::new()).expect("listing");
    assert!(left.runtimes.is_empty(), "nothing bills: {left:?}");
}

#[gpui::test]
fn plugins_are_listed_before_the_cli_is_installed(cx: &mut TestAppContext) {
    let options = Options {
        failures: MockFailures {
            cli_missing: true,
            ..MockFailures::default()
        },
        ..Options::scenario(MockScenario::signed_out())
    };
    let mut app = TestApp::with(cx, options);
    app.click(Route::Plugins.nav_selector());
    assert!(
        app.shown(&plugin_selector("llama-cpp")),
        "the list does not wait for the CLI"
    );
    let token = app.read(|state, _| state.hf_token().clone());
    assert!(
        matches!(&token, Load::Failed(error) if error.is(ErrorKind::NotInstalled)),
        "the keyring cannot be read yet: {token:?}"
    );

    app.click(Route::Overview.nav_selector());
    app.click(INSTALL_SELECTOR);
    assert_eq!(
        app.read(|state, _| state.hf_token().clone()),
        Load::Ready(false),
        "checked once the CLI is there"
    );
}

#[gpui::test]
fn accounts_cannot_change_while_a_deployment_runs(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    let lin = app.read(|state, _| {
        state.accounts().ready().and_then(|view| {
            view.accounts
                .iter()
                .find(|a| a.email.as_deref() == Some("lin@example.com"))
                .map(|a| a.id.clone())
        })
    });
    let lin = lin.expect("the demo's second account");
    app.click(Route::Plugins.nav_selector());
    app.click("plugin-run llama-cpp");
    app.update(|state, cx| {
        // Pick an existing session so the run does not hit the session limit.
        state.edit_run_form(|form| form.existing = Some("a100-7f3a".into()), cx);
        state.start_run(vec![("repo".into(), REPO.into())], String::new(), cx);
        assert!(state.is_running_plugin());
        assert!(state.account_change_block().is_some());
        state.request_switch(lin.clone(), cx);
        state.open_add_account(cx);
        assert!(
            state.dialog().is_none(),
            "neither a switch nor a sign-in starts"
        );
    });
    let blocked = app.text(keys::ACCOUNT_CHANGE_BLOCKED);
    assert!(app.toasts().contains(&blocked), "{:?}", app.toasts());
}

#[gpui::test]
fn the_forms_fields_follow_the_language(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    app.click(Route::Plugins.nav_selector());
    app.click("plugin-run llama-cpp");
    app.fill(|inputs| inputs.fields["repo"].clone(), REPO);
    let field =
        |app: &mut TestApp<'_>| app.read_root(|root, _| root.inputs().fields["repo"].clone());
    let before = field(&mut app);

    app.update(|state, cx| state.set_language(LanguagePref::ZhCn, cx));
    let after = field(&mut app);
    assert_ne!(
        before.entity_id(),
        after.entity_id(),
        "the field still carries the label of the language it was made in"
    );
    let typed = app.cx.update(|_, cx| after.read(cx).text().to_owned());
    assert_eq!(typed, REPO, "what was typed stays");
}

/// The run form on the demo's managed session, and that session.
fn form_on_a100(app: &mut TestApp<'_>) -> RuntimeRef {
    app.click(Route::Plugins.nav_selector());
    app.click("plugin-run llama-cpp");
    let picked = app.read(|state, _| {
        let view = state.runtimes().ready().expect("listed");
        let picked = view
            .runtimes
            .iter()
            .find(|runtime| runtime.id.name.as_deref() == Some("a100-7f3a"));
        picked.expect("the demo's managed session").id.clone()
    });
    app.update(|state, cx| state.pick_run_target(Some("a100-7f3a".into()), cx));
    picked
}

fn form_target(app: &mut TestApp<'_>) -> Option<String> {
    app.read(|state, _| state.run_form().and_then(|form| form.existing.clone()))
}

const TARGET_ENDED: &str = "run-target-ended";

#[gpui::test]
fn a_stopped_session_is_no_longer_the_forms_target(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    let picked = form_on_a100(&mut app);
    app.update(|state, cx| state.load_runtimes(cx));
    assert_eq!(
        form_target(&mut app).as_deref(),
        Some("a100-7f3a"),
        "kept while it is listed"
    );
    assert!(!app.shown(TARGET_ENDED));

    app.update(|state, cx| {
        state.request_stop(picked, cx);
        state.confirm_stop(cx);
    });
    assert_eq!(
        form_target(&mut app),
        None,
        "the form is back on a new session"
    );
    // A new session bills, where the run used to fail: the form says what changed.
    assert!(app.shown(TARGET_ENDED));
    let lost = app.read(|state, _| state.run_form().and_then(|form| form.target_lost.clone()));
    assert_eq!(lost, Some(TargetLost::Ended("a100-7f3a".into())));

    app.update(|state, cx| state.pick_run_target(None, cx));
    assert!(
        !app.shown(TARGET_ENDED),
        "gone once a target is picked again"
    );
}

/// Makes the demo's second account the active one.
fn switch_to_lin(app: &mut TestApp<'_>) {
    let lin = app.read(|state, _| {
        let accounts = &state.accounts().ready().expect("accounts").accounts;
        let lin = accounts
            .iter()
            .find(|account| account.email.as_deref() == Some("lin@example.com"));
        lin.expect("the demo's second account").id.clone()
    });
    app.update(|state, cx| state.request_switch(lin, cx));
    app.update(|state, cx| state.confirm_switch(cx));
}

#[gpui::test]
fn a_target_left_on_another_account_is_not_called_ended(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    form_on_a100(&mut app);
    switch_to_lin(&mut app);
    let running = app.read(|state, _| {
        let view = state.runtimes().ready().expect("listed");
        view.runtimes
            .iter()
            .any(|runtime| runtime.id.name.as_deref() == Some("a100-7f3a"))
    });
    assert!(
        running,
        "the session still bills on the account left behind"
    );

    // It cannot be run on from here, and the form does not say it has ended.
    assert_eq!(form_target(&mut app), None);
    let lost = app.read(|state, _| state.run_form().and_then(|form| form.target_lost.clone()));
    assert_eq!(lost, Some(TargetLost::OtherAccount("a100-7f3a".into())));
    app.click(Route::Plugins.nav_selector());
    assert!(app.shown("run-target-other-account"));
    assert!(!app.shown(TARGET_ENDED));
}

#[gpui::test]
fn a_session_of_the_same_name_on_the_account_switched_to_is_not_the_target(
    cx: &mut TestAppContext,
) {
    // A name is unique within an account only.
    let mut scenario = MockScenario::demo();
    scenario.runtimes.push(MockRuntime {
        account: 1,
        name: Some("a100-7f3a".into()),
        accelerator: Accelerator::T4,
        managed: true,
    });
    let mut app = TestApp::with(cx, Options::scenario(scenario));
    form_on_a100(&mut app);
    switch_to_lin(&mut app);
    let on_lin = app.read(|state, _| {
        let view = state.runtimes().ready().expect("listed");
        view.runtimes.iter().any(|runtime| {
            runtime.on_active_account && runtime.id.name.as_deref() == Some("a100-7f3a")
        })
    });
    assert!(on_lin, "the account switched to has a session of that name");

    // The run would land on that other session without a word.
    assert_eq!(form_target(&mut app), None);
    let lost = app.read(|state, _| state.run_form().and_then(|form| form.target_lost.clone()));
    assert_eq!(lost, Some(TargetLost::OtherAccount("a100-7f3a".into())));
}

#[gpui::test]
fn another_account_that_cannot_be_listed_does_not_keep_a_stopped_target(cx: &mut TestAppContext) {
    let options = Options {
        failures: MockFailures {
            unlisted_accounts: vec!["lin@example.com".into()],
            ..Default::default()
        },
        ..Options::default()
    };
    let mut app = TestApp::with(cx, options);
    let picked = form_on_a100(&mut app);
    app.update(|state, cx| {
        state.request_stop(picked, cx);
        state.confirm_stop(cx);
    });
    assert_eq!(form_target(&mut app), None);
    assert!(app.shown(TARGET_ENDED));
}

#[gpui::test]
fn a_target_is_kept_while_its_own_account_cannot_be_listed(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    form_on_a100(&mut app);
    app.set_failures(MockFailures {
        unlisted_accounts: vec!["ada@example.com".into()],
        ..Default::default()
    });
    app.update(|state, cx| state.load_runtimes(cx));
    let listed = app.read(|state, _| state.runtimes().ready().map(|view| view.errors.len()));
    assert_eq!(listed, Some(1), "the active account's listing failed");
    assert_eq!(
        form_target(&mut app).as_deref(),
        Some("a100-7f3a"),
        "a listing that failed says nothing about the session"
    );
    assert!(!app.shown(TARGET_ENDED));
}

// Several iterations shuffle task scheduling, so the older search can finish last.
#[gpui::test(iterations = 12)]
fn only_the_latest_search_is_shown(cx: &mut TestAppContext) {
    let mut app = TestApp::with(cx, Options::scenario(MockScenario::single()));
    open_deploy_form(&mut app);
    app.update(|state, cx| {
        state.search_hf("qwen3".into(), cx);
        state.search_hf("8b".into(), cx);
    });
    app.settle();
    let results: Vec<String> = app.read(|state, _| {
        state
            .run_form()
            .and_then(|form| form.hf.results.ready().cloned())
            .unwrap_or_default()
            .into_iter()
            .map(|model| model.id)
            .collect()
    });
    assert!(!results.is_empty());
    assert!(
        results
            .iter()
            .all(|id| id.to_ascii_lowercase().contains("8b")),
        "results match the last query only: {results:?}"
    );
}
