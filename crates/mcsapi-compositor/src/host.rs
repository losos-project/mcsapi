//! The compositor state, Wayland protocol handlers, input routing and
//! rendering.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    fs::File,
    hash::{Hash, Hasher},
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command as Process},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

mod automation;
mod capture;
mod cursor;
mod effects;
mod foreign;
mod hints;
mod input;
mod input_method;
mod layer_shell;
mod lock;
mod output_config;
mod responding;
mod security;
mod timing;
#[cfg(feature = "kms")]
pub(crate) use timing::refresh;
mod toplevel;
mod workspaces;

use mcsapi::{Geometry, WindowId};
use smithay::{
    backend::{
        allocator::Fourcc,
        egl,
        input::{
            AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, GestureBeginEvent as _,
            GestureEndEvent as _, GesturePinchUpdateEvent as _, GestureSwipeUpdateEvent as _,
            InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent,
            PointerButtonEvent, TouchEvent, TouchSlot,
        },
        renderer::{
            Bind, Color32F, Frame, Offscreen, Renderer, Texture,
            element::{
                AsRenderElements, Element, Id, RenderElement, surface::WaylandSurfaceRenderElement,
            },
            gles::{GlesFrame, GlesRenderer, GlesTexture},
            utils::{CommitCounter, on_commit_buffer_handler},
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    delegate_compositor, delegate_data_device, delegate_output, delegate_pointer_gestures,
    delegate_primary_selection, delegate_seat, delegate_shm, delegate_xdg_decoration,
    delegate_xdg_shell,
    desktop::{PopupKind, PopupManager, Space, Window, utils::send_frames_surface_tree},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, Keycode, Keysym, KeysymHandle, ModifiersState, XkbConfig},
        pointer::{self, AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopSignal, Mode as CalloopMode, PostAction, channel,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
            shell::server::xdg_toplevel::State as ToplevelState,
        },
        wayland_server::{
            Client, Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_seat, wl_shm, wl_surface::WlSurface},
        },
        winit::{dpi::LogicalSize, window::Window as WinitWindow},
    },
    utils::{
        Buffer as BufferCoord, Logical, Physical, Point, Rectangle, SERIAL_COUNTER, Scale, Serial,
        Size, Transform,
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface, with_states,
        },
        output::{OutputHandler, OutputManagerState},
        pointer_gestures::PointerGesturesState,
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus, set_data_device_selection,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ShellClient, ToplevelSurface, XdgShellHandler,
            XdgShellState, XdgToplevelSurfaceData,
            decoration::{XdgDecorationHandler, XdgDecorationState},
        },
        shm::{ShmHandler, ShmState},
        socket::ListeningSocketSource,
    },
};

#[cfg(feature = "kms")]
use smithay::{
    backend::{
        allocator::{Buffer as _, dmabuf::Dmabuf},
        renderer::ImportDma,
    },
    delegate_dmabuf, delegate_drm_syncobj,
    reexports::calloop::{EventSource, LoopHandle},
    wayland::{
        compositor::{BufferAssignment, SurfaceAttributes, add_blocker, add_pre_commit_hook},
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf},
        drm_syncobj::{DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState},
    },
};

use self::input::TabletInput;
use crate::frames::FrameStats;
use crate::{
    Apps, Blur, Capture, ClientRequest, Command, Compositor, GestureEvent, Input, InstanceId, Job,
    KeyInput, KeyRoute, Modifiers, Motion, MouseButton, OutputTiming, Placement, Press, Reserved,
    Role, RuntimeClient, Shell, WindowPictures, a11y, accesskit, blur, egui,
    text_input::TextInputs,
};
use mcsapi_ui::gesture::EguiBridge;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;
use tracing::{error, warn};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const MAX_SELECTION_TRANSFERS: usize = 4;
const SELECTION_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

static ACTIVE_SELECTION_TRANSFERS: AtomicUsize = AtomicUsize::new(0);

struct SelectionTransferGuard;

impl SelectionTransferGuard {
    fn acquire() -> Option<Self> {
        let mut active = ACTIVE_SELECTION_TRANSFERS.load(Ordering::Relaxed);
        loop {
            if active >= MAX_SELECTION_TRANSFERS {
                return None;
            }
            match ACTIVE_SELECTION_TRANSFERS.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self),
                Err(current) => active = current,
            }
        }
    }
}

impl Drop for SelectionTransferGuard {
    fn drop(&mut self) {
        ACTIVE_SELECTION_TRANSFERS.fetch_sub(1, Ordering::Release);
    }
}

fn write_selection(fd: OwnedFd, text: &[u8], timeout: Duration) -> io::Result<()> {
    let file = File::from(fd);
    let raw_fd = file.as_raw_fd();
    // SAFETY: fcntl is called with a valid owned file descriptor.
    let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl is called with a valid owned file descriptor and its existing flags.
    if unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }

    let deadline = Instant::now() + timeout;
    let mut remaining = text;
    while !remaining.is_empty() {
        if Instant::now() >= deadline {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        // SAFETY: the byte slice is valid for the duration of this write call.
        let written = unsafe { libc::write(raw_fd, remaining.as_ptr().cast(), remaining.len()) };
        if written > 0 {
            remaining = &remaining[written as usize..];
            continue;
        }
        if written == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }

        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }

        let remaining_time = deadline.saturating_duration_since(Instant::now());
        if remaining_time.is_zero() {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let timeout_ms = remaining_time.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: raw_fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd and the fd remains owned.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
    }
    Ok(())
}
/// Touchpad scrolling without stop events (nested under winit) ends after
/// this long without motion.
const SCROLL_IDLE_MS: u32 = 50;

/// An egui context plus its GL painter (created once GL is available).
struct Egui {
    ctx: egui::Context,
    painter: Option<egui_glow::Painter>,
}

impl Egui {
    /// A context whose requests to be drawn again go to `repaint`.
    fn new(repaint: &Repaint) -> Self {
        let ctx = egui::Context::default();
        repaint.watch(&ctx);
        Self { ctx, painter: None }
    }

    /// A context that also builds an AccessKit tree each frame.
    fn accessible(repaint: &Repaint) -> Self {
        let egui = Self::new(repaint);
        egui.ctx.enable_accesskit();
        egui
    }
}

/// When the soonest of the host's egui contexts wants to be drawn again.
/// egui asks from inside a pass, for an animation's next step or a
/// tooltip's delay, and from other threads, as an app does when work it
/// waited on finishes; each context's repaint callback brings both here.
#[derive(Clone, Default)]
struct Repaint(Arc<Mutex<Option<Instant>>>);

impl Repaint {
    fn watch(&self, ctx: &egui::Context) {
        let due = self.0.clone();
        ctx.set_request_repaint_callback(move |info| {
            // `Duration::MAX` and its like mean never.
            let Some(at) = Instant::now().checked_add(info.delay) else {
                return;
            };
            let mut due = due.lock().unwrap_or_else(|e| e.into_inner());
            *due = Some(due.map_or(at, |d| d.min(at)));
        });
    }

    /// Whether a context wants drawing by `by`; if so the request is spent.
    /// One further off stays for its own tick.
    fn take_due(&self, by: Instant) -> bool {
        let mut due = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let ready = due.is_some_and(|at| at <= by);
        if ready {
            *due = None;
        }
        ready
    }
}

/// Tessellated egui output with the texture updates it needs.
struct Pass {
    primitives: Vec<egui::ClippedPrimitive>,
    textures: egui::TexturesDelta,
}

/// Window content. Few windows exist, so the variant size gap is harmless.
#[allow(clippy::large_enum_variant)]
enum Content {
    /// A Wayland client toplevel.
    Wayland(Window),
    /// An in-process app instance with its own egui context and input queue.
    Internal {
        instance: InstanceId,
        egui: Egui,
        events: Vec<egui::Event>,
        /// When its queued presses and releases happened (`ClickTiming`).
        clicks: mcsapi_ui::ClickTiming,
        title: String,
        app_id: String,
        /// The app's accessibility tree from its last frame.
        access: Option<accesskit::TreeUpdate>,
    },
}

/// A running [`RuntimeClient`].
struct RuntimeChild {
    client: RuntimeClient,
    child: Child,
    started: Instant,
}

/// A toplevel from a runtime client with a panel or overlay role, placed by
/// the compositor instead of the shell.
struct Layer {
    window: Window,
    role: Role,
}

/// Who receives pointer input until all buttons are released.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Route {
    /// A runtime panel or overlay (a Wayland client).
    Layer,
    Chrome,
    Shell,
    Content(Option<WindowId>),
}

/// Who receives a touchpad gesture from its begin to its end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GestureRoute {
    Shell,
    Chrome,
    Internal(WindowId),
    Client,
}

/// Compositor state, generic over the desktop shell.
pub(crate) struct Host<S: Shell> {
    start: Instant,
    display: DisplayHandle,
    signal: LoopSignal,
    socket_name: OsString,

    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    _xdg_decoration_state: XdgDecorationState,
    shm_state: ShmState,
    _output_manager_state: OutputManagerState,
    _pointer_gestures_state: PointerGesturesState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    /// `zwp_linux_dmabuf_v1`, whose global exists only on the bare seat.
    #[cfg(feature = "kms")]
    dmabuf_state: DmabufState,
    #[cfg(feature = "kms")]
    dmabuf_global: Option<DmabufGlobal>,
    /// `wp_linux_drm_syncobj_v1`, where the device supports it (see
    /// `Kms::syncobj_device`).
    #[cfg(feature = "kms")]
    syncobj_state: Option<DrmSyncobjState>,
    /// For the sources that wake a commit waiting on its buffer's fences.
    #[cfg(feature = "kms")]
    loop_handle: LoopHandle<'static, Self>,
    /// `zwp_primary_selection_device_manager_v1`: middle-click paste.
    primary_selection_state: PrimarySelectionState,
    _hints: hints::Hints,
    toplevels: toplevel::Toplevels,
    inputs: input::Inputs<S>,
    cursors: cursor::Cursors,
    layer_shell: layer_shell::LayerShell,
    lock: lock::Lock,
    foreign: foreign::Foreign,
    screen_capture: capture::Captures,
    workspaces: workspaces::Workspaces,
    automation: automation::Automation,
    input_method: input_method::InputMethod,
    timing: timing::Timing,
    output_config: output_config::OutputConfig,
    security: security::Security<S>,
    responding: responding::Responding,
    text_inputs: TextInputs,
    popups: PopupManager,
    seat: Seat<Self>,
    space: Space<Window>,
    output: Output,
    backend: Backend,
    /// The offscreen frame, when [`Backend::offscreen`].
    scene: Option<GlesTexture>,
    /// What `scene` was drawn from, while it holds everything but the
    /// cursor (see [`scene_key`]). A frame built from the same passes and
    /// placements, with no client or shell change since, copies it to the
    /// screen again with the cursor where it is now instead of drawing it
    /// all: moving the pointer over a client, or over chrome that does not
    /// change under it, costs the copy of two cursor-sized areas.
    scene_key: Option<u64>,
    /// Where the cursor was drawn over the scene on the last frame copied
    /// to the screen, and what it showed ([`cursor::Drawn::id`]).
    shown_cursor: Option<(Rectangle<i32, Physical>, u64)>,
    /// The areas each of the last frames copied to the screen changed,
    /// newest first. A screen buffer last drawn `n` frames ago is brought up
    /// to date by copying the areas of the `n - 1` frames since and of this
    /// one, which after a cursor-only frame is a few cursor-sized squares.
    presented: VecDeque<Vec<Rectangle<i32, Physical>>>,
    gl: Option<Arc<glow::Context>>,
    retired: Vec<egui_glow::Painter>,
    blurrer: Option<blur::Blurrer>,
    /// Set once creating the blurrer failed, so it isn't retried every frame.
    blur_unavailable: bool,
    /// Where the windows are copied for [`Shell::window_pictures`].
    pictures: Option<Pictures>,
    /// Whether the shell asked for them this frame.
    wants_pictures: bool,
    vrr: bool,
    /// Refresh rate of the monitor showing the session window.
    refresh_mhz: u32,
    timing_checked: Option<Instant>,
    /// Frame times for the log, while it wants them.
    frames: FrameStats,

    shell: S,
    apps: Option<Box<dyn Apps>>,
    windows: HashMap<WindowId, Content>,
    /// Toplevels that have not committed yet.
    unmanaged: Vec<Window>,
    keyboard_focus: Option<WindowId>,

    chrome: Egui,
    decorations: Egui,
    /// Shared by every egui context's repaint callback.
    repaint: Repaint,
    /// Something on screen may have changed since the last frame: input
    /// arrived, a client sent a request, a job or command ran. A tick draws
    /// only when this holds, egui asked for a frame, or the shell needs one
    /// ([`Shell::needs_frame`]).
    dirty: bool,
    /// The pointer moved since the last frame. Unlike [`Host::dirty`] it
    /// does not by itself redraw the scene: the frame is built, and drawn
    /// only if the chrome or an in-process app shows something different
    /// for the move, such as a highlight under the pointer.
    moved: bool,
    /// A client sent a request since the last frame, such as a commit.
    /// Like `moved` it builds a frame without redrawing all of the scene:
    /// the surfaces are redrawn where their damage says, with whatever is
    /// above and below them there.
    requested: bool,
    /// Each surface as the scene last drew it, to tell what changed since.
    seen: HashMap<Id, Seen>,
    chrome_events: Vec<egui::Event>,
    /// When the chrome's queued presses and releases happened.
    chrome_clicks: mcsapi_ui::ClickTiming,
    /// The chrome's accessibility tree from its last frame.
    chrome_access: Option<accesskit::TreeUpdate>,
    merger: a11y::Merger,
    #[cfg(feature = "atspi")]
    atspi: Option<crate::atspi::Bridge>,
    /// [`Command::Capture`] requests waiting for the next frame.
    captures: Vec<u64>,
    /// Agent commands, carried out one per frame: egui sees only one click
    /// per frame, and a tree or capture taken right after input should show
    /// its result.
    agent_queue: VecDeque<Command>,
    /// Frames to wait after injected input before reading the tree or the
    /// screen: one for the app to handle it, one to draw the result.
    settle: u8,
    /// Whether the latest input was injected; see [`Shell::input_source`].
    synthetic: bool,
    pointer: Point<f64, Logical>,
    route: Option<Route>,
    buttons: u32,
    pointer_target: Option<WindowId>,
    /// A content press was cut off by [`Host::cancel_pointer_route`]; its
    /// releases still go to Smithay so its pressed-button state clears.
    cancelled_press: bool,
    egui_mods: egui::Modifiers,
    gesture: Option<GestureRoute>,
    gesture_bridge: EguiBridge,
    /// The in-process app being scrolled with fingers, and when it last moved.
    scroll: Option<(WindowId, u32)>,
    /// The touch point driving the pointer, from its down to its up.
    touch: Option<TouchSlot>,
    /// Keys whose press the shell consumed; their release is consumed too.
    consumed_keys: HashSet<u32>,

    children: Vec<Child>,

    /// Runtime clients this compositor started.
    runtime: Vec<RuntimeChild>,
    /// Their mapped panels and overlays, bottom to top.
    layers: Vec<Layer>,
    /// The panel a click gave the keyboard to ([`Role::Panel`] `keyboard`).
    layer_focus: Option<WlSurface>,
    /// The surface the keyboard was last given to.
    keyboard_surface: Option<WlSurface>,
    /// What the shell was last told panels cover.
    reserved: Reserved,
}

/// Per-client Wayland state.
#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
    /// The role of a runtime client, fixed when the compositor created its
    /// connection; `None` for clients of the public socket.
    role: Option<Role>,
    /// The sandbox a client connected through, which keeps it from the
    /// privileged globals (see `security`).
    sandbox: Option<crate::Sandbox>,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Where frames go and input comes from.
enum Backend {
    /// A window of another Wayland or X11 session.
    Winit(Box<WinitGraphicsBackend<GlesRenderer>>),
    /// The bare seat: DRM/KMS, libinput and libseat.
    #[cfg(feature = "kms")]
    Kms(Box<crate::kms::Kms>),
}

impl Backend {
    fn size(&self) -> Size<i32, Physical> {
        match self {
            Self::Winit(b) => b.window_size(),
            #[cfg(feature = "kms")]
            Self::Kms(k) => k.size(),
        }
    }

    fn renderer(&mut self) -> &mut GlesRenderer {
        match self {
            Self::Winit(b) => b.renderer(),
            #[cfg(feature = "kms")]
            Self::Kms(k) => &mut k.renderer,
        }
    }

    /// The refresh rate of the monitor showing the session window, checked
    /// again each time (the window can move between monitors). `None` on the
    /// bare seat, where the mode is the refresh rate and never changes.
    fn monitor_refresh(&self) -> Option<u32> {
        match self {
            Self::Winit(b) => Some(
                b.window()
                    .current_monitor()
                    .and_then(|m| m.refresh_rate_millihertz())
                    .filter(|&r| r >= 1_000)
                    .unwrap_or(60_000),
            ),
            #[cfg(feature = "kms")]
            Self::Kms(_) => None,
        }
    }

    /// Whether a frame may be drawn now: on the bare seat, only while the
    /// seat is ours and fewer than two frames wait for the screen.
    fn can_draw(&self) -> bool {
        match self {
            Self::Winit(_) => true,
            #[cfg(feature = "kms")]
            Self::Kms(k) => k.can_draw(),
        }
    }

    /// Whether the frame is drawn into an offscreen texture and copied to the
    /// screen afterwards (see `kms.rs`). Always on the bare seat; in a window
    /// only with `MCSAPI_OFFSCREEN=1`, which exercises the same path where it
    /// can be looked at.
    fn offscreen(&self) -> bool {
        match self {
            Self::Winit(_) => std::env::var_os("MCSAPI_OFFSCREEN").is_some_and(|v| v == "1"),
            #[cfg(feature = "kms")]
            Self::Kms(_) => true,
        }
    }
}

/// How the offscreen frame is turned when it is copied to the screen. It was
/// drawn as for a window, with raw GL painting row 0 at the bottom, and
/// smithay reads a texture with row 0 at the top, so the copy flips it back.
/// The frame's own transform (flipped for a window, normal for a scanout
/// buffer) accounts for the target, so the same flip serves both, and
/// `MCSAPI_OFFSCREEN=1` in a window shows what the bare seat will.
const OFFSCREEN_COPY: Transform = Transform::Flipped180;

