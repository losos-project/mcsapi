//! A stateless [MCP](https://modelcontextprotocol.io) server for mcsapi.
//!
//! Every tool call carries all the state it needs: the caller supplies
//! workspace IDs and a list of operations, and the server builds a fresh
//! [`Desktop`], replays the operations, and returns the resulting snapshot.
//! Nothing is kept between calls, so any number of server instances can serve
//! any client without sessions or shared storage.
//!
//! [`simulate`] and [`arrange`] are the plain-Rust core and can be used without
//! the transport. [`McsapiServer`] wraps them as MCP tools, and [`router`]
//! mounts those tools on stateless Streamable HTTP.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::sync::Arc;

use mcsapi::{Desktop, Geometry, Layout, WindowId, WorkspaceId};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Json, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// A tiling layout name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutName {
    /// Successive alternating splits forming a spiral.
    Spiral,
    /// One main window on the left, the rest stacked on the right.
    Tall,
    /// Every window fills the output; the host shows only the focused one.
    Monocle,
}

impl From<LayoutName> for Layout {
    fn from(name: LayoutName) -> Self {
        match name {
            LayoutName::Spiral => Self::Spiral,
            LayoutName::Tall => Self::Tall,
            LayoutName::Monocle => Self::Monocle,
        }
    }
}

/// A rectangle in logical compositor coordinates.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width; must be positive.
    pub width: i32,
    /// Height; must be positive.
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

/// One desktop operation, mirroring a [`Desktop`] method.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    /// Adds a new window to the active workspace and focuses it.
    Insert {
        /// Nonzero window ID, unique across the desktop.
        window: u64,
    },
    /// Removes a window from whichever workspace holds it.
    Remove {
        /// Window ID.
        window: u64,
    },
    /// Focuses a window in the active workspace.
    Focus {
        /// Window ID.
        window: u64,
    },
    /// Cycles focus forward in the active workspace, wrapping.
    FocusNext,
    /// Cycles focus backward in the active workspace, wrapping.
    FocusPrevious,
    /// Promotes the focused window of the active workspace to the main pane.
    PromoteFocused,
    /// Activates a workspace.
    SwitchTo {
        /// Workspace ID.
        workspace: u64,
    },
    /// Moves a window to another workspace and focuses it there.
    MoveWindow {
        /// Window ID.
        window: u64,
        /// Target workspace ID.
        workspace: u64,
    },
    /// Sets the active workspace's layout.
    SetLayout {
        /// The new layout.
        layout: LayoutName,
    },
}

/// Input for [`simulate`].
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SimulateRequest {
    /// Nonzero, unique workspace IDs. The first one starts active.
    pub workspaces: Vec<u64>,
    /// Operations applied in order to a fresh desktop.
    #[serde(default)]
    pub operations: Vec<Operation>,
    /// Output bounds; when given, the active workspace's placements are returned.
    #[serde(default)]
    pub bounds: Option<Rect>,
}

/// One workspace in a [`Snapshot`].
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkspaceSnapshot {
    /// Workspace ID.
    pub id: u64,
    /// Layout name, such as `tall` or `monocle`.
    pub layout: String,
    /// Windows in tiling order; the first is the main pane.
    pub windows: Vec<u64>,
    /// Focused window, if any.
    pub focused: Option<u64>,
}

/// A window and its logical geometry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PlacementSnapshot {
    /// Window ID.
    pub window: u64,
    /// Proposed logical geometry.
    pub geometry: Rect,
}

/// Desktop state after [`simulate`].
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Snapshot {
    /// Active workspace ID.
    pub active: u64,
    /// Every workspace, ordered by ID.
    pub workspaces: Vec<WorkspaceSnapshot>,
    /// Placements for the active workspace, present when bounds were given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placements: Option<Vec<PlacementSnapshot>>,
}

/// Input for [`arrange`].
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ArrangeRequest {
    /// Layout to apply.
    pub layout: LayoutName,
    /// Output bounds.
    pub bounds: Rect,
    /// Nonzero window IDs in tiling order; the first is the main pane.
    pub windows: Vec<u64>,
}

/// Output of [`arrange`].
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ArrangeResult {
    /// Placements in tiling order.
    pub placements: Vec<PlacementSnapshot>,
}

fn window(id: u64) -> Result<WindowId, String> {
    WindowId::new(id).ok_or_else(|| "window IDs must be nonzero".to_owned())
}

fn workspace(id: u64) -> Result<WorkspaceId, String> {
    WorkspaceId::new(id).ok_or_else(|| "workspace IDs must be nonzero".to_owned())
}

fn apply(desktop: &mut Desktop, operation: Operation) -> Result<(), String> {
    match operation {
        Operation::Insert { window: id } => desktop.insert(window(id)?),
        Operation::Remove { window: id } => desktop.remove(window(id)?),
        Operation::Focus { window: id } => desktop.focus(window(id)?),
        Operation::FocusNext => {
            desktop.focus_next();
            Ok(())
        }
        Operation::FocusPrevious => {
            desktop.focus_previous();
            Ok(())
        }
        Operation::PromoteFocused => {
            desktop.promote_focused();
            Ok(())
        }
        Operation::SwitchTo { workspace: id } => desktop.switch_to(workspace(id)?),
        Operation::MoveWindow {
            window: w,
            workspace: ws,
        } => desktop.move_window(window(w)?, workspace(ws)?),
        Operation::SetLayout { layout } => {
            desktop.set_layout(layout.into());
            Ok(())
        }
    }
    .map_err(|error| error.to_string())
}

