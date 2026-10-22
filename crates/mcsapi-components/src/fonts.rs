//! The theme's font families, read from the system, for egui.
//!
//! GPUI, GTK, Qt and web content find the theme's families through
//! fontconfig, while egui only knows the fonts embedded in it (Ubuntu Light
//! and Hack). [`install_fonts`] puts the installed faces of the theme's
//! families in front of egui's, so an egui app draws the same letters as the
//! rest of the desktop. egui's own fonts stay behind them, for the characters
//! a family lacks and for a family that is not installed.

use egui::{FontData, FontDefinitions, FontFamily};
use mcsapi_ui::theme::Fonts;
use std::sync::Arc;

/// Makes the families in `fonts` the ones egui draws with in `ctx`.
///
/// Call it whenever the theme may have changed, as often as every frame:
/// it does nothing while the families are the ones it last installed. The
/// first call in a process reads the system's font list, and each family is
/// read from disk once.
pub fn install_fonts(ctx: &egui::Context, fonts: &Fonts) {
    let id = egui::Id::new("mcsapi-components::install_fonts");
    let installed = Installed {
        sans: fonts.sans.clone(),
        sans_weight: fonts.sans_weight,
        monospace: fonts.monospace.clone(),
    };
    if ctx.data(|data| data.get_temp::<Installed>(id)).as_ref() == Some(&installed) {
        return;
    }
    ctx.set_fonts(definitions(fonts, system::face));
    ctx.data_mut(|data| data.insert_temp(id, installed));
}

/// The families [`install_fonts`] last put into a context.
#[derive(Clone, PartialEq)]
struct Installed {
    sans: String,
    sans_weight: u16,
    monospace: String,
}

/// egui's default fonts with the faces `face` finds for the theme's families
/// in front: the sans family at its weight for proportional text, and the
/// monospace one at the regular weight for code.
fn definitions(
    fonts: &Fonts,
    face: impl Fn(&str, u16) -> Option<Arc<FontData>>,
) -> FontDefinitions {
    let mut definitions = FontDefinitions::default();
    for (family, name, weight) in [
        (FontFamily::Proportional, &fonts.sans, fonts.sans_weight),
        (FontFamily::Monospace, &fonts.monospace, 400),
    ] {
        let Some(data) = face(name, weight) else {
            continue;
        };
        // Keyed by family and weight, so the two never collide when the
        // theme names one family for both.
        let key = format!("{name} {weight}");
        definitions.font_data.insert(key.clone(), data);
        definitions
            .families
            .entry(family)
            .or_default()
            .insert(0, key);
    }
    definitions
}

#[cfg(not(target_family = "wasm"))]
mod system {
    use egui::FontData;
    use egui::epaint::text::VariationCoords;
    use fontdb::{Database, Family, Query, Weight};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    /// Faces already looked up, by family and weight, found or not.
    type Faces = HashMap<(String, u16), Option<Arc<FontData>>>;

    /// The installed face of `family` closest to `weight`, as fontconfig's
    /// configuration lists the font directories. Every context in a process
    /// shares one copy of it.
    pub fn face(family: &str, weight: u16) -> Option<Arc<FontData>> {
        static DATABASE: OnceLock<Database> = OnceLock::new();
        static FACES: OnceLock<Mutex<Faces>> = OnceLock::new();
        let mut faces = FACES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        faces
            .entry((family.to_owned(), weight))
            .or_insert_with(|| {
                let database = DATABASE.get_or_init(|| {
                    let mut database = Database::new();
                    database.load_system_fonts();
                    database
                });
                let id = database.query(&Query {
                    families: &[Family::Name(family)],
                    weight: Weight(weight),
                    ..Query::default()
                })?;
                database.with_face_data(id, |bytes, index| {
                    let mut data = FontData::from_owned(bytes.to_vec());
                    data.index = index;
                    // A variable font draws at the theme's weight exactly;
                    // a static face has no axes and ignores it.
                    data.tweak.coords = VariationCoords::new([(b"wght", f32::from(weight))]);
                    Arc::new(data)
                })
            })
            .clone()
    }
}

#[cfg(target_family = "wasm")]
mod system {
    use egui::FontData;
    use std::sync::Arc;

    /// A browser tab has no font files to read, so egui keeps its own.
    pub fn face(_family: &str, _weight: u16) -> Option<Arc<FontData>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for an installed font: egui's embedded Hack, under the
    /// name the theme asks for.
    fn installed(name: &str) -> impl Fn(&str, u16) -> Option<Arc<FontData>> {
        let hack = FontDefinitions::default().font_data["Hack"].clone();
        let name = name.to_owned();
        move |family, _| (family == name).then(|| hack.clone())
    }

    #[test]
    fn an_installed_family_goes_in_front_of_eguis() {
        let fonts = Fonts::default();
        let definitions = definitions(&fonts, installed(&fonts.sans));
        let key = format!("{} {}", fonts.sans, fonts.sans_weight);
        assert_eq!(definitions.families[&FontFamily::Proportional][0], key);
        assert!(definitions.font_data.contains_key(&key));
        // egui's fonts stay behind it for what it lacks, such as emoji.
        assert_eq!(
            definitions.families[&FontFamily::Proportional][1..],
            FontDefinitions::default().families[&FontFamily::Proportional][..]
        );
        assert_eq!(
            definitions.families[&FontFamily::Monospace],
            FontDefinitions::default().families[&FontFamily::Monospace]
        );
    }

    #[test]
    fn a_missing_family_leaves_eguis_fonts() {
        let definitions = definitions(&Fonts::default(), installed("Not Installed"));
        assert_eq!(definitions, FontDefinitions::default());
    }
}
