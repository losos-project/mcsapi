//! What clients say about their windows beyond title and app ID, passed to
//! the shell as [`WindowHint`]s, plus the system bell and activation.
//!
//! - `xdg_toplevel.set_parent` and `xdg-foreign` (`zxdg_exporter_v2`,
//!   `zxdg_importer_v2`): a dialog's parent, also across clients, which is
//!   how a portal's file chooser names the app window it belongs to.
//! - `xdg_wm_dialog_v1`: whether a dialog is modal.
//! - `xdg_toplevel_icon_manager_v1`: a window's own icon.
//! - `xdg_toplevel_tag_manager_v1`: a stable tag and a description.
//! - The size a window's content is committed at.
//! - `xdg_system_bell_v1`: the bell.
//! - `xdg_activation_v1`: focus requests that carry a token from a user
//!   action. The compositor hands out tokens only to the client that has the
//!   keyboard, and puts one in `XDG_ACTIVATION_TOKEN` for programs it
//!   launches, so a window brings itself forward only when the user just
//!   asked for it.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use mcsapi::WindowId;
use smithay::{
    delegate_xdg_activation, delegate_xdg_dialog, delegate_xdg_foreign, delegate_xdg_system_bell,
    delegate_xdg_toplevel_icon, delegate_xdg_toplevel_tag,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel,
        wayland_server::{DisplayHandle, Resource, protocol::wl_surface::WlSurface},
    },
    wayland::{
        compositor::with_states,
        shell::xdg::{
            ToplevelSurface, XdgToplevelSurfaceData,
            dialog::{XdgDialogHandler, XdgDialogState},
        },
        shm::with_buffer_contents,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        xdg_foreign::{XdgForeignHandler, XdgForeignState},
        xdg_system_bell::{XdgSystemBellHandler, XdgSystemBellState},
        xdg_toplevel_icon::{
            ToplevelIconCachedState, XdgToplevelIconHandler, XdgToplevelIconManager,
        },
        xdg_toplevel_tag::{XdgToplevelTagHandler, XdgToplevelTagManager},
    },
};

use super::{Content, Host};
use crate::{Icon, IconImage, Shell, WindowHint};

/// How long an activation token stays usable: long enough for a slow app to
/// start and map its window.
const TOKEN_LIFETIME: Duration = Duration::from_secs(30);

/// The globals and what is waiting on a window to be mapped.
pub(super) struct Toplevels {
    pub(super) activation: XdgActivationState,
    foreign: XdgForeignState,
    _dialog: XdgDialogState,
    _icon: XdgToplevelIconManager,
    _tag: XdgToplevelTagManager,
    _bell: XdgSystemBellState,
    /// Surfaces with a valid activation request that were not mapped yet.
    pending_activation: Vec<WlSurface>,
}

impl Toplevels {
    pub(super) fn new<S: Shell + 'static>(dh: &DisplayHandle) -> Self {
        Self {
            activation: XdgActivationState::new::<Host<S>>(dh),
            foreign: XdgForeignState::new::<Host<S>>(dh),
            _dialog: XdgDialogState::new::<Host<S>>(dh),
            _icon: XdgToplevelIconManager::new::<Host<S>>(dh),
            _tag: XdgToplevelTagManager::new::<Host<S>>(dh),
            _bell: XdgSystemBellState::new::<Host<S>>(dh),
            pending_activation: Vec::new(),
        }
    }

    /// A fresh token for a program the compositor launches.
    pub(super) fn launch_token(&mut self) -> String {
        self.prune();
        let (token, _) = self.activation.create_external_token(None);
        token.as_str().to_owned()
    }

    fn prune(&mut self) {
        self.activation
            .retain_tokens(|_, data| data.timestamp.elapsed() < TOKEN_LIFETIME);
    }
}

