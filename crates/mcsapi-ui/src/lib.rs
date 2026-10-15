//! UI toolkit for apps that run on an mcsapi desktop.
//!
//! Apps implement [`App`] and draw with egui, styled by the same [`Theme`] as
//! the desktop shell. The toolkit renders one frame at a time and owns no event
//! loop, window, or painter: the host (usually `mcsapi-runtime`) feeds input,
//! paints the returned output, and schedules repaints.
//!
//! [`gesture`] adds 1:1 touchpad gestures: pan, pinch and rotate content that
//! follows the fingers exactly and coasts when they lift.
//!
//! [`error`] is the [`Result`] apps report failures with: a miette report
//! that knows which section of the desktop's documentation explains it, for
//! `mcsapi-components`' `ErrorAlert` to draw with a "Learn more" button.
//!
//! [`click`] keeps egui's clicks on the input's clock when frames are slow.
//!
//! [`dialog`] describes a dialog apart from its toolkit: the window it is
//! modal to, as a portal names it, and the actions in its button row.
//!
//! ```
//! use mcsapi_ui::{App, Theme, egui};
//!
//! struct Hello;
//!
//! impl App for Hello {
//!     fn title(&self) -> &str {
//!         "Hello"
//!     }
//!
//!     fn ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
//!         ui.label(egui::RichText::new("Hello").color(theme.foreground));
//!     }
//! }
//!
//! let context = egui::Context::default();
//! let mut output = mcsapi_ui::run_frame(&mut Hello, &context, Default::default(), &Theme::default());
//! assert!(!output.shapes.is_empty());
//! // No painter here; a graphical host uploads and frees these textures.
//! output.textures_delta.clear();
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod click;
pub mod dialog;
pub mod error;
pub mod gesture;

pub use click::ClickTiming;
pub use egui;
pub use error::{Context, DocLink, Docs, Error, Result};
pub use gesture::{GestureEvent, GestureTracker, Transform};
pub use mcsapi::theme;
pub use mcsapi::toolkit::{FallbackReason, GraphicsCapabilities, Toolkit};
pub use mcsapi::widgets::Theme;

/// An application drawn with the toolkit.
pub trait App {
    /// Human-readable title, for example for a window decoration or task list.
    fn title(&self) -> &str;

    /// Draws one frame of the app's content.
    fn ui(&mut self, ui: &mut egui::Ui, theme: &Theme);
}

/// Runs one egui frame of `app` inside a surface filled with the theme background.
///
/// The caller paints the returned shapes, applies texture deltas, handles
/// platform output, and honors requested repaints. egui panics in debug builds
/// if texture deltas are dropped unapplied; call `textures_delta.clear()` when
/// discarding them on purpose.
pub fn run_frame(
    app: &mut (impl App + ?Sized),
    context: &egui::Context,
    input: egui::RawInput,
    theme: &Theme,
) -> egui::FullOutput {
    context.run_ui(input, |ui| {
        egui::Frame::new()
            .fill(theme.background)
            .inner_margin(8)
            .show(ui, |ui| app.ui(ui, theme));
        paint_focus_ring(ui.ctx(), theme.accent);
    })
}

/// Outlines the focused widget while the keyboard (or a screen reader) is
/// moving focus, so Tab navigation shows where it is. egui only shades a
/// focused widget like a pressed one, which is easy to miss. Pointer input
/// hides the ring again. Call at the end of a frame.
pub fn paint_focus_ring(ctx: &egui::Context, color: egui::Color32) {
    let keyboard_id = egui::Id::new("mcsapi-focus-ring-keyboard");
    let latest = ctx.input(|i| {
        i.events.iter().rev().find_map(|e| match e {
            egui::Event::Key { pressed: true, .. } | egui::Event::AccessKitActionRequest(_) => {
                Some(true)
            }
            egui::Event::PointerButton { .. } => Some(false),
            _ => None,
        })
    });
    let keyboard = match latest {
        Some(keyboard) => {
            ctx.data_mut(|d| d.insert_temp(keyboard_id, keyboard));
            keyboard
        }
        None => ctx.data(|d| d.get_temp(keyboard_id)).unwrap_or(false),
    };
    let Some(rect) = keyboard
        .then(|| ctx.memory(|m| m.focused()))
        .flatten()
        .and_then(|id| ctx.read_response(id))
        .map(|r| r.rect)
    else {
        return;
    };
    ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("mcsapi-focus-ring"),
    ))
    .rect_stroke(
        rect.expand(2.0),
        4,
        egui::Stroke::new(2.5, color),
        egui::StrokeKind::Outside,
    );
}
