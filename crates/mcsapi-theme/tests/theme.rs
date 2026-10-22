use std::fs;

use mcsapi_theme::{Color, Library, LoadError, Scheme, Theme, export};

#[test]
fn colors_round_trip_through_text() {
    for text in ["#0f172a", "#a3e63580", "#000000"] {
        assert_eq!(text.parse::<Color>().unwrap().to_string(), text);
    }
    assert_eq!("#fff".parse::<Color>(), Ok(Color::WHITE));
    for bad in ["0f172a", "#0f172", "#gggggg", "#", "#0f172a0"] {
        assert!(bad.parse::<Color>().is_err(), "{bad}");
    }
}

#[test]
fn contrast_follows_wcag() {
    assert!((Color::BLACK.contrast(Color::WHITE) - 21.0).abs() < 0.01);
    assert!((Color::WHITE.contrast(Color::WHITE) - 1.0).abs() < 0.01);
}

#[test]
fn default_tokens_match_the_web_interface() {
    // Measured from a screenshot of the LosOS web interface: page, card,
    // card border and accent exactly; secondary text as close as the
    // derivation gets to its slate-400.
    let t = Theme::dark().tokens();
    assert_eq!(t.background, Color::rgb(10, 14, 18));
    assert_eq!(t.card, Color::rgb(20, 27, 34));
    assert_eq!(t.border, Color::rgb(33, 44, 54));
    assert_eq!(t.primary, Color::rgb(72, 179, 192));
    assert_eq!(t.muted_foreground, Color::rgb(148, 155, 164));
    assert_eq!(t.field, Color::rgb(16, 22, 28));
    assert_eq!(t.hover, Color::rgb(31, 41, 51));
    // Dark text on the accent, as on the web interface's selected item.
    assert_eq!(t.primary_foreground, Color::rgb(10, 14, 18));
    assert_eq!(t.destructive_foreground, Color::rgb(254, 242, 242));
    assert_eq!(t.overlay, Color::black_alpha(160));
    assert_eq!(t.radius, 6);

    // The light theme's border is the one the old derivation drew.
    let light = mcsapi_theme::Tokens::derive(&Theme::light().palette, 6);
    assert_eq!(light.card, Color::rgb(237, 241, 246));
    assert_eq!(light.muted_foreground, Color::rgb(99, 107, 121));
    assert_eq!(light.hover, Color::rgb(212, 220, 230));
    assert_eq!(light.border, Color::rgb(187, 198, 212));
}

#[test]
fn every_builtin_theme_keeps_its_accent_legible() {
    for id in Theme::BUILTIN {
        let theme = Theme::builtin(id).unwrap();
        let p = theme.palette;
        assert!(p.accent.contrast(p.background) >= 3.0, "{id} accent");
        assert!(p.foreground.contrast(p.background) >= 7.0, "{id} text");
        // A theme written out reads back identically.
        let parsed = Theme::parse(&theme.to_text(), |_| None).unwrap();
        assert_eq!(parsed.theme, theme, "{id}");
        assert!(parsed.warnings.is_empty());
    }
}

#[test]
fn light_accents_are_darkened_only_as_far_as_needed() {
    let sky = mcsapi_theme::accent("sky").unwrap();
    let light = Theme::light().with_accent(sky);
    assert_ne!(light.palette.accent, sky);
    assert!(light.palette.accent.contrast(light.palette.background) >= 3.0);
    // One step less would not have been enough.
    assert!(
        sky.mix(Color::BLACK, 0.0)
            .contrast(light.palette.background)
            < 3.0
    );
    // On dark, the accents already pass and are kept as they are.
    assert_eq!(Theme::dark().with_accent(sky).palette.accent, sky);
}

