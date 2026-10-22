//! Theme files.
//!
//! A theme file is a small subset of TOML, so TOML editors highlight it and
//! TOML tools can read it, without this crate depending on a TOML parser:
//! `[section]` headers, `key = value` lines, and `#` comments. Values are
//! double-quoted strings (with `\"` and `\\` escapes, and no others: no
//! value in a theme needs a line break), numbers, or `true` and
//! `false`. Every key is optional; missing ones come from the theme named by
//! `inherits`, or from `derisk-dark` when there is none.
//!
//! ```toml
//! name = "Derisk Dark"
//! scheme = "dark"            # or "light"
//! inherits = "derisk-dark"   # another theme ID; omitted when complete
//!
//! [colors]
//! background = "#0a0e12"
//! surface = "#1e2832"
//! foreground = "#e2e8f0"
//! border = "#212c36"         # drawn as is around cards and fields
//! accent = "#48b3c0"         # or a built-in accent name such as "sky"
//! destructive = "#dc2626"
//!
//! [fonts]
//! sans = "Inter"
//! sans_weight = 400
//! monospace = "Hack"
//! size = 14
//! small_size = 12
//! monospace_size = 12
//!
//! [icons]
//! theme = "Adwaita"
//! cursor = "Adwaita"
//! cursor_size = 24
//!
//! [shape]
//! radius = 6              # controls; cards are two more
//! ```
//!
//! Unknown keys and sections are reported as [`Warning`]s rather than errors,
//! so a theme written for a newer version still loads. A value of the wrong
//! type, or out of range, is an error: it would otherwise silently fall back
//! to the inherited value.

use std::fmt;

use crate::{Color, Scheme, Theme};

/// A parsed theme and anything in the file that was ignored.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    /// The theme.
    pub theme: Theme,
    /// Keys that were ignored.
    pub warnings: Vec<Warning>,
}

/// A line that was ignored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Warning {
    /// 1-based line number.
    pub line: usize,
    /// What was ignored.
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.line, self.message)
    }
}

/// Why a theme file is invalid.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error, miette::Diagnostic)]
#[error("{line}: {message}")]
#[diagnostic(code(mcsapi_theme::parse))]
pub struct ParseError {
    /// 1-based line number.
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Text(String),
    Number(f64),
    Bool(bool),
}

impl Value {
    fn describe(&self) -> &'static str {
        match self {
            Self::Text(_) => "a string",
            Self::Number(_) => "a number",
            Self::Bool(_) => "true or false",
        }
    }
}

fn parse_value(text: &str) -> Option<Value> {
    if let Some(inner) = text.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = inner.chars();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    _ => return None,
                },
                c => out.push(c),
            }
        }
        let rest = chars.as_str().trim_start();
        return (rest.is_empty() || rest.starts_with('#')).then_some(Value::Text(out));
    }
    let text = text.split_once('#').map_or(text, |(value, _)| value).trim();
    match text {
        "true" => Some(Value::Bool(true)),
        "false" => Some(Value::Bool(false)),
        _ => text
            .replace('_', "")
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .map(Value::Number),
    }
}

