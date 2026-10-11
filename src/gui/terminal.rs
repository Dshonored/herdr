//! Terminal surface drawing and input translation.
//!
//! The server renders each pane into cells; this module paints those cells with
//! GPUI and turns window key/mouse events into the same semantic pane input the
//! TUI client sends.

use gpui::{
    fill, point, px, size, App, Bounds, Font, FontStyle, FontWeight, Hsla, Keystroke, Pixels,
    SharedString, StrikethroughStyle, TextRun, UnderlineStyle, Window,
};
use unicode_width::UnicodeWidthStr as _;

use crate::protocol::{
    CellData, ClientKeyCode, ClientKeyKind, ClientMouseButton, ClientMouseKind,
    ClientMousePosition, ClientPaneInputEvent, CursorState, FrameData, PaneSurfacePane,
    SurfaceRect,
};

use super::theme::Theme;

// ratatui modifier bits carried in `CellData::modifier`.
const MOD_BOLD: u16 = 1 << 0;
const MOD_DIM: u16 = 1 << 1;
const MOD_ITALIC: u16 = 1 << 2;
const MOD_UNDERLINED: u16 = 1 << 3;
const MOD_REVERSED: u16 = 1 << 6;
const MOD_HIDDEN: u16 = 1 << 7;
const MOD_CROSSED_OUT: u16 = 1 << 8;
const UNDERLINE_STYLE_MASK: u16 = 0xF000;
const UNDERLINE_STYLE_CURLY: u16 = 3;

// crossterm `KeyModifiers` bits used by `ClientPaneInputEvent`.
const KEY_SHIFT: u8 = 0b0001;
const KEY_CONTROL: u8 = 0b0010;
const KEY_ALT: u8 = 0b0100;
const KEY_SUPER: u8 = 0b1000;

/// The platform's default monospace family. Both branches compile everywhere.
pub(crate) fn default_font_family() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CellMetrics {
    pub(crate) font: Font,
    pub(crate) font_size: Pixels,
    pub(crate) cell_width: Pixels,
    pub(crate) line_height: Pixels,
}

impl CellMetrics {
    pub(crate) fn measure(window: &Window, font: Font, font_size: Pixels) -> Self {
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font);
        let cell_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|advance| advance.width)
            .unwrap_or(font_size * 0.6);
        Self {
            font,
            font_size,
            cell_width,
            line_height: (font_size * 1.2).round(),
        }
    }

    /// Whole cells that fit in `bounds`.
    pub(crate) fn grid_size(&self, bounds: Bounds<Pixels>) -> (u16, u16) {
        let cols = (bounds.size.width / self.cell_width).floor().max(1.0);
        let rows = (bounds.size.height / self.line_height).floor().max(1.0);
        (
            cols.min(f32::from(u16::MAX)) as u16,
            rows.min(f32::from(u16::MAX)) as u16,
        )
    }

    /// Cell under a window position, relative to the surface origin.
    pub(crate) fn cell_at(
        &self,
        origin: gpui::Point<Pixels>,
        position: gpui::Point<Pixels>,
    ) -> (u16, u16) {
        let x = ((position.x - origin.x) / self.cell_width).floor().max(0.0);
        let y = ((position.y - origin.y) / self.line_height)
            .floor()
            .max(0.0);
        (
            x.min(f32::from(u16::MAX)) as u16,
            y.min(f32::from(u16::MAX)) as u16,
        )
    }

    pub(crate) fn pixel_size(&self) -> (u32, u32) {
        (
            f32::from(self.cell_width).round() as u32,
            f32::from(self.line_height).round() as u32,
        )
    }
}

#[derive(Clone, Copy, PartialEq)]
struct CellStyle {
    fg: Hsla,
    bg: Option<Hsla>,
    bold: bool,
    italic: bool,
    underline: Option<bool>,
    strikethrough: bool,
}

fn cell_style(cell: &CellData, theme: &Theme) -> CellStyle {
    let mut fg = theme.decode_color(cell.fg).unwrap_or(theme.terminal_fg);
    let mut bg = theme.decode_color(cell.bg);
    if cell.modifier & MOD_REVERSED != 0 {
        let swapped_bg = Some(fg);
        fg = bg.unwrap_or(theme.terminal_bg);
        bg = swapped_bg;
    }
    if cell.modifier & MOD_DIM != 0 {
        fg.a *= 0.6;
    }
    if cell.modifier & MOD_HIDDEN != 0 {
        fg = bg.unwrap_or(theme.terminal_bg);
    }
    let underline_style = (cell.modifier & UNDERLINE_STYLE_MASK) >> 12;
    CellStyle {
        fg,
        bg,
        bold: cell.modifier & MOD_BOLD != 0,
        italic: cell.modifier & MOD_ITALIC != 0,
        underline: (cell.modifier & MOD_UNDERLINED != 0 || underline_style != 0)
            .then_some(underline_style == UNDERLINE_STYLE_CURLY),
        strikethrough: cell.modifier & MOD_CROSSED_OUT != 0,
    }
}

