//! Design tokens: the one place colours, sizes and fonts are defined.
//!
//! Values come from the dark tokens of the Design canvas
//! (docs/design/desktop/README.md). Plain data with no GUI types, so the UI
//! layer converts them in its adapter and every view reads names, not numbers.
//! A light theme is an open owner question (#1028): a second [`Palette`]
//! constant is all it needs.

/// A colour as `0xRRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u32);

impl Rgb {
    pub const fn hex(self) -> u32 {
        self.0
    }

    pub const fn r(self) -> u8 {
        (self.0 >> 16) as u8
    }

    pub const fn g(self) -> u8 {
        (self.0 >> 8) as u8
    }

    pub const fn b(self) -> u8 {
        self.0 as u8
    }
}

/// Semantic colour names. A view asks for "panel" or "text muted", never for
/// a hex value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Window background.
    pub background: Rgb,
    /// Sidebar and cards.
    pub panel: Rgb,
    /// Raised surfaces: selected rows, inputs, popovers.
    pub raised: Rgb,
    /// Borders and dividers.
    pub line: Rgb,
    /// Headings and primary text.
    pub text_primary: Rgb,
    /// Body text.
    pub text_secondary: Rgb,
    /// Secondary labels.
    pub text_muted: Rgb,
    /// Hints, section captions, disabled text.
    pub text_faint: Rgb,
    /// The teal accent: primary button, focus, "working".
    pub accent: Rgb,
    /// Done, connected.
    pub ok: Rgb,
    /// Needs the owner.
    pub amber: Rgb,
    /// Errors and refusals.
    pub danger: Rgb,
}

/// What a status colour says, so a state names the meaning and the palette
/// picks the colour (a light theme then changes one place).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Nothing is claimed: a preview, an idle or paused state.
    Neutral,
    /// Done, connected.
    Good,
    /// Waiting: something is starting, or the owner is asked.
    Attention,
    /// A failure or a refusal.
    Bad,
}

impl Palette {
    /// The colour of a [`Tone`].
    pub const fn tone(&self, tone: Tone) -> Rgb {
        match tone {
            Tone::Neutral => self.text_faint,
            Tone::Good => self.ok,
            Tone::Attention => self.amber,
            Tone::Bad => self.danger,
        }
    }
}

/// The dark palette (the only theme for now).
pub const DARK: Palette = Palette {
    background: Rgb(0x0F1113),
    panel: Rgb(0x15181B),
    raised: Rgb(0x1C2025),
    line: Rgb(0x262B30),
    text_primary: Rgb(0xE8EAED),
    text_secondary: Rgb(0xC9CED3),
    text_muted: Rgb(0xA8AFB7),
    text_faint: Rgb(0x8B939B),
    accent: Rgb(0x5CC8B0),
    ok: Rgb(0x5BC98A),
    amber: Rgb(0xE8B04A),
    danger: Rgb(0xF2A39B),
};

/// Lengths in logical pixels.
pub mod size {
    /// Sidebar width (board S2).
    pub const SIDEBAR_WIDTH: f32 = 264.0;
    /// Title bar height.
    pub const TOPBAR_HEIGHT: f32 = 40.0;
    /// Rows and buttons are about 34 to 38 px.
    pub const ROW_HEIGHT: f32 = 34.0;
    /// Space between a row and the edge of its panel.
    pub const GUTTER: f32 = 8.0;
    /// Gap between stacked rows.
    pub const ROW_GAP: f32 = 2.0;
    /// Widest a block of text in the main area grows.
    pub const CONTENT_MAX_WIDTH: f32 = 640.0;
    /// Space inside a panel.
    pub const PADDING: f32 = 16.0;
    /// Corner radius of rows and buttons.
    pub const RADIUS: f32 = 6.0;
    /// Status dot diameter.
    pub const DOT: f32 = 8.0;
    /// Icon drawn from plain boxes (the sidebar toggle).
    pub const ICON: f32 = 16.0;
    /// Height of the connection footer of the sidebar: two lines.
    pub const FOOTER_HEIGHT: f32 = 44.0;
    /// Widest the connection label of the header grows before it is cut.
    pub const HEADER_LABEL_MAX_WIDTH: f32 = 220.0;
    /// Widest a folder name grows in a chip before it is cut.
    pub const CHIP_MAX_WIDTH: f32 = 180.0;
    /// First window size.
    pub const WINDOW_WIDTH: f32 = 1200.0;
    pub const WINDOW_HEIGHT: f32 = 780.0;
}

/// Type sizes in logical pixels.
pub mod font {
    pub const TITLE: f32 = 26.0;
    pub const BODY: f32 = 14.0;
    pub const CAPTION: f32 = 11.0;

    /// Interface font.
    pub const FAMILY_UI: &str = "Inter";
    /// Paths, commands and other technical detail.
    pub const FAMILY_MONO: &str = "JetBrains Mono";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_split_the_hex_value() {
        let c = Rgb(0x5CC8B0);
        assert_eq!((c.r(), c.g(), c.b()), (0x5C, 0xC8, 0xB0));
        assert_eq!(c.hex(), 0x5CC8B0);
    }

    #[test]
    fn dark_palette_matches_the_design_canvas() {
        assert_eq!(DARK.background.hex(), 0x0F1113);
        assert_eq!(DARK.panel.hex(), 0x15181B);
        assert_eq!(DARK.raised.hex(), 0x1C2025);
        assert_eq!(DARK.line.hex(), 0x262B30);
        assert_eq!(DARK.accent.hex(), 0x5CC8B0);
        assert_eq!(DARK.danger.hex(), 0xF2A39B);
    }

    #[test]
    fn every_tone_has_its_own_colour_and_neutral_is_visible_on_the_panel() {
        let tones = [Tone::Neutral, Tone::Good, Tone::Attention, Tone::Bad];
        for (i, a) in tones.iter().enumerate() {
            assert!(tones[i + 1..]
                .iter()
                .all(|b| DARK.tone(*a) != DARK.tone(*b)));
            assert_ne!(DARK.tone(*a), DARK.panel);
        }
    }

    #[test]
    fn surfaces_get_lighter_from_background_to_raised() {
        let lum = |c: Rgb| u32::from(c.r()) + u32::from(c.g()) + u32::from(c.b());
        assert!(lum(DARK.background) < lum(DARK.panel));
        assert!(lum(DARK.panel) < lum(DARK.raised));
        assert!(lum(DARK.raised) < lum(DARK.line));
    }
}