pub(crate) fn run<S: Shell + 'static>(config: Compositor<S>) -> Result {
    let Compositor {
        shell,
        apps,
        size: (w, h),
        title,
        vrr,
        launch,
        runtime,
        jobs,
    } = config;
    let merger = a11y::Merger::new(title.clone());
    let mut event_loop: EventLoop<Host<S>> = EventLoop::try_new()?;
    let display: Display<Host<S>> = Display::new()?;
    let dh = display.handle();

    #[cfg(feature = "kms")]
    let mut kms_sources = None;
    #[cfg(feature = "kms")]
    let kms = crate::kms::wanted();
    #[cfg(not(feature = "kms"))]
    let kms = false;
    let (backend, winit_loop) = if kms {
        #[cfg(feature = "kms")]
        {
            let (kms, sources) = crate::kms::Kms::open()?;
            kms_sources = Some(sources);
            (Backend::Kms(Box::new(kms)), None)
        }
        #[cfg(not(feature = "kms"))]
        unreachable!()
    } else {
        let (backend, events) = winit::init_from_attributes::<GlesRenderer>(
            WinitWindow::default_attributes()
                .with_title(title)
                .with_inner_size(LogicalSize::new(w, h))
                .with_visible(true),
        )
        .map_err(|e| format!("cannot open a window for the session: {e}"))?;
        (Backend::Winit(Box::new(backend)), Some(events))
    };
    let size = backend.size();
    let refresh_mhz = match &backend {
        #[cfg(feature = "kms")]
        Backend::Kms(k) => k.refresh_mhz(),
        _ => 60_000,
    };

    let output = Output::new(
        "mcsapi-0".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "mcsapi".into(),
            model: if kms { "display" } else { "nested" }.into(),
        },
    );
    let _global = output.create_global::<Host<S>>(&dh);
    let mode = OutputMode {
        size,
        refresh: refresh_mhz as i32,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "seat0");
    seat.add_keyboard(XkbConfig::default(), 400, 30)?;
    seat.add_pointer();

    let mut space = Space::default();
    space.map_output(&output, (0, 0));

    let socket = ListeningSocketSource::new_auto()?;
    let socket_name = socket.socket_name().to_os_string();
    event_loop
        .handle()
        .insert_source(socket, |stream, _, host| {
            if let Err(e) = host
                .display
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                warn!(error = %e, "rejected client");
            }
        })?;
    event_loop.handle().insert_source(
        Generic::new(display, Interest::READ, CalloopMode::Level),
        |_, display, host| {
            // Most requests change what is shown, and an idle client sends
            // none, so any of them is reason enough to build a frame. What
            // they change is on their surfaces, which `draw` redraws where
            // they say they changed, or in the placements and passes the
            // frame is built from; what else they change marks the frame
            // `dirty` where it is handled (a session lock, display power).
            host.requested = true;
            // SAFETY: the display outlives the event loop's sources.
            unsafe { display.get_mut().dispatch_clients(host)? };
            Ok(PostAction::Continue)
        },
    )?;

    // Buffers from the GPU, and explicit fences for them, on the bare seat
    // only: the feedback names the DRM device clients should allocate on,
    // and the nested window has none.
    #[cfg(feature = "kms")]
    let mut dmabuf_state = DmabufState::new();
    #[cfg(feature = "kms")]
    let (dmabuf_global, syncobj_state) = match &backend {
        Backend::Kms(k) => {
            let global = match k.dmabuf_feedback() {
                Ok(feedback) => Some(
                    dmabuf_state.create_global_with_default_feedback::<Host<S>>(&dh, &feedback),
                ),
                // Clients still draw into shared memory, in software.
                Err(e) => {
                    warn!(error = %e, "cannot offer dma-buf buffers");
                    None
                }
            };
            // The protocol only carries fences for dma-buf buffers.
            let syncobj = global
                .as_ref()
                .and(k.syncobj_device())
                .map(|device| DrmSyncobjState::new::<Host<S>>(&dh, device));
            (global, syncobj)
        }
        _ => (None, None),
    };

    let primary_selection_state = PrimarySelectionState::new::<Host<S>>(&dh);
    let repaint = Repaint::default();
    let mut host = Host {
        start: Instant::now(),
        display: dh.clone(),
        signal: event_loop.get_signal(),
        socket_name,
        compositor_state: CompositorState::new::<Host<S>>(&dh),
        xdg_shell_state: XdgShellState::new::<Host<S>>(&dh),
        _xdg_decoration_state: XdgDecorationState::new::<Host<S>>(&dh),
        // Beside the two every compositor has, the byte orders screen
        // captures can be written in (`capture::FORMATS`): a client sharing
        // the screen allocates its buffers in whichever it negotiated.
        shm_state: ShmState::new::<Host<S>>(
            &dh,
            vec![wl_shm::Format::Xbgr8888, wl_shm::Format::Abgr8888],
        ),
        _output_manager_state: OutputManagerState::new_with_xdg_output::<Host<S>>(&dh),
        _pointer_gestures_state: PointerGesturesState::new::<Host<S>>(&dh),
        data_device_state: DataDeviceState::new::<Host<S>>(&dh),
        #[cfg(feature = "kms")]
        dmabuf_state,
        #[cfg(feature = "kms")]
        dmabuf_global,
        #[cfg(feature = "kms")]
        syncobj_state,
        #[cfg(feature = "kms")]
        loop_handle: event_loop.handle(),
        automation: automation::Automation::new::<S>(&dh, &primary_selection_state),
        primary_selection_state,
        _hints: {
            effects::create_global::<S>(&dh);
            hints::Hints::new::<S>(&dh)
        },
        toplevels: toplevel::Toplevels::new::<S>(&dh),
        inputs: input::Inputs::new(&dh, event_loop.handle()),
        cursors: cursor::Cursors::new::<S>(&dh),
        layer_shell: layer_shell::LayerShell::new::<S>(&dh),
        lock: lock::Lock::new::<S>(&dh),
        foreign: foreign::Foreign::new::<S>(&dh),
        screen_capture: capture::Captures::new::<S>(&dh),
        workspaces: workspaces::Workspaces::new::<S>(&dh),
        input_method: input_method::InputMethod::new::<S>(&dh),
        timing: timing::Timing::new::<S>(&dh),
        output_config: output_config::OutputConfig::new::<S>(&dh),
        security: security::Security::new(&dh, event_loop.handle()),
        responding: responding::Responding::default(),
        text_inputs: {
            TextInputs::global::<S>(&dh);
            TextInputs::default()
        },
        seat_state,
        popups: PopupManager::default(),
        seat,
        space,
        output,
        backend,
        scene: None,
        scene_key: None,
        shown_cursor: None,
        presented: VecDeque::new(),
        gl: None,
        retired: Vec::new(),
        blurrer: None,
        blur_unavailable: false,
        pictures: None,
        wants_pictures: false,
        vrr,
        refresh_mhz,
        timing_checked: None,
        frames: FrameStats::default(),
        shell,
        apps,
        windows: HashMap::new(),
        unmanaged: Vec::new(),
        keyboard_focus: None,
        chrome: Egui::accessible(&repaint),
        decorations: Egui::new(&repaint),
        repaint,
        dirty: true,
        moved: false,
        requested: false,
        seen: HashMap::new(),
        chrome_events: Vec::new(),
        chrome_clicks: Default::default(),
        chrome_access: None,
        merger,
        #[cfg(feature = "atspi")]
        atspi: None,
        captures: Vec::new(),
        agent_queue: VecDeque::new(),
        settle: 0,
        synthetic: false,
        pointer: (0.0, 0.0).into(),
        route: None,
        buttons: 0,
        pointer_target: None,
        cancelled_press: false,
        egui_mods: egui::Modifiers::default(),
        gesture: None,
        gesture_bridge: EguiBridge::default(),
        scroll: None,
        touch: None,
        consumed_keys: HashSet::new(),
        children: Vec::new(),
        runtime: Vec::new(),
        layers: Vec::new(),
        layer_focus: None,
        keyboard_surface: None,
        reserved: Reserved::default(),
    };
    #[cfg(feature = "atspi")]
    {
        let (sender, requests) = channel::channel();
        host.atspi = Some(crate::atspi::Bridge::new(sender));
        event_loop
            .handle()
            .insert_source(requests, |event, _, host| {
                if let channel::Event::Msg(request) = event {
                    host.dirty = true;
                    host.atspi_request(request);
                    host.run_commands();
                }
            })?;
    }
    host.shell.set_output((size.w, size.h));
    host.shell
        .session_started(&host.socket_name.to_string_lossy());

    if let Some(winit_loop) = winit_loop {
        event_loop
            .handle()
            .insert_source(winit_loop, |event, _, host| host.winit_event(event))?;
    }
    #[cfg(feature = "kms")]
    if let Some(sources) = kms_sources {
        use smithay::backend::{drm::DrmEvent, session::Event as SessionEvent};
        let handle = event_loop.handle();
        handle.insert_source(sources.input, |event, _, host| {
            host.input(event);
            host.run_commands();
        })?;
        handle.insert_source(sources.drm, |event, metadata, host| match event {
            DrmEvent::VBlank(crtc) => {
                let now = host.timing.clock.now();
                if let Backend::Kms(k) = &mut host.backend {
                    k.vblank(crtc, metadata.take(), now);
                }
            }
            DrmEvent::Error(e) => error!(error = %e, "DRM error"),
        })?;
        handle.insert_source(sources.session, |event, _, host| {
            host.dirty = true;
            if let Backend::Kms(k) = &mut host.backend {
                match event {
                    SessionEvent::PauseSession => k.pause(),
                    SessionEvent::ActivateSession => k.resume(),
                }
            }
        })?;
    }
    if let Some(jobs) = jobs {
        event_loop.handle().insert_source(jobs, |event, _, host| {
            if let channel::Event::Msg(job) = event {
                let job: Job<S> = job;
                host.dirty = true;
                job(&mut host.shell);
                host.run_commands();
            }
        })?;
    }
    event_loop.handle().insert_source(
        Timer::from_duration(Duration::from_millis(300)),
        move |_, _, host| {
            for client in &runtime {
                host.spawn_runtime(client.clone());
            }
            for app in &launch {
                host.launch(app);
            }
            TimeoutAction::Drop
        },
    )?;
    event_loop.handle().insert_source(
        Timer::from_duration(Duration::from_secs(1)),
        |_, _, host| {
            host.reap_children();
            host.check_responding();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        },
    )?;
    event_loop
        .handle()
        .insert_source(Timer::immediate(), |deadline, _, host| {
            let start = Instant::now();
            let tick = host.render();
            let interval = host.frame_interval();
            if FrameStats::enabled() {
                match tick {
                    Tick::Drawn => host.frames.drawn(start, start.elapsed()),
                    Tick::Cursor => host.frames.cursor(start, start.elapsed()),
                    Tick::Busy => host.frames.skipped(),
                    Tick::Idle => host.frames.idle(),
                }
                host.frames.report(Instant::now(), interval);
            }
            TimeoutAction::ToInstant(next_tick(deadline, interval, Instant::now()))
        })?;

    let run_result = event_loop.run(None, &mut host, |host| {
        host.space.refresh();
        host.popups.cleanup();
        let _ = host.display.flush_clients();
    });
    for child in host
        .children
        .iter_mut()
        .chain(host.runtime.iter_mut().map(|r| &mut r.child))
    {
        let _ = child.kill();
        let _ = child.wait();
    }
    run_result?;
    Ok(())
}

/// What one tick of the frame timer did.
enum Tick {
    /// Drew a frame.
    Drawn,
    /// Copied the frame drawn before with the cursor somewhere else.
    Cursor,
    /// Drew nothing: the display was off or had frames enough queued.
    Busy,
    /// Drew nothing: nothing on screen changed.
    Idle,
}

/// When the frame after the one due at `deadline` is due: a whole number
/// of `interval`s on, so frames keep the rate the shell asked for however
/// long each takes to draw. Counting from the end of drawing instead added
/// the drawing time to every frame: 16.7 ms frames that took 6 ms to draw
/// came at 44 fps, not 60. Ticks a slow frame overran are skipped rather
/// than drawn back to back, and after a long stall (a suspend, a VT switch)
/// the count starts again from now.
pub(crate) fn next_tick(deadline: Instant, interval: Duration, now: Instant) -> Instant {
    let next = deadline + interval;
    if next > now {
        return next;
    }
    let behind = now - deadline;
    if behind > Duration::from_secs(1) {
        return now + interval;
    }
    let missed = behind.as_nanos() / interval.as_nanos().max(1);
    deadline + interval * (missed as u32 + 1)
}

impl<S: Shell> Host<S> {
    fn now_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Carries out commands the shell queued.
    fn run_commands(&mut self) {
        loop {
            let commands = self.shell.take_commands();
            if commands.is_empty() {
                return;
            }
            self.dirty = true;
            for command in commands {
                match command {
                    Command::Launch(app) => self.launch(&app),
                    Command::Close(window) => self.close(window),
                    Command::Runtime(client) => self.spawn_runtime(client),
                    Command::Quit => self.signal.stop(),
                    Command::Keymap {
                        layout,
                        variant,
                        options,
                    } => self.set_keymap(&layout, &variant, &options),
                    Command::Sandbox { socket, sandbox } => self.security.listen(socket, sandbox),
                    Command::CloseSandbox(socket) => self.security.close(&socket),
                    command @ (Command::Describe(_) | Command::Capture(_)) => {
                        // The shell may just have changed; show it drawn.
                        self.settle = self.settle.max(2);
                        self.agent_queue.push_back(command);
                    }
                    command @ (Command::Act { .. } | Command::Input(_)) => {
                        self.agent_queue.push_back(command);
                    }
                    // Commands added later are ignored until handled.
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
            }
        }
    }

    /// Compiles and installs a new keymap; clients get it with their next
    /// key event. A name xkbcommon does not know keeps the current keymap.
    fn set_keymap(&mut self, layout: &str, variant: &str, options: &str) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let config = XkbConfig {
            layout,
            variant,
            options: (!options.is_empty()).then(|| options.to_owned()),
            ..XkbConfig::default()
        };
        if let Err(e) = keyboard.set_xkb_config(self, config) {
            warn!(?layout, ?variant, error = ?e, "cannot set keymap");
            return;
        }
        self.send_input_method_keymap();
    }

    fn launch(&mut self, name: &str) {
        self.dirty = true;
        if let Some(apps) = &mut self.apps
            && let Some(app) = apps.resolve(name)
        {
            match apps.launch(&app) {
                Ok(instance) => {
                    let title = apps
                        .app_mut(instance)
                        .map(|a| a.title().to_owned())
                        .unwrap_or_default();
                    let id = self.shell.map_window(app.as_str(), &title);
                    self.windows.insert(
                        id,
                        Content::Internal {
                            instance,
                            egui: Egui::accessible(&self.repaint),
                            events: Vec::new(),
                            clicks: Default::default(),
                            title,
                            app_id: app.as_str().to_owned(),
                            access: None,
                        },
                    );
                }
                Err(e) => warn!(%name, error = %e, "cannot launch"),
            }
            return;
        }
        let argv = self.shell.spawn_argv(name);
        let Some((program, args)) = argv.split_first() else {
            return;
        };
        let lang = std::env::var("LANG")
            .ok()
            .filter(|l| l.to_uppercase().contains("UTF-8"))
            .unwrap_or_else(|| "C.UTF-8".into());
        // The launch is the user's action, so the window it opens may take
        // focus (`xdg_activation_v1`); DESKTOP_STARTUP_ID is the older name
        // GTK 3 also reads.
        let token = self.toplevels.launch_token();
        let spawned = Process::new(program)
            .args(args)
            .env("XDG_ACTIVATION_TOKEN", &token)
            .env("DESKTOP_STARTUP_ID", &token)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("GDK_BACKEND", "wayland")
            .env("QT_QPA_PLATFORM", "wayland")
            .env("LANG", lang)
            .env_remove("DISPLAY")
            .spawn();
        match spawned {
            Ok(child) => self.children.push(child),
            Err(e) => warn!(%name, error = %e, "cannot launch"),
        }
    }

    fn reap_children(&mut self) {
        self.children
            .retain_mut(|child| child.try_wait().map_or(true, |status| status.is_none()));
        let now = Instant::now();
        let mut restart = Vec::new();
        self.runtime.retain_mut(|r| {
            let exited = r.child.try_wait().is_ok_and(|status| status.is_some());
            if exited {
                if r.client.restarts_after(r.started, now) {
                    restart.push(r.client.clone());
                } else if r.client.restart {
                    warn!(
                        argv = ?r.client.argv,
                        within = ?RuntimeClient::MIN_UPTIME,
                        "exited soon after starting; not restarting it"
                    );
                }
            }
            !exited
        });
        for client in restart {
            self.spawn_runtime(client);
        }
    }

    /// Starts `client` on a private connection that carries its role.
    fn spawn_runtime(&mut self, client: RuntimeClient) {
        let Some((program, args)) = client.argv.split_first() else {
            return;
        };
        let (ours, theirs) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(e) => {
                warn!(%program, error = %e, "cannot start");
                return;
            }
        };
        let state = ClientState {
            role: Some(client.role),
            ..ClientState::default()
        };
        if let Err(e) = self.display.insert_client(ours, Arc::new(state)) {
            warn!(%program, error = %e, "cannot start");
            return;
        }
        let fd = theirs.as_raw_fd();
        let mut process = Process::new(program);
        process
            .args(args)
            // libwayland connects to WAYLAND_SOCKET before WAYLAND_DISPLAY,
            // and closes the fd once connected.
            .env("WAYLAND_SOCKET", fd.to_string())
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("MCSAPI_ROLE", client.role.as_str())
            .env("XDG_SESSION_TYPE", "wayland")
            .env_remove("DISPLAY");
        // SAFETY: fcntl is async-signal-safe and touches only the child's
        // copy of `fd`, which stays open in the parent until spawn returns.
        unsafe {
            process.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        match process.spawn() {
            Ok(child) => self.runtime.push(RuntimeChild {
                client,
                child,
                started: Instant::now(),
            }),
            Err(e) => warn!(%program, error = %e, "cannot start"),
        }
        // The child has its copy; ours closes here.
        drop(theirs);
    }

