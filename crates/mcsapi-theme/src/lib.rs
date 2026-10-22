//! The theming engine for mcsapi desktops such as derisk.
//!
//! A [`Theme`] is the one description of how the desktop looks: a light or
//! dark [`Scheme`], five shell colors plus a destructive one ([`Palette`]),
//! [`Fonts`], [`Icons`] and a corner radius. Everything that draws reads it,
//! whatever toolkit it uses:
//!
//! - egui and GPUI components derive their colors from [`Theme::tokens`],
//!   shadcn/ui-style roles such as `primary` and `muted`;
//! - `x2mcsapi` turns it into CSS, GTK and Qt style sheets;
//! - [`export`] turns it into what other consumers already read: the
//!   freedesktop appearance settings (`org.freedesktop.appearance`), GTK's
//!   `settings.ini`, cursor variables, Android resources for the Android
//!   translation layer, and JSON for anything else.
//!
//! This crate depends only on the derives behind its error types
//! (`thiserror`, and `miette` without its terminal renderer), so a process
//! that never draws (a portal backend, a theme bridge) can read themes
//! without linking a toolkit.
//!
//! Themes are plain text files, a small subset of TOML (see [`mod@file`]), and can
//! inherit from another theme so a variant only lists what it changes:
//!
//! ```
//! use mcsapi_theme::{Color, Scheme, Theme};
//!
//! let parsed = Theme::parse(
//!     r##"
//!     name = "Dusk"
//!     inherits = "derisk-dark"
//!
//!     [colors]
//!     accent = "#38bdf8"
//!     "##,
//!     |id| Theme::builtin(id),
//! )
//! .unwrap();
//! let dusk = parsed.theme;
//! assert_eq!(dusk.scheme, Scheme::Dark);
//! assert_eq!(dusk.tokens().primary, Color::rgb(0x38, 0xbd, 0xf8));
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod color;
pub mod export;
pub mod file;

use std::path::{Path, PathBuf};

pub use color::{Color, ParseColorError};
pub use file::{ParseError, Parsed, Warning};

/// Light or dark.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Scheme {
    /// Dark surfaces with light text.
    #[default]
    Dark,
    /// Light surfaces with dark text.
    Light,
}

impl Scheme {
    /// `"dark"` or `"light"`, as written in theme files.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// Parses `"dark"` or `"light"`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }
}

/// The colors a theme chooses; everything else is derived from them.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Palette {
    /// Page and panel background.
    pub background: Color,
    /// Raised surfaces: inactive controls, tracks, secondary buttons.
    pub surface: Color,
    /// Text.
    pub foreground: Color,
    /// Borders of cards, fields and inactive controls, drawn as chosen.
    pub border: Color,
    /// Highlight for active controls, the focused workspace and selections.
    pub accent: Color,
    /// Fill of destructive controls.
    pub destructive: Color,
}

/// Font families and sizes. Sizes are logical pixels.
///
/// Families are names of installed fonts, which GPUI, GTK, Qt and web content
/// resolve through fontconfig. egui draws with the fonts embedded in it unless
/// the app installs the families with mcsapi-components' `install_fonts`.
#[derive(Clone, Debug, PartialEq)]
pub struct Fonts {
    /// Family for interface text.
    pub sans: String,
    /// CSS weight of interface text, for example 300 for light.
    pub sans_weight: u16,
    /// Family for code and terminals.
    pub monospace: String,
    /// Body text size.
    pub size: f32,
    /// Small text size: captions, badges, descriptions.
    pub small_size: f32,
    /// Monospace text size.
    pub monospace_size: f32,
}

impl Default for Fonts {
    /// The LosOS web interface's letters: Inter at its regular weight, which
    /// that interface's screenshots and the machines it is tested on draw
    /// its system font stack with. The monospace family stays egui's Hack,
    /// which the desktop's terminal draws in its Nerd Font build.
    fn default() -> Self {
        Self {
            sans: "Inter".to_owned(),
            sans_weight: 400,
            monospace: "Hack".to_owned(),
            size: 14.0,
            small_size: 12.0,
            monospace_size: 12.0,
        }
    }
}

/// Icon and cursor themes, by their freedesktop theme directory names.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Icons {
    /// Icon theme, a directory under `icons/` in an XDG data directory.
    pub theme: String,
    /// Cursor theme, likewise.
    pub cursor: String,
    /// Cursor size in logical pixels.
    pub cursor_size: u16,
}