impl<S: Shell + 'static> Host<S> {
    /// The toplevel behind an `xdg_toplevel`, mapped or not yet.
    fn toplevel_surface(&self, toplevel: &XdgToplevel) -> Option<ToplevelSurface> {
        self.xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .find(|t| t.xdg_toplevel() == toplevel)
            .cloned()
    }

    fn window_of_xdg(&self, toplevel: &XdgToplevel) -> Option<WindowId> {
        self.windows.iter().find_map(|(id, content)| match content {
            Content::Wayland(w) if w.toplevel().is_some_and(|t| t.xdg_toplevel() == toplevel) => {
                Some(*id)
            }
            _ => None,
        })
    }

    /// Tells the shell every hint a newly mapped window already has, and
    /// carries out an activation that arrived before it was mapped.
    pub(super) fn window_mapped(&mut self, id: WindowId, toplevel: &ToplevelSurface) {
        let surface = toplevel.wl_surface();
        let parent = self.parent_of(toplevel);
        if parent.is_some() {
            self.shell.window_hint(id, WindowHint::Parent(parent));
        }
        if is_modal(surface) {
            self.shell.window_hint(id, WindowHint::Modal(true));
        }
        let (tag, description) = tag_of(surface);
        if let Some(tag) = tag {
            self.shell.window_hint(id, WindowHint::Tag(tag));
        }
        if let Some(description) = description {
            self.shell
                .window_hint(id, WindowHint::Description(description));
        }
        // The icon is read as it stands now, so the commit that mapped the
        // window does not report it again.
        take_icon_pending(surface);
        if let Some(icon) = icon_of(surface) {
            self.shell.window_hint(id, WindowHint::Icon(icon));
        }
        let pending = &mut self.toplevels.pending_activation;
        pending.retain(|s| s.is_alive());
        if let Some(i) = pending.iter().position(|s| s == surface) {
            pending.swap_remove(i);
            self.shell.activate(id);
        }
    }

    /// The window `toplevel`'s parent surface belongs to, if it is mapped.
    pub(super) fn parent_of(&self, toplevel: &ToplevelSurface) -> Option<WindowId> {
        let parent = toplevel.parent()?;
        self.wayland_window_of(&parent).map(|(id, _)| id)
    }

    pub(super) fn parent_changed(&mut self, toplevel: &ToplevelSurface) {
        if let Some((id, _)) = self.wayland_window_of(toplevel.wl_surface()) {
            let parent = self.parent_of(toplevel);
            self.shell.window_hint(id, WindowHint::Parent(parent));
        }
    }

    /// After a commit: the shell hears of the size the window's content
    /// now has when it differs from the last one it heard of.
    pub(super) fn size_committed(&mut self, surface: &WlSurface) {
        let Some((id, window)) = self.wayland_window_of(surface) else {
            return;
        };
        let size = window.geometry().size;
        let size = (size.w, size.h);
        if size.0 <= 0 || size.1 <= 0 {
            return;
        }
        let changed = with_states(surface, |states| {
            let last = states
                .data_map
                .get_or_insert_threadsafe(CommittedSize::default);
            let mut last = last.0.lock().unwrap_or_else(|e| e.into_inner());
            last.replace(size) != Some(size)
        });
        if changed {
            self.shell.window_hint(id, WindowHint::Size(size));
        }
    }

    /// After a commit: a new icon takes effect with it.
    pub(super) fn icon_committed(&mut self, surface: &WlSurface) {
        if !take_icon_pending(surface) {
            return;
        }
        if let Some((id, _)) = self.wayland_window_of(surface) {
            let icon = icon_of(surface).unwrap_or_default();
            self.shell.window_hint(id, WindowHint::Icon(icon));
        }
    }
}

fn is_modal(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok())
            .is_some_and(|d| d.modal)
    })
}

/// The content size last told to the shell.
#[derive(Default)]
struct CommittedSize(std::sync::Mutex<Option<(i32, i32)>>);

/// A toplevel's tag and description. Kept here rather than read from
/// Smithay's `XdgToplevelTagSurfaceData`, which 0.7 fills with the
/// description in place of the tag.
#[derive(Default)]
struct Tags(std::sync::Mutex<(Option<String>, Option<String>)>);

fn tag_of(surface: &WlSurface) -> (Option<String>, Option<String>) {
    with_states(surface, |states| {
        states
            .data_map
            .get::<Tags>()
            .map(|t| t.0.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default()
    })
}

fn set_tags(toplevel: &ToplevelSurface, f: impl FnOnce(&mut (Option<String>, Option<String>))) {
    with_states(toplevel.wl_surface(), |states| {
        let tags = states.data_map.get_or_insert_threadsafe(Tags::default);
        f(&mut tags.0.lock().unwrap_or_else(|e| e.into_inner()));
    });
}

/// The surface's current icon, with its largest image converted to RGBA.
fn icon_of(surface: &WlSurface) -> Option<Icon> {
    let (name, buffers) = with_states(surface, |states| {
        let mut cached = states.cached_state.get::<ToplevelIconCachedState>();
        let current = cached.current();
        (
            current.icon_name().map(str::to_owned),
            current.buffers().to_vec(),
        )
    });
    // The protocol only allows square ARGB8888 shm buffers; the largest one
    // has the most detail to scale down from.
    let image = buffers
        .iter()
        .filter_map(|(buffer, _scale)| {
            with_buffer_contents(buffer, |ptr, len, data| {
                let size = u32::try_from(data.width).ok()?;
                let stride = usize::try_from(data.stride).ok()?;
                let offset = usize::try_from(data.offset).ok()?;
                let rows = usize::try_from(data.height).ok()?;
                let row_bytes = size as usize * 4;
                let end = offset.checked_add(stride.checked_mul(rows)?)?;
                if end > len || stride < row_bytes {
                    return None;
                }
                // SAFETY: smithay maps the pool for the duration of this
                // closure, and the range was checked against its length.
                let pool = unsafe { std::slice::from_raw_parts(ptr, len) };
                let mut rgba = Vec::with_capacity(row_bytes * rows);
                for row in pool[offset..end].chunks_exact(stride) {
                    // Little-endian ARGB8888 is B, G, R, A in memory.
                    for p in row[..row_bytes].as_chunks::<4>().0 {
                        rgba.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
                    }
                }
                Some(IconImage { size, rgba })
            })
            .ok()
            .flatten()
        })
        .max_by_key(|image| image.size);
    (name.is_some() || image.is_some()).then_some(Icon { name, image })
}

/// Whether the surface's icon changed with its last commit; clears the mark.
fn take_icon_pending(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states
            .data_map
            .get::<IconPending>()
            .is_some_and(|p| p.0.swap(false, Ordering::Relaxed))
    })
}

