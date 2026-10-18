//! A Smithay Wayland compositor host for mcsapi desktops.
//!
//! The host owns the display, the Wayland socket, rendering and input. A
//! desktop shell plugs in through [`Shell`]: it decides where windows go
//! ([`Shell::placements`]), what keys and title-bar clicks do, and paints
//! its own wallpaper, decorations and chrome with egui. Windows come from two
//! places:
//!
//! - **Wayland clients** (foot, GTK, Qt, ...) connecting to the socket. They
//!   are asked to use server-side decorations.
//! - **In-process apps** registered with [`mcsapi_runtime`] and drawn with
//!   [`mcsapi_ui::App`], through an [`Apps`] provider. They get the same
//!   title bars, tiling and focus as Wayland clients.
//! - **Runtime clients** ([`RuntimeClient`]): programs the compositor starts
//!   itself on a private connection, usually GPUI apps. Because the
//!   compositor made the connection, it trusts the [`Role`] it gave them, so
//!   a GPUI window can be a panel or a full-screen overlay without a
//!   layer-shell protocol (which GPUI does not speak).
//!
//! Each frame paints the wallpaper, then for every window from bottom to top
//! its decoration and its content, then blurs the areas under translucent
//! panels ([`Shell::blur_regions`]), then the chrome. Frames come every
//! [`Shell::frame_interval`], by default once per refresh of the monitor the
//! session window is on. The session runs nested in a window of the current
//! X11 or Wayland session (Smithay's winit backend), or, with the `kms`
//! feature, on the bare seat: DRM/KMS output, libinput input and seat access
//! through libseat. The bare seat is picked when there is no session to nest
//! in, or with `MCSAPI_BACKEND=kms`; `MCSAPI_BACKEND=winit` forces nesting.
//! On the bare seat, clients that draw on the GPU hand over their buffers as
//! dma-bufs (`zwp_linux_dmabuf_v1`), with explicit fences
//! (`wp_linux_drm_syncobj_v1`) where the kernel and driver support them;
//! nested, every client draws into shared memory.
//!
//! ```no_run
//! use mcsapi::{Desktop, WindowId, WorkspaceId};
//! use mcsapi_compositor::{Compositor, Placement, Shell};
//!
//! struct Tiling {
//!     desktop: Desktop,
//!     next: u64,
//!     size: (i32, i32),
//! }
//!
//! impl Shell for Tiling {
//!     fn map_window(&mut self, _app_id: &str, _title: &str) -> WindowId {
//!         self.next += 1;
//!         let id = WindowId::new(self.next).unwrap();
//!         self.desktop.insert(id).unwrap();
//!         id
//!     }
//!     fn unmap_window(&mut self, window: WindowId) {
//!         let _ = self.desktop.remove(window);
//!     }
//!     fn set_output(&mut self, size: (i32, i32)) {
//!         self.size = size;
//!     }
//!     fn focused(&self) -> Option<WindowId> {
//!         self.desktop.active().focused()
//!     }
//!     fn placements(&self) -> Vec<Placement> {
//!         let bounds = mcsapi::Geometry::new((0, 0).into(), self.size.into());
//!         let focused = self.focused();
//!         self.desktop
//!             .active()
//!             .arrange(bounds)
//!             .into_iter()
//!             .flatten()
//!             .map(|p| Placement::undecorated(p.window, p.geometry, focused == Some(p.window)))
//!             .collect()
//!     }
//! }
//!
//! let desktop = Desktop::new((1..=4).filter_map(WorkspaceId::new))?;
//! Compositor::new(Tiling { desktop, next: 0, size: (1280, 800) })
//!     .launch("foot")
//!     .run()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod a11y;
#[cfg(feature = "atspi")]
mod atspi;
mod blur;
mod frames;
mod host;
mod hot;
#[cfg(feature = "kms")]
mod kms;
mod runtime;
mod text_input;

use std::{fmt, time::Duration};

pub use accesskit;
use mcsapi::{Geometry, WindowId};
pub use mcsapi_runtime::{AppId, InstanceId};
pub use mcsapi_ui::{App, GestureEvent, Theme, egui};
pub use smithay::input::keyboard::Keysym;
use smithay::reexports::calloop::channel;

