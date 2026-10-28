//! `ext_session_lock_v1`: a screen locker (swaylock, hyprlock, gtklock)
//! hides the session behind a surface of its own until the user
//! authenticates.
//!
//! From the lock request on, frames show only the locker's surface (or the
//! clear colour until it maps one), drawn over everything else, and the
//! locker is told the session is locked once such a frame has been drawn.
//! Every key and pointer event goes to the locker: the shell's shortcuts,
//! gestures and chrome see none, though Ctrl+Alt+F1–F12 still switch VTs on
//! the bare seat. A locker that dies without unlocking leaves the session
//! locked, as the protocol requires; a new locker can take over and unlock.

use std::time::Duration;

use smithay::{
    backend::renderer::{
        element::{
            Kind, surface::WaylandSurfaceRenderElement, surface::render_elements_from_surface_tree,
        },
        gles::GlesRenderer,
    },
    delegate_session_lock,
    desktop::{
        WindowSurfaceType,
        utils::{send_frames_surface_tree, under_from_surface_tree},
    },
    reexports::wayland_server::{
        DisplayHandle,
        protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    },
    utils::{Logical, Point, Scale},
    wayland::session_lock::{
        LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
    },
};

use super::{Host, security::unsandboxed};
use crate::Shell;

pub(super) struct Lock {
    state: SessionLockManagerState,
    /// From the lock request until the unlock.
    pub(super) locked: bool,
    /// The locker's confirmation, sent once a frame without the session is
    /// on screen.
    confirm: Option<SessionLocker>,
    surface: Option<LockSurface>,
}

impl Lock {
    pub(super) fn new<S: Shell + 'static>(dh: &DisplayHandle) -> Self {
        Self {
            state: SessionLockManagerState::new::<Host<S>, _>(dh, unsandboxed),
            locked: false,
            confirm: None,
            surface: None,
        }
    }

    /// The locker's surface, if one is mapped and the session is locked.
    fn surface(&self) -> Option<&WlSurface> {
        self.surface
            .as_ref()
            .filter(|_| self.locked)
            .map(LockSurface::wl_surface)
    }

    /// The locker's surface, locked or not.
    pub(super) fn mapped(&self) -> Option<&WlSurface> {
        self.surface.as_ref().map(LockSurface::wl_surface)
    }

    /// The locker's surface under `pointer`.
    pub(super) fn surface_under(
        &self,
        pointer: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        under_from_surface_tree(self.surface()?, pointer, (0, 0), WindowSurfaceType::ALL)
            .map(|(surface, at)| (surface, at.to_f64()))
    }

    /// The surface the keyboard belongs to while locked.
    pub(super) fn keyboard(&self) -> Option<WlSurface> {
        self.surface().cloned()
    }

    /// What to draw over the session while locked.
    pub(super) fn elements(
        &self,
        renderer: &mut GlesRenderer,
    ) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
        self.surface()
            .map(|surface| {
                render_elements_from_surface_tree(
                    renderer,
                    surface,
                    (0, 0),
                    Scale::from(1.0),
                    1.0,
                    Kind::Unspecified,
                )
            })
            .unwrap_or_default()
    }

    /// A frame without the session is on screen: tells the locker.
    pub(super) fn drawn(&mut self) {
        if let Some(confirm) = self.confirm.take() {
            confirm.lock();
        }
    }

    pub(super) fn send_frames(&self, output: &smithay::output::Output, now: Duration) {
        if let Some(surface) = self.surface() {
            send_frames_surface_tree(surface, output, now, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    /// Sizes the locker's surface to the output.
    pub(super) fn resize(&self, size: (i32, i32)) {
        let Some(surface) = &self.surface else {
            return;
        };
        let size = (size.0.max(1) as u32, size.1.max(1) as u32).into();
        let changed = surface.with_pending_state(|state| state.size.replace(size) != Some(size));
        if changed {
            surface.send_configure();
        }
    }
}

impl<S: Shell + 'static> SessionLockHandler for Host<S> {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.lock.state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        let was = self.lock.locked;
        self.lock.locked = true;
        self.dirty = true;
        self.lock.confirm = Some(confirmation);
        // A press that started before the lock must not finish on what is
        // now hidden.
        if self.route.is_some() {
            self.cancel_pointer_route();
        }
        if !was {
            self.shell.session_locked(true);
        }
        self.sync();
    }

    fn unlock(&mut self) {
        self.lock.locked = false;
        self.dirty = true;
        self.lock.confirm = None;
        self.lock.surface = None;
        self.shell.session_locked(false);
        self.sync();
    }

    /// There is one output, so the surface covers it whichever it names.
    fn new_surface(&mut self, surface: LockSurface, _output: WlOutput) {
        self.lock.surface = Some(surface);
        let size = self.backend.size();
        self.lock.resize((size.w, size.h));
        self.sync();
    }
}

delegate_session_lock!(@<S: Shell + 'static> Host<S>);