fn text_run(len: usize, style: CellStyle, metrics: &CellMetrics) -> TextRun {
    let mut font = metrics.font.clone();
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    TextRun {
        len,
        font,
        color: style.fg,
        background_color: None,
        underline: style.underline.map(|wavy| UnderlineStyle {
            thickness: px(1.0),
            color: Some(style.fg),
            wavy,
        }),
        strikethrough: style.strikethrough.then(|| StrikethroughStyle {
            thickness: px(1.0),
            color: Some(style.fg),
        }),
    }
}

/// A horizontal run of text drawn with one shaped line.
struct Segment {
    col: u16,
    text: String,
    runs: Vec<TextRun>,
    /// Wide glyphs are shaped on their own so they keep their natural width.
    fixed_width: bool,
}

/// Text selected by dragging inside one pane, in surface cells. Selection runs like
/// text: from the anchor to the end of its line, whole lines between, then up to the head.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Selection {
    pub(crate) pane_id: String,
    area: SurfaceRect,
    anchor: (u16, u16),
    head: (u16, u16),
}

impl Selection {
    pub(crate) fn start(pane: &PaneSurfacePane, col: u16, row: u16) -> Self {
        let area = pane.inner_rect;
        let point = clamp_to(area, col, row);
        Self {
            pane_id: pane.pane_id.clone(),
            area,
            anchor: point,
            head: point,
        }
    }

    /// The word under a cell (double-click): a run of non-blank, non-bracket characters.
    pub(crate) fn word(pane: &PaneSurfacePane, frame: &FrameData, col: u16, row: u16) -> Self {
        let mut selection = Self::start(pane, col, row);
        let (col, row) = selection.anchor;
        let in_word =
            |col: u16| {
                let index = usize::from(row) * usize::from(frame.width) + usize::from(col);
                frame.cells.get(index).is_some_and(|cell| {
                    cell.skip
                        || cell.symbol.chars().next().is_some_and(|ch| {
                            !ch.is_whitespace() && !"()[]{}<>'\"`,;|".contains(ch)
                        })
                })
            };
        if !in_word(col) {
            return selection;
        }
        let left = selection.area.x;
        let right = left + selection.area.width.saturating_sub(1);
        let mut first = col;
        while first > left && in_word(first - 1) {
            first -= 1;
        }
        let mut last = col;
        while last < right && in_word(last + 1) {
            last += 1;
        }
        selection.anchor = (first, row);
        selection.head = (last, row);
        selection
    }

    /// The pane's whole row under a cell (triple-click).
    pub(crate) fn line(pane: &PaneSurfacePane, col: u16, row: u16) -> Self {
        let mut selection = Self::start(pane, col, row);
        let row = selection.anchor.1;
        selection.anchor = (selection.area.x, row);
        selection.head = (
            selection.area.x + selection.area.width.saturating_sub(1),
            row,
        );
        selection
    }

    /// Everything visible in the pane (Cmd+A).
    pub(crate) fn all(pane: &PaneSurfacePane) -> Self {
        let area = pane.inner_rect;
        let mut selection = Self::start(pane, area.x, area.y);
        selection.head = clamp_to(area, u16::MAX, u16::MAX);
        selection
    }