pub use hot::Hot;
pub use runtime::{Edge, Reserved, Role, RuntimeClient};

/// The sandbox a window's client connected through: the metadata a sandbox
/// engine attached to the listening socket it opened with
/// `wp_security_context_v1` (Flatpak does this for every app it starts).
///
/// The client did not write any of it and cannot change it, so a shell can
/// show it as the truth about where a window came from: a window from a
/// virtual machine, say, framed in that machine's colour.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Sandbox {
    /// Which sandbox, as a reverse-DNS name: `org.flatpak`, `org.libvirt`.
    pub engine: Option<String>,
    /// What the sandbox calls what runs in it: an app's ID, a machine's name.
    pub app_id: Option<String>,
    /// Which instance of it, as the sandbox counts them.
    pub instance_id: Option<String>,
}

/// Where a window is drawn this frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Placement {
    /// The window.
    pub window: WindowId,
    /// Full frame, including any server-side decoration.
    pub frame: Geometry,
    /// Area for the window content; toplevels are configured to this size.
    pub client: Geometry,
    /// Whether the window has keyboard focus.
    pub focused: bool,
    /// Edges that touch a neighbour or the screen edge; clients square their
    /// corners and drop shadows there.
    pub tiled: Edges,
    /// Whether the window is maximized.
    pub maximized: bool,
    /// Drawn somewhere else, or faded, while the shell animates it; `None`
    /// draws it at `frame`.
    pub motion: Option<Motion>,
}

/// How a shell is animating a window this frame. Only the drawing moves:
/// the client stays configured to [`Placement::client`] and input follows
/// [`Placement::frame`], so a motion should be short (an opening window
/// growing in, a retiled one gliding to its new tile).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Motion {
    /// Where the frame is drawn. The decoration and content are stretched
    /// from [`Placement::frame`] to it.
    pub shown: Geometry,
    /// From 0 (not drawn) to 255 (opaque).
    pub opacity: u8,
}

impl Motion {
    /// Where a point of `frame` is drawn.
    pub fn map(&self, frame: Geometry, (x, y): (f32, f32)) -> (f32, f32) {
        let (sx, sy) = self.stretch(frame);
        (
            self.shown.loc.x as f32 + (x - frame.loc.x as f32) * sx,
            self.shown.loc.y as f32 + (y - frame.loc.y as f32) * sy,
        )
    }

    /// How much `frame` is stretched across, and down.
    pub fn stretch(&self, frame: Geometry) -> (f32, f32) {
        (
            self.shown.size.w as f32 / frame.size.w.max(1) as f32,
            self.shown.size.h as f32 / frame.size.h.max(1) as f32,
        )
    }
}

impl Placement {
    /// A tiled placement without decorations: content fills the frame.
    pub fn undecorated(window: WindowId, frame: Geometry, focused: bool) -> Self {
        Self {
            window,
            frame,
            client: frame,
            focused,
            tiled: Edges::ALL,
            maximized: false,
            motion: None,
        }
    }
}

/// A client's text field that has keyboard focus and asked for text input
/// (`zwp_text_input_v3`), as reported to [`Shell::text_input`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TextField {
    /// A password or PIN: nothing typed into it should be remembered.
    pub password: bool,
}

/// Something a client says about one of its windows besides its title and
/// app ID, passed to [`Shell::window_hint`]. Sent once when the window is
/// mapped for each hint already set, then on every change.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WindowHint {
    /// The window belongs to another one (`xdg_toplevel.set_parent`, or a
    /// window of another client through `xdg-foreign`, as a portal's file
    /// chooser does), or to none any more. A shell usually keeps such a
    /// window above its parent, centred on it, instead of tiling it.
    Parent(Option<WindowId>),
    /// The window is a modal dialog (`xdg_dialog_v1`): its parent should not
    /// take input until it closes.
    Modal(bool),
    /// The window's own icon (`xdg_toplevel_icon_v1`), for a task list.
    Icon(Icon),
    /// A name for the window among its app's windows that stays the same
    /// between runs (`xdg_toplevel_tag_v1`): "main", "preferences". Suited
    /// for remembering where a window went; not translated.
    Tag(String),
    /// A translated, human-readable description of the window
    /// (`xdg_toplevel_tag_v1`), for a screen reader or a window list.
    Description(String),
}