#[test]
fn a_theme_can_inherit_and_override() {
    let parsed = Theme::parse(
        "name = \"Paper\"\ninherits = \"derisk-light\"\n[colors]\naccent = \"rose\"\n[shape]\nradius = 10\n",
        Theme::builtin,
    )
    .unwrap();
    let theme = parsed.theme;
    assert_eq!(theme.name, "Paper");
    assert_eq!(theme.scheme, Scheme::Light);
    assert_eq!(theme.palette.background, Theme::light().palette.background);
    // An accent the file names is taken as written.
    assert_eq!(Some(theme.palette.accent), mcsapi_theme::accent("rose"));
    assert_eq!(theme.tokens().radius, 10);
}

#[test]
fn an_inherited_accent_is_rechecked_against_a_new_background() {
    let theme = Theme::parse(
        "inherits = \"derisk-dark\"\nscheme = \"light\"\n[colors]\nbackground = \"#ffffff\"\n",
        Theme::builtin,
    )
    .unwrap()
    .theme;
    assert!(theme.palette.accent.contrast(Color::WHITE) >= 3.0);
}

#[test]
fn unknown_keys_warn_and_bad_values_fail() {
    let parsed = Theme::parse("[colors]\nsparkle = \"#ffffff\"\n[future]\nx = 1\n", |_| {
        None
    })
    .unwrap();
    assert_eq!(parsed.warnings.len(), 2);
    assert_eq!(parsed.warnings[0].line, 2);

    for (text, line) in [
        ("[colors]\naccent = \"chartreuse\"", 2),
        ("scheme = \"sepia\"", 1),
        ("[shape]\nradius = 2.5", 2),
        ("[shape]\nradius = 99", 2),
        ("[fonts]\nsize = \"big\"", 2),
        ("inherits = \"nope\"", 1),
        ("name = \"a\"\nname = \"b\"", 2),
        ("[colors\n", 1),
        ("just words", 1),
        ("name = \"unterminated", 1),
    ] {
        let error = Theme::parse(text, Theme::builtin).unwrap_err();
        assert_eq!(error.line, line, "{text}: {error}");
    }
}

