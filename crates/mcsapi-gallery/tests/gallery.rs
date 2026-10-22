use std::collections::HashSet;

use mcsapi_gallery::{Category, Gallery, PRESETS, find, specimens};
use mcsapi_ui::{Theme, egui};

/// Exports of mcsapi-components that need no specimen of their own.
const NOT_SHOWN: &[&str] = &[
    // The theme switcher exercises the tokens on every specimen.
    "Tokens",
    // The gallery draws every specimen in the fonts it installs.
    "install_fonts",
];

fn frames(context: &egui::Context, gallery: &mut Gallery, count: usize) -> egui::FullOutput {
    let mut last = None;
    for frame in 0..count {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            time: Some(frame as f64 / 60.0),
            ..Default::default()
        };
        let mut output = mcsapi_ui::run_frame(gallery, context, input, &Theme::default());
        output.textures_delta.clear();
        last = Some(output);
    }
    last.expect("count is nonzero")
}

#[test]
fn every_specimen_renders_in_every_preset() {
    for preset in &PRESETS {
        let context = egui::Context::default();
        let mut gallery = Gallery::default();
        assert!(gallery.set_preset(preset.name));
        for specimen in specimens() {
            assert!(gallery.select(Some(specimen.name)));
            let output = frames(&context, &mut gallery, 2);
            assert!(
                !output.shapes.is_empty(),
                "{} drew nothing in {}",
                specimen.name,
                preset.name
            );
        }
        assert!(gallery.select(None));
        frames(&context, &mut gallery, 2);
    }
}

#[test]
fn specimens_are_named_uniquely_and_grouped() {
    let mut names = HashSet::new();
    let mut last = Category::ALL[0];
    for specimen in specimens() {
        assert!(names.insert(specimen.name), "duplicate {}", specimen.name);
        assert!(!specimen.api.is_empty(), "{} lists no API", specimen.name);
        assert!(specimen.category >= last, "specimens are not grouped");
        last = specimen.category;
        assert_eq!(
            find(&specimen.name.to_uppercase()).unwrap().name,
            specimen.name
        );
    }
    assert!(find("no such widget").is_none());
    assert!(!Gallery::default().select(Some("no such widget")));
    assert!(!Gallery::default().set_preset("no such theme"));
}

/// Names re-exported from the crate root of mcsapi-components.
fn component_exports() -> Vec<String> {
    let source = include_str!("../../mcsapi-components/src/lib.rs");
    let mut names = Vec::new();
    for statement in source.split("pub use ").skip(1) {
        let statement = statement.split(';').next().unwrap_or_default();
        let list = match statement.split_once('{') {
            Some((_, list)) => list.trim_end_matches(|c: char| c == '}' || c.is_whitespace()),
            None => statement.rsplit("::").next().unwrap_or_default(),
        };
        names.extend(
            list.split(',')
                .map(|name| {
                    name.trim()
                        .rsplit("::")
                        .next()
                        .unwrap_or_default()
                        .to_owned()
                })
                .filter(|name| !name.is_empty()),
        );
    }
    names
}

#[test]
fn every_component_has_a_specimen() {
    let exports = component_exports();
    assert!(
        exports.iter().any(|name| name == "Button"),
        "parsed {exports:?}"
    );
    let shown: HashSet<&str> = specimens().flat_map(|s| s.api.iter().copied()).collect();
    let missing: Vec<&String> = exports
        .iter()
        .filter(|name| !shown.contains(name.as_str()) && !NOT_SHOWN.contains(&name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "mcsapi-components exports {missing:?} but no gallery specimen lists them in `api`; \
         add a specimen under crates/mcsapi-gallery/src/specimens/"
    );
}
