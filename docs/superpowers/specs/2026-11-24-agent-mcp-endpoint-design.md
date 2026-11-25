# A local MCP endpoint, so an agent can drive the app

Status: **slices 1 and 2 are built** — the endpoint with its read tools is on `main`, and the
driving half (the write and paid tools, the approval dialog) is on `feat/agent-mcp-driving`.
Slice 3 is not. The plans and the implementation follow this document.

An agent can already drive ComfyUI through a session's port forward (the run view's
"drive it with an agent" card points the official `comfy-mcp` at `localhost:8188`). It cannot
drive **the app**: start a session, deploy and watch a plugin, read what a run produced, change
the settings. This adds one MCP server inside the running app so an agent works with the same
state the window shows.

## Decisions taken with the owner

- **Scope:** full driving, including actions that spend money (starting a session, extending one,
  running a plugin).
- **Entry:** an endpoint inside the running app, not a headless command. The agent drives the
  window that is open; what it does is visible there, and what the user does there is visible to
  the agent.
- **Paid actions:** the app asks, in its own window, and the agent waits for the answer. No answer
  in time is a refusal.

## Non-goals

- **Not the Colab REST-API backend.** That is a separate roadmap item; it replaces the `colab` CLI
  behind the `Backend` trait and has nothing to do with what a client speaks to the app.
- **Not reachable off this machine.** Loopback only, no TLS, no host to configure, no accounts of
  its own beyond the token.
- **The app does not consume MCP servers.** This is the server side only. The plugin contract with
  plugin authors ([PLUGINS.md](PLUGINS.md)) does not change.
- **No new tool for every button.** The surface is what an agent needs to do the app's jobs, not a
  mirror of the interface.

## Shape

### Transport

MCP **Streamable HTTP**, the current revision, at one path: `POST /mcp` for requests,
`GET /mcp` for the server-to-client stream, `DELETE /mcp` to end a session. A session is named by
the `Mcp-Session-Id` response header and repeated by the client; the protocol revision is the
`MCP-Protocol-Version` header, and a request naming one the app does not speak is answered with
the revision the app does speak.

A `POST` answer is JSON, or an event stream when the client's `Accept` asks for one. Only the
`GET` stream carries unsolicited messages; the app sends no notifications in the first two slices,
so a client that does not open it loses nothing.

Everything is written against `std::net` and the `serde` already in the workspace: the endpoint
serves one machine, one path, no TLS. No tokio, no HTTP framework.

### Layering

The protocol, the tool table and the argument checks live in `core` (`crates/core/src/mcp/`), where
the mock backend can test them without a window. The app supplies the **executor**: the thing that
turns an accepted tool call into work.

That split is the whole point of the design. A state-changing tool call goes through the same
`AppState::run_backend(op, …)` path a button press takes, so an agent's deployment shows the same
busy state, lands the same result and cannot run alongside a click on the same thing (one call per
`Op` at a time). A read-only tool call reads the state the interface renders, so both sides always
describe the same runtimes.

Calling the backend directly from `core` would be less work and is wrong here: an agent's session
would not exist as far as the window is concerned, and the one-call-per-`Op` rule would not apply
to it.

### Lifecycle

The endpoint is **off by default**. Settings → Agents turns it on, and the app binds `127.0.0.1`
on a port it remembers (8765 unless changed). Turning it off, or quitting, closes it and ends every
session; a tool call in flight fails like a cancelled call does. Changing the port rebinds. A second
app instance cannot bind the same port and says so in Settings; the first keeps serving.

## Security

- **Loopback only.** The listener binds `127.0.0.1`; there is no setting that would expose it to a
  network, and a request whose `Origin` header is not the loopback origin of this app is refused
  (this is what stops a page in a browser from reaching the endpoint, the DNS-rebinding case the
  MCP specification requires).
- **A token.** 32 random bytes, base64url, required as `Authorization: Bearer …` on every request.
  It is kept in the app's secret storage (the system keyring, the same place plugin secrets go),
  never logged, shown in full only after the user asks to see it, and replaceable with one button.