/// A window's icon: a name from the icon theme, pixels, or both.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Icon {
    /// An icon theme name, such as `org.gnome.Nautilus`.
    pub name: Option<String>,
    /// The largest image the client provided.
    pub image: Option<IconImage>,
}

/// A workspace as pagers outside the shell see it (`ext_workspace_v1`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct WorkspaceInfo {
    /// Stays the same while the workspace exists; how
    /// [`Shell::activate_workspace`] names it.
    pub id: String,
    /// What the user calls it.
    pub name: String,
    /// Whether it is the one shown.
    pub active: bool,
    /// Whether one of its windows wants attention.
    pub urgent: bool,
    /// Whether pagers should leave it out.
    pub hidden: bool,
}

impl WorkspaceInfo {
    /// A workspace that wants no attention and is not hidden.
    pub fn new(id: impl Into<String>, name: impl Into<String>, active: bool) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            active,
            ..Self::default()
        }
    }
}

/// A square icon image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IconImage {
    /// Width and height in pixels.
    pub size: u32,
    /// Rows top to bottom, 4 bytes (RGBA, premultiplied alpha) per pixel,
    /// as `egui::ColorImage::from_rgba_premultiplied` takes them.
    pub rgba: Vec<u8>,
}

/// A screen area whose backdrop is blurred before the chrome is painted.
///
/// Paint the panel itself in [`Shell::chrome`] with a translucent fill; the
/// fill's alpha sets how much of the blurred backdrop shows through.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Blur {
    /// Area in logical, output-relative coordinates.
    pub area: Geometry,
    /// Corner radius, matching the panel's rounded corners.
    pub corner_radius: u8,
    /// Blur strength from 1 (slight) to 10 (heavy); 0 draws nothing.
    pub strength: u8,
}

/// How the output refreshes, passed to [`Shell::frame_interval`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputTiming {
    /// Refresh rate in millihertz (60 Hz is 60 000).
    pub refresh_mhz: u32,
    /// Whether the output has variable refresh rate (VRR, Adaptive-Sync), so
    /// a frame can be shown whenever it is ready instead of on the next
    /// fixed refresh.
    pub vrr: bool,
}

impl Default for OutputTiming {
    /// 60 Hz without VRR.
    fn default() -> Self {
        Self {
            refresh_mhz: 60_000,
            vrr: false,
        }
    }
}

impl OutputTiming {
    /// One refresh period, the shortest useful frame interval.
    pub fn refresh_interval(&self) -> Duration {
        Duration::from_nanos(1_000_000_000_000 / u64::from(self.refresh_mhz.max(1_000)))
    }

    /// The frame interval for at most `fps` frames per second that this
    /// output can show evenly. With VRR that is simply `1/fps` (never faster
    /// than the refresh rate). Without it, frames must land on refreshes, so
    /// it is the shortest whole number of refresh periods that stays at or
    /// under `fps`: for 30 fps, 30 on 60 or 120 Hz, 28.8 on 144 Hz and 27.5
    /// on 165 Hz.
    pub fn interval_for(&self, fps: u32) -> Duration {
        let refresh = self.refresh_interval();
        let wanted = Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)));
        if wanted <= refresh {
            return refresh;
        }
        if self.vrr {
            return wanted;
        }
        // In millihertz, so 120 Hz / 30 fps is exactly 4 refreshes.
        let periods = self
            .refresh_mhz
            .max(1_000)
            .div_ceil(fps.max(1).saturating_mul(1_000));
        refresh * periods
    }

    /// The frame interval for the fewest frames per second this output can
    /// show evenly that is still at least `fps`, or every refresh when the
    /// display is no faster than that. With VRR that is simply `1/fps`.
    /// Without it, the longest whole number of refresh periods that stays
    /// at or over `fps`: for 60 fps, every refresh at 60 or 90 Hz, every
    /// second at 120, 144 (72 fps) or 165 Hz (82.5 fps), every fourth at 240.
    pub fn interval_at_least(&self, fps: u32) -> Duration {
        let refresh = self.refresh_interval();
        let wanted = Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)));
        if wanted <= refresh {
            return refresh;
        }
        if self.vrr {
            return wanted;
        }
        let periods = (self.refresh_mhz.max(1_000) / fps.max(1).saturating_mul(1_000)).max(1);
        refresh * periods
    }
}