    pub(crate) fn extend(&mut self, col: u16, row: u16) {
        self.head = clamp_to(self.area, col, row);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// `(row, first column, end column exclusive)` for each selected row.
    pub(crate) fn row_spans(&self) -> Vec<(u16, u16, u16)> {
        let (start, end) = {
            let (a, h) = (self.anchor, self.head);
            if (a.1, a.0) <= (h.1, h.0) {
                (a, h)
            } else {
                (h, a)
            }
        };
        let left = self.area.x;
        let right = self.area.x.saturating_add(self.area.width);
        (start.1..=end.1)
            .map(|row| {
                let first = if row == start.1 { start.0 } else { left };
                let last = if row == end.1 { end.0 + 1 } else { right };
                (row, first, last.max(first))
            })
            .collect()
    }

    /// The selected text, one line per row with trailing blanks trimmed.
    pub(crate) fn text(&self, frame: &FrameData) -> String {
        let width = usize::from(frame.width);
        self.row_spans()
            .into_iter()
            .map(|(row, first, last)| {
                let start = usize::from(row) * width;
                let mut line = String::new();
                for col in first..last.min(frame.width) {
                    let Some(cell) = frame.cells.get(start + usize::from(col)) else {
                        break;
                    };
                    // Wide characters occupy a second, skipped cell.
                    if cell.skip {
                        continue;
                    }
                    line.push_str(if cell.symbol.is_empty() {
                        " "
                    } else {
                        &cell.symbol
                    });
                }
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The link under a surface cell: its OSC 8 target, or a plain `http(s)://` URL in the
/// row's text.
pub(crate) fn link_at(frame: &FrameData, col: u16, row: u16) -> Option<String> {
    static URL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"https?://[^\s<>"'`]+"#).expect("valid URL pattern")
    });
    if col >= frame.width || row >= frame.height {
        return None;
    }
    let width = usize::from(frame.width);
    let cells = frame.cells.get(usize::from(row) * width..)?.get(..width)?;
    if let Some(link) = cells[usize::from(col)]
        .hyperlink
        .and_then(|index| frame.hyperlinks.get(index as usize))
    {
        return Some(link.clone());
    }
    // Rebuild the row's text, remembering which column each byte came from.
    let mut text = String::new();
    let mut byte_cols = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        if cell.skip {
            continue;
        }
        let symbol = if cell.symbol.is_empty() {
            " "
        } else {
            &cell.symbol
        };
        byte_cols.extend(std::iter::repeat_n(index, symbol.len()));
        text.push_str(symbol);
    }
    let col = usize::from(col);
    URL.find_iter(&text).find_map(|found| {
        let url = found
            .as_str()
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
        let first = byte_cols[found.start()];
        let last = byte_cols[found.start() + url.len() - 1];
        (first..=last).contains(&col).then(|| url.to_owned())
    })
}

fn clamp_to(area: SurfaceRect, col: u16, row: u16) -> (u16, u16) {
    let last_col = area.x.saturating_add(area.width.saturating_sub(1));
    let last_row = area.y.saturating_add(area.height.saturating_sub(1));
    (col.clamp(area.x, last_col), row.clamp(area.y, last_row))
}

/// Paints one server-rendered frame at `bounds.origin`.
pub(crate) fn paint_frame(
    bounds: Bounds<Pixels>,
    frame: &FrameData,
    panes: &[PaneSurfacePane],
    selection: Option<&Selection>,
    metrics: &CellMetrics,
    theme: &Theme,
    window_focused: bool,
    window: &mut Window,
    cx: &mut App,
) {
    window.paint_quad(fill(bounds, theme.terminal_bg));
    let origin = bounds.origin;
    let width = usize::from(frame.width);
    let grid = CellGrid {
        origin,
        cell_width: metrics.cell_width,
        line_height: metrics.line_height,
        scale: window.scale_factor(),
    };

    for y in 0..frame.height {
        let row_start = usize::from(y) * width;
        let Some(row) = frame.cells.get(row_start..row_start + width) else {
            break;
        };
        let top = origin.y + metrics.line_height * f32::from(y);

        // Backgrounds: merge equal neighbours into one quad.
        let mut x = 0usize;
        while x < row.len() {
            let style = cell_style(&row[x], theme);
            let mut end = x + 1;
            while end < row.len() && cell_style(&row[end], theme).bg == style.bg {
                end += 1;
            }
            if let Some(bg) = style.bg {
                window.paint_quad(fill(grid.cells(x as u16, end as u16, y), bg));
            }
            x = end;
        }

        if let Some(selection) = selection {
            let highlight = Hsla {
                a: 0.3,
                ..theme.terminal_fg
            };
            for (_, first, last) in selection.row_spans().into_iter().filter(|span| span.0 == y) {
                window.paint_quad(fill(grid.cells(first, last, y), highlight));
            }
        }

        let (segments, shapes) = row_segments(row, metrics, theme);
        for (col, shape, color) in shapes {
            paint_shape(&grid, col, y, &shape, color, window);
        }
        for segment in segments {
            let shaped = window.text_system().shape_line(
                SharedString::from(segment.text),
                metrics.font_size,
                &segment.runs,
                segment.fixed_width.then_some(metrics.cell_width),
            );
            let left = origin.x + metrics.cell_width * f32::from(segment.col);
            let _ = shaped.paint(point(left, top), metrics.line_height, window, cx);
        }
    }

    if let Some(cursor) = &frame.cursor {
        let focused_pane = panes.iter().any(|pane| pane.focused);
        if cursor.visible && focused_pane {
            paint_cursor(origin, cursor, metrics, theme, window_focused, window);
        }
    }
}

type RowShapes = Vec<(u16, CellShape, Hsla)>;

fn row_segments(
    row: &[CellData],
    metrics: &CellMetrics,
    theme: &Theme,
) -> (Vec<Segment>, RowShapes) {
    let mut segments: Vec<Segment> = Vec::new();
    let mut shapes: RowShapes = Vec::new();
    let mut current: Option<(Segment, Option<CellStyle>)> = None;
    let mut x = 0usize;
    while x < row.len() {
        let cell = &row[x];
        let symbol = if cell.skip || cell.symbol.is_empty() {
            " "
        } else {
            cell.symbol.as_str()
        };
        let wide = symbol.width() > 1;
        let style = cell_style(cell, theme);

        let mut chars = symbol.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            if let Some(shape) = cell_shape(ch) {
                if let Some((segment, _)) = current.take() {
                    segments.push(segment);
                }
                shapes.push((x as u16, shape, style.fg));
                x += 1;
                continue;
            }
        }

        if wide {
            if let Some((segment, _)) = current.take() {
                segments.push(segment);
            }
            segments.push(Segment {
                col: x as u16,
                text: symbol.to_owned(),
                runs: vec![text_run(symbol.len(), style, metrics)],
                fixed_width: false,
            });
            // The following cell is the wide glyph's continuation.
            x += 2;
            continue;
        }

        let (segment, last_style) = current.get_or_insert_with(|| {
            (
                Segment {
                    col: x as u16,
                    text: String::new(),
                    runs: Vec::new(),
                    fixed_width: true,
                },
                None,
            )
        });
        segment.text.push_str(symbol);
        match (&*last_style, segment.runs.last_mut()) {
            (Some(previous), Some(run)) if *previous == style => run.len += symbol.len(),
            _ => segment.runs.push(text_run(symbol.len(), style, metrics)),
        }
        *last_style = Some(style);
        x += 1;
    }
    if let Some((segment, _)) = current {
        segments.push(segment);
    }
    segments.retain(|segment| !segment.text.trim().is_empty() || has_decoration(segment));
    (segments, shapes)
}

/// Cell geometry snapped to device pixels so neighbouring cells meet without seams.
struct CellGrid {
    origin: gpui::Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    scale: f32,
}

impl CellGrid {
    fn snap(&self, value: Pixels) -> Pixels {
        (value * self.scale).round() / self.scale
    }

