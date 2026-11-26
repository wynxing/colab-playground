//! An agent's call, from the endpoint's thread to the window, and what it answers with.
//!
//! The endpoint serves on its own threads and never reaches into `AppState` (see `mcp`), so the
//! app's half of a tool call is a hand-off. [`Bridge`] is the executor the endpoint is started
//! with: it queues each call and waits, on the endpoint's own thread, for the window's answer. A
//! foreground task takes each call off that queue and starts the work through the same method a
//! click calls, so the busy state, the one-call-per-`Op` rule and the result all belong to the
//! window. Nothing here reaches a `Backend` directly: a second path to the same work would be a
//! second truth, which is the whole reason the snapshot exists.
//!
//! The response is held open while that happens, which is what makes an answer mean "it is done"
//! rather than "it was accepted". A call that would open a runtime or move its auto-stop waits for
//! the user's word in the app's own dialog first, while that question is switched on, and a
//! refusal is final: a denial says which kind it was, and it carries no next step because asking
//! again is wrong.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use colab_playground_core::{
    Error, ErrorKind, Result,
    accounts::AccountId,
    backend::{PluginRunSpec, RunTarget, RuntimeRef, accelerator_prefix},
    error::Denial,
    mcp::actions::{Action, Actions},
    plugins::{InputKind, InputValue, PluginInfo, PluginPreview},
    provision::ProvisionSpec,
    sessions::Accelerator,
    tunnel::{TunnelId, TunnelSpec},
};
use ely_gpui_component::{feedback::Toast, primitives::Severity};
use gpui::{Context, Task};
use serde_json::{Map, Value, json};

use super::{AppState, Op, Overlay, PluginView, RunForm, ops::key_of, tunnels::forward_key};

/// How long the endpoint holds an agent's HTTP response while it waits for the window. A backstop,
/// not the app's own wait: the approval's two minutes and the work itself are where a call really
/// ends. Reaching this means the window is gone, and saying so beats hanging the client.
///
/// An hour, because a call that reports finished work can be waiting on something long: a runtime
/// that is slow to come up, a checkpoint, a render. A client's own timeout is usually shorter, so a
/// long piece of work is better handed back as a handle and watched (see `run_watch`) than waited
/// on in one call.
const CALL_WAIT: Duration = Duration::from_secs(3_600);

/// How long a paid call waits for the user's word before it lapses, as the specification says.
pub const APPROVAL_WAIT: Duration = Duration::from_secs(120);

/// How long a `run_watch` waits before answering that nothing has happened yet. Short enough that
/// the answer always lands inside a client's own timeout, which is the whole point of the tool.
const WATCH_WAIT: Duration = Duration::from_secs(120);

/// How often the window looks for calls an agent has made. Short enough that a call feels immediate
/// and long enough that an idle endpoint costs a wake-up per this and no more.
const CALL_TICK: Duration = Duration::from_millis(20);

/// One call from an agent, on its way to the window.
pub struct Call {
    action: Action,
    /// The MCP client's own name, as it introduced itself.
    client: String,
    reply: mpsc::Sender<Result<Value>>,
}

/// An agent's `run_watch`: waiting for a run to change. Nothing is being done, so no `Op` belongs
/// to it — the answer comes from the window's own events, or from the wait's own deadline.
pub(super) struct Watch {
    /// Which wait this is, so a deadline an answer overtook does nothing.
    asked: u64,
    plugin: String,
    call: Call,
}

impl Call {
    /// The client that asked.
    pub fn client(&self) -> &str {
        &self.client
    }

    /// Answers the agent. The endpoint's thread is waiting for exactly one of these; dropping the
    /// call instead is what a window that went away does.
    pub fn answer(self, answer: Result<Value>) {
        // The line that closes a call: what was asked for, by whom, and whether it happened.
        match &answer {
            Ok(_) => log::info!("agent: {} answered for {}", self.action.name(), self.client),
            Err(error) => log::warn!(
                "agent: {} refused for {}: {error}",
                self.action.name(),
                self.client
            ),
        }
        let _ = self.reply.send(answer);
    }
}

/// The executor the endpoint is started with: a queue, and a wait for the window's answer.
///
/// The queue is a plain [`VecDeque`] the window looks into rather than a channel that wakes it,
/// and deliberately: the endpoint serves on its own threads, and waking the window's executor from
/// one of them would make the whole app non-deterministic — the window's own threads are the only
/// place a task may be scheduled. So a call is left in the queue, and the window comes for it.
pub struct Bridge {
    queued: Mutex<VecDeque<Call>>,
    /// False once the window stops listening: a later call is refused at once rather than waiting
    /// out the backstop for an answer nobody will give.
    listening: AtomicBool,
}

impl Bridge {
    /// A queue nothing is draining yet. The window starts reading it when the endpoint starts.
    pub fn new() -> Arc<Bridge> {
        Arc::new(Bridge {
            queued: Mutex::new(VecDeque::new()),
            listening: AtomicBool::new(true),
        })
    }

    /// The calls that arrived since the window last looked, oldest first.
    pub(crate) fn take(&self) -> Vec<Call> {
        let mut held = self
            .queued
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        held.drain(..).collect()
    }

    /// Stops listening. A call still in the queue is dropped, which answers it as dismissed — the
    /// window went away, so it is not a thing to keep waiting for.
    pub fn close(&self) {
        self.listening.store(false, Ordering::SeqCst);
        self.take();
    }
}

