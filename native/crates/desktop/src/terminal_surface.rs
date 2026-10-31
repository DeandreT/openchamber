//! One terminal's surface: the painted grid, keyboard, IME, mouse,
//! selection, scrolling and fit.
//!
//! Mirrors `packages/ui/src/lib/ghostty/surface.ts` and
//! `components/terminal/TerminalViewport.tsx`. The emulator lives in
//! [`Emulator`]; this entity feeds it, paints its [`Frame`] and turns input
//! into PTY bytes through Ghostty's encoders. A surface the view does not
//! render keeps parsing output and answering queries, and paints nothing.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{
    App, Bounds, ClipboardItem, Context, Element, ElementId, ElementInputHandler,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font, FontStyle, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, InteractiveElement, IntoElement, KeyDownEvent,
    KeyUpEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    Path, Pixels, Point, Render, Role, ScrollWheelEvent, SharedString, StatefulInteractiveElement,
    Style, Styled, Task, TestSupportExt, TextAlign, TextRun, UTF16Selection, UnderlineStyle,
    Window, div, fill, point, px, rgb, size,
};
use libghostty_vt::key::Mods;
use libghostty_vt::mouse;
use openchamber_native_transport::terminal::TerminalCommands;

use crate::terminal_emulator::{
    CursorShape, Emulator, Frame, MouseInput, Rgb, Shape, TerminalColors, is_copy_shortcut,
    is_paste_shortcut, is_procedural, is_zoom_shortcut, key_input, procedural_shapes,
};
use crate::theme::Palette;

/// The surface's key context; it outranks the workspace's bindings so the
/// shell receives Escape, Ctrl+K and the Alt arrows.
pub(crate) const KEY_CONTEXT: &str = "Terminal";
/// Space between the grid and the surface's edge.
const PADDING: f32 = 4.0;
/// The grid settles before the PTY learns its size (`surface.ts`).
const RESIZE_SETTLE: Duration = Duration::from_millis(150);
const RESIZE_RETRY_BASE: Duration = Duration::from_millis(250);
const RESIZE_MAX_RETRIES: u8 = 3;
/// Lines per wheel notch when the wheel scrolls in lines.
const WHEEL_LINES: f32 = 3.0;
const SCROLLBAR_WIDTH: f32 = 6.0;

/// What the surface tells its view.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SurfaceEvent {
    /// The fitted grid settled at a new size.
    Resized { cols: u16, rows: u16 },
    /// A secondary click the program does not own; the view shows Copy and Paste.
    ContextMenu { position: Point<Pixels> },
    /// Copy or paste failed; the view reports it.
    ClipboardFailed { paste: bool },
}

impl EventEmitter<SurfaceEvent> for TerminalSurface {}

/// The cell grid's metrics for the current font.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CellMetrics {
    width: Pixels,
    height: Pixels,
}

/// A pointer drag in progress.
#[derive(Clone, Copy, Debug)]
enum Drag {
    Select { anchor: (u16, u16) },
    Report,
}

/// Sends PTY input; the panel binds it to the socket and the terminal.
pub(crate) type Writer = Rc<dyn Fn(String)>;

/// Identifies one HTTP resize and the authoritative snapshot it followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResizeRequest {
    sequence: u64,
    revision: u64,
    size: (u16, u16),
}

pub(crate) struct TerminalSurface {
    emulator: Emulator,
    focus: FocusHandle,
    /// Where input goes: the server terminal's socket.
    writer: Option<Writer>,
    font: Font,
    font_size: Pixels,
    metrics: Option<CellMetrics>,
    /// The grid's bounds in the last painted frame.
    grid_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The size the server's PTY is known to have: from its snapshot, its
    /// create answer or the last accepted resize. `None` until it has one.
    pty_size: Option<(u16, u16)>,
    /// Only one request may be in flight, even across a newer snapshot.
    requested: Option<ResizeRequest>,
    resize_sequence: u64,
    resize_revision: u64,
    resize_failures: u8,
    retry_waiting: bool,
    force_resize: bool,
    resizing_stopped: bool,
    resize_declined: bool,
    resize_http: Option<Task<()>>,
    /// The size the PTY should get once the grid settles.
    settling: Option<(u16, u16)>,
    resize_task: Option<Task<()>>,
    /// IME composition shown at the cursor, not yet sent.
    marked: Option<String>,
    drag: Option<Drag>,
    /// Ctrl+C copied, so the next Ctrl+C clears the selection and reaches
    /// the shell (`clearSelectionAfterCopy`).
    has_selection: bool,
    /// Keys whose release the shell must not see because their press went
    /// elsewhere.
    suppressed_release: Vec<String>,
}

impl std::fmt::Debug for TerminalSurface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalSurface")
            .field("emulator", &self.emulator)
            .finish_non_exhaustive()
    }
}

impl Focusable for TerminalSurface {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The theme's terminal colors, as `convertThemeToXterm` maps the web theme.
pub(crate) fn terminal_colors(palette: &Palette) -> TerminalColors {
    let rgb = |color: Hsla| {
        let color = color.to_rgb();
        Rgb(
            (color.r * 255.0).round() as u8,
            (color.g * 255.0).round() as u8,
            (color.b * 255.0).round() as u8,
        )
    };
    let [keyword, kind, function, ..] = palette.agent_syntax;
    let normal = [
        palette.muted,
        palette.status_error,
        palette.status_success,
        palette.status_warning,
        function,
        keyword,
        kind,
        palette.syntax_foreground,
    ];
    let bright = [
        palette.syntax_comment,
        palette.status_error,
        palette.status_success,
        palette.status_warning,
        function,
        keyword,
        kind,
        palette.elevated_foreground,
    ];
    let mut ansi = [Rgb(0, 0, 0); 16];
    for (slot, color) in ansi.iter_mut().zip(normal.into_iter().chain(bright)) {
        *slot = rgb(color);
    }
    TerminalColors {
        background: rgb(palette.background),
        foreground: rgb(palette.syntax_foreground),
        cursor: rgb(palette.cursor),
        selection: rgb(palette.selection),
        selection_foreground: rgb(palette.selection_foreground),
        ansi,
    }
}

/// The cell grid a box of `size` holds, as `TerminalViewport` measures it.
pub(crate) fn fit(size: gpui_kit::Size<Pixels>, cell: (f32, f32)) -> (u16, u16) {
    let cols = ((f32::from(size.width) - 2.0 * PADDING) / cell.0).floor();
    let rows = ((f32::from(size.height) - 2.0 * PADDING) / cell.1).floor();
    (
        cols.clamp(2.0, 1000.0) as u16,
        rows.clamp(1.0, 500.0) as u16,
    )
}

impl TerminalSurface {
    pub fn new(
        cols: u16,
        rows: u16,
        font: Font,
        font_size: f32,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            emulator: Emulator::new(cols, rows, terminal_colors(palette)),
            focus: cx.focus_handle(),
            writer: None,
            font,
            font_size: px(font_size),
            metrics: None,
            grid_bounds: Rc::new(Cell::new(Bounds::default())),
            pty_size: None,
            requested: None,
            resize_sequence: 0,
            resize_revision: 0,
            resize_failures: 0,
            retry_waiting: false,
            force_resize: false,
            resizing_stopped: false,
            resize_declined: false,
            resize_http: None,
            settling: None,
            resize_task: None,
            marked: None,
            drag: None,
            has_selection: false,
            suppressed_release: Vec::new(),
        }
    }

