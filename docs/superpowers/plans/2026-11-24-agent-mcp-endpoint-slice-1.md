# A local MCP endpoint (slice 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An agent connected to `127.0.0.1` gets read-only access to what the running app knows — sessions, usage, plugin runs, run outputs, plugins, settings, accounts, forwards — over MCP, and the user can switch that on in Settings.

**Architecture:** The protocol, the transport and the tool table live in `core` (`crates/core/src/mcp/`), where tests drive them without a window; the app supplies a `Source` — a snapshot of the state the window renders — so an agent reads the same truth the interface shows. Nothing in this slice changes state, spends money or starts a session.

**Tech Stack:** Rust (workspace `edition 2024`, `rust-version 1.98`), `std::net`/`std::io`/`std::thread`, the workspace's `serde`/`serde_json`, `crates/core/src/error.rs` for errors, GPUI and Ely components for the settings section.

**Spec:** `docs/superpowers/specs/2026-11-24-agent-mcp-endpoint-design.md`

## Global Constraints

- **Toolchain.** The workspace wants `rust-version = 1.98`; this machine's `stable` is 1.97, so every command below runs as `rustup run 1.99.0 cargo …`.
- **No new dependencies.** The endpoint uses `std::net`, `std::io`, `std::sync`, `std::thread` and the `serde`/`serde_json` already in the workspace. `crates/core/Cargo.toml` is not touched.
- **Errors are core's own.** `crate::error::{Error, ErrorKind, Result}`; `core` produces machine-readable errors and no user-facing prose. A new `ErrorKind` only when no existing one fits.
- **Every string the window shows** comes from a `keys::` constant through `tr`/`tr_args`: one line in the `catalog_keys!` block of `crates/app/src/i18n/mod.rs` and one `(KEY, "text")` row in each of the five catalogs. Three tests enforce it — `every_catalog_holds_exactly_the_declared_keys_once`, `every_key_resolves_in_every_language_with_the_same_arguments`, and `app_code_names_keys_only_through_the_constants` (no `"app.` literal outside `i18n/`).
- **Every new interface element** carries a `tagged(SELECTOR, …)` selector whose constant is defined next to the page, and a test in `crates/app/tests/ui/` registered as a `mod …;` in `crates/app/tests/ui/main.rs`. No bare number: `crates/app/tests/lint.rs`'s `ui_code_names_its_numbers` refuses any literal except 0 and 1, so a size is a `const` or comes from the theme.
- **No module names an operating system.** `lint.rs`'s `only_the_platform_modules_name_a_target_os` allows `windows`/`unix`/`target_os` only in `platform.rs`, `host.rs` and `paths.rs`; what differs per OS is handled at run time (which is how the accepted-socket fix in task 5 works), never with `cfg`.
- **Before each commit:** `rustup run 1.99.0 cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`.
- **Commits:** conventional-commit subjects, one commit per task, and no attribution trailer that credits an agent — the repository forbids that in `AGENTS.md`. The changelog is written by hand at release time.
- **Landing:** nothing is pushed to `main` directly (a hard safety rule in `DESIGN.md`). The branch goes up, and the slice lands as one squash-merged pull request whose title becomes the changelog line.
- **A check is made once:** every failure path this slice can reach is a test, never a screenshot or something a person has to remember (`TESTING.md`).
- **Comments say why, not what**, and a doc section is named by its heading, never by a number (`DEVELOPMENT.md`).
- **Nothing in this slice may start, extend, stop or bill anything.** Starting a session, running a plugin and opening a forward are slice 2, and each of those asks in the app's own dialog first.

---

### Task 1: The MCP message shapes — LANDED

**Files:**
- Create: `crates/core/src/mcp/protocol.rs`

**Interfaces:**
- Produces: `PROTOCOL_VERSION: &str`, `SUPPORTED_VERSIONS: [&str; 2]`, the JSON-RPC codes, `Incoming::{Request, Notification}`, `Fault { code, message, id }` with `Fault::new/to/response`, `parse(&[u8]) -> Result<Incoming, Fault>`, `result(&Value, Value) -> Value`, `negotiate_version(Option<&str>) -> &'static str`, `initialize_result(name, version, Option<&str>) -> Value`.

