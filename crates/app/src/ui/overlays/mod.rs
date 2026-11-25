//! Dialogs other than the account ones: stop and end billing, new session (with its live
//! progress), forward a port, install or remove a plugin, quitting while runtimes bill,
//! updating the app, and asking for a star or feedback. Every button carries a selector
//! (`overlay-cancel` for the dismissing one).

mod approval;
mod feedback;
mod forward;
mod new_session;
mod plugin;
mod quit;
mod stop;
mod update;

use ely_gpui_component::layout::{ScrollShadow, Scrollbar};
use gpui::{
    AnyElement, App, Axis, Entity, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window, deferred, div,
};

use crate::{
    state::{AppState, Overlay},
    ui::{Inputs, metrics, tagged},
};

pub use approval::{APPROVAL_ALLOW, APPROVAL_REFUSE};
pub use feedback::{FEEDBACK_GIVE, FEEDBACK_PROMPT, FEEDBACK_STAR};
pub use forward::{FORWARD_BODY, FORWARD_SUBMIT, RESERVED_NOTE};
pub use new_session::{
    NEW_SESSION_CANCEL_RUN, NEW_SESSION_FORM, NEW_SESSION_LIMIT, NEW_SESSION_START,
};
pub use plugin::{INSTALL_CONFIRM, REMOVE_CONFIRM};
pub use quit::{QUIT_CHECKING, QUIT_LEAVE, QUIT_STOP_ALL, QUIT_UNCHECKED};
pub use stop::{CANCEL_AUTO_STOP_CONFIRM, STOP_CONFIRM};
pub use update::{
    ProgressLine, UPDATE_CANCEL, UPDATE_CONFIRM, UPDATE_OPEN_PAGE, UPDATE_QUIT, UPDATE_RETRY,
    progress_line, unsupported_reason,
};

/// The open overlay, if any (an account dialog takes precedence).
pub fn render(
    state: &AppState,
    handle: &Entity<AppState>,
    inputs: &Inputs,
    cx: &App,
) -> Option<AnyElement> {
    if state.dialog().is_some() {
        return None;
    }
    Some(match state.overlay()? {
        Overlay::Stop(prompt) => stop::render(prompt, state, handle, cx),
        Overlay::CancelAutoStop(prompt) => stop::cancel_auto_stop(prompt, state, handle, cx),
        Overlay::NewSession => new_session::render(state, handle, inputs, cx)?,
        Overlay::Forward(form) => forward::render(form, state, handle, inputs, cx),
        Overlay::InstallPlugin(prompt) => plugin::install(prompt, state, handle, cx),
        Overlay::RemovePlugin(prompt) => plugin::remove(prompt, handle, cx),
        Overlay::Quit(prompt) => quit::render(prompt, state, handle, cx),
        Overlay::Update(prompt) => update::render(prompt, state, handle, cx),
        Overlay::Feedback => feedback::render(state, handle, cx),
        Overlay::Approval(prompt) => approval::render(prompt, handle, cx),
    })
}

/// `metrics::DIALOG_CHROME` counts only a title and an action row, so this must be the whole content of
/// a dialog without a description. The thumb shows whenever the content runs past the room, not
/// only on hover: a form that continues below must say so.
#[derive(IntoElement)]
struct Scrolled {
    id: &'static str,
    handle: Option<ScrollHandle>,
    content: AnyElement,
}

fn scrolled(
    id: &'static str,
    handle: Option<&ScrollHandle>,
    content: impl IntoElement,
) -> Scrolled {
    Scrolled {
        id,
        handle: handle.cloned(),
        content: content.into_any_element(),
    }
}

impl RenderOnce for Scrolled {
    #[expect(
        clippy::disallowed_methods,
        reason = "Ely's `ScrollArea` lays its bar over the content and shows it only while in use"
    )]
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let rem = window.rem_size();
        let room = (window.viewport_size().height - metrics::DIALOG_CHROME.to_pixels(rem))
            .max(metrics::DIALOG_SCROLLED_MIN_HEIGHT.to_pixels(rem));
        let handle = match self.handle {
            Some(handle) => handle,
            None => window
                .use_keyed_state(self.id, cx, |_, _| ScrollHandle::new())
                .read(cx)
                .clone(),
        };
        // The bar sits in the card's right padding, beside the content rather than over it
        // (its track takes presses). The dialog's body clips to its content box on both axes,
        // so the bar is drawn deferred, outside that clip.
        let padding = metrics::DIALOG_PADDING.to_pixels(rem);
        tagged(
            self.id,
            div()
                .relative()
                // A column, so the scrolling body shrinks to the room instead of clipping.
                .flex()
                .flex_col()
                .max_h(room)
                .mr(-padding)
                .child(
                    div()
                        .id(self.id)
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .overflow_y_scroll()
                        .overflow_x_hidden()
                        .track_scroll(&handle)
                        // A scroll box measures its children at any width; held to its own,
                        // text wraps when laid out as it does when drawn, and the height fits.
                        .child(div().w_full().min_w_0().pr(padding).child(self.content)),
                )
                .child(ScrollShadow::new(&handle))
                .child(deferred(Scrollbar::new(
                    SharedString::from(format!("{}-bar", self.id)),
                    &handle,
                    Axis::Vertical,
                ))),
        )
    }
}