    /// The role a surface's client was started with, if it is a runtime
    /// client.
    fn role_of(&self, surface: &WlSurface) -> Option<Role> {
        let client = self.display.get_client(surface.id()).ok()?;
        client.get_data::<ClientState>()?.role
    }

    /// Places panels and overlays on the output and tells the shell what
    /// panels cover.
    fn arrange_layers(&mut self) {
        let size = self.backend.size();
        for layer in &self.layers {
            let Some(g) = layer.role.geometry((size.w, size.h)) else {
                continue;
            };
            if let Some(toplevel) = layer.window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some(g.size);
                    state.states.set(ToplevelState::Activated);
                });
                if toplevel.is_initial_configure_sent() {
                    toplevel.send_pending_configure();
                }
            }
            let offset = layer.window.geometry().loc;
            self.space
                .map_element(layer.window.clone(), g.loc - offset, false);
            self.space.raise_element(&layer.window, false);
        }
        // Runtime panels and layer-shell surfaces both start from the
        // output's edges, so the larger of the two covers each edge.
        let panels = Reserved::of(self.layers.iter().map(|l| l.role));
        let wlr = self.arrange_wlr_layers();
        self.lock
            .resize((self.backend.size().w, self.backend.size().h));
        let reserved = Reserved {
            top: panels.top.max(wlr.top),
            bottom: panels.bottom.max(wlr.bottom),
            left: panels.left.max(wlr.left),
            right: panels.right.max(wlr.right),
        };
        if reserved != self.reserved {
            self.reserved = reserved;
            self.shell.set_reserved(reserved);
        }
    }

    /// The topmost panel or overlay under the pointer. An overlay covers the
    /// whole output, so it takes everything.
    fn layer_under(&self) -> Option<&Layer> {
        let size = self.backend.size();
        let (x, y) = self.point();
        self.layers.iter().rev().find(|layer| {
            layer.role.geometry((size.w, size.h)).is_some_and(|g| {
                x >= g.loc.x && y >= g.loc.y && x < g.loc.x + g.size.w && y < g.loc.y + g.size.h
            })
        })
    }

    /// The overlay that has the keyboard, if one is mapped.
    fn overlay(&self) -> Option<&Layer> {
        self.layers.iter().rev().find(|l| l.role == Role::Overlay)
    }

    fn close(&mut self, window: WindowId) {
        match self.windows.get(&window) {
            Some(Content::Wayland(w)) => {
                if let Some(toplevel) = w.toplevel() {
                    toplevel.send_close();
                }
            }
            Some(Content::Internal { instance, .. }) => {
                let instance = *instance;
                if let Some(apps) = &mut self.apps {
                    apps.stop(instance);
                }
                if let Some(Content::Internal {
                    egui:
                        Egui {
                            painter: Some(painter),
                            ..
                        },
                    ..
                }) = self.windows.remove(&window)
                {
                    self.retired.push(painter);
                }
                self.shell.unmap_window(window);
            }
            None => {}
        }
    }

    fn wayland_window_of(&self, surface: &WlSurface) -> Option<(WindowId, &Window)> {
        self.windows.iter().find_map(|(id, content)| match content {
            Content::Wayland(w) if w.toplevel().is_some_and(|t| t.wl_surface() == surface) => {
                Some((*id, w))
            }
            _ => None,
        })
    }

    /// Pushes placements to toplevels and the space, and syncs keyboard focus.
    fn sync(&mut self) {
        let placements = self.shell.placements();
        for (id, content) in &self.windows {
            if let Content::Wayland(window) = content
                && !placements.iter().any(|p| p.window == *id)
            {
                self.space.unmap_elem(window);
            }
        }
        // The window a press went to, taken away mid-press (a lock screen
        // hiding everything, a workspace switch), must not keep receiving
        // the rest of that press through the click grab. A press on a
        // window that stays is left alone.
        if let Some(Route::Content(Some(id))) = self.route
            && !placements.iter().any(|p| p.window == id)
        {
            self.cancel_pointer_route();
        }
        for p in &placements {
            let Some(Content::Wayland(window)) = self.windows.get(&p.window) else {
                continue;
            };
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some(p.client.size);
                    let edges = p.tiled;
                    for (s, on) in [
                        (ToplevelState::TiledLeft, edges.left),
                        (ToplevelState::TiledRight, edges.right),
                        (ToplevelState::TiledTop, edges.top),
                        (ToplevelState::TiledBottom, edges.bottom),
                        (ToplevelState::Maximized, p.maximized),
                    ] {
                        if on {
                            state.states.set(s);
                        } else {
                            state.states.unset(s);
                        }
                    }
                    if p.focused {
                        state.states.set(ToplevelState::Activated);
                    } else {
                        state.states.unset(ToplevelState::Activated);
                    }
                });
                toplevel.send_pending_configure();
            }
            let offset = window.geometry().loc;
            self.space
                .map_element(window.clone(), p.client.loc - offset, false);
            self.space.raise_element(window, false);
        }

        self.arrange_layers();

        // The keyboard goes to an overlay while one is mapped, then to a
        // layer surface that holds it exclusively, then to a panel or layer
        // surface that was clicked, then to the shell's focused window.
        let focused = self.shell.focused();
        self.keyboard_focus = focused;
        let layer = self
            .overlay()
            .and_then(|l| l.window.toplevel())
            .map(|t| t.wl_surface().clone())
            .or_else(|| self.wlr_exclusive_keyboard())
            .or_else(|| self.layer_focus.clone());
        let surface = if self.lock.locked {
            self.lock.keyboard()
        } else {
            layer.or_else(|| {
                focused.and_then(|id| match self.windows.get(&id) {
                    Some(Content::Wayland(w)) => w.toplevel().map(|t| t.wl_surface().clone()),
                    _ => None,
                })
            })
        };
        if surface != self.keyboard_surface {
            self.keyboard_surface = surface.clone();
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
            }
        }
        self.update_idle_inhibit();
        self.update_foreign();
        self.update_workspaces();
        self.update_capture_sessions();
        self.place_input_popups();
        self.update_output_heads();
    }

    /// Ends a press that was going to content. The rest of it goes to the
    /// chrome; Wayland clients get a leave instead of motion and a release,
    /// and in-process apps see the pointer go.
    fn cancel_pointer_route(&mut self) {
        self.route = Some(Route::Chrome);
        self.cancelled_press = true;
        let target = self.pointer_target.take();
        if let Some(events) = self.internal_events(target) {
            events.push(egui::Event::PointerGone);
        }
        if let Some(pointer) = self.seat.get_pointer() {
            let serial = SERIAL_COUNTER.next_serial();
            let time = self.now_ms();
            pointer.unset_grab(self, serial, time);
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: self.pointer,
                    serial,
                    time,
                },
            );
            pointer.frame(self);
        }
    }

    fn winit_event(&mut self, event: WinitEvent) {
        // A resize, or the window shown again after being covered; input
        // says itself whether it changes the scene (see `input`).
        if !matches!(event, WinitEvent::Input(_)) {
            self.dirty = true;
        }
        match event {
            WinitEvent::Resized { size, .. } => {
                let mode = OutputMode {
                    size,
                    refresh: self.refresh_mhz as i32,
                };
                self.output
                    .change_current_state(Some(mode), None, None, None);
                self.output.set_preferred(mode);
                self.shell.set_output((size.w, size.h));
            }
            WinitEvent::Input(event) => self.input(event),
            WinitEvent::CloseRequested => self.signal.stop(),
            #[cfg(feature = "atspi")]
            WinitEvent::Focus(focused) => {
                if let Some(atspi) = &mut self.atspi
                    && !atspi.focused(focused)
                {
                    self.atspi = None;
                }
            }
            #[cfg(not(feature = "atspi"))]
            WinitEvent::Focus(_) => {}
            WinitEvent::Redraw => {}
        }
        self.run_commands();
    }

    fn point(&self) -> (i32, i32) {
        (self.pointer.x as i32, self.pointer.y as i32)
    }

    fn egui_pos(&self) -> egui::Pos2 {
        egui::pos2(self.pointer.x as f32, self.pointer.y as f32)
    }

    fn chrome_wants_pointer(&self) -> bool {
        self.shell.chrome_wants_pointer(self.point())
            || self.chrome.ctx.egui_wants_pointer_input()
            || egui::Popup::is_any_open(&self.chrome.ctx)
    }

    /// The topmost window whose content is under the pointer.
    fn content_under(&self) -> Option<WindowId> {
        let (x, y) = self.point();
        self.shell
            .placements()
            .into_iter()
            .rev()
            .find(|p| {
                let g = p.frame;
                x >= g.loc.x && y >= g.loc.y && x < g.loc.x + g.size.w && y < g.loc.y + g.size.h
            })
            .filter(|p| {
                let g = p.client;
                x >= g.loc.x && y >= g.loc.y && x < g.loc.x + g.size.w && y < g.loc.y + g.size.h
            })
            .map(|p| p.window)
    }

    fn internal_events(&mut self, window: Option<WindowId>) -> Option<&mut Vec<egui::Event>> {
        match self.windows.get_mut(&window?) {
            Some(Content::Internal { events, .. }) => Some(events),
            _ => None,
        }
    }

    /// Routes one input event. Generic so a libinput backend, which also
    /// reports touchpad gestures, can share it; winit reports none.
    fn input<B: InputBackend>(&mut self, event: InputEvent<B>) {
        // Input shows on screen through the chrome and in-process apps,
        // whose passes the frame compares, through the shell's placements,
        // also compared, or through the clients it reaches, which commit;
        // moving the pointer may change nothing but where the cursor is.
        self.moved = true;
        self.set_synthetic(false);
        self.seat_activity();
        match event {
            InputEvent::Keyboard { event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let pressed = event.state() == KeyState::Pressed;
                let keycode: u32 = event.key_code().into();
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };
                keyboard.input::<(), _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |host, modifiers, handle| {
                        host.filter_key(modifiers, &handle, pressed, keycode, Some(time))
                    },
                );
                // Typing into a window is waiting on it, as a click is.
                if pressed {
                    self.ping(self.keyboard_focus);
                }
            }
            // Nested, motion only comes as positions; the step between two
            // is the relative motion games and constraints work with.
            InputEvent::PointerMotionAbsolute { event } => {
                let size = self.backend.size();
                let to = event.position_transformed((size.w, size.h).into());
                let delta = to - self.inputs.absolute.replace(to).unwrap_or(self.pointer);
                self.pointer = self.relative_motion(to, delta, delta, Event::time(&event));
                self.pointer_motion(Event::time_msec(&event));
            }
            // A mouse or touchpad on the bare seat moves the pointer by
            // deltas; keep it on the output.
            InputEvent::PointerMotion { event } => {
                use smithay::backend::input::PointerMotionEvent as _;
                let size = self.backend.size();
                let delta = event.delta();
                let mut to = self.pointer + delta;
                to.x = to.x.clamp(0.0, f64::from(size.w - 1));
                to.y = to.y.clamp(0.0, f64::from(size.h - 1));
                self.pointer =
                    self.relative_motion(to, delta, event.delta_unaccel(), Event::time(&event));
                self.pointer_motion(Event::time_msec(&event));
            }
            InputEvent::PointerButton { event } => {
                self.pointer_button(event.button_code(), event.state(), Event::time_msec(&event));
            }
            // A touchscreen (a phone, or a laptop's screen) drives the pointer
            // with its first finger: down presses the primary button where it
            // lands, motion drags, and up or cancel releases. The chrome, the
            // shell's own hit testing and in-process apps then work by touch
            // as they do by mouse. Further fingers are ignored until the first
            // lifts. Clients get pointer events, not wl_touch.
            InputEvent::TouchDown { event } if self.touch.is_none() => {
                let size = self.backend.size();
                self.touch = Some(event.slot());
                self.pointer = event.position_transformed((size.w, size.h).into());
                let time = Event::time_msec(&event);
                self.pointer_motion(time);
                self.pointer_button(BTN_LEFT, ButtonState::Pressed, time);
            }
            InputEvent::TouchMotion { event } if self.touch == Some(event.slot()) => {
                let size = self.backend.size();
                self.pointer = event.position_transformed((size.w, size.h).into());
                self.pointer_motion(Event::time_msec(&event));
            }
            InputEvent::TouchUp { event } if self.touch == Some(event.slot()) => {
                self.touch = None;
                self.pointer_button(BTN_LEFT, ButtonState::Released, Event::time_msec(&event));
            }
            InputEvent::TouchCancel { event } if self.touch == Some(event.slot()) => {
                self.touch = None;
                self.pointer_button(BTN_LEFT, ButtonState::Released, Event::time_msec(&event));
            }
            InputEvent::PointerAxis { event } => {
                let amount = |axis| {
                    event
                        .amount(axis)
                        .or_else(|| event.amount_v120(axis).map(|v| v * 15.0 / 120.0))
                        .unwrap_or(0.0)
                };
                let (h, v) = (amount(Axis::Horizontal), amount(Axis::Vertical));
                self.axis(h, v, event.source(), Event::time_msec(&event));
            }
            InputEvent::GestureSwipeBegin { event } => self.gesture(
                GestureEvent::SwipeBegin {
                    fingers: event.fingers(),
                },
                Event::time_msec(&event),
            ),
            InputEvent::GestureSwipeUpdate { event } => self.gesture(
                GestureEvent::SwipeUpdate {
                    delta: egui::vec2(event.delta_x() as f32, event.delta_y() as f32),
                },
                Event::time_msec(&event),
            ),
            InputEvent::GestureSwipeEnd { event } => self.gesture(
                GestureEvent::SwipeEnd {
                    cancelled: event.cancelled(),
                },
                Event::time_msec(&event),
            ),
            InputEvent::GesturePinchBegin { event } => self.gesture(
                GestureEvent::PinchBegin {
                    fingers: event.fingers(),
                },
                Event::time_msec(&event),
            ),
            InputEvent::GesturePinchUpdate { event } => self.gesture(
                GestureEvent::PinchUpdate {
                    delta: egui::vec2(event.delta_x() as f32, event.delta_y() as f32),
                    scale: event.scale() as f32,
                    rotation: (event.rotation() as f32).to_radians(),
                },
                Event::time_msec(&event),
            ),
            InputEvent::GesturePinchEnd { event } => self.gesture(
                GestureEvent::PinchEnd {
                    cancelled: event.cancelled(),
                },
                Event::time_msec(&event),
            ),
            InputEvent::GestureHoldBegin { event } => self.gesture(
                GestureEvent::HoldBegin {
                    fingers: event.fingers(),
                },
                Event::time_msec(&event),
            ),
            InputEvent::GestureHoldEnd { event } => self.gesture(
                GestureEvent::HoldEnd {
                    cancelled: event.cancelled(),
                },
                Event::time_msec(&event),
            ),
            InputEvent::DeviceAdded { device } => {
                self.tablet_event::<B>(TabletInput::Added(device))
            }
            InputEvent::DeviceRemoved { device } => {
                self.tablet_event::<B>(TabletInput::Removed(device));
            }
            InputEvent::TabletToolAxis { event } => {
                self.tablet_event::<B>(TabletInput::Axis(event))
            }
            InputEvent::TabletToolProximity { event } => {
                self.tablet_event::<B>(TabletInput::Proximity(event));
            }
            InputEvent::TabletToolTip { event } => self.tablet_event::<B>(TabletInput::Tip(event)),
            InputEvent::TabletToolButton { event } => {
                self.tablet_event::<B>(TabletInput::Button(event));
            }
            _ => {}
        }
    }

    /// Carries out the next queued agent command, if it is its turn.
    fn agent_step(&mut self) {
        self.settle = self.settle.saturating_sub(1);
        let reads = matches!(
            self.agent_queue.front(),
            Some(Command::Describe(_) | Command::Capture(_))
        );
        if reads && self.settle > 0 {
            return;
        }
        match self.agent_queue.pop_front() {
            Some(Command::Describe(request)) => {
                let tree = self.build_tree();
                let snapshot = a11y::snapshot(&self.merger, &tree);
                self.shell.described(request, snapshot);
            }
            Some(Command::Capture(request)) => self.captures.push(request),
            Some(Command::Act {
                element,
                action,
                value,
            }) => {
                self.set_synthetic(true);
                self.act(accesskit::NodeId(element), action, value);
                self.settle = 2;
            }
            Some(Command::Input(input)) => {
                self.inject(input);
                self.settle = 2;
            }
            _ => {}
        }
        self.run_commands();
    }

    /// Tells the shell when input switches between injected and real.
    fn set_synthetic(&mut self, synthetic: bool) {
        if self.synthetic != synthetic {
            self.synthetic = synthetic;
            self.shell.input_source(synthetic);
        }
    }

    /// A window's title and app ID, as its client or app reports them.
    fn window_label(&self, window: WindowId) -> (String, String) {
        match self.windows.get(&window) {
            Some(Content::Internal { title, app_id, .. }) => (title.clone(), app_id.clone()),
            Some(Content::Wayland(w)) => w
                .toplevel()
                .map(|t| {
                    with_states(t.wl_surface(), |states| {
                        states
                            .data_map
                            .get::<XdgToplevelSurfaceData>()
                            .and_then(|d| d.lock().ok())
                            .map(|d| {
                                (
                                    d.title.clone().unwrap_or_default(),
                                    d.app_id.clone().unwrap_or_default(),
                                )
                            })
                            .unwrap_or_default()
                    })
                })
                .unwrap_or_default(),
            None => Default::default(),
        }
    }

    /// The merged accessibility tree of the chrome and the visible windows.
    fn build_tree(&mut self) -> accesskit::TreeUpdate {
        let placements = self.shell.placements();
        let labels: Vec<(String, String)> = placements
            .iter()
            .map(|p| self.window_label(p.window))
            .collect();
        let subtrees = self.shell.access_subtrees();
        let windows: Vec<a11y::WindowInfo<'_>> = placements
            .iter()
            .zip(&labels)
            .map(|(p, (title, app_id))| a11y::WindowInfo {
                window: p.window,
                title,
                app_id,
                frame: p.frame,
                focused: p.focused,
                content: match self.windows.get(&p.window) {
                    Some(Content::Internal { access, .. }) => access.as_ref(),
                    _ => None,
                },
            })
            .collect();
        self.merger
            .build(self.chrome_access.as_ref(), &windows, &subtrees)
    }

    /// An action from an AT-SPI client (a screen reader, or an agent
    /// driving the desktop through AT-SPI).
    #[cfg(feature = "atspi")]
    fn atspi_request(&mut self, request: accesskit::ActionRequest) {
        self.set_synthetic(true);
        let value = match &request.data {
            Some(accesskit::ActionData::Value(v)) => Some(v.to_string()),
            _ => None,
        };
        self.act(request.target_node, request.action, value);
    }

    /// Performs an accessibility action on a node of the merged tree.
    fn act(&mut self, node: accesskit::NodeId, action: accesskit::Action, value: Option<String>) {
        // What the lock hides cannot be acted on either.
        if self.lock.locked {
            return;
        }
        use a11y::Source;
        let Some(source) = self.merger.source(node) else {
            return;
        };
        let (window, target) = match source {
            Source::Root => return,
            Source::Window(window) => {
                if matches!(action, accesskit::Action::Focus | accesskit::Action::Click) {
                    self.shell.focus(window);
                }
                return;
            }
            Source::Shell(window, node, _) => {
                self.shell
                    .access_action(window, node, action, value.as_deref());
                return;
            }
            Source::Chrome(node) => (None, node),
            Source::App(window, node) => {
                // Keys only reach the focused app.
                self.shell.focus(window);
                (Some(window), node)
            }
        };
        let request = |action, data| {
            egui::Event::AccessKitActionRequest(accesskit::ActionRequest {
                action,
                target_tree: accesskit::TreeId::ROOT,
                target_node: target,
                data,
            })
        };
        let events = if action == accesskit::Action::SetValue {
            // Focus the field, select everything in it and type over it;
            // egui applies the focus before the field reads the keys.
            let select_all = egui::Modifiers::COMMAND;
            vec![
                request(accesskit::Action::Focus, None),
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: Some(egui::Key::A),
                    pressed: true,
                    repeat: false,
                    modifiers: select_all,
                },
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: Some(egui::Key::A),
                    pressed: false,
                    repeat: false,
                    modifiers: select_all,
                },
                egui::Event::Text(value.unwrap_or_default()),
            ]
        } else {
            vec![request(action, None)]
        };
        let queue = match window {
            None => Some(&mut self.chrome_events),
            Some(w) => self.internal_events(Some(w)),
        };
        if let Some(queue) = queue {
            queue.extend(events);
        }
    }

    pub(crate) fn text_inputs_mut(&mut self) -> &mut TextInputs {
        &mut self.text_inputs
    }

    /// Tells the shell when the focused text field starts or stops wanting
    /// text input.
    pub(crate) fn text_input_changed(&mut self) {
        if let Some(field) = self.text_inputs.changed() {
            self.shell.text_input(field);
        }
        self.update_input_method();
    }

    /// Feeds injected input through the same paths as the seat's.
    fn inject(&mut self, input: Input) {
        self.set_synthetic(true);
        let time = self.now_ms();
        let code = |button| match button {
            MouseButton::Left => BTN_LEFT,
            MouseButton::Right => BTN_RIGHT,
            MouseButton::Middle => BTN_MIDDLE,
        };
        match input {
            Input::Move { x, y } => self.move_pointer(x, y, time),
            Input::Button { button, pressed } => {
                let state = if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                self.pointer_button(code(button), state, time);
            }
            Input::Click { x, y, button } => {
                self.move_pointer(x, y, time);
                self.pointer_button(code(button), ButtonState::Pressed, time);
                self.pointer_button(code(button), ButtonState::Released, time);
            }
            Input::Scroll { dx, dy } => {
                self.axis(f64::from(dx), f64::from(dy), AxisSource::Wheel, time);
            }
            Input::Key { sym, mods } => {
                if !self.press_key(sym, mods, time) {
                    warn!(?sym, "no key for this keysym in the keymap");
                }
            }
            // A focused field that asked for text input gets the text as
            // one commit; line breaks and tabs stay keys, since they
            // usually mean "submit" or "next field" rather than text.
            Input::Text(text)
                if !text.contains(['\n', '\t']) && self.text_inputs.commit_string(&text) => {}
            Input::Text(text) => {
                for c in text.chars() {
                    let sym = match c {
                        '\n' => Keysym::Return,
                        '\t' => Keysym::Tab,
                        c => Keysym::from_char(c),
                    };
                    if !self.press_key(sym, Modifiers::default(), time) {
                        self.type_into_egui(c);
                    }
                }
            }
        }
    }

    fn move_pointer(&mut self, x: i32, y: i32, time: u32) {
        let size = self.backend.size();
        self.pointer = (
            f64::from(x.clamp(0, (size.w - 1).max(0))),
            f64::from(y.clamp(0, (size.h - 1).max(0))),
        )
            .into();
        self.pointer_motion(time);
    }

    /// Presses and releases the key that produces `sym`, holding `mods`
    /// (and Shift, if the layout puts `sym` on the shifted level). Returns
    /// `false` if the keymap has no such key.
    fn press_key(&mut self, sym: Keysym, mods: Modifiers, time: u32) -> bool {
        let Some((keycode, shifted)) = self.keycode_for(sym) else {
            return false;
        };
        let mut held = Vec::new();
        for (on, modifier) in [
            (mods.ctrl, Keysym::Control_L),
            (mods.alt, Keysym::Alt_L),
            (mods.logo, Keysym::Super_L),
            (mods.shift || shifted, Keysym::Shift_L),
        ] {
            if on && let Some((code, _)) = self.keycode_for(modifier) {
                held.push(code);
            }
        }
        for &code in &held {
            self.key_event(code, KeyState::Pressed, time);
        }
        self.key_event(keycode, KeyState::Pressed, time);
        self.key_event(keycode, KeyState::Released, time);
        for &code in held.iter().rev() {
            self.key_event(code, KeyState::Released, time);
        }
        true
    }

    /// The key producing `sym` in the active layout, and whether it needs
    /// Shift.
    fn keycode_for(&mut self, sym: Keysym) -> Option<(Keycode, bool)> {
        let keyboard = self.seat.get_keyboard()?;
        keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().ok()?;
            let layout = xkb.active_layout();
            // SAFETY: the keymap is only borrowed while the lock is held.
            let keymap = unsafe { xkb.keymap() };
            let mut found = None;
            keymap.key_for_each(|_, code| {
                if found.is_some() {
                    return;
                }
                for level in 0..2 {
                    if keymap
                        .key_get_syms_by_level(code, layout.0, level)
                        .contains(&sym)
                    {
                        found = Some((code, level == 1));
                        return;
                    }
                }
            });
            found
        })
    }

    fn key_event(&mut self, keycode: Keycode, state: KeyState, time: u32) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let pressed = state == KeyState::Pressed;
        let raw: u32 = keycode.into();
        keyboard.input::<(), _>(
            self,
            keycode,
            state,
            SERIAL_COUNTER.next_serial(),
            time,
            |host, modifiers, handle| host.filter_key(modifiers, &handle, pressed, raw, None),
        );
        if pressed {
            self.ping(self.keyboard_focus);
        }
    }

    /// Types a character the keymap lacks into the egui context that would
    /// receive keys: the chrome if it has keyboard focus, else the focused
    /// in-process app. Wayland clients only take keys, so they miss it.
    fn type_into_egui(&mut self, c: char) {
        let chrome = self.chrome.ctx.egui_wants_keyboard_input()
            || self.chrome.ctx.memory(|m| m.focused()).is_some();
        let focused = self.shell.focused();
        let events = if chrome {
            Some(&mut self.chrome_events)
        } else {
            self.internal_events(focused)
        };
        if let Some(events) = events {
            events.push(egui::Event::Text(c.to_string()));
        }
    }

    /// Scrolls whatever is under the pointer (or the chrome), from a wheel,
    /// a touchpad or injected input.
    fn axis(&mut self, h: f64, v: f64, source: AxisSource, time: u32) {
        let fingers = matches!(source, AxisSource::Finger | AxisSource::Continuous);
        // libinput ends finger scrolling with an empty event.
        let stop = fingers && h == 0.0 && v == 0.0;
        let modifiers = self.egui_mods;
        let wheel = |phase| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(-h as f32, -v as f32),
            phase,
            modifiers,
        };
        let on_layer = self.pointer_on_layer();
        if !on_layer && self.chrome_wants_pointer() {
            self.chrome_events.push(wheel(egui::TouchPhase::Move));
            return;
        }
        let under = if on_layer { None } else { self.content_under() };
        if fingers && let Some(window) = under.filter(|w| self.is_internal(*w)) {
            // Bracket finger scrolling in Start and End so apps can
            // follow it 1:1 and coast (mcsapi_ui::gesture).
            let now = self.now_ms();
            let ongoing = self.scroll.is_some_and(|(w, _)| w == window);
            if !ongoing {
                self.end_scroll();
            }
            let mut events = Vec::with_capacity(2);
            if !ongoing && !stop {
                events.push(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::Vec2::ZERO,
                    phase: egui::TouchPhase::Start,
                    modifiers,
                });
            }
            events.push(wheel(if stop {
                egui::TouchPhase::End
            } else {
                egui::TouchPhase::Move
            }));
            self.scroll = (!stop).then_some((window, now));
            if stop && !ongoing {
                return;
            }
            self.internal_events(Some(window))
                .expect("checked above")
                .extend(events);
            return;
        }
        if let Some(events) = self.internal_events(under) {
            events.push(wheel(egui::TouchPhase::Move));
            return;
        }
        let mut frame = AxisFrame::new(time).source(source);
        if h != 0.0 {
            frame = frame.value(Axis::Horizontal, h);
        }
        if v != 0.0 {
            frame = frame.value(Axis::Vertical, v);
        }
        if stop {
            frame = frame.stop(Axis::Horizontal).stop(Axis::Vertical);
        }
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.axis(self, frame);
            pointer.frame(self);
        }
    }

    fn is_internal(&self, window: WindowId) -> bool {
        matches!(self.windows.get(&window), Some(Content::Internal { .. }))
    }

    /// Ends finger scrolling of an in-process app.
    fn end_scroll(&mut self) {
        let Some((window, _)) = self.scroll.take() else {
            return;
        };
        self.dirty = true;
        let modifiers = self.egui_mods;
        if let Some(events) = self.internal_events(Some(window)) {
            events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::Vec2::ZERO,
                phase: egui::TouchPhase::End,
                modifiers,
            });
        }
    }

    /// Routes a touchpad gesture: the shell may take it at its begin;
    /// otherwise it goes to the chrome or the content under the pointer, and
    /// stays there until it ends.
    fn gesture(&mut self, event: GestureEvent, time: u32) {
        if event.is_begin() {
            self.end_scroll();
            self.gesture_bridge = EguiBridge::default();
            self.gesture = Some(if self.lock.locked {
                GestureRoute::Client
            } else if self.shell.gesture(&event) {
                GestureRoute::Shell
            } else if self.chrome_wants_pointer() {
                GestureRoute::Chrome
            } else {
                match self.content_under() {
                    Some(window) if self.is_internal(window) => GestureRoute::Internal(window),
                    _ => GestureRoute::Client,
                }
            });
        }
        let Some(route) = self.gesture else {
            return;
        };
        if event.is_end() {
            self.gesture = None;
        }
        match route {
            GestureRoute::Shell => {
                if !event.is_begin() {
                    self.shell.gesture(&event);
                }
            }
            GestureRoute::Chrome => {
                let events = self.gesture_bridge.events(event, self.egui_mods);
                self.chrome_events.extend(events);
            }
            GestureRoute::Internal(window) => {
                let events = self.gesture_bridge.events(event, self.egui_mods);
                if let Some(queue) = self.internal_events(Some(window)) {
                    queue.extend(events);
                }
            }
            GestureRoute::Client => self.client_gesture(event, time),
        }
    }

    /// Forwards a gesture to the focused Wayland client
    /// (`zwp_pointer_gestures_v1`).
    fn client_gesture(&mut self, event: GestureEvent, time: u32) {
        let Some(handle) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let delta = |d: egui::Vec2| Point::from((f64::from(d.x), f64::from(d.y)));
        match event {
            GestureEvent::SwipeBegin { fingers } => handle.gesture_swipe_begin(
                self,
                &pointer::GestureSwipeBeginEvent {
                    serial,
                    time,
                    fingers,
                },
            ),
            GestureEvent::SwipeUpdate { delta: d } => handle.gesture_swipe_update(
                self,
                &pointer::GestureSwipeUpdateEvent {
                    time,
                    delta: delta(d),
                },
            ),
            GestureEvent::SwipeEnd { cancelled } => handle.gesture_swipe_end(
                self,
                &pointer::GestureSwipeEndEvent {
                    serial,
                    time,
                    cancelled,
                },
            ),
            GestureEvent::PinchBegin { fingers } => handle.gesture_pinch_begin(
                self,
                &pointer::GesturePinchBeginEvent {
                    serial,
                    time,
                    fingers,
                },
            ),
            GestureEvent::PinchUpdate {
                delta: d,
                scale,
                rotation,
            } => handle.gesture_pinch_update(
                self,
                &pointer::GesturePinchUpdateEvent {
                    time,
                    delta: delta(d),
                    scale: f64::from(scale),
                    rotation: f64::from(rotation.to_degrees()),
                },
            ),
            GestureEvent::PinchEnd { cancelled } => handle.gesture_pinch_end(
                self,
                &pointer::GesturePinchEndEvent {
                    serial,
                    time,
                    cancelled,
                },
            ),
            GestureEvent::HoldBegin { fingers } => handle.gesture_hold_begin(
                self,
                &pointer::GestureHoldBeginEvent {
                    serial,
                    time,
                    fingers,
                },
            ),
            GestureEvent::HoldEnd { cancelled } => handle.gesture_hold_end(
                self,
                &pointer::GestureHoldEndEvent {
                    serial,
                    time,
                    cancelled,
                },
            ),
            _ => {}
        }
    }

    fn pointer_motion(&mut self, time: u32) {
        let pos = self.egui_pos();
        if self.route == Some(Route::Layer) || (self.route.is_none() && self.pointer_on_layer()) {
            self.chrome_events.push(egui::Event::PointerGone);
            let target = self.pointer_target.take();
            if let Some(events) = self.internal_events(target) {
                events.push(egui::Event::PointerGone);
            }
            let focus = self.surface_under();
            if let Some(pointer) = self.seat.get_pointer() {
                pointer.motion(
                    self,
                    focus,
                    &MotionEvent {
                        location: self.pointer,
                        serial: SERIAL_COUNTER.next_serial(),
                        time,
                    },
                );
                pointer.frame(self);
            }
            self.activate_constraint();
            return;
        }
        self.chrome_events.push(egui::Event::PointerMoved(pos));
        let route = self.route.unwrap_or(if self.chrome_wants_pointer() {
            Route::Chrome
        } else {
            Route::Content(None)
        });
        if route == Route::Shell {
            self.shell.pointer_motion(self.point());
        }
        let mut focus = None;
        let target = match self.route {
            Some(Route::Content(target)) => target,
            None if route == Route::Content(None) => self.content_under(),
            _ => None,
        };
        let target = target.filter(|id| {
            self.shell.placements().iter().any(|p| p.window == *id)
                && matches!(self.windows.get(id), Some(Content::Internal { .. }))
        });
        if self.pointer_target != target {
            if let Some(events) = self.internal_events(self.pointer_target) {
                events.push(egui::Event::PointerGone);
            }
            self.pointer_target = target;
        }
        if let Some(events) = self.internal_events(target) {
            events.push(egui::Event::PointerMoved(pos));
        } else if (route == Route::Content(None)
            || matches!(
                route,
                Route::Content(Some(id))
                    if matches!(self.windows.get(&id), Some(Content::Wayland(_)))
            ))
            && !matches!(
                self.content_under().and_then(|id| self.windows.get(&id)),
                Some(Content::Internal { .. })
            )
        {
            focus = self.surface_under();
        }
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.motion(
                self,
                focus,
                &MotionEvent {
                    location: self.pointer,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
            pointer.frame(self);
        }
        self.activate_constraint();
    }

    fn pointer_button(&mut self, button: u32, state: ButtonState, time: u32) {
        let pressed = state == ButtonState::Pressed;
        if pressed {
            self.buttons += 1;
        } else {
            self.buttons = self.buttons.saturating_sub(1);
        }
        let egui_button = match button {
            BTN_LEFT => Some(egui::PointerButton::Primary),
            BTN_RIGHT => Some(egui::PointerButton::Secondary),
            BTN_MIDDLE => Some(egui::PointerButton::Middle),
            _ => None,
        };
        if pressed && self.route.is_none() {
            // A click on a panel or layer surface that takes the keyboard
            // gives it the keyboard; a click anywhere else takes it back.
            let on_layer = self.pointer_on_layer();
            self.layer_focus = on_layer.then(|| self.layer_click_focus()).flatten();
            if on_layer {
                self.route = Some(Route::Layer);
                self.sync();
            }
        }
        if pressed && self.route.is_none() {
            self.route = Some(if self.chrome_wants_pointer() {
                Route::Chrome
            } else if button == BTN_LEFT {
                match self.shell.pointer_down(self.point(), u64::from(time)) {
                    Press::Handled => Route::Shell,
                    Press::Client => Route::Content(self.content_under()),
                }
            } else {
                if let Some(window) = self.content_under() {
                    self.shell.focus(window);
                }
                Route::Content(self.content_under())
            });
        }
        let pointer_event = egui_button.map(|button| egui::Event::PointerButton {
            pos: self.egui_pos(),
            button,
            pressed,
            modifiers: self.egui_mods,
        });
        match self.route.unwrap_or(Route::Content(None)) {
            Route::Layer => {
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state,
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                        },
                    );
                    pointer.frame(self);
                }
            }
            Route::Chrome => {
                if pointer_event.is_some() {
                    self.chrome_clicks.button(pressed, time);
                }
                self.chrome_events.extend(pointer_event);
                // The cancelled press's client lost focus, so this reaches
                // no surface; it only keeps Smithay's pressed buttons in
                // step, or its next click grab would never end.
                if self.cancelled_press
                    && !pressed
                    && let Some(pointer) = self.seat.get_pointer()
                {
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state,
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                        },
                    );
                    pointer.frame(self);
                }
            }
            Route::Shell => {
                if !pressed && self.buttons == 0 {
                    self.shell.pointer_up();
                }
            }
            Route::Content(target) => {
                let under = if self.route.is_some() {
                    target
                } else {
                    self.content_under()
                };
                if pressed {
                    self.ping(under);
                }
                if let Some(Content::Internal { events, clicks, .. }) =
                    under.and_then(|w| self.windows.get_mut(&w))
                {
                    if pointer_event.is_some() {
                        clicks.button(pressed, time);
                    }
                    events.extend(pointer_event);
                } else if let Some(pointer) = self.seat.get_pointer() {
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state,
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                        },
                    );
                    pointer.frame(self);
                }
            }
        }
        if self.buttons == 0 {
            self.route = None;
            self.cancelled_press = false;
            if !pressed {
                self.pointer_motion(time);
            }
        }
    }

    /// Routes a key: shell shortcut, chrome, an in-process app, or the
    /// focused Wayland client.
    fn filter_key(
        &mut self,
        modifiers: &ModifiersState,
        handle: &KeysymHandle<'_>,
        pressed: bool,
        keycode: u32,
        // When the key came from the seat; keys the shell types are
        // synthetic and never go to an input method.
        time: Option<u32>,
    ) -> FilterResult<()> {
        let consumed_release = !pressed && self.consumed_keys.remove(&keycode);
        self.egui_mods = egui::Modifiers {
            alt: modifiers.alt,
            ctrl: modifiers.ctrl,
            shift: modifiers.shift,
            mac_cmd: false,
            command: modifiers.ctrl,
        };
        let modified = handle.modified_sym();
        // Ctrl+Alt+F1–F12 on the bare seat: nothing else switches VTs once
        // the compositor owns the keyboard.
        #[cfg(feature = "kms")]
        if let Backend::Kms(k) = &mut self.backend {
            let first = Keysym::XF86_Switch_VT_1.raw();
            let raw = modified.raw();
            if (first..first + 12).contains(&raw) {
                if pressed {
                    k.change_vt((raw - first + 1) as i32);
                }
                return FilterResult::Intercept(());
            }
        }
        // A locked session's keys all go to the locker.
        if self.lock.locked {
            return FilterResult::Forward;
        }
        let sym = handle
            .raw_latin_sym_or_raw_current_sym()
            .unwrap_or(modified);
        let text = modified
            .key_char()
            .filter(|c| !c.is_control() && !modifiers.ctrl && !modifiers.alt && !modifiers.logo);
        let key = KeyInput {
            sym,
            text,
            pressed,
            mods: Modifiers {
                logo: modifiers.logo,
                shift: modifiers.shift,
                ctrl: modifiers.ctrl,
                alt: modifiers.alt,
            },
        };
        // What would go to a Wayland client goes to an input method's
        // keyboard grab instead while it is active.
        let forward = |host: &mut Self| match time {
            Some(time) if host.input_method_key(keycode, pressed, time) => {
                FilterResult::Intercept(())
            }
            _ => FilterResult::Forward,
        };
        // An overlay gets every key, shell shortcuts included; a panel or
        // layer surface holding the keyboard gets what the shell leaves.
        if self.overlay().is_some() {
            return forward(self);
        }
        if self.layer_focus.is_some() || self.wlr_exclusive_keyboard().is_some() {
            return match shell_key_route(&mut self.shell, &key, consumed_release) {
                KeyRoute::Consume => {
                    if pressed {
                        self.consumed_keys.insert(keycode);
                    }
                    FilterResult::Intercept(())
                }
                _ => forward(self),
            };
        }
        let chrome_focus = self.chrome.ctx.memory(|m| m.focused());
        // A client holding the shortcuts (a VM viewer, a remote desktop)
        // gets keys the shell would take; releases of presses the shell
        // took before still go to the shell.
        let shell_route = if self.shortcuts_inhibited() && !consumed_release {
            KeyRoute::Client
        } else {
            shell_key_route(&mut self.shell, &key, consumed_release)
        };
        let route = match shell_route {
            // A chrome widget focused from the keyboard (Tab) or by a
            // screen reader keeps the keys until Escape hands them back.
            KeyRoute::Client if chrome_focus.is_some() && pressed && sym == Keysym::Escape => {
                if let Some(id) = chrome_focus {
                    self.chrome.ctx.memory_mut(|m| m.surrender_focus(id));
                }
                self.consumed_keys.insert(keycode);
                return FilterResult::Intercept(());
            }
            KeyRoute::Client
                if self.chrome.ctx.egui_wants_keyboard_input() || chrome_focus.is_some() =>
            {
                KeyRoute::Chrome
            }
            route => route,
        };
        let mods = self.egui_mods;
        let events = match route {
            KeyRoute::Consume => {
                if pressed {
                    self.consumed_keys.insert(keycode);
                }
                return FilterResult::Intercept(());
            }
            KeyRoute::Chrome => &mut self.chrome_events,
            KeyRoute::Client => {
                let focused = self.shell.focused();
                match self.internal_events(focused) {
                    Some(events) => events,
                    None => return forward(self),
                }
            }
        };
        if let Some(egui_key) = egui_key(sym) {
            events.push(egui::Event::Key {
                key: egui_key,
                physical_key: Some(egui_key),
                pressed,
                repeat: false,
                modifiers: mods,
            });
        }
        if pressed && let Some(c) = text {
            events.push(egui::Event::Text(c.to_string()));
        }
        FilterResult::Intercept(())
    }

    fn timing(&self) -> OutputTiming {
        OutputTiming {
            refresh_mhz: self.refresh_mhz,
            vrr: self.vrr,
        }
    }

    /// Follows the refresh rate of the monitor the session window is on
    /// (checked about once a second; the window can move between monitors)
    /// and advertises it to clients through the output mode.
    fn update_refresh(&mut self) {
        if self
            .timing_checked
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.timing_checked = Some(Instant::now());
        // On the bare seat the mode is the refresh rate, set when it opened.
        let Some(refresh) = self.backend.monitor_refresh() else {
            return;
        };
        if refresh != self.refresh_mhz {
            self.refresh_mhz = refresh;
            if let Some(mode) = self.output.current_mode() {
                let mode = OutputMode {
                    refresh: refresh as i32,
                    ..mode
                };
                self.output
                    .change_current_state(Some(mode), None, None, None);
                self.output.set_preferred(mode);
            }
        }
    }

    /// The time until the next tick, as the shell asks for it.
    fn frame_interval(&self) -> Duration {
        self.shell
            .frame_interval(&self.timing())
            .clamp(Duration::from_millis(4), Duration::from_secs(1))
    }

    /// Whether this tick builds a frame (see [`Host::dirty`]), and if so
    /// whether it must draw the scene again whatever the frame holds: a
    /// client, the shell or the agent changed something egui's passes do
    /// not show.
    fn frame_wanted(&mut self) -> Option<bool> {
        // Asked every tick, not only when nothing else wants a frame, so
        // the shell can keep track of what it last showed.
        let shell = self.shell.needs_frame();
        // A request due before the tick after this one is drawn now; that
        // tick would come later than it asked for.
        let by = Instant::now() + self.frame_interval() / 2;
        let egui = self.repaint.take_due(by);
        // The agent's queue moves one command per frame, and its reads wait
        // for the frames that show what came before them.
        let agent = !self.agent_queue.is_empty() || self.settle > 0 || !self.captures.is_empty();
        let forced = self.dirty || shell || agent;
        (forced || egui || self.moved || self.requested).then_some(forced)
    }

    /// Draws one frame: background, then per window its decoration and
    /// content, then the chrome and the pointer, when something on screen
    /// may have changed.
    fn render(&mut self) -> Tick {
        // Nothing is drawn while output power has the display off.
        if !self.backend.can_draw() || !self.output_config.powered {
            self.run_commands();
            return Tick::Busy;
        }
        self.update_refresh();
        self.shell.tick();
        if self
            .scroll
            .is_some_and(|(_, at)| self.now_ms().wrapping_sub(at) > SCROLL_IDLE_MS)
        {
            self.end_scroll();
        }
        self.run_commands();
        self.agent_step();
        self.sync();
        let Some(forced) = self.frame_wanted() else {
            // A commit timer still falls due, and a fifo barrier still
            // clears, with nothing to draw.
            self.release_barriers();
            return Tick::Idle;
        };
        self.dirty = false;
        self.moved = false;
        self.requested = false;
        let size = self.backend.size();
        let screen =
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(size.w as f32, size.h as f32));
        let elapsed = self.now_ms();
        let time = self.start.elapsed().as_secs_f64();

        let input = egui::RawInput {
            screen_rect: Some(screen),
            events: std::mem::take(&mut self.chrome_events),
            time: Some(time),
            focused: true,
            ..Default::default()
        };
        let pointer = self.egui_pos();
        let accent = self.shell.theme().accent;
        // The texture is filled by draw() below, after the windows and
        // before the chrome, so the chrome built now shows this frame's.
        self.wants_pictures = self.shell.wants_window_pictures();
        let pictures = self
            .pictures
            .as_ref()
            .filter(|_| self.wants_pictures)
            .map(|p| WindowPictures {
                texture: p.id,
                size: screen.size(),
            });
        self.shell.window_pictures(pictures);
        self.chrome_clicks.frame(&self.chrome.ctx, time);
        let shell = &mut self.shell;
        let mut output = self.chrome.ctx.run_ui(input, |root| {
            shell.chrome(root, elapsed);
            mcsapi_ui::paint_focus_ring(root.ctx(), accent);
        });
        self.chrome_access = output.platform_output.accesskit_update.take();
        let chrome_cursor = output.platform_output.cursor_icon;
        let chrome = Pass {
            primitives: self
                .chrome
                .ctx
                .tessellate(output.shapes, output.pixels_per_point),
            textures: output.textures_delta,
        };

        // Its own pass, so it stays above overlays drawn after the chrome.
        let cursor = self.paint_only(screen, time, |_, painter| paint_cursor(painter, pointer));
        let blurs = self.shell.blur_regions();
        let placements = self.shell.placements();
        let background = self.paint_only(screen, time, |shell, painter| {
            shell.paint_background(painter, screen)
        });
        let mut decorations = Vec::with_capacity(placements.len());
        let mut contents = Vec::with_capacity(placements.len());
        let mut app_cursor = None;
        for p in &placements {
            decorations.push(self.paint_only(screen, time, |shell, painter| {
                shell.paint_decoration(painter, p)
            }));
            let (pass, cursor) = self.run_internal(p, time).unzip();
            if self.pointer_target == Some(p.window) {
                app_cursor = cursor;
            }
            contents.push(pass);
        }
        self.cursors.egui = cursor::from_egui(if self.chrome_wants_pointer() {
            chrome_cursor
        } else {
            app_cursor.unwrap_or_default()
        });
        let on_client = self
            .seat
            .get_pointer()
            .is_some_and(|p| p.current_focus().is_some());
        let cursor_drawn = self.cursors.drawn(
            self.backend.renderer(),
            on_client,
            self.pointer,
            self.start.elapsed(),
        );

        // The cursor goes into the scene where the scene is the screen (a
        // window drawn into directly), where egui paints it (the screen is
        // turned the other way up from egui's GL; see `kms.rs`), and where
        // a capture wants it; then the scene cannot be shown again with the
        // cursor elsewhere.
        let in_scene = !self.backend.offscreen()
            || matches!(cursor_drawn, cursor::Drawn::Painted)
            || !self.captures.is_empty()
            || self.screen_capture.wants(true);
        let key = (!in_scene && !self.wants_pictures && !self.screen_capture.wants(false))
            .then(|| {
                scene_key(
                    (size, self.lock.locked),
                    &placements,
                    &blurs,
                    [&background, &chrome]
                        .into_iter()
                        .chain(&decorations)
                        .chain(contents.iter().flatten()),
                    &cursor,
                )
            })
            .flatten();
        // Unless something else changed, the scene is drawn again only
        // where surfaces changed, and copied to the screen only there and
        // where the cursor moved: egui's passes and the placements came out
        // as for the scene already drawn.
        let partial = !forced && key.is_some() && key == self.scene_key;
        self.take_presentation_feedback();
        let drawn = self.draw(
            size,
            &placements,
            background,
            decorations,
            contents,
            &blurs,
            chrome,
            cursor,
            cursor_drawn,
            in_scene,
            partial,
        );
        self.scene_key = key.filter(|_| drawn.is_ok());
        let outcome = drawn.as_ref().map_or(Scene::Drawn, |(_, outcome)| *outcome);
        if outcome == Scene::Unchanged {
            // egui ran for a move or an animation, or a client committed
            // nothing new, and nothing on screen changed.
            self.release_barriers();
            self.send_frames();
            self.run_commands();
            return Tick::Idle;
        }
        self.frame_drawn(drawn.is_ok());
        // A frame that failed is tried again on the next tick.
        self.dirty |= drawn.is_err();
        let capture = match drawn {
            Ok((capture, _)) => capture.map(Ok),
            Err(e) => {
                error!(error = %e, "render failed");
                Some(Err(e.to_string()))
            }
        };
        if let Some(capture) = capture {
            for request in std::mem::take(&mut self.captures) {
                self.shell.captured(request, capture.clone());
            }
        }
        #[cfg(feature = "atspi")]
        if let Some(mut atspi) = self.atspi.take()
            && atspi.update(|| self.build_tree())
        {
            self.atspi = Some(atspi);
        }

        self.send_frames();
        self.run_commands();
        match outcome {
            Scene::Cursor => Tick::Cursor,
            _ => Tick::Drawn,
        }
    }

    /// Tells every surface on screen a frame was shown.
    fn send_frames(&mut self) {
        let now = self.start.elapsed();
        for window in self.space.elements() {
            window.send_frame(&self.output, now, Some(Duration::ZERO), |_, _| {
                Some(self.output.clone())
            });
        }
        self.wlr_send_frames(now);
        self.lock.send_frames(&self.output, now);
        self.input_method.send_frames(&self.output, now);
        if let Some(surface) = self.cursors.surface() {
            send_frames_surface_tree(surface, &self.output, now, Some(Duration::ZERO), |_, _| {
                Some(self.output.clone())
            });
        }
    }

    fn paint_only(
        &mut self,
        screen: egui::Rect,
        time: f64,
        mut paint: impl FnMut(&mut S, &egui::Painter),
    ) -> Pass {
        let input = egui::RawInput {
            screen_rect: Some(screen),
            time: Some(time),
            ..Default::default()
        };
        let shell = &mut self.shell;
        let output = self
            .decorations
            .ctx
            .run_ui(input, |root| paint(shell, root.painter()));
        Pass {
            primitives: self
                .decorations
                .ctx
                .tessellate(output.shapes, output.pixels_per_point),
            textures: output.textures_delta,
        }
    }

    /// Runs an in-process app's frame inside its content area.
    /// Also returns the cursor shape the app wants.
    fn run_internal(&mut self, p: &Placement, time: f64) -> Option<(Pass, egui::CursorIcon)> {
        let theme = self.shell.theme();
        let focused = p.focused;
        let Some(Content::Internal {
            instance,
            egui,
            events,
            clicks,
            title,
            access,
            ..
        }) = self.windows.get_mut(&p.window)
        else {
            return None;
        };
        let apps = self.apps.as_mut()?;
        apps.prepare(&egui.ctx, &theme);
        let app = apps.app_mut(*instance)?;
        let g = p.client;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(g.loc.x as f32, g.loc.y as f32),
                egui::vec2(g.size.w as f32, g.size.h as f32),
            )),
            events: std::mem::take(events),
            time: Some(time),
            focused,
            ..Default::default()
        };
        clicks.frame(&egui.ctx, time);
        let mut output = mcsapi_ui::run_frame(app, &egui.ctx, input, &theme);
        *access = output.platform_output.accesskit_update.take();
        for command in output.platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                set_data_device_selection(
                    &self.display,
                    &self.seat,
                    vec![
                        "text/plain;charset=utf-8".into(),
                        "text/plain".into(),
                        "UTF8_STRING".into(),
                    ],
                    text,
                );
            }
        }
        // Fill the whole content area, not only what the app laid out,
        // unless the app's own opaque panels already cover it: most do, and
        // filling under them filled every window twice each frame.
        let area = egui::Rect::from_min_size(
            egui::pos2(g.loc.x as f32, g.loc.y as f32),
            egui::vec2(g.size.w as f32, g.size.h as f32),
        );
        if !covers(&output.shapes, g) {
            output.shapes.insert(
                0,
                egui::epaint::ClippedShape {
                    clip_rect: area,
                    shape: egui::Shape::rect_filled(area, 0, theme.background),
                },
            );
        }
        let new_title = (app.title() != title.as_str()).then(|| app.title().to_owned());
        let pass = Pass {
            primitives: egui.ctx.tessellate(output.shapes, output.pixels_per_point),
            textures: output.textures_delta,
        };
        if let Some(new_title) = new_title {
            title.clone_from(&new_title);
            self.shell.set_title(p.window, &new_title);
        }
        Some((pass, output.platform_output.cursor_icon))
    }

    #[allow(clippy::too_many_arguments)] // one per layer of the frame
    fn draw(
        &mut self,
        size: Size<i32, Physical>,
        placements: &[Placement],
        mut background: Pass,
        decorations: Vec<Pass>,
        contents: Vec<Option<Pass>>,
        blurs: &[Blur],
        mut chrome: Pass,
        mut cursor: Pass,
        cursor_drawn: cursor::Drawn,
        in_scene: bool,
        partial: bool,
    ) -> std::result::Result<(Option<Capture>, Scene), Box<dyn std::error::Error>> {
        let scale = Scale::from(1.0);
        let screen_px = [size.w as u32, size.h as u32];
        let full = Rectangle::from_size(size);

        let offscreen = self.backend.offscreen();
        let buffer_size = Size::<i32, BufferCoord>::from((size.w, size.h));
        if offscreen && self.scene.as_ref().is_none_or(|t| t.size() != buffer_size) {
            self.scene = Some(Offscreen::<GlesTexture>::create_buffer(
                self.backend.renderer(),
                Fourcc::Abgr8888,
                buffer_size,
            )?);
        }
        // The scene is bound only once this frame is known to draw: on
        // llvmpipe binding it cost a cursor-only frame some 7 ms, five
        // times what the rest of it took.
        let renderer = self.backend.renderer();
        let gl = match &self.gl {
            Some(gl) => gl.clone(),
            None => {
                // SAFETY: the renderer's EGL context is current inside
                // with_context; symbols are resolved through EGL.
                let gl = renderer.with_context(|_| unsafe {
                    glow::Context::from_loader_function(|s| egl::get_proc_address(s))
                })?;
                let gl = Arc::new(gl);
                self.gl = Some(gl.clone());
                gl
            }
        };
        renderer.with_context(|_| {
            for mut painter in self.retired.drain(..) {
                painter.destroy();
            }
        })?;
        let new_painter =
            || egui_glow::Painter::new(gl.clone(), "", None, false).map_err(|e| e.to_string());
        if self.chrome.painter.is_none() {
            self.chrome.painter = Some(new_painter()?);
        }
        if self.decorations.painter.is_none() {
            self.decorations.painter = Some(new_painter()?);
        }
        for content in self.windows.values_mut() {
            if let Content::Internal { egui, .. } = content
                && egui.painter.is_none()
            {
                egui.painter = Some(new_painter()?);
            }
        }

        // Import client buffers before starting the frame.
        // With each window, what its client asked to blur behind it.
        let mut surfaces: Vec<(Vec<Blur>, Vec<WaylandSurfaceRenderElement<GlesRenderer>>)> =
            Vec::new();
        for p in placements {
            let elements = match self.windows.get(&p.window) {
                Some(Content::Wayland(w)) => self
                    .space
                    .element_location(w)
                    .map(|loc| {
                        let blurs = w
                            .toplevel()
                            .map(|t| effects::surface_blurs(t.wl_surface(), loc, p.client))
                            .unwrap_or_default();
                        let elements = w.render_elements(
                            renderer,
                            loc.to_physical_precise_round(scale),
                            scale,
                            p.motion.map_or(1.0, |m| f32::from(m.opacity) / 255.0),
                        );
                        (blurs, elements)
                    })
                    .unwrap_or_default(),
                _ => Default::default(),
            };
            surfaces.push(elements);
        }
        // Panels go under the chrome, so its popups can overlap them;
        // overlays go above it.
        let mut panels = Vec::new();
        let mut overlays = Vec::new();
        for layer in &self.layers {
            let Some(loc) = self.space.element_location(&layer.window) else {
                continue;
            };
            let elements = layer
                .window
                .render_elements::<WaylandSurfaceRenderElement<_>>(
                    renderer,
                    loc.to_physical_precise_round(scale),
                    scale,
                    1.0,
                );
            match layer.role {
                Role::Overlay => overlays.push(elements),
                _ => panels.push(elements),
            }
        }

        let wlr_below: Vec<_> = [WlrLayer::Background, WlrLayer::Bottom]
            .into_iter()
            .flat_map(|layer| layer_shell::wlr_elements(&self.output, renderer, layer))
            .collect();
        let wlr_top = layer_shell::wlr_elements(&self.output, renderer, WlrLayer::Top);
        let wlr_overlay = layer_shell::wlr_elements(&self.output, renderer, WlrLayer::Overlay);
        let locked = self.lock.locked;
        let lock_elements = self.lock.elements(renderer);
        let input_popups = self.input_method.elements(renderer, scale);

        // What the surfaces changed since the scene was last drawn. Kept up
        // on every frame, and used on one that may redraw only part of it.
        let changed = surface_damage(
            &mut self.seen,
            surfaces
                .iter()
                .chain(&wlr_below)
                .chain(&wlr_top)
                .chain(&wlr_overlay)
                .map(|(_, elements)| elements)
                .chain(&panels)
                .chain(&overlays)
                .chain([&lock_elements, &input_popups])
                .flatten(),
            scale,
        );
        let area = if partial {
            match changed
                .into_iter()
                .reduce(|a, b| a.merge(b))
                .and_then(|a| a.intersection(full))
            {
                None => {
                    // Nothing in the scene changed: at most the cursor moved.
                    let moved = cursor_drawn.area().map(|a| (a, cursor_drawn.id()));
                    if moved == self.shown_cursor {
                        return Ok((None, Scene::Unchanged));
                    }
                    self.present(size, Some(cursor_drawn), Changed::Cursor)?;
                    return Ok((None, Scene::Cursor));
                }
                Some(area) => Some(area),
            }
        } else {
            None
        };
        // A blur near the change blurs some of it, and a blur redrawn in
        // part would blur its own output: the area grows to take in each
        // blur it reaches, whole, which then reaches others. A terminal's
        // first line, under the top bar's blur, redraws its line and the
        // bar rather than the screen.
        let reachable: Vec<&Blur> = blurs
            .iter()
            .chain(
                surfaces
                    .iter()
                    .chain(&wlr_below)
                    .chain(&wlr_top)
                    .chain(&wlr_overlay)
                    .flat_map(|(behind, _)| behind),
            )
            .collect();
        let area = area.map(|mut area| {
            while let Some(b) = reachable.iter().find(|b| {
                blur_reaches(b, area)
                    && !area.contains_rect(blur_rect(b).intersection(full).unwrap_or(area))
            }) {
                area = area.merge(blur_rect(b)).intersection(full).unwrap_or(full);
            }
            area
        });
        // Where this frame draws, and where the screen changes: all of it,
        // or the area the surfaces and the blurs they reach changed. Only
        // the blurs inside it are drawn again; the rest are already right.
        let clip = area.unwrap_or(full);
        let redrawn = |blurs: &[Blur]| -> Vec<Blur> {
            blurs
                .iter()
                .filter(|b| area.is_none_or(|a| a.overlaps(blur_rect(b))))
                .copied()
                .collect()
        };
        let (renderer, mut framebuffer) = match (&mut self.backend, &mut self.scene) {
            (backend, Some(scene)) if offscreen => {
                let renderer = backend.renderer();
                let framebuffer = renderer.bind(scene)?;
                (renderer, framebuffer)
            }
            (Backend::Winit(backend), _) => backend.bind()?,
            #[cfg(feature = "kms")]
            (Backend::Kms(_), _) => unreachable!("the bare seat always draws offscreen"),
        };

        // The background is drawn only where no opaque window covers it.
        // Under two tiled windows the wallpaper was filled in full every
        // frame and almost none of it seen; on a GPU at its lowest clock it
        // was the costliest layer of the frame. In-process apps are opaque
        // over their content area, which is filled with the theme's
        // background, and clients say where they are opaque. A moving
        // window is drawn elsewhere, so it covers nothing.
        let opaque_apps = self.shell.theme().background.is_opaque();
        let mut covered = Vec::new();
        for (i, p) in placements.iter().enumerate() {
            if p.motion.is_some() {
                continue;
            }
            match self.windows.get(&p.window) {
                Some(Content::Internal { .. }) if opaque_apps && contents[i].is_some() => {
                    covered.push(p.client.to_physical(1));
                }
                Some(Content::Wayland(_)) => {
                    for element in &surfaces[i].1 {
                        let at = element.geometry(scale).loc;
                        covered.extend(
                            element
                                .opaque_regions(scale)
                                .iter()
                                .map(|r| Rectangle::new(r.loc + at, r.size)),
                        );
                    }
                }
                _ => {}
            }
        }
        if !covered.is_empty() {
            clip_pass(&mut background, &full.subtract_rects(covered));
        }

        let mut frame = renderer.render(&mut framebuffer, size, Transform::Flipped180)?;
        frame.clear(Color32F::new(0.06, 0.09, 0.16, 1.0), &[clip])?;
        let deco = self.decorations.painter.as_mut().expect("created above");
        paint_in(&gl, deco, screen_px, &mut background, area);
        for (behind, elements) in &wlr_below {
            blur_behind(
                &mut self.blurrer,
                &mut self.blur_unavailable,
                &gl,
                screen_px,
                &redrawn(behind),
            );
            draw_surfaces(&mut frame, elements, scale, area)?;
        }
        for (((p, mut decoration), content), (behind, elements)) in placements
            .iter()
            .zip(decorations)
            .zip(contents)
            .zip(&surfaces)
        {
            let motion = p.motion.map(|m| (p.frame, m));
            let deco = self.decorations.painter.as_mut().expect("created above");
            if let Some((frame, m)) = motion {
                move_pass(&mut decoration, frame, m);
            }
            paint_in(&gl, deco, screen_px, &mut decoration, area);
            if let (Some(mut pass), Some(Content::Internal { egui, .. })) =
                (content, self.windows.get_mut(&p.window))
            {
                if let Some((frame, m)) = motion {
                    move_pass(&mut pass, frame, m);
                }
                let painter = egui.painter.as_mut().expect("created above");
                paint_in(&gl, painter, screen_px, &mut pass, area);
            }
            // What a client blurs behind it is where it will be; while it
            // moves there is nothing steady to blur.
            if motion.is_none() {
                blur_behind(
                    &mut self.blurrer,
                    &mut self.blur_unavailable,
                    &gl,
                    screen_px,
                    &redrawn(behind),
                );
            }
            draw_moved_surfaces(&mut frame, elements, scale, motion, area)?;
        }
        // The windows as they are now, before the chrome's blur and panels
        // cover them.
        let chrome_painter = self.chrome.painter.as_mut().expect("created above");
        if self.wants_pictures {
            copy_windows(&gl, chrome_painter, &mut self.pictures, screen_px)?;
        } else if let Some(pictures) = self.pictures.take() {
            chrome_painter.free_texture(pictures.id);
        }
        blur_behind(
            &mut self.blurrer,
            &mut self.blur_unavailable,
            &gl,
            screen_px,
            &redrawn(blurs),
        );
        for elements in &panels {
            draw_surfaces(&mut frame, elements, scale, area)?;
        }
        for (behind, elements) in &wlr_top {
            blur_behind(
                &mut self.blurrer,
                &mut self.blur_unavailable,
                &gl,
                screen_px,
                &redrawn(behind),
            );
            draw_surfaces(&mut frame, elements, scale, area)?;
        }
        let chrome_painter = self.chrome.painter.as_mut().expect("created above");
        paint_in(&gl, chrome_painter, screen_px, &mut chrome, area);
        for elements in &overlays {
            draw_surfaces(&mut frame, elements, scale, area)?;
        }
        for (behind, elements) in &wlr_overlay {
            blur_behind(
                &mut self.blurrer,
                &mut self.blur_unavailable,
                &gl,
                screen_px,
                &redrawn(behind),
            );
            draw_surfaces(&mut frame, elements, scale, area)?;
        }
        // An input method's candidates go over everything but the lock.
        draw_surfaces(&mut frame, &input_popups, scale, area)?;
        // Locked: the session is drawn as usual, so egui's passes keep
        // their textures in step, then covered.
        if locked {
            frame.clear(Color32F::new(0.06, 0.09, 0.16, 1.0), &[clip])?;
            draw_surfaces(&mut frame, &lock_elements, scale, area)?;
        }
        let bare = self
            .screen_capture
            .wants(false)
            .then(|| read_frame(&gl, screen_px));
        // The arrow's pass also carries texture updates for the shared
        // decorations context, so it is painted, empty, when unused.
        if !matches!(cursor_drawn, cursor::Drawn::Painted) {
            cursor.primitives.clear();
        }
        let deco = self.decorations.painter.as_mut().expect("created above");
        paint_in(&gl, deco, screen_px, &mut cursor, area);
        // Held with the frame's other client buffers below.
        let mut cursor_elements = Vec::new();
        let mut over = None;
        if in_scene {
            cursor_elements = draw_cursor(&mut frame, cursor_drawn)?;
        } else {
            over = Some(cursor_drawn);
        }
        let with_cursor = (!self.captures.is_empty() || self.screen_capture.wants(true))
            .then(|| read_frame(&gl, screen_px));
        // Not waited on: offscreen, the copy to the screen reads this frame
        // on the same context after it, and the fence that copy ends with
        // covers both.
        let _sync = frame.finish()?;
        if locked {
            self.lock.drawn();
        }
        drop(framebuffer);
        // The GPU may still be reading the client buffers; they stay held
        // until the frame is on screen (see `Kms::in_flight`). Before the
        // copy to the screen, so a frame that then fails to reach it does
        // not release them early either.
        #[cfg(feature = "kms")]
        if let Backend::Kms(k) = &mut self.backend {
            let layers = wlr_below.into_iter().chain(wlr_top).chain(wlr_overlay);
            k.in_flight.extend(
                surfaces
                    .into_iter()
                    .chain(layers)
                    .map(|(_, elements)| elements)
                    .chain(panels)
                    .chain(overlays)
                    .chain([lock_elements, input_popups, cursor_elements])
                    .flatten(),
            );
        }
        #[cfg(not(feature = "kms"))]
        drop(cursor_elements);
        if bare.is_some() || with_cursor.is_some() {
            self.deliver_captures(bare.as_ref(), with_cursor.as_ref());
        }
        let capture = with_cursor.filter(|_| !self.captures.is_empty());
        if offscreen {
            self.present(size, over, area.map_or(Changed::All, Changed::Area))?;
            return Ok((capture, Scene::Drawn));
        }
        match &mut self.backend {
            Backend::Winit(backend) => backend.submit(Some(&[full]))?,
            #[cfg(feature = "kms")]
            Backend::Kms(_) => {}
        }
        Ok((capture, Scene::Drawn))
    }

    /// Copies the offscreen frame to the screen, and `cursor` over it: what
    /// `changed` says changed in it, where the cursor was and is, and what the
    /// screen buffer missed since it was last shown (its age).
    fn present(
        &mut self,
        size: Size<i32, Physical>,
        cursor: Option<cursor::Drawn>,
        changed: Changed,
    ) -> Result {
        let screen = Rectangle::from_size(size);
        let Some(scene) = &self.scene else {
            return Ok(());
        };
        let shown = cursor.as_ref().and_then(|c| c.area().map(|a| (a, c.id())));
        let full = changed == Changed::All;
        let changed: Vec<_> = match changed {
            Changed::All => vec![screen],
            Changed::Area(area) => std::iter::once(area)
                .chain(self.shown_cursor.iter().chain(&shown).map(|(a, _)| *a))
                .filter_map(|a| a.intersection(screen))
                .collect(),
            Changed::Cursor => self
                .shown_cursor
                .iter()
                .chain(&shown)
                .filter_map(|(a, _)| a.intersection(screen))
                .collect(),
        };
        // The areas to copy into a buffer the screen showed `age` frames
        // ago; all of it for a new buffer, or one older than the history.
        let presented = &self.presented;
        let damage = |age: usize| -> Vec<Rectangle<i32, Physical>> {
            if full || age == 0 || age > presented.len() + 1 {
                return vec![screen];
            }
            presented
                .iter()
                .take(age - 1)
                .flatten()
                .chain(&changed)
                .copied()
                .collect()
        };
        let src = Rectangle::from_size(scene.size()).to_f64();
        match &mut self.backend {
            Backend::Winit(backend) => {
                let damage = damage(backend.buffer_age().unwrap_or(0));
                let (renderer, mut framebuffer) = backend.bind()?;
                let mut frame = renderer.render(&mut framebuffer, size, Transform::Flipped180)?;
                Frame::render_texture_from_to(
                    &mut frame,
                    scene,
                    src,
                    screen,
                    &damage,
                    &[],
                    OFFSCREEN_COPY,
                    1.0,
                )?;
                if let Some(cursor) = cursor {
                    draw_cursor(&mut frame, cursor)?;
                }
                let _sync = frame.finish()?;
                drop(framebuffer);
                backend.submit(Some(&damage))?;
            }
            #[cfg(feature = "kms")]
            Backend::Kms(k) => {
                let (mut dmabuf, age) = k.surface.next_buffer()?;
                let damage = damage(usize::from(age));
                let mut framebuffer = k.renderer.bind(&mut dmabuf)?;
                let mut frame = k
                    .renderer
                    .render(&mut framebuffer, size, Transform::Normal)?;
                Frame::render_texture_from_to(
                    &mut frame,
                    scene,
                    src,
                    screen,
                    &damage,
                    &[],
                    OFFSCREEN_COPY,
                    1.0,
                )?;
                // Held until the vblank, as the scene's client buffers are.
                if let Some(cursor) = cursor {
                    let elements = draw_cursor(&mut frame, cursor)?;
                    k.in_flight.extend(elements);
                }
                let sync = frame.finish()?;
                drop(framebuffer);
                // The fence goes with the buffer: the flip waits on it in
                // the kernel (`IN_FENCE_FD`), or Smithay waits on it here
                // where the plane takes no fence. Nothing relies on the
                // driver ordering the flip after the drawing by itself.
                k.surface.queue_buffer(Some(sync), Some(damage), ())?;
                k.queued(self.timing.pending.take());
            }
        }
        self.presented.push_front(changed);
        self.presented.truncate(PRESENTED_FRAMES);
        self.shown_cursor = shown;
        Ok(())
    }

    /// Starts managing a toplevel on its initial commit, when its app ID and
    /// title are known, so the first configure already has its final size.
    fn manage_on_first_commit(&mut self, surface: &WlSurface) {
        let Some(i) = self
            .unmanaged
            .iter()
            .position(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        else {
            return;
        };
        let window = self.unmanaged.remove(i);
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        if let Some(role) = self.role_of(surface).filter(|r| *r != Role::App) {
            self.layers.push(Layer { window, role });
            self.sync();
            if !toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
            return;
        }
        let (app_id, title) = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok())
                .map(|d| (d.app_id.clone(), d.title.clone()))
                .unwrap_or_default()
        });
        let id = self.shell.map_window(
            app_id.as_deref().unwrap_or("app"),
            title.as_deref().unwrap_or_default(),
        );
        if let Some(sandbox) = sandbox_of(surface) {
            self.shell.window_sandbox(id, &sandbox);
        }
        // From the socket's peer credentials, which the client can't make
        // up: what a report on a hung window reads, and what force quitting
        // it signals.
        if let Some(pid) = surface
            .client()
            .and_then(|c| c.get_credentials(&self.display).ok())
            .and_then(|c| u32::try_from(c.pid).ok())
        {
            self.shell.window_process(id, pid);
        }
        self.windows.insert(id, Content::Wayland(window));
        self.window_mapped(id, &toplevel);
        self.sync();
        if !toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
        self.run_commands();
    }
}