    fn x(&self, col: f32) -> Pixels {
        self.snap(self.origin.x + self.cell_width * col)
    }

    fn y(&self, row: f32) -> Pixels {
        self.snap(self.origin.y + self.line_height * row)
    }

    /// Columns `start..end` of `row`.
    fn cells(&self, start: u16, end: u16, row: u16) -> Bounds<Pixels> {
        let row = f32::from(row);
        Bounds::from_corners(
            point(self.x(f32::from(start)), self.y(row)),
            point(self.x(f32::from(end)), self.y(row + 1.0)),
        )
    }

    /// Light box-drawing stroke width: about an eighth of a cell, at least one device pixel.
    fn stroke(&self) -> Pixels {
        let device_pixel = px(1.0 / self.scale);
        self.snap(self.cell_width / 8.0).max(device_pixel)
    }
}

/// A character drawn as geometry instead of a font glyph, so it tiles exactly.
#[derive(Clone, Debug, PartialEq)]
enum CellShape {
    /// Filled cell fractions as `(x0, y0, x1, y1)` in 0..=1, with an opacity.
    Blocks(Vec<(f32, f32, f32, f32)>, f32),
    /// Line weight towards up, right, down, left: 0 none, 1 light, 2 heavy, 3 double.
    Lines([u8; 4]),
}

const UL: (f32, f32, f32, f32) = (0.0, 0.0, 0.5, 0.5);
const UR: (f32, f32, f32, f32) = (0.5, 0.0, 1.0, 0.5);
const LL: (f32, f32, f32, f32) = (0.0, 0.5, 0.5, 1.0);
const LR: (f32, f32, f32, f32) = (0.5, 0.5, 1.0, 1.0);

fn cell_shape(ch: char) -> Option<CellShape> {
    let blocks = |rects: &[(f32, f32, f32, f32)]| Some(CellShape::Blocks(rects.to_vec(), 1.0));
    let code = ch as u32;
    match code {
        0x2580 => blocks(&[(0.0, 0.0, 1.0, 0.5)]),
        0x2581..=0x2587 => {
            let height = (code - 0x2580) as f32 / 8.0;
            blocks(&[(0.0, 1.0 - height, 1.0, 1.0)])
        }
        0x2588 => blocks(&[(0.0, 0.0, 1.0, 1.0)]),
        0x2589..=0x258F => {
            let width = (8 - (code - 0x2588)) as f32 / 8.0;
            blocks(&[(0.0, 0.0, width, 1.0)])
        }
        0x2590 => blocks(&[(0.5, 0.0, 1.0, 1.0)]),
        0x2591..=0x2593 => Some(CellShape::Blocks(
            vec![(0.0, 0.0, 1.0, 1.0)],
            (code - 0x2590) as f32 * 0.25,
        )),
        0x2594 => blocks(&[(0.0, 0.0, 1.0, 0.125)]),
        0x2595 => blocks(&[(0.875, 0.0, 1.0, 1.0)]),
        0x2596 => blocks(&[LL]),
        0x2597 => blocks(&[LR]),
        0x2598 => blocks(&[UL]),
        0x2599 => blocks(&[UL, LL, LR]),
        0x259A => blocks(&[UL, LR]),
        0x259B => blocks(&[UL, UR, LL]),
        0x259C => blocks(&[UL, UR, LR]),
        0x259D => blocks(&[UR]),
        0x259E => blocks(&[UR, LL]),
        0x259F => blocks(&[UR, LL, LR]),
        _ => box_lines(ch).map(CellShape::Lines),
    }
}

fn box_lines(ch: char) -> Option<[u8; 4]> {
    Some(match ch {
        '\u{2500}' => [0, 1, 0, 1],
        '\u{2501}' => [0, 2, 0, 2],
        '\u{2502}' => [1, 0, 1, 0],
        '\u{2503}' => [2, 0, 2, 0],
        '\u{250C}' | '\u{256D}' => [0, 1, 1, 0],
        '\u{250F}' => [0, 2, 2, 0],
        '\u{2510}' | '\u{256E}' => [0, 0, 1, 1],
        '\u{2513}' => [0, 0, 2, 2],
        '\u{2514}' | '\u{2570}' => [1, 1, 0, 0],
        '\u{2517}' => [2, 2, 0, 0],
        '\u{2518}' | '\u{256F}' => [1, 0, 0, 1],
        '\u{251B}' => [2, 0, 0, 2],
        '\u{251C}' => [1, 1, 1, 0],
        '\u{2523}' => [2, 2, 2, 0],
        '\u{2524}' => [1, 0, 1, 1],
        '\u{252B}' => [2, 0, 2, 2],
        '\u{252C}' => [0, 1, 1, 1],
        '\u{2533}' => [0, 2, 2, 2],
        '\u{2534}' => [1, 1, 0, 1],
        '\u{253B}' => [2, 2, 0, 2],
        '\u{253C}' => [1, 1, 1, 1],
        '\u{254B}' => [2, 2, 2, 2],
        '\u{2550}' => [0, 3, 0, 3],
        '\u{2551}' => [3, 0, 3, 0],
        '\u{2554}' => [0, 3, 3, 0],
        '\u{2557}' => [0, 0, 3, 3],
        '\u{255A}' => [3, 3, 0, 0],
        '\u{255D}' => [3, 0, 0, 3],
        '\u{2560}' => [3, 3, 3, 0],
        '\u{2563}' => [3, 0, 3, 3],
        '\u{2566}' => [0, 3, 3, 3],
        '\u{2569}' => [3, 3, 0, 3],
        '\u{256C}' => [3, 3, 3, 3],
        '\u{2574}' => [0, 0, 0, 1],
        '\u{2575}' => [1, 0, 0, 0],
        '\u{2576}' => [0, 1, 0, 0],
        '\u{2577}' => [0, 0, 1, 0],
        '\u{2578}' => [0, 0, 0, 2],
        '\u{2579}' => [2, 0, 0, 0],
        '\u{257A}' => [0, 2, 0, 0],
        '\u{257B}' => [0, 0, 2, 0],
        _ => return None,
    })
}

fn paint_shape(
    grid: &CellGrid,
    col: u16,
    row: u16,
    shape: &CellShape,
    color: Hsla,
    window: &mut Window,
) {
    let (col, row) = (f32::from(col), f32::from(row));
    match shape {
        CellShape::Blocks(rects, opacity) => {
            let mut color = color;
            color.a *= opacity;
            for &(fx0, fy0, fx1, fy1) in rects {
                window.paint_quad(fill(
                    Bounds::from_corners(
                        point(grid.x(col + fx0), grid.y(row + fy0)),
                        point(grid.x(col + fx1), grid.y(row + fy1)),
                    ),
                    color,
                ));
            }
        }
        CellShape::Lines(arms) => {
            let (x0, x1) = (grid.x(col), grid.x(col + 1.0));
            let (y0, y1) = (grid.y(row), grid.y(row + 1.0));
            let light = grid.stroke();
            // Top-left corner of a light stroke through the cell centre.
            let cx = grid.snap((x0 + x1 - light) / 2.0);
            let cy = grid.snap((y0 + y1 - light) / 2.0);
            let [up, right, down, left] = *arms;
            for (weight, arm) in [(up, 0), (right, 1), (down, 2), (left, 3)] {
                if weight == 0 {
                    continue;
                }
                // Double lines are two light strokes either side of the centre line.
                let strokes: &[(f32, f32)] = match weight {
                    2 => &[(-0.5, 2.0)],
                    3 => &[(-1.0, 1.0), (1.0, 1.0)],
                    _ => &[(0.0, 1.0)],
                };
                for &(offset, scale) in strokes {
                    let width = light * scale;
                    let shift = light * offset;
                    // Arms overlap the centre square so joints and corners are filled.
                    let bounds = match arm {
                        0 => Bounds::from_corners(
                            point(cx + shift, y0),
                            point(cx + shift + width, cy + light * 2.0),
                        ),
                        2 => Bounds::from_corners(
                            point(cx + shift, cy - light),
                            point(cx + shift + width, y1),
                        ),
                        3 => Bounds::from_corners(
                            point(x0, cy + shift),
                            point(cx + light * 2.0, cy + shift + width),
                        ),
                        _ => Bounds::from_corners(
                            point(cx - light, cy + shift),
                            point(x1, cy + shift + width),
                        ),
                    };
                    window.paint_quad(fill(bounds, color));
                }
            }
        }
    }
}

fn has_decoration(segment: &Segment) -> bool {
    segment
        .runs
        .iter()
        .any(|run| run.underline.is_some() || run.strikethrough.is_some())
}

fn paint_cursor(
    origin: gpui::Point<Pixels>,
    cursor: &CursorState,
    metrics: &CellMetrics,
    theme: &Theme,
    window_focused: bool,
    window: &mut Window,
) {
    let cell_origin = point(
        origin.x + metrics.cell_width * f32::from(cursor.x),
        origin.y + metrics.line_height * f32::from(cursor.y),
    );
    let color = theme.cursor;
    let bounds = match cursor.shape {
        3 | 4 => Bounds::new(
            point(cell_origin.x, cell_origin.y + metrics.line_height - px(2.0)),
            size(metrics.cell_width, px(2.0)),
        ),
        5 | 6 => Bounds::new(cell_origin, size(px(2.0), metrics.line_height)),
        _ => Bounds::new(cell_origin, size(metrics.cell_width, metrics.line_height)),
    };
    if window_focused {
        let mut color = color;
        if cursor.shape <= 2 {
            color.a = 0.55;
        }
        window.paint_quad(fill(bounds, color));
    } else {
        window.paint_quad(gpui::outline(
            Bounds::new(cell_origin, size(metrics.cell_width, metrics.line_height)),
            color,
            gpui::BorderStyle::Solid,
        ));
    }
}

/// The pane whose rectangle contains a surface cell.
pub(crate) fn pane_at(panes: &[PaneSurfacePane], col: u16, row: u16) -> Option<&PaneSurfacePane> {
    panes.iter().find(|pane| {
        let rect = pane.rect;
        col >= rect.x
            && col < rect.x.saturating_add(rect.width)
            && row >= rect.y
            && row < rect.y.saturating_add(rect.height)
    })
}

pub(crate) fn key_modifiers(modifiers: &gpui::Modifiers) -> u8 {
    let mut bits = 0;
    if modifiers.shift {
        bits |= KEY_SHIFT;
    }
    if modifiers.control {
        bits |= KEY_CONTROL;
    }
    if modifiers.alt {
        bits |= KEY_ALT;
    }
    if modifiers.platform {
        bits |= KEY_SUPER;
    }
    bits
}

/// Translates one window key press into semantic pane input.
pub(crate) fn key_input(keystroke: &Keystroke, repeat: bool) -> Option<ClientPaneInputEvent> {
    let modifiers = &keystroke.modifiers;
    let code = match keystroke.key.as_str() {
        "enter" => ClientKeyCode::Enter,
        "escape" => ClientKeyCode::Esc,
        "backspace" => ClientKeyCode::Backspace,
        "tab" if modifiers.shift => ClientKeyCode::BackTab,
        "tab" => ClientKeyCode::Tab,
        "left" => ClientKeyCode::Left,
        "right" => ClientKeyCode::Right,
        "up" => ClientKeyCode::Up,
        "down" => ClientKeyCode::Down,
        "home" => ClientKeyCode::Home,
        "end" => ClientKeyCode::End,
        "pageup" => ClientKeyCode::PageUp,
        "pagedown" => ClientKeyCode::PageDown,
        "delete" => ClientKeyCode::Delete,
        "insert" => ClientKeyCode::Insert,
        "space" => ClientKeyCode::Char(' '),
        key => {
            if let Some(number) = key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                ClientKeyCode::F(number)
            } else {
                let mut chars = key.chars();
                match (chars.next(), chars.next()) {
                    (Some(ch), None) => ClientKeyCode::Char(ch),
                    // Named keys this client does not map (e.g. media keys).
                    _ => {
                        return keystroke
                            .key_char
                            .clone()
                            .filter(|text| !text.is_empty())
                            .map(ClientPaneInputEvent::TextCommit);
                    }
                }
            }
        }
    };

