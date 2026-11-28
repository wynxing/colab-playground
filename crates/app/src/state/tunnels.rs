//! Port forwards: the dialog, opening and closing, and live state.
//!
//! `open_tunnel` returns at once and the forward keeps reporting (reconnects, failures,
//! health), so its event receiver is kept and drained on a timer until the backend drops the
//! sender. Closing a forward never stops billing, and the app says so when it closes.

use std::{
    sync::mpsc::{Receiver, TryRecvError},
    time::Duration,
};

use colab_playground_core::{
    Error,
    accounts::AccountId,
    backend::RuntimeRef,
    tunnel::{TunnelEvent, TunnelId, TunnelInfo, TunnelSpec, TunnelState},
};
use ely_gpui_component::{feedback::Toast, primitives::Severity};
use gpui::Context;

use super::{AppState, InputSlot, Op, Overlay, ops::key_of};
use crate::i18n::{keys, tr};

const TUNNEL_TICK: Duration = Duration::from_millis(250);

/// Ports the forward dialog starts with (the bundled model server's port; the user changes
/// them freely).
pub const FORWARD_START_PORT: u16 = 8000;

#[derive(Clone, Debug, PartialEq)]
pub struct ForwardForm {
    pub target: RuntimeRef,
    pub session: String,
    /// Port on the runtime.
    pub remote: u16,
    /// Port on this computer.
    pub local: u16,
    pub error: Option<Error>,
}

impl ForwardForm {
    pub fn spec(&self, health: Option<String>) -> TunnelSpec {
        TunnelSpec {
            account: self.target.account.clone(),
            session: self.session.clone(),
            local_port: self.local,
            remote_port: self.remote,
            health_path: health,
        }
    }
}

/// The address a forward is reached at on this computer.
pub fn local_url(spec: &TunnelSpec) -> String {
    format!("http://localhost:{}", spec.local_port)
}

impl AppState {
    /// Open forwards (and failed ones not yet dismissed).
    pub fn tunnels(&self) -> impl Iterator<Item = &TunnelInfo> {
        self.sessions.tunnels.values()
    }

    pub fn tunnels_for(&self, target: &RuntimeRef) -> Vec<&TunnelInfo> {
        self.sessions
            .tunnels
            .values()
            .filter(|tunnel| {
                tunnel.spec.account == target.account
                    && Some(&tunnel.spec.session) == target.name.as_ref()
            })
            .collect()
    }

    pub fn tunnel(&self, id: TunnelId) -> Option<&TunnelInfo> {
        self.sessions.tunnels.get(&id)
    }

    /// Opens the forward dialog for a named runtime. Direct mode does not require the active account.
    pub fn open_forward_dialog(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(session) = target.name.clone() else {
            return;
        };
        self.overlay = Some(Overlay::Forward(ForwardForm {
            target,
            session,
            remote: FORWARD_START_PORT,
            local: FORWARD_START_PORT,
            error: None,
        }));
        self.set_input(InputSlot::ForwardRemote, FORWARD_START_PORT.to_string());
        self.set_input(InputSlot::ForwardLocal, FORWARD_START_PORT.to_string());
        self.set_input(InputSlot::Health, String::new());
        cx.notify();
    }

