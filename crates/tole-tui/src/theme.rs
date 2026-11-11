//! Themes (#463 part 3): a plain map from semantic roles to a `ratatui`
//! [`Style`]. This module is pure data and parsing: it reads no file and no
//! environment (the host decides which theme to hand to the [`crate::App`]).
//!
//! Safety rules, enforced by construction rather than by review:
//! - a theme has NO background color: the only way to build a style here is
//!   [`parse_spec`], which yields a foreground color plus bold / italic /
//!   underline / dim, nothing else;
//! - there is no hidden / conceal / reverse / blink modifier in the
//!   vocabulary, so a theme cannot make text unreadable by itself;
//! - the *critical* roles ([`Role::is_critical`]: the approval prompt and
//!   notes, which carry errors) are never dim, and any custom style for them
//!   is forced bold;
//! - a foreground color is rejected only when it can never be valid.

use ratatui::style::{Color, Modifier, Style};

/// Every themable part of the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Echo of what the user sent.
    User,
    /// Plain answer text.
    Text,
    Reasoning,
    /// Tool call and result head lines.
    Tool,
    /// Continuation rows of a tool result that are not diff rows.
    ToolOutput,
    Heading,
    /// Fenced code lines.
    Code,
    /// Notes, which also carry errors. Critical.
    Note,
    DiffAdd,
    DiffRemove,
    /// `@@` / `+++` / `---` rows.
    DiffHunk,
    InlineCode,
    InlineBold,
    /// `queued:` and `image:` rows above the editor.
    Queue,
    /// Slash-command menu and `@` file panel rows.
    Menu,
    /// Rows of the model / session pickers.
    Picker,
    /// The dim key-hint lines under the live area.
    Hint,
    /// The rule above the editor.
    Border,
    Footer,
    /// The approval prompt line. Critical.
    Approval,
}

impl Role {
    pub const ALL: [Role; 20] = [
        Role::User,
        Role::Text,
        Role::Reasoning,
        Role::Tool,
        Role::ToolOutput,
        Role::Heading,
        Role::Code,
        Role::Note,
        Role::DiffAdd,
        Role::DiffRemove,
        Role::DiffHunk,
        Role::InlineCode,
        Role::InlineBold,
        Role::Queue,
        Role::Menu,
        Role::Picker,
        Role::Hint,
        Role::Border,
        Role::Footer,
        Role::Approval,
    ];

    /// The name used in the config (`[tui.colors]` keys).
    pub fn name(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Text => "text",
            Role::Reasoning => "reasoning",
            Role::Tool => "tool",
            Role::ToolOutput => "tool_output",
            Role::Heading => "heading",
            Role::Code => "code",
            Role::Note => "note",
            Role::DiffAdd => "diff_add",
            Role::DiffRemove => "diff_remove",
            Role::DiffHunk => "diff_hunk",
            Role::InlineCode => "inline_code",
            Role::InlineBold => "inline_bold",
            Role::Queue => "queue",
            Role::Menu => "menu",
            Role::Picker => "picker",
            Role::Hint => "hint",
            Role::Border => "border",
            Role::Footer => "footer",
            Role::Approval => "approval",
        }
    }

    pub fn from_name(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.name() == name)
    }

    /// Roles where the user must be able to act or must see an error: never
    /// dim, and bold in any custom style.
    pub fn is_critical(self) -> bool {
        matches!(self, Role::Note | Role::Approval)
    }

    fn index(self) -> usize {
        Role::ALL.iter().position(|r| *r == self).unwrap_or(0)
    }
}

/// Names of the built-in themes.
pub const BUILTIN: [&str; 3] = ["default", "light", "mono"];

/// A complete role -> style table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    styles: [Style; Role::ALL.len()],
}

impl Default for Theme {
    fn default() -> Self {
        Theme::builtin_default()
    }
}

fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

fn with(s: Style, m: Modifier) -> Style {
    s.add_modifier(m)
}

impl Theme {
    fn build(f: impl Fn(Role) -> Style) -> Theme {
        Theme {
            styles: Role::ALL.map(f),
        }
    }

    /// Exactly what `render.rs` drew before themes existed.
    pub fn builtin_default() -> Theme {
        Theme::build(|r| match r {
            Role::User => Style::new().add_modifier(Modifier::BOLD),
            Role::Text => Style::new(),
            Role::Reasoning => with(fg(Color::DarkGray), Modifier::DIM),
            Role::Tool => fg(Color::Cyan),
            Role::ToolOutput => with(fg(Color::Cyan), Modifier::DIM),
            Role::Heading => with(fg(Color::White), Modifier::BOLD),
            Role::Code => Style::new().add_modifier(Modifier::DIM),
            Role::Note => fg(Color::Yellow),
            Role::DiffAdd => fg(Color::Green),
            Role::DiffRemove => fg(Color::Red),
            Role::DiffHunk => Style::new().add_modifier(Modifier::DIM),
            Role::InlineCode => fg(Color::Cyan),
            Role::InlineBold => Style::new().add_modifier(Modifier::BOLD),
            Role::Queue => fg(Color::Magenta),
            Role::Menu => fg(Color::Cyan),
            Role::Picker => Style::new(),
            Role::Hint | Role::Border | Role::Footer => Style::new().add_modifier(Modifier::DIM),
            Role::Approval => fg(Color::Yellow),
        })
    }