- [x] **Step 1: Failing test — a request with an id is a request, one without is a notification** (`crates/core/src/mcp/protocol.rs`, `mod tests`).
- [x] **Step 2: Failing test — a body that is not JSON-RPC 2.0 is a fault with no id** (five bodies, including a `null` id, which stays a request).
- [x] **Step 3: Failing test — the revision answered is the client's when it is known, otherwise ours.**
- [x] **Step 4: Implement `parse`, `Fault`, `result`, `negotiate_version`, `initialize_result`.**
- [x] **Step 5: Run** `rustup run 1.99.0 cargo test -p colab-playground-core --lib mcp::protocol` — 7 tests pass.
- [x] **Step 6: Commit** `feat(core): serve a local agent endpoint over MCP` (one commit covers tasks 1–5).

### Task 2: A minimal loopback HTTP exchange — LANDED

**Files:**
- Create: `crates/core/src/mcp/http.rs`

**Interfaces:**
- Consumes: `crate::error::{Error, ErrorKind, Result}`.
- Produces: `MAX_HEAD_BYTES`, `MAX_BODY_BYTES`, `Request { method, target, headers, body }` with `header/path/bearer`, `ReadOutcome::{Request, Closed}`, `read_request(&mut impl BufRead) -> Result<ReadOutcome>`, `Response { status, content_type, body, extra }` with `json/text/with/bytes/write_to`, `is_disconnect(&io::Error) -> bool`.

- [x] **Step 1: Failing test — a POST is read with its body and headers, header names matched without case, a query string left out of the path.**
- [x] **Step 2: Failing test — a closed connection is `ReadOutcome::Closed`, not an error; a request line that is not HTTP/1.x, a header without a colon, a chunked body and a body over the limit are refused (the chunked one as `Unsupported`).**
- [x] **Step 3: Failing test — a head that never ends is refused at `MAX_HEAD_BYTES` rather than allocated for.**
- [x] **Step 4: Implement the reader and the writer.** A body is read only when `Content-Length` says so; `Transfer-Encoding` is refused; the response carries `Content-Type`, `Content-Length`, `Cache-Control: no-store` and any extra header (the session id).
- [x] **Step 5: Run** `--lib mcp::http` — 8 tests pass.
- [x] **Step 6: Commit** with task 1's commit.

### Task 3: What a tool may read — LANDED

**Files:**
- Create: `crates/core/src/mcp/view.rs`

**Interfaces:**
- Produces: `trait Source: Send + Sync { fn snapshot(&self) -> Snapshot }`, `Snapshot { sessions, usage, runs, plugins, accounts, forwards, settings }` and its members (`Session`, `Usage`, `Run`, `Workflow`, `Node`, `Link`, `Output`, `Plugin`, `Account`, `Forward`, `AppSettings`), all `Serialize`, plus `Empty` for tests.

- [x] **Step 1: Failing test — an empty source answers with empty lists.**
- [x] **Step 2: Failing test — a snapshot serializes with numbers as numbers and an absent host setting as `null`.**
- [x] **Step 3: Implement the shapes.** Anything a person reads (a stage, an address) is a string; anything money-shaped is a number; an absent run is an error from the tool, never a run with nulls in it.
- [x] **Step 4: Run** `--lib mcp::view` — 2 tests pass.
- [x] **Step 5: Commit** with task 1's commit.

### Task 4: The read tools and their argument checks — LANDED

**Files:**
- Create: `crates/core/src/mcp/tools.rs`

**Interfaces:**
- Consumes: `view::Snapshot`.
- Produces: `Tool { name, description, input, read }`, `Args::parse(Option<&Value>)`, `all() -> &'static [Tool]` (nine tools), `find(&str) -> Option<&'static Tool>`, `descriptors() -> Value`, `call(&Snapshot, &str, Option<&Value>) -> Result<Value>`.

- [x] **Step 1: Failing test — the table: nine names, all `snake_case`, each description long enough to serve as the agent's whole manual, every schema `type: object` with `additionalProperties: false`** (`every_tool_is_named_uniquely_and_explains_itself`).
- [x] **Step 2: Failing test — `tools/list` reports the table, and every tool answers an object** (`tools_list_reports_what_the_table_holds`, `a_tool_never_answers_with_anything_but_an_object`).
- [x] **Step 3: Failing test — a read tool answers from the snapshot; an empty state answers with empty lists, not an error** (`a_read_tool_answers_from_the_snapshot`, `an_empty_state_answers_with_empty_lists_not_an_error`).
- [x] **Step 4: Failing test — a run that does not exist is an error naming `runs_list`, an unknown tool names itself, and a missing / wrongly typed / surplus argument is refused** (`a_run_that_does_not_exist_is_an_error_not_an_empty_answer`, `an_unknown_tool_names_itself_in_the_error`, `arguments_are_checked_strictly`).
- [x] **Step 5: Implement the nine read tools** — `sessions_list`, `session_usage`, `runs_list`, `run_status`, `run_outputs`, `plugin_list`, `settings_get`, `accounts_list`, `forwards_list`. `run_status` and `run_outputs` take `{"plugin": "<id>"}` and nothing else.
- [x] **Step 6: Run** `--lib mcp::tools` — 7 tests pass.
- [x] **Step 7: Commit** with task 1's commit.