    let chorded = modifiers.control || modifiers.alt || modifiers.platform;
    let generated_text = match &code {
        ClientKeyCode::Char(_) if !chorded => keystroke.key_char.clone(),
        _ => None,
    };
    // A printable key from a layout whose text differs from its ASCII key name
    // (or a multi-char IME result) is committed as text.
    if let Some(text) = &generated_text {
        if text.chars().count() > 1 {
            return Some(ClientPaneInputEvent::TextCommit(text.clone()));
        }
    }
    let code = match (&code, &generated_text) {
        (ClientKeyCode::Char(_), Some(text)) => {
            text.chars().next().map(ClientKeyCode::Char).unwrap_or(code)
        }
        _ => code,
    };
    let mut bits = key_modifiers(modifiers);
    if code == ClientKeyCode::BackTab {
        bits |= KEY_SHIFT;
    }
    Some(ClientPaneInputEvent::Key {
        code,
        modifiers: bits,
        kind: if repeat {
            ClientKeyKind::Repeat
        } else {
            ClientKeyKind::Press
        },
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text,
        tracks_release: false,
        physical_key_id: None,
        windows_record: None,
    })
}

fn editing_key(code: ClientKeyCode, modifiers: u8) -> ClientPaneInputEvent {
    ClientPaneInputEvent::Key {
        code,
        modifiers,
        kind: ClientKeyKind::Press,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: false,
        physical_key_id: None,
        windows_record: None,
    }
}