/// A set of window edges.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Edges {
    /// Left edge.
    pub left: bool,
    /// Right edge.
    pub right: bool,
    /// Top edge.
    pub top: bool,
    /// Bottom edge.
    pub bottom: bool,
}

impl Edges {
    /// No edges (a floating window).
    pub const NONE: Self = Self {
        left: false,
        right: false,
        top: false,
        bottom: false,
    };
    /// All edges (a tiled window).
    pub const ALL: Self = Self {
        left: true,
        right: true,
        top: true,
        bottom: true,
    };
}

/// What a primary-button press handled by [`Shell::pointer_down`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Press {
    /// Forward the press (and the drag that follows) to the window content.
    Client,
    /// The shell took it, for example a title-bar button or a window drag;
    /// motion goes to [`Shell::pointer_motion`] until release.
    Handled,
}

/// Modifier state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    /// Super (logo) key.
    pub logo: bool,
    /// Shift.
    pub shift: bool,
    /// Control.
    pub ctrl: bool,
    /// Alt.
    pub alt: bool,
}

/// A key press or release, offered to [`Shell::key`] before anyone else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyInput {
    /// The keysym without modifiers applied (layout-independent shortcuts).
    pub sym: Keysym,
    /// The text the key produces with the current modifiers, if any.
    pub text: Option<char>,
    /// Press or release.
    pub pressed: bool,
    /// Modifiers held.
    pub mods: Modifiers,
}

/// Who receives a key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KeyRoute {
    /// The shell consumed it (a shortcut).
    Consume,
    /// The chrome's egui context (for example a search field).
    Chrome,
    /// The focused window.
    Client,
}

/// Work for the host, returned by [`Shell::take_commands`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Command {
    /// Launch an app: an in-process app if the [`Apps`] provider resolves
    /// the name, otherwise the program from [`Shell::spawn_argv`] with
    /// `WAYLAND_DISPLAY` pointing at this session.
    Launch(String),
    /// Ask a window to close (in-process apps are stopped).
    Close(WindowId),
    /// Start a runtime client (see [`RuntimeClient`]).
    Runtime(RuntimeClient),
    /// End the session.
    Quit,
    /// Build the accessibility tree and pass it to [`Shell::described`]
    /// with this request number.
    Describe(u64),
    /// Read back the next frame and pass it to [`Shell::captured`] with
    /// this request number.
    Capture(u64),
    /// Perform an accessibility action on an element of the tree, as a
    /// screen reader would. Counts as synthetic input
    /// ([`Shell::input_source`]). Act and [`Command::Input`] run one per
    /// frame in the order sent, so egui sees each click on the frame the
    /// last one produced.
    Act {
        /// [`a11y::Element::id`].
        element: u64,
        /// The action; [`accesskit::Action::SetValue`] focuses the element
        /// and replaces its text with `value`.
        action: accesskit::Action,
        /// Text for [`accesskit::Action::SetValue`].
        value: Option<String>,
    },
    /// Inject input as if it came from the seat (see [`Input`]). Paced
    /// like [`Command::Act`], and synthetic as well.
    Input(Input),
    /// Switch the keyboard layout, for every client and the chrome alike,
    /// for example when a setup screen offers the layouts to try. Fields are
    /// XKB names as in `localectl list-x11-keymap-layouts`; empty ones fall
    /// back to xkbcommon's defaults (`XKB_DEFAULT_*`, then `us`).
    Keymap {
        /// Layouts, comma-separated (`us`, `de,ru`).
        layout: String,
        /// Variants, one per layout (`nodeadkeys`), or empty.
        variant: String,
        /// Options (`grp:alt_shift_toggle`), or empty.
        options: String,
    },
    /// Listen on a Unix socket at `socket` for the clients of one sandbox,
    /// as if a sandbox engine had opened it with `wp_security_context_v1`:
    /// every client that connects there is marked with `sandbox`, kept from
    /// the privileged globals, and reported through
    /// [`Shell::window_sandbox`]. For a sandbox the shell runs itself (a
    /// virtual machine's windows, say), which has no Wayland connection to
    /// ask for one with. Whatever is at `socket` already is replaced; the
    /// directory should be one only the session's user can enter.
    Sandbox {
        /// Where to listen.
        socket: std::path::PathBuf,
        /// What to mark its clients with.
        sandbox: Sandbox,
    },
    /// Stop listening at a socket [`Command::Sandbox`] opened, and remove
    /// it. Clients already connected through it stay connected.
    CloseSandbox(std::path::PathBuf),
}