/// The sandbox `surface`'s client connected through, if it did.
fn sandbox_of(surface: &WlSurface) -> Option<crate::Sandbox> {
    surface.client()?.get_data::<ClientState>()?.sandbox.clone()
}

fn shell_key_route(shell: &mut impl Shell, key: &KeyInput, consumed_release: bool) -> KeyRoute {
    let route = shell.key(key);
    if consumed_release {
        KeyRoute::Consume
    } else {
        route
    }
}

/// Draws one window's surfaces; Smithay lists them topmost first.
/// Draws the cursor into `frame`, and returns the client buffers it drew,
/// to be held until the frame is on screen.
fn draw_cursor(
    frame: &mut GlesFrame<'_, '_>,
    cursor: cursor::Drawn,
) -> Result<Vec<WaylandSurfaceRenderElement<GlesRenderer>>> {
    match cursor {
        cursor::Drawn::Hidden | cursor::Drawn::Painted => Ok(Vec::new()),
        cursor::Drawn::Surface(elements) => {
            draw_surfaces(frame, &elements, Scale::from(1.0), None)?;
            Ok(elements)
        }
        cursor::Drawn::Image {
            texture, at, size, ..
        } => {
            Frame::render_texture_from_to(
                frame,
                &texture,
                Rectangle::from_size(texture.size()).to_f64(),
                Rectangle::new(at, size),
                &[Rectangle::from_size(size)],
                &[],
                Transform::Normal,
                1.0,
            )?;
            Ok(Vec::new())
        }
    }
}

