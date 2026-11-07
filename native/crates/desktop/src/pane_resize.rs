//! Resizable sidebar and context panel, and the device-local layout that
//! restores them: the session sidebar's open state and width
//! (`Sidebar.tsx`) and the right zone's width (`ContextPanel.tsx`), which
//! the web keeps in its persisted UI store and never sends to the server.
//!
//! Widths are logical pixels. A drag changes a width by the pointer's
//! movement toward the side that widens the pane, mirrored in right-to-left
//! layouts, and saves once when the pointer is released.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use gpui_kit::{
    AppContext, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Role, StatefulInteractiveElement, Styled, Task, Window, div,
    prelude::FluentBuilder, px,
};
use serde::{Deserialize, Serialize};

use crate::locale::Direction;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `SIDEBAR_CONTENT_WIDTH`, `SIDEBAR_MIN_WIDTH` and `SIDEBAR_MAX_WIDTH`.
pub(crate) const SIDEBAR_DEFAULT_WIDTH: f32 = 280.0;
pub(crate) const SIDEBAR_MIN_WIDTH: f32 = 264.0;
pub(crate) const SIDEBAR_MAX_WIDTH: f32 = 500.0;
/// `CONTEXT_ZONE_DEFAULT_WIDTH` and `CONTEXT_PANEL_MIN_WIDTH`.
pub(crate) const PANEL_DEFAULT_WIDTH: f32 = 640.0;
pub(crate) const PANEL_MIN_WIDTH: f32 = 320.0;
/// `CONTEXT_PANEL_MAX_PERSISTED_WIDTH`: a sanity bound for stored values;
/// the real ceiling is the chat's minimum beside the panel.
const PANEL_MAX_STORED_WIDTH: f32 = 10_000.0;
/// `CONTEXT_CHAT_MIN_WIDTH`: the chat keeps this much beside the panel.
pub(crate) const CHAT_MIN_WIDTH: f32 = 320.0;
const MAX_BYTES: u64 = 4 * 1024;

pub(crate) fn clamp_sidebar(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
    } else {
        SIDEBAR_DEFAULT_WIDTH
    }
}

fn clamp_stored_panel(width: f32) -> f32 {
    if width.is_finite() {
        width.round().clamp(PANEL_MIN_WIDTH, PANEL_MAX_STORED_WIDTH)
    } else {
        PANEL_DEFAULT_WIDTH
    }
}

/// The panel's width beside `area`, the room the chat and the panel share:
/// the chat keeps its minimum, and the panel never goes under its own.
pub(crate) fn fit_panel(width: f32, area: f32) -> f32 {
    width.min(area - CHAT_MIN_WIDTH).max(PANEL_MIN_WIDTH)
}

/// The layout a device restores.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PaneLayout {
    pub(crate) sidebar_open: bool,
    pub(crate) sidebar_width: f32,
    pub(crate) panel_width: f32,
}

impl Default for PaneLayout {
    fn default() -> Self {
        Self {
            sidebar_open: true,
            sidebar_width: SIDEBAR_DEFAULT_WIDTH,
            panel_width: PANEL_DEFAULT_WIDTH,
        }
    }
}

