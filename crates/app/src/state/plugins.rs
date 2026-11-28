//! Plugins: the installed list, installing from a folder or a Git URL (checked and confirmed
//! first), enabling, disabling and removing, and the stored Hugging Face token.
//!
//! A stored secret is never read back: the app only knows whether one is stored. The value
//! lives in the masked field until it is saved, then the field is cleared.

use std::path::PathBuf;

use colab_playground_core::{
    Error,
    plugins::{Capability, PluginInfo, PluginPreview, PluginSource},
};
use gpui::{AppContext, Context};

use super::{AppState, InputSlot, Load, Op, Overlay, ops::key_of};
use crate::i18n::{keys, tr_args};

/// The Hugging Face token's name in the vault.
pub const HF_TOKEN: &str = "hf_token";

#[derive(Clone, Debug, PartialEq)]
pub enum InstallPrompt {
    /// Checking the folder or cloning the repository.
    Checking,
    Preview(Box<PluginPreview>),
    Failed(Error),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemovePrompt {
    pub id: String,
    pub name: String,
    pub error: Option<Error>,
}

#[derive(Default)]
pub struct PluginsState {
    pub(crate) list: Load<Vec<PluginInfo>>,
    pub(crate) hf_token: Load<bool>,
}

/// Whether `text` names a Git repository rather than a local folder.
pub fn is_git_source(text: &str) -> bool {
    let text = text.trim();
    text.contains("://") || text.starts_with("git@")
}

impl AppState {
    pub fn plugins(&self) -> &Load<Vec<PluginInfo>> {
        &self.plugins.list
    }

    pub fn plugin(&self, id: &str) -> Option<&PluginInfo> {
        self.plugins
            .list
            .ready()?
            .iter()
            .find(|plugin| plugin.manifest.id == id)
    }