impl Actions for Bridge {
    fn perform(&self, action: Action, client: &str) -> Result<Value> {
        let name = action.name();
        let (reply, answer) = mpsc::channel();
        if !self.listening.load(Ordering::SeqCst) {
            return Err(Error::denied(name, Denial::Dismissed)
                .detail("the app is not listening for agent calls"));
        }
        self.queued
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(Call {
                action,
                client: client.to_owned(),
                reply,
            });
        match answer.recv_timeout(CALL_WAIT) {
            Ok(answer) => answer,
            // Dropped without an answer: the window went away, or stopped listening.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(Error::denied(name, Denial::Dismissed).detail("the window went away"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(Error::denied(name, Denial::TimedOut).detail("the window did not answer"))
            }
        }
    }
}

/// Takes calls off the queue on the foreground until the window goes away. A poll rather than a
/// wait: the queue cannot wake the window (see [`Bridge`]), and an agent's call can afford the
/// tick — the work it asks for takes far longer than this.
pub(crate) fn start_drain(bridge: Arc<Bridge>, cx: &mut Context<AppState>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(CALL_TICK).await;
            for call in bridge.take() {
                if this
                    .update(cx, |state, cx| state.begin_agent_call(call, cx))
                    .is_err()
                {
                    return;
                }
            }
        }
    })
}

/// What a paid call is asking the user about, in the window's own words.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalPrompt {
    /// The client's own name, as it introduced itself.
    pub client: String,
    /// The tool that asked (`session_start`, …).
    pub action: String,
    /// What it is about: a runtime, hardware, a plugin.
    pub target: String,
    /// What the hardware would burn per hour, where the app knows it.
    pub units_per_hour: Option<f64>,
    /// Something that went wrong answering it.
    pub error: Option<Error>,
}

/// A call that takes more than one `Op` — a plugin's template before its run, a plugin's check
/// before its install — carried across the stages, since `agent_calls` answers on the one `Op` a
/// call is filed under.
pub(super) enum Chain {
    Run {
        plugin: String,
        session: Option<String>,
        inputs: Map<String, Value>,
        call: Call,
    },
    Install {
        call: Call,
    },
    /// `session_start` with no name: the app names the session first, the way its own form does,
    /// and provisions second.
    Start {
        spec: ProvisionSpec,
        call: Call,
    },
}

impl AppState {
    /// A call arrived from an agent. A target the app does not have is answered here, before
    /// anything else: there is no work to allow and none to wait for. A call that would spend
    /// more asks the user first, while that question is switched on; everything else starts now.
    pub(crate) fn begin_agent_call(&mut self, call: Call, cx: &mut Context<Self>) {
        log::info!(
            "agent: {} asked for by {}",
            call.action.name(),
            call.client()
        );
        // A watch does no work and costs nothing: nobody is asked to allow it, no `Op` is filed,
        // and the window's own events answer it.
        if let Action::RunWatch { plugin } = &call.action {
            let plugin = plugin.clone();
            self.begin_agent_watch(call, plugin, cx);
            return;
        }
        if let Err(error) = self.resolvable_op(&call.action) {
            call.answer(Err(error));
            return;
        }
        if call.action.needs_approval() && self.agents_confirm() {
            self.ask_to_allow(call, cx);
        } else {
            self.run_agent_call(call, cx);
        }
    }

    /// The `Op` an action starts, or nothing for the three that carry stages of their own (a
    /// plugin's template, a plugin's check, a settings write). A target the app does not have is
    /// an error here — which is what keeps the window from asking the user about a runtime that
    /// could not be stopped anyway.
    fn resolvable_op(&self, action: &Action) -> Result<Option<Op>> {
        match action {
            Action::SettingsSet { .. }
            | Action::PluginInstall { .. }
            | Action::PluginRun { .. }
            | Action::RunWatch { .. } => Ok(None),
            other => self.agent_op(other).map(Some),
        }
    }

    /// Files an agent's watch: one at a time, and only for a plugin that has a run.
    fn begin_agent_watch(&mut self, call: Call, plugin: String, cx: &mut Context<Self>) {
        if self.plugin_run().is_none_or(|run| run.plugin != plugin) {
            call.answer(Err(
                Error::new(ErrorKind::PluginNotFound).with("plugin", plugin)
            ));
            return;
        }
        if self.agent_watch.is_some() {
            call.answer(Err(Error::denied("run_watch", Denial::Busy)));
            return;
        }
        self.watch_asks += 1;
        let asked = self.watch_asks;
        log::info!("agent: run_watch on {plugin} is waiting");
        self.agent_watch = Some(Watch {
            asked,
            plugin,
            call,
        });
        self.watch_timer(asked, cx);
    }