/// How many frames' areas [`Host::presented`] keeps: past triple
/// buffering's three, a buffer that old is copied in full anyway.
const PRESENTED_FRAMES: usize = 4;

/// A fingerprint of everything a scene is drawn from that egui builds each
/// frame: the screen's size, the windows' placements, the blurs, and the
/// passes' meshes. `None` when it cannot stand for the scene: a pass
/// carries a texture update, which must be painted, or a paint callback,
/// whose drawing no mesh shows. The cursor's own pass counts only for its
/// texture updates; its mesh follows the pointer and is not in the scene.
fn scene_key<'a>(
    (size, locked): (Size<i32, Physical>, bool),
    placements: &[Placement],
    blurs: &[Blur],
    passes: impl IntoIterator<Item = &'a Pass>,
    cursor: &Pass,
) -> Option<u64> {
    let mut hasher = FrameHasher::default();
    (size.w, size.h, locked).hash(&mut hasher);
    for p in placements {
        format!("{p:?}").hash(&mut hasher);
    }
    for b in blurs {
        format!("{b:?}").hash(&mut hasher);
    }
    let unchanged = |pass: &Pass| pass.textures.set.is_empty() && pass.textures.free.is_empty();
    if !unchanged(cursor) {
        return None;
    }
    for pass in passes {
        if !unchanged(pass) {
            return None;
        }
        // Where one pass ends, so meshes cannot slide between passes.
        hasher.write_u64(u64::MAX);
        for clipped in &pass.primitives {
            let egui::epaint::Primitive::Mesh(mesh) = &clipped.primitive else {
                return None;
            };
            let r = clipped.clip_rect;
            for v in [r.min.x, r.min.y, r.max.x, r.max.y] {
                hasher.write_u32(v.to_bits());
            }
            mesh.texture_id.hash(&mut hasher);
            hasher.write_u64(mesh.indices.len() as u64);
            for &i in &mesh.indices {
                hasher.write_u32(i);
            }
            for v in &mesh.vertices {
                hasher.write_u32(v.pos.x.to_bits());
                hasher.write_u32(v.pos.y.to_bits());
                hasher.write_u32(v.uv.x.to_bits());
                hasher.write_u32(v.uv.y.to_bits());
                hasher.write_u32(u32::from_le_bytes(v.color.to_array()));
            }
        }
    }
    Some(hasher.finish())
}

