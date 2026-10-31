//! Node.js bindings for mcsapi, so a TypeScript host can drive desktop policy.
//!
//! IDs cross the boundary as JavaScript numbers. They must be positive safe
//! integers (1 to 2^53 - 1), so every ID round-trips without precision loss.
//! Policy errors are thrown as `Error`s whose `code` names the Rust variant.

#![deny(missing_docs)]

use mcsapi::{Geometry, WindowId, WorkspaceId};
use napi::{Either, Error, bindgen_prelude::Null};

/// A result whose error carries a string `code`, such as `UnknownWindow`.
type Result<T> = std::result::Result<T, Error<String>>;
use napi_derive::napi;

/// The largest integer a JavaScript number represents exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

fn id_value(value: f64, kind: &str) -> Result<u64> {
    if value.fract() == 0.0 && (1.0..=MAX_SAFE_INTEGER).contains(&value) {
        Ok(value as u64)
    } else {
        Err(Error::new(
            "InvalidArg".to_owned(),
            format!("{kind} ID must be an integer from 1 to Number.MAX_SAFE_INTEGER, got {value}"),
        ))
    }
}

fn window_id(value: f64) -> Result<WindowId> {
    id_value(value, "window").map(|v| WindowId::new(v).expect("validated nonzero"))
}

fn workspace_id(value: f64) -> Result<WorkspaceId> {
    id_value(value, "workspace").map(|v| WorkspaceId::new(v).expect("validated nonzero"))
}

fn policy_error(error: mcsapi::Error) -> Error<String> {
    let code = match error {
        mcsapi::Error::NoWorkspaces => "NoWorkspaces",
        mcsapi::Error::DuplicateWorkspace(_) => "DuplicateWorkspace",
        mcsapi::Error::UnknownWorkspace(_) => "UnknownWorkspace",
        mcsapi::Error::DuplicateWindow(_) => "DuplicateWindow",
        mcsapi::Error::UnknownWindow(_) => "UnknownWindow",
        mcsapi::Error::InvalidGeometry => "InvalidGeometry",
        mcsapi::Error::InsufficientSpace => "InsufficientSpace",
        _ => "PolicyError",
    };
    Error::new(code.to_owned(), error.to_string())
}

/// Simple xmonad-like tiling policies.
#[napi(string_enum = "lowercase")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Layout {
    /// Successive alternating splits forming a spiral.
    Spiral,
    /// One main window on the left, with the rest stacked vertically on the right.
    Tall,
    /// Every window fills the output; the host displays only the focused one.
    Monocle,
}

impl From<Layout> for mcsapi::Layout {
    fn from(layout: Layout) -> Self {
        match layout {
            Layout::Spiral => Self::Spiral,
            Layout::Tall => Self::Tall,
            Layout::Monocle => Self::Monocle,
        }
    }
}

impl From<mcsapi::Layout> for Layout {
    fn from(layout: mcsapi::Layout) -> Self {
        match layout {
            mcsapi::Layout::Spiral => Self::Spiral,
            mcsapi::Layout::Monocle => Self::Monocle,
            mcsapi::Layout::Tall => Self::Tall,
        }
    }
}

/// A rectangle in logical compositor coordinates, not physical pixels.
#[napi(object)]
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width; must be positive for output bounds.
    pub width: i32,
    /// Height; must be positive for output bounds.
    pub height: i32,
}

impl From<Rect> for Geometry {
    fn from(rect: Rect) -> Self {
        Geometry::new((rect.x, rect.y).into(), (rect.width, rect.height).into())
    }
}

impl From<Geometry> for Rect {
    fn from(geometry: Geometry) -> Self {
        Self {
            x: geometry.loc.x,
            y: geometry.loc.y,
            width: geometry.size.w,
            height: geometry.size.h,
        }
    }
}

/// A window and its proposed logical geometry.
#[napi(object)]
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    /// The window to configure.
    pub window: i64,
    /// Bounds to apply through the host's configure/rendering path.
    pub geometry: Rect,
}

impl From<mcsapi::Placement> for Placement {
    fn from(placement: mcsapi::Placement) -> Self {
        Self {
            window: placement.window.get() as i64,
            geometry: placement.geometry.into(),
        }
    }
}

/// A snapshot of one workspace. Changing it does not change the desktop.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct WorkspaceState {
    /// Stable workspace identity.
    pub id: i64,
    /// Windows in tiling order: the main window first, then the rest by ID.
    pub windows: Vec<i64>,
    /// The focused window, or `null` for an empty workspace.
    pub focused: Either<i64, Null>,
    /// The workspace's layout.
    pub layout: Layout,
    /// Whether this is the active workspace.
    pub active: bool,
}

fn workspace_state(workspace: &mcsapi::Workspace, active: WorkspaceId) -> WorkspaceState {
    WorkspaceState {
        id: workspace.id().get() as i64,
        windows: workspace.windows().map(|w| w.get() as i64).collect(),
        focused: workspace
            .focused()
            .map_or(Either::B(Null), |w| Either::A(w.get() as i64)),
        layout: workspace.layout().into(),
        active: workspace.id() == active,
    }
}