/// macOS text-editing shortcuts translated into keys shells, readline, and agent CLIs
/// understand, as iTerm2 and Ghostty do. Programs never see Cmd, and treat Option+arrow
/// inconsistently, so these are rewritten before they reach the pane.
///
/// | Shortcut         | Sends    | Action                        |
/// | ---------------- | -------- | ----------------------------- |
/// | Cmd+Backspace    | Ctrl+U   | delete to start of line       |
/// | Cmd+Delete       | Ctrl+K   | delete to end of line         |
/// | Option+Backspace | Ctrl+W   | delete previous word          |
/// | Option+Delete    | Option+D | delete next word              |
/// | Cmd+Left         | Ctrl+A   | start of line                 |
/// | Cmd+Right        | Ctrl+E   | end of line                   |
/// | Option+Left      | Option+B | previous word                 |
/// | Option+Right     | Option+F | next word                     |
/// | Cmd+Z            | Ctrl+_   | undo (readline, zsh)          |
/// | Cmd+Shift+Z      | Option+_ | redo where bound (zsh)        |
pub(crate) fn editing_shortcut(keystroke: &Keystroke) -> Option<ClientPaneInputEvent> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let modifiers = &keystroke.modifiers;
    if modifiers.control {
        return None;
    }
    let command = modifiers.platform && !modifiers.alt;
    let option = modifiers.alt && !modifiers.platform;
    let ctrl = |ch| Some(editing_key(ClientKeyCode::Char(ch), KEY_CONTROL));
    let alt = |ch| Some(editing_key(ClientKeyCode::Char(ch), KEY_ALT));
    match (keystroke.key.as_str(), command, option, modifiers.shift) {
        ("backspace", true, _, false) => ctrl('u'),
        ("delete", true, _, false) => ctrl('k'),
        ("backspace", _, true, false) => ctrl('w'),
        ("delete", _, true, false) => alt('d'),
        ("left", true, _, false) => ctrl('a'),
        ("right", true, _, false) => ctrl('e'),
        ("left", _, true, false) => alt('b'),
        ("right", _, true, false) => alt('f'),
        ("z", true, _, false) => ctrl('_'),
        ("z", true, _, true) => alt('_'),
        _ => None,
    }
}