    /// Opens the forward the dialog describes (ports as typed); the dialog closes once it is
    /// up and shows why otherwise. Port 8080 on the runtime is refused with the reason.
    pub fn submit_forward(
        &mut self,
        remote: &str,
        local: &str,
        health: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(Overlay::Forward(form)) = &mut self.overlay else {
            return;
        };
        let parse = |text: &str, field: &str| {
            text.trim()
                .parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| {
                    Error::invalid(field, "ports must be 1-65535").with("value", text.trim())
                })
        };
        match (parse(remote, "remote_port"), parse(local, "local_port")) {
            (Ok(remote), Ok(local)) => {
                form.remote = remote;
                form.local = local;
            }
            (Err(error), _) | (_, Err(error)) => {
                form.error = Some(error);
                cx.notify();
                return;
            }
        }
        let spec = form.spec(health);
        if let Err(error) = spec.validate() {
            form.error = Some(error);
            cx.notify();
            return;
        }
        self.open_tunnel(spec, true, cx);
    }

    /// Whether a forward to `spec`'s runtime and remote port is being opened.
    pub fn is_opening_forward(&self, spec: &TunnelSpec) -> bool {
        self.ops.is_running(Op::Forward(forward_key(
            &spec.account,
            &spec.session,
            spec.remote_port,
        )))
    }

    /// Opens a forward. `from_dialog`: the forward dialog shows the outcome.
    pub(crate) fn open_tunnel(
        &mut self,
        spec: TunnelSpec,
        from_dialog: bool,
        cx: &mut Context<Self>,
    ) {
        let work_spec = spec.clone();
        self.run_backend(
            Op::Forward(forward_key(&spec.account, &spec.session, spec.remote_port)),
            cx,
            move |backend, _| {
                let (events, received) = std::sync::mpsc::channel();
                let id = backend.open_tunnel(work_spec, events)?;
                Ok((id, received))
            },
            move |state, result, cx| {
                match result {
                    Ok((id, received)) => {
                        state.sessions.tunnels.insert(
                            id,
                            TunnelInfo {
                                id,
                                spec: spec.clone(),
                                state: TunnelState::Starting,
                                healthy: None,
                            },
                        );
                        let open = state.drain_tunnel(&received, cx).open;
                        let failed = match state.sessions.tunnels.get(&id).map(|t| &t.state) {
                            Some(TunnelState::Failed { error }) => Some(error.clone()),
                            _ => None,
                        };
                        if let Some(error) = failed {
                            // A forward that failed at once is not kept; the dialog or a toast
                            // says why and it can be tried again.
                            state.sessions.tunnels.remove(&id);
                            state.discard_tunnel(id, cx);
                            state.forward_failed(error, from_dialog, cx);
                        } else {
                            if from_dialog && matches!(state.overlay, Some(Overlay::Forward(_))) {
                                state.overlay = None;
                            }
                            if open {
                                watch_tunnel(received, cx);
                            }
                        }
                    }
                    Err(error) => state.forward_failed(error, from_dialog, cx),
                }
                cx.notify();
            },
        );
    }

    fn forward_failed(&mut self, error: Error, from_dialog: bool, cx: &mut Context<Self>) {
        log::warn!("tunnels: the forward failed: {error}");
        match &mut self.overlay {
            Some(Overlay::Forward(form)) if from_dialog => form.error = Some(error),
            _ => self.toast_error(&error, cx),
        }
    }

    fn drain_tunnel(&mut self, received: &Receiver<TunnelEvent>, cx: &mut Context<Self>) -> Drained {
        let mut changed = false;
        loop {
            match received.try_recv() {
                Ok(event) => {
                    self.apply_tunnel_event(event, cx);
                    changed = true;
                }
                Err(TryRecvError::Empty) => {
                    return Drained {
                        open: true,
                        changed,
                    };
                }
                Err(TryRecvError::Disconnected) => {
                    return Drained {
                        open: false,
                        changed,
                    };
                }
            }
        }
    }

    fn apply_tunnel_event(&mut self, event: TunnelEvent, cx: &mut Context<Self>) {
        match event {
            TunnelEvent::State { id, state } => {
                let transition = self.sessions.tunnels.get_mut(&id).map(|tunnel| {
                    let prev = tunnel.state.clone();
                    let session = tunnel.spec.session.clone();
                    let local_port = tunnel.spec.local_port;
                    let remote_port = tunnel.spec.remote_port;
                    tunnel.state = state.clone();
                    (prev, session, local_port, remote_port)
                });
                if let Some((prev, session, local_port, remote_port)) = transition {
                    let disconnected_state = match &state {
                        TunnelState::Reconnecting { .. }
                            if !matches!(prev, TunnelState::Reconnecting { .. }) =>
                        {
                            Some("reconnecting")
                        }
                        TunnelState::Failed { .. }
                            if !matches!(prev, TunnelState::Starting | TunnelState::Failed { .. }) =>
                        {
                            Some("failed")
                        }
                        _ => None,
                    };
                    if let Some(state_label) = disconnected_state {
                        self.emit_session_event(
                            "tunnel_disconnected",
                            Some(&session),
                            serde_json::json!({
                                "local_port": local_port,
                                "remote_port": remote_port,
                                "state": state_label,
                            }),
                            cx,
                        );
                    } else if matches!(state, TunnelState::Up)
                        && matches!(prev, TunnelState::Reconnecting { .. })
                    {
                        self.emit_session_event(
                            "tunnel_reconnected",
                            Some(&session),
                            serde_json::json!({
                                "local_port": local_port,
                                "remote_port": remote_port,
                            }),
                            cx,
                        );
                    } else {
                        self.publish_snapshot();
                    }
                }
            }
            TunnelEvent::Health { id, healthy } => {
                let transition = self.sessions.tunnels.get_mut(&id).map(|tunnel| {
                    let prev = tunnel.healthy;
                    let session = tunnel.spec.session.clone();
                    let local_port = tunnel.spec.local_port;
                    let remote_port = tunnel.spec.remote_port;
                    tunnel.healthy = Some(healthy);
                    (prev, session, local_port, remote_port)
                });
                if let Some((prev, session, local_port, remote_port)) = transition
                    && prev != Some(healthy)
                {
                    if !healthy {
                        self.emit_session_event(
                            "tunnel_unhealthy",
                            Some(&session),
                            serde_json::json!({
                                "local_port": local_port,
                                "remote_port": remote_port,
                                "healthy": false,
                            }),
                            cx,
                        );
                    } else if prev == Some(false) {
                        self.emit_session_event(
                            "tunnel_healthy",
                            Some(&session),
                            serde_json::json!({
                                "local_port": local_port,
                                "remote_port": remote_port,
                                "healthy": true,
                            }),
                            cx,
                        );
                    } else {
                        self.publish_snapshot();
                    }
                }
            }
        }
    }

    /// Closes a forward; the runtime keeps running and billing, and the toast says so.
    pub fn close_forward(&mut self, id: TunnelId, cx: &mut Context<Self>) {
        self.run_backend(
            Op::CloseForward(id.0),
            cx,
            move |backend, _| backend.close_tunnel(id),
            move |state, result, cx| {
                if let Some(tunnel) = state.sessions.tunnels.remove(&id) {
                    state.emit_session_event(
                        "tunnel_disconnected",
                        Some(&tunnel.spec.session),
                        serde_json::json!({
                            "local_port": tunnel.spec.local_port,
                            "remote_port": tunnel.spec.remote_port,
                            "state": "closed",
                        }),
                        cx,
                    );
                }
                match result {
                    Ok(()) => state.toast(
                        Toast::new(tr(cx, keys::FORWARD_CLOSED))
                            .body(tr(cx, keys::FORWARD_CLOSED_BILLING))
                            .severity(Severity::Info),
                        cx,
                    ),
                    Err(error) => log::warn!("tunnels: closing failed: {error}"),
                }
                cx.notify();
            },
        );
    }

    /// Forgets a forward that already failed, telling the backend too.
    fn discard_tunnel(&mut self, id: TunnelId, cx: &mut Context<Self>) {
        self.run_backend(
            Op::CloseForward(id.0),
            cx,
            move |backend, _| backend.close_tunnel(id),
            |_, _, _| {},
        );
    }

    pub fn dismiss_forward(&mut self, id: TunnelId, cx: &mut Context<Self>) {
        self.sessions.tunnels.remove(&id);
        self.discard_tunnel(id, cx);
        cx.notify();
    }
}

/// The key of the call that opens a forward to `spec`'s runtime and remote port.
/// The key a forward's `Op` carries: one call per runtime and remote port, whatever local port it
/// lands on — the port on this computer is picked rather than named when an agent asks for one.
pub fn forward_key(account: &AccountId, session: &str, remote_port: u16) -> u64 {
    key_of(&(account, session, remote_port))
}

struct Drained {
    /// False once the backend dropped the sender.
    open: bool,
    changed: bool,
}

/// Keeps draining a live forward's events until the backend drops the sender.
fn watch_tunnel(received: Receiver<TunnelEvent>, cx: &mut Context<AppState>) {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(TUNNEL_TICK).await;
            let open = this.update(cx, |state, cx| {
                let drained = state.drain_tunnel(&received, cx);
                if drained.changed {
                    cx.notify();
                }
                drained.open
            });
            if !matches!(open, Ok(true)) {
                break;
            }
        }
    })
    .detach();
}
