//! Plugin runs: the run form generated from the manifest (pre-filled by
//! `plugin_run_template`), Hugging Face search and file listing, and the run view (stages,
//! progress, log, outputs, endpoints).
//!
//! Secret inputs never enter this state: they are read from their masked fields when the run
//! starts, sent with that one request, and the fields are cleared (a second run asks again).

use std::{collections::VecDeque, sync::Arc};

use colab_playground_core::{
    Error, ErrorKind,
    accounts::AccountId,
    backend::{
        Backend, DiscoveredRun, PluginRunEvent, PluginRunOutcome, PluginRunSpec, RunTarget, Runtime,
    },
    hf::HfModel,
    plugins::{InputKind, InputValue, PluginInfo, ResolvedEndpoint, resolve_endpoints},
    provision::ProvisionSpec,
    quant::GgufListing,
    tunnel::TunnelSpec,
    vm::RunState,
    vm::RunStatus,
};
use gpui::{AppContext, Context};

use super::{AppState, InputSlot, Load, Op, ProvisionProgress, ops::key_of};
use crate::{
    i18n::{keys, tr_args},
    route::Route,
};

pub const HF_RESULTS: u32 = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PluginView {
    #[default]
    List,
    Form,
    Run,
}

/// Hugging Face lookups for an `hf-model` input. Each lookup carries a generation; a result
/// for an older one is dropped, so results always match the latest query.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HfLookup {
    pub(crate) generation: u64,
    pub results: Load<Vec<HfModel>>,
    pub repo: Option<String>,
    pub files: Load<GgufListing>,
}

