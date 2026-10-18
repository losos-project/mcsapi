//! Frame timing for clients: `wp_presentation` says when a commit reached
//! the screen, `wp_fifo_v1` holds a commit until the one before it was
//! shown (Vulkan's FIFO present mode without a frame callback per frame),
//! and `wp_commit_timing_v1` holds one until the frame it asks for (video
//! players timing frames to the refresh).
//!
//! Each drawn frame takes the presentation feedback of every surface it
//! shows. On the bare seat it is sent at the vblank that puts the frame on
//! screen, with the kernel's timestamp and counter (see `Kms::vblank`); in
//! a window there is no such event, so it is sent when the frame is handed
//! to the host, without the vsync flags. A frame that never reaches the
//! screen discards it.
//!
//! After each frame, every surface's fifo barrier is cleared, shown or not,
//! so a hidden window's FIFO swapchain keeps going at the refresh rate, and
//! commit timers due by the next refresh fire.

use std::time::Duration;

use smithay::{
    delegate_commit_timing, delegate_fifo, delegate_presentation,
    desktop::{
        PopupManager, layer_map_for_output,
        utils::{
            OutputPresentationFeedback, take_presentation_feedback_surface_tree,
            with_surfaces_surface_tree,
        },
    },
    reexports::{
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::{Client, DisplayHandle, Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Clock, ClockSource, Monotonic, Time},
    wayland::{
        commit_timing::{CommitTimerBarrierStateUserData, CommitTimingManagerState},
        fifo::{FifoBarrierCachedState, FifoManagerState},
        presentation::{PresentationState, Refresh},
    },
};

use super::{ClientState, Content, Host};
use crate::Shell;

pub(super) struct Timing {
    _presentation: PresentationState,
    _fifo: FifoManagerState,
    _commit_timing: CommitTimingManagerState,
    pub(super) clock: Clock<Monotonic>,
    /// The feedback of the frame being drawn, until it is handed on.
    pub(super) pending: Option<OutputPresentationFeedback>,
}

impl Timing {
    pub(super) fn new<S: Shell + 'static>(dh: &DisplayHandle) -> Self {
        Self {
            _presentation: PresentationState::new::<Host<S>>(dh, Monotonic::ID as u32),
            _fifo: FifoManagerState::new::<Host<S>>(dh),
            _commit_timing: CommitTimingManagerState::new::<Host<S>>(dh),
            clock: Clock::new(),
            pending: None,
        }
    }
}

/// The refresh a presentation reports for `millihertz`: variable with
/// adaptive sync, where it is the fastest rate.
pub(crate) fn refresh(millihertz: u32, vrr: bool) -> Refresh {
    if millihertz == 0 {
        return Refresh::Unknown;
    }
    let period = Duration::from_nanos(1_000_000_000_000 / u64::from(millihertz));
    if vrr {
        Refresh::variable(period)
    } else {
        Refresh::fixed(period)
    }
}

/// `root` and its popups, each a surface tree of its own.
fn with_popups(root: WlSurface) -> impl Iterator<Item = WlSurface> {
    let popups = PopupManager::popups_for_surface(&root)
        .map(|(popup, _)| popup.wl_surface().clone())
        .collect::<Vec<_>>();
    std::iter::once(root).chain(popups)
}