/// A fast hash for [`scene_key`], which reads every vertex of every frame:
/// FxHash's multiply and rotate, one word at a time. Only compared with the
/// key of the frame before, so collisions are as rare as they need to be.
#[derive(Default)]
struct FrameHasher(u64);

impl Hasher for FrameHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn write_u32(&mut self, i: u32) {
        self.write_u64(u64::from(i));
    }

    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

fn draw_surfaces(
    frame: &mut GlesFrame<'_, '_>,
    elements: &[WaylandSurfaceRenderElement<GlesRenderer>],
    scale: Scale<f64>,
    clip: Option<Rectangle<i32, Physical>>,
) -> Result {
    draw_moved_surfaces(frame, elements, scale, None, clip)
}

/// [`draw_surfaces`], stretched from the window's frame to where its
/// [`Motion`] draws it. Their alpha was given when the elements were made.
fn draw_moved_surfaces(
    frame: &mut GlesFrame<'_, '_>,
    elements: &[WaylandSurfaceRenderElement<GlesRenderer>],
    scale: Scale<f64>,
    motion: Option<(Geometry, Motion)>,
    clip: Option<Rectangle<i32, Physical>>,
) -> Result {
    for element in elements.iter().rev() {
        let mut dst = element.geometry(scale);
        if let Some((window, m)) = motion {
            dst = moved_rect(window, m, dst);
            if dst.is_empty() {
                continue;
            }
        }
        // Only within `clip`, relative to the surface as Smithay takes it.
        let damage = match clip {
            None => Rectangle::from_size(dst.size),
            Some(clip) => match clip.intersection(dst) {
                Some(part) => Rectangle::new(part.loc - dst.loc, part.size),
                None => continue,
            },
        };
        element.draw(frame, element.src(), dst, &[damage], &[])?;
    }
    Ok(())
}

