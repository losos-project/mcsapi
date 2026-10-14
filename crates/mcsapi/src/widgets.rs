//! Small native shell widgets with shared, restrained visual tokens.
//!
//! Inspired by React Bits' card emphasis, Aceternity's tabs, and shadcn/ui's
//! compact controls, not their implementations. No decorative motion is used.

#[cfg(not(target_family = "wasm"))]
use crate::{Desktop, WorkspaceId};
use egui::Color32;
#[cfg(not(target_family = "wasm"))]
use egui::{Button, RichText, Stroke};

/// Shared opaque colors for the egui and GPUI workspace bars.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    /// Background of the bar.
    pub background: Color32,
    /// Background of an inactive workspace control.
    pub surface: Color32,
    /// Text color.
    pub foreground: Color32,
    /// Border color for inactive controls.
    pub border: Color32,
    /// Border color for the active workspace.
    pub accent: Color32,
}

impl Default for Theme {
    /// The shell colors of the default theme, `derisk-dark`.
    fn default() -> Self {
        Self::from(&crate::theme::Theme::dark())
    }
}

fn color32(color: crate::theme::Color) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a)
}

impl From<&crate::theme::Theme> for Theme {
    /// The shell colors of a full theme from the theming engine. Fonts, icons,
    /// the radius and the destructive color are not shell colors; components
    /// read them from the theme itself.
    fn from(theme: &crate::theme::Theme) -> Self {
        let p = &theme.palette;
        Self {
            background: color32(p.background),
            surface: color32(p.surface),
            foreground: color32(p.foreground),
            border: color32(p.border),
            accent: color32(p.accent),
        }
    }
}

impl Theme {
    /// The theming engine's palette for these shell colors, with the default
    /// destructive color.
    pub fn palette(&self) -> crate::theme::Palette {
        let color = |c: Color32| {
            let [r, g, b, a] = c.to_srgba_unmultiplied();
            crate::theme::Color::rgba(r, g, b, a)
        };
        crate::theme::Palette {
            background: color(self.background),
            surface: color(self.surface),
            foreground: color(self.foreground),
            border: color(self.border),
            accent: color(self.accent),
            destructive: crate::theme::Theme::dark().palette.destructive,
        }
    }
}

// The workspace bars draw a Desktop, which a wasm32 build does not have.
#[cfg(not(target_family = "wasm"))]
fn label(id: WorkspaceId, active: bool) -> String {
    if active {
        format!("Workspace {id} · active")
    } else {
        format!("Workspace {id}")
    }
}

/// Draws native egui buttons and returns a requested workspace switch.
///
/// The host applies the returned ID with [`Desktop::switch_to`]. egui supplies
/// button focus and keyboard activation; this is not an ARIA-style tab list.
/// Calling this function does not mutate desktop state or allocate a collection.
#[cfg(not(target_family = "wasm"))]
pub fn egui_workspace_bar(
    ui: &mut egui::Ui,
    desktop: &Desktop,
    theme: Theme,
) -> Option<WorkspaceId> {
    egui::Frame::new()
        .fill(theme.background)
        .inner_margin(8)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let mut requested = None;
                for workspace in desktop.workspaces() {
                    let active = workspace.id() == desktop.active().id();
                    let response = ui.add(
                        Button::new(
                            RichText::new(label(workspace.id(), active)).color(theme.foreground),
                        )
                        .selected(active)
                        .fill(theme.surface)
                        .stroke(Stroke::new(
                            1.0,
                            if active { theme.accent } else { theme.border },
                        ))
                        .corner_radius(6)
                        .min_size(egui::vec2(40.0, 32.0)),
                    );
                    if response.clicked() {
                        requested = Some(workspace.id());
                    }
                }
                requested
            })
            .inner
        })
        .inner
}

/// Builds a GPUI workspace bar directly from an iterator of desktop workspaces.
///
/// The callback should update host state and request a redraw. GPUI controls
/// are focusable and support Enter/Space activation. The host is responsible
/// for application-level focus traversal and accessibility integration.
#[cfg(all(feature = "gpui", not(target_family = "wasm")))]
pub fn gpui_workspace_bar(
    desktop: &Desktop,
    theme: Theme,
    on_select: impl Fn(WorkspaceId, &mut gpui::Window, &mut gpui::App) + Clone + 'static,
) -> impl gpui::IntoElement {
    use gpui::{
        InteractiveElement, ParentElement, StatefulInteractiveElement, Styled, div, px, rgb,
    };

    let color = |value: Color32| {
        rgb(u32::from(value.r()) << 16 | u32::from(value.g()) << 8 | u32::from(value.b()))
    };
    div()
        .flex()
        .flex_wrap()
        .gap(px(6.0))
        .p(px(8.0))
        .bg(color(theme.background))
        .text_color(color(theme.foreground))
        .children(desktop.workspaces().map(|workspace| {
            let id = workspace.id();
            let active = id == desktop.active().id();
            let click = on_select.clone();
            let keyboard = on_select.clone();
            div()
                .id(("workspace", id.get()))
                .focusable()
                .px(px(12.0))
                .py(px(6.0))
                .min_h(px(32.0))
                .rounded(px(6.0))
                .bg(color(theme.surface))
                .border_1()
                .border_color(color(if active { theme.accent } else { theme.border }))
                .focus(|style| style.border_2().border_color(color(theme.foreground)))
                .child(label(id, active))
                .on_click(move |_, window, cx| click(id, window, cx))
                .on_key_down(move |event, window, cx| {
                    if event.keystroke.key == "enter" || event.keystroke.key == "space" {
                        keyboard(id, window, cx);
                        cx.stop_propagation();
                    }
                })
        }))
}
