//! A local endpoint an agent can drive the app through.
//!
//! An agent that can reach this app can work with the same state the window shows: what is running,
//! what a plugin run has produced, what the settings are. The endpoint speaks MCP over Streamable
//! HTTP on `127.0.0.1`, requires a token, and refuses anything that did not come from an agent the
//! user configured.
//!
//! The split this module is built around: the protocol, the transport and the tool table live here
//! in `core`, where a test can drive them against a mock, while the app supplies a
//! [`view::Source`] — the snapshot of its own state. Nothing in this module reaches into the app,
//! and nothing in slice 1 changes state, spends money or starts a session: the tools are read-only
//! until the app can ask the user first.
//!
//! The design and the later slices: `docs/superpowers/specs/2026-11-24-agent-mcp-endpoint-design.md`.

pub mod http;
pub mod protocol;
pub mod tools;
pub mod view;

mod server;

pub use server::{Config, Endpoint, PATH, SESSION_HEADER};

/// What the endpoint calls itself in `initialize`.
pub const SERVER_NAME: &str = "colab-playground";

/// The secret the endpoint's token is kept under, in the app's own secret store.
pub const TOKEN_SECRET: &str = "mcp_token";

/// A fresh token: 128 bits of the workspace's unique string.
///
/// It is not cryptographic randomness, and the workspace says so about its own helper — a token
/// guards a loopback endpoint against a page in the user's browser and against a stray process,
/// which is what the design claims for it and no more.
pub fn new_token() -> String {
    format!("{}{}", crate::accounts::nonce(), crate::accounts::nonce())
}
