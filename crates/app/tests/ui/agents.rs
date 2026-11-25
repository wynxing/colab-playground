//! The Settings → Agents section: what it shows while the endpoint is off, what switching it on
//! does, and the token's two states. The endpoint itself is core's and is tested there; these
//! tests are about the window.

use colab_playground::{
    route::Route,
    ui::pages::settings::{
        AGENTS_ADDRESS_SELECTOR, AGENTS_CONFIG_SELECTOR, AGENTS_TOGGLE_SELECTOR,
        AGENTS_TOKEN_HIDDEN_SELECTOR, AGENTS_TOKEN_REVEAL_SELECTOR, AGENTS_TOKEN_ROTATE_SELECTOR,
        AGENTS_TOKEN_SHOWN_SELECTOR,
    },
};
use gpui::TestAppContext;

use crate::support::{ConfigDir, Options, TestApp};

/// Settings that switch the endpoint on with a port the system picks, so two runs of these tests
/// cannot collide on a fixed one.
const ON: &str = r#"{"mcp_enabled":true,"mcp_port":0}"#;

fn open_settings(cx: &mut TestAppContext, options: Options) -> TestApp<'_> {
    let mut app = TestApp::with(cx, options);
    app.click(Route::Settings.nav_selector());
    assert_eq!(app.route(), Route::Settings);
    app
}

fn with_settings<'a>(cx: &'a mut TestAppContext, text: &str) -> TestApp<'a> {
    let options = Options {
        config: ConfigDir::TempWith(text.to_owned()),
        ..Options::default()
    };
    open_settings(cx, options)
}

#[gpui::test]
fn the_section_is_there_and_listens_to_nothing_until_it_is_switched_on(cx: &mut TestAppContext) {
    let mut app = open_settings(cx, Options::default());
    assert!(app.shown(AGENTS_TOGGLE_SELECTOR));
    assert!(!app.read(|state, _| state.agents_enabled()));
    assert!(app.read(|state, _| state.agents().address().is_none()));
    // Nothing to show or copy while it is off.
    assert!(!app.shown(AGENTS_ADDRESS_SELECTOR));
    assert!(!app.shown(AGENTS_TOKEN_SHOWN_SELECTOR));
    assert!(!app.shown(AGENTS_CONFIG_SELECTOR));
}

#[gpui::test]
fn switching_it_on_binds_loopback_saves_it_and_switching_it_off_frees_the_port(
    cx: &mut TestAppContext,
) {
    // On a port the system picks: the app on this machine may well hold the default one, and what
    // this test is about is the switch binding at all, not which port it lands on.
    let mut app = with_settings(cx, r#"{"mcp_port":0}"#);

    app.click(AGENTS_TOGGLE_SELECTOR);
    app.settle();
    assert!(app.read(|state, _| state.agents_enabled()));
    let address = app
        .read(|state, _| state.agents().address())
        .expect("it listens after being switched on");
    assert!(address.ip().is_loopback(), "{address} is not loopback");
    assert!(app.shown(AGENTS_ADDRESS_SELECTOR));
    assert_eq!(
        app.saved_settings().map(|settings| settings.mcp_enabled),
        Some(true),
        "the switch is a setting, so it survives a restart"
    );

    app.click(AGENTS_TOGGLE_SELECTOR);
    app.settle();
    assert!(!app.read(|state, _| state.agents_enabled()));
    assert!(app.read(|state, _| state.agents().address().is_none()));
    assert!(
        std::net::TcpListener::bind(address).is_ok(),
        "a stopped endpoint holds no port"
    );
}

#[gpui::test]
fn the_token_is_hidden_until_it_is_asked_for_and_replacing_it_makes_a_new_one(
    cx: &mut TestAppContext,
) {
    let mut app = with_settings(cx, ON);
    let first = app
        .read(|state, _| state.agents().token().map(str::to_owned))
        .expect("an endpoint that was asked for has a token");

    assert!(
        app.shown(AGENTS_TOKEN_HIDDEN_SELECTOR),
        "hidden to begin with"
    );
    assert!(!app.shown(AGENTS_TOKEN_SHOWN_SELECTOR));

    app.click(AGENTS_TOKEN_REVEAL_SELECTOR);
    assert!(
        app.shown(AGENTS_TOKEN_SHOWN_SELECTOR),
        "shown once asked for"
    );
    assert!(!app.shown(AGENTS_TOKEN_HIDDEN_SELECTOR));

    // What an agent needs is on the page: where to connect and what to bring.
    assert!(app.shown(AGENTS_ADDRESS_SELECTOR));
    assert!(app.shown(AGENTS_CONFIG_SELECTOR));

    app.click(AGENTS_TOKEN_ROTATE_SELECTOR);
    app.settle();
    let second = app
        .read(|state, _| state.agents().token().map(str::to_owned))
        .expect("a token after replacing");
    assert_ne!(
        second, first,
        "replacing it stops an agent with the old one"
    );
}
