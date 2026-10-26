use std::{
    cell::RefCell,
    ops::{Range, RangeInclusive},
    rc::Rc,
};

use crate::{
    TextSelectionContentKey, TextSelectionCoverage, TextSelectionEndpoint, TextSelectionEvent,
    TextSelectionHandle, TextSelectionRegistration, TextSelectionRun, TextSelectionSnapshot,
};
use gpui::{App, Bounds, EntityId, Hitbox, Pixels, Point, WeakEntity, Window};

use super::TextViewState;

#[derive(Clone, Copy, Debug, PartialEq)]
struct CachedBlockEndpoint {
    endpoint: TextSelectionEndpoint,
    block_ix: Option<usize>,
}

#[derive(Default)]
struct VirtualBlockSelection {
    anchor: Option<CachedBlockEndpoint>,
    cursor: Option<CachedBlockEndpoint>,
    coverage: TextSelectionCoverage,
    is_selecting: bool,
}

impl VirtualBlockSelection {
    fn update(&mut self, snapshot: Option<TextSelectionSnapshot>, entity_id: EntityId) -> bool {
        let Some(snapshot) = snapshot else {
            let changed = self.anchor.is_some() || self.cursor.is_some();
            *self = Self::default();
            return changed;
        };

        // Origin and scroll changes only alter the window projection. They must
        // not turn a held logical range back into a hit test in reflowed text.
        let changed = self.anchor.map(|cached| cached.endpoint) != Some(snapshot.anchor())
            || self.cursor.map(|cached| cached.endpoint) != Some(snapshot.cursor())
            || self.coverage != snapshot.coverage()
            || self.is_selecting != snapshot.is_selecting();
        self.coverage = snapshot.coverage();
        self.is_selecting = snapshot.is_selecting();

        Self::update_endpoint(&mut self.anchor, snapshot.anchor(), entity_id);
        Self::update_endpoint(&mut self.cursor, snapshot.cursor(), entity_id);
        changed
    }

    fn update_endpoint(
        cached: &mut Option<CachedBlockEndpoint>,
        endpoint: TextSelectionEndpoint,
        entity_id: EntityId,
    ) {
        if cached.is_some_and(|cached| cached.endpoint == endpoint) {
            return;
        }
        let block_ix = (endpoint.entity_id() == Some(entity_id))
            .then(|| endpoint.content_key().map(|key| key.value() as usize))
            .flatten();
        *cached = Some(CachedBlockEndpoint { endpoint, block_ix });
    }

    fn block_range(&self, entity_id: EntityId, last: usize) -> Option<RangeInclusive<usize>> {
        let anchor = self.anchor?;
        let cursor = self.cursor?;
        match self.coverage {
            TextSelectionCoverage::Full => Some(0..=last),
            TextSelectionCoverage::FromStart => Some(0..=anchor.block_ix.or(cursor.block_ix)?),
            TextSelectionCoverage::ToEnd => Some(anchor.block_ix.or(cursor.block_ix)?..=last),
            TextSelectionCoverage::Bounded => {
                if anchor.endpoint.entity_id() != Some(entity_id)
                    || cursor.endpoint.entity_id() != Some(entity_id)
                {
                    return None;
                }
                let (anchor, cursor) = (anchor.block_ix?, cursor.block_ix?);
                Some(anchor.min(cursor)..=anchor.max(cursor))
            }
        }
    }
}

/// TextView's renderer-specific bridge to base-owned window selection.
#[derive(Clone)]
pub(super) struct TextViewSelectionAdapter {
    selection: TextSelectionHandle,
    /// What the view's text painted this frame, shared by the adapter's
    /// clones. Inlines report it as they paint, through a read of the view's
    /// state: reporting it is not a change of the view.
    frame: Rc<RefCell<PaintedText>>,
    layout_revision: Option<usize>,
}

/// The text a [`TextViewSelectionAdapter`]'s view painted this frame.
#[derive(Default)]
struct PaintedText {
    text_bounds: Vec<Bounds<Pixels>>,
    text_runs: Vec<TextSelectionRun>,
    /// The caret boxes at the first and last selected character painted this
    /// frame, where the touch handles go.
    selection_edges: Option<(Bounds<Pixels>, Bounds<Pixels>)>,
    logical_runs: Vec<(usize, TextSelectionRun)>,
}

impl TextViewSelectionAdapter {
    pub(super) fn attach_document(
        &self,
        binding: super::selection_document::SelectionDocumentBinding,
        cx: &mut App,
    ) {
        self.selection.attach_document(Some(binding), cx);
    }

    pub(super) fn detach_document(&self, cx: &mut App) {
        self.discard_geometry(cx);
        self.selection.attach_document(None, cx);
    }