    /// Connects input to a server terminal.
    pub fn set_writer(&mut self, commands: TerminalCommands, session: String) {
        self.writer = Some(Rc::new(move |data| commands.write(&session, data)));
    }

    #[cfg(test)]
    pub fn set_test_writer(&mut self, writer: Writer) {
        self.writer = Some(writer);
    }

    pub fn focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn grid_size(&self) -> (u16, u16) {
        self.emulator.size()
    }

    /// Whether the grid was measured from a painted frame yet.
    pub fn measured(&self) -> bool {
        self.metrics.is_some()
    }

    /// Live output. Replies the emulator owes go back to the PTY.
    pub fn feed(&mut self, data: &str, cx: &mut Context<Self>) {
        self.emulator.feed(data.as_bytes());
        self.flush_replies();
        cx.notify();
    }

    /// Replaces the screen with a snapshot's history.
    pub fn replay(&mut self, history: &str, recorded: Option<(u16, u16)>, cx: &mut Context<Self>) {
        let fit = self.emulator.size();
        self.emulator.replay(history, recorded, fit);
        self.has_selection = false;
        if let Some(size) = recorded {
            // A snapshot names the PTY's current size, which another client
            // may have changed while this one was away.
            self.set_pty_size(size, cx);
        }
        cx.notify();
    }

    /// Records the server PTY's size and reports the fitted grid when they
    /// differ.
    pub fn set_pty_size(&mut self, size: (u16, u16), cx: &mut Context<Self>) {
        self.pty_size = Some(size);
        self.resize_revision += 1;
        self.resize_failures = 0;
        self.force_resize = false;
        self.retry_waiting = false;
        self.settling = None;
        self.resize_task = None;
        self.reconcile_size(cx);
    }

    pub fn pending_resize(&self) -> Option<ResizeRequest> {
        self.requested
    }

    /// The panel declined dispatch because the process is not running. Keep
    /// the fitted grid, but wait for a running event before proposing it again.
    pub fn decline_resize(&mut self) {
        self.requested = None;
        self.resize_declined = true;
        self.resize_task = None;
        self.settling = None;
        self.retry_waiting = false;
    }

    pub fn resume_resizing(&mut self, cx: &mut Context<Self>) {
        self.resize_declined = false;
        self.reconcile_size(cx);
    }

    pub fn retain_resize_http(&mut self, task: Task<()>) {
        self.resize_http = Some(task);
    }