    /// Sets the wait's own deadline, so its answer never depends on something happening.
    fn watch_timer(&mut self, asked: u64, cx: &mut Context<Self>) {
        let previous = self.watch_timer.take();
        self.watch_timer = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            cx.background_executor().timer(WATCH_WAIT).await;
            this.update(cx, |state, cx| state.lapse_agent_watch(asked, cx))
                .ok();
        }));
    }

    fn lapse_agent_watch(&mut self, asked: u64, cx: &mut Context<Self>) {
        if self
            .agent_watch
            .as_ref()
            .is_some_and(|watch| watch.asked == asked)
        {
            self.answer_agent_watch("none", cx);
        }
    }

    /// Answers the waiting watch — on a change, or on its deadline — with the run as it stands.
    pub(super) fn answer_agent_watch(&mut self, changed: &str, cx: &mut Context<Self>) {
        let Some(watch) = self.agent_watch.take() else {
            return;
        };
        let answer = super::mcp::watch_answer(self, &watch.plugin, changed);
        if let Err(error) = &answer {
            log::warn!(
                "agent: run_watch on {} could not be answered: {error}",
                watch.plugin
            );
        } else {
            log::info!("agent: run_watch on {} answered ({changed})", watch.plugin);
        }
        watch.call.answer(answer);
        cx.notify();
    }

    /// Whether an agent is waiting on a `run_watch` right now.
    pub fn agent_is_watching(&self) -> bool {
        self.agent_watch.is_some()
    }

    /// What a paid call is asking the user about, while it waits.
    pub fn agent_approval(&self) -> Option<&ApprovalPrompt> {
        match &self.overlay {
            Some(Overlay::Approval(prompt)) => Some(prompt),
            _ => None,
        }
    }

    /// Puts a paid call to the user, naming the action, its target, what it costs and who asked.
    fn ask_to_allow(&mut self, call: Call, cx: &mut Context<Self>) {
        let name = call.action.name();
        if self.agent_approval.is_some() {
            // The window asks about one thing at a time, and the first call still has its answer
            // coming: this one is told so rather than queued behind it.
            call.answer(Err(Error::denied(name, Denial::Busy)));
            return;
        }
        let prompt = ApprovalPrompt {
            client: call.client().to_owned(),
            action: name.to_owned(),
            target: call.action.target(),
            units_per_hour: self.rate_for(&call.action),
            error: None,
        };
        log::info!(
            "agent: asking about {} on {} for {}",
            prompt.action,
            prompt.target,
            prompt.client
        );
        self.agent_approvals += 1;
        let asked = self.agent_approvals;
        self.agent_approval = Some((asked, call));
        self.overlay = Some(Overlay::Approval(prompt));
        self.watch_approval_deadline(asked, cx);
        cx.notify();
    }

    /// Lets the call lapse once the wait is over, so a window nobody is looking at does not hold
    /// an agent open forever. A deadline overtaken by an answer does nothing.
    fn watch_approval_deadline(&mut self, asked: u64, cx: &mut Context<Self>) {
        let previous = self.agent_deadline.take();
        self.agent_deadline = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            cx.background_executor().timer(APPROVAL_WAIT).await;
            this.update(cx, |state, cx| state.lapse_agent_approval(asked, cx))
                .ok();
        }));
    }

    fn lapse_agent_approval(&mut self, asked: u64, cx: &mut Context<Self>) {
        match self.agent_approval.take() {
            Some((mine, call)) if mine == asked => {
                if matches!(self.overlay, Some(Overlay::Approval(_))) {
                    self.overlay = None;
                }
                let name = call.action.name();
                call.answer(Err(Error::denied(name, Denial::TimedOut)));
                // The user looked away and the agent was told no: the window says so, since
                // nothing else on screen would have changed.
                let toast = Toast::new(crate::i18n::tr(cx, crate::i18n::keys::APPROVAL_CLOSED))
                    .severity(Severity::Warning);
                self.toast(toast, cx);
                cx.notify();
            }
            // A later approval than the one this deadline was set for: put it back.
            Some(other) => self.agent_approval = Some(other),
            None => {}
        }
    }

    /// The user allowed the waiting call: it runs now, and the agent's answer follows it.
    pub(crate) fn allow_agent_call(&mut self, cx: &mut Context<Self>) {
        let Some(call) = self.take_agent_approval() else {
            return;
        };
        if matches!(self.overlay, Some(Overlay::Approval(_))) {
            self.overlay = None;
        }
        log::info!("agent: {} allowed", call.action.name());
        self.run_agent_call(call, cx);
    }

    /// The user said no, or closed the dialog: the agent is told which it was and nothing starts.
    pub(crate) fn refuse_agent_call(&mut self, reason: Denial, cx: &mut Context<Self>) {
        if let Some(call) = self.take_agent_approval() {
            let name = call.action.name();
            log::warn!("agent: {name} refused ({reason:?})");
            call.answer(Err(Error::denied(name, reason)));
        }
        if matches!(self.overlay, Some(Overlay::Approval(_))) {
            self.overlay = None;
        }
        cx.notify();
    }

    /// Takes the waiting call, if any, and forgets its deadline.
    fn take_agent_approval(&mut self) -> Option<Call> {
        self.agent_deadline = None;
        self.agent_approval.take().map(|(_, call)| call)
    }

    /// Starts the work an action asks for, on the foreground, and files the call so that its
    /// answer is what the window shows once that work has landed.
    fn run_agent_call(&mut self, call: Call, cx: &mut Context<Self>) {
        let action = call.action.clone();
        let name = action.name();
        // Three actions do not fit the one-`Op` shape: a settings change is a file write with
        // nothing to wait for, and the other two go through a stage before the work exists.
        match &action {
            Action::SettingsSet { changes } => {
                let answer = self.set_settings_for_agent(changes.clone(), cx);
                call.answer(answer);
                return;
            }
            Action::PluginInstall { source } => {
                let source = source.clone();
                self.install_for_agent(call, source, cx);
                return;
            }
            Action::PluginRun {
                plugin,
                session,
                inputs,
            } => {
                let (plugin, session, inputs) = (plugin.clone(), session.clone(), inputs.clone());
                self.run_plugin_for_agent(call, plugin, session, inputs, cx);
                return;
            }
            // A session the agent did not name is named by the app, the way its own new-session
            // form names one: that is a call of its own before any provisioning starts, so it
            // cannot be the single `Op` the rest of this method files.
            Action::SessionStart {
                name: None,
                hardware,
                auto_stop_minutes,
            } => {
                let (hardware, minutes) = (hardware.clone(), *auto_stop_minutes);
                self.start_session_for_agent(call, hardware, minutes, cx);
                return;
            }
            _ => {}
        }
        let op = match self.agent_op(&action) {
            Ok(op) => op,
            Err(error) => {
                call.answer(Err(error));
                return;
            }
        };
        if self.ops.is_running(op) {
            call.answer(Err(Error::denied(name, Denial::Busy)));
            return;
        }
        // Filed before the work starts, so a call that lands while it starts (a backend that
        // cannot be reached answers at once) finds the call it belongs to.
        self.agent_calls.insert(op, call);
        if let Err(error) = self.perform_for_agent(&action, cx) {
            if let Some(call) = self.agent_calls.remove(&op) {
                call.answer(Err(error));
            }
            return;
        }
        // Nothing started after all — the app's own guards turned it down and nothing said so —
        // and the agent is told rather than left waiting for a call that will never come.
        if !self.ops.is_running(op)
            && let Some(call) = self.agent_calls.remove(&op)
        {
            call.answer(Err(Error::new(ErrorKind::Busy)
                .with("action", name)
                .detail(format!("{name} did not start"))));
        }
        cx.notify();
    }

    /// `session_start` with no name: the app asks the backend for a name the way its own form
    /// does, then provisions with it. Two stages, so the call is carried across them and answered
    /// once the runtime is there.
    fn start_session_for_agent(
        &mut self,
        call: Call,
        hardware: Option<String>,
        auto_stop_minutes: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let spec = match self.provision_spec_for_agent(&None, &hardware, auto_stop_minutes) {
            Ok(spec) => spec,
            Err(error) => {
                call.answer(Err(error));
                return;
            }
        };
        if self.ops.is_running(Op::Provision) || self.ops.is_running(Op::SessionName) {
            call.answer(Err(Error::denied("session_start", Denial::Busy)));
            return;
        }
        let prefix = accelerator_prefix(spec.accelerator);
        self.agent_chain = Some(Chain::Start { spec, call });
        self.run_backend(
            Op::SessionName,
            cx,
            move |backend, _| Ok(backend.new_session_name(Some(&prefix))),
            |state, result, cx| state.provision_named_for_agent(result, cx),
        );
    }

    /// The naming call came back: check the spec and provision, with the agent's call filed so
    /// that its answer is what the window shows once the runtime is there.
    fn provision_named_for_agent(&mut self, result: Result<String>, cx: &mut Context<Self>) {
        let Some(Chain::Start { mut spec, call }) = self.agent_chain.take() else {
            return;
        };
        match result {
            Ok(name) => spec.name = name,
            Err(error) => {
                call.answer(Err(error));
                return;
            }
        }
        if let Err(error) = spec.validate() {
            call.answer(Err(error));
            return;
        }
        self.agent_calls.insert(Op::Provision, call);
        self.provision_for_agent(spec, cx);
        if !self.ops.is_running(Op::Provision)
            && let Some(call) = self.agent_calls.remove(&Op::Provision)
        {
            call.answer(Err(Error::new(ErrorKind::Busy)
                .with("action", "session_start")
                .detail("session_start did not start")));
        }
        cx.notify();
    }

    /// Answers an agent's waiting `plugin_run` at the first stage the plugin publishes. A plugin
    /// that keeps a server running — ComfyUI does — never settles on its own, so an answer held
    /// until the end would never come: the client times out while the run is working perfectly.
    /// The tool's description promises a stage, and reading `run_status` is what it points to for
    /// the rest.
    pub(super) fn answer_agent_run_stage(&mut self, cx: &mut Context<Self>) {
        let Some(action) = self
            .agent_calls
            .get(&Op::PluginRun)
            .map(|call| call.action.clone())
        else {
            return;
        };
        let answer = super::mcp::answer_for_action(&action, self);
        if let Some(call) = self.agent_calls.remove(&Op::PluginRun) {
            call.answer(answer);
        }
        cx.notify();
    }

    /// The `Op` an action starts — the very one the button for it starts, so the busy state and
    /// the one-call-per-`Op` rule are the window's own. A target the app does not have is refused
    /// here, before anything is filed.
    fn agent_op(&self, action: &Action) -> Result<Op> {
        Ok(match action {
            Action::SessionStart { .. } => Op::Provision,
            Action::SessionStop { session } => Op::Stop(key_of(&self.runtime_named(session)?)),
            // Zero minutes cancels the auto-stop, which is a call of its own.
            Action::SessionExtend { session, minutes } => {
                let target = self.runtime_named(session)?;
                if *minutes == 0 {
                    Op::CancelAutoStop(key_of(&target))
                } else {
                    Op::Extend(key_of(&target))
                }
            }
            Action::RunDownload { plugin } => {
                let path = self.download_path(plugin)?;
                Op::DownloadOutputs(key_of(&path))
            }
            Action::AccountsSwitch { .. } => Op::Switch,
            Action::PluginEnable { plugin, .. } => Op::PluginChange(key_of(plugin)),
            Action::ForwardOpen {
                session,
                remote_port,
                ..
            } => {
                let target = self.runtime_named(session)?;
                Op::Forward(forward_key(&target.account, session, *remote_port))
            }
            Action::ForwardClose { local_port, .. } => {
                let id = self.tunnel_id(*local_port)?;
                Op::CloseForward(id.0)
            }
            Action::SettingsSet { .. }
            | Action::PluginInstall { .. }
            | Action::PluginRun { .. }
            | Action::RunWatch { .. } => {
                unreachable!("answered from the window's own events")
            }
        })
    }

    /// Carries out one action through the window's own methods. The `Op` it starts is the one
    /// `agent_op` named.
    fn perform_for_agent(&mut self, action: &Action, cx: &mut Context<Self>) -> Result<()> {
        match action {
            Action::SessionStart {
                name,
                hardware,
                auto_stop_minutes,
            } => {
                let spec = self.provision_spec_for_agent(name, hardware, *auto_stop_minutes)?;
                spec.validate()?;
                self.provision_for_agent(spec, cx);
            }
            Action::SessionStop { session } => {
                let target = self.runtime_named(session)?;
                self.stop_runtime(target, cx);
            }
            Action::SessionExtend { session, minutes } => {
                let target = self.runtime_named(session)?;
                if *minutes == 0 {
                    self.cancel_auto_stop(target, cx);
                } else {
                    self.extend_runtime(target, Duration::from_secs(minutes * 60), cx);
                }
            }
            Action::RunDownload { plugin } => {
                let path = self.download_path(plugin)?;
                self.save_run_outputs(&path, cx);
            }
            Action::AccountsSwitch { account } => {
                let to = self.account_named(account)?;
                // The path a click takes: the switch is asked for, then confirmed — the same two
                // steps, in one turn of the foreground, so no dialog is left standing.
                self.request_switch(to, cx);
                self.confirm_switch(cx);
                if !self.ops.is_running(Op::Switch) {
                    return Err(Error::new(ErrorKind::AccountNotFound).with("account", account));
                }
            }
            Action::PluginEnable { plugin, enabled } => {
                let (plugin, enabled) = (plugin.clone(), *enabled);
                self.set_plugin_enabled(plugin, enabled, cx);
            }
            Action::ForwardOpen {
                session,
                remote_port,
                local_port,
            } => {
                let target = self.runtime_named(session)?;
                let spec = TunnelSpec {
                    account: target.account.clone(),
                    session: session.clone(),
                    local_port: local_port.unwrap_or_else(free_local_port),
                    remote_port: *remote_port,
                    health_path: None,
                };
                spec.validate()?;
                self.open_tunnel(spec, false, cx);
            }
            Action::ForwardClose { local_port, .. } => {
                let id = self.tunnel_id(*local_port)?;
                self.close_forward(id, cx);
            }
            Action::SettingsSet { .. }
            | Action::PluginInstall { .. }
            | Action::PluginRun { .. }
            | Action::RunWatch { .. } => {
                unreachable!("answered from the window's own events")
            }
        }
        Ok(())
    }

    /// The `Op` an agent's call started has landed: the call is answered with the state the window
    /// now shows, or with what went wrong. Called wherever a call completes.
    pub(crate) fn settle_agent_call(
        &mut self,
        op: Op,
        failure: Option<Error>,
        cx: &mut Context<Self>,
    ) {
        let Some(call) = self.agent_calls.remove(&op) else {
            return;
        };
        let action = call.action.clone();
        log::info!("agents: answering {} for {}", action.name(), call.client());
        let answer = match failure {
            Some(error) => Err(error),
            None => super::mcp::answer_for_action(&action, self),
        };
        call.answer(answer);
        // What the call changed is what an agent reads next.
        self.publish_snapshot();
        cx.notify();
    }

    /// `plugin_run`: the plugin's template first, then the run itself — the stages the window's own
    /// form goes through, so the agent's inputs land in the same form the button fills.
    fn run_plugin_for_agent(
        &mut self,
        call: Call,
        plugin: String,
        session: Option<String>,
        inputs: Map<String, Value>,
        cx: &mut Context<Self>,
    ) {
        let busy = self.agent_chain.is_some()
            || self.ops.is_running(Op::PluginRun)
            || self.ops.is_running(Op::RunTemplate);
        if busy {
            call.answer(Err(Error::denied("plugin_run", Denial::Busy)));
            return;
        }
        if self.plugin(&plugin).is_none() {
            call.answer(Err(
                Error::new(ErrorKind::PluginNotFound).with("plugin", &plugin)
            ));
            return;
        }
        if let Some(session) = &session
            && let Err(error) = self.runtime_named(session)
        {
            call.answer(Err(error));
            return;
        }
        self.agent_chain = Some(Chain::Run {
            plugin: plugin.clone(),
            session,
            inputs,
            call,
        });
        self.run_backend(
            Op::RunTemplate,
            cx,
            move |backend, _| backend.plugin_run_template(&plugin),
            |state, result, cx| state.template_for_agent(result, cx),
        );
    }

    fn template_for_agent(&mut self, result: Result<PluginRunSpec>, cx: &mut Context<Self>) {
        let Some(Chain::Run {
            plugin,
            session,
            inputs,
            call,
        }) = self.agent_chain.take()
        else {
            return;
        };
        let template = match result {
            Ok(template) => template,
            Err(error) => {
                call.answer(Err(error));
                return;
            }
        };
        let Some(info) = self.plugin(&plugin).cloned() else {
            call.answer(Err(
                Error::new(ErrorKind::PluginNotFound).with("plugin", &plugin)
            ));
            return;
        };
        let values = match inputs_for_agent(&info, &inputs) {
            Ok(values) => values,
            Err(error) => {
                call.answer(Err(error));
                return;
            }
        };
        let provision = match &template.target {
            RunTarget::Provision(spec) => spec.clone(),
            RunTarget::Existing { .. } => self.defaults().provision_spec(String::new()),
        };
        let session_name = provision.name.clone();
        self.run_form = Some(RunForm {
            plugin: info.manifest.id.clone(),
            values,
            provision,
            existing: session,
            existing_account: None,
            target_lost: None,
            stop_on_failure: template.stop_on_failure,
            hf: super::HfLookup::default(),
            error: None,
        });
        self.plugin_view = PluginView::Form;
        if self.ops.is_running(Op::PluginRun) {
            call.answer(Err(Error::denied("plugin_run", Denial::Busy)));
            cx.notify();
            return;
        }
        if self.start_run(Vec::new(), session_name, cx) {
            self.agent_calls.insert(Op::PluginRun, call);
        } else {
            let error = self
                .run_form
                .as_ref()
                .and_then(|form| form.error.clone())
                .unwrap_or_else(|| {
                    Error::new(ErrorKind::PluginRunFailed).detail("the run did not start")
                });
            call.answer(Err(error));
        }
        cx.notify();
    }

    /// `plugin_install`: the app checks the manifest first, exactly as its own dialog does, and
    /// installs only what that check accepted.
    fn install_for_agent(&mut self, call: Call, source: String, cx: &mut Context<Self>) {
        let source = source.trim().to_owned();
        if source.is_empty() {
            call.answer(Err(Error::invalid(
                "source",
                "a folder or a Git URL is required",
            )));
            return;
        }
        if self.agent_chain.is_some() || self.ops.is_running(Op::InspectPlugin) {
            call.answer(Err(Error::denied("plugin_install", Denial::Busy)));
            return;
        }
        self.agent_chain = Some(Chain::Install { call });
        self.run_backend(
            Op::InspectPlugin,
            cx,
            move |backend, cancel| {
                if super::is_git_source(&source) {
                    backend.inspect_plugin_git(&source, cancel)
                } else {
                    backend.inspect_plugin_folder(&std::path::PathBuf::from(&source))
                }
            },
            |state, result, cx| state.install_checked_for_agent(result, cx),
        );
    }

    fn install_checked_for_agent(&mut self, result: Result<PluginPreview>, cx: &mut Context<Self>) {
        let Some(Chain::Install { call }) = self.agent_chain.take() else {
            return;
        };
        let preview = match result {
            // The manifest's own complaint is the agent's answer: it is what the window would have
            // shown, and that check exists so a plugin the app refuses is never installed.
            Ok(preview) => preview,
            Err(error) => {
                call.answer(Err(error));
                cx.notify();
                return;
            }
        };
        let id = preview.manifest.id.clone();
        let op = Op::PluginChange(key_of(&id));
        if self.ops.is_running(op) {
            call.answer(Err(Error::denied("plugin_install", Denial::Busy)));
            cx.notify();
            return;
        }
        self.agent_calls.insert(op, call);
        self.run_backend(
            op,
            cx,
            move |backend, _| backend.install_plugin(&preview),
            |state, result, cx| {
                if let Ok(info) = &result {
                    let name = state.plugin_name(info);
                    let toast = crate::i18n::tr_args(
                        cx,
                        crate::i18n::keys::PLUGIN_INSTALLED,
                        &[("name", &name)],
                    );
                    state.toast_done(toast, cx);
                    state.load_plugins(cx);
                }
            },
        );
        cx.notify();
    }

    /// `settings_set`: every field checked the way the settings page checks it, then saved. The
    /// endpoint's own switch, its port and the question before spending are deliberately not among
    /// them — an agent must not turn that question off, and turning the endpoint off from a tool
    /// call would cut off the answer to that very call.
    fn set_settings_for_agent(
        &mut self,
        changes: Map<String, Value>,
        cx: &mut Context<Self>,
    ) -> Result<Value> {
        if changes.is_empty() {
            return Err(Error::invalid("changes", "no settings were named"));
        }
        for (name, value) in &changes {
            match name.as_str() {
                "language" => {
                    let text = value
                        .as_str()
                        .ok_or_else(|| Error::invalid("language", "must be a string"))?;
                    let pref =
                        crate::settings::LanguagePref::from_value(text).ok_or_else(|| {
                            Error::invalid("language", format!("unknown language {text:?}"))
                        })?;
                    self.set_language(pref, cx);
                }
                "appearance" => {
                    let pref: crate::settings::AppearancePref =
                        serde_json::from_value(value.clone()).map_err(|_| {
                            Error::invalid("appearance", "must be system, light or dark")
                        })?;
                    self.set_appearance(pref, cx);
                }
                "default_hardware" => {
                    let text = value
                        .as_str()
                        .ok_or_else(|| Error::invalid("default_hardware", "must be a string"))?;
                    let accelerator = accelerator_named(text).ok_or_else(|| {
                        Error::invalid("default_hardware", format!("unknown accelerator {text:?}"))
                    })?;
                    self.update_defaults(|defaults| defaults.accelerator = accelerator, cx);
                }
                "default_auto_stop" => {
                    let minutes = auto_stop_minutes(value)?;
                    self.update_defaults(
                        |defaults| {
                            defaults.max_runtime = minutes.map(|m| Duration::from_secs(m * 60))
                        },
                        cx,
                    );
                }
                "wsl_distro" => {
                    let distro = match value {
                        Value::Null => None,
                        Value::String(text) if text.trim().is_empty() => None,
                        Value::String(text) => Some(text.trim().to_owned()),
                        _ => {
                            return Err(Error::invalid(
                                "wsl_distro",
                                "must be a distribution name or null",
                            ));
                        }
                    };
                    self.set_wsl_distro(distro, cx);
                }
                "colab_path" => {
                    let path = match value {
                        Value::Null => None,
                        Value::String(text) if text.trim().is_empty() => None,
                        Value::String(text) => Some(text.trim().to_owned()),
                        _ => return Err(Error::invalid("colab_path", "must be a path or null")),
                    };
                    // The interpreter that goes with it is the user's own setting and stays:
                    // naming `colab_path` moves one field, not the whole host setup.
                    let python = self.settings().colab_python.clone();
                    self.set_toolchain(path, python, cx);
                }
                "demo_data" => {
                    return Err(Error::invalid(
                        "demo_data",
                        "is read-only: it is which backend this copy runs",
                    ));
                }
                other => {
                    return Err(Error::invalid(
                        "changes",
                        format!("{other} is not a setting an agent may change"),
                    ));
                }
            }
        }
        Ok(json!({ "settings": super::mcp::settings_of(self) }))
    }

    /// What an action costs per hour, where the app knows: the runtime's own measured rate. A
    /// session that does not exist yet has none — the published table is what the form shows, and
    /// the dialog says nothing rather than inventing a number.
    fn rate_for(&self, action: &Action) -> Option<f64> {
        let session = match action {
            Action::SessionStop { session } | Action::SessionExtend { session, .. } => session,
            Action::PluginRun {
                session: Some(session),
                ..
            } => session,
            _ => return None,
        };
        self.runtimes()
            .ready()?
            .runtimes
            .iter()
            .find(|runtime| runtime.id.name.as_deref() == Some(session.as_str()))
            .and_then(|runtime| runtime.estimated_rate_cu_per_hour)
    }

    /// The runtime an agent named. A name no listing has is a session that is gone, which is a
    /// thing to say rather than something to retry.
    fn runtime_named(&self, name: &str) -> Result<RuntimeRef> {
        self.runtimes()
            .ready()
            .and_then(|view| {
                view.runtimes
                    .iter()
                    .find(|runtime| runtime.id.name.as_deref() == Some(name))
                    .map(|runtime| runtime.id.clone())
            })
            .ok_or_else(|| Error::new(ErrorKind::SessionGone).with("session", name))
    }

    fn account_named(&self, email: &str) -> Result<AccountId> {
        self.accounts()
            .ready()
            .and_then(|view| {
                view.accounts
                    .iter()
                    .find(|account| account.email.as_deref() == Some(email))
                    .map(|account| account.id.clone())
            })
            .ok_or_else(|| Error::new(ErrorKind::AccountNotFound).with("account", email))
    }

    /// The forward an agent named, by the port it listens on here: the port is what an agent holds
    /// on to, since it is the one it connects to.
    fn tunnel_id(&self, local_port: u16) -> Result<TunnelId> {
        self.tunnels()
            .find(|tunnel| tunnel.spec.local_port == local_port && !tunnel.state.is_terminal())
            .map(|tunnel| tunnel.id)
            .ok_or_else(|| Error::new(ErrorKind::PortInUse).with("local_port", local_port))
    }

    /// The directory on the runtime a run's files are read from: the folder the run published.
    fn download_path(&self, plugin: &str) -> Result<String> {
        self.plugin_run()
            .filter(|run| run.plugin == plugin)
            .ok_or_else(|| Error::new(ErrorKind::PluginNotFound).with("plugin", plugin))?;
        // The path the plugin's own manifest declares as worth keeping — the one the run page's
        // save button copies — not the run's published outputs: what a plugin publishes there is
        // written for people to read (a ComfyUI run publishes its preset, its checkpoint, the GPU
        // and view URLs), so it is not a list of files.
        super::mcp::keep_path(self, plugin).ok_or_else(|| {
            Error::new(ErrorKind::SessionGone)
                .with("plugin", plugin)
                .detail("the plugin declares no files to save")
        })
    }

    /// The provisioning spec an agent's `session_start` asks for: the app's own defaults for
    /// everything it did not name.
    fn provision_spec_for_agent(
        &self,
        name: &Option<String>,
        hardware: &Option<String>,
        auto_stop_minutes: Option<u64>,
    ) -> Result<ProvisionSpec> {
        let mut spec = self
            .defaults()
            .provision_spec(name.clone().unwrap_or_default());
        if let Some(hardware) = hardware {
            spec.accelerator = accelerator_named(hardware).ok_or_else(|| {
                Error::invalid("hardware", format!("unknown accelerator {hardware:?}"))
            })?;
            if !spec.accelerator.is_gpu() {
                spec.min_vram_mib = None;
            }
        }
        if let Some(minutes) = auto_stop_minutes {
            spec.guard.max_runtime = Some(Duration::from_secs(minutes * 60));
        }
        Ok(spec)
    }
}

