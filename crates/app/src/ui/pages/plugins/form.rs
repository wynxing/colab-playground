//! The run form, generated from the manifest: text, number, switch, choice, secret and
//! Hugging Face model inputs (with search, file listing and a quantisation preview for the
//! chosen minimum GPU memory), where to run (a new session with the same options as the New
//! session dialog, or an existing one), and whether to stop on failure.

use std::rc::Rc;

use colab_playground_core::{
    plugins::{InputKind, InputSpec, InputValue, PluginInfo},
    quant::{self, FitInput, GgufListing},
};
use ely_gpui_component::{
    buttons::{Button, ButtonVariant},
    feedback::InlineMessage,
    forms::{Choice, Input, Select, Switch},
    layout::{Card, CardHeader, Page, Well},
    lists::ListItem,
    primitives::{IconName, Severity},
    theme::{ActiveTheme, TextSize},
    typography::{Caption, Paragraph},
};
use gpui::{
    AnyElement, App, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, div,
};

use crate::{
    format,
    i18n::{keys, tr, tr_args},
    state::{AppState, Load, RunForm, TargetLost},
    ui::{Inputs, error_view, parts::field, provision_form, runtime_options::MIB_PER_GIB, tagged},
};

pub const RUN_START: &str = "run-start";
pub const RUN_KEEP_CONFIGURE: &str = "run-keep-configure";
/// Selector of a Hugging Face search result: `hf-result <repo>`.
pub fn hf_result_selector(repo: &str) -> String {
    format!("hf-result {repo}")
}

fn label(input: &InputSpec, tag: &str) -> String {
    input.label.get(tag).unwrap_or(&input.id).to_owned()
}

