//! Keep Plan configuration, on-demand sync, Google Drive OAuth mount, and VM file upload.

use colab_playground_core::{
    Error, ErrorKind,
    backend::{DriveMountFlow, DriveMountStart, RuntimeRef},
    keep::{
        DEFAULT_DRIVE_DEST_SUBPATH, DEFAULT_DRIVE_MOUNT_PATH, DriveKeepTarget, HfKeepTarget,
        KeepPlan, LocalKeepTarget,
    },
};
use gpui::{AppContext, Context};

use super::{AppState, InputSlot, Op, Overlay, ops::key_of, runtime_label};
use crate::i18n::{keys, tr, tr_args};

#[derive(Clone, Debug, PartialEq)]
pub struct KeepPlanDialog {
    /// `None` when configuring Keep Plan on the Plugin Run Form before deploy.
    pub target: Option<RuntimeRef>,
    pub label: String,
    pub local_enabled: bool,
    pub hf_enabled: bool,
    pub drive_enabled: bool,
    pub drive_mounted: bool,
    pub error: Option<Error>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeepPlanInputs {
    pub paths: String,
    pub local_folder: String,
    pub hf_repo: String,
    pub hf_subpath: String,
    pub hf_interval: String,
    pub drive_mount: String,
    pub drive_subpath: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DriveMountPhase {
    Starting,
    Consent { url: String },
    Confirming { url: String },
    Failed { error: Error },
}

#[derive(Clone, Debug, PartialEq)]
pub struct DriveMountDialog {
    pub target: RuntimeRef,
    pub label: String,
    pub mount_path: String,
    pub phase: DriveMountPhase,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VmUploadDialog {
    pub target: RuntimeRef,
    pub label: String,
    pub error: Option<Error>,
}

fn seed_keep_inputs(state: &mut AppState, plan: Option<&KeepPlan>, default_folder: String) {
    let paths = plan.map(|p| p.paths.join("\n")).unwrap_or_default();
    let local_folder = plan
        .and_then(|p| p.local.as_ref())
        .map(|l| l.folder.clone())
        .unwrap_or(default_folder);
    let hf_repo = plan
        .and_then(|p| p.hf.as_ref())
        .map(|h| h.repo.clone())
        .unwrap_or_default();
    let hf_subpath = plan
        .and_then(|p| p.hf.as_ref())
        .and_then(|h| h.path_in_repo.clone())
        .unwrap_or_default();
    let hf_interval = plan
        .and_then(|p| p.hf.as_ref())
        .and_then(|h| h.interval_minutes)
        .map(|m| m.to_string())
        .unwrap_or_default();
    let drive_mount = plan
        .and_then(|p| p.drive.as_ref())
        .map(|d| d.mount_path.clone())
        .unwrap_or_else(|| DEFAULT_DRIVE_MOUNT_PATH.to_owned());
    let drive_subpath = plan
        .and_then(|p| p.drive.as_ref())
        .map(|d| d.dest_subpath.clone())
        .unwrap_or_else(|| DEFAULT_DRIVE_DEST_SUBPATH.to_owned());

    state.set_input(InputSlot::KeepPaths, paths);
    state.set_input(InputSlot::KeepLocalFolder, local_folder);
    state.set_input(InputSlot::KeepHfRepo, hf_repo);
    state.set_input(InputSlot::KeepHfSubpath, hf_subpath);
    state.set_input(InputSlot::KeepHfInterval, hf_interval);
    state.set_input(InputSlot::KeepDriveMount, drive_mount);
    state.set_input(InputSlot::KeepDriveSubpath, drive_subpath);
}

impl AppState {
    /// Opens the Keep Plan dialog for an existing managed session.
    pub fn open_keep_plan(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(runtime) = self.runtime(&target).cloned() else {
            return;
        };
        let plan = runtime.tracked.as_ref().and_then(|t| t.keep_plan.as_ref());
        let label = runtime_label(&runtime);
        let default_folder = super::runs::outputs_dir(&label)
            .to_string_lossy()
            .into_owned();
        seed_keep_inputs(self, plan, default_folder);
        self.overlay = Some(Overlay::KeepPlan(KeepPlanDialog {
            target: Some(target),
            label,
            local_enabled: plan.is_some_and(|p| p.local.is_some()),
            hf_enabled: plan.is_some_and(|p| p.hf.is_some()),
            drive_enabled: plan.and_then(|p| p.drive.as_ref()).is_some_and(|d| d.consented),
            drive_mounted: plan.and_then(|p| p.drive.as_ref()).is_some_and(|d| d.mounted),
            error: None,
        }));
        cx.notify();
    }

    /// Opens the Keep Plan dialog for the plugin run form before deploy.
    pub fn open_run_keep_plan(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.run_form.clone() else {
            return;
        };
        let label = self
            .plugin(&form.plugin)
            .map(|info| self.plugin_name(info))
            .unwrap_or_else(|| form.plugin.clone());
        let default_plan = self.plugin(&form.plugin).map(|info| KeepPlan {
            paths: info.manifest.keep.iter().map(|k| k.path.clone()).collect(),
            ..KeepPlan::default()
        });
        let seed = form.keep_plan.as_ref().or(default_plan.as_ref());
        let default_folder = super::runs::outputs_dir(&form.plugin)
            .to_string_lossy()
            .into_owned();
        seed_keep_inputs(self, seed, default_folder);
        self.overlay = Some(Overlay::KeepPlan(KeepPlanDialog {
            target: None,
            label,
            local_enabled: seed.is_some_and(|p| p.local.is_some()),
            hf_enabled: seed.is_some_and(|p| p.hf.is_some()),
            drive_enabled: seed.and_then(|p| p.drive.as_ref()).is_some_and(|d| d.consented),
            drive_mounted: false,
            error: None,
        }));
        cx.notify();
    }

    pub fn toggle_keep_local(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(Overlay::KeepPlan(dialog)) = &mut self.overlay {
            dialog.local_enabled = on;
            dialog.error = None;
            cx.notify();
        }
    }

    pub fn toggle_keep_hf(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(Overlay::KeepPlan(dialog)) = &mut self.overlay {
            dialog.hf_enabled = on;
            dialog.error = None;
            cx.notify();
        }
    }

    pub fn toggle_keep_drive(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(Overlay::KeepPlan(dialog)) = &mut self.overlay {
            dialog.drive_enabled = on;
            dialog.error = None;
            cx.notify();
        }
    }

    pub fn is_saving_keep_plan(&self, target: Option<&RuntimeRef>) -> bool {
        target.is_some_and(|t| self.ops.is_running(Op::KeepPlan(key_of(t))))
    }

    pub fn is_syncing_keep(&self, target: &RuntimeRef) -> bool {
        self.ops.is_running(Op::KeepSync(key_of(target)))
    }

    pub fn is_mounting_drive(&self, target: &RuntimeRef) -> bool {
        self.ops.is_running(Op::DriveMount(key_of(target)))
    }

    pub fn is_uploading(&self, target: &RuntimeRef) -> bool {
        self.ops.is_running(Op::VmUpload(key_of(target)))
    }

    pub fn submit_keep_plan(&mut self, draft: KeepPlanInputs, cx: &mut Context<Self>) {
        let Some(Overlay::KeepPlan(dialog)) = self.overlay.clone() else {
            return;
        };
        let parsed = build_keep_plan(&dialog, &draft);
        let plan = match parsed {
            Ok(plan) => plan,
            Err(error) => {
                if let Some(Overlay::KeepPlan(d)) = &mut self.overlay {
                    d.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        let Some(target) = dialog.target else {
            if let Some(form) = &mut self.run_form {
                form.keep_plan = plan;
            }
            self.overlay = None;
            self.toast_done(tr(cx, keys::KEEP_RUN_FORM_SAVED), cx);
            cx.notify();
            return;
        };
        let label = dialog.label;
        let work_target = target.clone();
        self.run_backend(
            Op::KeepPlan(key_of(&target)),
            cx,
            move |backend, cancel| backend.set_keep_plan(&work_target, plan, cancel),
            move |state, result, cx| {
                let open =
                    matches!(&state.overlay, Some(Overlay::KeepPlan(d)) if d.target.as_ref() == Some(&target));
                match result {
                    Ok(()) => {
                        if open {
                            state.overlay = None;
                        }
                        state.toast_done(tr_args(cx, keys::KEEP_SAVED, &[("name", &label)]), cx);
                        state.load_runtimes(cx);
                    }
                    Err(error) => {
                        if open && let Some(Overlay::KeepPlan(d)) = &mut state.overlay {
                            d.error = Some(error);
                        } else {
                            state.toast_error(&error, cx);
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    /// Runs the session's configured Keep Plan immediately without stopping it.
    pub fn sync_keep(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(runtime) = self.runtime(&target) else {
            return;
        };
        let label = runtime_label(runtime);
        let work_target = target.clone();
        self.run_backend(
            Op::KeepSync(key_of(&target)),
            cx,
            move |backend, cancel| backend.sync_keep(&work_target, None, cancel),
            move |state, result, cx| {
                match result {
                    Ok(report) => {
                        let files: u64 = report.results.iter().map(|(_, r)| r.files).sum();
                        let count = files.to_string();
                        let body = tr_args(
                            cx,
                            keys::KEEP_SYNC_DONE,
                            &[("files", &count), ("name", &label)],
                        );
                        state.toast_done(body, cx);
                    }
                    Err(error) => {
                        log::warn!("keep: sync failed for {label}: {error}");
                        state.toast_error(&error, cx);
                    }
                }
                state.load_runtimes(cx);
                cx.notify();
            },
        );
    }

    /// Starts Google Drive mount (`colab drivemount`), opening the interactive consent dialog when
    /// Drive requires OAuth authorization.
    pub fn begin_drive_mount(
        &mut self,
        target: RuntimeRef,
        mount_path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.drive_mount_flow.is_some() || self.is_mounting_drive(&target) {
            return;
        }
        let Some(runtime) = self.runtime(&target) else {
            return;
        };
        let label = runtime_label(runtime);
        let mount_path = mount_path
            .or_else(|| {
                runtime
                    .tracked
                    .as_ref()
                    .and_then(|t| t.keep_plan.as_ref())
                    .and_then(|p| p.drive.as_ref())
                    .map(|d| d.mount_path.clone())
            })
            .unwrap_or_else(|| DEFAULT_DRIVE_MOUNT_PATH.to_owned());
        self.overlay = Some(Overlay::DriveMount(DriveMountDialog {
            target: target.clone(),
            label: label.clone(),
            mount_path: mount_path.clone(),
            phase: DriveMountPhase::Starting,
        }));
        let work_target = target.clone();
        let work_path = mount_path;
        self.run_backend(
            Op::DriveMount(key_of(&target)),
            cx,
            move |backend, cancel| backend.begin_drive_mount(&work_target, &work_path, cancel),
            move |state, result, cx| match result {
                Ok(DriveMountStart::Mounted) => {
                    if matches!(&state.overlay, Some(Overlay::DriveMount(d)) if d.target == target)
                    {
                        state.overlay = None;
                    }
                    state.toast_done(
                        tr_args(cx, keys::KEEP_DRIVE_MOUNTED_TOAST, &[("name", &label)]),
                        cx,
                    );
                    state.load_runtimes(cx);
                }
                Ok(DriveMountStart::NeedsConsent(flow)) => {
                    let still_starting = matches!(
                        &state.overlay,
                        Some(Overlay::DriveMount(d))
                            if d.target == target && matches!(d.phase, DriveMountPhase::Starting)
                    );
                    if !still_starting {
                        state.cancel_drive_flow_bg(flow, cx);
                        return;
                    }
                    let url = flow.auth_url().to_owned();
                    state.drive_mount_flow = Some(flow);
                    if let Some(Overlay::DriveMount(d)) = &mut state.overlay {
                        d.phase = DriveMountPhase::Consent { url: url.clone() };
                    }
                    cx.open_url(&url);
                }
                Err(error) if error.is(ErrorKind::Cancelled) => {}
                Err(error) => {
                    if let Some(Overlay::DriveMount(d)) = &mut state.overlay
                        && d.target == target
                    {
                        d.phase = DriveMountPhase::Failed { error };
                    }
                    state.load_runtimes(cx);
                }
            },
        );
    }

    pub fn confirm_drive_mount(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::DriveMount(dialog)) = self.overlay.clone() else {
            return;
        };
        let DriveMountPhase::Consent { url } = dialog.phase else {
            return;
        };
        let Some(flow) = self.drive_mount_flow.take() else {
            return;
        };
        let target = dialog.target;
        let label = dialog.label;
        if let Some(Overlay::DriveMount(d)) = &mut self.overlay {
            d.phase = DriveMountPhase::Confirming { url };
        }
        self.run_backend(
            Op::DriveMount(key_of(&target)),
            cx,
            move |_, cancel| flow.confirm(cancel),
            move |state, result, cx| match result {
                Ok(()) => {
                    if matches!(&state.overlay, Some(Overlay::DriveMount(d)) if d.target == target)
                    {
                        state.overlay = None;
                    }
                    state.toast_done(
                        tr_args(cx, keys::KEEP_DRIVE_MOUNTED_TOAST, &[("name", &label)]),
                        cx,
                    );
                    state.load_runtimes(cx);
                }
                Err(error) => {
                    if let Some(Overlay::DriveMount(d)) = &mut state.overlay
                        && d.target == target
                    {
                        d.phase = DriveMountPhase::Failed { error };
                    }
                    state.load_runtimes(cx);
                }
            },
        );
    }

    pub fn close_drive_mount(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::DriveMount(dialog)) = self.overlay.take() else {
            return;
        };
        let op = Op::DriveMount(key_of(&dialog.target));
        if self.ops.is_running(op) {
            self.ops.cancel(op);
        }
        if let Some(flow) = self.drive_mount_flow.take() {
            self.cancel_drive_flow_bg(flow, cx);
        }
        cx.notify();
    }

    fn cancel_drive_flow_bg(&mut self, flow: Box<dyn DriveMountFlow>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let res = cx.background_spawn(async move { flow.cancel() }).await;
            if let Err(error) = res {
                log::warn!("keep: cancelling Drive consent failed: {error}");
            }
            this.update(cx, |state, cx| state.load_runtimes(cx)).ok();
        })
        .detach();
    }

    /// Opens the VM file upload dialog for `target`.
    pub fn open_upload_dialog(&mut self, target: RuntimeRef, cx: &mut Context<Self>) {
        let Some(runtime) = self.runtime(&target) else {
            return;
        };
        let label = runtime_label(runtime);
        self.set_input(InputSlot::UploadLocal, String::new());
        self.set_input(InputSlot::UploadRemote, "/content/".to_owned());
        self.overlay = Some(Overlay::VmUpload(VmUploadDialog {
            target,
            label,
            error: None,
        }));
        cx.notify();
    }

    pub fn submit_upload(&mut self, local: &str, remote: &str, cx: &mut Context<Self>) {
        let Some(Overlay::VmUpload(dialog)) = self.overlay.clone() else {
            return;
        };
        let local = local.trim().to_owned();
        let mut remote = remote.trim().to_owned();
        if local.is_empty() {
            if let Some(Overlay::VmUpload(d)) = &mut self.overlay {
                d.error = Some(Error::invalid("local_path", "enter a file on this computer"));
            }
            cx.notify();
            return;
        }
        if remote.is_empty()
            || remote.split('/').any(|s| s == "..")
            || remote.chars().any(|c| c.is_control())
        {
            if let Some(Overlay::VmUpload(d)) = &mut self.overlay {
                d.error = Some(Error::invalid(
                    "remote_path",
                    "enter a VM destination path without `..`",
                ));
            }
            cx.notify();
            return;
        }
        if remote.ends_with('/') {
            let name = std::path::Path::new(&local)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "upload.bin".to_owned());
            remote.push_str(&name);
        }
        let target = dialog.target;
        let label = dialog.label;
        let work_target = target.clone();
        let shown_remote = remote.clone();
        self.run_backend(
            Op::VmUpload(key_of(&target)),
            cx,
            move |backend, cancel| backend.vm_upload(&work_target, &local, &remote, cancel),
            move |state, result, cx| {
                let open = matches!(&state.overlay, Some(Overlay::VmUpload(d)) if d.target == target);
                match result {
                    Ok(()) => {
                        if open {
                            state.overlay = None;
                        }
                        state.toast_done(
                            tr_args(
                                cx,
                                keys::VM_UPLOAD_DONE,
                                &[("path", &shown_remote), ("name", &label)],
                            ),
                            cx,
                        );
                    }
                    Err(error) => {
                        if open && let Some(Overlay::VmUpload(d)) = &mut state.overlay {
                            d.error = Some(error);
                        } else {
                            state.toast_error(&error, cx);
                        }
                    }
                }
                cx.notify();
            },
        );
    }
}

fn build_keep_plan(
    dialog: &KeepPlanDialog,
    draft: &KeepPlanInputs,
) -> colab_playground_core::Result<Option<KeepPlan>> {
    let paths: Vec<String> = draft
        .paths
        .split(|c| c == '\n' || c == ',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if !dialog.local_enabled && !dialog.hf_enabled && !dialog.drive_enabled && paths.is_empty() {
        return Ok(None);
    }
    let local = dialog.local_enabled.then(|| LocalKeepTarget {
        folder: draft.local_folder.trim().to_owned(),
    });
    let hf = if dialog.hf_enabled {
        let subpath = draft.hf_subpath.trim();
        let interval_text = draft.hf_interval.trim();
        let interval_minutes = if interval_text.is_empty() {
            None
        } else {
            let mins = interval_text.parse::<u32>().map_err(|_| {
                Error::invalid("hf_interval_minutes", "must be between 1 and 1440 minutes")
                    .with("value", interval_text)
            })?;
            Some(mins)
        };
        Some(HfKeepTarget {
            repo: draft.hf_repo.trim().to_owned(),
            path_in_repo: (!subpath.is_empty()).then(|| subpath.to_owned()),
            interval_minutes,
        })
    } else {
        None
    };
    let drive = dialog.drive_enabled.then(|| DriveKeepTarget {
        mount_path: draft.drive_mount.trim().to_owned(),
        dest_subpath: draft.drive_subpath.trim().to_owned(),
        consented: true,
        mounted: dialog.drive_mounted,
    });
    let plan = KeepPlan {
        paths,
        local,
        hf,
        drive,
    };
    if plan.has_enabled_target() && plan.paths.is_empty() {
        return Err(Error::invalid(
            "keep_paths",
            "enter at least one VM path to keep",
        ));
    }
    plan.validate()?;
    Ok(Some(plan))
}