    /// A late or duplicate completion cannot clear a newer request. A snapshot
    /// received during HTTP keeps the serial gate closed until HTTP completes;
    /// its old acknowledgement then requires a fresh write of the latest size.
    pub fn resize_finished(
        &mut self,
        request: ResizeRequest,
        succeeded: bool,
        cx: &mut Context<Self>,
    ) {
        if self.resizing_stopped || self.requested != Some(request) {
            return;
        }
        self.requested = None;
        self.resize_http = None;
        if succeeded {
            self.resize_failures = 0;
            self.force_resize = request.revision != self.resize_revision;
            if !self.force_resize {
                self.pty_size = Some(request.size);
            }
            self.reconcile_size(cx);
            return;
        }
        // A lost response may have applied the resize. Retrying the latest
        // size is idempotent, including when it equals our older known size.
        self.force_resize = true;
        self.resize_failures += 1;
        if self.resize_failures > RESIZE_MAX_RETRIES {
            return;
        }
        self.retry_waiting = true;
        let delay = RESIZE_RETRY_BASE * (1 << (self.resize_failures - 1));
        self.resize_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                this.retry_waiting = false;
                this.emit_resize(cx);
            });
        }));
    }

    /// Cancel timers and owned HTTP on close or runtime invalidation, even if
    /// another view still retains this entity.
    pub fn stop_resizing(&mut self) {
        self.resizing_stopped = true;
        self.resize_revision += 1;
        self.requested = None;
        self.resize_task = None;
        self.resize_http = None;
        self.retry_waiting = false;
        self.settling = None;
    }

    fn flush_replies(&mut self) {
        let replies = self.emulator.take_replies();
        if !replies.is_empty() {
            self.write(&replies);
        }
    }

    fn write(&self, bytes: &[u8]) {
        if let Some(writer) = self.writer.as_ref().filter(|_| !bytes.is_empty()) {
            writer(String::from_utf8_lossy(bytes).into_owned());
        }
    }

    /// Writes typed input and returns the view to the live screen.
    fn send_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        self.emulator.scroll_to_bottom();
        self.write(bytes);
        cx.notify();
    }

    /// The one paste path: shortcut and menu both encode through Ghostty.
    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            cx.emit(SurfaceEvent::ClipboardFailed { paste: true });
            return;
        };
        let encoded = self.emulator.encode_paste(&text);
        self.send_input(&encoded, cx);
    }

    pub fn has_selection(&self) -> bool {
        self.has_selection
    }

    /// Copies the selection. Answers whether anything was copied.
    pub fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.emulator.selection_text() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.has_selection {
            self.emulator.clear_selection();
            self.has_selection = false;
            cx.notify();
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // A composition in progress belongs to the input method.
        if self.marked.is_some() {
            return;
        }
        let keystroke = &event.keystroke;
        if is_copy_shortcut(keystroke) && self.has_selection {
            let copied = self.copy(cx);
            if !keystroke.modifiers.shift {
                // Plain Ctrl+C copies once; the next one is SIGINT.
                self.clear_selection(cx);
            }
            if copied {
                self.suppressed_release.push(keystroke.key.clone());
                cx.stop_propagation();
                return;
            }
        }
        if is_paste_shortcut(keystroke) {
            self.suppressed_release.push(keystroke.key.clone());
            self.paste(cx);
            cx.stop_propagation();
            return;
        }
        if is_zoom_shortcut(keystroke) || keystroke.modifiers.platform {
            self.suppressed_release.push(keystroke.key.clone());
            return;
        }
        let Some(input) = key_input(keystroke) else {
            return;
        };
        let bytes = self.emulator.encode_key(&input, false);
        if bytes.is_empty() {
            return;
        }
        self.suppressed_release.retain(|key| key != &keystroke.key);
        self.clear_selection(cx);
        self.send_input(&bytes, cx);
        window.prevent_default();
        cx.stop_propagation();
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if let Some(index) = self
            .suppressed_release
            .iter()
            .position(|key| key == &keystroke.key)
        {
            self.suppressed_release.remove(index);
            return;
        }
        if let Some(input) = key_input(keystroke) {
            // Nothing unless the program asked for release events (Kitty).
            let bytes = self.emulator.encode_key(&input, true);
            if !bytes.is_empty() {
                self.write(&bytes);
                cx.stop_propagation();
            }
        }
    }

    /// The viewport cell under a window position, clamped to the grid.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let metrics = self.metrics?;
        let bounds = self.grid_bounds.get();
        let (cols, rows) = self.emulator.size();
        let x = f32::from(position.x - bounds.origin.x) / f32::from(metrics.width);
        let y = f32::from(position.y - bounds.origin.y) / f32::from(metrics.height);
        Some((
            (x.max(0.0) as u16).min(cols.saturating_sub(1)),
            (y.max(0.0) as u16).min(rows.saturating_sub(1)),
        ))
    }

    fn mouse_input(
        &self,
        action: mouse::Action,
        button: Option<mouse::Button>,
        modifiers: &gpui_kit::Modifiers,
        position: Point<Pixels>,
    ) -> MouseInput {
        let bounds = self.grid_bounds.get();
        let mut mods = Mods::empty();
        mods.set(Mods::SHIFT, modifiers.shift);
        mods.set(Mods::CTRL, modifiers.control);
        mods.set(Mods::ALT, modifiers.alt);
        MouseInput {
            action,
            button,
            mods,
            x: f32::from(position.x - bounds.origin.x).max(0.0),
            y: f32::from(position.y - bounds.origin.y).max(0.0),
        }
    }

    fn grid_pixels(&self) -> (u32, u32) {
        let bounds = self.grid_bounds.get();
        (
            f32::from(bounds.size.width) as u32,
            f32::from(bounds.size.height) as u32,
        )
    }

    /// The program owns the mouse unless Shift asks for a selection.
    fn reports_mouse(&self, modifiers: &gpui_kit::Modifiers) -> bool {
        self.emulator.mouse_tracking() && !modifiers.shift
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        let button = match event.button {
            MouseButton::Left => mouse::Button::Left,
            MouseButton::Middle => mouse::Button::Middle,
            MouseButton::Right => mouse::Button::Right,
            _ => return,
        };
        if self.reports_mouse(&event.modifiers) {
            let input = self.mouse_input(
                mouse::Action::Press,
                Some(button),
                &event.modifiers,
                event.position,
            );
            let bytes = self.emulator.encode_mouse(input, self.grid_pixels());
            self.write(&bytes);
            self.drag = Some(Drag::Report);
            cx.stop_propagation();
            return;
        }
        let Some(cell) = self.cell_at(event.position) else {
            return;
        };
        match event.button {
            MouseButton::Right => {
                cx.emit(SurfaceEvent::ContextMenu {
                    position: event.position,
                });
            }
            MouseButton::Left if event.modifiers.control => {
                if let Some(url) = self.emulator.link_at(cell) {
                    cx.open_url(&url);
                }
            }
            MouseButton::Left => match event.click_count {
                1 => {
                    self.clear_selection(cx);
                    self.drag = Some(Drag::Select { anchor: cell });
                }
                count => {
                    self.emulator.select_unit(cell, count >= 3);
                    self.has_selection = self.emulator.selection_text().is_some();
                    cx.notify();
                }
            },
            _ => {}
        }
        cx.stop_propagation();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        match self.drag {
            Some(Drag::Report) => {
                let input = self.mouse_input(
                    mouse::Action::Motion,
                    event.pressed_button.map(|_| mouse::Button::Left),
                    &event.modifiers,
                    event.position,
                );
                let bytes = self.emulator.encode_mouse(input, self.grid_pixels());
                self.write(&bytes);
            }
            Some(Drag::Select { anchor }) if event.pressed_button == Some(MouseButton::Left) => {
                let Some(head) = self.cell_at(event.position) else {
                    return;
                };
                self.emulator.select(anchor, head);
                self.has_selection = true;
                cx.notify();
            }
            _ if self.emulator.mouse_tracking() && event.pressed_button.is_none() => {
                // Any-motion tracking (mode 1003) reports hover; the encoder
                // drops it for the other modes.
                let input = self.mouse_input(
                    mouse::Action::Motion,
                    None,
                    &event.modifiers,
                    event.position,
                );
                let bytes = self.emulator.encode_mouse(input, self.grid_pixels());
                self.write(&bytes);
            }
            _ => {}
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(Drag::Report) = self.drag.take() {
            let button = match event.button {
                MouseButton::Left => mouse::Button::Left,
                MouseButton::Middle => mouse::Button::Middle,
                MouseButton::Right => mouse::Button::Right,
                _ => return,
            };
            let input = self.mouse_input(
                mouse::Action::Release,
                Some(button),
                &event.modifiers,
                event.position,
            );
            let bytes = self.emulator.encode_mouse(input, self.grid_pixels());
            self.write(&bytes);
            cx.stop_propagation();
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(metrics) = self.metrics else {
            return;
        };
        let lines =
            f32::from(event.delta.pixel_delta(metrics.height).y) / f32::from(metrics.height);
        let lines = if lines.abs() < 1.0 && lines != 0.0 {
            lines.signum() * WHEEL_LINES
        } else {
            lines
        };
        let steps = lines.round() as isize;
        if steps == 0 {
            return;
        }
        if self.reports_mouse(&event.modifiers) {
            let button = if steps > 0 {
                mouse::Button::Four
            } else {
                mouse::Button::Five
            };
            for _ in 0..steps.unsigned_abs().min(10) {
                let input = self.mouse_input(
                    mouse::Action::Press,
                    Some(button),
                    &event.modifiers,
                    event.position,
                );
                let bytes = self.emulator.encode_mouse(input, self.grid_pixels());
                self.write(&bytes);
            }
        } else {
            // A positive delta scrolls up, toward history.
            self.emulator.scroll(-steps);
            cx.notify();
        }
        cx.stop_propagation();
    }

    /// Fits the grid to the painted bounds. A changed size reflows at once.
    fn fit_to(&mut self, cols: u16, rows: u16, metrics: CellMetrics, cx: &mut Context<Self>) {
        self.metrics = Some(metrics);
        if self.emulator.size() != (cols, rows) {
            self.resize_failures = 0;
        }
        self.emulator.resize(
            cols,
            rows,
            f32::from(metrics.width).round() as u32,
            f32::from(metrics.height).round() as u32,
        );
        self.reconcile_size(cx);
    }

    /// Reports the fitted grid once it has held for [`RESIZE_SETTLE`] while
    /// it differs from the PTY's known size and no request is in flight. An
    /// unmeasured grid, or a PTY of unknown size, reports nothing, so a
    /// provisional size never reaches an existing PTY.
    fn reconcile_size(&mut self, cx: &mut Context<Self>) {
        if self.resizing_stopped
            || self.resize_declined
            || self.requested.is_some()
            || self.retry_waiting
            || self.resize_failures > RESIZE_MAX_RETRIES
        {
            return;
        }
        let target = self
            .metrics
            .is_some()
            .then(|| self.emulator.size())
            .filter(|size| {
                self.pty_size
                    .is_some_and(|pty| pty != *size || self.force_resize)
            });
        if target.is_none() {
            self.settling = None;
            self.resize_task = None;
            return;
        }
        if self.settling == target {
            return;
        }
        self.settling = target;
        self.resize_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESIZE_SETTLE).await;
            let _ = this.update(cx, |this, cx| {
                this.settling = None;
                this.emit_resize(cx);
            });
        }));
    }

    fn emit_resize(&mut self, cx: &mut Context<Self>) {
        if self.resizing_stopped
            || self.resize_declined
            || self.requested.is_some()
            || self.metrics.is_none()
        {
            return;
        }
        let (cols, rows) = self.emulator.size();
        if !self
            .pty_size
            .is_some_and(|pty| pty != (cols, rows) || self.force_resize)
        {
            return;
        }
        self.resize_sequence += 1;
        self.requested = Some(ResizeRequest {
            sequence: self.resize_sequence,
            revision: self.resize_revision,
            size: (cols, rows),
        });
        cx.emit(SurfaceEvent::Resized { cols, rows });
    }

    /// The cell metrics of the surface's font, as `TerminalViewport` measures
    /// the grid: the advance of `M`, and a line height of 1.35 times the
    /// font size, never shorter than the glyph box.
    fn measure(&self, window: &Window) -> CellMetrics {
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&self.font);
        let width = text_system
            .advance(font_id, self.font_size, 'M')
            .map(|advance| advance.width)
            .unwrap_or(self.font_size * 0.6);
        let glyph = text_system.ascent(font_id, self.font_size)
            + text_system.descent(font_id, self.font_size).abs();
        let height = px((f32::from(self.font_size) * 1.35).round()).max(glyph.ceil());
        CellMetrics { width, height }
    }
}

