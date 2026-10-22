//! A gallery of every widget mcsapi apps can use, with its variants and states.
//!
//! [`Gallery`] is an ordinary [`mcsapi_ui::App`]: a sidebar lists each
//! [`Specimen`] by [`Category`], and the main area draws the selected one (or
//! all of them) live, so every control can be clicked, typed into, and themed.
//! It covers the shell widgets in `mcsapi::widgets` and every component in
//! `mcsapi-components`.
//!
//! ```
//! use mcsapi_gallery::Gallery;
//! use mcsapi_ui::{Theme, egui};
//!
//! let mut gallery = Gallery::default();
//! let context = egui::Context::default();
//! let mut output = mcsapi_ui::run_frame(&mut gallery, &context, Default::default(), &Theme::default());
//! assert!(!output.shapes.is_empty());
//! output.textures_delta.clear();
//! ```
//!
//! To add a component, write a `fn(&mut Ui, &mut State)` that draws it in each
//! variant and state, and list it in the `SPECIMENS` slice of the matching
//! module under `src/specimens/`. The crate's tests fail while a component
//! exported by `mcsapi-components` has no specimen.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(feature = "gpui")]
pub mod gpui_gallery;
mod specimens;

use egui::{Color32, RichText, Ui};
use mcsapi_components::{Input, Select, Toaster, Tokens};
use mcsapi_ui::{App, Theme};

pub use specimens::State;

/// The error the Error Alert specimens show: a diagnostic with a code, help
/// and a documentation section, under the sentence an app would put first.
pub(crate) fn sample_error() -> mcsapi_ui::Error {
    let diagnostic = mcsapi_ui::error::miette::MietteDiagnostic::new("no theme named pastel")
        .with_code("mcsapi_theme::not_found")
        .with_help("Pick one of the themes Settings lists, or install pastel.toml.")
        .with_url("docs:icon-theme");
    mcsapi_ui::Error::new(diagnostic).context("Could not apply the theme")
}

/// Where the specimens' "Learn more" goes: the documentation the desktop
/// names, so the button shows only on a desktop that has some.
pub(crate) fn sample_docs() -> mcsapi_ui::Docs {
    mcsapi_ui::Docs::from_env()
}

/// The group a [`Specimen`] is listed under.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum Category {
    /// Buttons, badges, toggles, and keys.
    Actions,
    /// Text inputs and selection controls.
    Forms,
    /// Cards, alerts, avatars, typography, and loading states.
    Display,
    /// Tabs, breadcrumbs, pagination, disclosure, and tables.
    Navigation,
    /// Dialogs, tooltips, and toasts.
    Overlays,
    /// Widgets the desktop shell itself draws.
    Shell,
}

impl Category {
    /// Every category, in sidebar order.
    pub const ALL: [Self; 6] = [
        Self::Actions,
        Self::Forms,
        Self::Display,
        Self::Navigation,
        Self::Overlays,
        Self::Shell,
    ];

    /// Display name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Actions => "Actions",
            Self::Forms => "Forms",
            Self::Display => "Display",
            Self::Navigation => "Navigation",
            Self::Overlays => "Overlays",
            Self::Shell => "Shell",
        }
    }
}

/// One widget in the gallery.
#[derive(Clone, Copy, Debug)]
pub struct Specimen {
    /// Display name, for example `"Button"`.
    pub name: &'static str,
    /// Sidebar group.
    pub category: Category,
    /// Where the design comes from, for example `"shadcn/ui"`.
    pub source: &'static str,
    /// One sentence on what the widget is for.
    pub summary: &'static str,
    /// The public Rust items this specimen demonstrates.
    pub api: &'static [&'static str],
    /// Draws the widget in each of its variants and states.
    pub show: fn(&mut Ui, &mut State),
}

impl Specimen {
    pub(crate) fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.name.to_lowercase().contains(&query)
            || self.category.name().to_lowercase().contains(&query)
            || self
                .api
                .iter()
                .any(|item| item.to_lowercase().contains(&query))
    }
}

/// Every specimen, grouped by category in sidebar order.
pub fn specimens() -> impl Iterator<Item = &'static Specimen> {
    let mut all: Vec<&'static Specimen> = specimens::ALL.iter().flat_map(|s| s.iter()).collect();
    all.sort_by_key(|specimen| specimen.category);
    all.into_iter()
}

/// The specimen called `name`, ignoring case.
pub fn find(name: &str) -> Option<&'static Specimen> {
    specimens().find(|specimen| specimen.name.eq_ignore_ascii_case(name))
}

/// A theme the gallery can switch to, to check widgets against more than the
/// desktop's colors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    /// Display name.
    pub name: &'static str,
    /// Its colors, or `None` for the theme the host passes in.
    pub theme: Option<Theme>,
}