/// Set on a surface whose icon changes with its next commit.
#[derive(Default)]
struct IconPending(AtomicBool);

impl<S: Shell + 'static> XdgToplevelIconHandler for Host<S> {
    fn set_icon(&mut self, _toplevel: XdgToplevel, surface: WlSurface) {
        with_states(&surface, |states| {
            states
                .data_map
                .insert_if_missing_threadsafe(IconPending::default);
            if let Some(pending) = states.data_map.get::<IconPending>() {
                pending.0.store(true, Ordering::Relaxed);
            }
        });
    }
}

impl<S: Shell + 'static> XdgToplevelTagHandler for Host<S> {
    fn set_tag(&mut self, toplevel: XdgToplevel, tag: String) {
        let Some(surface) = self.toplevel_surface(&toplevel) else {
            return;
        };
        set_tags(&surface, |t| t.0 = Some(tag.clone()));
        if let Some(id) = self.window_of_xdg(&toplevel) {
            self.shell.window_hint(id, WindowHint::Tag(tag));
        }
    }

    fn set_description(&mut self, toplevel: XdgToplevel, description: String) {
        let Some(surface) = self.toplevel_surface(&toplevel) else {
            return;
        };
        set_tags(&surface, |t| t.1 = Some(description.clone()));
        if let Some(id) = self.window_of_xdg(&toplevel) {
            self.shell
                .window_hint(id, WindowHint::Description(description));
        }
    }
}

impl<S: Shell + 'static> XdgDialogHandler for Host<S> {
    fn modal_changed(&mut self, toplevel: ToplevelSurface, is_modal: bool) {
        if let Some((id, _)) = self.wayland_window_of(toplevel.wl_surface()) {
            self.shell.window_hint(id, WindowHint::Modal(is_modal));
        }
    }
}

impl<S: Shell + 'static> XdgSystemBellHandler for Host<S> {
    fn ring(&mut self, surface: Option<WlSurface>) {
        let window = surface.and_then(|s| self.wayland_window_of(&s).map(|(id, _)| id));
        self.shell.bell(window);
    }
}

impl<S: Shell + 'static> XdgForeignHandler for Host<S> {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.toplevels.foreign
    }
}

impl<S: Shell + 'static> XdgActivationHandler for Host<S> {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.toplevels.activation
    }

    /// Only the client that has the keyboard may hand out tokens: it is the
    /// one the user is working in, so whatever it asks to bring forward is
    /// what the user just asked for.
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        self.toplevels.prune();
        let focused = self
            .keyboard_surface
            .as_ref()
            .and_then(|s| s.client())
            .map(|c| c.id());
        focused.is_some() && focused == data.client_id
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        self.toplevels.activation.remove_token(&token);
        if data.timestamp.elapsed() >= TOKEN_LIFETIME {
            return;
        }
        if let Some((id, _)) = self.wayland_window_of(&surface) {
            self.shell.activate(id);
            return;
        }
        let unmapped = self
            .unmanaged
            .iter()
            .any(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &surface));
        if unmapped && !self.toplevels.pending_activation.contains(&surface) {
            self.toplevels.pending_activation.push(surface);
        }
    }
}

delegate_xdg_dialog!(@<S: Shell + 'static> Host<S>);
delegate_xdg_toplevel_icon!(@<S: Shell + 'static> Host<S>);
delegate_xdg_toplevel_tag!(@<S: Shell + 'static> Host<S>);
delegate_xdg_system_bell!(@<S: Shell + 'static> Host<S>);
delegate_xdg_foreign!(@<S: Shell + 'static> Host<S>);
delegate_xdg_activation!(@<S: Shell + 'static> Host<S>);