    /// Bundled, enabled plugins (the Overview's quick-deploy cards).
    pub fn quick_deploys(&self) -> Vec<&PluginInfo> {
        self.plugins
            .list
            .ready()
            .map(|list| {
                list.iter()
                    .filter(|p| p.enabled && p.source == PluginSource::Bundled)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether a Hugging Face token is stored.
    pub fn hf_token(&self) -> &Load<bool> {
        &self.plugins.hf_token
    }

    pub fn load_plugins(&mut self, cx: &mut Context<Self>) {
        self.plugins.list.begin();
        self.run_backend(
            Op::Plugins,
            cx,
            |backend, _| Ok(backend.plugins()),
            |state, result, cx| {
                state.plugins.list = match result {
                    Ok(list) => Load::Ready(list),
                    Err(error) => Load::Failed(error),
                };
                cx.notify();
            },
        );
        self.check_hf_token(cx);
    }

    fn check_hf_token(&mut self, cx: &mut Context<Self>) {
        // The keyring is read through the CLI's Python.
        if !self.cli_installed() {
            self.plugins.hf_token = Load::Failed(Self::not_installed());
            cx.notify();
            return;
        }
        self.plugins.hf_token.begin();
        self.run_backend(
            Op::Secret,
            cx,
            |backend, cancel| backend.has_secret(HF_TOKEN, cancel),
            |state, result, cx| {
                state.plugins.hf_token = match result {
                    Ok(stored) => Load::Ready(stored),
                    Err(error) => Load::Failed(error),
                };
                cx.notify();
            },
        );
    }

    /// Saves the token typed into the masked field. The caller clears the field.
    pub fn save_hf_token(&mut self, value: String, cx: &mut Context<Self>) {
        if value.trim().is_empty() {
            return;
        }
        self.run_backend(
            Op::Secret,
            cx,
            move |backend, cancel| backend.set_secret(HF_TOKEN, value.trim(), cancel),
            |state, result, cx| {
                match result {
                    Ok(()) => {
                        state.plugins.hf_token = Load::Ready(true);
                        state.toast_done(crate::i18n::tr(cx, keys::HF_TOKEN_SAVED), cx);
                    }
                    Err(error) => state.toast_error(&error, cx),
                }
                cx.notify();
            },
        );
    }

    /// Saves one named plugin secret. The caller clears the masked field; the value is not kept.
    pub fn save_plugin_secret(&mut self, name: String, value: String, cx: &mut Context<Self>) {
        let value = value.trim().to_owned();
        if value.is_empty() {
            return;
        }
        self.run_backend(
            Op::Secret,
            cx,
            move |backend, cancel| backend.set_secret(&name, &value, cancel),
            |state, result, cx| {
                match result {
                    Ok(()) => state.toast_done(crate::i18n::tr(cx, keys::ACTION_SAVE), cx),
                    Err(error) => state.toast_error(&error, cx),
                }
                cx.notify();
            },
        );
    }

    pub fn delete_hf_token(&mut self, cx: &mut Context<Self>) {
        self.run_backend(
            Op::Secret,
            cx,
            |backend, cancel| backend.delete_secret(HF_TOKEN, cancel),
            |state, result, cx| {
                match result {
                    Ok(()) => {
                        state.plugins.hf_token = Load::Ready(false);
                        state.toast_done(crate::i18n::tr(cx, keys::HF_TOKEN_DELETED), cx);
                    }
                    Err(error) => state.toast_error(&error, cx),
                }
                cx.notify();
            },
        );
    }

    pub fn inspect_plugin(&mut self, source: String, cx: &mut Context<Self>) {
        let source = source.trim().to_owned();
        if source.is_empty() {
            return;
        }
        if self.ops.is_running(Op::InspectPlugin) {
            // The check of a dialog closed since is still stopping; its answer is not this
            // source's.
            return;
        }
        self.overlay = Some(Overlay::InstallPlugin(InstallPrompt::Checking));
        self.run_backend(
            Op::InspectPlugin,
            cx,
            move |backend, cancel| {
                if is_git_source(&source) {
                    backend.inspect_plugin_git(&source, cancel)
                } else {
                    backend.inspect_plugin_folder(&PathBuf::from(&source))
                }
            },
            |state, result, cx| match &mut state.overlay {
                Some(Overlay::InstallPlugin(prompt)) => {
                    *prompt = match result {
                        Ok(preview) => InstallPrompt::Preview(Box::new(preview)),
                        Err(error) => InstallPrompt::Failed(error),
                    };
                    cx.notify();
                }
                // The dialog was closed while the check ran: nothing will confirm it.
                _ => {
                    if let Ok(preview) = result {
                        state.discard_preview(preview, cx);
                    }
                }
            },
        );
        cx.notify();
    }

    /// Closes the install dialog and stops the check still running for it.
    pub(super) fn close_install_plugin(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.overlay,
            Some(Overlay::InstallPlugin(InstallPrompt::Checking))
        ) {
            self.ops.cancel(Op::InspectPlugin);
        }
        self.drop_install_preview(cx);
        self.overlay = None;
        cx.notify();
    }

    /// The install dialog is going away: a preview it shows is dropped along with the copy
    /// staged for it, unless its install is running, which uses the copy and removes it.
    pub(super) fn drop_install_preview(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::InstallPlugin(InstallPrompt::Preview(preview))) = &self.overlay else {
            return;
        };
        if self.is_busy(Op::PluginChange(key_of(&preview.manifest.id))) {
            return;
        }
        let preview = preview.as_ref().clone();
        self.discard_preview(preview, cx);
    }

    fn discard_preview(&mut self, preview: PluginPreview, cx: &mut Context<Self>) {
        if let Ok(backend) = &self.backend {
            let backend = backend.clone();
            cx.background_spawn(async move { backend.discard_plugin_preview(&preview) })
                .detach();
        }
    }

    pub fn confirm_install(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::InstallPlugin(InstallPrompt::Preview(preview))) = &self.overlay else {
            return;
        };
        let preview = preview.as_ref().clone();
        let id = preview.manifest.id.clone();
        self.run_backend(
            Op::PluginChange(key_of(&id)),
            cx,
            move |backend, _| backend.install_plugin(&preview),
            |state, result, cx| {
                match result {
                    Ok(info) => {
                        if matches!(state.overlay, Some(Overlay::InstallPlugin(_))) {
                            state.overlay = None;
                        }
                        state.set_input(InputSlot::PluginSource, String::new());
                        let name = state.plugin_name(&info);
                        state.toast_done(
                            tr_args(cx, keys::PLUGIN_INSTALLED, &[("name", &name)]),
                            cx,
                        );
                        state.load_plugins(cx);
                    }
                    Err(error) => {
                        if let Some(Overlay::InstallPlugin(prompt)) = &mut state.overlay {
                            *prompt = InstallPrompt::Failed(error);
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    pub fn set_plugin_enabled(&mut self, id: String, enabled: bool, cx: &mut Context<Self>) {
        let work_id = id.clone();
        self.run_backend(
            Op::PluginChange(key_of(&id)),
            cx,
            move |backend, _| backend.set_plugin_enabled(&work_id, enabled),
            |state, result, cx| {
                if let Err(error) = result {
                    state.toast_error(&error, cx);
                }
                state.load_plugins(cx);
            },
        );
    }

    /// Asks before removing a user plugin.
    pub fn request_remove_plugin(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(info) = self.plugin(&id) else {
            return;
        };
        let name = self.plugin_name(info);
        self.overlay = Some(Overlay::RemovePlugin(RemovePrompt {
            id,
            name,
            error: None,
        }));
        cx.notify();
    }

    pub fn confirm_remove_plugin(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::RemovePlugin(prompt)) = &self.overlay else {
            return;
        };
        let (id, name) = (prompt.id.clone(), prompt.name.clone());
        let work_id = id.clone();
        self.run_backend(
            Op::PluginChange(key_of(&id)),
            cx,
            move |backend, _| backend.remove_plugin(&work_id),
            move |state, result, cx| {
                match result {
                    Ok(()) => {
                        if matches!(state.overlay, Some(Overlay::RemovePlugin(_))) {
                            state.overlay = None;
                        }
                        state.toast_done(tr_args(cx, keys::PLUGIN_REMOVED, &[("name", &name)]), cx);
                        state.load_plugins(cx);
                    }
                    Err(error) => {
                        if let Some(Overlay::RemovePlugin(prompt)) = &mut state.overlay {
                            prompt.error = Some(error);
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    /// A plugin's name in the display language.
    pub fn plugin_name(&self, info: &PluginInfo) -> String {
        info.manifest
            .name
            .get(self.language().tag())
            .unwrap_or(&info.manifest.id)
            .to_owned()
    }

    pub fn wants_hf_token(info: &PluginInfo) -> bool {
        info.manifest.needs.contains(&Capability::HfToken)
    }
}