    pub(super) fn discard_geometry(&self, cx: &mut App) {
        self.begin_frame();
        self.selection.discard_geometry(cx);
    }

    pub(super) fn register_logical_run(
        &self,
        offset: usize,
        text: gpui::SharedString,
        layout: gpui::TextLayout,
        bounds: Bounds<Pixels>,
    ) {
        self.frame
            .borrow_mut()
            .logical_runs
            .push((offset, TextSelectionRun::new(text, layout, bounds)));
    }

    fn logical_offset_at(&self, point: Point<Pixels>) -> Option<usize> {
        let frame = self.frame.borrow();
        let (offset, run) = frame.logical_runs.iter().min_by(|(_, a), (_, b)| {
            let distance = |run: &TextSelectionRun| {
                let bounds = run.bounds();
                let dy = (bounds.top() - point.y)
                    .max(point.y - bounds.bottom())
                    .max(gpui::px(0.));
                let dx = (bounds.left() - point.x)
                    .max(point.x - bounds.right())
                    .max(gpui::px(0.));
                (dy, dx)
            };
            distance(a)
                .partial_cmp(&distance(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
        let index = run
            .layout()
            .index_for_position(point)
            .unwrap_or_else(|index| index);
        Some(offset + run.text().floor_char_boundary(index.min(run.text().len())))
    }

    pub(super) fn logical_line_range(&self, line: Bounds<Pixels>) -> Option<Range<usize>> {
        let frame = self.frame.borrow();
        let mut start = usize::MAX;
        let mut end = 0;
        for (offset, run) in &frame.logical_runs {
            if !run.bounds().intersects(&line) {
                continue;
            }
            let y = line.center().y;
            // Hitboxes round to pixels; sample just outside their horizontal
            // edge so the line's final glyph is included rather than its caret.
            let left = gpui::point(line.left().max(run.bounds().left()) - gpui::px(1.), y);
            let right = gpui::point(line.right().min(run.bounds().right()) + gpui::px(1.), y);
            let first = run
                .layout()
                .index_for_position(left)
                .unwrap_or_else(|index| index);
            let last = run
                .layout()
                .index_for_position(right)
                .unwrap_or_else(|index| index);
            start = start.min(offset + run.text().floor_char_boundary(first.min(run.text().len())));
            end = end.max(offset + run.text().floor_char_boundary(last.min(run.text().len())));
        }
        (start < end).then_some(start..end)
    }

    pub(super) fn new(view: WeakEntity<TextViewState>, cx: &mut App) -> Self {
        let selection = TextSelectionHandle::new("", cx);
        let selection_id = selection.entity_id();
        let virtual_blocks = Rc::new(RefCell::new(VirtualBlockSelection::default()));

        let view_for_events = view.clone();
        let blocks_for_events = virtual_blocks.clone();
        selection
            .subscribe(
                move |event, cx| match event {
                    TextSelectionEvent::SelectionChanged(snapshot) => {
                        let snapshot = *snapshot;
                        let _ = view_for_events.update(cx, |state, cx| {
                            let changed = blocks_for_events
                                .borrow_mut()
                                .update(snapshot, selection_id);
                            if snapshot.is_none() || changed {
                                state.preserve_inline_selection = false;
                            }
                            state.is_selecting =
                                snapshot.is_some_and(|snapshot| snapshot.is_selecting());
                            cx.notify();
                        });
                    }
                    TextSelectionEvent::AutoScroll(delta) => {
                        let delta = *delta;
                        let _ = view_for_events.update(cx, |state, cx| {
                            if state.scrollable {
                                state.set_auto_scroll(delta, cx);
                            } else if delta.is_none() {
                                state.stop_auto_scroll();
                            }
                        });
                    }
                    TextSelectionEvent::Cleared => {}
                    TextSelectionEvent::TouchSelectionChanged => {
                        let _ = view_for_events.update(cx, |_, cx| cx.notify());
                    }
                },
                cx,
            )
            .detach();

        let view_for_clear = view.clone();
        let blocks_for_clear = virtual_blocks.clone();
        selection.clear_with(
            move |cx| {
                blocks_for_clear.replace(VirtualBlockSelection::default());
                let _ = view_for_clear.update(cx, |state, cx| {
                    state.reset_selection();
                    cx.notify();
                });
            },
            cx,
        );

        let view_for_copy = view.clone();
        let blocks_for_copy = virtual_blocks.clone();
        selection.copy_with(
            move |cx| {
                let Some(view) = view_for_copy.upgrade() else {
                    return String::new();
                };
                let state = view.read(cx);
                let last = state.parsed_content.document.blocks.len().saturating_sub(1);
                let blocks = blocks_for_copy.borrow().block_range(selection_id, last);
                state.selected_text_in(blocks)
            },
            cx,
        );

        let view_for_content_key = view.clone();
        selection.resolve_content_key_with(
            move |point, cx| {
                let view = view_for_content_key.upgrade()?;
                let state = view.read(cx);
                if state.retained_selection.is_some() {
                    return state
                        .selection_adapter
                        .logical_offset_at(point + state.bounds().origin + state.scroll_offset())
                        .map(|offset| TextSelectionContentKey::new(offset as u64));
                }
                state
                    .block_ix_at(point.y)
                    .map(|block| TextSelectionContentKey::new(block as u64))
            },
            cx,
        );

        selection.focus_with(
            move |window, cx| {
                let Some(view) = view.upgrade() else {
                    return;
                };
                let focus_handle = view.read(cx).focus_handle.clone();
                view.update(cx, |state, _| state.is_selecting = true);
                focus_handle.focus(window, cx);
            },
            cx,
        );

        Self {
            selection,
            frame: Rc::default(),
            layout_revision: None,
        }
    }

    pub(super) fn update_layout_revision(&mut self, revision: usize, is_selecting: bool) -> bool {
        let changed = self
            .layout_revision
            .is_some_and(|previous| previous != revision);
        if !changed || !is_selecting {
            self.layout_revision = Some(revision);
        }
        changed && !is_selecting
    }

    pub(super) fn begin_frame(&self) {
        *self.frame.borrow_mut() = PaintedText::default();
    }

    /// Records one inline's painted selection ends. Inlines paint in document
    /// order, so the first start and the last end are the view's.
    pub(super) fn register_selection_edges(&self, start: Bounds<Pixels>, end: Bounds<Pixels>) {
        let mut frame = self.frame.borrow_mut();
        let first = frame.selection_edges.map_or(start, |(first, _)| first);
        frame.selection_edges = Some((first, end));
    }

    pub(super) fn register_text_run(&self, run: TextSelectionRun) {
        self.frame.borrow_mut().text_runs.push(run);
    }

    pub(super) fn register_inline(&self, bounds: Vec<Bounds<Pixels>>) {
        self.frame.borrow_mut().text_bounds.extend(bounds);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn register(
        &self,
        hitbox: Hitbox,
        bounds: Bounds<Pixels>,
        scroll_offset: Point<Pixels>,
        document_order: u64,
        self_scroll: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let frame = self.frame.borrow();
        self.selection.set_hit_test_runs(&frame.text_runs, cx);
        let registration = TextSelectionRegistration::new(hitbox, bounds)
            .with_scroll_offset(scroll_offset)
            .with_document_order(document_order)
            .with_text_bounds(frame.text_bounds.clone())
            .with_self_scroll(self_scroll)
            .with_rendered_element(&self.selection, window, cx);
        let registration = match frame.selection_edges {
            Some((start, end)) => registration.with_selection_edges(start, end),
            None => registration,
        };
        drop(frame);
        self.selection.register(registration, window, cx);
    }

    /// Lays out the touch handles this view owns; see
    /// [`TextSelectionHandle::prepaint_touch_handles`].
    pub(super) fn prepaint_touch_handles(
        &self,
        window: &mut Window,
        cx: &App,
    ) -> crate::TouchHandleLayout {
        self.selection.prepaint_touch_handles(window, cx)
    }

    /// Paints the touch handles this view owns, after its content for the
    /// frame; see [`TextSelectionHandle::paint_touch_handles`].
    pub(super) fn paint_touch_handles(
        &self,
        layout: &crate::TouchHandleLayout,
        color: gpui::Hsla,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.selection
            .paint_touch_handles(layout, color, window, cx);
    }

    pub(super) fn selection_points(&self, cx: &App) -> Option<(Point<Pixels>, Point<Pixels>)> {
        let points = self.selection.snapshot(cx)?.window_points()?;
        Some((points.anchor(), points.cursor()))
    }

    pub(super) fn set_local_selection(&self, active: bool, cx: &mut App) {
        self.selection.set_local_selection(active, cx);
    }

    pub(super) fn is_part_of_window_selection(&self, cx: &App) -> bool {
        let id = self.selection.entity_id();
        self.selection.snapshot(cx).is_some_and(|snapshot| {
            snapshot.anchor().entity_id() == Some(id) || snapshot.cursor().entity_id() == Some(id)
        })
    }

    pub(super) fn has_selection_snapshot(&self, cx: &App) -> bool {
        self.selection.snapshot(cx).is_some()
    }

    #[cfg(test)]
    pub(super) fn text_bounds(&self) -> Vec<Bounds<Pixels>> {
        self.frame.borrow().text_bounds.clone()
    }
}