#[test]
fn comments_and_escapes_parse() {
    let theme = Theme::parse(
        "# a theme\nname = \"Say \\\"hi\\\" # not a comment\" # a comment\n[shape] # shape\nradius = 4 # px\n",
        |_| None,
    )
    .unwrap()
    .theme;
    assert_eq!(theme.name, "Say \"hi\" # not a comment");
    assert_eq!(theme.radius, 4);
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mcsapi-theme-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn libraries_shadow_in_order_and_follow_inheritance() {
    let (user, system) = (scratch("user"), scratch("system"));
    fs::write(
        system.join("ocean.theme"),
        "name = \"Ocean\"\n[colors]\naccent = \"sky\"\n",
    )
    .unwrap();
    fs::write(
        user.join("ocean.theme"),
        "name = \"My Ocean\"\ninherits = \"base\"\n",
    )
    .unwrap();
    fs::write(
        system.join("base.theme"),
        "[shape]\nradius = 2\n[x]\ny = 1\n",
    )
    .unwrap();
    // A file may extend the built-in theme it replaces.
    fs::write(
        user.join("derisk-dark.theme"),
        "inherits = \"derisk-dark\"\n[shape]\nradius = 9\n",
    )
    .unwrap();
    fs::write(user.join("loop.theme"), "inherits = \"loop2\"\n").unwrap();
    fs::write(user.join("loop2.theme"), "inherits = \"loop\"\n").unwrap();
    let library = Library::new([user.clone(), system.clone()]);

    let ocean = library.load("ocean").unwrap();
    assert_eq!(ocean.theme.name, "My Ocean");
    assert_eq!(ocean.theme.radius, 2);
    // The warning from the inherited file is kept.
    assert_eq!(ocean.warnings.len(), 1);

    assert_eq!(library.load("derisk-dark").unwrap().theme.radius, 9);
    assert_eq!(library.load("derisk-light").unwrap().theme, Theme::light());
    assert!(matches!(library.load("loop"), Err(LoadError::Cycle(_))));
    assert!(matches!(
        library.load("missing"),
        Err(LoadError::NotFound(_))
    ));
    assert!(matches!(
        library.load("../etc/passwd"),
        Err(LoadError::NotFound(_))
    ));

    let ids = library.ids();
    for id in [
        "base",
        "derisk-dark",
        "derisk-high-contrast",
        "derisk-light",
        "ocean",
    ] {
        assert!(ids.iter().any(|known| known == id), "{id} in {ids:?}");
    }
    assert_eq!(ids.iter().filter(|id| *id == "derisk-dark").count(), 1);
    let _ = fs::remove_dir_all(user);
    let _ = fs::remove_dir_all(system);
}

#[test]
fn exports_carry_the_theme() {
    let light = Theme::light();
    let portal = export::portal(&light);
    assert_eq!(portal.color_scheme, 2);
    assert_eq!(portal.contrast, 0);
    assert_eq!(export::portal(&Theme::high_contrast()).contrast, 1);

    let ini = export::gtk_settings(&Theme::dark());
    assert!(ini.contains("gtk-application-prefer-dark-theme=1"));
    assert!(ini.contains("gtk-font-name=Inter 10.5"));

    let xml = export::android_colors(&Theme::dark());
    assert!(xml.contains("<color name=\"mcsapi_background\">#ff0a0e12</color>"));
    assert!(xml.contains("<color name=\"colorAccent\">@color/mcsapi_primary</color>"));
    assert!(xml.contains("<color name=\"mcsapi_overlay\">#a0000000</color>"));

    let json = export::json("derisk-dark", &Theme::dark());
    assert!(
        json.starts_with("{\"id\":\"derisk-dark\",\"name\":\"Derisk Dark\",\"scheme\":\"dark\"")
    );
    assert!(json.contains("\"primary\":\"#48b3c0\""));
    assert!(json.contains("\"field\":\"#10161c\""));
    assert!(json.contains("\"radius\":6"));
    // Balanced braces and brackets: the JSON is well formed enough for any
    // parser to read; the CI job for consumers checks it with a real one.
    assert_eq!(json.matches('{').count(), json.matches('}').count());

    let css = export::css_variables(&Theme::dark());
    assert!(css.starts_with(":root {\n  color-scheme: dark;\n"));
    assert!(css.contains("  --background: #0a0e12;\n"));
    assert!(css.contains("  --muted-foreground: #949ba4;\n"));
    assert!(css.contains("  --overlay: #000000a0;\n"));
    assert!(css.contains("  --radius: 8px;\n  --radius-control: 6px;\n"));
    assert!(css.contains("  --font-sans: \"Inter\";\n"));
    assert!(css.ends_with("}\n"));
}

#[test]
fn names_cannot_inject_lines_into_exports() {
    // A downloaded theme trying to add `gtk-modules=` to settings.ini.
    for key in [
        "[icons]\ntheme",
        "[icons]\ncursor",
        "[fonts]\nsans",
        "[fonts]\nmonospace",
        "name",
    ] {
        let text = format!("{key} = \"Adwaita\\ngtk-modules=evil\"\n");
        assert!(Theme::parse(&text, |_| None).is_err(), "{key}");
    }
    for bad in ["../../etc", "a/b", ".."] {
        let text = format!("[icons]\ntheme = \"{bad}\"\n");
        assert!(Theme::parse(&text, |_| None).is_err(), "{bad}");
    }
    // A theme built in code is cleaned on the way out instead.
    let mut theme = Theme::dark();
    theme.icons.theme = "Adwaita\ngtk-modules=evil".to_owned();
    theme.fonts.sans = "Ubuntu\r\ngtk-modules=evil".to_owned();
    theme.icons.cursor = "x\ny".to_owned();
    let ini = export::gtk_settings(&theme);
    assert_eq!(
        ini.lines().filter(|l| l.starts_with("gtk-modules")).count(),
        0
    );
    assert_eq!(ini.lines().count(), 6);
    assert!(
        export::environment(&theme)
            .iter()
            .all(|(_, v)| !v.contains('\n'))
    );
}