/// A mouse button.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseButton {
    /// Left (primary).
    Left,
    /// Right (secondary).
    Right,
    /// Middle.
    Middle,
}

/// Input injected with [`Command::Input`], for agents and automation. It
/// takes the same path as real input, so shell shortcuts, the chrome and
/// clients all see it, and the shell is told it is synthetic first.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Input {
    /// Move the pointer to a logical, output-relative position.
    Move {
        /// X.
        x: i32,
        /// Y.
        y: i32,
    },
    /// Press or release a button where the pointer is.
    Button {
        /// Which button.
        button: MouseButton,
        /// Press or release.
        pressed: bool,
    },
    /// Move there, then press and release.
    Click {
        /// X.
        x: i32,
        /// Y.
        y: i32,
        /// Which button.
        button: MouseButton,
    },
    /// Scroll where the pointer is, in logical pixels (positive is down
    /// and right).
    Scroll {
        /// Horizontal.
        dx: i32,
        /// Vertical.
        dy: i32,
    },
    /// Press and release a key with modifiers held, for example
    /// `Keysym::s` with `ctrl`.
    Key {
        /// The key, without modifiers applied.
        sym: Keysym,
        /// Modifiers to hold around it.
        mods: Modifiers,
    },
    /// Type text. Characters the keyboard layout has are typed as key
    /// presses; others reach in-process apps and the chrome directly and
    /// are skipped for Wayland clients.
    Text(String),
}

/// A frame read back with [`Command::Capture`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capture {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Rows top to bottom, 4 bytes (RGBA) per pixel.
    pub rgba: Vec<u8>,
}

/// The screen with every window drawn and nothing over them (no panels, no
/// chrome), for the chrome to show what windows hold: an overview's live
/// thumbnails. See [`Shell::wants_window_pictures`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowPictures {
    /// Drawable with the chrome's painter; this frame's windows are copied
    /// into it before the chrome is drawn.
    pub texture: egui::TextureId,
    /// The screen's size, logical.
    pub size: egui::Vec2,
}

impl WindowPictures {
    /// The texture coordinates of `area` (logical, output-relative), for
    /// [`egui::Painter::image`].
    pub fn uv(&self, area: egui::Rect) -> egui::Rect {
        let u = |x: f32| x / self.size.x;
        // The copy keeps GL's rows, which run bottom to top.
        let v = |y: f32| 1.0 - y / self.size.y;
        egui::Rect::from_min_max(
            egui::pos2(u(area.min.x), v(area.min.y)),
            egui::pos2(u(area.max.x), v(area.max.y)),
        )
    }
}

/// A request a client made about its own window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ClientRequest {
    /// Toggle maximized.
    Maximize,
    /// Minimize.
    Minimize,
}