impl Default for Icons {
    /// Papirus for icons, the theme derisk is designed around and the one
    /// LosOS ships; its dark variant, since the default theme is dark.
    fn default() -> Self {
        Self {
            theme: "Papirus-Dark".to_owned(),
            cursor: "Adwaita".to_owned(),
            cursor_size: 24,
        }
    }
}

/// A complete theme.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Human-readable name.
    pub name: String,
    /// Light or dark; foreign toolkits pick their own light or dark base
    /// from this.
    pub scheme: Scheme,
    /// The chosen colors.
    pub palette: Palette,
    /// Font families and sizes.
    pub fonts: Fonts,
    /// Icon and cursor themes.
    pub icons: Icons,
    /// Corner radius of controls in logical pixels; cards use two more.
    pub radius: u8,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// Built-in accent colors, by the name settings store them under.
///
/// They are tuned for dark backgrounds; [`Theme::with_accent`] darkens them
/// where a light theme needs it.
pub const ACCENTS: [(&str, Color); 6] = [
    // The LosOS web interface's accent, and the default theme's.
    ("teal", Color::rgb(72, 179, 192)),
    ("lime", Color::rgb(163, 230, 53)),
    ("sky", Color::rgb(56, 189, 248)),
    ("violet", Color::rgb(167, 139, 250)),
    ("rose", Color::rgb(251, 113, 133)),
    ("amber", Color::rgb(251, 191, 36)),
];

/// Looks up one of [`ACCENTS`] by name.
pub fn accent(name: &str) -> Option<Color> {
    ACCENTS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// Contrast an accent needs against the background: WCAG 2's minimum for
/// controls and focus indicators (success criterion 1.4.11).
const ACCENT_CONTRAST: f32 = 3.0;

impl Theme {
    /// IDs of the themes built into this crate, for [`Theme::builtin`].
    pub const BUILTIN: [&str; 3] = ["derisk-dark", "derisk-light", "derisk-high-contrast"];

    /// The default dark theme, the LosOS web interface's colors: a
    /// near-black navy page, slightly lighter cards with a low-contrast
    /// border, slate text and a teal accent. Desktop and web look alike
    /// because both start from these values.
    pub fn dark() -> Self {
        Self {
            name: "Derisk Dark".to_owned(),
            scheme: Scheme::Dark,
            palette: Palette {
                background: Color::rgb(10, 14, 18),
                // Chosen so cards, half way to it, land on the web
                // interface's #141b22.
                surface: Color::rgb(30, 40, 50),
                foreground: Color::rgb(226, 232, 240),
                border: Color::rgb(33, 44, 54),
                accent: Color::rgb(72, 179, 192),
                destructive: Color::rgb(220, 38, 38),
            },
            fonts: Fonts::default(),
            icons: Icons::default(),
            radius: 6,
        }
    }

    /// The default light theme, with the same accent darkened for contrast.
    pub fn light() -> Self {
        Self {
            name: "Derisk Light".to_owned(),
            scheme: Scheme::Light,
            palette: Palette {
                background: Color::rgb(248, 250, 252),
                surface: Color::rgb(226, 232, 240),
                foreground: Color::rgb(15, 23, 42),
                // The border the old derivation drew, half way from the
                // surface to slate-400, now that borders are drawn as chosen.
                border: Color::rgb(187, 198, 212),
                accent: Color::rgb(72, 179, 192),
                destructive: Color::rgb(220, 38, 38),
            },
            // Papirus's variant drawn for light backgrounds.
            icons: Icons {
                theme: "Papirus".to_owned(),
                ..Icons::default()
            },
            ..Self::dark()
        }
        .with_accent(Color::rgb(72, 179, 192))
    }

    /// Black and white with a yellow accent, for low vision.
    pub fn high_contrast() -> Self {
        Self {
            name: "Derisk High Contrast".to_owned(),
            scheme: Scheme::Dark,
            palette: Palette {
                background: Color::BLACK,
                surface: Color::rgb(26, 26, 26),
                foreground: Color::WHITE,
                border: Color::WHITE,
                accent: Color::rgb(255, 214, 0),
                destructive: Color::rgb(255, 92, 92),
            },
            ..Self::dark()
        }
    }

    /// One of the built-in themes by ID (see [`Theme::BUILTIN`]).
    pub fn builtin(id: &str) -> Option<Self> {
        match id {
            "derisk-dark" => Some(Self::dark()),
            "derisk-light" => Some(Self::light()),
            "derisk-high-contrast" => Some(Self::high_contrast()),
            _ => None,
        }
    }

    /// This theme with `accent` as its accent color, moved toward black or
    /// white just far enough to keep 3:1 contrast against the background.
    pub fn with_accent(mut self, accent: Color) -> Self {
        self.palette.accent = accent.legible_on(self.palette.background, ACCENT_CONTRAST);
        self
    }

    /// The semantic colors components draw with.
    pub fn tokens(&self) -> Tokens {
        Tokens::derive(&self.palette, self.radius)
    }

    /// Parses a theme file. `resolve` supplies the theme named by `inherits`,
    /// usually [`Theme::builtin`] or a [`Library`] lookup.
    pub fn parse(
        text: &str,
        resolve: impl FnMut(&str) -> Option<Theme>,
    ) -> Result<Parsed, ParseError> {
        file::parse(text, resolve)
    }

    /// Writes this theme as a complete theme file, without `inherits`.
    pub fn to_text(&self) -> String {
        file::write(self)
    }
}

/// shadcn/ui's semantic color roles, derived from a [`Palette`].
///
/// Components name colors by role instead of by hue, so a theme only chooses
/// six colors and every component, in every toolkit, follows it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Tokens {
    /// Page background.
    pub background: Color,
    /// Default text.
    pub foreground: Color,
    /// Card and popover background.
    pub card: Color,
    /// Subtle fill for secondary controls, skeletons, and tracks.
    pub muted: Color,
    /// De-emphasized text such as descriptions and placeholders.
    pub muted_foreground: Color,
    /// Fill of primary controls.
    pub primary: Color,
    /// Text on a primary fill.
    pub primary_foreground: Color,
    /// Fill of secondary controls.
    pub secondary: Color,
    /// Fill shown under hovered ghost and outline controls.
    pub hover: Color,
    /// Fill of destructive controls and the text of destructive alerts.
    pub destructive: Color,
    /// Text on a destructive fill.
    pub destructive_foreground: Color,
    /// Borders and separators.
    pub border: Color,
    /// Focus ring.
    pub ring: Color,
    /// Backdrop behind dialogs.
    pub overlay: Color,
    /// Text selection: the primary color, translucent.
    pub selection: Color,
    /// Fill of text fields, switch tracks that are off and sunken panels
    /// such as a sidebar: a step down from the card toward the page.
    pub field: Color,
    /// Corner radius of controls in logical pixels; cards use two more.
    pub radius: u8,
}