impl EntityInputHandler for TerminalSurface {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        adjusted: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let marked = self.marked.as_deref().unwrap_or("");
        let units: Vec<u16> = marked.encode_utf16().collect();
        let end = range.end.min(units.len());
        let start = range.start.min(end);
        *adjusted = Some(start..end);
        Some(String::from_utf16_lossy(&units[start..end]))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let len = self
            .marked
            .as_deref()
            .map_or(0, |text| text.encode_utf16().count());
        Some(UTF16Selection {
            range: len..len,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.marked
            .as_deref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.marked.take().is_some() {
            cx.notify();
        }
    }

    /// Committed text from the input method, or a typed character no key
    /// handler encoded, reaches the PTY as text.
    fn replace_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        self.clear_selection(cx);
        self.send_input(text.as_bytes(), cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        _: Option<std::ops::Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!text.is_empty()).then(|| text.to_owned());
        cx.notify();
    }

    /// The cursor cell, where the input method places its candidates.
    fn bounds_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let metrics = self.metrics?;
        let grid = self.grid_bounds.get();
        let cursor = self.emulator.frame().cursor?;
        Some(Bounds::new(
            point(
                grid.origin.x + metrics.width * f32::from(cursor.col),
                grid.origin.y + metrics.height * f32::from(cursor.row),
            ),
            size(metrics.width, metrics.height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for TerminalSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = cx.global::<Palette>().clone();
        let position = self.emulator.scroll_position();
        let strings = crate::locale::strings(cx);
        let scrollbar = (position.total > position.len).then(|| {
            let total = position.total as f32;
            let start = position.offset as f32 / total;
            let length = (position.len as f32 / total).max(0.05);
            div()
                .id("terminal-scrollbar")
                .role(Role::ScrollBar)
                .accessibility_id("terminal-scrollbar")
                .aria_label(strings.get("terminalView.viewport.scrollbarAria"))
                .aria_orientation(gpui_kit::accesskit::Orientation::Vertical)
                .aria_min_numeric_value(0.0)
                .aria_max_numeric_value(position.total.saturating_sub(position.len) as f64)
                .aria_numeric_value(position.offset as f64)
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(SCROLLBAR_WIDTH + 2.0))
                .child(
                    div()
                        .absolute()
                        .right(px(1.0))
                        .w(px(SCROLLBAR_WIDTH))
                        .top(gpui_kit::relative(start))
                        .h(gpui_kit::relative(length))
                        .rounded_full()
                        .bg(palette.muted_foreground.opacity(0.4)),
                )
        });
        let _ = window;
        div()
            .id("terminal-surface")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            // The painted grid is hidden from the tree; the focusable input
            // carries the name (`surface.ts`).
            .role(Role::Terminal)
            .accessibility_id("terminal-surface")
            .aria_label(strings.get("terminalView.viewport.inputAria"))
            .relative()
            .size_full()
            .overflow_hidden()
            .cursor_text()
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(GridElement {
                surface: cx.entity(),
            })
            .children(scrollbar)
            .test_support()
    }
}

/// Paints the grid and registers the input handler.
struct GridElement {
    surface: gpui_kit::Entity<TerminalSurface>,
}

struct Prepared {
    frame: Frame,
    metrics: CellMetrics,
    marked: Option<String>,
    font: Font,
    font_size: Pixels,
    focused: bool,
}

impl IntoElement for GridElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for GridElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<Prepared>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui_kit::relative(1.0).into();
        style.size.height = gpui_kit::relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let metrics = self.surface.read(cx).measure(window);
        let (cols, rows) = fit(
            bounds.size,
            (f32::from(metrics.width), f32::from(metrics.height)),
        );
        let grid = Bounds::new(
            point(bounds.origin.x + px(PADDING), bounds.origin.y + px(PADDING)),
            size(
                metrics.width * f32::from(cols),
                metrics.height * f32::from(rows),
            ),
        );
        let focused = self.surface.read(cx).focus.is_focused(window);
        Some(self.surface.update(cx, |surface, cx| {
            surface.grid_bounds.set(grid);
            surface.fit_to(cols, rows, metrics, cx);
            Prepared {
                frame: surface.emulator.frame(),
                metrics,
                marked: surface.marked.clone(),
                font: surface.font.clone(),
                font_size: surface.font_size,
                focused,
            }
        }))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepared: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(prepared) = prepared.take() else {
            return;
        };
        let focus = self.surface.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.surface.clone()),
            cx,
        );
        let color = |Rgb(r, g, b): Rgb| -> Hsla {
            rgb((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)).into()
        };
        if let Some(background) = prepared.frame.background {
            window.paint_quad(fill(bounds, color(background)));
        }
        let origin = point(bounds.origin.x + px(PADDING), bounds.origin.y + px(PADDING));
        let CellMetrics { width, height } = prepared.metrics;
        let stroke = (f32::from(prepared.font_size) / 14.0).round().max(1.0);
        for (row_index, row) in prepared.frame.rows.iter().enumerate() {
            let y = origin.y + height * row_index as f32;
            // Backgrounds, merged into runs.
            let mut col = 0;
            while col < row.len() {
                let Some(background) = row[col].background else {
                    col += 1;
                    continue;
                };
                let start = col;
                while col < row.len() && row[col].background == Some(background) {
                    col += 1;
                }
                window.paint_quad(fill(
                    Bounds::new(
                        point(origin.x + width * start as f32, y),
                        size(width * (col - start) as f32, height),
                    ),
                    color(background),
                ));
            }
            // Text runs of one style; wide and procedural cells stand alone.
            let mut col = 0;
            while col < row.len() {
                let cell = &row[col];
                let first = cell.text.chars().next();
                if cell.spacer || first.is_none() || cell.text == " " {
                    col += 1;
                    continue;
                }
                let x = origin.x + width * col as f32;
                if let Some(character) = first.filter(|character| is_procedural(*character))
                    && let Some(shapes) =
                        procedural_shapes(character, f32::from(width), f32::from(height), stroke)
                {
                    paint_shapes(&shapes, point(x, y), color(cell.foreground), window);
                    col += 1;
                    continue;
                }
                let style_of = |cell: &crate::terminal_emulator::FrameCell| {
                    (
                        cell.foreground,
                        cell.bold,
                        cell.italic,
                        cell.faint,
                        cell.underline,
                        cell.strikethrough,
                    )
                };
                let key = style_of(cell);
                let mut text = String::new();
                let start = col;
                loop {
                    let current = &row[col];
                    text.push_str(if current.text.is_empty() {
                        " "
                    } else {
                        &current.text
                    });
                    col += 1;
                    if current.wide
                        || col >= row.len()
                        || row[col].spacer
                        || row[col].wide
                        || style_of(&row[col]) != key
                        || row[col].text.chars().next().is_some_and(is_procedural)
                    {
                        break;
                    }
                }
                let mut font = prepared.font.clone();
                if cell.bold {
                    font.weight = FontWeight::BOLD;
                }
                if cell.italic {
                    font.style = FontStyle::Italic;
                }
                let mut foreground = color(cell.foreground);
                if cell.faint {
                    foreground = foreground.opacity(0.5);
                }
                let run = TextRun {
                    len: text.len(),
                    font,
                    color: foreground,
                    background_color: None,
                    underline: cell.underline.then(|| UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(foreground),
                        wavy: false,
                    }),
                    strikethrough: cell.strikethrough.then(|| gpui_kit::StrikethroughStyle {
                        thickness: px(1.0),
                        color: Some(foreground),
                    }),
                };
                let advance = if row[start].wide { width * 2.0 } else { width };
                let shaped = window.text_system().shape_line(
                    SharedString::from(text),
                    prepared.font_size,
                    &[run],
                    Some(advance),
                );
                let _ = shaped.paint(point(x, y), height, TextAlign::Left, None, window, cx);
            }
        }
        if let Some(cursor) = prepared.frame.cursor {
            let cell = Bounds::new(
                point(
                    origin.x + width * f32::from(cursor.col),
                    origin.y + height * f32::from(cursor.row),
                ),
                size(width, height),
            );
            let cursor_color = color(cursor.color);
            let shape = if prepared.focused {
                cursor.shape
            } else {
                CursorShape::Hollow
            };
            match shape {
                CursorShape::Block => window.paint_quad(fill(cell, cursor_color.opacity(0.7))),
                CursorShape::Bar => window.paint_quad(fill(
                    Bounds::new(cell.origin, size(px(stroke.max(2.0)), height)),
                    cursor_color,
                )),
                CursorShape::Underline => window.paint_quad(fill(
                    Bounds::new(
                        point(cell.origin.x, cell.origin.y + height - px(2.0)),
                        size(width, px(2.0)),
                    ),
                    cursor_color,
                )),
                CursorShape::Hollow => window.paint_quad(gpui_kit::outline(
                    cell,
                    cursor_color,
                    gpui_kit::BorderStyle::Solid,
                )),
            }
            if let Some(marked) = prepared.marked {
                // The composition draws over the cursor, underlined.
                let foreground = color(cursor.color);
                let run = TextRun {
                    len: marked.len(),
                    font: prepared.font.clone(),
                    color: foreground,
                    background_color: prepared.frame.background.map(color),
                    underline: Some(UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(foreground),
                        wavy: false,
                    }),
                    strikethrough: None,
                };
                let shaped = window.text_system().shape_line(
                    SharedString::from(marked),
                    prepared.font_size,
                    &[run],
                    None,
                );
                let _ = shaped.paint(cell.origin, height, TextAlign::Left, None, window, cx);
            }
        }
    }
}

