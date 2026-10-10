//! GUI colors, built from herdr's own theme palettes (`[theme]` in config.toml) so the
//! desktop app and the terminal client share every built-in theme and custom override.
//! Terminal cell colors follow the wire encoding in `protocol::color_to_u32`.

use gpui::{rgb, Hsla};
use ratatui::style::Color;

use crate::api::schema::AgentStatus;
use crate::app::state::Palette;

#[derive(Clone)]
pub(crate) struct Theme {
    pub(crate) terminal_fg: Hsla,
    pub(crate) terminal_bg: Hsla,
    pub(crate) cursor: Hsla,
    pub(crate) sidebar_bg: Hsla,
    pub(crate) panel_bg: Hsla,
    pub(crate) border: Hsla,
    pub(crate) text: Hsla,
    pub(crate) text_muted: Hsla,
    pub(crate) accent: Hsla,
    pub(crate) hover: Hsla,
    pub(crate) selected: Hsla,
    pub(crate) danger: Hsla,
    idle: Hsla,
    working: Hsla,
    done: Hsla,
    palette: [Hsla; 256],
}

impl Default for Theme {
    fn default() -> Self {
        Self::from_palette(&Palette::catppuccin(), true)
    }
}

/// Standard xterm values for ratatui's named colors (Black..White).
const NAMED: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
    0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
];

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn xterm_256(index: u8) -> u32 {
    const LEVELS: [u32; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    match index {
        0..16 => NAMED[usize::from(index)],
        16..232 => {
            let cube = usize::from(index - 16);
            (LEVELS[cube / 36] << 16) | (LEVELS[(cube / 6) % 6] << 8) | LEVELS[cube % 6]
        }
        _ => {
            let level = 8 + 10 * (u32::from(index) - 232);
            (level << 16) | (level << 8) | level
        }
    }
}

/// A theme color, or `None` for `Reset` (the terminal's own default).
fn color(value: Color) -> Option<Hsla> {
    Some(match value {
        Color::Reset => return None,
        Color::Rgb(r, g, b) => hex((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)),
        Color::Indexed(index) => hex(xterm_256(index)),
        Color::Black => hex(NAMED[0]),
        Color::Red => hex(NAMED[1]),
        Color::Green => hex(NAMED[2]),
        Color::Yellow => hex(NAMED[3]),
        Color::Blue => hex(NAMED[4]),
        Color::Magenta => hex(NAMED[5]),
        Color::Cyan => hex(NAMED[6]),
        Color::Gray => hex(NAMED[7]),
        Color::DarkGray => hex(NAMED[8]),
        Color::LightRed => hex(NAMED[9]),
        Color::LightGreen => hex(NAMED[10]),
        Color::LightYellow => hex(NAMED[11]),
        Color::LightBlue => hex(NAMED[12]),
        Color::LightMagenta => hex(NAMED[13]),
        Color::LightCyan => hex(NAMED[14]),
        Color::White => hex(NAMED[15]),
    })
}

impl Theme {
    /// GUI colors for a herdr palette. `dark` picks fallbacks for colors a theme leaves
    /// to the terminal (`Reset`), as the `terminal` theme does.
    pub(crate) fn from_palette(palette: &Palette, dark: bool) -> Self {
        let (default_bg, default_fg) = if dark {
            (hex(0x1e1e1e), hex(0xd4d4d4))
        } else {
            (hex(0xffffff), hex(0x333333))
        };
        let pick = |value: Color, fallback: Hsla| color(value).unwrap_or(fallback);
        // Backgrounds only come from a theme's own RGB colors; named colors such as the
        // `terminal` theme's DarkGray are accents, not surfaces.
        let surface = |value: Color, fallback: Hsla| match value {
            Color::Rgb(..) => pick(value, fallback),
            _ => fallback,
        };
        let terminal_bg = surface(palette.surface_dim, default_bg);
        let text = pick(palette.text, default_fg);
        let panel_bg = surface(palette.panel_bg, terminal_bg);
        let border = pick(
            palette.surface0,
            pick(palette.surface1, default_fg.opacity(0.2)),
        );
        let muted = pick(palette.overlay1, default_fg.opacity(0.6));
        let red = pick(palette.red, hex(NAMED[9]));
        let green = pick(palette.green, hex(NAMED[2]));
        let yellow = pick(palette.yellow, hex(NAMED[3]));
        let blue = pick(palette.blue, hex(NAMED[4]));
        let magenta = pick(palette.mauve, hex(NAMED[5]));
        let cyan = pick(palette.teal, hex(NAMED[6]));

        // The 16 ANSI colors follow the theme so command output matches the chrome.
        let ansi = [
            pick(palette.surface1, hex(NAMED[0])),
            red,
            green,
            yellow,
            blue,
            magenta,
            cyan,
            pick(palette.subtext0, hex(NAMED[7])),
            pick(palette.overlay0, hex(NAMED[8])),
            red,
            green,
            yellow,
            blue,
            magenta,
            cyan,
            text,
        ];
        let mut colors = [Hsla::default(); 256];
        for (index, slot) in colors.iter_mut().enumerate() {
            *slot = match ansi.get(index) {
                Some(color) => *color,
                None => hex(xterm_256(index as u8)),
            };
        }

        Self {
            terminal_fg: text,
            terminal_bg,
            cursor: text,
            sidebar_bg: surface(palette.sidebar_bg, panel_bg),
            panel_bg,
            border,
            text,
            text_muted: muted,
            accent: pick(palette.accent, blue),
            hover: pick(palette.active_row_bg, border),
            selected: border,
            danger: red,
            idle: green,
            working: yellow,
            done: blue,
            palette: colors,
        }
    }

    /// Representative colors for a theme preview swatch.
    pub(crate) fn swatch(&self) -> [Hsla; 4] {
        [self.terminal_bg, self.accent, self.working, self.danger]
    }

    /// Decodes a packed wire color. `None` means the terminal default (Reset).
    pub(crate) fn decode_color(&self, value: u32) -> Option<Hsla> {
        match value >> 24 {
            0x00 => match value & 0xFF {
                // Named colors 1..=16 are Black..White in ANSI order.
                index @ 1..=16 => Some(self.palette[(index - 1) as usize]),
                _ => None,
            },
            0x01 => Some(self.palette[(value & 0xFF) as usize]),
            0x02 => Some(hex(value & 0x00FF_FFFF)),
            _ => None,
        }
    }

    pub(crate) fn status_color(&self, status: AgentStatus) -> Hsla {
        match status {
            AgentStatus::Blocked => self.danger,
            AgentStatus::Working => self.working,
            AgentStatus::Done => self.done,
            AgentStatus::Idle => self.idle,
            AgentStatus::Unknown => self.text_muted,
        }
    }
}

pub(crate) fn status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Blocked => "blocked",
        AgentStatus::Working => "working",
        AgentStatus::Done => "done",
        AgentStatus::Idle => "idle",
        AgentStatus::Unknown => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_colors_decode_by_tag() {
        let theme = Theme::default();
        let catppuccin = Palette::catppuccin();
        assert_eq!(theme.decode_color(0x00_00_00_00), None);
        assert_eq!(theme.decode_color(0x00_00_00_02), color(catppuccin.red));
        assert_eq!(theme.decode_color(0x01_00_00_10), Some(hex(0x000000)));
        assert_eq!(theme.decode_color(0x01_00_00_E7), Some(hex(0xffffff)));
        assert_eq!(theme.decode_color(0x01_00_00_FF), Some(hex(0xeeeeee)));
        assert_eq!(theme.decode_color(0x02_12_34_56), Some(hex(0x123456)));
        assert_eq!(theme.decode_color(0x07_00_00_00), None);
    }

    #[test]
    fn every_built_in_theme_maps_without_reset_holes() {
        for name in crate::config::THEME_NAMES {
            let palette = Palette::from_name(name).unwrap();
            for dark in [true, false] {
                let theme = Theme::from_palette(&palette, dark);
                assert_ne!(theme.terminal_bg, theme.terminal_fg, "{name}");
                assert_ne!(theme.text, theme.sidebar_bg, "{name}");
            }
        }
    }

    #[test]
    fn terminal_theme_falls_back_by_appearance() {
        let dark = Theme::from_palette(&Palette::terminal(), true);
        let light = Theme::from_palette(&Palette::terminal(), false);
        assert_eq!(dark.terminal_bg, hex(0x1e1e1e));
        assert_eq!(light.terminal_bg, hex(0xffffff));
    }
}