impl Tokens {
    /// Derives the roles from `palette`, with controls rounded by `radius`.
    pub fn derive(palette: &Palette, radius: u8) -> Self {
        let p = palette;
        let destructive_foreground = if p.destructive.is_light() {
            Color::rgb(69, 10, 10)
        } else {
            Color::rgb(254, 242, 242)
        };
        let card = p.background.mix(p.surface, 0.5);
        Self {
            background: p.background,
            foreground: p.foreground,
            card,
            muted: p.surface,
            // Toward the surface rather than the border: borders are drawn as
            // chosen and are often faint, which would make this unreadable.
            muted_foreground: p.foreground.mix(p.surface, 0.4),
            primary: p.accent,
            primary_foreground: p.background,
            secondary: p.surface,
            hover: p.surface.mix(p.border, 0.35),
            destructive: p.destructive,
            destructive_foreground,
            border: p.border,
            ring: p.accent,
            overlay: Color::black_alpha(160),
            selection: p.accent.with_alpha(0x55),
            field: p.background.mix(card, 0.6),
            radius,
        }
    }
}

/// Themes found on disk, by ID (the file name without `.theme`).
///
/// Later directories in the search list are shadowed by earlier ones, like
/// every XDG lookup: a theme in the user's data directory overrides a system
/// theme of the same ID, and both override a built-in theme.
#[derive(Clone, Debug, Default)]
pub struct Library {
    dirs: Vec<PathBuf>,
}