fn placements(placements: impl Iterator<Item = mcsapi::Placement>) -> Vec<PlacementSnapshot> {
    placements
        .map(|placement| PlacementSnapshot {
            window: placement.window.get(),
            geometry: placement.geometry.into(),
        })
        .collect()
}

/// The most workspaces, operations or windows one call may name.
///
/// A call is replayed synchronously on a server worker, and some operations
/// are linear in the number of windows, so an unbounded request (rmcp accepts
/// bodies of 4 MiB, a few hundred thousand operations) would hold a worker for
/// quadratic time. Real desktops are many orders of magnitude below this.
pub const MAX_ITEMS: usize = 10_000;

fn within_limit(what: &str, len: usize) -> Result<(), String> {
    if len > MAX_ITEMS {
        Err(format!("too many {what}: {len}, the limit is {MAX_ITEMS}"))
    } else {
        Ok(())
    }
}

/// Builds a fresh desktop, replays `request.operations`, and returns its state.
///
/// Errors name the failing operation's zero-based index.
pub fn simulate(request: &SimulateRequest) -> Result<Snapshot, String> {
    within_limit("workspaces", request.workspaces.len())?;
    within_limit("operations", request.operations.len())?;
    let ids = request
        .workspaces
        .iter()
        .map(|&id| workspace(id))
        .collect::<Result<Vec<_>, _>>()?;
    let mut desktop = Desktop::new(ids).map_err(|error| error.to_string())?;
    for (index, &operation) in request.operations.iter().enumerate() {
        apply(&mut desktop, operation)
            .map_err(|error| format!("operation {index} ({operation:?}) failed: {error}"))?;
    }
    let placements = request
        .bounds
        .map(|bounds| {
            desktop
                .active()
                .arrange(bounds.into())
                .map(placements)
                .map_err(|error| error.to_string())
        })
        .transpose()?;
    Ok(Snapshot {
        active: desktop.active().id().get(),
        workspaces: desktop
            .workspaces()
            .map(|workspace| WorkspaceSnapshot {
                id: workspace.id().get(),
                layout: format!("{:?}", workspace.layout()).to_lowercase(),
                windows: workspace.windows().map(WindowId::get).collect(),
                focused: workspace.focused().map(WindowId::get),
            })
            .collect(),
        placements,
    })
}

/// Computes placements for windows already in tiling order.
pub fn arrange(request: &ArrangeRequest) -> Result<ArrangeResult, String> {
    within_limit("windows", request.windows.len())?;
    let windows = request
        .windows
        .iter()
        .map(|&id| window(id))
        .collect::<Result<Vec<_>, _>>()?;
    let layout = Layout::from(request.layout);
    Ok(ArrangeResult {
        placements: placements(
            layout
                .arrange(request.bounds.into(), windows)
                .map_err(|error| error.to_string())?,
        ),
    })
}

/// The MCP tool handler. It holds only the static tool table, never desktop state.
#[derive(Clone, Debug)]
pub struct McsapiServer {
    tool_router: ToolRouter<Self>,
}

impl Default for McsapiServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl McsapiServer {
    /// Creates a handler.
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Replays desktop operations on a fresh desktop.
    #[tool(
        description = "Create an mcsapi desktop from workspace IDs, apply operations in order \
                       (insert, remove, focus, focus_next, focus_previous, promote_focused, \
                       switch_to, move_window, set_layout), and return every workspace's \
                       windows in tiling order, focus, layout, the active workspace, and, \
                       when bounds are given, the active workspace's placements. Stateless: \
                       send the full operation history on every call."
    )]
    pub async fn simulate(
        &self,
        Parameters(request): Parameters<SimulateRequest>,
    ) -> Result<Json<Snapshot>, String> {
        simulate(&request).map(Json)
    }

    /// Lays out windows with a single layout.
    #[tool(
        description = "Compute logical window geometry for a spiral, tall or monocle layout. \
                       Windows are in tiling order; the first is the main pane."
    )]
    pub async fn arrange(
        &self,
        Parameters(request): Parameters<ArrangeRequest>,
    ) -> Result<Json<ArrangeResult>, String> {
        arrange(&request).map(Json)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McsapiServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mcsapi", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "mcsapi is xmonad-like tiling policy for Smithay compositors. These tools \
                 are stateless: each call builds a new desktop, so pass all operations \
                 every time. IDs are nonzero integers. Geometry is in logical pixels.",
            )
    }
}

/// Returns an axum router serving the tools at `/mcp` without sessions.
///
/// Every POST is answered on its own with a JSON body, so instances can be
/// restarted or load-balanced freely. Cancelling `shutdown` stops the service.
pub fn router(shutdown: CancellationToken) -> axum::Router {
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false;
    config.json_response = true;
    config.cancellation_token = shutdown;
    let service = StreamableHttpService::new(
        || Ok(McsapiServer::new()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    axum::Router::new().nest_service("/mcp", service)
}