- **The window still gates what spends money** ([DESIGN.md](DESIGN.md#hard-safety-rules) applies to
  every caller): a tool that would start, extend or run something asks in the app and waits.
- **What it does not protect against:** any process on this machine that can read the keyring, and
  a compromised app process. Loopback and a token are not a sandbox; the endpoint is exactly as
  trusted as the user's own account.

## The tools

Names are `snake_case`, no prefixes. Every tool answers with a JSON object (never a bare string),
so an agent can rely on the shape. "Slice" is when it is built.

| Tool | Kind | What it does | Slice |
| --- | --- | --- | --- |
| `sessions_list` | read | The runtimes of every account: name, hardware, state, auto-stop, hourly rate | 1 |
| `session_usage` | read | Each account's compute units spent and left, and the burn rate | 1 |
| `runs_list` | read | The plugin runs the app knows about: plugin, session, stage | 1 |
| `run_status` | read | One run: stage, the tail of its log, whether its server is ready | 1 |
| `run_outputs` | read | What a run produced: the workflow's nodes and links, and the VM paths of its files | 1 |
| `plugin_list` | read | Installed plugins: id, name, enabled, bundled | 1 |
| `settings_get` | read | Language, appearance, hardware and auto-stop defaults, host settings | 1 |
| `accounts_list` | read | Signed-in accounts and which one is active | 1 |
| `forwards_list` | read | Open forwards: local port, remote port, session | 1 |
| `run_download` | write | Copies a run's files here; answers with the local paths | 2 |
| `settings_set` | write | The fields `settings_get` returns, checked the way the form checks them | 2 |
| `accounts_switch` | write | Makes another account the active one | 2 |
| `plugin_enable` | write | Enables or disables a plugin | 2 |
| `plugin_install` | write | Installs a plugin from a folder or a Git URL | 2 |
| `forward_open` / `forward_close` | write | Opens or closes a forward through a session | 2 |
| `session_start` | **paid** | Provisions a runtime: hardware, name, auto-stop | 2 |
| `session_stop` | **paid** | Stops a runtime (ends its billing) | 2 |
| `session_extend` | **paid** | Moves or cancels a runtime's auto-stop | 2 |
| `plugin_run` | **paid** | Deploys and starts a plugin on a runtime, with its manifest's inputs | 2 |
| `run_watch` | read | Waits for a run's stage to change, or a timeout, and answers with the new state | 3 |

`session_start` reports the hourly rate of the hardware it was asked for, so the card and the
agent both say the same number.

## Approval

A paid tool call:

1. The app opens a dialog — the same one the interface uses for stopping a runtime, naming the
   action, its target, the hourly rate where there is one, and the client that asked (the MCP
   client's own name, as it introduced itself).
2. The HTTP response is held open while it waits, two minutes by default. **Allowed**, and the call
   runs and answers normally. **Refused**, **dismissed** or **timed out**, and the call fails with
   an error saying which, marked as a refusal — that answer is final; the agent must not try the
   same call again.
3. Either way the app records it: the action, the client, and the answer, in the session's activity
   log where a runtime is involved, and in the run log where a run is.

A second paid call while one is waiting is refused at once (the interface shows one dialog at a
time). Closing the app while one waits is a refusal.

## Errors and concurrency

- Errors come back as tool results with `isError`, carrying the text the interface would show —
  `core`'s error and its "what to do next", never a stack trace or an internal path.
- Missing or wrong token is HTTP 401; an unknown tool is JSON-RPC `-32601`, bad arguments
  `-32602`, and a protocol the app cannot speak is answered with the one it can.
- Calls obey the interface's own rule: one call per `Op` at a time, a second one answered as busy
  rather than queued, keyed the same way (`key_of`) so a call about one runtime never blocks a call
  about another.
- A tool the app cannot serve right now — no backend, nothing signed in — answers with that error
  the way a button does, instead of failing the session.

## What the window shows

- **Settings → Agents:** the on/off switch, the address, the token (hidden until asked for, one
  button to replace it), and a button that copies a client configuration — the exact JSON an agent
  needs, with the address and token filled in. The same card explains that the endpoint is local
  and what an agent can do through it.
- The run view's existing "drive it with an agent" card (the `comfy-mcp` instructions) gains a line
  pointing at this endpoint when it is on, so the two ways to drive a session read together.
- A tool call from an agent appears as the same busy state a click makes, and names the client in
  the activity line, so the window never looks idle while an agent is working.

## Testing

- **`core`** (mock backend, no window): the JSON-RPC shapes (`initialize`, `tools/list`,
  `tools/call`), the session header, the protocol-revision answer, a bad token, a foreign `Origin`,
  unknown tools and bad arguments, and each read tool's fields against the mock's data — including
  that no tool ever returns the token.
- **`core`**: the tool table is complete — every `Op` that a tool claims to use exists, every paid
  tool says what it costs, and no tool reaches a `Backend` method no interface path reaches.
- **the app**: a tool call goes through the same `Op` path a button does (the busy state appears and
  the result lands), the dialog's allowed / refused / timed-out paths, and that the endpoint is not
  bound while the switch is off.
- **How the dialog is tested** (checked against the harness, not assumed): `settle` is
  `run_until_parked`, so it does not wait for a task that is parked — and a held HTTP response parks
  instead of blocking the interface. The approval dialog is therefore driven exactly as the stop and
  quit dialogs are: run to the parked state, `shown("mcp-approval …")`, `click(...)`, and the parked
  call resumes. The transport itself is tested in `core` over a real socket, where there is no
  window to park.

## Slices

1. **An endpoint and its read tools.** The protocol, the token, the switch, the nine read tools. No
   action can spend money, so it can be turned on for real from the first day.
2. **Driving.** The write and paid tools, the approval dialog, and the Settings card with the
   copyable client configuration.
3. **Watching.** `run_watch` over the `GET` stream, and reattaching to a run after the app restarts
   (the same work the roadmap asks for, done once).

## Risks and open questions

- **Client compatibility is untested until a client connects.** The first slice is verified against
  at least one real client (a hand-written request first, then an agent) before slice 2 starts; the
  protocol revision and the `Accept` handling are the parts most likely to need adjusting.
- **Tool names and descriptions are the agent's whole manual.** They are written for a model that
  has never seen this app, and are worth a second pass once one has used them.
- **Where this document and the plans live** (`docs/superpowers/…`) is the workflow's default, not
  this repository's habit: its own docs are [DESIGN.md](DESIGN.md), [ROADMAP.md](ROADMAP.md) and
  [DEVELOPMENT.md](DEVELOPMENT.md). Moving them, or folding what survives into those, is a decision
  worth making once the feature exists.
