# mcsapi-compositor

A Smithay Wayland compositor host for mcsapi desktops. It owns the display,
the Wayland socket, rendering and input; a desktop shell plugs in through the
`Shell` trait and decides placements, shortcuts, title-bar behavior, and
paints its wallpaper, decorations and chrome with egui.

Windows come from two places, and get the same decorations, tiling and focus:

- **Wayland clients** connecting to the socket (asked to use server-side
  decorations through xdg-decoration).
- **In-process apps**: `mcsapi_ui::App`s registered with `mcsapi-runtime`,
  provided through the `Apps` trait (for example the derisk core apps).

Wayland protocols are listed under [Protocols](#protocols). The session runs nested in a window of the current X11 or Wayland session
(Smithay's winit backend, GLES renderer, egui painted with `egui_glow`), or,
with the `kms` feature, on the bare seat: every connected display at its
preferred mode through DRM/KMS and GBM, the first one the output and the
others showing the same frame scaled to fit, displays plugged in or out
followed as they come and go, input from libinput, and the GPU and
input devices taken through libseat (logind or seatd) and given back on a VT
switch (Ctrl+Alt+F1–F12). The bare seat is used when neither `WAYLAND_DISPLAY`
nor `DISPLAY` is set; `MCSAPI_BACKEND=kms` or `=winit` decides instead. On the
bare seat each frame is drawn offscreen and copied to the scanout buffer, so
every layer comes out the same as nested; `MCSAPI_OFFSCREEN=1` takes the same
path in a window, to compare the two.

Effects: shells can ask for frosted-glass panels by returning areas from
`Shell::blur_regions`; the host blurs what is behind them (dual Kawase, with
rounded corners) before painting the chrome, and the panel's own fill alpha
sets how much shows through. Frames follow the refresh rate of the monitor the
session window is on; `Shell::frame_interval` can slow them down, and
`OutputTiming::interval_for` picks a rate the display shows evenly (any rate
with VRR, a whole number of refreshes without; declare VRR with
`Compositor::vrr`, since a nested session cannot detect it).

Live window pictures: while `Shell::wants_window_pictures` holds, the host
copies the screen into a texture once every window is drawn and before the
panels and chrome are, and hands it over with `Shell::window_pictures`, so an
overview can draw what each window shows (`WindowPictures::uv` maps a window's
frame onto it). It is the same for Wayland clients and in-process apps, and
costs a screen-sized copy each frame, so it is off unless asked for.

Window motion: a placement's `motion` draws the window somewhere else, or
faded, for the frame (`Motion::shown`, `Motion::opacity`), so a shell can
grow an opening window in or glide a retiled one to its new place. The
decoration, an in-process app's pass and a client's surfaces are stretched
from `frame` to `shown`, nothing is laid out again, and the client and input
stay where `frame` and `client` say. A moving window gets no blur behind it.

Accessibility: the host merges the chrome's egui tree, every in-process
app's tree, a node per window and any `Shell::access_subtrees` (title-bar
buttons, nodes an out-of-process program registered) into one AccessKit
tree with IDs that stay the same while an element exists. With the default
`atspi` feature it is published over AT-SPI for screen readers, and
screen-reader actions come back through the same paths as real input. The
focused widget gets a visible ring (`mcsapi_ui::paint_focus_ring`) after
keyboard or AT-SPI focus, not after a click.

Computer use: `Command::Describe` returns that tree flattened
(`a11y::Snapshot`), `Command::Capture` reads back a frame, `Command::Act`
performs an accessibility action on an element, and `Command::Input`
injects pointer and keyboard input into the seat. Act and Input run one per
frame, so each click lands on what the last one drew, and a Describe or
Capture queued after them sees their result. While one runs,
`Shell::input_source(true)` tells the shell the input is synthetic, so it
can refuse to let an agent confirm what only a person should.

Not yet: displays that extend the desktop rather than mirror it, direct
scanout of client buffers, XWayland, popup grabs, linux-dmabuf.

## Protocols

Core and windows: `wl_compositor`, `wl_subcompositor`, `wl_shm`, `wl_seat`
(keyboard and pointer), `wl_output` with `zxdg_output_manager_v1`,
`xdg_wm_base` (toplevels and popups), `zxdg_decoration_manager_v1` (always
server-side).

Buffers and surfaces: `wp_viewporter`, `wp_fractional_scale_v1` (every
surface is told scale 1, which is what the compositor draws at),
`wp_single_pixel_buffer_v1`, `wp_alpha_modifier_v1`, and the hints
`wp_content_type_v1` and `wp_tearing_control_v1`, recorded per surface; frames
stay in step with the refresh either way. `ext_background_effect_manager_v1` blurs what is
behind the region a client asks for (a translucent terminal or bar), with
the same blur as `Shell::blur_regions`.

Window hints, passed to `Shell::window_hint`: parents from
`xdg_toplevel.set_parent` and `zxdg_exporter_v2`/`zxdg_importer_v2`
(xdg-foreign), modal dialogs from `xdg_wm_dialog_v1`, icons from
`xdg_toplevel_icon_manager_v1`, tags and descriptions from
`xdg_toplevel_tag_manager_v1`. `xdg_system_bell_v1` rings `Shell::bell`.
`xdg_activation_v1` reaches `Shell::activate` only with a token the client
that had the keyboard asked for, or the one a launch puts in
`XDG_ACTIVATION_TOKEN`, so windows cannot steal focus.

Input and clipboard: `wl_data_device_manager` (clipboard and drag and drop;
the compositor keeps a copy of each selection's text, HTML, file lists and
one picture, and serves it once the client that copied it has gone, and
in-process apps and the chrome copy and paste with Ctrl+C, Ctrl+X and
Ctrl+V like any client),
`zwp_primary_selection_device_manager_v1` (middle-click paste),
`zwp_pointer_gestures_v1`, `zwp_text_input_manager_v3` (see
`Shell::text_input`), `zwp_relative_pointer_manager_v1` and
`zwp_pointer_constraints_v1` (games and remote desktops lock or confine the
pointer while it is over them and focused), `wp_pointer_warp_v1` (a client
may move the pointer only within its own surface while it has it), and
`zwp_tablet_manager_v2` (pens over clients; over the chrome and in-process
apps a pen moves the pointer, its tip and buttons click).
`zwp_keyboard_shortcuts_inhibit_manager_v1` passes the shell's shortcuts to
a focused client that asks, if `Shell::inhibit_shortcuts` allows it.

Input methods: `zwp_input_method_manager_v2` (fcitx5, squeekboard) edits the
focused `zwp_text_input_v3` field: it hears the field's surrounding text and
content type, and its preedit, commits and deletions go back to the field.
Its keyboard grab gets the seat's keys after the shell's shortcuts, and its
candidate popup is drawn under the field's cursor, above everything but a
session lock. One input method is connected at a time.

Cursors: `wp_cursor_shape_manager_v1` and cursor surfaces. Named shapes, and
the shapes egui asks for over the chrome and in-process apps, come from the
XCursor theme in `XCURSOR_THEME` at `XCURSOR_SIZE`; the compositor paints its
own arrow when there is no theme.

Idle: `ext_idle_notifier_v1` tells clients when the seat has been idle, and
`zwp_idle_inhibit_manager_v1` keeps it awake while an inhibiting surface is
on screen, reported through `Shell::idle_inhibited` (a video player
playing).

Desktop components from other projects: `zwlr_layer_shell_v1` (bars,
wallpapers, launchers, notifications such as waybar, swaybg, fuzzel, mako
and slurp). Background and bottom surfaces sit under windows, top ones with
runtime panels under the chrome, overlay ones above it; exclusive zones reach
`Shell::set_reserved` together with runtime panels. A surface that asks for
the keyboard exclusively on the top or overlay layer gets it, and one that
asks on demand gets it when clicked, after the shell's shortcuts.

Tools that drive the session: `ext_data_control_manager_v1` and
`zwlr_data_control_manager_v1` (wl-copy, clipboard managers),
`zwp_virtual_keyboard_manager_v1` (wtype; keys go straight to the focused
client) and `zwlr_virtual_pointer_manager_v1` (wayvnc, wlrctl; the same path
as a mouse, reported to `Shell::input_source` as synthetic).

Window lists and pagers: `ext_foreign_toplevel_list_v1` and
`zwlr_foreign_toplevel_manager_v1` list every window, Wayland and in-process,
with title, app ID, parent, focus and maximized state, and let a taskbar
focus (`Shell::activate`), close, maximize or minimize one
(`Shell::client_request`). `ext_workspace_manager_v1` shows what
`Shell::workspaces` lists and passes activation to
`Shell::activate_workspace`.

Frame timing: `wp_presentation` reports when a commit reached the screen,
on the bare seat with the vblank's timestamp and counter; `wp_fifo_manager_v1`
and `wp_commit_timing_manager_v1` (Mesa's Vulkan FIFO mode, video players)
hold commits until the previous one was shown or the requested time. A
hidden window's FIFO barriers still clear every frame.

The display: `zwlr_output_manager_v1` (wlr-randr, kanshi) lists the output;
a configuration that keeps it as it is succeeds and any other fails, since
there is one output at one mode, which every display mirrors. `zwlr_output_power_manager_v1` (wlopm,
swayidle) turns it off, with DPMS on the bare seat, and nothing is drawn
until it is back on. `zwlr_gamma_control_manager_v1` (gammastep, wlsunset)
sets the bare seat's gamma ramp, on every display, for one client at a
time; nested it fails.

Screen capture: `ext_image_copy_capture_manager_v1` with output and window
sources (`ext_output_image_capture_source_manager_v1`,
`ext_foreign_toplevel_image_capture_source_manager_v1`), and
`zwlr_screencopy_manager_v1` (grim, wf-recorder, wayvnc,
xdg-desktop-portal-wlr). Captures go into `wl_shm` buffers from the next
frame, with or without the pointer; a window is cut out of the frame where
its content is placed.

Screen lockers: `ext_session_lock_v1` (swaylock, hyprlock). While locked,
only the locker's surface is shown and every key and pointer event goes to
it; the shell's shortcuts, gestures and chrome get none, and
`Shell::session_locked` says when it starts and ends. A locker that dies
leaves the session locked until another one unlocks it.

Sandboxes: `wp_security_context_manager_v1` marks clients that connect
through a socket a sandbox (Flatpak) opened. They do not see layer-shell,
data control, the virtual keyboard or pointer, input methods, the session
lock, the window lists and workspaces, screen capture, output management,
power and gamma, or the security context manager itself.

## Try it

```console
$ cargo run -p mcsapi-compositor --example tiling -- foot clock
```

`clock` is a built-in in-process app; anything else is spawned with
`WAYLAND_DISPLAY` set to the session. Super+Enter opens foot, Super+C the
clock, Super+J/K move focus, Super+Space promotes, Super+1…4 switch
workspaces, Super+Q closes, Super+B toggles the frosted dock's blur, Super+L
toggles a ≤30 fps low power mode, Super+Escape quits.

## Hot reload in a nested instance

Run a nested session that picks up code changes while it keeps running:

```console
$ cargo install --locked dioxus-cli@0.7.10   # once
$ dx serve --hot-patch --platform linux -p mcsapi-compositor --example tiling \
    --features hotpatch --args clock
```

Edit a `Shell` method in `examples/tiling.rs` (placements, keys, painting,
chrome) and save: about a second later the window draws with the new code.
The window, its Wayland clients and the shell's state stay alive. This is
[Subsecond](https://crates.io/crates/subsecond) hot-patching, wired in through
the `hotpatch` feature:

- Wrap the shell in `Hot` (`Compositor::new(Hot(shell))`). Each call into
  the shell then goes through Subsecond's jump table and runs the newest
  version. Without the feature, or in release builds, `Hot` is a plain
  pass-through and nothing extra is compiled in.
- `Compositor::run` connects to `dx serve` for patches; run normally, it
  does nothing.
- Only the binary's own crate is patched (the example, or a desktop's
  `main.rs` crate). Changes to a struct's fields, to this library or to
  other crates need a rebuild; press `r` in `dx serve` to restart.

Without the Dioxus CLI, `bacon nested` (from the root `bacon.toml`) rebuilds
and restarts the nested instance on every save of any workspace crate.

The nested window opens on whatever `WAYLAND_DISPLAY` or `DISPLAY` the
command starts with, so a terminal inside another session (an mcsapi one
included) nests the instance there.

## Using it

Implement `Shell` (only window bookkeeping and `placements` are required),
optionally an `Apps` provider, then:

```rust,ignore
let mut compositor = Compositor::new(shell).apps(apps).size(1600, 900);
let remote = compositor.remote(); // run jobs on the shell from other threads
compositor.launch("foot").run()?;
```

Building needs `libxkbcommon-dev`. Running needs EGL/GLES drivers (Mesa's
llvmpipe works, also under Xvfb).

## GPUI runtime clients

GPUI draws only into windows it opens itself, as an ordinary Wayland client,
and has no layer-shell, so a shell cannot paint GPUI inside the compositor.
Instead the compositor starts GPUI programs as runtime clients, on a private
connection it creates with a socket pair and hands over as `WAYLAND_SOCKET`.
Their toplevels get the role they were started with, not one they ask for:

```rust,ignore
use mcsapi_compositor::{Edge, Role, RuntimeClient};

let panel = Role::Panel { edge: Edge::Top, size: 32, keyboard: false };
compositor
    .runtime(RuntimeClient::new(["my-gpui-panel"], panel).restart(true))
    .runtime(RuntimeClient::new(["my-gpui-launcher"], Role::Overlay))
    .run()?;
```

- `Role::Panel` sits on its edge above windows and below the shell's chrome,
  and its strip is passed to `Shell::set_reserved` to keep windows out.
- `Role::Overlay` covers the output above the chrome and gets every key and
  pointer event while mapped.
- `Role::App` is managed like any other window.

The child also gets `MCSAPI_ROLE` (`app`, `panel` or `overlay`). A shell can
start one at run time with `Command::Runtime`. A client with `restart` comes
back when it exits, unless it ran for less than two seconds.