pub(super) fn render(
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> Page {
    let Some(form) = state.run_form() else {
        return Page::new("");
    };
    let Some(info) = state.plugin(&form.plugin) else {
        return Page::new("");
    };
    let tag = state.language().tag();
    let name = state.plugin_name(info);
    let back = handle.clone();
    let mut page = Page::new(name).action(
        Button::new("run-form-back", tr(cx, keys::ACTION_BACK))
            .variant(ButtonVariant::Ghost)
            .icon(IconName::ArrowLeft)
            .on_click(move |_, _, cx| back.update(cx, |state, cx| state.close_run_form(cx))),
    );
    let fields = info
        .manifest
        .inputs
        .iter()
        .map(|input| input_row(input, form, info, state, handle, inputs, cx));
    if let Some(description) = info.manifest.description.get(tag) {
        page = page.child(Paragraph::new(description.to_owned()));
    }
    page = page.child(
        Card::new()
            .header(CardHeader::new(tr(cx, keys::RUN_INPUTS)))
            .child(div().flex().flex_col().gap_3().children(fields))
            .w_full(),
    );
    if AppState::wants_hf_token(info) {
        let text = match state.hf_token() {
            Load::Ready(true) => tr(cx, keys::RUN_HF_TOKEN_SENT),
            _ => tr(cx, keys::RUN_HF_TOKEN_NONE),
        };
        page = page.child(InlineMessage::new(Severity::Info, text));
    }
    page = page.child(target_card(form, state, handle, inputs, cx));
    page = page.child(run_keep_card(form, info, handle, cx));
    if let Some(error) = &form.error {
        page = page.child(error_view::alert("run-form", None, error, None, handle, cx));
    }
    let start = {
        let handle = handle.clone();
        let inputs = inputs.clone();
        let ids: Vec<(String, bool)> = info
            .manifest
            .inputs
            .iter()
            .filter(|i| {
                matches!(
                    i.kind,
                    InputKind::Text | InputKind::Secret | InputKind::HfModel | InputKind::Number
                )
            })
            .map(|i| (i.id.clone(), i.kind == InputKind::Secret))
            .collect();
        let running = state.is_running_plugin();
        // A new session would only fail at the cap: say so before Deploy, like New session.
        let limited = form.existing.is_none() && state.session_limit_reached().is_some();
        let ineligible = form.existing.is_none()
            && state
                .hardware_catalog()
                .blocks(form.provision.accelerator, form.provision.high_mem);
        tagged(
            RUN_START,
            Button::new(RUN_START, tr(cx, keys::RUN_START))
                .primary()
                .icon(IconName::Rocket)
                .disabled(running || limited || ineligible)
                .on_click(move |_, _, cx| {
                    let mut texts = Vec::new();
                    for (id, _) in &ids {
                        if let Some(field) = inputs.field(id) {
                            texts.push((id.clone(), field.read(cx).text().to_owned()));
                        }
                    }
                    let name = inputs.run_session_name.read(cx).text().to_owned();
                    if !handle.update(cx, |state, cx| state.start_run(texts, name, cx)) {
                        return;
                    }
                    for (id, secret) in &ids {
                        if *secret && let Some(field) = inputs.field(id) {
                            field.update(cx, |input, cx| input.set_text(String::new(), cx));
                        }
                    }
                }),
        )
    };
    page.child(
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(start)
            .child(Caption::new(tr(cx, keys::RUN_START_NOTE))),
    )
}

fn input_row(
    input: &InputSpec,
    form: &RunForm,
    info: &PluginInfo,
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let tag = state.language().tag();
    let title = {
        let mut text = label(input, tag);
        if input.required {
            text.push_str(" *");
        }
        text
    };
    let control: AnyElement = match input.kind {
        InputKind::Text | InputKind::Secret | InputKind::HfModel | InputKind::Number => {
            match inputs.field(&input.id) {
                Some(field) => Input::new(&field).into_any_element(),
                None => Caption::new("").into_any_element(),
            }
        }
        InputKind::Bool => {
            let on = matches!(form.values.get(&input.id), Some(InputValue::Bool(true)));
            let (handle, id) = (handle.clone(), input.id.clone());
            Switch::new(SharedString::from(format!("run-input-{}", input.id)), on)
                .name(label(input, tag))
                .on_change(move |on, _, cx| {
                    let id = id.clone();
                    handle.update(cx, |state, cx| {
                        state.edit_run_form(
                            |form| {
                                form.values.insert(id, InputValue::Bool(on));
                            },
                            cx,
                        )
                    })
                })
                .into_any_element()
        }
        InputKind::Select => {
            let selected = match form.values.get(&input.id) {
                Some(InputValue::Text(t)) => t.clone(),
                _ => input.options.first().cloned().unwrap_or_default(),
            };
            let (handle, id) = (handle.clone(), input.id.clone());
            div()
                .w(theme.menu_width())
                .child(
                    Select::new(
                        SharedString::from(format!("run-input-{}", input.id)),
                        input
                            .options
                            .iter()
                            .map(|o| Choice::new(o.clone(), o.clone())),
                    )
                    .label(label(input, tag))
                    .selected(selected)
                    .on_change(move |value, _, cx| {
                        let id = id.clone();
                        handle.update(cx, |state, cx| {
                            state.edit_run_form(
                                |form| {
                                    form.values.insert(id, InputValue::Text(value.to_string()));
                                },
                                cx,
                            )
                        })
                    }),
                )
                .into_any_element()
        }
    };
    let mut row = field(
        title,
        tagged(format!("run-input {}", input.id), control),
        cx,
    )
    .children(
        input
            .help
            .get(tag)
            .map(|help| Caption::new(help.to_owned())),
    );
    if input.kind == InputKind::Secret {
        row = row.child(Caption::new(tr(cx, keys::RUN_SECRET_NOTE)));
    }
    if input.kind == InputKind::HfModel {
        row = row.child(hf_lookup(input, form, info, handle, inputs, cx));
    }
    row.into_any_element()
}

/// Search, results, and the quantisation preview for the chosen repo.
fn hf_lookup(
    input: &InputSpec,
    form: &RunForm,
    info: &PluginInfo,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> AnyElement {
    let (search, query) = (handle.clone(), inputs.hf_query.clone());
    let (list, field) = (handle.clone(), inputs.field(&input.id));
    let mut lookup = Well::new().flex().flex_col().gap_2().child(
        div()
            .flex()
            .items_end()
            .gap_2()
            .child(div().flex_1().child(Input::new(&inputs.hf_query)))
            .child(tagged(
                "hf-search",
                Button::new("hf-search", tr(cx, keys::HF_SEARCH_ACTION))
                    .icon(IconName::Search)
                    .on_click(move |_, _, cx| {
                        let text = query.read(cx).text().to_owned();
                        search.update(cx, |state, cx| state.search_hf(text, cx))
                    }),
            ))
            .child(tagged(
                "hf-list-files",
                Button::new("hf-list-files", tr(cx, keys::HF_LIST_FILES))
                    .variant(ButtonVariant::Ghost)
                    .on_click(move |_, _, cx| {
                        if let Some(field) = &field {
                            let repo = field.read(cx).text().to_owned();
                            list.update(cx, |state, cx| state.load_hf_files(repo, cx));
                        }
                    }),
            )),
    );
    match &form.hf.results {
        Load::Loading => lookup = lookup.child(Caption::new(tr(cx, keys::HF_SEARCHING))),
        Load::Failed(error) => {
            lookup = lookup.child(InlineMessage::new(
                Severity::Warning,
                crate::errors::explain(error, false, cx).message,
            ))
        }
        Load::Ready(models) if models.is_empty() => {
            lookup = lookup.child(Caption::new(tr(cx, keys::HF_NO_RESULTS)))
        }
        Load::Ready(models) => {
            let rows = models.iter().map(|model| {
                let (handle, repo, id) = (handle.clone(), model.id.clone(), input.id.clone());
                tagged(
                    hf_result_selector(&model.id),
                    ListItem::new(
                        SharedString::from(format!("hf-result-{}", model.id)),
                        model.id.clone(),
                    )
                    .trailing(Caption::new(tr_args(
                        cx,
                        if model.gated {
                            keys::HF_RESULT_GATED
                        } else {
                            keys::HF_RESULT
                        },
                        &[("downloads", &format::number(cx, model.downloads as f64, 0))],
                    )))
                    .on_click(move |_, _, cx| {
                        let (id, repo) = (id.clone(), repo.clone());
                        handle.update(cx, |state, cx| state.choose_hf_repo(id, repo, cx))
                    }),
                )
            });
            lookup = lookup.child(div().flex().flex_col().children(rows));
        }
        Load::Idle => {}
    }
    match &form.hf.files {
        Load::Loading => lookup = lookup.child(Caption::new(tr(cx, keys::HF_LISTING))),
        Load::Failed(error) => {
            lookup = lookup.child(InlineMessage::new(
                Severity::Warning,
                crate::errors::explain(error, false, cx).message,
            ))
        }
        Load::Ready(listing) => {
            let ctx = inputs
                .field(quant::INPUT_CTX)
                .and_then(|field| field.read(cx).text().trim().parse::<f64>().ok());
            lookup = lookup.child(quant_preview(listing, form, info, ctx, cx))
        }
        Load::Idle => {}
    }
    tagged(format!("hf-lookup {}", input.id), lookup).into_any_element()
}

/// Which quantisation fits: for the minimum GPU memory when one is set, else for each size
/// the chosen accelerator comes in.
fn quant_preview(
    listing: &GgufListing,
    form: &RunForm,
    info: &PluginInfo,
    ctx: Option<f64>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let sizes: Vec<u32> = match form.provision.min_vram_mib {
        Some(mib) => vec![mib],
        None => quant::vram_choices_gib(form.provision.accelerator)
            .iter()
            .map(|gib| gib * MIB_PER_GIB)
            .collect(),
    };
    let ctx = ctx.filter(|n| *n > 0.0).map_or(0, |n| n as u32);
    let fixed = match form.values.get(quant::INPUT_QUANT) {
        Some(InputValue::Text(q))
            if q != quant::QUANT_AUTO && info.manifest.input(quant::INPUT_QUANT).is_some() =>
        {
            Some(q.clone())
        }
        _ => None,
    };
    let variants: Vec<String> = listing
        .variants
        .iter()
        .map(|v| format!("{} ({})", v.quant, format::bytes(cx, v.size)))
        .collect();
    let mut rows = div()
        .flex()
        .flex_col()
        .gap_1()
        .text_size(theme.text_size(TextSize::Sm))
        .child(Caption::new(tr_args(
            cx,
            keys::QUANT_FILES,
            &[("list", &format::list(cx, &variants))],
        )));
    if sizes.is_empty() {
        rows = rows.child(Caption::new(tr(cx, keys::QUANT_NO_GPU)));
    }
    for mib in sizes {
        let input = FitInput {
            vram_mib: mib,
            ctx,
            model_ctx: None,
            kv_bytes_per_token: None,
            mmproj_bytes: 0,
            kv: None,
        };
        let fit = match &fixed {
            Some(q) => quant::fit_fixed(listing, q, &input),
            None => quant::fit_auto(listing, &input),
        };
        let gpu = format::vram(cx, mib);
        let line = match fit {
            Ok(choice) => tr_args(
                cx,
                // With context 0 the run uses the model's own maximum, which is only known on
                // the runtime; the preview assumes a context, and says so.
                if ctx == 0 {
                    keys::QUANT_FITS_ASSUMED
                } else {
                    keys::QUANT_FITS
                },
                &[
                    ("gpu", &gpu),
                    ("quant", &choice.quant),
                    ("ctx", &format::number(cx, f64::from(choice.ctx), 0)),
                    ("estimate", &format::vram(cx, choice.estimate_mib)),
                ],
            ),
            Err(_) => tr_args(cx, keys::QUANT_NOTHING_FITS, &[("gpu", &gpu)]),
        };
        rows = rows.child(tagged(format!("quant-preview {mib}"), div().child(line)));
    }
    rows.child(Caption::new(tr(cx, keys::QUANT_ESTIMATE_NOTE)))
        .into_any_element()
}

/// Where to run: a new session (its options and name) or an existing one.
fn target_card(
    form: &RunForm,
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let existing: Vec<String> = state
        .runtimes()
        .ready()
        .map(|view| {
            view.runtimes
                .iter()
                .filter(|r| r.on_active_account && r.managed)
                .filter_map(|r| r.id.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut choices = vec![Choice::new("", tr(cx, keys::RUN_TARGET_NEW))];
    choices.extend(
        existing
            .iter()
            .map(|name| Choice::new(name.clone(), name.clone())),
    );
    let pick = handle.clone();
    let target = div().w(theme.menu_width()).child(
        Select::new("run-target", choices)
            .label(tr(cx, keys::RUN_TARGET))
            .selected(form.existing.clone().unwrap_or_default())
            .on_change(move |value, _, cx| {
                let existing = (!value.is_empty()).then(|| value.to_string());
                pick.update(cx, |state, cx| state.pick_run_target(existing, cx))
            }),
    );
    let mut card = Card::new()
        .header(
            CardHeader::new(tr(cx, keys::RUN_TARGET)).description(tr(cx, keys::RUN_TARGET_HINT)),
        )
        .child(tagged("run-target", target));
    if let Some(lost) = &form.target_lost {
        let (selector, key, name) = match lost {
            TargetLost::Ended(name) => ("run-target-ended", keys::RUN_TARGET_ENDED, name),
            TargetLost::OtherAccount(name) => (
                "run-target-other-account",
                keys::RUN_TARGET_OTHER_ACCOUNT,
                name,
            ),
        };
        card = card.child(tagged(
            selector,
            InlineMessage::new(Severity::Warning, tr_args(cx, key, &[("name", name)])),
        ));
    }
    if form.existing.is_none()
        && let Some(limit) = state.session_limit_reached()
    {
        let email = format::email(cx, limit.email.as_deref());
        let settings = handle.clone();
        card = card.child(tagged(
            "run-limit",
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(InlineMessage::new(
                    Severity::Warning,
                    tr_args(
                        cx,
                        keys::NEW_LIMIT_BODY,
                        &[("limit", &limit.limit.to_string()), ("email", &email)],
                    ),
                ))
                .child(Caption::new(tr(cx, keys::RUN_LIMIT_HINT)))
                .child(div().flex().child(
                    Button::new("run-limit-settings", tr(cx, keys::NEW_OPEN_SETTINGS)).on_click(
                        move |_, _, cx| {
                            settings.update(cx, |state, cx| state.open_limit_setting(cx))
                        },
                    ),
                )),
        ));
    }
    if form.existing.is_none() {
        let edit: provision_form::SpecEdit = {
            let handle = handle.clone();
            Rc::new(move |cx: &mut App, f: &dyn Fn(&mut colab_playground_core::provision::ProvisionSpec)| {
                handle.update(cx, |state, cx| state.edit_run_form(|form| f(&mut form.provision), cx))
            })
        };
        let stop = handle.clone();
        card = card
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(theme.text_size(TextSize::Sm))
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr(cx, keys::NEW_NAME)),
                    )
                    .child(Input::new(&inputs.run_session_name)),
            )
            .child(provision_form::render(
                "run",
                &form.provision,
                state.measured_rate(&form.provision),
                edit,
                None,
                state
                    .hardware_catalog()
                    .blocks(form.provision.accelerator, form.provision.high_mem),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(tagged(
                        "run-stop-on-failure",
                        Switch::new("run-stop-on-failure", form.stop_on_failure)
                            .label(tr(cx, keys::RUN_STOP_ON_FAILURE))
                            .on_change(move |on, _, cx| {
                                stop.update(cx, |state, cx| {
                                    state.edit_run_form(|form| form.stop_on_failure = on, cx)
                                })
                            }),
                    ))
                    .child(Caption::new(tr(
                        cx,
                        if form.stop_on_failure {
                            keys::RUN_STOP_ON_FAILURE_ON
                        } else {
                            keys::RUN_STOP_ON_FAILURE_OFF
                        },
                    ))),
            );
    } else {
        card = card.child(Caption::new(tr(cx, keys::RUN_EXISTING_NOTE)));
    }
    card.w_full().into_any_element()
}

fn run_keep_card(
    form: &RunForm,
    info: &PluginInfo,
    handle: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let summary = match &form.keep_plan {
        Some(plan) if !plan.is_empty() => {
            let mut targets = Vec::new();
            if plan.local.is_some() {
                targets.push(tr(cx, keys::KEEP_TARGET_LOCAL).to_string());
            }
            if plan.hf.is_some() {
                targets.push(tr(cx, keys::KEEP_TARGET_HF).to_string());
            }
            if plan.drive.as_ref().is_some_and(|d| d.consented) {
                targets.push(tr(cx, keys::KEEP_TARGET_DRIVE).to_string());
            }
            let targets_joined = if targets.is_empty() {
                "—".to_owned()
            } else {
                targets.join(", ")
            };
            let count = plan.paths.len().to_string();
            Caption::new(tr_args(
                cx,
                keys::KEEP_CARD_SUMMARY,
                &[("paths", &count), ("targets", &targets_joined)],
            ))
        }
        _ if !info.manifest.keep.is_empty() => {
            let count = info.manifest.keep.len().to_string();
            Caption::new(tr_args(
                cx,
                keys::KEEP_CARD_SUMMARY,
                &[("paths", &count), ("targets", "—")],
            ))
        }
        _ => Caption::new(tr(cx, keys::KEEP_CARD_NONE)),
    };
    let configure = handle.clone();
    Card::new()
        .header(CardHeader::new(tr(cx, keys::KEEP_TITLE)).description(tr(cx, keys::KEEP_SUBTITLE)))
        .child(summary)
        .child(
            div().flex().child(tagged(
                RUN_KEEP_CONFIGURE,
                Button::new(RUN_KEEP_CONFIGURE, tr(cx, keys::KEEP_CONFIGURE_ACTION))
                    .variant(ButtonVariant::Ghost)
                    .icon(IconName::Settings)
                    .on_click(move |_, _, cx| {
                        configure.update(cx, |state, cx| state.open_run_keep_plan(cx))
                    }),
            )),
        )
        .w_full()
        .into_any_element()
}
