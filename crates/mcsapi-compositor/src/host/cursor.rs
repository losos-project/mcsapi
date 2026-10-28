//! The pointer's image: what the client under it asked for (a surface of
//! its own, a named shape through `wp_cursor_shape_manager_v1`, or none),
//! or over the chrome and in-process apps the shape egui asked for. Named
//! shapes come from the XCursor theme in `XCURSOR_THEME` at
//! `XCURSOR_SIZE` (Adwaita-style `default` and 24 when unset), animated
//! where the theme animates them; without a theme, or for a shape it lacks,
//! the compositor paints its own arrow.

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    time::Duration,
};

use smithay::backend::input::TabletToolDescriptor;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportMem,
            element::{Kind, surface::render_elements_from_surface_tree},
            gles::{GlesRenderer, GlesTexture},
        },
    },
    delegate_cursor_shape,
    input::{
        Seat,
        pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData},
    },
    reexports::wayland_server::{DisplayHandle, protocol::wl_surface::WlSurface},
    utils::{Logical, Physical, Point, Rectangle, Scale, Size},
    wayland::{
        compositor::with_states, cursor_shape::CursorShapeManagerState,
        tablet_manager::TabletSeatHandler,
    },
};
use tracing::warn;

use super::Host;
use crate::{Shell, egui};

/// A frame ready to draw: its texture, hotspot, size, and which frame of
/// its animation it is.
type Frame = (GlesTexture, Point<i32, Logical>, Size<i32, Logical>, usize);

/// One frame of a themed cursor.
struct Image {
    size: Size<i32, Logical>,
    hotspot: Point<i32, Logical>,
    delay: Duration,
    rgba: Vec<u8>,
    texture: Option<GlesTexture>,
}

pub(super) struct Cursors {
    _shape: CursorShapeManagerState,
    /// What the client under the pointer last asked for.
    pub(super) client: CursorImageStatus,
    /// The shape the chrome or the in-process app under the pointer wants.
    pub(super) egui: CursorIcon,
    theme: xcursor::CursorTheme,
    size: u32,
    /// Loaded shapes; `None` for one the theme lacks.
    images: HashMap<CursorIcon, Option<Vec<Image>>>,
}

/// What [`Host::draw`] puts at the pointer.
pub(super) enum Drawn {
    /// Nothing: the client hid it.
    Hidden,
    /// The compositor's own arrow (see `paint_cursor`).
    Painted,
    /// A client's cursor surface, drawn as its render elements.
    Surface(Vec<WaylandSurfaceRenderElement<GlesRenderer>>),
    /// A themed image, its top-left corner at `at`.
    Image {
        texture: GlesTexture,
        at: Point<i32, Physical>,
        size: Size<i32, Physical>,
        /// The shape and the frame of its animation, so a frame that moves
        /// nothing else can tell whether the image changed.
        id: u64,
    },
}

impl Drawn {
    /// Where on screen it is drawn, if it is drawn over the scene rather
    /// than in it; `None` when hidden or painted by egui.
    pub(super) fn area(&self) -> Option<Rectangle<i32, Physical>> {
        match self {
            Self::Hidden | Self::Painted => None,
            Self::Surface(elements) => {
                use smithay::backend::renderer::element::Element as _;
                elements
                    .iter()
                    .map(|e| e.geometry(Scale::from(1.0)))
                    .reduce(|a, b| a.merge(b))
            }
            Self::Image { at, size, .. } => Some(Rectangle::new(*at, *size)),
        }
    }

    /// What it shows: the same where only its position changed.
    pub(super) fn id(&self) -> u64 {
        match self {
            Self::Hidden => 0,
            Self::Painted => 1,
            Self::Surface(elements) => {
                let mut hasher = DefaultHasher::new();
                for e in elements {
                    use smithay::backend::renderer::element::Element as _;
                    e.id().hash(&mut hasher);
                    format!("{:?}", e.current_commit()).hash(&mut hasher);
                }
                hasher.finish()
            }
            Self::Image { id, .. } => *id,
        }
    }
}

