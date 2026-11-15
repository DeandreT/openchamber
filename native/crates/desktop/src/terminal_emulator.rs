//! The terminal emulator: libghostty-vt behind a renderer-independent API.
//!
//! Mirrors `packages/ui/src/lib/ghostty/core.ts`, which runs the same Ghostty
//! revision as WebAssembly: VT parsing, the grid, scrollback, selection, and
//! the key, paste and mouse encoders. Painting reads a [`Frame`]; nothing
//! here touches GPUI except the keystroke type it maps to Ghostty keys.
//!
//! Replies to queries in the output (cursor position, DECRQM, Kitty flags)
//! go to [`Emulator::take_replies`]. History replay detaches that writer, so
//! queries recorded in history never reach the live shell. The server answers
//! theme queries (OSC 10/11, mode 2031) itself; this revision of Ghostty
//! leaves them unanswered.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::Keystroke;
use libghostty_vt::key::{self, Mods};
use libghostty_vt::mouse;
use libghostty_vt::paste;
use libghostty_vt::render::{CellIterator, CursorVisualStyle, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::selection::{FormatOptions, Selection};
use libghostty_vt::style::{RgbColor, Underline};
use libghostty_vt::terminal::{Mode, Point, PointCoordinate, ScrollViewport};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};

/// Rows of scrollback a terminal keeps.
pub(crate) const SCROLLBACK_ROWS: usize = 10_000;
/// Ghostty bounds scrollback in bytes of page memory, which is about 13 to
/// 15 bytes per column per row. This budget keeps at least
/// [`SCROLLBACK_ROWS`] rows up to the width it was sized for.
const SCROLLBACK_BYTES_PER_COLUMN: usize = 16;
const SCROLLBACK_BYTES_PER_ROW: usize = 256;
/// Widths below this are budgeted as this wide, so a docked panel keeps the
/// row count when its font shrinks. The docked panel is about 65 columns at
/// the default font size.
const SCROLLBACK_MIN_BUDGET_COLUMNS: usize = 100;

/// The bytes of page memory that keep [`SCROLLBACK_ROWS`] rows at `cols`.
pub(crate) fn scrollback_budget(cols: u16) -> usize {
    let cols = usize::from(cols).max(SCROLLBACK_MIN_BUDGET_COLUMNS);
    SCROLLBACK_ROWS * (cols * SCROLLBACK_BYTES_PER_COLUMN + SCROLLBACK_BYTES_PER_ROW)
}

/// An RGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rgb(pub u8, pub u8, pub u8);

impl From<RgbColor> for Rgb {
    fn from(color: RgbColor) -> Self {
        Self(color.r, color.g, color.b)
    }
}

impl From<Rgb> for RgbColor {
    fn from(Rgb(r, g, b): Rgb) -> Self {
        Self { r, g, b }
    }
}

/// The theme's terminal colors (`convertThemeToXterm`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalColors {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
    pub selection_foreground: Rgb,
    /// The 16 ANSI colors; the rest of the 256-color palette is Ghostty's.
    pub ansi: [Rgb; 16],
}

/// One painted cell.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FrameCell {
    /// The grapheme cluster, empty for a blank cell.
    pub text: String,
    pub foreground: Rgb,
    /// `None` paints the terminal background.
    pub background: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: bool,
    pub strikethrough: bool,
    /// The first cell of a two-cell character.
    pub wide: bool,
    /// The second cell of a two-cell character; it paints only its background.
    pub spacer: bool,
}

/// How the cursor is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorShape {
    Block,
    Bar,
    Underline,
    Hollow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub color: Rgb,
}

/// What the painter draws: the viewport's cells, cursor and colors.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Frame {
    pub rows: Vec<Vec<FrameCell>>,
    pub cursor: Option<Cursor>,
    pub background: Option<Rgb>,
}

/// The scrollback position: rows in total, the viewport's first row and its
/// height.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScrollPosition {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

#[cfg(test)]
impl ScrollPosition {
    pub fn at_bottom(self) -> bool {
        self.offset + self.len >= self.total
    }
}

/// A key press in Ghostty's terms.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KeyInput {
    pub key: key::Key,
    pub mods: Mods,
    /// The text the key types, if any.
    pub text: Option<String>,
    pub unshifted: Option<char>,
}

/// A mouse event for the program's mouse reporting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MouseInput {
    pub action: mouse::Action,
    pub button: Option<mouse::Button>,
    pub mods: Mods,
    /// Position inside the grid in pixels, padding excluded.
    pub x: f32,
    pub y: f32,
}

/// The writer the emulator answers queries through.
#[derive(Debug, Default)]
struct PtySink {
    attached: bool,
    written: Vec<u8>,
}

/// One terminal's emulator state.
pub(crate) struct Emulator {
    terminal: Terminal<'static, 'static>,
    sink: Rc<RefCell<PtySink>>,
    keys: key::Encoder<'static>,
    mouse: mouse::Encoder<'static>,
    render: RenderState<'static>,
    row_iter: RowIterator<'static>,
    cell_iter: CellIterator<'static>,
    colors: TerminalColors,
    cell_size: (u32, u32),
}

impl std::fmt::Debug for Emulator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Emulator")
            .field("size", &self.size())
            .finish_non_exhaustive()
    }
}

