//! GUI colors. Terminal colors follow the wire encoding in
//! `protocol::color_to_u32`; UI chrome uses Catppuccin Mocha, Herdr's default theme.

use gpui::{rgb, Hsla};

use crate::api::schema::AgentStatus;

const ANSI: [u32; 16] = [
    0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de, 0x585b70,
    0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
];

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
    palette: [Hsla; 256],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            terminal_fg: hex(0xcdd6f4),
            terminal_bg: hex(0x1e1e2e),
            cursor: hex(0xf5e0dc),
            sidebar_bg: hex(0x181825),
            panel_bg: hex(0x11111b),
            border: hex(0x313244),
            text: hex(0xcdd6f4),
            text_muted: hex(0x7f849c),
            accent: hex(0x89b4fa),
            hover: hex(0x262637),
            selected: hex(0x313244),
            danger: hex(0xf38ba8),
            palette: xterm_palette(),
        }
    }
}

impl Theme {
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
            AgentStatus::Blocked => hex(0xf38ba8),
            AgentStatus::Working => hex(0xf9e2af),
            AgentStatus::Done => hex(0x89b4fa),
            AgentStatus::Idle => hex(0xa6e3a1),
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

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn xterm_palette() -> [Hsla; 256] {
    const LEVELS: [u32; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    let mut palette = [Hsla::default(); 256];
    for (index, slot) in palette.iter_mut().enumerate() {
        let value = match index {
            0..16 => ANSI[index],
            16..232 => {
                let cube = index - 16;
                (LEVELS[cube / 36] << 16) | (LEVELS[(cube / 6) % 6] << 8) | LEVELS[cube % 6]
            }
            _ => {
                let level = 8 + 10 * (index as u32 - 232);
                (level << 16) | (level << 8) | level
            }
        };
        *slot = hex(value);
    }
    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_colors_decode_by_tag() {
        let theme = Theme::default();
        assert_eq!(theme.decode_color(0x00_00_00_00), None);
        assert_eq!(theme.decode_color(0x00_00_00_02), Some(hex(ANSI[1])));
        assert_eq!(theme.decode_color(0x01_00_00_10), Some(hex(0x000000)));
        assert_eq!(theme.decode_color(0x01_00_00_E7), Some(hex(0xffffff)));
        assert_eq!(theme.decode_color(0x01_00_00_FF), Some(hex(0xeeeeee)));
        assert_eq!(theme.decode_color(0x02_12_34_56), Some(hex(0x123456)));
        assert_eq!(theme.decode_color(0x07_00_00_00), None);
    }
}