/// The accelerator an agent named. `settings_get` reports the debug name while the settings file
/// carries the serde one, and they differ for the TPUs (`V5E1` against `TpuV5e1`), so both are
/// accepted.
fn accelerator_named(name: &str) -> Option<Accelerator> {
    let name = name.trim();
    Accelerator::ALL.into_iter().find(|accelerator| {
        format!("{accelerator:?}").eq_ignore_ascii_case(name)
            || serde_json::to_value(accelerator)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .is_some_and(|serde| serde.eq_ignore_ascii_case(name))
    })
}

/// Parses a `default_auto_stop` the way `settings_get` reports it (`"30 min"`, `"none"`). A bare
/// number of minutes and `null` are accepted too, and zero means no limit.
fn auto_stop_minutes(value: &Value) -> Result<Option<u64>> {
    let minutes = match value {
        Value::Null => 0,
        Value::Number(number) => number.as_u64().ok_or_else(|| {
            Error::invalid("default_auto_stop", "must be a number of minutes or none")
        })?,
        Value::String(text) => {
            let text = text.trim().to_ascii_lowercase();
            let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                if ["none", "no", "off", "unlimited"].contains(&text.as_str()) {
                    0
                } else {
                    return Err(Error::invalid(
                        "default_auto_stop",
                        "must be a number of minutes or none",
                    ));
                }
            } else {
                digits.parse().unwrap_or(0)
            }
        }
        _ => {
            return Err(Error::invalid(
                "default_auto_stop",
                "must be a number of minutes or none",
            ));
        }
    };
    Ok((minutes > 0).then_some(minutes))
}