/// The desktop shell driven by the compositor.
///
/// Coordinates are logical and output-relative. Only window bookkeeping and
/// [`Shell::placements`] are required; everything else has a neutral default.
pub trait Shell: 'static {
    /// Starts managing a new window (on its first commit, when its app ID and
    /// title are known) and returns its ID.
    fn map_window(&mut self, app_id: &str, title: &str) -> WindowId;

    /// Stops managing a destroyed window.
    fn unmap_window(&mut self, window: WindowId);

    /// The output was resized.
    fn set_output(&mut self, size: (i32, i32));

    /// Runtime panels ([`Role::Panel`]) now cover these strips along the
    /// output's edges; keep windows out of them.
    fn set_reserved(&mut self, _reserved: Reserved) {}

    /// The keyboard-focused window.
    fn focused(&self) -> Option<WindowId>;

    /// Visible windows, bottom to top.
    fn placements(&self) -> Vec<Placement>;

    /// A window changed its title.
    fn set_title(&mut self, _window: WindowId, _title: &str) {}

    /// A window changed its app ID. GPUI and some other toolkits set it
    /// only after the commit that maps the window, so
    /// [`Shell::map_window`] can see `app`.
    fn set_app_id(&mut self, _window: WindowId, _app_id: &str) {}

    /// The window that was just mapped belongs to a client that connected
    /// through a sandbox's socket. Called right after [`Shell::map_window`],
    /// before the window is first placed, and only for such clients: a window
    /// that never gets this call came from outside any sandbox.
    fn window_sandbox(&mut self, _window: WindowId, _sandbox: &Sandbox) {}

    /// Focus a window (secondary clicks on window content).
    fn focus(&mut self, _window: WindowId) {}

    /// The Wayland socket is ready; clients connect with this
    /// `WAYLAND_DISPLAY`.
    fn session_started(&mut self, _wayland_display: &str) {}

    /// Called about every frame for clocks and other polled state.
    fn tick(&mut self) {}

    /// Whether something the shell paints changed in a way the host cannot
    /// see, asked every tick right after [`Shell::tick`]: a clock's minute
    /// turned, a background thread answered, a window is sliding. The host
    /// draws a frame only when this holds, or input, a client, a
    /// [`Command`] or a job changed something, or one of its egui contexts
    /// asked to be drawn again (`egui::Context::request_repaint` and
    /// `request_repaint_after`, as an animation or an app waiting on a
    /// thread does). Otherwise the screen keeps the last frame and nothing
    /// is drawn: drawing every tick cost a whole frame of GPU (or, on a
    /// software renderer, CPU) work sixty times a second while nothing on
    /// screen moved.
    ///
    /// The default draws every tick, for shells that keep state of their
    /// own that their chrome shows.
    fn needs_frame(&mut self) -> bool {
        true
    }

    /// Whether the chrome owns the pointer at `at` (bars, overlays).
    fn chrome_wants_pointer(&self, _at: (i32, i32)) -> bool {
        false
    }

    /// A primary-button press outside the chrome.
    fn pointer_down(&mut self, _at: (i32, i32), _time_ms: u64) -> Press {
        Press::Client
    }

    /// Pointer motion during a press the shell handled.
    fn pointer_motion(&mut self, _at: (i32, i32)) {}

    /// Release of a press the shell handled.
    fn pointer_up(&mut self) {}

    /// Decides who gets a key. Called for every press and release. Releases
    /// of consumed presses are still offered here, but remain hidden from clients.
    fn key(&mut self, _key: &KeyInput) -> KeyRoute {
        KeyRoute::Client
    }

    /// A touchpad gesture. Return `true` from a begin event to take the whole
    /// gesture (for example three-finger swipes between workspaces); its later
    /// events then all come here and the return value is ignored. Gestures
    /// the shell leaves go to the chrome or the content under the pointer.
    fn gesture(&mut self, _event: &GestureEvent) -> bool {
        false
    }

    /// A client asked to change its window state.
    fn client_request(&mut self, _window: WindowId, _request: ClientRequest) {}

    /// Colors for in-process apps.
    fn theme(&self) -> Theme {
        Theme::default()
    }

    /// Paints the desktop background.
    fn paint_background(&mut self, _painter: &egui::Painter, _screen: egui::Rect) {}

    /// Paints one window's decoration, below its content.
    fn paint_decoration(&mut self, _painter: &egui::Painter, _placement: &Placement) {}

    /// Whether the chrome is about to show what windows hold (an overview's
    /// live thumbnails). Asked before each [`Shell::chrome`]. While it
    /// holds, the host copies the screen into a texture once the windows
    /// are drawn, a screen-sized copy each frame, so it is off by default.
    fn wants_window_pictures(&self) -> bool {
        false
    }

    /// Where the windows are copied, given before each [`Shell::chrome`]:
    /// `Some` while [`Shell::wants_window_pictures`] holds (from the second
    /// frame it does; the texture is made in the first), `None` otherwise.
    fn window_pictures(&mut self, _pictures: Option<WindowPictures>) {}

    /// Shows the chrome above all windows. `elapsed_ms` counts from start.
    fn chrome(&mut self, _ui: &mut egui::Ui, _elapsed_ms: u32) {}

    /// Areas to blur under the chrome this frame, bottom to top. Called right
    /// after [`Shell::chrome`], so it can report panels laid out there.
    fn blur_regions(&self) -> Vec<Blur> {
        Vec::new()
    }

    /// Time until the next frame, given how the output refreshes; for
    /// example longer in a low power mode (see [`OutputTiming::interval_for`]).
    /// Clamped to 4 ms–1 s. The default renders once per refresh.
    fn frame_interval(&self, timing: &OutputTiming) -> Duration {
        timing.refresh_interval()
    }

    /// The command line for launching `app` as a Wayland client.
    fn spawn_argv(&mut self, app: &str) -> Vec<String> {
        vec![app.to_owned()]
    }

    /// Commands queued since the last call.
    fn take_commands(&mut self) -> Vec<Command> {
        Vec::new()
    }

    /// Accessibility nodes the shell adds under windows: buttons it paints
    /// on title bars, or a tree an out-of-process program registered.
    /// Called whenever the tree is built.
    fn access_subtrees(&mut self) -> Vec<a11y::Subtree> {
        Vec::new()
    }

    /// An accessibility action on one of the shell's [`a11y::Subtree`]
    /// nodes, from a screen reader or [`Command::Act`]. `node` is the
    /// subtree's own ID.
    fn access_action(
        &mut self,
        _window: WindowId,
        _node: accesskit::NodeId,
        _action: accesskit::Action,
        _value: Option<&str>,
    ) {
    }

    /// The tree asked for with [`Command::Describe`].
    fn described(&mut self, _request: u64, _tree: a11y::Snapshot) {}

    /// The frame asked for with [`Command::Capture`].
    fn captured(&mut self, _request: u64, _frame: Result<Capture, String>) {}

    /// The input that follows is synthetic (`true`: injected with
    /// [`Command::Input`] or [`Command::Act`], or an action from an AT-SPI
    /// client) or comes from the seat (`false`). Called when that changes.
    /// A shell can refuse to let synthetic input confirm what only a person
    /// should, such as powering off.
    fn input_source(&mut self, _synthetic: bool) {}

    /// A Wayland client's focused text field asked for text input
    /// (`Some`), or the one that had stopped (`None`). An on-screen keyboard
    /// shows itself here; while a field is active, [`Input::Text`] reaches it
    /// as committed text rather than as key presses, so characters the
    /// keymap lacks still arrive.
    fn text_input(&mut self, _field: Option<TextField>) {}

    /// A client said something new about one of its windows.
    fn window_hint(&mut self, _window: WindowId, _hint: WindowHint) {}

    /// A client rang the system bell (`xdg_system_bell_v1`), from one of
    /// its windows or from none, as a terminal does on `\a`.
    fn bell(&mut self, _window: Option<WindowId>) {}

    /// A client asked for `window` to be brought forward and given the
    /// keyboard (`xdg_activation_v1`), with a token from a recent user
    /// action: a click in another app's link, a notification's button, or a
    /// launch through [`Command::Launch`]. Requests without one never reach
    /// the shell, so a window cannot steal focus on its own. By default the
    /// window is focused; a shell may instead mark it as wanting attention.
    fn activate(&mut self, window: WindowId) {
        self.focus(window);
    }

    /// A visible surface asked the session to stay awake
    /// (`zwp_idle_inhibit_v1`, a playing video), or the last such surface
    /// went away or out of sight. A shell that dims or locks the screen
    /// after a while of no input should wait while this is `true`.
    fn idle_inhibited(&mut self, _inhibited: bool) {}

    /// A screen locker (swaylock, through `ext_session_lock_v1`) locked the
    /// session, or unlocked it. While locked the compositor shows only the
    /// locker's surface and sends it every key and pointer event; the
    /// shell's shortcuts, gestures and chrome get none. If the locker dies
    /// without unlocking, the session stays locked until another locker
    /// unlocks it.
    fn session_locked(&mut self, _locked: bool) {}

    /// The shell's workspaces in order, for pagers outside it
    /// (`ext_workspace_v1`, waybar's workspaces module). Asked every frame
    /// while a pager is connected; none by default.
    fn workspaces(&self) -> Vec<WorkspaceInfo> {
        Vec::new()
    }

    /// A pager asked to show the workspace with this [`WorkspaceInfo::id`].
    fn activate_workspace(&mut self, _id: &str) {}

    /// Whether `window` may take the keys the shell otherwise takes
    /// (`zwp_keyboard_shortcuts_inhibit_manager_v1`) while it has the
    /// keyboard: a virtual machine or remote desktop viewer passes Super and
    /// Alt+Tab on to the machine it shows. Asked when it asks; allowed by
    /// default, as other compositors do. Clicking another window, or
    /// switching virtual terminals, always gets out.
    fn inhibit_shortcuts(&mut self, _window: WindowId) -> bool {
        true
    }
}