/// Why the session picked as the run form's target was unpicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetLost {
    /// It is no longer running.
    Ended(String),
    /// The active account changed: it may well be running, and billing, on the other one.
    OtherAccount(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunForm {
    pub plugin: String,
    /// Non-text values (number, bool, select) by input id.
    pub values: std::collections::BTreeMap<String, InputValue>,
    /// The fresh session's options (kept while an existing session is chosen).
    pub provision: ProvisionSpec,
    /// An existing session to run on instead.
    pub existing: Option<String>,
    /// The account of the session picked above, when it was picked from the list.
    pub(crate) existing_account: Option<AccountId>,
    /// The session picked above that can no longer be run on: the run would now start a new
    /// one, and the form says so until a target is picked again.
    pub target_lost: Option<TargetLost>,
    /// Stop a session provisioned for this run when the run fails.
    pub stop_on_failure: bool,
    /// Optional Keep Plan to attach when the run starts.
    pub keep_plan: Option<colab_playground_core::keep::KeepPlan>,
    pub hf: HfLookup,
    pub error: Option<Error>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RunPhase {
    /// Provisioning or running.
    Running,
    Ready(Box<PluginRunOutcome>),
    Failed(Error),
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PluginRun {
    pub plugin: String,
    /// It provisions its own session.
    pub provisions: bool,
    /// Provisioning progress (fresh sessions only).
    pub provision: ProvisionProgress,
    /// Session it runs on, once known.
    pub session: Option<String>,
    pub account: Option<AccountId>,
    /// Run directory id on the VM once launched or reattached.
    pub run_id: Option<String>,
    pub status: Option<RunStatus>,
    pub log: RunLog,
    pub phase: RunPhase,
    /// The script is a server: it reported ready and keeps running, so its
    /// status is still read.
    pub live: bool,
    /// Whether the manifest declared `keep_alive` (persisted in `meta.json` across restarts).
    pub keep_alive: bool,
    /// The endpoints the run offers, resolved when it starts.
    pub endpoints: Vec<ResolvedEndpoint>,
}

/// Lines of a run's log the app keeps, the newest ones. The log view builds every line it has
/// on each frame, and a frame of the run page costs about 0.8 ms plus 4.6 ms per 1,000 lines
/// (release build, Apple M4 Pro), so this many leave about half of a 60 Hz frame. The whole log
/// is a file on the runtime.
pub const RUN_LOG_LINES_KEPT: usize = 2_000;

/// Where the files a run produced are saved: a folder of the plugin's own in the
/// user's downloads, or in the app's own directory when there is none.
pub(crate) fn outputs_dir(plugin: &str) -> std::path::PathBuf {
    let base = dirs::download_dir()
        .or_else(colab_playground_core::paths::default_local_data_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("colab-playground").join(plugin)
}

/// The newest lines of a run's log (secrets already redacted by core).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunLog(VecDeque<String>);

impl RunLog {
    fn extend(&mut self, lines: impl IntoIterator<Item = String>) {
        self.0.extend(lines);
        let extra = self.0.len().saturating_sub(RUN_LOG_LINES_KEPT);
        self.0.drain(..extra);
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Oldest first.
    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

impl AppState {
    pub fn plugin_view(&self) -> PluginView {
        self.plugin_view
    }

    pub fn run_form(&self) -> Option<&RunForm> {
        self.run_form.as_ref()
    }

    /// The current (or last) run.
    pub fn plugin_run(&self) -> Option<&PluginRun> {
        self.plugin_run.as_ref()
    }

    pub fn show_plugin_list(&mut self, cx: &mut Context<Self>) {
        self.plugin_view = PluginView::List;
        cx.notify();
    }

    pub fn show_plugin_run(&mut self, cx: &mut Context<Self>) {
        if self.plugin_run.is_some() {
            self.plugin_view = PluginView::Run;
            self.navigate(Route::Plugins, cx);
        }
    }

    pub fn open_run_form(&mut self, plugin: String, cx: &mut Context<Self>) {
        self.navigate(Route::Plugins, cx);
        self.refresh_hardware_catalog(cx);
        let work_plugin = plugin.clone();
        self.run_backend(
            Op::RunTemplate,
            cx,
            move |backend, _| backend.plugin_run_template(&work_plugin),
            move |state, result, cx| {
                match result {
                    Ok(template) => state.fill_run_form(template, cx),
                    Err(error) => state.toast_error(&error, cx),
                }
                cx.notify();
            },
        );
    }

    fn fill_run_form(&mut self, template: PluginRunSpec, cx: &mut Context<Self>) {
        let Some(info) = self.plugin(&template.plugin).cloned() else {
            return;
        };
        let provision = match &template.target {
            RunTarget::Provision(spec) => spec.clone(),
            RunTarget::Existing { .. } => self.defaults().provision_spec(String::new()),
        };
        let mut values = std::collections::BTreeMap::new();
        for input in &info.manifest.inputs {
            let value = template.inputs.get(&input.id).cloned();
            match input.kind {
                InputKind::Text | InputKind::HfModel | InputKind::Number => {
                    let text = value.map(|v| v.to_env()).unwrap_or_default();
                    self.set_input(InputSlot::Field(input.id.clone()), text);
                }
                InputKind::Secret => {
                    self.set_input(InputSlot::Field(input.id.clone()), String::new())
                }
                InputKind::Bool | InputKind::Select => {
                    if let Some(value) = value {
                        values.insert(input.id.clone(), value);
                    }
                }
            }
        }
        self.set_input(InputSlot::RunSessionName, provision.name.clone());
        self.run_form = Some(RunForm {
            plugin: info.manifest.id.clone(),
            values,
            provision,
            existing: None,
            existing_account: None,
            target_lost: None,
            stop_on_failure: template.stop_on_failure,
            keep_plan: template.keep_plan,
            hf: HfLookup::default(),
            error: None,
        });
        self.plugin_view = PluginView::Form;
        cx.notify();
    }

    /// Edits the form's non-text values, target or provisioning options.
    pub fn edit_run_form(&mut self, edit: impl FnOnce(&mut RunForm), cx: &mut Context<Self>) {
        if let Some(form) = &mut self.run_form {
            edit(form);
            if !form.provision.accelerator.is_gpu() {
                form.provision.min_vram_mib = None;
            }
            form.error = None;
            cx.notify();
        }
    }

    /// Picks the form's target: an existing session by name, or a new one.
    pub fn pick_run_target(&mut self, existing: Option<String>, cx: &mut Context<Self>) {
        let account = existing.as_ref().and_then(|name| {
            let view = self.runtimes().ready()?;
            let picked = view.runtimes.iter().find(|runtime| {
                runtime.on_active_account && runtime.id.name.as_ref() == Some(name)
            })?;
            Some(picked.id.account.clone())
        });
        self.edit_run_form(
            |form| {
                form.existing = existing;
                form.existing_account = account;
                form.target_lost = None;
            },
            cx,
        );
    }

    /// After a listing: a session picked as the form's target that is gone (stopped, or its
    /// account is no longer the active one) is unpicked, and the form says that the run would
    /// now start a new session instead of failing on a name that no longer exists.
    pub(crate) fn drop_gone_run_target(&mut self) {
        let Some(name) = self
            .run_form
            .as_ref()
            .and_then(|form| form.existing.clone())
        else {
            return;
        };
        let Some(view) = self.runtimes().ready() else {
            return;
        };
        // Only the active account's sessions can be picked, so only its listing counts: without
        // it (or without knowing which account that is) nothing says the session is gone.
        let Some(active) = self
            .accounts()
            .ready()
            .and_then(|accounts| accounts.active.as_ref())
            .filter(|active| !view.errors.iter().any(|failed| &failed.account == *active))
        else {
            return;
        };
        let named = |runtime: &&Runtime| runtime.id.name.as_deref() == Some(name.as_str());
        let picked_on = self
            .run_form
            .as_ref()
            .and_then(|form| form.existing_account.as_ref());
        // A name is unique within an account only: after a switch, a session of that name on
        // the active account is not the one that was picked.
        let switched = picked_on.is_some_and(|account| account != active);
        if !switched
            && view
                .runtimes
                .iter()
                .filter(named)
                .any(|runtime| runtime.on_active_account && runtime.managed)
        {
            return;
        }
        // Gone from the active account. After a switch that is not the end of the session: it
        // may still be billing on the account it was picked on.
        let elsewhere = match picked_on {
            Some(_) => switched,
            None => view.runtimes.iter().any(|runtime| named(&runtime)),
        };
        let lost = if elsewhere {
            TargetLost::OtherAccount(name)
        } else {
            TargetLost::Ended(name)
        };
        log::info!("plugins: the run form's session is unpicked ({lost:?}); back to a new session");
        if let Some(form) = &mut self.run_form {
            form.existing = None;
            form.existing_account = None;
            form.target_lost = Some(lost);
        }
    }

    pub fn close_run_form(&mut self, cx: &mut Context<Self>) {
        self.run_form = None;
        self.plugin_view = PluginView::List;
        cx.notify();
    }

    /// Searches Hugging Face; only the latest query's results are kept.
    pub fn search_hf(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(form) = &mut self.run_form else {
            return;
        };
        form.hf.generation += 1;
        form.hf.results = Load::Loading;
        let generation = form.hf.generation;
        self.run_latest(
            cx,
            move |backend, cancel| backend.hf_search(query.trim(), HF_RESULTS, cancel),
            move |state, result, cx| {
                if let Some(form) = &mut state.run_form
                    && form.hf.generation == generation
                {
                    form.hf.results = match result {
                        Ok(models) => Load::Ready(models),
                        Err(error) => Load::Failed(error),
                    };
                    cx.notify();
                }
            },
        );
        cx.notify();
    }

    pub fn choose_hf_repo(&mut self, field: String, repo: String, cx: &mut Context<Self>) {
        self.set_input(InputSlot::Field(field), repo.clone());
        self.load_hf_files(repo, cx);
    }

    /// Lists `repo`'s GGUF files (for the quantisation preview).
    pub fn load_hf_files(&mut self, repo: String, cx: &mut Context<Self>) {
        let repo = repo.trim().to_owned();
        let Some(form) = &mut self.run_form else {
            return;
        };
        if repo.is_empty()
            || (form.hf.repo.as_deref() == Some(&repo) && form.hf.files.ready().is_some())
        {
            return;
        }
        form.hf.repo = Some(repo.clone());
        form.hf.files = Load::Loading;
        let work_repo = repo.clone();
        self.run_backend(
            Op::HfFiles(key_of(&repo)),
            cx,
            move |backend, cancel| backend.hf_files(&work_repo, cancel),
            move |state, result, cx| {
                if let Some(form) = &mut state.run_form
                    && form.hf.repo.as_deref() == Some(&repo)
                {
                    form.hf.files = match result {
                        Ok(listing) => Load::Ready(listing),
                        Err(error) => Load::Failed(error),
                    };
                    cx.notify();
                }
            },
        );
        cx.notify();
    }

    /// Starts the run with the text fields' contents (`texts`: input id → text, secrets
    /// included). Returns whether it started: the caller clears the secret fields only then,
    /// so a form that stays open with an error keeps what was typed.
    pub fn start_run(
        &mut self,
        texts: Vec<(String, String)>,
        session_name: String,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.ops.is_running(Op::PluginRun) {
            return false;
        }
        let Some(form) = &self.run_form else {
            return false;
        };
        let Some(info) = self.plugin(&form.plugin).cloned() else {
            return false;
        };
        let mut inputs = form.values.clone();
        for (id, text) in texts {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let numeric = info
                .manifest
                .input(&id)
                .is_some_and(|input| input.kind == InputKind::Number);
            let value = if numeric {
                match text.parse::<f64>() {
                    Ok(number) => InputValue::Number(number),
                    Err(_) => {
                        let error = Error::new(ErrorKind::InvalidInput)
                            .with("field", &id)
                            .with("value", text);
                        if let Some(form) = &mut self.run_form {
                            form.error = Some(error);
                        }
                        cx.notify();
                        return false;
                    }
                }
            } else {
                InputValue::Text(text.to_owned())
            };
            inputs.insert(id, value);
        }
        if let Some(missing) = missing_required(&info, &inputs) {
            let error = Error::new(ErrorKind::InvalidInput).with("field", missing);
            if let Some(form) = &mut self.run_form {
                form.error = Some(error);
            }
            cx.notify();
            return false;
        }
        let target = match &form.existing {
            Some(name) => RunTarget::Existing { name: name.clone() },
            None => {
                let mut spec = form.provision.clone();
                if !session_name.trim().is_empty() {
                    spec.name = session_name.trim().to_owned();
                }
                RunTarget::Provision(spec)
            }
        };
        let provisions = matches!(target, RunTarget::Provision(_));
        let keep_alive = self
            .plugin(&form.plugin)
            .is_some_and(|info| info.manifest.run.keep_alive);
        let endpoints = self
            .plugin(&form.plugin)
            .and_then(|info| resolve_endpoints(&info.manifest, &inputs).ok())
            .unwrap_or_default();
        let spec = PluginRunSpec {
            plugin: form.plugin.clone(),
            inputs,
            target,
            stop_on_failure: form.stop_on_failure,
            keep_alive,
            keep_plan: form.keep_plan.clone(),
        };
        self.plugin_run = Some(PluginRun {
            plugin: form.plugin.clone(),
            provisions,
            provision: ProvisionProgress::default(),
            session: None,
            account: None,
            run_id: None,
            status: None,
            log: RunLog::default(),
            phase: RunPhase::Running,
            live: false,
            keep_alive,
            endpoints,
        });
        self.plugin_view = PluginView::Run;
        self.run_backend_streaming(
            Op::PluginRun,
            cx,
            move |backend, events, cancel| backend.run_plugin(spec, events, cancel),
            |state, event, cx| {
                state.apply_run_event(event, cx);
                cx.notify();
            },
            |state, result, cx| {
                state.finish_run(result, cx);
                state.continue_stop_all(cx);
                cx.notify();
            },
        );
        cx.notify();
        true
    }

    /// Scans live runtimes once for a detached plugin run started before this app launch and
    /// reattaches the first active one when no run is currently tracked.
    pub(crate) fn discover_active_runs(&mut self, cx: &mut Context<Self>) {
        if self.plugin_run.is_some()
            || self.ops.is_running(Op::PluginRun)
            || self.ops.is_running(Op::DiscoverRuns)
        {
            return;
        }
        let Some(view) = self.sessions.runtimes.ready() else {
            return;
        };
        if view.runtimes.is_empty() {
            return;
        }
        let targets: Vec<_> = view.runtimes.iter().map(|runtime| runtime.id.clone()).collect();
        self.run_backend(
            Op::DiscoverRuns,
            cx,
            move |backend, cancel| backend.discover_runs(&targets, cancel),
            |state, result, cx| match result {
                Ok(runs) => {
                    if state.plugin_run.is_some() || state.ops.is_running(Op::PluginRun) {
                        return;
                    }
                    if let Some(discovered) = runs
                        .into_iter()
                        .find(|run| !state.finished_run_ids.contains(&run.meta.run_id))
                    {
                        state.reattach_discovered_run(discovered, cx);
                    }
                }
                Err(error) => log::warn!("runs: discovering active runs failed: {error}"),
            },
        );
    }

    fn reattach_discovered_run(&mut self, discovered: DiscoveredRun, cx: &mut Context<Self>) {
        let endpoints = if !discovered.meta.endpoints.is_empty() {
            discovered.meta.endpoints.clone()
        } else {
            self.plugin(&discovered.meta.plugin)
                .and_then(|info| resolve_endpoints(&info.manifest, &discovered.meta.inputs).ok())
                .unwrap_or_default()
        };
        let mut log = RunLog::default();
        log.extend(discovered.log_tail.clone());
        let keep_alive = discovered.meta.keep_alive
            || self
                .plugin(&discovered.meta.plugin)
                .is_some_and(|info| info.manifest.run.keep_alive);
        let ready_status = discovered
            .status
            .clone()
            .filter(|status| status.state == RunState::Ready);
        let (phase, live) = match ready_status {
            Some(status) => (
                RunPhase::Ready(Box::new(PluginRunOutcome {
                    session: discovered.session.clone(),
                    account: discovered.account.clone(),
                    run_id: discovered.meta.run_id.clone(),
                    status,
                    endpoints: endpoints.clone(),
                })),
                keep_alive,
            ),
            None => (RunPhase::Running, false),
        };
        let already_ready = matches!(phase, RunPhase::Ready(_));
        self.plugin_run = Some(PluginRun {
            plugin: discovered.meta.plugin.clone(),
            provisions: false,
            provision: ProvisionProgress::default(),
            session: Some(discovered.session.clone()),
            account: Some(discovered.account.clone()),
            run_id: Some(discovered.meta.run_id.clone()),
            status: discovered.status.clone(),
            log,
            phase,
            live,
            keep_alive,
            endpoints: endpoints.clone(),
        });
        if already_ready {
            self.open_endpoint_forwards(
                discovered.account.clone(),
                discovered.session.clone(),
                &endpoints,
                cx,
            );
        }
        self.publish_snapshot();
        self.run_backend_streaming(
            Op::PluginRun,
            cx,
            move |backend, events, cancel| backend.reattach_run(discovered, events, cancel),
            |state, event, cx| {
                state.apply_run_event(event, cx);
                cx.notify();
            },
            |state, result, cx| {
                state.finish_run(result, cx);
                state.continue_stop_all(cx);
                cx.notify();
            },
        );
        cx.notify();
    }

    fn open_endpoint_forwards(
        &mut self,
        account: AccountId,
        session: String,
        endpoints: &[ResolvedEndpoint],
        cx: &mut Context<Self>,
    ) {
        for endpoint in endpoints {
            let already_open = self.sessions.tunnels.values().any(|tunnel| {
                tunnel.spec.account == account
                    && tunnel.spec.session == session
                    && tunnel.spec.remote_port == endpoint.remote_port
            });
            if !already_open {
                self.open_tunnel(
                    TunnelSpec {
                        account: account.clone(),
                        session: session.clone(),
                        local_port: endpoint.remote_port,
                        remote_port: endpoint.remote_port,
                        health_path: endpoint.health.clone(),
                    },
                    false,
                    cx,
                );
            }
        }
    }

    pub fn apply_run_event(&mut self, event: PluginRunEvent, cx: &mut Context<Self>) {
        let at = self.backend_now();
        let Some(run) = &mut self.plugin_run else {
            return;
        };
        let mut became_ready = false;
        let mut published_stage = false;
        match event {
            PluginRunEvent::Provision(event) => run.provision.push(event, at),
            PluginRunEvent::Launched { session, run_id } => {
                run.session = Some(session);
                run.run_id = Some(run_id);
            }
            PluginRunEvent::Status(status) => {
                became_ready = status.state == RunState::Ready;
                run.status = Some(status);
                published_stage = true;
            }
            PluginRunEvent::Log { lines } => run.log.extend(lines),
            PluginRunEvent::LogSkipped { path } => {
                run.log
                    .extend([tr_args(cx, keys::RUN_LOG_SKIPPED, &[("path", &path)]).into()])
            }
        }
        if became_ready {
            self.run_became_ready(cx);
        }
        self.publish_snapshot();
        if published_stage {
            // A waiting `run_watch` hears about it too: the same event, told to whoever asked.
            self.answer_agent_run_stage(cx);
            let kind = if became_ready { "ready" } else { "stage" };
            self.answer_agent_watch(kind, cx);
            self.notify_run_event(if became_ready { "run_ready" } else { "run_stage" });
        }
    }

    fn finish_run(
        &mut self,
        result: colab_playground_core::Result<PluginRunOutcome>,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = &self.plugin_run else {
            return;
        };
        // A run that reported ready while it was still going is already shown
        // as ready; it must not say so twice.
        let was_ready = matches!(run.phase, RunPhase::Ready(_));
        let prev_session = run.session.clone();
        let prev_account = run.account.clone();
        let prev_run_id = run.run_id.clone();
        let is_session_gone = matches!(&result, Err(error) if error.is(ErrorKind::SessionGone));
        let watch_reason = match &result {
            Ok(_) => "finished",
            Err(error) if error.is(ErrorKind::Cancelled) => "cancelled",
            Err(_) => "failed",
        };
        let sse_event = match &result {
            Ok(_) => "run_finished",
            Err(error) if error.is(ErrorKind::Cancelled) => "run_cancelled",
            Err(error) if error.is(ErrorKind::SessionGone) => "run_gone",
            Err(_) => "run_failed",
        };
        let forwards_target = result
            .as_ref()
            .ok()
            .map(|outcome| {
                (
                    outcome.account.clone(),
                    outcome.session.clone(),
                    outcome.endpoints.clone(),
                )
            });
        let next_phase = match result {
            Ok(outcome) => {
                if let Some(run) = &mut self.plugin_run {
                    run.session = Some(outcome.session.clone());
                    run.account = Some(outcome.account.clone());
                    run.run_id = Some(outcome.run_id.clone());
                    run.status = Some(outcome.status.clone());
                }
                RunPhase::Ready(Box::new(outcome))
            }
            Err(error) if error.is(ErrorKind::Cancelled) => RunPhase::Cancelled,
            Err(error) => RunPhase::Failed(error),
        };
        let active = self.accounts.ready().and_then(|view| view.active.clone());
        let Some(run) = &mut self.plugin_run else {
            return;
        };
        run.phase = next_phase;
        if run.account.is_none() {
            run.account = active;
        }
        // The script is gone now, whatever it was.
        run.live = false;
        let ready = matches!(run.phase, RunPhase::Ready(_));
        let plugin = run.plugin.clone();
        let final_run_id = run.run_id.clone().or(prev_run_id);
        let final_session = run.session.clone().or(prev_session);
        let final_account = run.account.clone().or(prev_account);
        if let Some(run_id) = final_run_id {
            self.finished_run_ids.insert(run_id);
        }
        self.publish_snapshot();
        self.answer_agent_watch(watch_reason, cx);
        self.notify_run_event(sse_event);
        if is_session_gone {
            self.notify_run_session_gone(final_account, final_session, cx);
        }
        if ready && !was_ready {
            self.toast_ready(&plugin, cx);
        }
        if let Some((account, session, endpoints)) = forwards_target {
            self.open_endpoint_forwards(account, session, &endpoints, cx);
        }
        self.after_runtime_change(cx);
    }

    /// The run reported ready: the phase turns ready so the endpoints show, and
    /// the forwards open. A server keeps running, so it stays cancellable.
    fn run_became_ready(&mut self, cx: &mut Context<Self>) {
        let Some(run) = &self.plugin_run else {
            return;
        };
        if matches!(run.phase, RunPhase::Ready(_)) {
            return;
        }
        let Some(status) = run.status.clone() else {
            return;
        };
        let session = run.session.clone().unwrap_or_default();
        let endpoints = run.endpoints.clone();
        let plugin = run.plugin.clone();
        let run_id = run.run_id.clone().unwrap_or_default();
        let live = run.keep_alive
            || self
                .plugin(&plugin)
                .is_some_and(|info| info.manifest.run.keep_alive);
        let Some(account) = run
            .account
            .clone()
            .or_else(|| self.accounts.ready().and_then(|view| view.active.clone()))
        else {
            return;
        };
        let outcome = PluginRunOutcome {
            session: session.clone(),
            account: account.clone(),
            run_id,
            status,
            endpoints: endpoints.clone(),
        };
        if let Some(run) = &mut self.plugin_run {
            run.phase = RunPhase::Ready(Box::new(outcome));
            run.live = live;
        }
        self.open_endpoint_forwards(account, session, &endpoints, cx);
        self.toast_ready(&plugin, cx);
    }

    /// Says that a run is ready, once.
    fn toast_ready(&mut self, plugin: &str, cx: &mut Context<Self>) {
        let name = self
            .plugin(plugin)
            .map(|info| self.plugin_name(info))
            .unwrap_or_else(|| plugin.to_owned());
        self.toast_done(tr_args(cx, keys::RUN_READY_TOAST, &[("name", &name)]), cx);
        self.count_ready(cx);
    }

    /// Asks the run to stop; a session it provisioned is stopped too.
    pub fn cancel_run(&mut self, cx: &mut Context<Self>) {
        if let Some(run_id) = self.plugin_run.as_ref().and_then(|run| run.run_id.clone()) {
            self.finished_run_ids.insert(run_id);
        }
        self.ops.cancel(Op::PluginRun);
        cx.notify();
    }

    /// Saves what the run produced on the VM to this computer: every file
    /// under `path` lands in a folder of its own.
    pub fn save_run_outputs(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(target) = self.run_runtime() else {
            self.toast_error(&Error::new(ErrorKind::SessionGone), cx);
            return;
        };
        let plugin = self
            .plugin_run
            .as_ref()
            .map(|run| run.plugin.clone())
            .unwrap_or_default();
        let path = path.to_owned();
        // The work closure moves the plugin's name, and the answer's log line wants it too.
        let named = plugin.clone();
        self.run_backend(
            Op::DownloadOutputs(key_of(&path)),
            cx,
            move |backend, cancel| {
                let files = backend.vm_ls(&target, &path, cancel)?;
                if files.is_empty() {
                    // Say where it looked. "Nothing to save" and "I looked in the wrong place" are
                    // the same answer otherwise — which is how a save copies nothing and still
                    // reads like it worked.
                    return Err(Error::new(ErrorKind::Io)
                        .with("path", &path)
                        .with("session", target.name.clone().unwrap_or_default())
                        .detail("the runtime has no files there"));
                }
                let dir = outputs_dir(&plugin);
                std::fs::create_dir_all(&dir)?;
                for file in &files {
                    let name = file.path.rsplit('/').next().unwrap_or("file");
                    let local = dir.join(name);
                    // The CLI writes it, so it needs the path in its own namespace: a Windows path
                    // given to it as-is becomes one file name in its working directory.
                    let target_path = colab_playground_core::paths::host_path_for_local(&local)
                        .ok_or_else(|| {
                            Error::new(ErrorKind::Io)
                                .with("path", local.to_string_lossy())
                                .detail("the download cannot be written there")
                        })?;
                    backend.vm_download(&target, &file.path, target_path.as_str(), cancel)?;
                }
                Ok(dir.to_string_lossy().to_string())
            },
            move |state, result: colab_playground_core::Result<String>, cx| {
                match result {
                    Ok(dir) => {
                        let here = super::mcp::files_in(std::path::Path::new(dir.as_str())).len();
                        log::info!("download: {named} -> {dir} ({here} file(s) here)");
                        state.toast_done(tr_args(cx, keys::RUN_SAVED, &[("path", &dir)]), cx)
                    }
                    Err(error) => {
                        log::warn!("download: {named} failed: {error}");
                        state.toast_error(&error, cx)
                    }
                }
                cx.notify();
            },
        );
    }

    pub fn is_running_plugin(&self) -> bool {
        self.ops.is_running(Op::PluginRun)
    }

    /// Whether a run's cancel was asked and it is still winding down.
    pub fn is_cancelling_run(&self) -> bool {
        self.ops.is_cancelling(Op::PluginRun)
    }

    /// The runtime a run used, as listed (to stop it from the run view).
    pub fn run_runtime(&self) -> Option<colab_playground_core::backend::RuntimeRef> {
        let run = self.plugin_run.as_ref()?;
        let session = run.session.as_ref()?;
        self.runtimes()
            .ready()?
            .runtimes
            .iter()
            .find(|runtime| {
                runtime.id.name.as_ref() == Some(session)
                    && run
                        .account
                        .as_ref()
                        .is_none_or(|a| &runtime.id.account == a)
            })
            .map(|runtime| runtime.id.clone())
    }

    /// Runs `work` on the background executor outside the one-per-kind rule; `done` decides
    /// whether the result is still wanted (latest-wins lookups).
    pub(crate) fn run_latest<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce(
            &dyn Backend,
            &colab_playground_core::CancelToken,
        ) -> colab_playground_core::Result<R>
        + Send
        + 'static,
        done: impl FnOnce(&mut AppState, colab_playground_core::Result<R>, &mut Context<AppState>)
        + 'static,
    ) {
        let backend: Arc<dyn Backend> = match &self.backend {
            Ok(backend) => backend.clone(),
            Err(error) => {
                let error = error.clone();
                done(self, Err(error), cx);
                return;
            }
        };
        cx.spawn(async move |this, cx| {
            let token = colab_playground_core::CancelToken::new();
            let result = cx
                .background_spawn(async move { work(&*backend, &token) })
                .await;
            this.update(cx, |state, cx| done(state, result, cx)).ok();
        })
        .detach();
    }
}

fn missing_required(
    info: &PluginInfo,
    inputs: &std::collections::BTreeMap<String, InputValue>,
) -> Option<String> {
    info.manifest
        .inputs
        .iter()
        .find(|input| {
            input.required
                && !matches!(inputs.get(&input.id), Some(InputValue::Text(t)) if !t.trim().is_empty())
                && !matches!(inputs.get(&input.id), Some(InputValue::Number(_) | InputValue::Bool(_)))
        })
        .map(|input| input.id.clone())
}