/// A local port nothing is using, starting where the forward dialog starts. The tunnel code
/// refuses a port that is taken, so this only has to be close.
fn free_local_port() -> u16 {
    let start = super::FORWARD_START_PORT;
    (start..=u16::MAX)
        .find(|port| {
            !colab_playground_core::http::local_port_in_use(*port, Duration::from_millis(50))
        })
        .unwrap_or(start)
}

/// The plugin's inputs as the agent gave them, read as the manifest's declared kind says, so a
/// number stays a number and a switch a switch.
fn inputs_for_agent(
    info: &PluginInfo,
    inputs: &Map<String, Value>,
) -> Result<std::collections::BTreeMap<String, InputValue>> {
    let mut values = std::collections::BTreeMap::new();
    for (id, value) in inputs {
        let spec = info.manifest.input(id).ok_or_else(|| {
            Error::invalid(
                "inputs",
                format!("{id} is not an input of {}", info.manifest.id),
            )
        })?;
        let parsed = match spec.kind {
            InputKind::Bool => value
                .as_bool()
                .map(InputValue::Bool)
                .ok_or_else(|| Error::invalid(id, "must be true or false"))?,
            InputKind::Number => value
                .as_f64()
                .map(InputValue::Number)
                .ok_or_else(|| Error::invalid(id, "must be a number"))?,
            InputKind::Text | InputKind::Select | InputKind::Secret | InputKind::HfModel => value
                .as_str()
                .map(|text| InputValue::Text(text.to_owned()))
                .ok_or_else(|| Error::invalid(id, "must be a string"))?,
        };
        values.insert(id.clone(), parsed);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use colab_playground_core::{
        ErrorKind,
        accounts::AccountId,
        error::Denial,
        mcp::actions::{Action, Actions},
        sessions::Accelerator,
    };
    use serde_json::json;

    use super::{Bridge, accelerator_named, auto_stop_minutes, forward_key};

    /// An executor with no window behind it: a call is refused at once rather than waiting out the
    /// backstop for an answer that cannot come.
    #[test]
    fn the_bridge_refuses_when_the_app_is_not_listening() {
        let bridge = Bridge::new();
        bridge.close();
        let started = Instant::now();
        let error = bridge
            .perform(
                Action::SessionStop {
                    session: "run-1".to_owned(),
                },
                "a client",
            )
            .expect_err("nothing is listening");
        assert_eq!(error.kind, ErrorKind::Denied);
        assert_eq!(error.param("action"), Some("session_stop"));
        assert_eq!(error.param("reason"), Some(Denial::Dismissed.as_str()));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// A window that answers: the call comes back with exactly what it was given, and the window
    /// is the one that took it off the queue.
    #[test]
    fn a_call_comes_back_with_the_foreground_answer() {
        let bridge = Bridge::new();
        let window = Arc::clone(&bridge);
        let answered = std::thread::spawn(move || {
            // The window looks for a call the way the foreground drain does, rather than being
            // woken by the endpoint's thread.
            let mut waiting = Vec::new();
            for _ in 0..200 {
                waiting = window.take();
                if !waiting.is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let call = waiting.pop().expect("a call to answer");
            assert_eq!(call.action.name(), "session_extend");
            assert_eq!(call.client(), "an agent");
            call.answer(Ok(json!({ "session": "run-1", "minutes": 30 })));
        });
        let answer = bridge
            .perform(
                Action::SessionExtend {
                    session: "run-1".to_owned(),
                    minutes: 30,
                },
                "an agent",
            )
            .expect("answered");
        assert_eq!(answer["minutes"], 30);
        answered.join().expect("the window finished");
    }

    /// A window that goes away while a call waits answers it at once, rather than making the agent
    /// wait out the backstop.
    #[test]
    fn a_call_waiting_when_the_window_closes_is_answered_at_once() {
        let bridge = Bridge::new();
        let closer = Arc::clone(&bridge);
        std::thread::spawn(move || {
            for _ in 0..200 {
                if !closer.take().is_empty() {
                    closer.close();
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let started = Instant::now();
        let error = bridge
            .perform(
                Action::SessionStop {
                    session: "run-1".to_owned(),
                },
                "an agent",
            )
            .expect_err("the window went away");
        assert_eq!(error.kind, ErrorKind::Denied);
        assert_eq!(error.param("reason"), Some(Denial::Dismissed.as_str()));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_accelerator_is_read_the_way_the_app_writes_it() {
        assert_eq!(accelerator_named("A100"), Some(Accelerator::A100));
        // `settings_get` reports the debug name, the settings file the serde one: both are read.
        assert_eq!(accelerator_named("TpuV5e1"), Some(Accelerator::TpuV5e1));
        assert_eq!(accelerator_named("V5E1"), Some(Accelerator::TpuV5e1));
        assert_eq!(accelerator_named("a100"), Some(Accelerator::A100));
        assert_eq!(accelerator_named("H200"), None);
    }

    #[test]
    fn an_auto_stop_is_read_the_way_the_settings_report_it() {
        assert_eq!(auto_stop_minutes(&json!("30 min")).unwrap(), Some(30));
        assert_eq!(auto_stop_minutes(&json!("none")).unwrap(), None);
        assert_eq!(auto_stop_minutes(&json!(45)).unwrap(), Some(45));
        assert_eq!(auto_stop_minutes(&json!(null)).unwrap(), None);
        assert_eq!(auto_stop_minutes(&json!(0)).unwrap(), None);
        assert!(auto_stop_minutes(&json!("soon")).is_err());
    }

    /// The key ignores the local port, so the `Op` an agent names and the `Op` the dialog opens
    /// are the same call about the same forward.
    #[test]
    fn a_forward_is_keyed_by_where_it_goes_not_by_where_it_lands() {
        let account = AccountId::new("someone").expect("an account id");
        assert_eq!(
            forward_key(&account, "run-1", 8188),
            forward_key(&account, "run-1", 8188)
        );
        assert_ne!(
            forward_key(&account, "run-1", 8188),
            forward_key(&account, "run-2", 8188)
        );
    }
}