fn placements(placements: impl Iterator<Item = mcsapi::Placement>) -> Vec<Placement> {
    placements.map(Placement::from).collect()
}

/// Computes placements for windows in the given order, without a desktop.
///
/// Useful for previewing a layout or for hosts that track membership
/// themselves. Throws `InvalidGeometry` or `InsufficientSpace`.
#[napi]
pub fn arrange(layout: Layout, bounds: Rect, windows: Vec<f64>) -> Result<Vec<Placement>> {
    let windows = windows
        .into_iter()
        .map(window_id)
        .collect::<Result<Vec<_>>>()?;
    mcsapi::Layout::from(layout)
        .arrange(bounds.into(), windows)
        .map(placements)
        .map_err(policy_error)
}

/// Desktop policy with fixed workspaces and globally unique windows.
///
/// The host owns surfaces, input, and rendering; it maps window IDs to its own
/// surfaces and calls `remove` when a window is destroyed.
#[napi]
pub struct Desktop {
    inner: mcsapi::Desktop,
}

#[napi]
impl Desktop {
    /// Creates workspaces ordered by ID and activates the first supplied ID.
    #[napi(constructor)]
    pub fn new(workspaces: Vec<f64>) -> Result<Self> {
        let ids = workspaces
            .into_iter()
            .map(workspace_id)
            .collect::<Result<Vec<_>>>()?;
        mcsapi::Desktop::new(ids)
            .map(|inner| Self { inner })
            .map_err(policy_error)
    }

    /// The active workspace's ID.
    #[napi(getter)]
    pub fn active_workspace(&self) -> i64 {
        self.inner.active().id().get() as i64
    }

    /// Returns a snapshot of the active workspace.
    #[napi]
    pub fn active(&self) -> WorkspaceState {
        let active = self.inner.active().id();
        workspace_state(self.inner.active(), active)
    }

    /// Returns snapshots of every workspace in ID order.
    #[napi]
    pub fn workspaces(&self) -> Vec<WorkspaceState> {
        let active = self.inner.active().id();
        self.inner
            .workspaces()
            .map(|workspace| workspace_state(workspace, active))
            .collect()
    }

    /// Returns the workspace containing a window, or `null` if it is unmanaged.
    #[napi]
    pub fn workspace_of(&self, window: f64) -> Result<Option<i64>> {
        let window = window_id(window)?;
        Ok(self
            .inner
            .workspaces()
            .find(|workspace| workspace.windows().any(|w| w == window))
            .map(|workspace| workspace.id().get() as i64))
    }

    /// Adds a new window to the active workspace and focuses it.
    #[napi]
    pub fn insert(&mut self, window: f64) -> Result<()> {
        self.inner.insert(window_id(window)?).map_err(policy_error)
    }

    /// Removes a window from any workspace, repairing focus if necessary.
    #[napi]
    pub fn remove(&mut self, window: f64) -> Result<()> {
        self.inner.remove(window_id(window)?).map_err(policy_error)
    }

    /// Moves a window to another workspace and focuses it there.
    ///
    /// The active workspace is unchanged.
    #[napi]
    pub fn move_window(&mut self, window: f64, workspace: f64) -> Result<()> {
        self.inner
            .move_window(window_id(window)?, workspace_id(workspace)?)
            .map_err(policy_error)
    }

    /// Activates an existing workspace, preserving each workspace's focus.
    #[napi]
    pub fn switch_to(&mut self, workspace: f64) -> Result<()> {
        self.inner
            .switch_to(workspace_id(workspace)?)
            .map_err(policy_error)
    }

    /// Focuses a member of the active workspace.
    #[napi]
    pub fn focus(&mut self, window: f64) -> Result<()> {
        self.inner.focus(window_id(window)?).map_err(policy_error)
    }

    /// Cycles focus forward in the active workspace, wrapping at the end.
    #[napi]
    pub fn focus_next(&mut self) -> Option<i64> {
        self.inner.focus_next().map(|w| w.get() as i64)
    }

    /// Cycles focus backward in the active workspace, wrapping at the beginning.
    #[napi]
    pub fn focus_previous(&mut self) -> Option<i64> {
        self.inner.focus_previous().map(|w| w.get() as i64)
    }

    /// Promotes the focused window in the active workspace to the main pane.
    #[napi]
    pub fn promote_focused(&mut self) {
        self.inner.promote_focused();
    }

    /// Changes the active workspace's layout.
    #[napi]
    pub fn set_layout(&mut self, layout: Layout) {
        self.inner.set_layout(layout.into());
    }

    /// Computes placements for the active workspace within the output bounds.
    ///
    /// Throws `InvalidGeometry` or `InsufficientSpace`.
    #[napi]
    pub fn arrange(&self, bounds: Rect) -> Result<Vec<Placement>> {
        self.inner
            .active()
            .arrange(bounds.into())
            .map(placements)
            .map_err(policy_error)
    }
}