impl Emulator {
    pub fn new(cols: u16, rows: u16, colors: TerminalColors) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: scrollback_budget(cols),
        })
        .expect("Ghostty allocates a terminal");
        let sink = Rc::new(RefCell::new(PtySink {
            attached: true,
            written: Vec::new(),
        }));
        let writer = Rc::clone(&sink);
        terminal
            .on_pty_write(move |_, data| {
                let mut sink = writer.borrow_mut();
                if sink.attached {
                    sink.written.extend_from_slice(data);
                }
            })
            .expect("Ghostty registers the PTY writer");
        let mut emulator = Self {
            terminal,
            sink,
            keys: key::Encoder::new().expect("Ghostty allocates a key encoder"),
            mouse: mouse::Encoder::new().expect("Ghostty allocates a mouse encoder"),
            render: RenderState::new().expect("Ghostty allocates a render state"),
            row_iter: RowIterator::new().expect("Ghostty allocates a row iterator"),
            cell_iter: CellIterator::new().expect("Ghostty allocates a cell iterator"),
            colors: colors.clone(),
            cell_size: (8, 16),
        };
        emulator.set_colors(colors);
        emulator
    }

    /// Applies theme colors, as `setTheme` does.
    pub fn set_colors(&mut self, colors: TerminalColors) {
        let mut palette = self
            .terminal
            .default_color_palette()
            .expect("Ghostty reads its palette");
        for (slot, color) in palette.iter_mut().zip(colors.ansi) {
            *slot = color.into();
        }
        let _ = self
            .terminal
            .set_default_fg_color(Some(colors.foreground.into()))
            .and_then(|terminal| terminal.set_default_bg_color(Some(colors.background.into())))
            .and_then(|terminal| terminal.set_default_cursor_color(Some(colors.cursor.into())))
            .and_then(|terminal| terminal.set_default_color_palette(Some(palette)));
        self.colors = colors;
    }

    pub fn size(&self) -> (u16, u16) {
        (
            self.terminal.cols().unwrap_or(1),
            self.terminal.rows().unwrap_or(1),
        )
    }

    /// Feeds live PTY output.
    pub fn feed(&mut self, data: &[u8]) {
        self.terminal.vt_write(data);
    }

    /// Replaces the screen with history: reset, then the history at the size
    /// it was drawn for, then a reflow to `fit`. The PTY writer stays
    /// detached throughout, so queries in history are never answered.
    pub fn replay(&mut self, history: &str, recorded: Option<(u16, u16)>, fit: (u16, u16)) {
        self.sink.borrow_mut().attached = false;
        self.terminal.reset();
        let (cell_width, cell_height) = self.cell_size;
        if let Some((cols, rows)) = recorded {
            let _ = self
                .terminal
                .resize(cols.max(1), rows.max(1), cell_width, cell_height);
        }
        self.terminal.vt_write(history.as_bytes());
        let _ = self
            .terminal
            .resize(fit.0.max(1), fit.1.max(1), cell_width, cell_height);
        let mut sink = self.sink.borrow_mut();
        sink.written.clear();
        sink.attached = true;
    }

    /// Replies the emulator owes the PTY since the last call.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.sink.borrow_mut().written)
    }

    /// Resizes and reflows; the same size again changes nothing.
    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u32, cell_height: u32) {
        self.cell_size = (cell_width.max(1), cell_height.max(1));
        let _ = self
            .terminal
            .resize(cols.max(1), rows.max(1), self.cell_size.0, self.cell_size.1);
    }

    pub fn bracketed_paste(&self) -> bool {
        self.terminal.mode(Mode::BRACKETED_PASTE).unwrap_or(false)
    }

    pub fn mouse_tracking(&self) -> bool {
        self.terminal.is_mouse_tracking().unwrap_or(false)
    }

    pub fn encode_key(&mut self, input: &KeyInput, release: bool) -> Vec<u8> {
        let Ok(mut event) = key::Event::new() else {
            return Vec::new();
        };
        event
            .set_action(if release {
                key::Action::Release
            } else {
                key::Action::Press
            })
            .set_key(input.key)
            .set_mods(input.mods);
        if let Some(text) = &input.text {
            event.set_utf8(Some(text.clone()));
        }
        if let Some(unshifted) = input.unshifted {
            event.set_unshifted_codepoint(unshifted);
        }
        self.keys.set_options_from_terminal(&self.terminal);
        let mut out = Vec::new();
        if self.keys.encode_to_vec(&event, &mut out).is_err() {
            return Vec::new();
        }
        out
    }

    /// Encodes pasted text: unsafe control bytes become spaces, and the text
    /// is bracketed when the program asked for it (DEC mode 2004), or has
    /// its line feeds turned into carriage returns when it did not.
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        if text.is_empty() {
            return Vec::new();
        }
        let mut data = text.as_bytes().to_vec();
        let bracketed = self.bracketed_paste();
        let mut out = vec![0u8; data.len() + 16];
        match paste::encode(&mut data, bracketed, &mut out) {
            Ok(len) => {
                out.truncate(len);
                out
            }
            Err(libghostty_vt::Error::OutOfSpace { required }) => {
                let mut data = text.as_bytes().to_vec();
                out.resize(required, 0);
                let len = paste::encode(&mut data, bracketed, &mut out).unwrap_or(0);
                out.truncate(len);
                out
            }
            Err(_) => Vec::new(),
        }
    }

    pub fn encode_mouse(&mut self, input: MouseInput, grid: (u32, u32)) -> Vec<u8> {
        let (cell_width, cell_height) = self.cell_size;
        self.mouse
            .set_options_from_terminal(&self.terminal)
            .set_size(mouse::EncoderSize {
                screen_width: grid.0.max(1),
                screen_height: grid.1.max(1),
                cell_width,
                cell_height,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            });
        let Ok(mut event) = mouse::Event::new() else {
            return Vec::new();
        };
        event
            .set_action(input.action)
            .set_button(input.button)
            .set_mods(input.mods)
            .set_position(mouse::Position {
                x: input.x,
                y: input.y,
            });
        let mut out = Vec::new();
        if self.mouse.encode_to_vec(&event, &mut out).is_err() {
            return Vec::new();
        }
        out
    }

    pub fn scroll(&mut self, rows: isize) {
        self.terminal.scroll_viewport(ScrollViewport::Delta(rows));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
    }

    #[cfg(test)]
    pub fn scroll_to_top(&mut self) {
        self.terminal.scroll_viewport(ScrollViewport::Top);
    }

    pub fn scroll_position(&self) -> ScrollPosition {
        self.terminal
            .scrollbar()
            .map(|bar| ScrollPosition {
                total: bar.total,
                offset: bar.offset,
                len: bar.len,
            })
            .unwrap_or_default()
    }

    /// Rows above the active area.
    #[cfg(test)]
    pub fn scrollback_rows(&self) -> usize {
        self.terminal.scrollback_rows().unwrap_or(0)
    }

    /// Selects from `anchor` to `head`, both viewport cells, inclusive.
    pub fn select(&mut self, anchor: (u16, u16), head: (u16, u16)) {
        let offset = self.scroll_position().offset;
        let point = |(col, row): (u16, u16)| {
            Point::Screen(PointCoordinate {
                x: col,
                y: (offset + u64::from(row)) as u32,
            })
        };
        let (Ok(start), Ok(end)) = (
            self.terminal.grid_ref(point(anchor)),
            self.terminal.grid_ref(point(head)),
        ) else {
            return;
        };
        let selection = Selection::new(start, end, false);
        let _ = self.terminal.set_selection(Some(&selection));
    }

    /// Selects the word, or with `line`, the whole line at a viewport cell.
    pub fn select_unit(&mut self, cell: (u16, u16), line: bool) {
        let Ok(grid_ref) = self.terminal.grid_ref(Point::Viewport(PointCoordinate {
            x: cell.0,
            y: u32::from(cell.1),
        })) else {
            return;
        };
        let selection = if line {
            self.terminal
                .select_line(libghostty_vt::selection::SelectLineOptions::new(grid_ref))
        } else {
            self.terminal
                .select_word(libghostty_vt::selection::SelectWordOptions::new(grid_ref))
        };
        if let Ok(Some(selection)) = selection {
            let _ = self.terminal.set_selection(Some(&selection));
        }
    }

    pub fn clear_selection(&mut self) {
        let _ = self.terminal.set_selection(None);
    }

    /// The selected text as copied: soft wraps joined, trailing blanks trimmed.
    pub fn selection_text(&self) -> Option<String> {
        let options = || FormatOptions::new().with_unwrap(true).with_trim(true);
        let mut buffer = vec![0u8; 4096];
        let written = match self.terminal.format_selection_buf(options(), &mut buffer) {
            Ok(written) => written,
            Err(libghostty_vt::Error::OutOfSpace { required }) => {
                buffer.resize(required, 0);
                self.terminal
                    .format_selection_buf(options(), &mut buffer)
                    .ok()
                    .flatten()
            }
            Err(_) => None,
        }?;
        buffer.truncate(written);
        let text = String::from_utf8_lossy(&buffer).into_owned();
        (!text.is_empty()).then_some(text)
    }

    /// The link at a viewport cell: an OSC 8 hyperlink, else an `http(s)`
    /// URL in the cell's logical line, soft-wrapped rows included
    /// (`terminalLinks.ts`).
    pub fn link_at(&self, cell: (u16, u16)) -> Option<String> {
        let viewport = |col: u16, row: u32| Point::Viewport(PointCoordinate { x: col, y: row });
        if let Ok(grid_ref) = self.terminal.grid_ref(viewport(cell.0, u32::from(cell.1))) {
            let mut uri = vec![0u8; 2048];
            if let Ok(len) = grid_ref.hyperlink_uri(&mut uri)
                && len > 0
                && len <= uri.len()
            {
                return Some(String::from_utf8_lossy(&uri[..len]).into_owned());
            }
        }
        let (cols, rows) = self.size();
        let wrapped = |row: u32| {
            self.terminal
                .grid_ref(viewport(0, row))
                .and_then(|grid_ref| grid_ref.row())
                .and_then(|row| row.is_wrapped())
                .unwrap_or(false)
        };
        let mut first = u32::from(cell.1);
        while first > 0 && wrapped(first - 1) {
            first -= 1;
        }
        let mut last = u32::from(cell.1);
        while last + 1 < u32::from(rows) && wrapped(last) {
            last += 1;
        }
        // The logical line as text, with the character index of each cell.
        let mut text = String::new();
        let mut target = None;
        for row in first..=last {
            for col in 0..cols {
                let Ok(grid_ref) = self.terminal.grid_ref(viewport(col, row)) else {
                    continue;
                };
                if grid_ref
                    .cell()
                    .and_then(|cell| cell.wide())
                    .is_ok_and(|wide| matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead))
                {
                    continue;
                }
                if row == u32::from(cell.1) && col == cell.0 {
                    target = Some(text.len());
                }
                let mut graphemes = ['\0'; 16];
                match grid_ref.graphemes(&mut graphemes) {
                    Ok(len) if len > 0 => text.extend(&graphemes[..len.min(16)]),
                    _ => text.push(' '),
                }
            }
        }
        url_at(&text, target?)
    }

    /// The viewport as plain text rows.
    #[cfg(test)]
    pub fn viewport_text(&mut self) -> Vec<String> {
        self.frame()
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .filter(|cell| !cell.spacer)
                    .map(|cell| {
                        if cell.text.is_empty() {
                            " "
                        } else {
                            cell.text.as_str()
                        }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// The viewport to paint.
    pub fn frame(&mut self) -> Frame {
        let colors = self.colors.clone();
        let Ok(snapshot) = self.render.update(&self.terminal) else {
            return Frame::default();
        };
        let default_background = snapshot
            .colors()
            .map(|colors| Rgb::from(colors.background))
            .unwrap_or(colors.background);
        let default_foreground = snapshot
            .colors()
            .map(|colors| Rgb::from(colors.foreground))
            .unwrap_or(colors.foreground);
        let cursor = snapshot
            .cursor_visible()
            .unwrap_or(false)
            .then(|| snapshot.cursor_viewport().ok().flatten())
            .flatten()
            .map(|position| Cursor {
                col: position.x,
                row: position.y,
                shape: match snapshot.cursor_visual_style() {
                    Ok(CursorVisualStyle::Bar) => CursorShape::Bar,
                    Ok(CursorVisualStyle::Underline) => CursorShape::Underline,
                    Ok(CursorVisualStyle::BlockHollow) => CursorShape::Hollow,
                    _ => CursorShape::Block,
                },
                color: snapshot
                    .cursor_color()
                    .ok()
                    .flatten()
                    .map(Rgb::from)
                    .unwrap_or(colors.cursor),
            });
        let mut rows = Vec::new();
        let Ok(mut row_iteration) = self.row_iter.update(&snapshot) else {
            return Frame::default();
        };
        while let Some(row) = row_iteration.next() {
            let selection = row.selection().ok().flatten();
            let mut cells = Vec::new();
            let Ok(mut cell_iteration) = self.cell_iter.update(row) else {
                rows.push(cells);
                continue;
            };
            let mut col: u16 = 0;
            while let Some(cell) = cell_iteration.next() {
                let style = cell.style().unwrap_or_default();
                let wide = cell
                    .raw_cell()
                    .and_then(|raw| raw.wide())
                    .unwrap_or(CellWide::Narrow);
                let mut text = String::new();
                let _ = cell.graphemes_utf8(&mut text);
                let mut foreground = cell
                    .fg_color()
                    .ok()
                    .flatten()
                    .map(Rgb::from)
                    .unwrap_or(default_foreground);
                let mut background = cell.bg_color().ok().flatten().map(Rgb::from);
                if style.inverse {
                    let previous = background.unwrap_or(default_background);
                    background = Some(foreground);
                    foreground = previous;
                }
                if style.invisible {
                    foreground = background.unwrap_or(default_background);
                }
                let selected =
                    selection.is_some_and(|range| col >= range.start_x && col <= range.end_x);
                if selected {
                    background = Some(colors.selection);
                    foreground = colors.selection_foreground;
                }
                cells.push(FrameCell {
                    text,
                    foreground,
                    background,
                    bold: style.bold,
                    italic: style.italic,
                    faint: style.faint,
                    underline: style.underline != Underline::None,
                    strikethrough: style.strikethrough,
                    wide: wide == CellWide::Wide,
                    spacer: matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead),
                });
                col += 1;
            }
            rows.push(cells);
        }
        Frame {
            rows,
            cursor,
            background: Some(default_background),
        }
    }
}

/// The `http(s)` URL around character index `at` of `line`, without the
/// trailing punctuation a sentence puts after it.
fn url_at(line: &str, at: usize) -> Option<String> {
    let is_url_char = |character: char| {
        !character.is_whitespace() && !matches!(character, '"' | '<' | '>' | '`' | '\'')
    };
    let start = line[..at.min(line.len())]
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_url_char(*character))
        .last()
        .map_or(at, |(index, _)| index);
    let end = line[start..]
        .char_indices()
        .find(|(_, character)| !is_url_char(*character))
        .map_or(line.len(), |(index, _)| start + index);
    if at >= end {
        return None;
    }
    let token = &line[start..end];
    let scheme = token.find("https://").or_else(|| token.find("http://"))?;
    if start + scheme > at {
        return None;
    }
    let url = token[scheme..].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
    (url.len() > "https://".len()).then(|| url.to_owned())
}