/// The theme presets, starting with the host's.
pub const PRESETS: [Preset; 3] = [
    Preset {
        name: "Desktop",
        theme: None,
    },
    Preset {
        name: "Light",
        theme: Some(Theme {
            background: Color32::from_rgb(255, 255, 255),
            surface: Color32::from_rgb(241, 245, 249),
            foreground: Color32::from_rgb(15, 23, 42),
            // Borders are drawn as chosen, so a faint one, like shadcn's.
            border: Color32::from_rgb(226, 232, 240),
            accent: Color32::from_rgb(37, 99, 235),
        }),
    },
    Preset {
        name: "Violet",
        theme: Some(Theme {
            background: Color32::from_rgb(24, 16, 38),
            surface: Color32::from_rgb(44, 33, 66),
            foreground: Color32::from_rgb(245, 243, 255),
            border: Color32::from_rgb(61, 47, 90),
            accent: Color32::from_rgb(196, 181, 253),
        }),
    },
];

/// The gallery app.
#[derive(Default)]
pub struct Gallery {
    state: State,
    selected: Option<&'static str>,
    query: String,
    preset: Option<usize>,
}

impl Gallery {
    /// Shows only the specimen called `name`, or every specimen for `None`.
    ///
    /// Returns `false`, changing nothing, if no specimen has that name.
    pub fn select(&mut self, name: Option<&str>) -> bool {
        match name {
            None => self.selected = None,
            Some(name) => match find(name) {
                Some(specimen) => self.selected = Some(specimen.name),
                None => return false,
            },
        }
        true
    }

    /// Filters the sidebar and the all-widgets view by name, category, or API
    /// item, as if `query` were typed into the search field.
    pub fn search(&mut self, query: impl Into<String>) {
        self.query = query.into();
    }

    /// Switches to the theme preset called `name`, ignoring case.
    ///
    /// Returns `false`, changing nothing, if no preset has that name.
    pub fn set_preset(&mut self, name: &str) -> bool {
        match PRESETS
            .iter()
            .position(|preset| preset.name.eq_ignore_ascii_case(name))
        {
            Some(index) => {
                self.preset = Some(index);
                true
            }
            None => false,
        }
    }

    fn theme(&self, host: &Theme) -> Theme {
        self.preset
            .and_then(|index| PRESETS[index].theme)
            .unwrap_or(*host)
    }

    fn sidebar(&mut self, ui: &mut Ui, tokens: &Tokens) {
        ui.add(
            Input::new(&mut self.query)
                .placeholder("Search widgets")
                .width(ui.available_width()),
        );
        ui.add_space(8.0);
        let all = self.selected.is_none();
        if nav_button(ui, tokens, "All widgets", all).clicked() {
            self.selected = None;
        }
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                for category in Category::ALL {
                    let mut items = specimens()
                        .filter(|s| s.category == category && s.matches(&self.query))
                        .peekable();
                    if items.peek().is_none() {
                        continue;
                    }
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(category.name())
                            .font(tokens.small_font())
                            .color(tokens.muted_foreground),
                    );
                    for specimen in items {
                        let selected = self.selected == Some(specimen.name);
                        if nav_button(ui, tokens, specimen.name, selected).clicked() {
                            self.selected = Some(specimen.name);
                        }
                    }
                }
            });
    }

    fn header(&mut self, ui: &mut Ui, tokens: &Tokens) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("mcsapi widget gallery")
                    .size(16.0)
                    .strong()
                    .color(tokens.foreground),
            );
            // A chip, like the web interface's "this box" beside its logo.
            ui.add(
                mcsapi_components::Badge::new(format!("{} widgets", specimens().count()))
                    .variant(mcsapi_components::BadgeVariant::Outline),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Right to left: the select sits at the edge, its caption before it.
                let names = PRESETS.map(|preset| preset.name);
                let mut preset = Some(self.preset.unwrap_or(0));
                ui.add(Select::new("mcsapi-gallery-theme", &mut preset, &names).width(140.0));
                self.preset = preset.filter(|&index| index != 0);
                ui.label(
                    RichText::new("Theme")
                        .font(tokens.body_font())
                        .color(tokens.muted_foreground),
                );
            });
        });
    }

    fn content(&mut self, ui: &mut Ui, tokens: &Tokens) {
        let shown: Vec<&'static Specimen> = match self.selected.and_then(find) {
            Some(specimen) => vec![specimen],
            None => specimens().filter(|s| s.matches(&self.query)).collect(),
        };
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                if shown.is_empty() {
                    ui.add_space(24.0);
                    mcsapi_components::Empty::new("No widgets match")
                        .description("Try another name, or clear the search.")
                        .show(ui, |_| ());
                }
                for specimen in shown {
                    specimen_frame(ui, tokens, specimen, &mut self.state);
                    ui.add_space(16.0);
                }
            });
    }
}