impl Cursors {
    pub(super) fn new<S: Shell + 'static>(dh: &DisplayHandle) -> Self {
        let theme = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&s: &u32| s > 0)
            .unwrap_or(24);
        Self {
            _shape: CursorShapeManagerState::new::<Host<S>>(dh),
            client: CursorImageStatus::default_named(),
            egui: CursorIcon::Default,
            theme: xcursor::CursorTheme::load(&theme),
            size,
            images: HashMap::new(),
        }
    }

    /// The client's cursor surface, if it set one, for frame callbacks.
    pub(super) fn surface(&self) -> Option<&WlSurface> {
        match &self.client {
            CursorImageStatus::Surface(surface) => Some(surface),
            _ => None,
        }
    }

    /// Loads `icon` from the theme, under its CSS name or an older X11
    /// alias.
    fn load(&mut self, icon: CursorIcon) -> Option<&mut Vec<Image>> {
        let size = self.size;
        let theme = &self.theme;
        self.images
            .entry(icon)
            .or_insert_with(|| {
                let path = std::iter::once(icon.name())
                    .chain(icon.alt_names().iter().copied())
                    .find_map(|name| theme.load_icon(name))?;
                let bytes = std::fs::read(&path)
                    .map_err(|e| warn!(?path, error = %e, "cannot read cursor"))
                    .ok()?;
                let images = xcursor::parser::parse_xcursor(&bytes)?;
                // The nominal size closest to the one asked for, with every
                // frame of its animation.
                let nominal = images
                    .iter()
                    .map(|i| i.size)
                    .min_by_key(|&s| s.abs_diff(size))?;
                let frames: Vec<Image> = images
                    .into_iter()
                    .filter(|i| i.size == nominal)
                    .map(|i| Image {
                        size: (i.width as i32, i.height as i32).into(),
                        hotspot: (i.xhot as i32, i.yhot as i32).into(),
                        delay: Duration::from_millis(u64::from(i.delay)),
                        rgba: i.pixels_rgba,
                        texture: None,
                    })
                    .collect();
                (!frames.is_empty()).then_some(frames)
            })
            .as_mut()
    }

    /// The themed frame of `icon` to show at `elapsed`, as a texture.
    fn image(
        &mut self,
        renderer: &mut GlesRenderer,
        icon: CursorIcon,
        elapsed: Duration,
    ) -> Option<Frame> {
        let frames = match self.load(icon) {
            Some(frames) => frames,
            None if icon != CursorIcon::Default => self.load(CursorIcon::Default)?,
            None => return None,
        };
        let total: Duration = frames.iter().map(|f| f.delay).sum();
        let index = if total.is_zero() || frames.len() == 1 {
            0
        } else {
            let mut t = Duration::from_nanos((elapsed.as_nanos() % total.as_nanos()) as u64);
            frames
                .iter()
                .position(|f| {
                    let here = t < f.delay;
                    t = t.saturating_sub(f.delay);
                    here
                })
                .unwrap_or(0)
        };
        let frame = &mut frames[index];
        if frame.texture.is_none() {
            // RGBA bytes are ABGR8888 in DRM's little-endian naming.
            match renderer.import_memory(
                &frame.rgba,
                Fourcc::Abgr8888,
                frame.size.to_buffer(1, Default::default()),
                false,
            ) {
                Ok(texture) => frame.texture = Some(texture),
                Err(e) => {
                    warn!(error = %e, "cannot upload cursor");
                    return None;
                }
            }
        }
        Some((frame.texture.clone()?, frame.hotspot, frame.size, index))
    }

    /// Decides what to draw at `pointer`: the client's wish while a client
    /// has the pointer, else egui's.
    pub(super) fn drawn(
        &mut self,
        renderer: &mut GlesRenderer,
        on_client: bool,
        pointer: Point<f64, Logical>,
        elapsed: Duration,
    ) -> Drawn {
        let scale = Scale::from(1.0);
        let icon = if on_client {
            match &self.client {
                CursorImageStatus::Hidden => return Drawn::Hidden,
                CursorImageStatus::Named(icon) => *icon,
                CursorImageStatus::Surface(surface) => {
                    let hotspot = with_states(surface, |states| {
                        states
                            .data_map
                            .get::<CursorImageSurfaceData>()
                            .and_then(|d| d.lock().ok().map(|d| d.hotspot))
                            .unwrap_or_default()
                    });
                    let at = (pointer.to_i32_round() - hotspot).to_physical(1);
                    return Drawn::Surface(render_elements_from_surface_tree(
                        renderer,
                        surface,
                        at,
                        scale,
                        1.0,
                        Kind::Cursor,
                    ));
                }
            }
        } else {
            self.egui
        };
        match self.image(renderer, icon, elapsed) {
            Some((texture, hotspot, size, index)) => Drawn::Image {
                texture,
                at: (pointer.to_i32_round() - hotspot).to_physical(1),
                size: size.to_physical(1),
                id: {
                    let mut hasher = DefaultHasher::new();
                    (icon, index).hash(&mut hasher);
                    hasher.finish()
                },
            },
            None => Drawn::Painted,
        }
    }
}

