# Plan: `run_watch` (slice 3, first piece)

The design is [2026-11-25-agent-messages-design.md](../specs/2026-11-25-agent-messages-design.md).
Each task names its files, its test and the command that proves it. Commands run as
`rustup run 1.99.0 cargo …` and every commit goes through the three checks in `colab-playground-dev`.

## 1. The vocabulary: an action and a tool row

* `crates/core/src/mcp/actions.rs`: `Action::RunWatch { plugin }`; `name` → `run_watch`; `target` →
  the plugin; `is_paid` → false.
* `crates/core/src/mcp/tools.rs`: the row after `run_status`, `readOnlyHint: true`, description:
  *"Answers when the run changes — a new stage, ready, failed, or the runtime gone — and at the
  latest after about two minutes with what it is doing now. Call it again to keep watching."*
* Tests in `actions.rs`/`tools.rs`: the name, the target, not paid, and the table's own checks
  (read tools must say they change nothing).
* Command: `cargo test -p colab-playground-core mcp`.

## 2. The wait, in the window

* `crates/app/src/state/agents.rs`: `Watch { call, plugin, asked }` on `AppState`, one at a time,
  with a deadline task set the way `watch_approval_deadline` is; `WATCH_WAIT` = two minutes.
* `begin_agent_call`: a `run_watch` does not go through `resolvable_op`/`allow` — it is read, so it
  is filed at once; a second one is answered `busy`; a plugin with no run is answered
  `plugin_not_found`.
* Test: `crates/app/tests/ui/agents.rs`, `a_second_watch_is_told_to_wait_its_turn`.
* Command: `cargo test -p colab-playground --test ui agents::`.

## 3. Answering it when the app changes

* `crates/app/src/state/runs.rs`: `apply_run_event` (a status landed), `run_became_ready`,
  `finish_run`, `stop_run` call `state.answer_agent_watch("stage" | "ready" | "failed" | "gone")`.
* `crates/app/src/state/mcp.rs`: `answer_for_action` builds the watch's answer — `run` as
  `run_status` gives it, plus `changed`.
* Test: `a_watch_is_answered_when_the_run_reaches_its_next_stage` — start a run over the socket,
  watch, settle until the stage moves, read the answer.
* Command: `cargo test -p colab-playground --test ui agents::`.

## 4. Answering it when nothing happens

* The deadline answers `changed: "none"` with the run as it stands, so a client is never left past
  its own timeout.
* Test: `a_watch_that_sees_nothing_answers_with_the_run_as_it_stands` — settle past two minutes with
  the run held at one stage.
* Command: `cargo test -p colab-playground --test ui agents::`.

## 5. The documentation the agent reads

* `docs/USAGE.md`: the long job in four calls (start → watch → watch → download), with the tool's
  own words beside it.
* `docs/ROADMAP.md`: slice 3's first piece done; session watches and notifications still open.
* `docs/superpowers/specs/2026-11-24-agent-mcp-endpoint-design.md`: the tool table gains the row.
* Command: `cargo test -p colab-playground --test lint` (the docs' own checks).

## 6. The live check

* With the app running and its endpoint on: start a runtime, deploy ComfyUI, watch it to `ready` in
  two or three calls, download the image, stop the runtime — and read the log to confirm each step
  drew a line and each call answered inside its own deadline.