/// Why a theme could not be loaded.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum LoadError {
    /// No directory has the theme and it is not built in.
    #[error("no theme named {0}")]
    #[diagnostic(code(mcsapi_theme::not_found))]
    NotFound(String),
    /// The theme file exists but could not be read.
    #[error("{path}: {1}", path = .0.display())]
    #[diagnostic(code(mcsapi_theme::io))]
    Io(PathBuf, #[source] std::io::Error),
    /// The theme file is not a valid theme.
    #[error("{path}:{1}", path = .0.display())]
    #[diagnostic(code(mcsapi_theme::parse))]
    Parse(PathBuf, #[source] ParseError),
    /// Themes inherit from each other in a loop.
    #[error("theme {0} inherits from itself")]
    #[diagnostic(code(mcsapi_theme::cycle))]
    Cycle(String),
}

/// How deep `inherits` may chain before it is treated as a loop.
const MAX_INHERITANCE: usize = 16;

impl Library {
    /// A library searching `dirs` in order.
    pub fn new(dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            dirs: dirs.into_iter().collect(),
        }
    }

    /// A library searching `<app>/themes` under `$XDG_DATA_HOME` (or
    /// `~/.local/share`) and then each of `$XDG_DATA_DIRS` (or
    /// `/usr/local/share:/usr/share`).
    pub fn xdg(app: &str) -> Self {
        let var = |name| std::env::var_os(name).filter(|value| !value.is_empty());
        let home = var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| var("HOME").map(|home| Path::new(&home).join(".local/share")));
        let system = var("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        // `split_paths` panics on wasm32, where the toolkit also runs; the
        // environment is empty there, so all it would split is the default.
        #[cfg(target_family = "wasm")]
        let system: Vec<PathBuf> = system
            .to_string_lossy()
            .split(':')
            .map(PathBuf::from)
            .collect();
        #[cfg(not(target_family = "wasm"))]
        let system = std::env::split_paths(&system);
        let dirs = home
            .into_iter()
            .chain(system.into_iter().filter(|dir| dir.is_absolute()))
            .map(|dir| dir.join(app).join("themes"));
        Self::new(dirs)
    }

    /// The directories searched, most important first.
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// The file a theme ID is read from, if any directory has one.
    pub fn path(&self, id: &str) -> Option<PathBuf> {
        if !valid_id(id) {
            return None;
        }
        self.dirs
            .iter()
            .map(|dir| dir.join(format!("{id}.theme")))
            .find(|path| path.is_file())
    }

    /// Every theme ID available: files in the search directories, then the
    /// built-in themes, without duplicates and sorted.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Theme::BUILTIN.iter().map(|id| (*id).to_owned()).collect();
        for dir in &self.dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".theme")) else {
                    continue;
                };
                if valid_id(id) && !ids.iter().any(|known| known == id) {
                    ids.push(id.to_owned());
                }
            }
        }
        ids.sort();
        ids
    }

    /// Loads a theme by ID, following `inherits`. Files shadow built-in
    /// themes; warnings from every file in the chain are returned too.
    pub fn load(&self, id: &str) -> Result<Parsed, LoadError> {
        self.load_at(id, 0)
    }

    fn load_at(&self, id: &str, depth: usize) -> Result<Parsed, LoadError> {
        if depth > MAX_INHERITANCE {
            return Err(LoadError::Cycle(id.to_owned()));
        }
        let Some(path) = self.path(id) else {
            return Theme::builtin(id)
                .map(|theme| Parsed {
                    theme,
                    warnings: Vec::new(),
                })
                .ok_or_else(|| LoadError::NotFound(id.to_owned()));
        };
        let text = std::fs::read_to_string(&path).map_err(|e| LoadError::Io(path.clone(), e))?;
        let mut inherited = Vec::new();
        let mut failure = None;
        let parsed = file::parse(&text, |parent| {
            // A theme may extend the built-in theme it shadows.
            let result = if parent == id {
                Theme::builtin(parent).ok_or_else(|| LoadError::Cycle(id.to_owned()))
            } else {
                self.load_at(parent, depth + 1).map(|parsed| {
                    inherited.extend(parsed.warnings);
                    parsed.theme
                })
            };
            result.map_err(|error| failure = Some(error)).ok()
        });
        match (parsed, failure) {
            (_, Some(error)) => Err(error),
            (Err(error), None) => Err(LoadError::Parse(path, error)),
            (Ok(mut parsed), None) => {
                inherited.append(&mut parsed.warnings);
                parsed.warnings = inherited;
                Ok(parsed)
            }
        }
    }
}

/// Theme IDs are file names, so they may not contain path separators or
/// start with a dot.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