    /// For a light terminal background: no yellow, white or cyan text, and
    /// nothing that depends on `dim` to be legible.
    pub fn builtin_light() -> Theme {
        let amber = Color::Indexed(130);
        Theme::build(|r| match r {
            Role::Reasoning => with(fg(Color::DarkGray), Modifier::ITALIC),
            Role::Tool | Role::InlineCode | Role::Menu => fg(Color::Blue),
            Role::ToolOutput => with(fg(Color::Blue), Modifier::DIM),
            Role::Heading => Style::new().add_modifier(Modifier::BOLD),
            Role::Note | Role::Approval => with(fg(amber), Modifier::BOLD),
            _ => Theme::builtin_default().get(r),
        })
    }

    /// No colors at all: only bold, dim, italic and underline.
    pub fn builtin_mono() -> Theme {
        let m = |m: Modifier| Style::new().add_modifier(m);
        Theme::build(|r| match r {
            Role::User | Role::InlineBold | Role::Note | Role::Approval => m(Modifier::BOLD),
            Role::Text | Role::Tool | Role::Menu | Role::Picker => Style::new(),
            Role::Reasoning => m(Modifier::DIM | Modifier::ITALIC),
            Role::ToolOutput | Role::Code | Role::DiffRemove => m(Modifier::DIM),
            Role::Heading => m(Modifier::BOLD | Modifier::UNDERLINED),
            Role::DiffAdd => m(Modifier::BOLD),
            Role::DiffHunk | Role::Queue => m(Modifier::ITALIC),
            Role::InlineCode => m(Modifier::UNDERLINED),
            Role::Hint | Role::Border | Role::Footer => m(Modifier::DIM),
        })
    }

    /// A built-in theme by name (case-insensitive).
    pub fn named(name: &str) -> Option<Theme> {
        match name.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Theme::builtin_default()),
            "light" => Some(Theme::builtin_light()),
            "mono" => Some(Theme::builtin_mono()),
            _ => None,
        }
    }

    pub fn get(&self, role: Role) -> Style {
        self.styles[role.index()]
    }

    /// Replace one role's style from a spec such as `"blue bold"` or
    /// `"#cc8800"`. The error names the role. Critical roles come out bold
    /// and never dim whatever the spec says.
    pub fn set_role(&mut self, role: &str, spec: &str) -> Result<(), String> {
        let r = Role::from_name(role).ok_or_else(|| {
            let names: Vec<&str> = Role::ALL.iter().map(|r| r.name()).collect();
            format!("unknown role `{role}` (roles: {})", names.join(", "))
        })?;
        let mut style = parse_spec(spec).map_err(|e| format!("role `{role}`: {e}"))?;
        if r.is_critical() {
            style.add_modifier.remove(Modifier::DIM);
            style = style.add_modifier(Modifier::BOLD);
        }
        self.styles[r.index()] = style;
        Ok(())
    }
}

/// `"[color] [bold] [italic] [underline] [dim]"` in any order, whitespace
/// separated; a color is a name, `0`-`255` or `#rrggbb`. At least one token.
pub fn parse_spec(spec: &str) -> Result<Style, String> {
    let mut style = Style::new();
    let mut color: Option<&str> = None;
    let mut any = false;
    for tok in spec.split_whitespace() {
        any = true;
        match tok.to_ascii_lowercase().as_str() {
            "bold" => style = style.add_modifier(Modifier::BOLD),
            "italic" => style = style.add_modifier(Modifier::ITALIC),
            "underline" | "underlined" => style = style.add_modifier(Modifier::UNDERLINED),
            "dim" => style = style.add_modifier(Modifier::DIM),
            _ => {
                if let Some(first) = color {
                    return Err(format!("two colors (`{first}` and `{tok}`)"));
                }
                style = style.fg(parse_color(tok)?);
                color = Some(tok);
            }
        }
    }
    if any {
        Ok(style)
    } else {
        Err("empty style (give a color and/or bold, italic, underline, dim)".into())
    }
}

/// A foreground color: a name, an index `0`-`255`, or `#rrggbb`.
pub fn parse_color(tok: &str) -> Result<Color, String> {
    let bad = |why: &str| {
        Err(format!(
            "invalid color `{tok}`: {why} (use a name such as red or lightblue, a number 0-255, \
             or #rrggbb)"
        ))
    };
    let lower = tok.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return bad("a hex color needs exactly six hex digits");
        }
        let n = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
        return Ok(Color::Rgb(n(0), n(2), n(4)));
    }
    if !lower.is_empty() && lower.bytes().all(|b| b.is_ascii_digit()) {
        return match lower.parse::<u8>() {
            Ok(i) => Ok(Color::Indexed(i)),
            Err(_) => bad("indexed colors go from 0 to 255"),
        };
    }
    let name: String = lower.chars().filter(|c| !matches!(c, '-' | '_')).collect();
    Ok(match name.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        "hidden" | "conceal" | "reverse" | "reversed" | "blink" => {
            return bad("themes cannot hide, reverse or blink text")
        }
        n if n.starts_with("bg") || n.starts_with("on") || n.contains([':', '=']) => {
            return bad("themes cannot set background colors")
        }
        _ => return bad("unknown color name"),
    })
}

#[cfg(test)]
mod tests {
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
}