### Task 5: The server: bind, token, origin, sessions, dispatch — LANDED

**Files:**
- Create: `crates/core/src/mcp/server.rs`, `crates/core/src/mcp/mod.rs`
- Modify: `crates/core/src/lib.rs` (one line: `pub mod mcp;`, in alphabetical order)
- Test: `crates/core/tests/mcp_endpoint.rs`

**Interfaces:**
- Consumes: `protocol`, `http`, `tools`, `view::Source`.
- Produces: `Config { token, port }`, `Endpoint::start(Config, Arc<dyn Source>) -> Result<Endpoint>`, `Endpoint::addr`, `Endpoint::stop`, `PATH`, `SESSION_HEADER`, `SERVER_NAME`, `tool_result`, `tool_failure`, `machine_error` (the app's own `Error`, serialized: `kind`, `params`, `detail`, `next_step`).

- [x] **Step 1: Failing test (in-file) — without a token, or with the wrong one, nothing is served; a foreign `Origin` is refused and the endpoint's own origin is allowed.**
- [x] **Step 2: Failing test (in-file) — another path is 404, `GET` and `PATCH` are 405, `initialize` answers and hands out a 16-character session id, a session the endpoint never issued is 404, a notification is 202 with an empty body.**
- [x] **Step 3: Failing test (in-file) — `tools/list` brings the table, a tool call reads the source and wraps its answer, a refused tool is a result marked `isError`, a tool that is not there is a JSON-RPC fault, a broken body is a parse fault with status 200, `DELETE` closes a session.**
- [x] **Step 4: Implement the listener**: bind `127.0.0.1` (port 0 means the system picks), accept without blocking, one thread per connection, `handle` deciding everything from the request, `dispatch` routing the five methods, session ids tracked in a `Mutex<HashSet<String>>`, tokens compared without an early exit.
- [x] **Step 5: Failing test (integration, real socket)** in `crates/core/tests/mcp_endpoint.rs`: `initialize` → `tools/list` → `tools/call` over one kept-alive connection; a repeated session id; a wrong token (401), a foreign origin (403), another path (404), `GET` (405); a request the endpoint cannot read (400) with the listener still answering afterwards; `stop()` frees the port.
- [x] **Step 6: Implement what the socket test found.** Windows hands an accepted socket the listener's non-blocking mode, so `serve` sets it back to blocking before reading; without that, every served request failed its first read with `WSAEWOULDBLOCK`.
- [x] **Step 7: Run** `--test mcp_endpoint` — 5 tests pass; `--lib mcp` — 40 tests pass.
- [x] **Step 8: Commit** with task 1's commit.
- [x] **Step 9: After a pass over the repository's own conventions** (`accounts.rs`, `error.rs`, `lint.rs`): the session id comes from `accounts::nonce()` instead of a duplicated hash, and `machine_error` serializes the app's own `Error` instead of formatting a string, so a caller reads `kind`, `params`, `detail` and `next_step`. The tests above cover both, and `cargo test -p colab-playground --test lint` passes with the new module in place.

### Task 6: The token, and where the app keeps its endpoint's address

**Files:**
- Modify: `crates/core/src/backend.rs` (two methods on the `Backend` trait), `crates/core/src/real.rs`, `crates/core/src/mock.rs`
- Modify: `crates/app/src/settings.rs` (`Settings`: `mcp_enabled`, `mcp_port`)

**Interfaces:**
- Produces on `Backend`: `fn mcp_token(&self, cancel: &CancelToken) -> Result<String>` (created on first use, stored under the secret name `mcp_token`), `fn rotate_mcp_token(&self, cancel: &CancelToken) -> Result<String>`.
- Produces on `Settings`: `pub mcp_enabled: bool`, `pub mcp_port: u16`.

**The decision this task needs first, and why:** `backend.rs` says app secrets are "never returned to the UI", and no method reads one back — the app passes the *name* of a secret into a plugin run and `core` resolves it. The endpoint's token is different in kind: a person has to read it and type it into an agent. The plan therefore adds exactly these two methods, documented as the one exception; nothing else on the trait gains a read.

- [ ] **Step 1: Failing test** in `crates/core/src/mock.rs`'s tests: `mcp_token` answers the same value twice, `rotate_mcp_token` answers a different one, and neither is empty or shorter than 32 characters.
- [ ] **Step 2: Implement on the mock** (a `BTreeMap` entry beside the other secrets, the way `set_secret`/`has_secret` already keep theirs; the value comes from `crate::accounts::nonce()`, the workspace's own unique string, documented there as not cryptographic — the test asserts only that it is stable and that rotating changes it).
- [ ] **Step 3: Implement on the real backend** with the helper's own secret calls, creating the value from two `nonce()` calls (32 hex characters) when it is absent — no new dependency. The name `mcp_token` satisfies `accounts::validate_secret_name` (lowercase letters, digits and `_`, under 32 characters).
- [ ] **Step 4: Failing test** in `crates/app/src/settings.rs`'s tests: settings that do not mention the two fields load with `mcp_enabled == false` and `mcp_port == 8765`, and a saved file round-trips them.
- [ ] **Step 5: Implement the two fields** with `#[serde(default, deserialize_with = "lenient")]`, next to the fields they resemble.
- [ ] **Step 6: Run** `rustup run 1.99.0 cargo test -p colab-playground-core --locked` and `-p colab-playground` — all pass.
- [ ] **Step 7: Commit** `feat: keep the agent endpoint's token and port in the settings`.

### Task 7: The app's executor and the endpoint's life

**Files:**
- Create: `crates/app/src/state/mcp.rs`
- Modify: `crates/app/src/state/mod.rs` (`mod mcp;`, one field, the start/stop calls), `crates/app/src/state/ops.rs` (`Op::Mcp`)

**Interfaces:**
- Consumes: `colab_playground_core::mcp::{Config, Endpoint, view::{Snapshot, Source}}`.
- Produces: `AppState::agents` (the endpoint's slice of state: whether it is on, where it listens, the last error), `AppState::publish_snapshot()`, and `struct AppSnapshot(Arc<Mutex<Snapshot>>)` implementing `Source`.

- [ ] **Step 1: Failing test** in `crates/app/src/state/mcp.rs`'s tests over the mock backend: with `mcp_enabled` false nothing is listening on the configured port; turning it on binds it and `addr()` answers; turning it off frees the port; a snapshot taken from a state with one runtime lists that runtime, the way `tests/ui/sessions.rs` asserts about the card.
- [ ] **Step 2: Implement `AppSnapshot`** — the app's `Source`: a `Mutex<Snapshot>` the state publishes into, so the endpoint's thread never touches `AppState`.
- [ ] **Step 3: Implement `publish_snapshot`** and call it where the state changes: after a runtime listing, a usage round, a plugin-run event, a settings save. One call per place and no second cache: the snapshot is built from the same fields the pages read.
- [ ] **Step 4: Start and stop with the setting** — at startup when `mcp_enabled`, on the save that changes it, and on quit. A bind failure (another instance holds the port) is kept as the endpoint's last error and shown, never panicked on.
- [ ] **Step 5: Failing test** — the busy state appears while the token is being fetched, and a taken port shows the error rather than failing the start.
- [ ] **Step 6: Run** `cargo test --workspace --locked`.
- [ ] **Step 7: Commit** `feat: run the agent endpoint with the settings`.

### Task 8: Settings → Agents

**Files:**
- Modify: `crates/app/src/ui/pages/settings.rs` (a new `SettingsSection`), `crates/app/src/i18n/mod.rs` (the keys) and all five `crates/app/src/i18n/*.rs`

**Interfaces:**
- Consumes: `state.agents`, `state.settings`, `keys::…`, `tr`/`tr_args`.
- Produces: the selector constants `AGENTS_TOGGLE`, `AGENTS_ADDRESS`, `AGENTS_TOKEN_REVEAL`, `AGENTS_TOKEN_ROTATE`, `AGENTS_CONFIG_COPY`.

- [ ] **Step 1: Failing test** in `crates/app/tests/ui/agents.rs` (task 9 registers it): the section is drawn, the address is the one the state reports, the token is not in the window until reveal is pressed, and the copy button puts the client configuration on the clipboard.
- [ ] **Step 2: Add the keys** — `SETTINGS_AGENTS_TITLE`, `_DESCRIPTION`, `_TOGGLE_TITLE`, `_TOGGLE_DESCRIPTION`, `_ADDRESS_TITLE`, `_TOKEN_TITLE`, `_TOKEN_DESCRIPTION`, `_TOKEN_REVEAL`, `_TOKEN_ROTATE`, `_CONFIG_TITLE`, `_CONFIG_COPY`, `_OFF`, `_PORT_TAKEN` — as constants in `i18n/mod.rs`, each with a line in all five catalogs.
- [ ] **Step 3: Build the section** the way the page builds its others — `SettingsSection::new(tr(cx, keys::SETTINGS_AGENTS_TITLE)).description(…).row(SettingsRow::new(…).control(tagged(AGENTS_TOGGLE, …)))` — with the switch following the update row's shape (`Switch::new(AGENTS_TOGGLE, …).on_change(move |on, _, cx| …)`, into a setter that saves and notifies). Unlike `OnByDefault`, which is for a switch that starts on, the endpoint's switch is a plain `bool`: a fresh install is off.
- [ ] **Step 4: The token row** — hidden until reveal, showing the value the backend just answered and holding it in the state; a rotate button that goes through the secret operation the plugins page already uses; and one plain sentence that this endpoint is local and says what an agent can read through it.
- [ ] **Step 5: The copy button** writes the configuration an agent needs:

  ```json
  {"mcpServers":{"colab-playground":{"type":"http","url":"http://127.0.0.1:8765/mcp","headers":{"Authorization":"Bearer <token>"}}}}
  ```

- [ ] **Step 6: Run** `cargo test --workspace --locked` — including the i18n test that every key resolves in every locale.
- [ ] **Step 7: Commit** `feat(app): switch the agent endpoint on in Settings`.

### Task 9: The interface tests of the endpoint's own section

**Files:**
- Create: `crates/app/tests/ui/agents.rs`; modify `crates/app/tests/ui/main.rs` (`mod agents;`)

**Interfaces:**
- Consumes: `support::TestApp` — `mount`, `settle`, `shown`, `click`, `read`.

- [ ] **Step 1: Test — off by default.** A fresh app shows the section with the switch off and nothing listening.
- [ ] **Step 2: Test — switching on binds.** `app.click(AGENTS_TOGGLE)`, `app.settle()`, and the state reports an address.
- [ ] **Step 3: Test — the port is taken.** A test that binds the configured port first sees the section's own message instead of a panic.
- [ ] **Step 4: Test — the token is hidden until asked for**, then shown, then rotated (two different values).
- [ ] **Step 5: Test — the copied configuration** carries the address and the token that are on screen.
- [ ] **Step 6: Run** `cargo test -p colab-playground --locked`.
- [ ] **Step 7: Commit** `test(app): cover the agent endpoint's settings section`.

### Task 10: Docs, and the two places that already mention agents

**Files:**
- Modify: `docs/ROADMAP.md`, `crates/app/src/ui/pages/plugins/run.rs` (the existing `endpoint-mcp` card), `docs/USAGE.md`

**Interfaces:** no code interface; the run view's card gains a line when the endpoint is on.

- [ ] **Step 1: Roadmap entry** — where the endpoint stands, what slice 2 adds, a link to the spec.
- [ ] **Step 2: The run view's card** — when the endpoint is on, one more line saying the app itself can be driven, beside the existing `comfy-mcp` instructions; a UI test asserts the line appears only when the endpoint is on.
- [ ] **Step 3: USAGE** — a short "Point an agent at the app": switch it on, copy the configuration, and what an agent may read.
- [ ] **Step 4: Run** `cargo test --workspace --locked`.
- [ ] **Step 5: Commit** `docs: how an agent reaches the app`.

---

## Self-review

- **Spec coverage.** The transport, the token, the `Origin` rule, the switch, the nine read tools and the refusal rules are tasks 1–5, and are landed and tested over a real socket. The app's half — the token and the settings, the executor and the endpoint's life, the section, its tests — is tasks 6–9. The run view's card and the roadmap are task 10. Slice 2's tools and its confirmation dialog are deliberately absent, as the spec says.
- **Two things the spec assumed and the code did not have.** (1) The app cannot read a secret back, so the token needs the two methods in task 6 and the invariant's wording changes with them — the one decision worth the owner's nod before it is written. (2) A `GET` on `/mcp` answers 405 rather than opening an empty event stream: nothing is sent on its own yet, and a client that does not open that stream loses nothing.
- **Placeholders:** none; every task names its files, its test and the command that proves it.
- **What the convention pass changed in already-written code:** two things, both landed — the session id uses `accounts::nonce()` rather than a duplicated hash, and a tool's error is the app's own `Error` serialized rather than a hand-made string. The constraints above now carry the rules that pass turned up (`lint.rs`'s two checks, the three i18n tests, the no-direct-push rule, "a check is made once").