/// Maps a GPUI keystroke to the key Ghostty encodes, or `None` for keys it
/// has no code for. Shifted symbols map back to their US-layout key, as
/// `KeyboardEvent.code` names the physical key.
pub(crate) fn key_input(keystroke: &Keystroke) -> Option<KeyInput> {
    use key::Key;
    let modifiers = &keystroke.modifiers;
    let mut mods = Mods::empty();
    mods.set(Mods::SHIFT, modifiers.shift);
    mods.set(Mods::CTRL, modifiers.control);
    mods.set(Mods::ALT, modifiers.alt);
    mods.set(Mods::SUPER, modifiers.platform);
    let name = keystroke.key.as_str();
    let named = match name {
        "enter" => Some(Key::Enter),
        "tab" => Some(Key::Tab),
        "backspace" => Some(Key::Backspace),
        "escape" => Some(Key::Escape),
        "space" => Some(Key::Space),
        "delete" => Some(Key::Delete),
        "insert" => Some(Key::Insert),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "up" => Some(Key::ArrowUp),
        "down" => Some(Key::ArrowDown),
        "left" => Some(Key::ArrowLeft),
        "right" => Some(Key::ArrowRight),
        "f1" => Some(Key::F1),
        "f2" => Some(Key::F2),
        "f3" => Some(Key::F3),
        "f4" => Some(Key::F4),
        "f5" => Some(Key::F5),
        "f6" => Some(Key::F6),
        "f7" => Some(Key::F7),
        "f8" => Some(Key::F8),
        "f9" => Some(Key::F9),
        "f10" => Some(Key::F10),
        "f11" => Some(Key::F11),
        "f12" => Some(Key::F12),
        _ => None,
    };
    let printable = keystroke
        .key_char
        .as_deref()
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control));
    if let Some(key) = named {
        let text = match key {
            Key::Space => Some(printable.unwrap_or(" ").to_owned()),
            _ => None,
        };
        return Some(KeyInput {
            key,
            mods,
            text,
            unshifted: (key == Key::Space).then_some(' '),
        });
    }
    let mut characters = name.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        // A named key Ghostty has no code for, or a composed keysym name.
        return printable.map(|text| KeyInput {
            key: Key::Unidentified,
            mods,
            text: Some(text.to_owned()),
            unshifted: text.chars().next(),
        });
    }
    let (key, unshifted) = physical_key(character)?;
    // `KeyboardEvent.key`: the typed character, or the key's own character
    // when Ctrl or Alt keeps it from typing.
    let text = printable.map(str::to_owned).or_else(|| {
        let mut base = unshifted.to_string();
        if modifiers.shift {
            base = base.to_uppercase();
        }
        Some(base)
    });
    Some(KeyInput {
        key,
        mods,
        text,
        unshifted: Some(unshifted),
    })
}