/// Where `m` draws `r`, a rectangle within `window`'s frame, rounded
/// outward to whole pixels so neighbouring surfaces leave no seam.
fn moved_rect(
    window: Geometry,
    m: Motion,
    r: Rectangle<i32, Physical>,
) -> Rectangle<i32, Physical> {
    let (x0, y0) = m.map(window, (r.loc.x as f32, r.loc.y as f32));
    let (x1, y1) = m.map(
        window,
        ((r.loc.x + r.size.w) as f32, (r.loc.y + r.size.h) as f32),
    );
    let (x0, y0) = (x0.floor() as i32, y0.floor() as i32);
    let (x1, y1) = (x1.ceil() as i32, y1.ceil() as i32);
    Rectangle::new((x0, y0).into(), ((x1 - x0).max(0), (y1 - y0).max(0)).into())
}

/// Moves an egui pass drawn at `window`'s frame to where `m` draws it, and
/// fades it by `m`'s opacity: every vertex and clip is mapped, so the
/// shapes are stretched rather than laid out again.
fn move_pass(pass: &mut Pass, window: Geometry, m: Motion) {
    let map = |p: egui::Pos2| {
        let (x, y) = m.map(window, (p.x, p.y));
        egui::pos2(x, y)
    };
    let fade = f32::from(m.opacity) / 255.0;
    for clipped in &mut pass.primitives {
        clipped.clip_rect =
            egui::Rect::from_min_max(map(clipped.clip_rect.min), map(clipped.clip_rect.max));
        if let egui::epaint::Primitive::Mesh(mesh) = &mut clipped.primitive {
            for v in &mut mesh.vertices {
                v.pos = map(v.pos);
                // Colours are premultiplied, so every channel fades.
                v.color = v.color.gamma_multiply(fade);
            }
        }
    }
}

/// Whether `shapes` paint all of `area` opaquely with plain square
/// rectangles, such as an app's panels, so nothing under them shows.
fn covers(shapes: &[egui::epaint::ClippedShape], area: Geometry) -> bool {
    let opaque = shapes.iter().filter_map(|clipped| match &clipped.shape {
        egui::Shape::Rect(r)
            if r.fill.is_opaque()
                && r.corner_radius == egui::CornerRadius::ZERO
                && r.brush.is_none()
                && r.blur_width == 0.0 =>
        {
            // Only whole pixels the rectangle fills count.
            let rect = r.rect.intersect(clipped.clip_rect);
            let (min, max) = (rect.min.ceil(), rect.max.floor());
            (max.x > min.x && max.y > min.y).then(|| {
                Geometry::new(
                    (min.x as i32, min.y as i32).into(),
                    ((max.x - min.x) as i32, (max.y - min.y) as i32).into(),
                )
            })
        }
        _ => None,
    });
    area.subtract_rects(opaque).is_empty()
}

/// Keeps `pass` to `areas`: each primitive is drawn once for every area it
/// overlaps, scissored to it, and not at all outside them. Its texture
/// updates stay, so the textures are kept up to date either way.
fn clip_pass(pass: &mut Pass, areas: &[Rectangle<i32, Physical>]) {
    let areas: Vec<egui::Rect> = areas
        .iter()
        .map(|a| {
            egui::Rect::from_min_size(
                egui::pos2(a.loc.x as f32, a.loc.y as f32),
                egui::vec2(a.size.w as f32, a.size.h as f32),
            )
        })
        .collect();
    pass.primitives = std::mem::take(&mut pass.primitives)
        .into_iter()
        .flat_map(|p| {
            areas
                .iter()
                .map(|area| p.clip_rect.intersect(*area))
                .filter(|clip| clip.is_positive())
                .map(|clip_rect| egui::ClippedPrimitive {
                    clip_rect,
                    primitive: p.primitive.clone(),
                })
                .collect::<Vec<_>>()
        })
        .collect();
}

/// Draws the pointer, which the frame paints last (the nested window hides
/// the host's).
fn paint_cursor(painter: &egui::Painter, at: egui::Pos2) {
    let points = [
        (0.0, 0.0),
        (0.0, 17.0),
        (4.5, 13.0),
        (7.5, 19.5),
        (10.0, 18.5),
        (7.0, 12.0),
        (12.5, 12.0),
    ]
    .map(|(x, y)| at + egui::vec2(x, y))
    .to_vec();
    painter.add(egui::Shape::convex_polygon(
        points.clone(),
        egui::Color32::BLACK,
        egui::Stroke::NONE,
    ));
    painter.add(egui::Shape::closed_line(
        points,
        egui::Stroke::new(1.5, egui::Color32::WHITE),
    ));
}

/// Paints an egui pass as [`paint`] does, only within `area` when given.
fn paint_in(
    gl: &glow::Context,
    painter: &mut egui_glow::Painter,
    screen: [u32; 2],
    pass: &mut Pass,
    area: Option<Rectangle<i32, Physical>>,
) {
    if let Some(area) = area {
        clip_pass(pass, &[area]);
    }
    paint(gl, painter, screen, pass);
}

/// What [`Host::draw`] did with the scene.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scene {
    /// Drew it, all of it or where it changed, and copied it to the screen.
    Drawn,
    /// Left it as it was and copied it with the cursor somewhere else.
    Cursor,
    /// Left it, and the screen, as they were.
    Unchanged,
}

/// What changed in the scene since it was last copied to the screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Changed {
    /// Any of it.
    All,
    /// Only this area.
    Area(Rectangle<i32, Physical>),
    /// Nothing; only the cursor moved or changed.
    Cursor,
}

/// A surface as the scene last drew it: the commit it showed, where, and
/// where it said it was opaque, on which the wallpaper's clip depends.
struct Seen {
    commit: CommitCounter,
    geometry: Rectangle<i32, Physical>,
    opaque: u64,
}

/// Where `elements` changed on screen since `seen` was taken, and `seen`
/// brought up to date: the damage each surface reported since the commit
/// last drawn, all of a surface that is new, moved, resized or changed where
/// it is opaque, and where a surface no longer drawn was.
fn surface_damage<'a>(
    seen: &mut HashMap<Id, Seen>,
    elements: impl IntoIterator<Item = &'a WaylandSurfaceRenderElement<GlesRenderer>>,
    scale: Scale<f64>,
) -> Vec<Rectangle<i32, Physical>> {
    let mut damage = Vec::new();
    let mut now = HashMap::with_capacity(seen.len());
    for element in elements {
        let geometry = element.geometry(scale);
        let opaque = {
            let mut hasher = FrameHasher::default();
            format!("{:?}", element.opaque_regions(scale)).hash(&mut hasher);
            hasher.finish()
        };
        match seen.remove(element.id()) {
            Some(before) if before.geometry == geometry && before.opaque == opaque => {
                damage.extend(
                    element
                        .damage_since(scale, Some(before.commit))
                        .iter()
                        .map(|d| Rectangle::new(d.loc + geometry.loc, d.size)),
                );
            }
            Some(before) => damage.extend([before.geometry, geometry]),
            None => damage.push(geometry),
        }
        now.insert(
            element.id().clone(),
            Seen {
                commit: element.current_commit(),
                geometry,
                opaque,
            },
        );
    }
    damage.extend(seen.drain().map(|(_, gone)| gone.geometry));
    *seen = now;
    damage
}

/// How far around its area a blur reads what it blurs, generously: the
/// strongest dual Kawase pass samples a few dozen pixels out.
const BLUR_REACH: i32 = 96;

/// Whether `blur` reads or covers anything in `area`.
fn blur_reaches(blur: &Blur, area: Rectangle<i32, Physical>) -> bool {
    let g = blur.area;
    Rectangle::<i32, Physical>::new(
        (g.loc.x - BLUR_REACH, g.loc.y - BLUR_REACH).into(),
        (g.size.w + 2 * BLUR_REACH, g.size.h + 2 * BLUR_REACH).into(),
    )
    .overlaps(area)
}

/// Where a blur draws, on the screen.
fn blur_rect(blur: &Blur) -> Rectangle<i32, Physical> {
    let g = blur.area;
    Rectangle::new((g.loc.x, g.loc.y).into(), (g.size.w, g.size.h).into())
}

/// Paints an egui pass into the bound framebuffer, then restores the GL
/// state Smithay's renderer relies on.
fn paint(gl: &glow::Context, painter: &mut egui_glow::Painter, screen: [u32; 2], pass: &mut Pass) {
    painter.paint_and_update_textures(screen, 1.0, &pass.primitives, &mut pass.textures);
    reset_gl(gl, screen);
}

/// Restores the GL state Smithay's renderer relies on.
/// Reads the bound framebuffer back as top-to-bottom RGBA rows.
fn read_frame(gl: &glow::Context, [w, h]: [u32; 2]) -> Capture {
    use glow::HasContext as _;
    let row = w as usize * 4;
    let mut rgba = vec![0; row * h as usize];
    // SAFETY: the buffer holds exactly w×h RGBA pixels and the frame's
    // context is current.
    unsafe {
        gl.read_pixels(
            0,
            0,
            w as i32,
            h as i32,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut rgba)),
        );
    }
    // GL rows run bottom to top; the framebuffer's alpha is meaningless.
    let mut flipped = Vec::with_capacity(rgba.len());
    for line in rgba.chunks_exact(row).rev() {
        let (pixels, _) = line.as_chunks::<4>();
        flipped.extend(pixels.iter().flat_map(|p| [p[0], p[1], p[2], 255]));
    }
    Capture {
        width: w,
        height: h,
        rgba: flipped,
    }
}

/// Blurs `blurs` in what the frame being drawn holds so far, creating the
/// blurrer the first time.
fn blur_behind(
    blurrer: &mut Option<blur::Blurrer>,
    unavailable: &mut bool,
    gl: &glow::Context,
    screen_px: [u32; 2],
    blurs: &[Blur],
) {
    if blurs.is_empty() {
        return;
    }
    if blurrer.is_none() && !*unavailable {
        // SAFETY: the frame's context is current while it is open.
        match unsafe { blur::Blurrer::new(gl) } {
            Ok(b) => *blurrer = Some(b),
            Err(e) => {
                *unavailable = true;
                warn!(error = %e, "blur unavailable");
            }
        }
    }
    if let Some(blurrer) = blurrer {
        // SAFETY: as above, with the frame's framebuffer bound.
        if let Err(e) = unsafe { blurrer.apply(gl, screen_px, blurs) } {
            warn!(error = %e, "blur failed");
        }
        reset_gl(gl, screen_px);
    }
}

/// The texture behind [`WindowPictures`], registered with the chrome's
/// painter (which deletes it with [`egui_glow::Painter::free_texture`]).
struct Pictures {
    texture: glow::Texture,
    id: egui::TextureId,
    /// In pixels; a resize makes a new texture under the same ID.
    size: [u32; 2],
}

/// Copies the bound framebuffer into `pictures`, making the texture first
/// when there is none of the screen's size.
fn copy_windows(
    gl: &glow::Context,
    painter: &mut egui_glow::Painter,
    pictures: &mut Option<Pictures>,
    screen: [u32; 2],
) -> std::result::Result<(), String> {
    use glow::HasContext as _;
    let [w, h] = [screen[0] as i32, screen[1] as i32];
    // SAFETY: the frame's context is current with its framebuffer bound;
    // the texture is made on that context.
    unsafe {
        if pictures.as_ref().is_none_or(|p| p.size != screen) {
            let texture = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            for (param, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, param, value as i32);
            }
            // RGB, as the blur's: GLES can't copy a framebuffer without
            // alpha into an RGBA texture, and windows are opaque anyway.
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGB as i32,
                w,
                h,
                0,
                glow::RGB,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            let id = match pictures.take() {
                Some(old) => {
                    painter.replace_native_texture(old.id, texture);
                    old.id
                }
                None => painter.register_native_texture(texture),
            };
            *pictures = Some(Pictures {
                texture,
                id,
                size: screen,
            });
        }
        let texture = pictures.as_ref().map(|p| p.texture);
        gl.bind_texture(glow::TEXTURE_2D, texture);
        gl.copy_tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, 0, 0, w, h);
        gl.bind_texture(glow::TEXTURE_2D, None);
    }
    Ok(())
}

fn reset_gl(gl: &glow::Context, screen: [u32; 2]) {
    use glow::HasContext as _;
    // SAFETY: plain state resets on the current context.
    unsafe {
        gl.disable(glow::SCISSOR_TEST);
        gl.enable(glow::BLEND);
        gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
        gl.bind_vertex_array(None);
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        gl.use_program(None);
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.viewport(0, 0, screen[0] as i32, screen[1] as i32);
    }
}