fn is_key(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// One assignment, in file order.
struct Entry {
    line: usize,
    section: String,
    key: String,
    value: Value,
}

fn tokenize(text: &str) -> Result<Vec<Entry>, ParseError> {
    let mut section = String::new();
    let mut entries: Vec<Entry> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let error = |message: &str| ParseError {
            line,
            message: message.to_owned(),
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(header) = trimmed.strip_prefix('[') {
            let header = header.split_once('#').map_or(header, |(h, _)| h).trim_end();
            let name = header
                .strip_suffix(']')
                .map(str::trim)
                .filter(|name| is_key(name))
                .ok_or_else(|| error("expected a section header like [colors]"))?;
            section = name.to_owned();
            continue;
        }
        let (key, value) = trimmed
            .split_once('=')
            .ok_or_else(|| error("expected key = value"))?;
        let key = key.trim();
        if !is_key(key) {
            return Err(error("expected a key made of letters, digits, - and _"));
        }
        let value = parse_value(value.trim())
            .ok_or_else(|| error("expected a quoted string, a number, true or false"))?;
        if entries.iter().any(|e| e.section == section && e.key == key) {
            return Err(error(&format!("{} is set twice", qualified(&section, key))));
        }
        entries.push(Entry {
            line,
            section: section.clone(),
            key: key.to_owned(),
            value,
        });
    }
    Ok(entries)
}

fn qualified(section: &str, key: &str) -> String {
    if section.is_empty() {
        key.to_owned()
    } else {
        format!("{section}.{key}")
    }
}

/// Parses `text`, taking the theme named by `inherits` from `resolve`.
pub(crate) fn parse(
    text: &str,
    mut resolve: impl FnMut(&str) -> Option<Theme>,
) -> Result<Parsed, ParseError> {
    let entries = tokenize(text)?;
    let mut theme = match entries
        .iter()
        .find(|e| e.section.is_empty() && e.key == "inherits")
    {
        Some(entry) => {
            let Value::Text(id) = &entry.value else {
                return Err(ParseError {
                    line: entry.line,
                    message: "inherits must be a theme ID in quotes".to_owned(),
                });
            };
            resolve(id).ok_or_else(|| ParseError {
                line: entry.line,
                message: format!("inherits an unknown theme {id}"),
            })?
        }
        None => Theme::dark(),
    };
    // A file that changes the scheme but not the accent keeps the inherited
    // accent, re-checked for contrast against its own background below.
    let mut accent_set = false;
    let mut warnings = Vec::new();
    for entry in &entries {
        let Entry {
            line,
            section,
            key,
            value,
        } = entry;
        let name = qualified(section, key);
        let error = |message: String| ParseError {
            line: *line,
            message: format!("{name}: {message}"),
        };
        let wrong_type =
            |expected: &str| error(format!("expected {expected}, found {}", value.describe()));
        let text = || match value {
            Value::Text(text) => Ok(text.as_str()),
            _ => Err(wrong_type("a string")),
        };
        let number = |min: f64, max: f64| match value {
            Value::Number(n) if (min..=max).contains(n) => Ok(*n),
            Value::Number(_) => Err(error(format!("must be between {min} and {max}"))),
            _ => Err(wrong_type("a number")),
        };
        let integer = |min: f64, max: f64| {
            number(min, max).and_then(|n| {
                if n.fract() == 0.0 {
                    Ok(n)
                } else {
                    Err(error("must be a whole number".to_owned()))
                }
            })
        };
        let color = || {
            let text = text()?;
            crate::accent(text)
                .map(Ok)
                .unwrap_or_else(|| text.parse::<Color>())
                .map_err(|e| error(e.to_string()))
        };
        // Names end up as lines of GTK's settings.ini and in environment
        // variables, where a line break would let a downloaded theme add
        // settings of its own (`gtk-modules=` loads code into every GTK app).
        let single_line = || {
            let text = text()?;
            if text.chars().any(char::is_control) {
                Err(error(
                    "must not contain line breaks or control characters".to_owned(),
                ))
            } else {
                Ok(text.to_owned())
            }
        };
        let family = || {
            let text = single_line()?;
            if text.trim().is_empty() {
                Err(error("must name a font family".to_owned()))
            } else {
                Ok(text)
            }
        };
        // Icon and cursor themes are directory names under icons/.
        let directory = || {
            let text = single_line()?;
            if text.is_empty() || text.contains('/') || text == "." || text == ".." {
                Err(error("must be an icon theme directory name".to_owned()))
            } else {
                Ok(text)
            }
        };
        let palette = &mut theme.palette;
        match (section.as_str(), key.as_str()) {
            ("", "inherits") => {}
            ("", "name") => theme.name = single_line()?,
            ("", "scheme") => {
                theme.scheme = Scheme::parse(text()?)
                    .ok_or_else(|| error("must be \"dark\" or \"light\"".to_owned()))?;
            }
            ("colors", "background") => palette.background = color()?,
            ("colors", "surface") => palette.surface = color()?,
            ("colors", "foreground") => palette.foreground = color()?,
            ("colors", "border") => palette.border = color()?,
            ("colors", "accent") => {
                palette.accent = color()?;
                accent_set = true;
            }
            ("colors", "destructive") => palette.destructive = color()?,
            ("fonts", "sans") => theme.fonts.sans = family()?,
            // CSS weights run from 1 to 1000.
            ("fonts", "sans_weight") => theme.fonts.sans_weight = integer(1.0, 1000.0)? as u16,
            ("fonts", "monospace") => theme.fonts.monospace = family()?,
            ("fonts", "size") => theme.fonts.size = number(6.0, 72.0)? as f32,
            ("fonts", "small_size") => theme.fonts.small_size = number(6.0, 72.0)? as f32,
            ("fonts", "monospace_size") => theme.fonts.monospace_size = number(6.0, 72.0)? as f32,
            ("icons", "theme") => theme.icons.theme = directory()?,
            ("icons", "cursor") => theme.icons.cursor = directory()?,
            ("icons", "cursor_size") => theme.icons.cursor_size = integer(8.0, 256.0)? as u16,
            ("shape", "radius") => theme.radius = integer(0.0, 32.0)? as u8,
            _ => warnings.push(Warning {
                line: *line,
                message: format!("unknown key {name} ignored"),
            }),
        }
    }
    if !accent_set {
        // An inherited accent was legible on the inherited background; this
        // file may have changed the background under it.
        let accent = theme.palette.accent;
        theme = theme.with_accent(accent);
    }
    Ok(Parsed { theme, warnings })
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Writes every field of `theme`.
pub(crate) fn write(theme: &Theme) -> String {
    let p = &theme.palette;
    let f = &theme.fonts;
    let i = &theme.icons;
    format!(
        "name = {name}\n\
         scheme = \"{scheme}\"\n\
         \n\
         [colors]\n\
         background = \"{background}\"\n\
         surface = \"{surface}\"\n\
         foreground = \"{foreground}\"\n\
         border = \"{border}\"\n\
         accent = \"{accent}\"\n\
         destructive = \"{destructive}\"\n\
         \n\
         [fonts]\n\
         sans = {sans}\n\
         sans_weight = {sans_weight}\n\
         monospace = {monospace}\n\
         size = {size}\n\
         small_size = {small_size}\n\
         monospace_size = {monospace_size}\n\
         \n\
         [icons]\n\
         theme = {icons}\n\
         cursor = {cursor}\n\
         cursor_size = {cursor_size}\n\
         \n\
         [shape]\n\
         radius = {radius}\n",
        name = quote(&theme.name),
        scheme = theme.scheme.as_str(),
        background = p.background,
        surface = p.surface,
        foreground = p.foreground,
        border = p.border,
        accent = p.accent,
        destructive = p.destructive,
        sans = quote(&f.sans),
        sans_weight = f.sans_weight,
        monospace = quote(&f.monospace),
        size = f.size,
        small_size = f.small_size,
        monospace_size = f.monospace_size,
        icons = quote(&i.theme),
        cursor = quote(&i.cursor),
        cursor_size = i.cursor_size,
        radius = theme.radius,
    )
}