/// In-process apps the compositor can launch, usually backed by an
/// [`mcsapi_runtime::Runtime`].
pub trait Apps {
    /// The app a launch name refers to (an app ID, or a friendly name).
    fn resolve(&self, name: &str) -> Option<AppId>;

    /// Starts an instance.
    fn launch(&mut self, app: &AppId) -> Result<InstanceId, mcsapi_runtime::Error>;

    /// Stops an instance.
    fn stop(&mut self, instance: InstanceId);

    /// A running app.
    fn app_mut(&mut self, instance: InstanceId) -> Option<&mut dyn App>;

    /// Styles an app's egui context before each frame (visuals, zoom).
    fn prepare(&mut self, _ctx: &egui::Context, _theme: &Theme) {}
}

/// Work to run on the shell from another thread.
pub type Job<S> = Box<dyn FnOnce(&mut S) + Send>;

/// Sends [`Job`]s to a running compositor, for example from an IPC thread.
pub struct Remote<S>(channel::Sender<Job<S>>);

impl<S> Clone for Remote<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S> fmt::Debug for Remote<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Remote")
    }
}

impl<S> Remote<S> {
    /// Queues `job`; returns `false` once the compositor has stopped.
    pub fn run(&self, job: impl FnOnce(&mut S) + Send + 'static) -> bool {
        self.0.send(Box::new(job)).is_ok()
    }
}