impl PaneLayout {
    /// Clamps every width, as the web clamps a persisted one when it reads it.
    fn sanitized(self) -> Self {
        Self {
            sidebar_open: self.sidebar_open,
            sidebar_width: clamp_sidebar(self.sidebar_width),
            panel_width: clamp_stored_panel(self.panel_width),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LayoutFile {
    version: u32,
    layout: PaneLayout,
}

/// `layout.json` in the app's private folder. Reads fall back to the
/// defaults; a failed write keeps the window as it is.
#[derive(Clone, Debug)]
pub(crate) struct LayoutStore {
    path: PathBuf,
}

impl LayoutStore {
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            path: directory.join("layout.json"),
        }
    }

    pub(crate) fn load(&self) -> PaneLayout {
        let read = || -> Option<PaneLayout> {
            let file = File::open(&self.path).ok()?;
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
            if bytes.len() as u64 > MAX_BYTES {
                return None;
            }
            let file: LayoutFile = serde_json::from_slice(&bytes).ok()?;
            (file.version == 1).then_some(file.layout)
        };
        read().unwrap_or_default().sanitized()
    }

    pub(crate) fn save(&self, layout: PaneLayout) -> std::io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("the layout file has no folder"))?;
        let mut directories = fs::DirBuilder::new();
        directories.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directories.mode(0o700);
        }
        directories.create(parent)?;
        let bytes = serde_json::to_vec_pretty(&LayoutFile {
            version: 1,
            layout: layout.sanitized(),
        })
        .map_err(std::io::Error::other)?;
        let temporary = parent.join(format!("layout.json.partial-{}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| {
            let mut output = options.open(&temporary)?;
            output.write_all(&bytes)?;
            output.sync_all()?;
            fs::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

/// Which edge a drag moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pane {
    /// The sidebar's end edge; dragging toward the end widens it.
    Sidebar,
    /// The panel's start edge; dragging toward the chat widens it.
    Panel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Drag {
    pane: Pane,
    start_x: f32,
    start_width: f32,
}

/// The pane widths, the drag in progress and where they are kept.
#[derive(Debug, Default)]
pub(crate) struct PaneSizes {
    pub(crate) sidebar_width: Option<f32>,
    pub(crate) panel_width: Option<f32>,
    drag: Option<Drag>,
    store: Option<LayoutStore>,
    /// One write at a time; a change during it writes again after.
    saving: Option<Task<()>>,
    dirty: bool,
}

impl PaneSizes {
    pub(crate) fn sidebar_width(&self) -> f32 {
        self.sidebar_width.unwrap_or(SIDEBAR_DEFAULT_WIDTH)
    }

    pub(crate) fn panel_width(&self) -> f32 {
        self.panel_width.unwrap_or(PANEL_DEFAULT_WIDTH)
    }

    pub(crate) fn dragging(&self) -> Option<Pane> {
        self.drag.map(|drag| drag.pane)
    }

    fn begin(&mut self, pane: Pane, x: f32, width: f32) {
        self.drag = Some(Drag {
            pane,
            start_x: x,
            start_width: width,
        });
    }

    /// The width a pointer at `x` asks for, before the panel's fit.
    fn dragged_width(&self, x: f32, direction: Direction) -> Option<(Pane, f32)> {
        let drag = self.drag?;
        let moved = (x - drag.start_x)
            * match direction {
                Direction::LeftToRight => 1.0,
                Direction::RightToLeft => -1.0,
            };
        Some(match drag.pane {
            Pane::Sidebar => (Pane::Sidebar, clamp_sidebar(drag.start_width + moved)),
            Pane::Panel => (Pane::Panel, (drag.start_width - moved).max(PANEL_MIN_WIDTH)),
        })
    }
}

impl Workspace {
    /// Restores a device's layout and keeps saving it to `store`.
    pub(crate) fn restore_layout(&mut self, store: LayoutStore, layout: PaneLayout) {
        self.sidebar_open = layout.sidebar_open;
        self.pane_sizes.sidebar_width = Some(layout.sidebar_width);
        self.pane_sizes.panel_width = Some(layout.panel_width);
        self.pane_sizes.store = Some(store);
    }

    /// Writes the layout off the UI thread, one write at a time, so the
    /// last change is the one kept; a failure keeps the window as it is.
    pub(crate) fn save_layout(&mut self, cx: &mut Context<Self>) {
        if self.pane_sizes.store.is_none() {
            return;
        }
        self.pane_sizes.dirty = true;
        if self.pane_sizes.saving.is_none() {
            self.write_layout(cx);
        }
    }

    fn write_layout(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.pane_sizes.store.clone() else {
            return;
        };
        self.pane_sizes.dirty = false;
        let layout = PaneLayout {
            sidebar_open: self.sidebar_open,
            sidebar_width: self.pane_sizes.sidebar_width(),
            panel_width: self.pane_sizes.panel_width(),
        };
        let write = cx.background_spawn(async move { store.save(layout) });
        self.pane_sizes.saving = Some(cx.spawn(async move |this, cx| {
            if let Err(error) = write.await {
                tracing::warn!(%error, "the window layout could not be saved");
            }
            let _ = this.update(cx, |this, cx| {
                this.pane_sizes.saving = None;
                if this.pane_sizes.dirty {
                    this.write_layout(cx);
                }
            });
        }));
    }

    /// The room the chat and the panel share.
    pub(crate) fn panel_area(&self, window: &Window) -> f32 {
        let sidebar = if self.sidebar_open {
            self.pane_sizes.sidebar_width()
        } else {
            0.0
        };
        f32::from(crate::chrome::content_size(window).width)
            - sidebar
            - crate::context_panel::RAIL_WIDTH
    }

    /// The open panel's width beside a chat of at least its minimum.
    pub(crate) fn fitted_panel_width(&self, window: &Window) -> f32 {
        fit_panel(self.pane_sizes.panel_width(), self.panel_area(window))
    }

    fn begin_pane_drag(
        &mut self,
        pane: Pane,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let width = match pane {
            Pane::Sidebar => self.pane_sizes.sidebar_width(),
            Pane::Panel => self.fitted_panel_width(window),
        };
        self.pane_sizes
            .begin(pane, f32::from(event.position.x), width);
        cx.stop_propagation();
        cx.notify();
    }

    /// Follows the pointer while a pane edge is held.
    pub(crate) fn drag_pane(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_sizes.drag.is_none() {
            return;
        }
        let direction = crate::locale::strings(cx).direction();
        let Some((pane, width)) = self
            .pane_sizes
            .dragged_width(f32::from(event.position.x), direction)
        else {
            return;
        };
        if !event.dragging() {
            self.end_pane_drag(cx);
            return;
        }
        match pane {
            Pane::Sidebar => self.pane_sizes.sidebar_width = Some(width),
            Pane::Panel => {
                self.pane_sizes.panel_width = Some(fit_panel(width, self.panel_area(window)))
            }
        }
        cx.notify();
    }

    pub(crate) fn end_pane_drag(&mut self, cx: &mut Context<Self>) {
        if self.pane_sizes.drag.take().is_some() {
            self.save_layout(cx);
            cx.notify();
        }
    }

    pub(crate) fn release_pane(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.end_pane_drag(cx);
    }

    /// The thin handle on a pane's edge toward the chat.
    pub(crate) fn render_resize_handle(
        &self,
        pane: Pane,
        label: gpui_kit::SharedString,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.pane_sizes.dragging() == Some(pane);
        let border = palette.border;
        let id = match pane {
            Pane::Sidebar => "sidebar-resize",
            Pane::Panel => "panel-resize",
        };
        let handle = div()
            .id(id)
            .role(Role::Splitter)
            .accessibility_id(id)
            .aria_label(label)
            .absolute()
            .top_0()
            .bottom_0()
            .cursor_col_resize()
            .hover(move |style| style.bg(border.opacity(0.8)))
            .when(active, |handle| handle.bg(border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.begin_pane_drag(pane, event, window, cx)
                }),
            );
        // The sidebar's 3 px strip sits on its edge (`Sidebar.tsx`); the
        // panel's 8 px strip fills the gutter that faces the chat
        // (`ContextPanel.tsx`).
        match pane {
            Pane::Sidebar => handle.w(px(3.0)).inset_end(direction, px(0.0)),
            Pane::Panel => handle.w(px(8.0)).inset_start(direction, px(-6.0)),
        }
    }
}

/// The pointer's width change for a drag, exposed for the window's tests.
#[cfg(test)]
fn moved(pane: Pane, start: f32, from: f32, to: f32, direction: Direction) -> Option<f32> {
    let mut sizes = PaneSizes::default();
    sizes.begin(pane, from, start);
    sizes.dragged_width(to, direction).map(|(_, width)| width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_drags_toward_its_end_edge_in_both_directions() {
        let ltr = Direction::LeftToRight;
        let rtl = Direction::RightToLeft;
        assert_eq!(moved(Pane::Sidebar, 280.0, 280.0, 360.0, ltr), Some(360.0));
        assert_eq!(moved(Pane::Sidebar, 280.0, 1000.0, 920.0, rtl), Some(360.0));
        assert_eq!(
            moved(Pane::Sidebar, 280.0, 280.0, 2000.0, ltr),
            Some(SIDEBAR_MAX_WIDTH)
        );
        assert_eq!(
            moved(Pane::Sidebar, 280.0, 280.0, 0.0, ltr),
            Some(SIDEBAR_MIN_WIDTH)
        );
    }

    #[test]
    fn panel_widens_toward_the_chat_and_keeps_the_chat_minimum() {
        let ltr = Direction::LeftToRight;
        let rtl = Direction::RightToLeft;
        assert_eq!(moved(Pane::Panel, 640.0, 600.0, 500.0, ltr), Some(740.0));
        assert_eq!(moved(Pane::Panel, 640.0, 600.0, 700.0, rtl), Some(740.0));
        assert_eq!(
            moved(Pane::Panel, 640.0, 600.0, 1600.0, ltr),
            Some(PANEL_MIN_WIDTH)
        );
        assert_eq!(fit_panel(900.0, 1000.0), 680.0, "the chat keeps 320 px");
        assert_eq!(
            fit_panel(900.0, 500.0),
            PANEL_MIN_WIDTH,
            "the panel keeps its minimum"
        );
        assert_eq!(fit_panel(500.0, 2000.0), 500.0);
    }

    #[test]
    fn stored_layouts_round_trip_and_clamp() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = PathBuf::from("/tmp/opencode")
            .join(format!("layout-test-{}-{nonce}", std::process::id()));
        let store = LayoutStore::new(&directory);
        assert_eq!(
            store.load(),
            PaneLayout::default(),
            "missing reads the defaults"
        );
        let layout = PaneLayout {
            sidebar_open: false,
            sidebar_width: 333.0,
            panel_width: 777.0,
        };
        store.save(layout).expect("save");
        assert_eq!(store.load(), layout);
        fs::write(
            directory.join("layout.json"),
            br#"{"version":1,"layout":{"sidebarOpen":true,"sidebarWidth":9000,"panelWidth":10}}"#,
        )
        .expect("fixture");
        assert_eq!(
            store.load(),
            PaneLayout {
                sidebar_open: true,
                sidebar_width: SIDEBAR_MAX_WIDTH,
                panel_width: PANEL_MIN_WIDTH,
            }
        );
        fs::write(directory.join("layout.json"), b"not json").expect("fixture");
        assert_eq!(
            store.load(),
            PaneLayout::default(),
            "a damaged file reads the defaults"
        );
        fs::write(
            directory.join("layout.json"),
            br#"{"version":2,"layout":{"sidebarOpen":false,"sidebarWidth":300,"panelWidth":700}}"#,
        )
        .expect("fixture");
        assert_eq!(
            store.load(),
            PaneLayout::default(),
            "a newer file is not guessed at"
        );
        fs::remove_dir_all(directory).expect("remove owned fixture");
    }
}