impl<S: Shell + 'static> Host<S> {
    /// The roots of the surface trees a frame shows: windows and panels
    /// with their popups, layer surfaces, an input method's popups and the
    /// cursor, or only the locker and the cursor while locked.
    fn shown_roots(&self) -> Vec<WlSurface> {
        let cursor = self.cursors.surface().cloned();
        if self.lock.locked {
            return self
                .lock
                .mapped()
                .cloned()
                .into_iter()
                .chain(cursor)
                .collect();
        }
        let windows = self
            .space
            .elements()
            .filter_map(|w| w.toplevel().map(|t| t.wl_surface().clone()));
        let layers = layer_map_for_output(&self.output)
            .layers()
            .map(|l| l.wl_surface().clone())
            .collect::<Vec<_>>();
        windows
            .chain(layers)
            .flat_map(with_popups)
            .chain(self.input_method.shown_surfaces().cloned())
            .chain(cursor)
            .collect()
    }

    /// The roots of every surface tree, shown or not.
    fn all_roots(&self) -> Vec<WlSurface> {
        let windows = self
            .windows
            .values()
            .filter_map(|c| match c {
                Content::Wayland(w) => w.toplevel(),
                _ => None,
            })
            .chain(self.unmanaged.iter().filter_map(|w| w.toplevel()))
            .chain(self.layers.iter().filter_map(|l| l.window.toplevel()))
            .map(|t| t.wl_surface().clone());
        let layers = layer_map_for_output(&self.output)
            .layers()
            .map(|l| l.wl_surface().clone())
            .collect::<Vec<_>>();
        windows
            .chain(layers)
            .flat_map(with_popups)
            .chain(self.lock.mapped().cloned())
            .chain(self.input_method.surfaces().cloned())
            .chain(self.cursors.surface().cloned())
            .collect()
    }

    /// Before a frame is drawn: takes the presentation feedback of what it
    /// shows, to be sent when it reaches the screen.
    pub(super) fn take_presentation_feedback(&mut self) {
        let mut feedback = OutputPresentationFeedback::new(&self.output);
        for root in self.shown_roots() {
            take_presentation_feedback_surface_tree(
                &root,
                &mut feedback,
                |_, _| Some(self.output.clone()),
                |_, _| wp_presentation_feedback::Kind::empty(),
            );
        }
        self.timing.pending = Some(feedback);
    }

    /// After a frame is drawn: a frame no vblank will report is presented
    /// now (or was discarded, if drawing failed), and the clients' fifo
    /// barriers and due commit timers are cleared.
    pub(super) fn frame_drawn(&mut self, ok: bool) {
        let now = self.timing.clock.now();
        if let Some(mut feedback) = self.timing.pending.take() {
            if ok {
                feedback.presented(
                    now,
                    refresh(self.refresh_mhz, self.vrr),
                    0,
                    wp_presentation_feedback::Kind::empty(),
                );
            } else {
                feedback.discarded();
            }
        }
        self.release_barriers();
    }

    /// Clears the clients' fifo barriers and the commit timers due before
    /// the next refresh, after each frame and each tick that drew none.
    pub(super) fn release_barriers(&mut self) {
        let now = self.timing.clock.now();
        // Timers due before the next refresh can show their commit in it.
        let period = match refresh(self.refresh_mhz, self.vrr) {
            Refresh::Fixed(p) | Refresh::Variable(p) => p,
            Refresh::Unknown => Duration::from_millis(16),
        };
        let next: Time<Monotonic> = (Duration::from(now) + period).into();
        let mut cleared: Vec<Client> = Vec::new();
        for root in self.all_roots() {
            with_surfaces_surface_tree(&root, |surface, states| {
                let fifo = states
                    .cached_state
                    .get::<FifoBarrierCachedState>()
                    .current()
                    .barrier
                    .take();
                let mut signalled = fifo.inspect(|b| b.signal()).is_some();
                if let Some(timers) = states.data_map.get::<CommitTimerBarrierStateUserData>() {
                    signalled |= timers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .signal_until(next);
                }
                if signalled
                    && let Some(client) = surface.client()
                    && !cleared.iter().any(|c| c.id() == client.id())
                {
                    cleared.push(client);
                }
            });
        }
        let dh = self.display.clone();
        for client in cleared {
            if let Some(data) = client.get_data::<ClientState>() {
                data.compositor_state.blocker_cleared(self, &dh);
            }
        }
    }
}

delegate_presentation!(@<S: Shell + 'static> Host<S>);
delegate_fifo!(@<S: Shell + 'static> Host<S>);
delegate_commit_timing!(@<S: Shell + 'static> Host<S>);