/// Session configuration and entry point.
pub struct Compositor<S> {
    shell: S,
    apps: Option<Box<dyn Apps>>,
    size: (i32, i32),
    title: String,
    vrr: bool,
    launch: Vec<String>,
    runtime: Vec<RuntimeClient>,
    jobs: Option<channel::Channel<Job<S>>>,
}

impl<S: Shell + 'static> Compositor<S> {
    /// A compositor for `shell`, 1280×800 when nested.
    pub fn new(shell: S) -> Self {
        Self {
            shell,
            apps: None,
            size: (1280, 800),
            title: "mcsapi".into(),
            vrr: false,
            launch: Vec::new(),
            runtime: Vec::new(),
            jobs: None,
        }
    }

    /// Provides in-process apps.
    pub fn apps(mut self, apps: impl Apps + 'static) -> Self {
        self.apps = Some(Box::new(apps));
        self
    }

    /// Window size when nested.
    pub fn size(mut self, width: i32, height: i32) -> Self {
        self.size = (width, height);
        self
    }

    /// Title of the nested window.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Declares that the output has variable refresh rate, for example when
    /// the parent session drives a VRR monitor. Nested sessions cannot detect
    /// it, so [`OutputTiming::vrr`] is `false` unless set here.
    pub fn vrr(mut self, vrr: bool) -> Self {
        self.vrr = vrr;
        self
    }

    /// Launches an app once the session is up (see [`Command::Launch`]).
    pub fn launch(mut self, app: impl Into<String>) -> Self {
        self.launch.push(app.into());
        self
    }

    /// Starts a runtime client when the session is up, for example a GPUI
    /// panel (see [`RuntimeClient`]).
    pub fn runtime(mut self, client: RuntimeClient) -> Self {
        self.runtime.push(client);
        self
    }

    /// A handle for running jobs on the shell from other threads.
    pub fn remote(&mut self) -> Remote<S> {
        let (sender, channel) = channel::channel();
        self.jobs = Some(channel);
        Remote(sender)
    }

    /// Runs the session until its window closes or the shell quits.
    ///
    /// With the `hotpatch` feature in a debug build, this also listens for
    /// patches from `dx serve --hot-patch`; see [`Hot`].
    pub fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        hot::connect();
        host::run(self)
    }
}