fn egui_key(sym: Keysym) -> Option<egui::Key> {
    use egui::Key as K;
    Some(match sym {
        Keysym::BackSpace => K::Backspace,
        Keysym::Return | Keysym::KP_Enter | Keysym::Linefeed => K::Enter,
        Keysym::Tab | Keysym::ISO_Left_Tab => K::Tab,
        Keysym::Escape => K::Escape,
        Keysym::Delete => K::Delete,
        Keysym::Home => K::Home,
        Keysym::End => K::End,
        Keysym::Page_Up => K::PageUp,
        Keysym::Page_Down => K::PageDown,
        Keysym::Left => K::ArrowLeft,
        Keysym::Right => K::ArrowRight,
        Keysym::Up => K::ArrowUp,
        Keysym::Down => K::ArrowDown,
        Keysym::space => K::Space,
        _ => {
            let c = sym.key_char()?.to_ascii_uppercase();
            K::from_name(&c.to_string())?
        }
    })
}

impl<S: Shell> BufferHandler for Host<S> {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl<S: Shell + 'static> CompositorHandler for Host<S> {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("every client is inserted with ClientState")
            .compositor_state
    }

    #[cfg(feature = "kms")]
    fn new_surface(&mut self, surface: &WlSurface) {
        // Without the dma-buf global no buffer can carry a fence.
        if self.dmabuf_global.is_some() {
            add_pre_commit_hook::<Self, _>(surface, |host, _, surface| {
                host.wait_for_buffer(surface);
            });
        }
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Also reached outside a client's dispatch, when a commit that
        // waited on its buffer's fences or a commit timer is applied.
        self.requested = true;
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some((_, window)) = self.wayland_window_of(&root) {
                window.on_commit();
            } else if let Some(layer) = self
                .layers
                .iter()
                .find(|l| l.window.toplevel().is_some_and(|t| t.wl_surface() == &root))
            {
                layer.window.on_commit();
            }
        }
        self.manage_on_first_commit(surface);
        self.icon_committed(surface);
        self.size_committed(surface);
        self.wlr_commit(surface);
        self.input_popup_commit(surface);
        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
        {
            let _ = popup.send_configure();
        }
    }
}

impl<S: Shell + 'static> XdgShellHandler for Host<S> {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        self.unmanaged.push(Window::new_wayland_window(surface));
    }

    fn client_pong(&mut self, client: ShellClient) {
        self.ponged(&client);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unmanaged
            .retain(|w| w.toplevel().is_some_and(|t| t != &surface));
        if let Some(i) = self
            .layers
            .iter()
            .position(|l| l.window.toplevel().is_some_and(|t| t == &surface))
        {
            let layer = self.layers.remove(i);
            self.space.unmap_elem(&layer.window);
            if self.layer_focus.as_ref() == Some(surface.wl_surface()) {
                self.layer_focus = None;
            }
            if self.route == Some(Route::Layer) {
                self.route = None;
            }
            self.sync();
            return;
        }
        if let Some((id, window)) = self
            .wayland_window_of(surface.wl_surface())
            .map(|(id, w)| (id, w.clone()))
        {
            self.space.unmap_elem(&window);
            self.windows.remove(&id);
            self.shell.unmap_window(id);
            if self.keyboard_focus == Some(id) {
                self.keyboard_focus = None;
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
                }
            }
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        let Some((id, _)) = self.wayland_window_of(surface.wl_surface()) else {
            return;
        };
        let app_id = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok())
                .and_then(|d| d.app_id.clone())
        });
        if let Some(app_id) = app_id {
            self.shell.set_app_id(id, &app_id);
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let Some((id, _)) = self.wayland_window_of(surface.wl_surface()) else {
            return;
        };
        let title = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok())
                .and_then(|d| d.title.clone())
        });
        self.shell.set_title(id, &title.unwrap_or_default());
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Popup grabs are not implemented; popups close when their client
        // dismisses them.
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        Host::parent_changed(self, &surface);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some((id, _)) = self.wayland_window_of(surface.wl_surface()) {
            self.shell.client_request(id, ClientRequest::Maximize);
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some((id, _)) = self.wayland_window_of(surface.wl_surface()) {
            self.shell.client_request(id, ClientRequest::Minimize);
        }
    }
}

impl<S: Shell + 'static> XdgDecorationHandler for Host<S> {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        // The shell always draws the decorations.
        self.new_decoration(toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.new_decoration(toplevel);
    }
}

#[cfg(feature = "kms")]
impl<S: Shell + 'static> Host<S> {
    /// Holds back a commit whose new dma-buf the client's GPU has not
    /// finished drawing, until it has: the explicit acquire point when the
    /// client sent one, else the buffer's implicit fences. Drawing it
    /// earlier would sample a half-drawn frame where the driver does not
    /// wait on the fences itself, or stall the compositor's whole frame on
    /// one client where it does.
    fn wait_for_buffer(&mut self, surface: &WlSurface) {
        let (acquire, dmabuf) = with_states(surface, |states| {
            let acquire = states
                .cached_state
                .get::<DrmSyncobjCachedState>()
                .pending()
                .acquire_point
                .clone();
            let dmabuf = match &states
                .cached_state
                .get::<SurfaceAttributes>()
                .pending()
                .buffer
            {
                Some(BufferAssignment::NewBuffer(buffer)) => get_dmabuf(buffer).ok().cloned(),
                _ => None,
            };
            (acquire, dmabuf)
        });
        let (Some(dmabuf), Some(client)) = (dmabuf, surface.client()) else {
            return;
        };
        if let Some(acquire) = acquire {
            match acquire.generate_blocker() {
                Ok((blocker, source)) => {
                    if self.unblock_when(source, client.clone()) {
                        add_blocker(surface, blocker);
                        return;
                    }
                }
                // Explicit sync is only offered where this works; if it
                // still fails, fall back to the implicit fences below.
                Err(e) => warn!(error = %e, "cannot wait on an acquire point"),
            }
        }
        // `Err` here means every fence has already signalled.
        if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ)
            && self.unblock_when(source, client)
        {
            add_blocker(surface, blocker);
        }
    }

    /// Re-evaluates `client`'s blocked commits once `source` fires; false
    /// if the source could not be put on the loop.
    fn unblock_when<Src>(&self, source: Src, client: Client) -> bool
    where
        Src: EventSource<Event = (), Ret = io::Result<()>> + 'static,
    {
        let inserted = self.loop_handle.insert_source(source, move |_, _, host| {
            let dh = host.display.clone();
            host.client_compositor_state(&client)
                .blocker_cleared(host, &dh);
            Ok(())
        });
        if let Err(e) = &inserted {
            warn!(error = %e.error, "cannot wait on a buffer's fence");
        }
        inserted.is_ok()
    }
}

#[cfg(feature = "kms")]
impl<S: Shell + 'static> DmabufHandler for Host<S> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    /// Imports the buffer into the renderer as it is created, so a client
    /// whose buffer the GPU cannot sample learns now (and can try another
    /// format or modifier) rather than showing nothing later. The renderer
    /// keeps the import for when the buffer is drawn.
    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        match self.backend.renderer().import_dmabuf(&dmabuf, None) {
            Ok(_) => {
                let _ = notifier.successful::<Self>();
            }
            Err(e) => {
                warn!(error = %e, format = ?dmabuf.format(), "cannot import a client's dma-buf");
                notifier.failed();
            }
        }
    }
}

#[cfg(feature = "kms")]
impl<S: Shell + 'static> DrmSyncobjHandler for Host<S> {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}

impl<S: Shell> ShmHandler for Host<S> {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl<S: Shell + 'static> SeatHandler for Host<S> {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, seat: &Seat<Self>, image: CursorImageStatus) {
        self.set_cursor(seat, image);
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|s| self.display.get_client(s.id()).ok());
        set_data_device_focus(&self.display, seat, client.clone());
        set_primary_focus(&self.display, seat, client);
        self.text_inputs.set_focus(focused.cloned());
        self.text_input_changed();
    }
}

impl<S: Shell + 'static> SelectionHandler for Host<S> {
    type SelectionUserData = String;

    fn send_selection(
        &mut self,
        _ty: smithay::wayland::selection::SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        if matches!(
            mime_type.as_str(),
            "text/plain" | "text/plain;charset=utf-8" | "UTF8_STRING"
        ) {
            let Some(guard) = SelectionTransferGuard::acquire() else {
                return;
            };
            let text = user_data.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("mcsapi-selection-send".into())
                .spawn(move || {
                    let _guard = guard;
                    let _ = write_selection(fd, text.as_bytes(), SELECTION_WRITE_TIMEOUT);
                })
            {
                warn!(%error, "cannot start clipboard transfer");
            }
        }
    }
}

impl<S: Shell + 'static> DataDeviceHandler for Host<S> {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl<S: Shell + 'static> PrimarySelectionHandler for Host<S> {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    #[test]
    fn egui_repaint_requests_come_due_once() {
        let repaint = Repaint::default();
        let ctx = egui::Context::default();
        repaint.watch(&ctx);
        // A pass with nothing in it; its font atlas upload is dropped.
        let pass = |ctx: &egui::Context| {
            ctx.run_ui(egui::RawInput::default(), |_| {})
                .textures_delta
                .clear();
        };
        // A new context asks for its first frames itself; once it settles,
        // a pass that asks for nothing leaves nothing due...
        pass(&ctx);
        let mut passes = 1;
        while repaint.take_due(Instant::now()) {
            pass(&ctx);
            passes += 1;
            assert!(passes < 10, "an empty context kept asking for frames");
        }
        assert!(passes > 1);
        assert!(!repaint.take_due(Instant::now() + Duration::from_secs(3600)));
        // ...and one asked for later comes due then, not before.
        ctx.request_repaint_after(Duration::from_secs(10));
        assert!(!repaint.take_due(Instant::now() + Duration::from_secs(5)));
        assert!(repaint.take_due(Instant::now() + Duration::from_secs(11)));
        assert!(!repaint.take_due(Instant::now() + Duration::from_secs(11)));
        // From another thread, as an app's worker asks.
        let other = ctx.clone();
        std::thread::spawn(move || other.request_repaint())
            .join()
            .unwrap();
        assert!(repaint.take_due(Instant::now()));
    }

    #[test]
    fn opaque_panels_cover_an_app_but_gaps_and_glass_do_not() {
        let area = Geometry::new((10, 20).into(), (100, 50).into());
        let rect = |x0: f32, x1: f32, fill: egui::Color32| egui::epaint::ClippedShape {
            clip_rect: egui::Rect::EVERYTHING,
            shape: egui::Shape::rect_filled(
                egui::Rect::from_min_max(egui::pos2(x0, 20.0), egui::pos2(x1, 70.0)),
                0,
                fill,
            ),
        };
        let panel = egui::Color32::from_rgb(20, 20, 30);
        // A side panel and a central panel meeting at x = 40.
        assert!(covers(
            &[rect(10.0, 40.0, panel), rect(40.0, 110.0, panel)],
            area
        ));
        // A pixel-wide gap between them.
        assert!(!covers(
            &[rect(10.0, 39.5, panel), rect(40.5, 110.0, panel)],
            area
        ));
        // A translucent panel lets what is under it show.
        let glass = egui::Color32::from_black_alpha(200);
        assert!(!covers(
            &[rect(10.0, 40.0, panel), rect(40.0, 110.0, glass)],
            area
        ));
    }

    #[test]
    fn a_clipped_pass_is_drawn_only_in_its_areas() {
        let mesh = |n| egui::ClippedPrimitive {
            clip_rect: egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0)),
            primitive: egui::epaint::Primitive::Mesh(egui::epaint::Mesh {
                indices: vec![n],
                ..Default::default()
            }),
        };
        let mut pass = Pass {
            primitives: vec![mesh(1), mesh(2)],
            textures: Default::default(),
        };
        // The screen less a window over its right half, and an area
        // outside the primitives' clip.
        let shown = [
            Rectangle::new((0, 0).into(), (50, 100).into()),
            Rectangle::new((200, 0).into(), (10, 10).into()),
        ];
        clip_pass(&mut pass, &shown);
        let clips: Vec<_> = pass.primitives.iter().map(|p| p.clip_rect).collect();
        let left = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(50.0, 100.0));
        assert_eq!(clips, [left, left]);
    }

    #[test]
    fn frames_keep_their_rate_however_long_they_take() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let frame = ms(16);
        // Drawn in 6 ms: the next is still 16 ms after this one was due.
        assert_eq!(next_tick(t0, frame, t0 + ms(6)), t0 + frame);
        // Due late by 3 ms and drawn in 6: the rate holds, not the delay.
        assert_eq!(next_tick(t0, frame, t0 + ms(9)), t0 + frame);
        // 40 ms to draw: the ticks it overran are skipped, not drawn at once.
        assert_eq!(next_tick(t0, frame, t0 + ms(40)), t0 + ms(48));
        // Exactly on a tick: that one is gone, so the one after.
        assert_eq!(next_tick(t0, frame, t0 + ms(32)), t0 + ms(48));
        // Back from a suspend: count from now.
        let back = t0 + Duration::from_secs(30);
        assert_eq!(next_tick(t0, frame, back), back + frame);
    }

    #[test]
    fn a_moved_pass_is_stretched_and_faded() {
        let window = Geometry::new((100, 100).into(), (400, 200).into());
        let m = Motion {
            shown: Geometry::new((200, 150).into(), (200, 100).into()),
            opacity: 0,
        };
        let mut mesh = egui::epaint::Mesh::default();
        mesh.colored_vertex(egui::pos2(500.0, 300.0), egui::Color32::WHITE);
        let mut pass = Pass {
            primitives: vec![egui::ClippedPrimitive {
                clip_rect: egui::Rect::from_min_max(
                    egui::pos2(100.0, 100.0),
                    egui::pos2(500.0, 300.0),
                ),
                primitive: egui::epaint::Primitive::Mesh(mesh),
            }],
            textures: Default::default(),
        };
        move_pass(&mut pass, window, m);
        let clipped = &pass.primitives[0];
        assert_eq!(
            clipped.clip_rect,
            egui::Rect::from_min_max(egui::pos2(200.0, 150.0), egui::pos2(400.0, 250.0))
        );
        let egui::epaint::Primitive::Mesh(mesh) = &clipped.primitive else {
            unreachable!()
        };
        assert_eq!(mesh.vertices[0].pos, egui::pos2(400.0, 250.0));
        assert_eq!(mesh.vertices[0].color, egui::Color32::TRANSPARENT);
    }

    #[test]
    fn a_moved_surface_covers_whole_pixels() {
        let window = Geometry::new((0, 0).into(), (300, 300).into());
        let m = Motion {
            shown: Geometry::new((10, 10).into(), (200, 200).into()),
            opacity: 255,
        };
        let r = moved_rect(window, m, Rectangle::new((0, 30).into(), (300, 270).into()));
        assert_eq!(r, Rectangle::new((10, 30).into(), (200, 180).into()));
        let odd = moved_rect(window, m, Rectangle::new((1, 1).into(), (1, 1).into()));
        assert_eq!(odd, Rectangle::new((10, 10).into(), (2, 2).into()));
    }

    struct TestShell {
        keys: Vec<bool>,
    }

    impl Shell for TestShell {
        fn map_window(&mut self, _app_id: &str, _title: &str) -> WindowId {
            WindowId::new(1).unwrap()
        }

        fn unmap_window(&mut self, _window: WindowId) {}

        fn set_output(&mut self, _size: (i32, i32)) {}

        fn focused(&self) -> Option<WindowId> {
            None
        }

        fn placements(&self) -> Vec<Placement> {
            Vec::new()
        }

        fn key(&mut self, key: &KeyInput) -> KeyRoute {
            self.keys.push(key.pressed);
            if key.pressed {
                KeyRoute::Consume
            } else {
                KeyRoute::Client
            }
        }
    }

    #[test]
    fn consumed_key_release_reaches_shell_but_not_client() {
        let mut shell = TestShell { keys: Vec::new() };
        let key = |pressed| KeyInput {
            sym: Keysym::Escape,
            text: None,
            pressed,
            mods: Modifiers::default(),
        };

        assert_eq!(
            shell_key_route(&mut shell, &key(true), false),
            KeyRoute::Consume
        );
        assert_eq!(
            shell_key_route(&mut shell, &key(false), true),
            KeyRoute::Consume
        );
        assert_eq!(shell.keys, [true, false]);
    }

    #[test]
    fn clipboard_transfers_are_bounded() {
        let guards: Vec<_> = (0..MAX_SELECTION_TRANSFERS)
            .map(|_| SelectionTransferGuard::acquire().unwrap())
            .collect();
        assert!(SelectionTransferGuard::acquire().is_none());
        drop(guards);
        assert!(SelectionTransferGuard::acquire().is_some());
    }

    #[test]
    fn stalled_clipboard_write_times_out() {
        let mut fds = [0; 2];
        // SAFETY: fds points to two writable integers for pipe to initialize.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe initialized both descriptors and ownership is transferred exactly once.
        let _read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: pipe initialized both descriptors and ownership is transferred exactly once.
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        let error =
            write_selection(write, &vec![0; 1024 * 1024], Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}

impl<S: Shell + 'static> ClientDndGrabHandler for Host<S> {}
impl<S: Shell + 'static> ServerDndGrabHandler for Host<S> {}
impl<S: Shell> OutputHandler for Host<S> {}

delegate_compositor!(@<S: Shell + 'static> Host<S>);
delegate_xdg_shell!(@<S: Shell + 'static> Host<S>);
delegate_xdg_decoration!(@<S: Shell + 'static> Host<S>);
delegate_shm!(@<S: Shell + 'static> Host<S>);
delegate_seat!(@<S: Shell + 'static> Host<S>);
delegate_data_device!(@<S: Shell + 'static> Host<S>);
delegate_primary_selection!(@<S: Shell + 'static> Host<S>);
delegate_pointer_gestures!(@<S: Shell + 'static> Host<S>);
delegate_output!(@<S: Shell + 'static> Host<S>);
#[cfg(feature = "kms")]
delegate_dmabuf!(@<S: Shell + 'static> Host<S>);
#[cfg(feature = "kms")]
delegate_drm_syncobj!(@<S: Shell + 'static> Host<S>);