/// The cursor shape egui asked for, in the names cursor themes use.
pub(super) fn from_egui(icon: egui::CursorIcon) -> CursorIcon {
    use egui::CursorIcon as E;
    match icon {
        E::Default | E::None => CursorIcon::Default,
        E::ContextMenu => CursorIcon::ContextMenu,
        E::Help => CursorIcon::Help,
        E::PointingHand => CursorIcon::Pointer,
        E::Progress => CursorIcon::Progress,
        E::Wait => CursorIcon::Wait,
        E::Cell => CursorIcon::Cell,
        E::Crosshair => CursorIcon::Crosshair,
        E::Text => CursorIcon::Text,
        E::VerticalText => CursorIcon::VerticalText,
        E::Alias => CursorIcon::Alias,
        E::Copy => CursorIcon::Copy,
        E::Move => CursorIcon::Move,
        E::NoDrop => CursorIcon::NoDrop,
        E::NotAllowed => CursorIcon::NotAllowed,
        E::Grab => CursorIcon::Grab,
        E::Grabbing => CursorIcon::Grabbing,
        E::AllScroll => CursorIcon::AllScroll,
        E::ResizeHorizontal => CursorIcon::EwResize,
        E::ResizeNeSw => CursorIcon::NeswResize,
        E::ResizeNwSe => CursorIcon::NwseResize,
        E::ResizeVertical => CursorIcon::NsResize,
        E::ResizeEast => CursorIcon::EResize,
        E::ResizeSouthEast => CursorIcon::SeResize,
        E::ResizeSouth => CursorIcon::SResize,
        E::ResizeSouthWest => CursorIcon::SwResize,
        E::ResizeWest => CursorIcon::WResize,
        E::ResizeNorthWest => CursorIcon::NwResize,
        E::ResizeNorth => CursorIcon::NResize,
        E::ResizeNorthEast => CursorIcon::NeResize,
        E::ResizeColumn => CursorIcon::ColResize,
        E::ResizeRow => CursorIcon::RowResize,
        E::ZoomIn => CursorIcon::ZoomIn,
        E::ZoomOut => CursorIcon::ZoomOut,
    }
}

impl<S: Shell + 'static> Host<S> {
    pub(super) fn set_cursor(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursors.client = image;
    }
}

/// A pen over a client may carry its own image, as the pointer does.
impl<S: Shell + 'static> TabletSeatHandler for Host<S> {
    fn tablet_tool_image(&mut self, _tool: &TabletToolDescriptor, image: CursorImageStatus) {
        self.cursors.client = image;
    }
}

delegate_cursor_shape!(@<S: Shell + 'static> Host<S>);
