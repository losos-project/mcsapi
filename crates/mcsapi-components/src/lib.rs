//! shadcn/ui-style components for mcsapi apps, as native egui widgets.
//!
//! Every component reads its colors from [`Tokens`], which derive shadcn's
//! semantic roles (`primary`, `muted`, `destructive`, ...) from the shell
//! [`Theme`](mcsapi_ui::Theme). Install the tokens once per frame, then add
//! components like any egui widget:
//!
//! ```
//! use mcsapi_components::{Badge, Button, ButtonVariant, Card, Switch, Tokens};
//! use mcsapi_ui::{App, Theme, egui};
//!
//! struct Settings {
//!     notifications: bool,
//! }
//!
//! impl App for Settings {
//!     fn title(&self) -> &str {
//!         "Settings"
//!     }
//!
//!     fn ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
//!         Tokens::from_theme(theme).install(ui.ctx());
//!         Card::new().title("Notifications").show(ui, |ui| {
//!             ui.add(Switch::new(&mut self.notifications).label("Push notifications"));
//!             ui.horizontal(|ui| {
//!                 ui.add(Button::new("Save"));
//!                 ui.add(Button::new("Cancel").variant(ButtonVariant::Outline));
//!                 ui.add(Badge::new("Beta"));
//!             });
//!         });
//!     }
//! }
//!
//! let mut app = Settings { notifications: true };
//! let context = egui::Context::default();
//! let mut output = mcsapi_ui::run_frame(&mut app, &context, Default::default(), &Theme::default());
//! output.textures_delta.clear();
//! ```
//!
//! See the crate README for which shadcn components are ported and how React
//! Bits and Aceternity UI components take precedence where they overlap.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod button;
mod display;
mod error;
mod fonts;
mod form;
mod navigation;
mod overlay;
mod tokens;

pub use button::{
    Badge, BadgeVariant, Button, ButtonSize, ButtonVariant, Kbd, Toggle, ToggleGroup,
};
pub use display::{
    Alert, AlertVariant, AspectRatio, Avatar, Card, Empty, Label, Progress, Separator, Skeleton,
    Spinner, blockquote, typography,
};
pub use error::{ErrorAlert, ErrorAlertResponse, ErrorDialog, ErrorDialogResponse};
pub use fonts::install_fonts;
pub use form::{Checkbox, Input, RadioGroup, Select, Slider, Switch, Textarea};
pub use navigation::{
    Collapsible, Pagination, Table, Tabs, accordion_item, breadcrumb, page_window,
};
pub use overlay::{
    AlertDialog, AlertDialogAction, Dialog, NativeDialog, Toast, Toaster, toast, toasts, tooltip,
};
pub use tokens::Tokens;

/// Draws shadcn's focus ring around `rect` while `response` has keyboard focus.
fn paint_focus_ring(
    ui: &egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    radius: egui::CornerRadius,
) {
    if response.has_focus() {
        let tokens = Tokens::current(ui.ctx());
        ui.painter().rect_stroke(
            rect,
            radius,
            tokens.ring_stroke(),
            egui::StrokeKind::Outside,
        );
    }
}

/// Turns the "did anything change" flag of a composite widget into
/// [`egui::Response::changed`].
trait IntoChanged {
    fn into_changed(self) -> egui::Response;
}

impl IntoChanged for egui::InnerResponse<bool> {
    fn into_changed(self) -> egui::Response {
        let mut response = self.response;
        if self.inner {
            response.mark_changed();
        }
        response
    }
}
