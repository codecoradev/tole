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
mod tests;