/// The US-layout key that types `character`, and its unshifted character.
fn physical_key(character: char) -> Option<(key::Key, char)> {
    use key::Key;
    let lower = character.to_ascii_lowercase();
    let letter = match lower {
        'a' => Some(Key::A),
        'b' => Some(Key::B),
        'c' => Some(Key::C),
        'd' => Some(Key::D),
        'e' => Some(Key::E),
        'f' => Some(Key::F),
        'g' => Some(Key::G),
        'h' => Some(Key::H),
        'i' => Some(Key::I),
        'j' => Some(Key::J),
        'k' => Some(Key::K),
        'l' => Some(Key::L),
        'm' => Some(Key::M),
        'n' => Some(Key::N),
        'o' => Some(Key::O),
        'p' => Some(Key::P),
        'q' => Some(Key::Q),
        'r' => Some(Key::R),
        's' => Some(Key::S),
        't' => Some(Key::T),
        'u' => Some(Key::U),
        'v' => Some(Key::V),
        'w' => Some(Key::W),
        'x' => Some(Key::X),
        'y' => Some(Key::Y),
        'z' => Some(Key::Z),
        _ => None,
    };
    if let Some(key) = letter {
        return Some((key, lower));
    }
    Some(match character {
        '0' | ')' => (Key::Digit0, '0'),
        '1' | '!' => (Key::Digit1, '1'),
        '2' | '@' => (Key::Digit2, '2'),
        '3' | '#' => (Key::Digit3, '3'),
        '4' | '$' => (Key::Digit4, '4'),
        '5' | '%' => (Key::Digit5, '5'),
        '6' | '^' => (Key::Digit6, '6'),
        '7' | '&' => (Key::Digit7, '7'),
        '8' | '*' => (Key::Digit8, '8'),
        '9' | '(' => (Key::Digit9, '9'),
        '-' | '_' => (Key::Minus, '-'),
        '=' | '+' => (Key::Equal, '='),
        '[' | '{' => (Key::BracketLeft, '['),
        ']' | '}' => (Key::BracketRight, ']'),
        '\\' | '|' => (Key::Backslash, '\\'),
        ';' | ':' => (Key::Semicolon, ';'),
        '\'' | '"' => (Key::Quote, '\''),
        ',' | '<' => (Key::Comma, ','),
        '.' | '>' => (Key::Period, '.'),
        '/' | '?' => (Key::Slash, '/'),
        '`' | '~' => (Key::Backquote, '`'),
        // A character outside the US layout types itself.
        other => (key::Key::Unidentified, other),
    })
}

/// Ctrl with `=`, `-` or `0` and no other modifier: the app's zoom, which
/// the terminal leaves alone (`isTerminalZoomShortcut`).
pub(crate) fn is_zoom_shortcut(keystroke: &Keystroke) -> bool {
    let modifiers = &keystroke.modifiers;
    modifiers.control
        && !modifiers.alt
        && !modifiers.shift
        && !modifiers.platform
        && matches!(keystroke.key.as_str(), "=" | "-" | "0")
}

/// Ctrl+Shift+V or Shift+Insert (`isTerminalPasteShortcut` on Linux).
pub(crate) fn is_paste_shortcut(keystroke: &Keystroke) -> bool {
    let modifiers = &keystroke.modifiers;
    match keystroke.key.as_str() {
        "insert" => modifiers.shift && !modifiers.control && !modifiers.platform,
        "v" | "V" => modifiers.control && modifiers.shift && !modifiers.alt,
        _ => false,
    }
}

/// Ctrl+C or Ctrl+Shift+C. Plain Ctrl+C copies only while text is selected.
pub(crate) fn is_copy_shortcut(keystroke: &Keystroke) -> bool {
    let modifiers = &keystroke.modifiers;
    matches!(keystroke.key.as_str(), "c" | "C") && modifiers.control && !modifiers.alt
}

// ---------------------------------------------------------------------------
// Procedural glyphs
// ---------------------------------------------------------------------------

/// A filled shape in pixels relative to the cell's origin.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Shape {
    Rect {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        /// 1.0 for solid, less for the shade blocks.
        alpha: f32,
    },
    Polygon(Vec<(f32, f32)>),
}