pub(crate) fn mouse_button(button: gpui::MouseButton) -> Option<ClientMouseButton> {
    match button {
        gpui::MouseButton::Left => Some(ClientMouseButton::Left),
        gpui::MouseButton::Right => Some(ClientMouseButton::Right),
        gpui::MouseButton::Middle => Some(ClientMouseButton::Middle),
        gpui::MouseButton::Navigate(_) => None,
    }
}

/// Mouse input for `pane`, with the position made relative to its content.
pub(crate) fn mouse_input(
    pane: &PaneSurfacePane,
    kind: ClientMouseKind,
    col: u16,
    row: u16,
    modifiers: &gpui::Modifiers,
    lines: u16,
) -> ClientPaneInputEvent {
    ClientPaneInputEvent::Mouse {
        kind,
        position: ClientMousePosition::Cell {
            column: col.saturating_sub(pane.inner_rect.x),
            row: row.saturating_sub(pane.inner_rect.y),
        },
        geometry: None,
        modifiers: key_modifiers(modifiers),
        lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_frame(lines: &[&str]) -> FrameData {
        let width = lines.iter().map(|line| line.len()).max().unwrap_or(0) as u16;
        let cells = lines
            .iter()
            .flat_map(|line| {
                (0..usize::from(width)).map(move |col| CellData {
                    symbol: line.get(col..=col).unwrap_or("").to_owned(),
                    fg: 0,
                    bg: 0,
                    modifier: 0,
                    skip: false,
                    hyperlink: None,
                })
            })
            .collect();
        FrameData {
            cells,
            width,
            height: lines.len() as u16,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        }
    }

    fn selection(area: SurfaceRect, anchor: (u16, u16), head: (u16, u16)) -> Selection {
        let mut selection = Selection {
            pane_id: "p".into(),
            area,
            anchor: clamp_to(area, anchor.0, anchor.1),
            head: (0, 0),
        };
        selection.extend(head.0, head.1);
        selection
    }

    #[test]
    fn mac_editing_shortcuts_become_terminal_keys() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let command = gpui::Modifiers::command();
        let option = gpui::Modifiers::alt();
        let sent = |key: &str, modifiers| match editing_shortcut(&keystroke(key, None, modifiers)) {
            Some(ClientPaneInputEvent::Key {
                code: ClientKeyCode::Char(ch),
                modifiers,
                ..
            }) => Some((ch, modifiers)),
            _ => None,
        };
        assert_eq!(sent("backspace", command), Some(('u', KEY_CONTROL)));
        assert_eq!(sent("backspace", option), Some(('w', KEY_CONTROL)));
        assert_eq!(sent("left", command), Some(('a', KEY_CONTROL)));
        assert_eq!(sent("right", option), Some(('f', KEY_ALT)));
        assert_eq!(sent("z", command), Some(('_', KEY_CONTROL)));
        // Plain keys and other chords pass through unchanged.
        assert_eq!(sent("backspace", gpui::Modifiers::none()), None);
        assert_eq!(sent("a", command), None);
    }

    #[test]
    fn double_and_triple_click_pick_a_word_or_line() {
        let frame = text_frame(&["|cd ~/src/herdr && ls |"]);
        let pane = PaneSurfacePane {
            pane_id: "p".into(),
            content_revision: 0,
            rect: SurfaceRect {
                x: 0,
                y: 0,
                width: 23,
                height: 1,
            },
            inner_rect: SurfaceRect {
                x: 1,
                y: 0,
                width: 21,
                height: 1,
            },
            scrollbar_rect: None,
            scroll: None,
            focused: true,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        };
        assert_eq!(
            Selection::word(&pane, &frame, 8, 0).text(&frame),
            "~/src/herdr"
        );
        assert!(Selection::word(&pane, &frame, 3, 0).is_empty());
        assert_eq!(
            Selection::line(&pane, 5, 0).text(&frame),
            "cd ~/src/herdr && ls"
        );
        assert_eq!(Selection::all(&pane).text(&frame), "cd ~/src/herdr && ls");
    }

    #[test]
    fn links_come_from_osc8_or_plain_urls() {
        let mut frame = text_frame(&["see https://herdr.dev/docs. ok"]);
        assert_eq!(
            link_at(&frame, 10, 0).as_deref(),
            Some("https://herdr.dev/docs")
        );
        // The trailing period and the text after the URL are not part of it.
        assert_eq!(link_at(&frame, 26, 0), None);
        assert_eq!(link_at(&frame, 1, 0), None);
        frame.cells[1].hyperlink = Some(0);
        frame.hyperlinks.push("https://example.test".into());
        assert_eq!(
            link_at(&frame, 1, 0).as_deref(),
            Some("https://example.test")
        );
        assert_eq!(link_at(&frame, 99, 0), None);
    }

    #[test]
    fn selection_reads_like_text_and_stays_in_its_pane() {
        let frame = text_frame(&["|hello world |", "|second line |", "|third       |"]);
        let pane = SurfaceRect {
            x: 1,
            y: 0,
            width: 12,
            height: 3,
        };
        // Dragged backwards from "line" up to "world", past the pane's right border.
        let picked = selection(pane, (11, 1), (7, 0));
        assert_eq!(picked.text(&frame), "world\nsecond line");
        // Dragging outside the pane clamps to its edges instead of copying borders.
        let all = selection(pane, (0, 0), (40, 9));
        assert_eq!(all.text(&frame), "hello world\nsecond line\nthird");
        assert!(selection(pane, (3, 1), (3, 1)).is_empty());
    }

    fn keystroke(key: &str, key_char: Option<&str>, modifiers: gpui::Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(str::to_owned),
        }
    }

    #[test]
    fn block_and_box_characters_become_shapes() {
        assert_eq!(
            cell_shape('\u{259B}'),
            Some(CellShape::Blocks(vec![UL, UR, LL], 1.0))
        );
        assert_eq!(
            cell_shape('\u{2584}'),
            Some(CellShape::Blocks(vec![(0.0, 0.5, 1.0, 1.0)], 1.0))
        );
        assert_eq!(
            cell_shape('\u{258C}'),
            Some(CellShape::Blocks(vec![(0.0, 0.0, 0.5, 1.0)], 1.0))
        );
        assert_eq!(cell_shape('\u{2502}'), Some(CellShape::Lines([1, 0, 1, 0])));
        assert_eq!(cell_shape('\u{256D}'), Some(CellShape::Lines([0, 1, 1, 0])));
        assert_eq!(cell_shape('a'), None);
    }

    #[test]
    fn printable_keys_carry_their_text() {
        let event = key_input(&keystroke("a", Some("A"), gpui::Modifiers::shift()), false);
        assert!(matches!(
            event,
            Some(ClientPaneInputEvent::Key {
                code: ClientKeyCode::Char('A'),
                generated_text: Some(ref text),
                modifiers: KEY_SHIFT,
                ..
            }) if text == "A"
        ));
    }

    #[test]
    fn control_chords_send_the_base_key_without_text() {
        let event = key_input(&keystroke("c", None, gpui::Modifiers::control()), false);
        assert!(matches!(
            event,
            Some(ClientPaneInputEvent::Key {
                code: ClientKeyCode::Char('c'),
                generated_text: None,
                modifiers: KEY_CONTROL,
                ..
            })
        ));
    }

    #[test]
    fn named_and_function_keys_map_to_key_codes() {
        let none = gpui::Modifiers::default();
        for (key, expected) in [
            ("enter", ClientKeyCode::Enter),
            ("escape", ClientKeyCode::Esc),
            ("pagedown", ClientKeyCode::PageDown),
            ("f5", ClientKeyCode::F(5)),
        ] {
            assert!(matches!(
                key_input(&keystroke(key, None, none), false),
                Some(ClientPaneInputEvent::Key { code, .. }) if code == expected
            ));
        }
        assert!(matches!(
            key_input(&keystroke("tab", None, gpui::Modifiers::shift()), false),
            Some(ClientPaneInputEvent::Key {
                code: ClientKeyCode::BackTab,
                modifiers: KEY_SHIFT,
                ..
            })
        ));
    }

    #[test]
    fn layout_text_that_is_not_one_char_is_committed() {
        assert_eq!(
            key_input(
                &keystroke("e", Some("é́"), gpui::Modifiers::default()),
                false
            ),
            Some(ClientPaneInputEvent::TextCommit("é́".into()))
        );
    }
}
