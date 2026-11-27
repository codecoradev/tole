use super::*;

fn all_themes() -> Vec<(&'static str, Theme)> {
    BUILTIN
        .iter()
        .map(|n| (*n, Theme::named(n).expect("builtin")))
        .collect()
}

/// The styles `render.rs` hardcoded before themes (#463 part 3). If this
/// fails, the default look changed.
#[test]
fn default_theme_is_pinned_to_the_pre_theme_styles() {
    let d = Theme::default();
    let dim = Style::new().add_modifier(Modifier::DIM);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let expected: [(Role, Style); 20] = [
        (Role::User, bold),
        (Role::Text, Style::new()),
        (
            Role::Reasoning,
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        ),
        (Role::Tool, Style::new().fg(Color::Cyan)),
        (
            Role::ToolOutput,
            Style::new().fg(Color::Cyan).add_modifier(Modifier::DIM),
        ),
        (
            Role::Heading,
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        (Role::Code, dim),
        (Role::Note, Style::new().fg(Color::Yellow)),
        (Role::DiffAdd, Style::new().fg(Color::Green)),
        (Role::DiffRemove, Style::new().fg(Color::Red)),
        (Role::DiffHunk, dim),
        (Role::InlineCode, Style::new().fg(Color::Cyan)),
        (Role::InlineBold, bold),
        (Role::Queue, Style::new().fg(Color::Magenta)),
        (Role::Menu, Style::new().fg(Color::Cyan)),
        (Role::Picker, Style::new()),
        (Role::Hint, dim),
        (Role::Border, dim),
        (Role::Footer, dim),
        (Role::Approval, Style::new().fg(Color::Yellow)),
    ];
    for (role, style) in expected {
        assert_eq!(d.get(role), style, "{}", role.name());
    }
    assert_eq!(Role::ALL.len(), expected.len());
}

#[test]
fn every_builtin_defines_every_role_and_names_round_trip() {
    for r in Role::ALL {
        assert_eq!(Role::from_name(r.name()), Some(r));
    }
    let mut names: Vec<_> = Role::ALL.iter().map(|r| r.name()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), Role::ALL.len(), "role names are unique");
    for (name, t) in all_themes() {
        assert_eq!(t.styles.len(), Role::ALL.len(), "{name}");
    }
    assert!(Theme::named("nope").is_none());
    assert!(Theme::named(" LIGHT ").is_some());
}

#[test]
fn mono_has_no_color_and_light_differs_where_it_matters() {
    for r in Role::ALL {
        let s = Theme::builtin_mono().get(r);
        assert_eq!((s.fg, s.bg), (None, None), "mono {}", r.name());
    }
    let (d, l) = (Theme::builtin_default(), Theme::builtin_light());
    for r in [
        Role::Tool,
        Role::Heading,
        Role::Note,
        Role::Approval,
        Role::Reasoning,
        Role::InlineCode,
        Role::Menu,
    ] {
        assert_ne!(d.get(r), l.get(r), "light {}", r.name());
    }
    // Colors that vanish on a white background are not used by `light`.
    for r in Role::ALL {
        let fgc = l.get(r).fg;
        assert!(
            !matches!(fgc, Some(Color::White | Color::Yellow | Color::Cyan)),
            "light {}",
            r.name()
        );
    }
}

#[test]
fn parses_names_indexes_and_hex() {
    assert_eq!(parse_color("red"), Ok(Color::Red));
    assert_eq!(parse_color("Light-Blue"), Ok(Color::LightBlue));
    assert_eq!(parse_color("dark_grey"), Ok(Color::DarkGray));
    assert_eq!(parse_color("0"), Ok(Color::Indexed(0)));
    assert_eq!(parse_color("255"), Ok(Color::Indexed(255)));
    assert_eq!(parse_color("#cc8800"), Ok(Color::Rgb(0xcc, 0x88, 0)));
    assert_eq!(parse_color("#CC8800"), Ok(Color::Rgb(0xcc, 0x88, 0)));
    for bad in [
        "256",
        "-1",
        "#abc",
        "#12345g",
        "#1234567",
        "#",
        "ultraviolet",
        "",
        "#é12345",
    ] {
        assert!(parse_color(bad).is_err(), "{bad:?} must be rejected");
    }
    let s = parse_spec("blue bold italic underline dim").unwrap();
    assert_eq!(s.fg, Some(Color::Blue));
    assert_eq!(
        s.add_modifier,
        Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED | Modifier::DIM
    );
    assert_eq!(parse_spec("bold").unwrap().fg, None);
    assert!(parse_spec("").is_err());
    assert!(parse_spec("red blue").is_err());
}

#[test]
fn an_invalid_value_names_the_role() {
    let mut t = Theme::default();
    let e = t.set_role("tool", "ultraviolet").unwrap_err();
    assert!(e.contains("`tool`") && e.contains("ultraviolet"), "{e}");
    let e = t.set_role("toool", "red").unwrap_err();
    assert!(
        e.contains("unknown role `toool`") && e.contains("diff_add"),
        "{e}"
    );
    assert_eq!(t, Theme::default(), "a failed set changes nothing");
}

#[test]
fn no_theme_can_produce_a_background_or_conceal() {
    let banned = Modifier::HIDDEN
        | Modifier::REVERSED
        | Modifier::SLOW_BLINK
        | Modifier::RAPID_BLINK
        | Modifier::CROSSED_OUT;
    for spec in [
        "hidden",
        "conceal",
        "reversed",
        "reverse",
        "blink",
        "bg:red",
        "bg=red",
        "on_red",
        "background",
        "red on blue",
        "bgred",
        "crossed",
    ] {
        assert!(parse_spec(spec).is_err(), "{spec:?} must be rejected");
    }
    let mut custom = Theme::default();
    for r in Role::ALL {
        custom
            .set_role(r.name(), "#123456 bold italic underline dim")
            .unwrap();
    }
    let mut themes = all_themes();
    themes.push(("custom", custom));
    for (name, t) in themes {
        for r in Role::ALL {
            let s = t.get(r);
            assert_eq!(s.bg, None, "{name} {}", r.name());
            assert_eq!(s.underline_color, None, "{name} {}", r.name());
            assert!(!s.add_modifier.intersects(banned), "{name} {}", r.name());
            assert!(s.sub_modifier.is_empty(), "{name} {}", r.name());
        }
    }
}

#[test]
fn critical_roles_are_never_dim_and_custom_ones_are_bold() {
    let critical: Vec<Role> = Role::ALL.into_iter().filter(|r| r.is_critical()).collect();
    assert_eq!(critical, [Role::Note, Role::Approval]);
    for (name, t) in all_themes() {
        for r in &critical {
            assert!(!t.get(*r).add_modifier.contains(Modifier::DIM), "{name}");
            // The default keeps its historical (non-bold) look; every
            // other built-in is bold.
            if name != "default" {
                assert!(t.get(*r).add_modifier.contains(Modifier::BOLD), "{name}");
            }
        }
    }
    for spec in [
        "dim",
        "red dim",
        "dim italic",
        "green",
        "underline",
        "#ffffff dim",
    ] {
        for r in &critical {
            let mut t = Theme::default();
            t.set_role(r.name(), spec).unwrap();
            let m = t.get(*r).add_modifier;
            assert!(m.contains(Modifier::BOLD), "{spec}");
            assert!(!m.contains(Modifier::DIM), "{spec}");
        }
    }
    // A non-critical role may be dim and is not forced bold.
    let mut t = Theme::default();
    t.set_role("hint", "dim").unwrap();
    assert!(!t.get(Role::Hint).add_modifier.contains(Modifier::BOLD));
}