/// Line weights of a box-drawing character's four arms: up, right, down,
/// left. 0 none, 1 light, 2 heavy, 3 double.
fn box_arms(character: char) -> Option<[u8; 4]> {
    const TABLE: [[u8; 4]; 0x80] = {
        let mut table = [[0u8; 4]; 0x80];
        let entries: &[(usize, [u8; 4])] = &[
            (0x00, [0, 1, 0, 1]),
            (0x01, [0, 2, 0, 2]),
            (0x02, [1, 0, 1, 0]),
            (0x03, [2, 0, 2, 0]),
            (0x04, [0, 1, 0, 1]),
            (0x05, [0, 2, 0, 2]),
            (0x06, [1, 0, 1, 0]),
            (0x07, [2, 0, 2, 0]),
            (0x08, [0, 1, 0, 1]),
            (0x09, [0, 2, 0, 2]),
            (0x0a, [1, 0, 1, 0]),
            (0x0b, [2, 0, 2, 0]),
            (0x0c, [0, 1, 1, 0]),
            (0x0d, [0, 2, 1, 0]),
            (0x0e, [0, 1, 2, 0]),
            (0x0f, [0, 2, 2, 0]),
            (0x10, [0, 0, 1, 1]),
            (0x11, [0, 0, 1, 2]),
            (0x12, [0, 0, 2, 1]),
            (0x13, [0, 0, 2, 2]),
            (0x14, [1, 1, 0, 0]),
            (0x15, [1, 2, 0, 0]),
            (0x16, [2, 1, 0, 0]),
            (0x17, [2, 2, 0, 0]),
            (0x18, [1, 0, 0, 1]),
            (0x19, [1, 0, 0, 2]),
            (0x1a, [2, 0, 0, 1]),
            (0x1b, [2, 0, 0, 2]),
            (0x1c, [1, 1, 1, 0]),
            (0x1d, [1, 2, 1, 0]),
            (0x1e, [2, 1, 1, 0]),
            (0x1f, [1, 1, 2, 0]),
            (0x20, [2, 1, 2, 0]),
            (0x21, [2, 2, 1, 0]),
            (0x22, [1, 2, 2, 0]),
            (0x23, [2, 2, 2, 0]),
            (0x24, [1, 0, 1, 1]),
            (0x25, [1, 0, 1, 2]),
            (0x26, [2, 0, 1, 1]),
            (0x27, [1, 0, 2, 1]),
            (0x28, [2, 0, 2, 1]),
            (0x29, [2, 0, 1, 2]),
            (0x2a, [1, 0, 2, 2]),
            (0x2b, [2, 0, 2, 2]),
            (0x2c, [0, 1, 1, 1]),
            (0x2d, [0, 1, 1, 2]),
            (0x2e, [0, 2, 1, 1]),
            (0x2f, [0, 2, 1, 2]),
            (0x30, [0, 1, 2, 1]),
            (0x31, [0, 1, 2, 2]),
            (0x32, [0, 2, 2, 1]),
            (0x33, [0, 2, 2, 2]),
            (0x34, [1, 1, 0, 1]),
            (0x35, [1, 1, 0, 2]),
            (0x36, [1, 2, 0, 1]),
            (0x37, [1, 2, 0, 2]),
            (0x38, [2, 1, 0, 1]),
            (0x39, [2, 1, 0, 2]),
            (0x3a, [2, 2, 0, 1]),
            (0x3b, [2, 2, 0, 2]),
            (0x3c, [1, 1, 1, 1]),
            (0x3d, [1, 1, 1, 2]),
            (0x3e, [1, 2, 1, 1]),
            (0x3f, [1, 2, 1, 2]),
            (0x40, [2, 1, 1, 1]),
            (0x41, [1, 1, 2, 1]),
            (0x42, [2, 1, 2, 1]),
            (0x43, [2, 1, 1, 2]),
            (0x44, [2, 2, 1, 1]),
            (0x45, [1, 1, 2, 2]),
            (0x46, [1, 2, 2, 1]),
            (0x47, [2, 2, 1, 2]),
            (0x48, [1, 2, 2, 2]),
            (0x49, [2, 1, 2, 2]),
            (0x4a, [2, 2, 2, 1]),
            (0x4b, [2, 2, 2, 2]),
            (0x4c, [0, 1, 0, 1]),
            (0x4d, [0, 2, 0, 2]),
            (0x4e, [1, 0, 1, 0]),
            (0x4f, [2, 0, 2, 0]),
            (0x50, [0, 3, 0, 3]),
            (0x51, [3, 0, 3, 0]),
            (0x52, [0, 3, 1, 0]),
            (0x53, [0, 1, 3, 0]),
            (0x54, [0, 3, 3, 0]),
            (0x55, [0, 0, 1, 3]),
            (0x56, [0, 0, 3, 1]),
            (0x57, [0, 0, 3, 3]),
            (0x58, [1, 3, 0, 0]),
            (0x59, [3, 1, 0, 0]),
            (0x5a, [3, 3, 0, 0]),
            (0x5b, [1, 0, 0, 3]),
            (0x5c, [3, 0, 0, 1]),
            (0x5d, [3, 0, 0, 3]),
            (0x5e, [1, 3, 1, 0]),
            (0x5f, [3, 1, 3, 0]),
            (0x60, [3, 3, 3, 0]),
            (0x61, [1, 0, 1, 3]),
            (0x62, [3, 0, 3, 1]),
            (0x63, [3, 0, 3, 3]),
            (0x64, [0, 3, 1, 3]),
            (0x65, [0, 1, 3, 1]),
            (0x66, [0, 3, 3, 3]),
            (0x67, [1, 3, 0, 3]),
            (0x68, [3, 1, 0, 1]),
            (0x69, [3, 3, 0, 3]),
            (0x6a, [1, 3, 1, 3]),
            (0x6b, [3, 1, 3, 1]),
            (0x6c, [3, 3, 3, 3]),
            (0x6d, [0, 1, 1, 0]),
            (0x6e, [0, 0, 1, 1]),
            (0x6f, [1, 0, 0, 1]),
            (0x70, [1, 1, 0, 0]),
            (0x74, [0, 0, 0, 1]),
            (0x75, [1, 0, 0, 0]),
            (0x76, [0, 1, 0, 0]),
            (0x77, [0, 0, 1, 0]),
            (0x78, [0, 0, 0, 2]),
            (0x79, [2, 0, 0, 0]),
            (0x7a, [0, 2, 0, 0]),
            (0x7b, [0, 0, 2, 0]),
            (0x7c, [0, 2, 0, 1]),
            (0x7d, [1, 0, 2, 0]),
            (0x7e, [0, 1, 0, 2]),
        ];
        let mut index = 0;
        while index < entries.len() {
            table[entries[index].0] = entries[index].1;
            index += 1;
        }
        table[0x7f] = [2, 0, 1, 0];
        table
    };
    let offset = (character as u32).checked_sub(0x2500)? as usize;
    TABLE.get(offset).copied().filter(|arms| arms != &[0; 4])
}

/// How many dashes a dashed line character has (`┄` 3, `┈` 4, `╌` 2).
fn dashes(character: char) -> Option<u8> {
    match character as u32 {
        0x2504..=0x2507 => Some(3),
        0x2508..=0x250b => Some(4),
        0x254c..=0x254f => Some(2),
        _ => None,
    }
}

