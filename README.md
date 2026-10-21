# mcsapi

A small Rust scaffold for xmonad-like desktop policy on
[Smithay](https://smithay.github.io/). `mcsapi-compositor` turns it into a runnable
(nested) Wayland compositor; see `cargo run -p mcsapi-compositor --example tiling`.

## Workspace layout

This repository is a Cargo workspace. Every directory under `crates/` is a
member:

| Crate | Path | Purpose |
| --- | --- | --- |
| `mcsapi` | `crates/mcsapi` | Desktop policy, layouts, toolkit selection, and shell widgets (the original library; public API unchanged). |
| `mcsapi-theme` | `crates/mcsapi-theme` | Theming engine: theme files, derived component tokens, and exports for the freedesktop appearance settings, GTK settings, cursors, Android resources and JSON. No dependencies, so any process can read the theme. |
| `mcsapi-ui` | `crates/mcsapi-ui` | UI toolkit for apps: an `App` trait drawn with egui and the shared shell `Theme`, 1:1 touchpad gestures (`gesture`), and the `Result` apps report failures with (`error`): a miette report that names the documentation section explaining it, which `ErrorDialog` (an alert dialog) or the inline `ErrorAlert` in `mcsapi-components` and `mcsapi-components-gpui` draws with a "Learn more" button, offline copy first. |
| `mcsapi-runtime` | `crates/mcsapi-runtime` | Separate runtime for running apps: app registration and instance lifecycle. Starter. |
| `mcsapi-components-gpui` | `crates/mcsapi-components-gpui` | The `mcsapi-components` library as native GPUI elements, behind its `gpui` feature. Buttons, toggles, badges, checkboxes, switches, alerts, separators, labels, progress bars and tooltips are drawn by Zed's `ui` components; `Assets` serves the icons they draw. |
| `mcsapi-zed-ui` and friends | `crates/mcsapi-zed-*` | Zed's GPUI component stack, copied from [Sonne](https://github.com/dasmatus/sonne) under GPL-3.0-or-later with Zed's library names: `ui`, `component`, `menu`, `ui_macros`, `icons` (with the SVGs embedded) and `theme`. The theme crate has no registry or JSON themes; `theme::theme_from_mcsapi` builds Zed's `ThemeColors` from an mcsapi theme's tokens. Behind each crate's `gpui` feature. |
| `mcsapi-gallery` | `crates/mcsapi-gallery` | Widget gallery in a GPUI window (`--features gpui`): every shell widget and component with its variants and states, under switchable themes. |
| `x2mcsapi` | `crates/x2mcsapi` | Adapter that restyles foreign apps (web pages and Electron via an injected script, GTK 3/4, Qt Widgets) from the shell `Theme`, so they look coherent with derisk. |
| `mcsapi-mcp` | `crates/mcsapi-mcp` | Stateless MCP server exposing desktop policy as `simulate` and `arrange` tools over Streamable HTTP. |
| `mcsapi-compositor` | `crates/mcsapi-compositor` | Smithay Wayland compositor host: runs a desktop shell (the `Shell` trait) with Wayland clients and in-process `mcsapi-runtime` apps side by side. Nested (winit) backend, and DRM/KMS on a bare seat with the `kms` feature. |
| `mcsapi-hardened-malloc` | `crates/mcsapi-hardened-malloc` | GrapheneOS's [hardened_malloc](https://github.com/GrapheneOS/hardened_malloc) as a `#[global_allocator]`. Links the system's `libhardened_malloc.so` (set `HARDENED_MALLOC_LIB_DIR` if the linker cannot find it), so a process the OS already preloads it into keeps one heap, and frees through `free_sized` so a wrong length aborts. |

Shared package metadata and dependency versions live in the root
`Cargo.toml` under `[workspace.package]` and `[workspace.dependencies]`; a new
crate inherits them with `version.workspace = true` and `dep.workspace = true`.

`mcsapi-runtime` does not depend on the UI toolkit, so apps may use any
toolkit and a compositor can use `mcsapi` without either starter crate.

## What is here (`mcsapi`)

- Distinct nonzero `WindowId` and `WorkspaceId` types.
- Fixed workspaces, unique window membership, focus cycling, promotion to the
  main pane, and moving windows between workspaces.
- Tall and monocle layouts using Smithay's logical-coordinate rectangles.
- Iterator-first APIs: ordered trees own state; window/workspace enumeration
  and lazy placement generation need no temporary collections. No `Vec`
  storage is used by mcsapi; upstream toolkits may use their own collections.
- Optional GPUI integration and an always-available egui fallback context.
- Matching native workspace bars with shared theme colors, rounded surfaces,
  active labels, and keyboard activation.

## Quick start

Requires Rust 1.95 or later. On Linux, Smithay needs the xkbcommon development
library (`libxkbcommon-dev` on Debian/Ubuntu).

```rust
use mcsapi::{Desktop, Geometry, WindowId, WorkspaceId};

fn main() -> Result<(), mcsapi::Error> {
    let mut desktop =
        Desktop::new((1..=9).map(|id| WorkspaceId::new(id).unwrap()))?;
    desktop.insert(WindowId::new(1).unwrap())?;
    desktop.insert(WindowId::new(2).unwrap())?;
    desktop.active_mut().focus_previous();
    desktop.active_mut().promote_focused();

    let bounds = Geometry::new((0, 0).into(), (1920, 1080).into());
    for placement in desktop.active().arrange(bounds)? {
        // Apply this logical rectangle to the host's corresponding surface.
        println!("{}: {:?}", placement.window, placement.geometry);
    }
    Ok(())
}
```

Workspaces iterate by ID; the first supplied ID is initially active. Within a
workspace the main window comes first, then the other windows by ID. Insertion
focuses the new window. Focus wraps in this same order. Removing a focused window
focuses its next neighbor, wrapping when needed. IDs are assigned by the host:
do not reuse an ID while stale commands for it can still arrive.

`arrange` validates bounds before returning an exact-size iterator. Tall puts
one window in the left half and stacks the others on the right, distributing
remainder pixels from the top. Tiny outputs return `InsufficientSpace`, rather
than emitting zero-sized tiles. Monocle configures all windows to the same
bounds; the host displays only the focused one.

## Smithay integration boundary

The `mcsapi::smithay` re-export and `Geometry = Rectangle<i32, Logical>` use the
actual Smithay types, with no coordinate conversion or extra protocol wrapper.
Only `wayland_frontend` is enabled by default. A host can enable additional
Smithay features in its own dependency declaration.

The compositor host must implement:

1. Display/socket setup, protocol handlers, output discovery, seats, and input.
2. A mapping from `WindowId` to live surfaces/toplevels; call `insert` when a
   window is managed and `remove` when it is destroyed.
3. Keybindings calling the focus, promotion, workspace, and move methods.
4. XDG configure/commit handling, placement, focus synchronization, stacking,
   rendering, damage tracking, and repaint scheduling.

Policy state does not claim a surface is already configured, nor manage its
lifetime. A useful host starting point is Smithay's
[Smallvil example](https://github.com/Smithay/smithay/tree/v0.7.0/smallvil).
Multi-output policy, floating windows, IPC, and session management are future
extensions, not implemented features.

## Toolkit selection and widgets

Enable the native Wayland GPUI integration with `--features gpui`. On
Debian/Ubuntu its build prerequisites include `libwayland-dev`,
`libxkbcommon-dev`, `libxkbcommon-x11-dev`, `libfontconfig1-dev`,
`libssl-dev`, and `libvulkan-dev`, plus a C/C++ compiler and `pkg-config`.
Runtime use also requires a display and compatible graphics drivers.
GPUI is Zed's in-tree GPUI (`github.com/zed-industries/zed`), pinned by
revision in the workspace `Cargo.toml`, with `gpui_platform` choosing Wayland
or X11 at run time. A git dependency's `[patch]` section does not reach the
workspace that depends on it, so a workspace building GPUI repeats Zed's
`async-task` and `calloop` patches from mcsapi's `Cargo.toml`, and pins
`calloop` to the patched 0.14.3 with
`cargo update -p calloop@0.14.5 --precise 0.14.3` when the resolver picks a
newer release from crates.io.

`Toolkit::initialize(capabilities, initialize_gpui)` prefers GPUI only if the
feature is compiled, the host has verified hardware acceleration and required
drivers, and the callback succeeds. Otherwise it supplies an egui context and a
`FallbackReason`, retaining initialization error text. Default/unprobed
capabilities conservatively select egui. Device files and environment variables
alone are not capability probes.

The callback returns the host's initialized GPUI handle, not a placeholder
success value. Run it inside the host's application context and include fallible
window/renderer setup. Panics are not caught; GPUI's `gpui_platform::application()` is not
itself a fallible capability probe. The library does not open a second event
loop or automatically migrate an already-running UI after a driver failure.

- `widgets::gpui_workspace_bar` produces GPUI elements and sends requested IDs
  to a host callback.
- `widgets::egui_workspace_bar` returns an optional workspace ID; apply it with
  `Desktop::switch_to`.
- `widgets::Theme` supplies the shared colors. Selection has a text label, not
  only a color change. egui uses native button focus/activation. GPUI supports
  focus and Enter/Space activation; host-level focus traversal remains the
  host's responsibility. `mcsapi-compositor` publishes egui chrome and apps
  over AT-SPI; a GPUI program out of process registers its own nodes with the
  shell (derisk's `register_tree`).

Visual inspiration: [React Bits](https://reactbits.dev) (card emphasis),
[Aceternity UI](https://ui.aceternity.com) (workspace-like tabs), and
[shadcn/ui](https://ui.shadcn.com) (restrained composable controls). These are
original native widgets, not copied React implementations. No decorative
animations, pointer-following effects, or continuously ticking transitions are
introduced.

**egui is not a software renderer.** A real fallback host must feed `RawInput`
into `Context::run_ui`, paint/tessellate shapes, handle texture upload/free,
process platform output, and schedule requested repaints. GL/wgpu painters still
need functioning drivers; a machine with no usable graphics backend needs a
separately supplied software painter or a headless control path. The example
intentionally clears texture deltas because it does not present a window.

## Theming

`mcsapi-theme` describes how the desktop looks, once, for every toolkit:
a light or dark scheme, six colors (background, surface, foreground,
border, accent, destructive), fonts, icon and cursor themes, and a corner
radius. egui components (`Tokens::from_spec`), GPUI components (the same
name in `mcsapi-components-gpui`), `x2mcsapi --theme` for web, GTK and Qt,
and the exports in `mcsapi_theme::export` all derive from it, so changing
the theme changes every app.

Themes are small TOML files, `<id>.theme`, and may inherit from another:

```toml
name = "Dusk"
inherits = "derisk-dark"

[colors]
accent = "sky"        # a built-in accent name, or "#38bdf8"

[shape]
radius = 8
```

`Library::xdg("derisk")` finds them under `derisk/themes` in the XDG data
directories, user first; `derisk-dark`, `derisk-light` and
`derisk-high-contrast` are built in and can be shadowed. Accents are kept
at 3:1 contrast against the background (WCAG 1.4.11), darkened or
lightened only as far as needed. Unknown keys load with a warning, so
themes written for newer versions still work.

## Touchpad gestures

`mcsapi_ui::gesture` gives apps pan, pinch and rotate that follow the fingers
1:1: the content point under the fingers stays under them, and when they lift
mid-motion the content coasts and slows down. Resting fingers on the touchpad
catch it again.

```rust
use mcsapi_ui::gesture::GestureTracker;

// In the app's state:
let mut view = GestureTracker::default();
// In `App::ui`, once per frame:
view.update(ui, ui.max_rect());
let t = view.transform(); // draw content through t.apply(point)
```

`GestureSettings` tunes friction, the scale range, momentum and rotation.
Hosts that see raw touchpad events (libinput) call `GestureTracker::handle`
directly, or translate them for egui apps with `EguiBridge`.

`mcsapi-compositor` routes gestures from begin to end: `Shell::gesture` may
take a gesture at its begin (three-finger workspace swipes, say); otherwise it
goes to the chrome, the in-process app under the pointer, or the Wayland
client under it through `zwp_pointer_gestures_v1`. Finger scrolling reaches
in-process apps with start and end phases, so two-finger panning gets momentum
too. Under the nested winit backend only two-finger scrolling arrives (winit
drops pinch and swipe events on Linux), and its end is inferred after 50 ms
without motion; a libinput backend gets all of them through the same path.

## TypeScript bindings

[`bindings/node`](bindings/node) is a native Node.js addon (napi-rs) exposing
`Desktop`, the layouts, and typed errors to TypeScript, with generated
`index.d.ts` types. A TypeScript program can then drive workspace, focus, and
layout policy while the compositor host applies the placements. See its README
for build steps and usage.

## MCP server

`crates/mcsapi-mcp` serves mcsapi's policy to AI coding agents (Claude Code,
GitHub Copilot, or any MCP client) over Streamable HTTP:

```sh
cargo run -p mcsapi-mcp   # http://127.0.0.1:8787/mcp; set MCSAPI_MCP_ADDR to change
```

It is stateless: no `Mcp-Session-Id` is issued, nothing is stored between
requests, and `tools/list` or `tools/call` work without a prior `initialize`.
Each call carries everything it needs.

- `simulate` takes workspace IDs, an ordered list of operations (`insert`,
  `remove`, `focus`, `focus_next`, `focus_previous`, `promote_focused`,
  `switch_to`, `move_window`, `set_layout`), and optional output bounds. It
  replays them on a fresh `Desktop` and returns each workspace's windows in
  tiling order, focus, layout, the active workspace, and the placements. A
  failing operation is reported by index with the library's error text.
- `arrange` lays out windows already in tiling order with `tall` or `monocle`.

`.mcp.json` (Claude Code) and `.vscode/mcp.json` (VS Code and Copilot) point at
the default address, so start the server and the tools appear in either.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p mcsapi --example desktop
cargo check --workspace --all-targets --features mcsapi/gpui
cargo test --workspace --features mcsapi/gpui
cargo doc --workspace --no-deps
```

For a nested desktop that hot-reloads while it runs, see
[mcsapi-compositor's README](crates/mcsapi-compositor/README.md#hot-reload-in-a-nested-instance):
`dx serve --hot-patch` patches the running instance on save in debug builds,
keeping its windows and state.

To rerun checks on save, install [bacon](https://dystroy.org/bacon/) once with
`cargo install --locked bacon`, then run `bacon` at the root. It reruns the
current job whenever a workspace crate changes. Jobs from `bacon.toml`:
`check` (default), `clippy` (`c`), `test` (`t`), `doc` (`d`), `example` (`e`),
`mcp` (`m`), which restarts the MCP server, and `nested` (`n`), which rebuilds
and restarts the nested tiling session. Start on one directly with
`bacon test` or `bacon nested`.

### Nix

`flake.nix` has a dev shell with the Rust toolchain, the Wayland, libinput,
GPU and windowing libraries, bacon, cargo-llvm-cov, cargo-audit and Node.js
for `bindings/node`. With [nix-direnv](https://github.com/nix-community/nix-direnv),
`direnv allow` enters it on `cd` (see `.envrc`).

```sh
nix develop                # the dev shell
nix build                  # mcsapi-mcp and x2mcsapi; also .#mcsapi-mcp, .#x2mcsapi
nix flake check            # rustfmt, clippy, tests and docs (default and GPUI), the example
nix fmt                    # nixfmt and rustfmt
nix flake update nixpkgs   # bump one input by name
```

The root is a virtual manifest, so features are named per crate
(`mcsapi/gpui`), or use `-p mcsapi --features gpui`.

The example is headless: it prints placements and generates an egui workspace
bar frame without requiring a GPU or display.

## Releases

Every push to `main` that passes CI is published as a GitHub prerelease,
`nightly-<YYYYMMDD>T<HHMM>` after the commit's time in UTC, with the source as `mcsapi-<version>.tar.gz` and
a `SHA256SUMS`. Every day at 02:17 UTC `release.yml` also cuts a `weekly-`,
`monthly-` or `yearly-` release from the newest nightly when that cadence has
none yet this ISO week, month or year: the same commit and files under the
cadence's tag. Only a yearly release becomes GitHub's latest. These are the
cadences the OS releases on ([Release
cadences](https://github.com/losos-project/desktop/blob/main/docs/releases.md#release-cadences)),
and a tag is the version to pin mcsapi at. Each release is named with an
adjective and a verb that share a letter, from Oxford's word lists, and none
is edited once published (`.github/release`).

`lockfiles.yml` moves every lockfile to what its manifest allows each Monday
at 03:00 UTC, in one commit pushed straight to the default branch, where
CI checks it as any other push.

## API and performance principles

The [Rust API Guidelines checklist](https://rust-lang.github.io/api-guidelines/checklist.html)
informs the typed IDs, private invariants, standard traits, explicit errors,
documented behavior, and borrowed/generic iterator interfaces. Unsafe code is
forbidden and public documentation is checked by the compiler.

The [Rust Performance Book](https://nnethercote.github.io/perf-book/) informs the
allocation-free placement iterator and deliberate separation of policy from
rendering. Owned trees still allocate when membership changes; they are not
claimed to outperform contiguous storage. Toolkit text/layout also allocates.
Desktop operations target small workspace/window counts: membership checks are
tree lookups, while focus traversal and finding a window's workspace visit
state as needed.

Measure representative release-build workloads (focus/workspace switching,
resize bursts, window churn, frame latency, allocations, and idle repainting)
before changing data structures or adding caches, parallelism, custom hashers,
inlining, or unsafe fast paths. Keep logical coordinates distinct from physical
pixels. There is no benchmark-backed speed claim in this scaffold.

Licensed under GPL-3.0-only; see [LICENSE](LICENSE).