fn paint_shapes(shapes: &[Shape], origin: Point<Pixels>, color: Hsla, window: &mut Window) {
    for shape in shapes {
        match shape {
            Shape::Rect {
                x,
                y,
                width,
                height,
                alpha,
            } => window.paint_quad(fill(
                Bounds::new(
                    point(origin.x + px(*x), origin.y + px(*y)),
                    size(px(*width), px(*height)),
                ),
                color.opacity(*alpha),
            )),
            Shape::Polygon(points) => {
                let mut points = points.iter();
                let Some((x, y)) = points.next() else {
                    continue;
                };
                let mut path = Path::new(point(origin.x + px(*x), origin.y + px(*y)));
                for (x, y) in points {
                    path.line_to(point(origin.x + px(*x), origin.y + px(*y)));
                }
                window.paint_path(path, color);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        AnyWindowHandle, AppContext, Entity, TestAppContext, WindowBounds, WindowOptions,
    };

    use super::*;
    use crate::locale::Strings;
    use crate::theme::{self, Appearance};

    /// The workspace context around a surface, with the workspace's own
    /// Escape action, so tests see which one a key reaches.
    struct Host {
        surface: Entity<TerminalSurface>,
        transcript: Option<Entity<gpui_kit::base::text::TextViewState>>,
        width: f32,
        stops: usize,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .key_context("OpenChamberWorkspace")
                .on_action(
                    cx.listener(|this, _: &crate::workspace_view::StopGenerating, _, _| {
                        this.stops += 1;
                    }),
                )
                .size_full()
                .children(
                    self.transcript
                        .as_ref()
                        .map(gpui_kit::base::text::TextView::new),
                )
                .child(
                    div()
                        .w(px(self.width))
                        .h(px(300.0))
                        .child(self.surface.clone()),
                )
        }
    }

    struct Opened {
        window: AnyWindowHandle,
        host: Entity<Host>,
        surface: Entity<TerminalSurface>,
        written: Rc<RefCell<Vec<String>>>,
        events: Rc<RefCell<Vec<SurfaceEvent>>>,
    }

    fn open(cx: &mut TestAppContext) -> Opened {
        let written = Rc::new(RefCell::new(Vec::new()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let (window, host, surface) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::load("en"));
            theme::apply(Palette::default_for(Appearance::Dark), cx);
            crate::workspace_view::bind_keys(cx);
            let written = Rc::clone(&written);
            let events = Rc::clone(&events);
            let mut surface_slot = None;
            let (window, host) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.0), px(0.0)),
                        size: size(px(800.0), px(600.0)),
                    })),
                    ..WindowOptions::default()
                },
                cx,
                |window, cx| {
                    let palette = cx.global::<Palette>().clone();
                    let surface = cx.new(|cx| {
                        let mut surface = TerminalSurface::new(
                            80,
                            24,
                            gpui_kit::font("Kit Mono"),
                            14.0,
                            &palette,
                            cx,
                        );
                        let sink = Rc::clone(&written);
                        surface.set_test_writer(Rc::new(move |data| sink.borrow_mut().push(data)));
                        surface
                    });
                    cx.subscribe(&surface, move |_, event, _| {
                        events.borrow_mut().push(event.clone())
                    })
                    .detach();
                    window.focus(&surface.read(cx).focus(), cx);
                    surface_slot = Some(surface.clone());
                    cx.new(|_| Host {
                        surface,
                        transcript: None,
                        width: 400.0,
                        stops: 0,
                    })
                },
            )
            .expect("the test window opens");
            (window, host, surface_slot.expect("the surface was made"))
        });
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("window open");
        Opened {
            window,
            host,
            surface,
            written,
            events,
        }
    }

    fn press(opened: &Opened, cx: &mut TestAppContext, key: &str) {
        cx.update_window(opened.window, |_, window, cx| window.press(key, cx))
            .expect("window open");
    }

    fn take(opened: &Opened) -> Vec<String> {
        std::mem::take(&mut *opened.written.borrow_mut())
    }

    #[gpui_kit::test]
    fn the_shell_receives_keys_the_workspace_binds(cx: &mut TestAppContext) {
        let opened = open(cx);
        for (key, bytes) in [
            ("escape", "\x1b"),
            ("ctrl-k", "\x0b"),
            ("alt-up", "\x1b[1;3A"),
            ("tab", "\t"),
            ("a", "a"),
            ("ctrl-c", "\x03"),
            ("enter", "\r"),
        ] {
            press(&opened, cx, key);
            assert_eq!(take(&opened), vec![bytes.to_owned()], "{key}");
        }
        assert_eq!(
            cx.read(|cx| opened.host.read(cx).stops),
            0,
            "Escape stayed in the terminal"
        );
        // Ctrl+0 is the app's zoom and never reaches the shell.
        press(&opened, cx, "ctrl-0");
        assert!(take(&opened).is_empty());
    }

    #[gpui_kit::test]
    fn composition_reaches_the_pty_once_as_committed_text(cx: &mut TestAppContext) {
        let opened = open(cx);
        cx.update_window(opened.window, |_, window, cx| {
            opened.surface.update(cx, |surface, cx| {
                surface.replace_and_mark_text_in_range(None, "ni", None, window, cx);
                assert_eq!(surface.marked_text_range(window, cx), Some(0..2));
            });
        })
        .expect("window open");
        // Enter while composing belongs to the input method.
        press(&opened, cx, "enter");
        assert!(
            take(&opened).is_empty(),
            "nothing is sent during composition"
        );
        cx.update_window(opened.window, |_, window, cx| {
            opened.surface.update(cx, |surface, cx| {
                assert!(
                    surface
                        .bounds_for_range(0..0, Bounds::default(), window, cx)
                        .is_some()
                );
                surface.replace_text_in_range(None, "你好", window, cx);
            });
        })
        .expect("window open");
        assert_eq!(take(&opened), vec!["你好".to_owned()]);
        assert!(cx.read(|cx| opened.surface.read(cx).marked.is_none()));
    }

    #[gpui_kit::test]
    fn paste_shortcuts_deliver_once_through_the_paste_encoder(cx: &mut TestAppContext) {
        let opened = open(cx);
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("echo hi\nls".to_owned())));
        press(&opened, cx, "ctrl-shift-v");
        assert_eq!(
            take(&opened),
            vec!["echo hi\rls".to_owned()],
            "unbracketed: LF becomes CR"
        );
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.feed("\x1b[?2004h", cx))
        });
        press(&opened, cx, "shift-insert");
        assert_eq!(
            take(&opened),
            vec!["\x1b[200~echo hi\nls\x1b[201~".to_owned()]
        );
        cx.update(|cx| opened.surface.update(cx, |surface, cx| surface.paste(cx)));
        assert_eq!(
            take(&opened).len(),
            1,
            "the menu's paste is the same single path"
        );
    }

    #[gpui_kit::test]
    fn copy_takes_the_selection_and_plain_ctrl_c_then_interrupts(cx: &mut TestAppContext) {
        let opened = open(cx);
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                surface.feed("hello world\r\n", cx);
                surface.emulator.select((0, 0), (4, 0));
                surface.has_selection = true;
            })
        });
        press(&opened, cx, "ctrl-shift-c");
        assert_eq!(
            cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
                .as_deref(),
            Some("hello")
        );
        assert!(take(&opened).is_empty());
        press(&opened, cx, "ctrl-c");
        assert!(take(&opened).is_empty(), "the first Ctrl+C copies");
        press(&opened, cx, "ctrl-c");
        assert_eq!(
            take(&opened),
            vec!["\x03".to_owned()],
            "the next one interrupts"
        );
    }

    #[gpui_kit::test]
    fn root_copy_cannot_take_ctrl_c_from_a_terminal(cx: &mut TestAppContext) {
        use gpui_kit::base::text::{
            TextViewSelectionDocument, TextViewSelectionKey, TextViewSelectionPosition,
            TextViewState,
        };
        let opened = open(cx);
        let document = TextViewSelectionDocument::new();
        let text = cx.update(|cx| cx.new(|cx| TextViewState::markdown("transcript", cx)));
        cx.update_window(opened.window, |_, window, cx| {
            let key = TextViewSelectionKey(1);
            document
                .attach(key, 0, &text, cx)
                .expect("attach transcript");
            opened.host.update(cx, |host, cx| {
                host.transcript = Some(text.clone());
                cx.notify();
            });
            window.render_frame(cx);
            document.activate(window, cx);
            document
                .select(
                    TextViewSelectionPosition {
                        member: key,
                        offset: 0,
                    },
                    TextViewSelectionPosition {
                        member: key,
                        offset: 10,
                    },
                    cx,
                )
                .expect("select transcript");
            assert_eq!(
                gpui_kit::base::TextSelection::selected_text(window, cx),
                "transcript"
            );
        })
        .expect("window open");
        for (key, expected) in [
            ("ctrl-c", "\x03"),
            ("ctrl-v", "\x16"),
            ("ctrl-x", "\x18"),
            ("ctrl-a", "\x01"),
            ("ctrl-z", "\x1a"),
            ("ctrl-w", "\x17"),
        ] {
            press(&opened, cx, key);
            assert_eq!(take(&opened), [expected], "{key} belongs to the shell");
        }
    }

    #[gpui_kit::test]
    fn snapshot_size_is_reconciled_even_when_the_grid_does_not_change(cx: &mut TestAppContext) {
        let opened = open(cx);
        let fit = cx.read(|cx| opened.surface.read(cx).grid_size());
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.replay("", Some((120, 40)), cx))
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        assert_eq!(
            opened.events.borrow_mut().drain(..).collect::<Vec<_>>(),
            [SurfaceEvent::Resized {
                cols: fit.0,
                rows: fit.1
            }]
        );
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                let request = surface.pending_resize().expect("first resize");
                surface.resize_finished(request, true, cx);
            })
        });
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.replay("", Some((90, 30)), cx))
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        assert_eq!(
            opened.events.borrow_mut().drain(..).collect::<Vec<_>>(),
            [SurfaceEvent::Resized {
                cols: fit.0,
                rows: fit.1
            }]
        );
    }

    #[gpui_kit::test]
    fn resize_waits_for_the_previous_http_request(cx: &mut TestAppContext) {
        let opened = open(cx);
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.set_pty_size((120, 40), cx))
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        assert_eq!(opened.events.borrow().len(), 1);
        let a = cx.read(|cx| opened.surface.read(cx).pending_resize().expect("A"));
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                let metrics = surface.metrics.expect("measured");
                surface.fit_to(70, 20, metrics, cx);
            })
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        assert_eq!(
            opened.events.borrow().len(),
            1,
            "B cannot reach the server before A completes"
        );
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.resize_finished(a, true, cx))
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        let b = cx.read(|cx| {
            opened
                .surface
                .read(cx)
                .pending_resize()
                .expect("B follows A")
        });
        assert_eq!(b.size, (70, 20));
        assert_eq!(opened.events.borrow().len(), 2);
        // A delayed duplicate acknowledgement cannot clear B or alter its size.
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.resize_finished(a, true, cx))
        });
        assert_eq!(
            cx.read(|cx| opened.surface.read(cx).pending_resize()),
            Some(b)
        );
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.resize_finished(b, true, cx))
        });
        assert_eq!(
            cx.read(|cx| opened.surface.read(cx).pty_size),
            Some((70, 20))
        );
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        assert_eq!(
            opened.events.borrow().len(),
            2,
            "server sees A then B, never parallel A/B"
        );
    }

    #[gpui_kit::test]
    fn resize_failure_retries_latest_target_with_bounded_backoff(cx: &mut TestAppContext) {
        let opened = open(cx);
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.set_pty_size((120, 40), cx))
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        let a = cx.read(|cx| opened.surface.read(cx).pending_resize().expect("A"));
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.resize_finished(a, false, cx))
        });
        assert_eq!(
            cx.read(|cx| opened.surface.read(cx).pending_resize()),
            None,
            "failed request clears"
        );
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                surface.fit_to(71, 21, surface.metrics.expect("metrics"), cx);
                surface.reconcile_size(cx);
            })
        });
        cx.executor().advance_clock(RESIZE_SETTLE);
        cx.run_until_parked();
        assert_eq!(
            opened.events.borrow().len(),
            1,
            "new target cannot bypass failure backoff"
        );
        cx.executor().advance_clock(RESIZE_RETRY_BASE);
        cx.run_until_parked();
        let b = cx.read(|cx| opened.surface.read(cx).pending_resize().expect("retry"));
        assert_eq!(b.size, (71, 21), "retry follows latest target");
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                surface.resize_finished(a, false, cx);
                assert_eq!(
                    surface.pending_resize(),
                    Some(b),
                    "late failure cannot clear B"
                );
                surface.resize_finished(b, true, cx);
            })
        });
        assert_eq!(
            cx.read(|cx| opened.surface.read(cx).pty_size),
            Some((71, 21))
        );
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();
        assert_eq!(opened.events.borrow().len(), 2, "recovery stops retries");
    }

    #[gpui_kit::test]
    fn resize_same_size_retry_exhaustion_and_close_cancel_timers(cx: &mut TestAppContext) {
        let opened = open(cx);
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.set_pty_size((120, 40), cx))
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        let target = cx.read(|cx| opened.surface.read(cx).grid_size());
        for failure in 0..=RESIZE_MAX_RETRIES {
            let request = cx.read(|cx| {
                opened
                    .surface
                    .read(cx)
                    .pending_resize()
                    .expect("retry or initial request")
            });
            assert_eq!(
                request.size, target,
                "unchanged target is retried after failure"
            );
            cx.update(|cx| {
                opened.surface.update(cx, |surface, cx| {
                    surface.resize_finished(request, false, cx)
                })
            });
            cx.run_until_parked();
            if failure < RESIZE_MAX_RETRIES {
                let delay = RESIZE_RETRY_BASE * (1 << failure);
                cx.executor()
                    .advance_clock(delay - Duration::from_millis(1));
                cx.run_until_parked();
                assert_eq!(
                    opened.events.borrow().len(),
                    usize::from(failure + 1),
                    "retry waits for its backoff deadline"
                );
                cx.executor().advance_clock(Duration::from_millis(1));
            } else {
                cx.executor().advance_clock(Duration::from_secs(5));
            }
            cx.run_until_parked();
            assert_eq!(
                opened.events.borrow().len(),
                usize::from((failure + 2).min(RESIZE_MAX_RETRIES + 1))
            );
        }
        for _ in 0..5 {
            cx.update(|cx| {
                opened
                    .surface
                    .update(cx, |surface, cx| surface.reconcile_size(cx))
            });
            cx.executor().advance_clock(Duration::from_secs(10));
            cx.run_until_parked();
        }
        assert_eq!(
            opened.events.borrow().len(),
            usize::from(RESIZE_MAX_RETRIES + 1),
            "rendering cannot restart exhausted retries"
        );
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                surface.fit_to(72, 22, surface.metrics.expect("metrics"), cx)
            })
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        let request = cx.read(|cx| {
            opened
                .surface
                .read(cx)
                .pending_resize()
                .expect("new target")
        });
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                surface.resize_finished(request, false, cx);
                surface.stop_resizing();
                surface.resize_finished(request, true, cx);
            })
        });
        let count = opened.events.borrow().len();
        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
        assert_eq!(
            opened.events.borrow().len(),
            count,
            "closed surface cancels scheduled retries"
        );
    }

    #[gpui_kit::test]
    fn lost_resize_response_retries_unchanged_target_then_recovers(cx: &mut TestAppContext) {
        let opened = open(cx);
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.set_pty_size((120, 40), cx))
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        let a = cx.read(|cx| {
            opened
                .surface
                .read(cx)
                .pending_resize()
                .expect("initial request")
        });
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.resize_finished(a, false, cx))
        });
        cx.executor().advance_clock(RESIZE_RETRY_BASE * 2);
        cx.run_until_parked();
        let retry = cx.read(|cx| {
            opened
                .surface
                .read(cx)
                .pending_resize()
                .expect("same size retry")
        });
        assert_eq!(retry.size, a.size);
        assert_ne!(
            retry.sequence, a.sequence,
            "retry owns a distinct completion token"
        );
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                surface.resize_finished(a, true, cx);
                assert_eq!(
                    surface.pending_resize(),
                    Some(retry),
                    "lost response arriving late cannot finish the retry"
                );
                surface.resize_finished(retry, true, cx);
            })
        });
        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
        assert_eq!(opened.events.borrow().len(), 2);
        assert_eq!(cx.read(|cx| opened.surface.read(cx).pty_size), Some(a.size));
    }

    #[gpui_kit::test]
    fn stopping_resize_aborts_owned_http_even_when_the_surface_is_retained(
        cx: &mut TestAppContext,
    ) {
        struct Dropped(Rc<Cell<bool>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let opened = open(cx);
        let cancelled = Rc::new(Cell::new(false));
        let guard = Dropped(Rc::clone(&cancelled));
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                let task = cx.spawn(async move |_, _| {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                });
                surface.retain_resize_http(task);
            })
        });
        cx.run_until_parked();
        assert!(!cancelled.get(), "HTTP remains owned until shutdown");
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, _| surface.stop_resizing())
        });
        cx.run_until_parked();
        assert!(cancelled.get(), "close/runtime shutdown cancels owned HTTP");
    }

    #[gpui_kit::test]
    fn snapshot_during_resize_keeps_serial_gate_and_rejects_old_revision(cx: &mut TestAppContext) {
        let opened = open(cx);
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.set_pty_size((120, 40), cx))
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        let a = cx.read(|cx| opened.surface.read(cx).pending_resize().expect("A"));
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                surface.fit_to(73, 23, surface.metrics.expect("metrics"), cx);
                surface.set_pty_size((73, 23), cx);
            })
        });
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        assert_eq!(
            opened.events.borrow().len(),
            1,
            "snapshot must not release in-flight gate"
        );
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.resize_finished(a, true, cx))
        });
        assert_eq!(
            cx.read(|cx| opened.surface.read(cx).pty_size),
            Some((73, 23)),
            "stale acknowledgement cannot overwrite new authority"
        );
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        assert_eq!(
            cx.read(|cx| opened
                .surface
                .read(cx)
                .pending_resize()
                .expect("correct uncertain ordering")
                .size),
            (73, 23)
        );
    }

    #[gpui_kit::test]
    fn the_grid_fits_its_box_and_reports_a_settled_size_once(cx: &mut TestAppContext) {
        let opened = open(cx);
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.set_pty_size((80, 24), cx))
        });
        let first = cx.read(|cx| opened.surface.read(cx).grid_size());
        assert!(cx.read(|cx| opened.surface.read(cx).measured()));
        assert_ne!(
            first,
            (80, 24),
            "the provisional size was replaced by the fitted one"
        );
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        assert_eq!(
            opened.events.borrow_mut().drain(..).collect::<Vec<_>>(),
            vec![SurfaceEvent::Resized {
                cols: first.0,
                rows: first.1
            }]
        );
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                let request = surface.pending_resize().expect("first resize");
                surface.resize_finished(request, true, cx);
            })
        });
        // Two quick changes settle into one report of the last size.
        for width in [500.0, 600.0] {
            cx.update(|cx| {
                opened.host.update(cx, |host, cx| {
                    host.width = width;
                    cx.notify();
                })
            });
            cx.update_window(opened.window, |_, window, cx| window.render_frame(cx))
                .expect("window open");
        }
        let last = cx.read(|cx| opened.surface.read(cx).grid_size());
        assert!(last.0 > first.0, "a wider box fits more columns");
        cx.executor().advance_clock(RESIZE_SETTLE * 2);
        cx.run_until_parked();
        assert_eq!(
            opened.events.borrow_mut().drain(..).collect::<Vec<_>>(),
            vec![SurfaceEvent::Resized {
                cols: last.0,
                rows: last.1
            }]
        );
    }

    #[gpui_kit::test]
    fn a_surface_nobody_paints_still_parses_and_answers(cx: &mut TestAppContext) {
        let written = Rc::new(RefCell::new(Vec::new()));
        let surface = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::load("en"));
            theme::apply(Palette::default_for(Appearance::Dark), cx);
            let palette = cx.global::<Palette>().clone();
            cx.new(|cx| {
                let mut surface =
                    TerminalSurface::new(40, 10, gpui_kit::font("Kit Mono"), 14.0, &palette, cx);
                let sink = Rc::clone(&written);
                surface.set_test_writer(Rc::new(move |data| sink.borrow_mut().push(data)));
                surface
            })
        });
        cx.update(|cx| surface.update(cx, |surface, cx| surface.feed("ok\r\n\x1b[6n", cx)));
        assert_eq!(written.borrow().as_slice(), ["\x1b[2;1R".to_owned()]);
        assert_eq!(
            cx.update(|cx| surface.update(cx, |surface, _| surface.emulator.viewport_text()))[0],
            "ok"
        );
        // History replay never answers.
        cx.update(|cx| surface.update(cx, |surface, cx| surface.replay("\x1b[6n$ ", None, cx)));
        assert_eq!(written.borrow().len(), 1);
    }

    #[gpui_kit::test]
    fn the_wheel_scrolls_history_unless_the_program_tracks_the_mouse(cx: &mut TestAppContext) {
        let opened = open(cx);
        cx.update(|cx| {
            opened.surface.update(cx, |surface, cx| {
                for line in 0..200 {
                    surface.feed(&format!("{line}\r\n"), cx);
                }
            })
        });
        let bottom = cx.read(|cx| opened.surface.read(cx).emulator.scroll_position());
        cx.update_window(opened.window, |_, window, cx| {
            window.scroll(
                "terminal-surface",
                gpui_kit::ScrollDelta::Lines(point(0.0, 2.0)),
                cx,
            );
        })
        .expect("window open");
        let scrolled = cx.read(|cx| opened.surface.read(cx).emulator.scroll_position());
        assert!(
            scrolled.offset < bottom.offset,
            "the wheel moved toward history"
        );
        // Typing returns to the live screen.
        press(&opened, cx, "a");
        assert!(
            cx.read(|cx| opened.surface.read(cx).emulator.scroll_position())
                .at_bottom()
        );
        take(&opened);
        cx.update(|cx| {
            opened
                .surface
                .update(cx, |surface, cx| surface.feed("\x1b[?1000h\x1b[?1006h", cx))
        });
        cx.update_window(opened.window, |_, window, cx| {
            window.scroll(
                "terminal-surface",
                gpui_kit::ScrollDelta::Lines(point(0.0, 1.0)),
                cx,
            );
        })
        .expect("window open");
        let reports = take(&opened);
        assert!(
            !reports.is_empty() && reports.iter().all(|report| report.starts_with("\x1b[<64;")),
            "{reports:?}"
        );
    }
}