impl App for Gallery {
    fn title(&self) -> &str {
        "Widget gallery"
    }

    fn ui(&mut self, ui: &mut Ui, host: &Theme) {
        let theme = self.theme(host);
        self.state.theme = theme;
        let tokens = Tokens::from_theme(&theme);
        tokens.install(ui.ctx());
        // In the default theme's letters, where they are installed, as the
        // desktop draws its egui apps.
        mcsapi_components::install_fonts(ui.ctx(), &mcsapi_ui::theme::Fonts::default());
        ui.ctx().set_visuals(visuals(&theme));

        // Laid out like the LosOS web interface: a top bar over the page, a
        // sunken, bordered sidebar, and the specimens as cards beside it.
        egui::Frame::new()
            .fill(tokens.field)
            .inner_margin(egui::Margin::symmetric(16, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.header(ui, &tokens);
            });
        let bar = ui.min_rect();
        ui.painter()
            .hline(bar.x_range(), bar.bottom(), tokens.border_stroke());
        egui::Frame::new()
            .fill(theme.background)
            .inner_margin(16)
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                egui::Panel::left("mcsapi-gallery-sidebar")
                    .resizable(false)
                    .show_separator_line(false)
                    .exact_size(220.0)
                    .frame(egui::Frame::new().inner_margin(egui::Margin {
                        right: 24,
                        ..Default::default()
                    }))
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .fill(tokens.field)
                            .stroke(tokens.border_stroke())
                            .corner_radius(tokens.card_radius())
                            .inner_margin(8)
                            .show(ui, |ui| {
                                ui.set_min_height(ui.available_height());
                                self.sidebar(ui, &tokens)
                            });
                    });
                egui::CentralPanel::default()
                    .frame(egui::Frame::new())
                    .show(ui, |ui| self.content(ui, &tokens));
            });
        Toaster::show(ui.ctx());
    }
}

/// A sidebar entry: the selected one is a solid accent pill with a
/// chevron, as in the web interface's navigation tree.
fn nav_button(ui: &mut Ui, tokens: &Tokens, text: &str, selected: bool) -> egui::Response {
    let size = egui::vec2(ui.available_width(), 32.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, text));
    let painter = ui.painter();
    let (fill, color) = if selected {
        (tokens.primary, tokens.primary_foreground)
    } else if response.hovered() {
        (tokens.hover, tokens.foreground)
    } else {
        (Color32::TRANSPARENT, tokens.foreground)
    };
    painter.rect_filled(rect, tokens.control_radius(), fill);
    let inner = rect.shrink2(egui::vec2(10.0, 0.0));
    painter.text(
        inner.left_center(),
        egui::Align2::LEFT_CENTER,
        text,
        tokens.body_font(),
        color,
    );
    if selected {
        painter.text(
            inner.right_center(),
            egui::Align2::RIGHT_CENTER,
            "›",
            tokens.body_font(),
            color,
        );
    }
    response
}

fn specimen_frame(ui: &mut Ui, tokens: &Tokens, specimen: &Specimen, state: &mut State) {
    egui::Frame::new()
        .fill(tokens.card)
        .stroke(tokens.border_stroke())
        .corner_radius(tokens.card_radius())
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(specimen.name)
                        .size(18.0)
                        .strong()
                        .color(tokens.foreground),
                );
                ui.add(
                    mcsapi_components::Badge::new(specimen.source)
                        .variant(mcsapi_components::BadgeVariant::Outline),
                );
            });
            ui.label(
                RichText::new(specimen.summary)
                    .font(tokens.body_font())
                    .color(tokens.muted_foreground),
            );
            ui.horizontal_wrapped(|ui| {
                for item in specimen.api {
                    ui.label(mcsapi_components::typography::inline_code(tokens, *item));
                }
            });
            ui.add_space(12.0);
            ui.push_id(specimen.name, |ui| (specimen.show)(ui, state));
        });
}

/// egui visuals matching `theme`, for the plain egui widgets around the
/// components (scroll bars, popups, text selection).
pub fn visuals(theme: &Theme) -> egui::Visuals {
    let [r, g, b, _] = theme.background.to_array();
    let dark = u16::from(r) + u16::from(g) + u16::from(b) < 384;
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.override_text_color = Some(theme.foreground);
    visuals.panel_fill = theme.background;
    visuals.window_fill = theme.background;
    visuals.extreme_bg_color = theme.surface;
    visuals.faint_bg_color = theme.surface;
    visuals.selection.bg_fill = theme.accent.gamma_multiply(0.45);
    visuals.selection.stroke.color = theme.accent;
    visuals.hyperlink_color = theme.accent;
    visuals.widgets.noninteractive.bg_stroke.color = theme.border;
    visuals
}
