//! The window's root view: the app's title strip over Ely's app shell (sidebar and page), the
//! toasts, and Ely's resize border for client-drawn windows.

use colab_playground_core::plugins::InputKind;
use ely_gpui_component::{
    feedback::ToastViewport,
    forms::TextInput,
    layout::{AppShell, ScrollArea},
    primitives::{FocusScope, raise},
    shell::ResizeBorder,
    theme::{ActiveTheme, Mode, TextSize},
};
use gpui::{
    AppContext, Context, Entity, FocusHandle, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, div, prelude::FluentBuilder as _,
};

use crate::{
    i18n::keys,
    platform::{self, Chrome},
    state::AppState,
    ui::{Inputs, dialogs, overlays, pages, sidebar, tagged, titlebar},
};

pub const PAGE_SELECTOR: &str = "page-area";

/// Draw order of the title strip: above Ely's dialog backdrops (priority 0).
const TITLE_STRIP_PRIORITY: usize = 1;
const PORT_DIGITS: usize = u16::MAX.ilog10() as usize + 1;
const INTERVAL_DIGITS: usize = 4;

pub struct RootView {
    focus: FocusHandle,
    state: Entity<AppState>,
    inputs: Inputs,
    fields_plugin: Option<String>,
    /// The label and kind each run-form field was made with.
    field_specs: std::collections::BTreeMap<String, (String, InputKind)>,
    /// Label each plugin secret field was made with, so a language change rebuilds it.
    secret_labels: std::collections::BTreeMap<String, String>,
    page_scroll: gpui::ScrollHandle,
    shown_route: crate::route::Route,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    /// The root over `state`; follows the window's light or dark appearance into the state.
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let (colab_path, colab_python) = {
            let settings = state.read(cx).settings();
            (
                settings.colab_path.clone().unwrap_or_default(),
                settings.colab_python.clone().unwrap_or_default(),
            )
        };
        let plain = |label: &'static str,
                     placeholder: &'static str,
                     masked: bool,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                let input = TextInput::new(window, cx)
                    .label_key(label)
                    .placeholder_key(placeholder);
                if masked { input.masked() } else { input }
            })
        };
        let port = |label: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                TextInput::new(window, cx)
                    .label_key(label)
                    .filter(|c| c.is_ascii_digit())
                    .max_len(PORT_DIGITS)
            })
        };
        let field =
            |text: String, label: &'static str, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    let mut input = TextInput::new(window, cx)
                        .label_key(label)
                        .placeholder_key(keys::HOST_PATH_PLACEHOLDER);
                    input.set_text(text, cx);
                    input
                })
            };
        let inputs = Inputs {
            code: cx.new(|cx| {
                TextInput::new(window, cx)
                    .label_key(keys::ADD_CODE_LABEL)
                    .placeholder_key(keys::ADD_CODE_PLACEHOLDER)
            }),
            colab_path: field(colab_path, keys::HOST_COLAB_PATH, window, cx),
            colab_python: field(colab_python, keys::HOST_PYTHON_PATH, window, cx),
            session_name: plain(
                keys::NEW_NAME,
                keys::NEW_NAME_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            run_session_name: plain(
                keys::NEW_NAME,
                keys::NEW_NAME_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            health: plain(
                keys::FORWARD_HEALTH,
                keys::FORWARD_HEALTH_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            forward_remote: port(keys::FORWARD_REMOTE, window, cx),
            forward_local: port(keys::FORWARD_LOCAL, window, cx),
            plugin_source: plain(
                keys::PLUGIN_SOURCE,
                keys::PLUGIN_SOURCE_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            hf_query: plain(
                keys::HF_SEARCH,
                keys::HF_SEARCH_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            hf_token: plain(
                keys::HF_TOKEN_FIELD,
                keys::HF_TOKEN_PLACEHOLDER,
                true,
                window,
                cx,
            ),
            keep_paths: plain(
                keys::KEEP_PATHS,
                keys::KEEP_PATHS_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            keep_local_folder: plain(
                keys::KEEP_LOCAL_FOLDER,
                keys::KEEP_LOCAL_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            keep_hf_repo: plain(
                keys::KEEP_HF_REPO,
                keys::KEEP_HF_REPO_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            keep_hf_subpath: plain(
                keys::KEEP_HF_SUBPATH,
                keys::KEEP_HF_SUBPATH_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            keep_hf_interval: cx.new(|cx| {
                TextInput::new(window, cx)
                    .label_key(keys::KEEP_HF_INTERVAL)
                    .placeholder_key(keys::KEEP_HF_INTERVAL_PLACEHOLDER)
                    .filter(|c| c.is_ascii_digit())
                    .max_len(INTERVAL_DIGITS)
            }),
            keep_drive_mount: plain(
                keys::KEEP_DRIVE_MOUNT_PATH,
                keys::KEEP_DRIVE_MOUNT_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            keep_drive_subpath: plain(
                keys::KEEP_DRIVE_SUBPATH,
                keys::KEEP_DRIVE_SUBPATH_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            upload_local: plain(
                keys::VM_UPLOAD_LOCAL,
                keys::VM_UPLOAD_LOCAL_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            upload_remote: plain(
                keys::VM_UPLOAD_REMOTE,
                keys::VM_UPLOAD_REMOTE_PLACEHOLDER,
                false,
                window,
                cx,
            ),
            fields: Default::default(),
            secret_slots: Default::default(),
        };
        // Closing (system and drawn close alike) quits, asking first while runtimes still
        // bill. The close itself never removes the window: quitting does. Ely keeps the check
        // in a global, so it holds the state weakly.
        let guard = state.downgrade();
        ely_gpui_component::shell::guard_close(window, cx, move |_, cx| {
            guard.update(cx, |state, cx| state.request_quit(cx)).ok();
            false
        });
        let subscriptions = vec![
            cx.observe(&state, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |this, window, cx| {
                let mode = Mode::from(window.appearance());
                this.state
                    .update(cx, |state, cx| state.system_appearance_changed(mode, cx));
            }),
        ];
        let mode = Mode::from(window.appearance());
        state.update(cx, |state, cx| state.system_appearance_changed(mode, cx));
        Self {
            focus,
            state,
            inputs,
            fields_plugin: None,
            field_specs: Default::default(),
            secret_labels: Default::default(),
            page_scroll: gpui::ScrollHandle::new(),
            shown_route: crate::route::Route::default(),
            _subscriptions: subscriptions,
        }
    }

    pub fn inputs(&self) -> &Inputs {
        &self.inputs
    }

    pub fn state(&self) -> &Entity<AppState> {
        &self.state
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Makes the open run form's text, secret and model fields, then applies the text
    /// state asked for (`AppState::set_input`).
    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (plugin, wanted) = {
            let state = self.state.read(cx);
            let tag = state.language().tag();
            let info = state.run_form().and_then(|form| state.plugin(&form.plugin));
            let wanted: Vec<(String, String, InputKind)> = info
                .map(|info| {
                    info.manifest
                        .inputs
                        .iter()
                        .filter(|input| {
                            matches!(
                                input.kind,
                                InputKind::Text
                                    | InputKind::Secret
                                    | InputKind::HfModel
                                    | InputKind::Number
                                    | InputKind::File
                                    | InputKind::Dir
                                    | InputKind::Notebook
                                    | InputKind::Textarea
                            )
                        })
                        .map(|input| {
                            let label = input.label.get(tag).unwrap_or(&input.id).to_owned();
                            (input.id.clone(), label, input.kind)
                        })
                        .collect()
                })
                .unwrap_or_default();
            (info.map(|info| info.manifest.id.clone()), wanted)
        };
        if plugin.is_some() && plugin != self.fields_plugin {
            self.inputs.fields.clear();
            self.field_specs.clear();
            self.fields_plugin = plugin;
        }
        for (id, label, kind) in wanted {
            let spec = (label.clone(), kind);
            if self.field_specs.get(&id) == Some(&spec) {
                continue;
            }
            // New, or made for another label or kind (the language changed, or the plugin was
            // installed again under its id): made again, keeping what was typed into the
            // same kind of field.
            let typed = self
                .inputs
                .fields
                .get(&id)
                .filter(|_| self.field_specs.get(&id).is_some_and(|old| old.1 == kind))
                .map(|old| old.read(cx).text().to_owned());
            self.field_specs.insert(id.clone(), spec);
            let field = cx.new(|cx| {
                let input = TextInput::new(window, cx).label(label);
                let mut input = match kind {
                    InputKind::Secret => input.masked(),
                    // Numbers are typed as text: Ely's number field groups digits, and
                    // a port must never read "8,000".
                    InputKind::Number => {
                        input.filter(|c| c.is_ascii_digit() || c == '.' || c == '-')
                    }
                    _ => input,
                };
                if let Some(typed) = typed {
                    input.set_text(typed, cx);
                }
                input
            });
            self.inputs.fields.insert(id, field);
        }
        let pending = self
            .state
            .update(cx, |state, _| state.take_pending_inputs());
        for pending in pending {
            if let Some(input) = self.inputs.slot(&pending.slot) {
                input.update(cx, |input, cx| {
                    let shown = input.text().trim();
                    let typed = pending
                        .replaces
                        .is_some_and(|replaces| !shown.is_empty() && shown != replaces);
                    if !typed {
                        input.set_text(pending.text, cx);
                    }
                });
            }
        }
    }

    /// Masked fields for named plugin secret slots. Their ids cannot collide with run-form
    /// inputs, and clearing a run form does not clear them.
    fn sync_secret_slots(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted: Vec<(String, String)> = {
            let state = self.state.read(cx);
            let Some(list) = state.plugins().ready() else {
                return;
            };
            let tag = state.language().tag();
            list.iter()
                .flat_map(|info| {
                    let plugin = info.manifest.id.clone();
                    info.manifest.secrets.iter().map(move |slot| {
                        let key = format!("secret-slot/{plugin}/{}", slot.id);
                        let label = slot.label.get(tag).unwrap_or(&slot.id).to_owned();
                        (key, label)
                    })
                })
                .collect()
        };
        let keep: std::collections::BTreeSet<_> =
            wanted.iter().map(|(key, _)| key.clone()).collect();
        self.inputs.secret_slots.retain(|key, _| keep.contains(key));
        self.secret_labels.retain(|key, _| keep.contains(key));
        for (key, label) in wanted {
            if self.secret_labels.get(&key) == Some(&label)
                && self.inputs.secret_slots.contains_key(&key)
            {
                continue;
            }
            let field = cx.new(|cx| TextInput::new(window, cx).label(label.clone()).masked());
            self.secret_labels.insert(key.clone(), label);
            self.inputs.secret_slots.insert(key, field);
        }
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_inputs(window, cx);
        self.sync_secret_slots(window, cx);
        // A new page starts at its top, not at the previous page's scroll offset.
        let route = self.state.read(cx).route();
        if route != self.shown_route {
            self.shown_route = route;
            self.page_scroll.set_offset(gpui::Point::default());
        }
        let chrome = platform::chrome(window.window_decorations());
        let title_bar = titlebar::render(chrome, window, cx);
        let handle = self.state.clone();
        let state = self.state.read(cx);
        let theme = cx.theme();
        #[expect(
            clippy::disallowed_methods,
            reason = "the title strip sits outside Ely's `AppShell`, which grounds only itself, so the window grounds the strip"
        )]
        let mut root = FocusScope::new(&self.focus)
            .root()
            .size_full()
            .flex()
            .flex_col()
            // Client-drawn on Linux, Ely's resize border grounds a rounded pane instead.
            .when(chrome != Chrome::LinuxClient, |root| {
                root.bg(theme.colors.bg)
            })
            .text_color(theme.colors.fg)
            .font_family(theme.font_family.clone())
            .text_size(theme.text_size(TextSize::Base));
        // Han characters come from the platform's CJK faces; see `platform`.
        root.text_style().font_fallbacks = state.font_fallbacks().cloned();
        let root = root
            // Raised over dialog backdrops (Ely draws them at priority 0, full window), so the
            // title strip stays live while any dialog is open: one click on close runs the
            // close check, and minimise, maximise and dragging keep working. It keeps its place
            // in the layout; menus and toasts (also priority 1, raised later) still draw above it.
            .child(raise("title-strip", title_bar).with_priority(TITLE_STRIP_PRIORITY))
            .child(
                div().flex_1().min_h_0().w_full().child(
                    AppShell::new()
                        .sidebar(sidebar::render(state, &handle, cx))
                        .child(
                            tagged(
                                PAGE_SELECTOR,
                                ScrollArea::new(PAGE_SELECTOR)
                                    .vertical()
                                    .track_scroll(&self.page_scroll)
                                    .size_full()
                                    .child(pages::render(state, &handle, &self.inputs, cx)),
                            )
                            .flex_1()
                            .min_h_0()
                            .w_full(),
                        ),
                ),
            )
            .children(dialogs::render(state, &handle, &self.inputs, cx))
            .children(overlays::render(state, &handle, &self.inputs, cx))
            .child(ToastViewport::new("toasts", state.toaster()));
        ResizeBorder::new().child(root)
    }
}