/// Whether the cell is drawn procedurally: box drawing, block elements and
/// the Powerline separators (`boxDrawing.ts`).
pub(crate) fn is_procedural(character: char) -> bool {
    matches!(character as u32, 0x2500..=0x259f | 0xe0b0..=0xe0b3)
}

/// The shapes that fill a cell for `character`, or `None` when the font
/// draws it. `stroke` is the light line width in pixels.
pub(crate) fn procedural_shapes(
    character: char,
    width: f32,
    height: f32,
    stroke: f32,
) -> Option<Vec<Shape>> {
    let rect = |x: f32, y: f32, w: f32, h: f32| Shape::Rect {
        x,
        y,
        width: w,
        height: h,
        alpha: 1.0,
    };
    let code = character as u32;
    match code {
        0x2580..=0x259f => return Some(block_shapes(code, width, height)),
        0xe0b0 => {
            return Some(vec![Shape::Polygon(vec![
                (0.0, 0.0),
                (width, height / 2.0),
                (0.0, height),
            ])]);
        }
        0xe0b2 => {
            return Some(vec![Shape::Polygon(vec![
                (width, 0.0),
                (0.0, height / 2.0),
                (width, height),
            ])]);
        }
        0xe0b1 | 0xe0b3 => {
            // A thin chevron: two strokes meeting at the middle.
            let (outer, inner) = if code == 0xe0b1 {
                (0.0, width)
            } else {
                (width, 0.0)
            };
            let half = stroke / 2.0;
            let mid = height / 2.0;
            return Some(vec![
                Shape::Polygon(vec![
                    (outer, -half),
                    (inner, mid - half),
                    (inner, mid + half),
                    (outer, half),
                ]),
                Shape::Polygon(vec![
                    (outer, height - half),
                    (inner, mid - half),
                    (inner, mid + half),
                    (outer, height + half),
                ]),
            ]);
        }
        0x2571..=0x2573 => {
            let half = stroke / 2.0;
            let mut shapes = Vec::new();
            if code != 0x2572 {
                shapes.push(Shape::Polygon(vec![
                    (width - half, 0.0),
                    (width + half, 0.0),
                    (half, height),
                    (-half, height),
                ]));
            }
            if code != 0x2571 {
                shapes.push(Shape::Polygon(vec![
                    (-half, 0.0),
                    (half, 0.0),
                    (width + half, height),
                    (width - half, height),
                ]));
            }
            return Some(shapes);
        }
        _ => {}
    }
    let arms = box_arms(character)?;
    let cx = (width / 2.0).floor();
    let cy = (height / 2.0).floor();
    let thickness = |weight: u8| match weight {
        2 => (stroke * 2.0).round().max(stroke + 1.0),
        _ => stroke,
    };
    let mut shapes = Vec::new();
    if let Some(count) = dashes(character) {
        let horizontal = arms[1] != 0;
        let weight = arms[1].max(arms[0]);
        let t = thickness(weight);
        let length = if horizontal { width } else { height };
        let segment = length / f32::from(count);
        let gap = (segment * 0.3).max(1.0);
        for index in 0..count {
            let start = f32::from(index) * segment + gap / 2.0;
            let size = segment - gap;
            shapes.push(if horizontal {
                rect(start, cy - (t / 2.0).floor(), size, t)
            } else {
                rect(cx - (t / 2.0).floor(), start, t, size)
            });
        }
        return Some(shapes);
    }
    // Each arm runs from the cell edge to the center, overlapping the
    // perpendicular strokes so corners join without a notch.
    let reach = |weight: u8| (thickness(weight.max(1)) / 2.0).ceil() + stroke;
    let [up, right, down, left] = arms;
    let vertical_reach = reach(left.max(right));
    let horizontal_reach = reach(up.max(down));
    let mut arm = |weight: u8, horizontal: bool, from: f32, to: f32| {
        if weight == 0 {
            return;
        }
        let (start, end) = (from.min(to), from.max(to));
        let offsets: &[f32] = if weight == 3 { &[-1.0, 1.0] } else { &[0.0] };
        let t = thickness(weight);
        for offset in offsets {
            let shift = offset * stroke;
            if horizontal {
                shapes.push(rect(start, cy - (t / 2.0).floor() + shift, end - start, t));
            } else {
                shapes.push(rect(cx - (t / 2.0).floor() + shift, start, t, end - start));
            }
        }
    };
    arm(up, false, 0.0, cy + vertical_reach);
    arm(down, false, cy - vertical_reach, height);
    arm(left, true, 0.0, cx + horizontal_reach);
    arm(right, true, cx - horizontal_reach, width);
    Some(shapes)
}

fn block_shapes(code: u32, width: f32, height: f32) -> Vec<Shape> {
    let rect = |x: f32, y: f32, w: f32, h: f32| Shape::Rect {
        x,
        y,
        width: w,
        height: h,
        alpha: 1.0,
    };
    let eighth_h = height / 8.0;
    let eighth_w = width / 8.0;
    let (hw, hh) = (width / 2.0, height / 2.0);
    let quadrants = |ul: bool, ur: bool, ll: bool, lr: bool| {
        let mut shapes = Vec::new();
        if ul {
            shapes.push(rect(0.0, 0.0, hw, hh));
        }
        if ur {
            shapes.push(rect(hw, 0.0, width - hw, hh));
        }
        if ll {
            shapes.push(rect(0.0, hh, hw, height - hh));
        }
        if lr {
            shapes.push(rect(hw, hh, width - hw, height - hh));
        }
        shapes
    };
    match code {
        0x2580 => vec![rect(0.0, 0.0, width, hh)],
        0x2581..=0x2588 => {
            let eighths = (code - 0x2580) as f32;
            let h = eighth_h * eighths;
            vec![rect(0.0, height - h, width, h)]
        }
        0x2589..=0x258f => {
            let eighths = (0x2590 - code) as f32;
            vec![rect(0.0, 0.0, eighth_w * eighths, height)]
        }
        0x2590 => vec![rect(hw, 0.0, width - hw, height)],
        0x2591..=0x2593 => vec![Shape::Rect {
            x: 0.0,
            y: 0.0,
            width,
            height,
            alpha: (code - 0x2590) as f32 * 0.25,
        }],
        0x2594 => vec![rect(0.0, 0.0, width, eighth_h)],
        0x2595 => vec![rect(width - eighth_w, 0.0, eighth_w, height)],
        0x2596 => quadrants(false, false, true, false),
        0x2597 => quadrants(false, false, false, true),
        0x2598 => quadrants(true, false, false, false),
        0x2599 => quadrants(true, false, true, true),
        0x259a => quadrants(true, false, false, true),
        0x259b => quadrants(true, true, true, false),
        0x259c => quadrants(true, true, false, true),
        0x259d => quadrants(false, true, false, false),
        0x259e => quadrants(false, true, true, false),
        _ => quadrants(false, true, true, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::Modifiers;

    fn colors() -> TerminalColors {
        TerminalColors {
            background: Rgb(0x11, 0x11, 0x11),
            foreground: Rgb(0xdd, 0xdd, 0xdd),
            cursor: Rgb(0xff, 0xcc, 0x00),
            selection: Rgb(0x33, 0x55, 0x99),
            selection_foreground: Rgb(0xff, 0xff, 0xff),
            ansi: [Rgb(1, 2, 3); 16],
        }
    }

    fn keystroke(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.to_owned(),
            key_char: key_char.map(str::to_owned),
        }
    }

    fn encode(
        emulator: &mut Emulator,
        key: &str,
        key_char: Option<&str>,
        modifiers: Modifiers,
    ) -> Vec<u8> {
        let input = key_input(&keystroke(key, key_char, modifiers)).expect("a mapped key");
        emulator.encode_key(&input, false)
    }

    #[test]
    fn keys_encode_as_the_reference_terminal_sends_them() {
        let mut emulator = Emulator::new(80, 24, colors());
        let none = Modifiers::none();
        let control = Modifiers::control();
        let alt = Modifiers::alt();
        assert_eq!(encode(&mut emulator, "a", Some("a"), none), b"a");
        assert_eq!(
            encode(&mut emulator, "a", Some("A"), Modifiers::shift()),
            b"A"
        );
        assert_eq!(
            encode(&mut emulator, "1", Some("!"), Modifiers::shift()),
            b"!"
        );
        assert_eq!(encode(&mut emulator, "space", Some(" "), none), b" ");
        assert_eq!(encode(&mut emulator, "enter", None, none), b"\r");
        assert_eq!(encode(&mut emulator, "backspace", None, none), b"\x7f");
        assert_eq!(encode(&mut emulator, "tab", None, none), b"\t");
        assert_eq!(encode(&mut emulator, "escape", None, none), b"\x1b");
        assert_eq!(encode(&mut emulator, "c", None, control), b"\x03");
        assert_eq!(encode(&mut emulator, "d", None, control), b"\x04");
        assert_eq!(encode(&mut emulator, "b", Some("b"), alt), b"\x1bb");
        assert_eq!(encode(&mut emulator, "up", None, none), b"\x1b[A");
        assert_eq!(encode(&mut emulator, "home", None, none), b"\x1b[H");
        assert_eq!(encode(&mut emulator, "end", None, none), b"\x1b[F");
        assert_eq!(encode(&mut emulator, "f1", None, none), b"\x1bOP");
        assert_eq!(encode(&mut emulator, "f12", None, none), b"\x1b[24~");
        assert_eq!(encode(&mut emulator, "delete", None, none), b"\x1b[3~");
        assert_eq!(encode(&mut emulator, "pageup", None, none), b"\x1b[5~");
        emulator.feed(b"\x1b[?1h");
        assert_eq!(encode(&mut emulator, "up", None, none), b"\x1bOA", "DECCKM");
        assert_eq!(encode(&mut emulator, "home", None, none), b"\x1bOH");
        emulator.feed(b"\x1b[>1u");
        assert_eq!(
            encode(&mut emulator, "escape", None, none),
            b"\x1b[27u",
            "Kitty"
        );
        assert_eq!(encode(&mut emulator, "c", None, control), b"\x1b[99;5u");
        // Text outside the US layout types itself.
        assert_eq!(encode(&mut emulator, "é", Some("é"), none), "é".as_bytes());
    }

    #[test]
    fn app_shortcuts_stay_with_the_app() {
        let control = Modifiers::control();
        assert!(is_zoom_shortcut(&keystroke("=", None, control)));
        assert!(is_zoom_shortcut(&keystroke("0", None, control)));
        assert!(!is_zoom_shortcut(&keystroke(
            "0",
            None,
            Modifiers::control_shift()
        )));
        assert!(is_paste_shortcut(&keystroke(
            "v",
            None,
            Modifiers::control_shift()
        )));
        assert!(is_paste_shortcut(&keystroke(
            "insert",
            None,
            Modifiers::shift()
        )));
        assert!(!is_paste_shortcut(&keystroke("v", None, control)));
        assert!(is_copy_shortcut(&keystroke(
            "c",
            None,
            Modifiers::control_shift()
        )));
    }

    #[test]
    fn paste_is_bracketed_only_when_the_program_asks() {
        let mut emulator = Emulator::new(80, 24, colors());
        assert_eq!(emulator.encode_paste("a\nb"), b"a\rb");
        emulator.feed(b"\x1b[?2004h");
        assert_eq!(
            emulator.encode_paste("echo hi\nls"),
            b"\x1b[200~echo hi\nls\x1b[201~"
        );
        // An embedded end marker cannot close the bracket early.
        assert_eq!(
            emulator.encode_paste("x\x1b[201~rm -rf ~\n"),
            b"\x1b[200~x [201~rm -rf ~\n\x1b[201~"
        );
        let large = "line of pasted text\n".repeat(10_000);
        let encoded = emulator.encode_paste(&large);
        assert_eq!(
            encoded.len(),
            large.len() + 12,
            "a 200 KiB paste encodes whole"
        );
    }

    #[test]
    fn replay_answers_no_query_from_history_and_live_queries_after() {
        let mut emulator = Emulator::new(40, 10, colors());
        emulator.replay(
            "$ ls\r\n\x1b[6n\x1b[c\x1b[?2004$p\x1b[?u\x1b]11;?\x1b\\done\r\n",
            Some((40, 10)),
            (40, 10),
        );
        assert!(
            emulator.take_replies().is_empty(),
            "history reached the PTY"
        );
        assert_eq!(
            emulator.viewport_text()[..2],
            ["$ ls".to_owned(), "done".to_owned()]
        );
        emulator.feed(b"\x1b[6n");
        assert_eq!(emulator.take_replies(), b"\x1b[3;1R");
        emulator.feed(b"\x1b]10;?\x1b\\\x1b]11;?\x07");
        assert!(
            emulator.take_replies().is_empty(),
            "the server answers theme queries"
        );
    }

    #[test]
    fn replay_draws_at_the_recorded_size_then_reflows() {
        let mut emulator = Emulator::new(80, 10, colors());
        let line = "0123456789".repeat(10);
        // Drawn at 120 columns: one row. Replayed into 50 columns: two rows.
        emulator.replay(&format!("{line}\r\n$ "), Some((120, 10)), (50, 10));
        assert_eq!(emulator.size(), (50, 10));
        let text = emulator.viewport_text();
        assert_eq!(text[0], line[..50]);
        assert_eq!(text[1], line[50..]);
        assert_eq!(text[2], "$");
        assert_eq!(emulator.link_at((0, 0)), None, "no URL in plain digits");
    }

    #[test]
    fn grid_keeps_wide_cells_and_attributes() {
        let mut emulator = Emulator::new(40, 6, colors());
        emulator.feed("\x1b[31mred\x1b[0m \x1b[1;4mbold\x1b[0m \x1b[7mrev\x1b[0m\r\n".as_bytes());
        emulator.feed("日本\u{1f389}x\r\n".as_bytes());
        emulator
            .feed("e\u{301}\u{643}\u{62a}\u{627}\u{628} \u{2500}\u{2502}\u{e0b0}\r\n".as_bytes());
        let frame = emulator.frame();
        assert_eq!(frame.rows.len(), 6);
        assert_eq!(frame.rows[0][0].foreground, Rgb(1, 2, 3), "palette red");
        assert!(frame.rows[0][4].bold && frame.rows[0][4].underline);
        let reversed = &frame.rows[0][9];
        assert_eq!(reversed.background, Some(Rgb(0xdd, 0xdd, 0xdd)));
        assert_eq!(reversed.foreground, Rgb(0x11, 0x11, 0x11));
        let wide: Vec<(bool, bool)> = frame.rows[1][..7]
            .iter()
            .map(|cell| (cell.wide, cell.spacer))
            .collect();
        assert_eq!(
            wide,
            [
                (true, false),
                (false, true),
                (true, false),
                (false, true),
                (true, false),
                (false, true),
                (false, false)
            ]
        );
        assert_eq!(
            frame.rows[1][6].text, "x",
            "text after two CJK cells and an emoji lands at column 6"
        );
        assert_eq!(
            frame.rows[2][0].text, "e\u{301}",
            "a combining mark joins its base"
        );
        assert_eq!(
            emulator.viewport_text()[2],
            "e\u{301}\u{643}\u{62a}\u{627}\u{628} \u{2500}\u{2502}\u{e0b0}"
        );
        let cursor = frame.cursor.expect("a visible cursor");
        assert_eq!((cursor.col, cursor.row), (0, 3));
    }

    #[test]
    fn scrollback_keeps_ten_thousand_rows() {
        for cols in [80u16, 120] {
            let mut emulator = Emulator::new(cols, 24, colors());
            for line in 0..20_000 {
                emulator.feed(format!("{line}\r\n").as_bytes());
            }
            let kept = emulator.scrollback_rows();
            assert!(kept >= SCROLLBACK_ROWS, "{cols} columns kept {kept} rows");
            assert!(kept < 20_000, "{cols} columns kept everything");
            emulator.scroll_to_top();
            let position = emulator.scroll_position();
            assert_eq!(position.offset, 0);
            assert!(!position.at_bottom());
            let first: u64 = emulator.viewport_text()[0].parse().expect("a number");
            assert_eq!(first, 20_000 - 24 + 1 - kept as u64);
            emulator.scroll_to_bottom();
            assert!(emulator.scroll_position().at_bottom());
        }
    }

    #[test]
    fn selection_copies_unwrapped_text_without_padding() {
        let mut emulator = Emulator::new(10, 5, colors());
        emulator.feed("hello world again\r\n日本 x\r\n".as_bytes());
        // "hello worl" wraps into "d again".
        emulator.select((6, 0), (6, 1));
        assert_eq!(emulator.selection_text().as_deref(), Some("world again"));
        assert!(emulator.frame().rows[0][6].background == Some(Rgb(0x33, 0x55, 0x99)));
        emulator.select((0, 2), (5, 2));
        assert_eq!(emulator.selection_text().as_deref(), Some("日本 x"));
        emulator.select_unit((1, 0), false);
        assert_eq!(emulator.selection_text().as_deref(), Some("hello"));
        emulator.clear_selection();
        assert_eq!(emulator.selection_text(), None);
    }

    #[test]
    fn links_come_from_osc8_or_urls_across_wrapped_rows() {
        let mut emulator = Emulator::new(20, 5, colors());
        emulator.feed(b"see https://example.com/a/long/path. ok\r\n");
        emulator.feed(b"\x1b]8;;https://osc.example/x\x1b\\here\x1b]8;;\x1b\\\r\n");
        assert_eq!(
            emulator.link_at((10, 0)).as_deref(),
            Some("https://example.com/a/long/path")
        );
        assert_eq!(
            emulator.link_at((3, 1)).as_deref(),
            Some("https://example.com/a/long/path"),
            "the wrapped tail"
        );
        assert_eq!(emulator.link_at((0, 0)), None);
        assert_eq!(
            emulator.link_at((1, 2)).as_deref(),
            Some("https://osc.example/x")
        );
    }

    #[test]
    fn mouse_reports_when_the_program_tracks_it() {
        let mut emulator = Emulator::new(20, 5, colors());
        assert!(!emulator.mouse_tracking());
        emulator.feed(b"\x1b[?1000h\x1b[?1006h");
        assert!(emulator.mouse_tracking());
        emulator.resize(20, 5, 10, 20);
        let press = emulator.encode_mouse(
            MouseInput {
                action: mouse::Action::Press,
                button: Some(mouse::Button::Left),
                mods: Mods::empty(),
                x: 25.0,
                y: 65.0,
            },
            (200, 100),
        );
        assert_eq!(press, b"\x1b[<0;3;4M");
    }

    #[test]
    fn box_glyphs_fill_their_cell() {
        let (width, height) = (9.0, 19.0);
        let covered = |shapes: &[Shape]| {
            let (mut min_x, mut min_y, mut max_x, mut max_y) =
                (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for shape in shapes {
                if let Shape::Rect {
                    x,
                    y,
                    width,
                    height,
                    ..
                } = shape
                {
                    min_x = min_x.min(*x);
                    min_y = min_y.min(*y);
                    max_x = max_x.max(x + width);
                    max_y = max_y.max(y + height);
                }
            }
            (min_x, min_y, max_x, max_y)
        };
        let horizontal = procedural_shapes('\u{2500}', width, height, 1.0).expect("drawn");
        let (min_x, _, max_x, _) = covered(&horizontal);
        assert_eq!((min_x, max_x), (0.0, width), "─ spans the cell");
        let vertical = procedural_shapes('\u{2502}', width, height, 1.0).expect("drawn");
        let (_, min_y, _, max_y) = covered(&vertical);
        assert_eq!((min_y, max_y), (0.0, height), "│ spans the cell");
        let cross = procedural_shapes('\u{253c}', width, height, 1.0).expect("drawn");
        assert_eq!(covered(&cross), (0.0, 0.0, width, height));
        assert_eq!(
            procedural_shapes('\u{2588}', width, height, 1.0),
            Some(vec![Shape::Rect {
                x: 0.0,
                y: 0.0,
                width,
                height,
                alpha: 1.0
            }]),
            "█ fills the cell"
        );
        let heavy = procedural_shapes('\u{2501}', width, height, 1.0).expect("drawn");
        assert!(matches!(heavy[0], Shape::Rect { height: thickness, .. } if thickness >= 2.0));
        assert_eq!(
            procedural_shapes('\u{2550}', width, height, 1.0)
                .expect("double")
                .len(),
            4
        );
        assert!(matches!(
            procedural_shapes('\u{e0b0}', width, height, 1.0).as_deref(),
            Some([Shape::Polygon(points)]) if points == &vec![(0.0, 0.0), (width, height / 2.0), (0.0, height)]
        ));
        assert!(procedural_shapes('a', width, height, 1.0).is_none());
        for code in (0x2500..=0x259f).chain(0xe0b0..=0xe0b3) {
            let character = char::from_u32(code).expect("valid");
            assert!(
                procedural_shapes(character, width, height, 1.0).is_some(),
                "{code:x} is drawn"
            );
        }
    }
}
