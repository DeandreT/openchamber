//! Logical selection for an explicitly owned, virtualized Markdown document.

use std::{
    cell::RefCell,
    collections::HashMap,
    ops::Range,
    rc::{Rc, Weak},
};

use gpui::{App, Entity, SharedString, WeakEntity, Window};

use super::{RenderedText, TextViewState};

/// An application-assigned identity, unique within one selection document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextViewSelectionKey(pub u64);

/// A UTF-8 byte position in a member's rendered text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextViewSelectionPosition {
    /// Stable identity of the member containing this position.
    pub member: TextViewSelectionKey,
    /// A character boundary in the member's rendered UTF-8 text.
    pub offset: usize,
}

/// A rejected document operation leaves existing membership unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextViewSelectionError {
    /// The member was removed, invalidated, or released by its owner.
    MissingMember,
    /// The key or state is already attached to a live document.
    DuplicateMember,
    /// A position or source span does not address the current text.
    InvalidRange,
    /// Retained document selection supports Markdown views.
    UnsupportedFormat,
    /// Parsing failed; the affected selection has been invalidated.
    ParseFailed,
}

impl std::fmt::Display for TextViewSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for TextViewSelectionError {}

struct Member {
    view: WeakEntity<TextViewState>,
    order: u64,
    text: SharedString,
    rendered: Option<RenderedText>,
    // A nested custom renderer replaces one atomic range of its parent.
    parent: Option<(TextViewSelectionKey, Range<usize>, Range<usize>)>,
    valid: bool,
}

#[derive(Default)]
pub(crate) struct DocumentState {
    members: HashMap<TextViewSelectionKey, Member>,
    selection: Option<(TextViewSelectionPosition, TextViewSelectionPosition)>,
    pub(crate) active: bool,
}

/// Retain one owner per transcript and retain each attached TextViewState.
/// Membership and logical selection survive absence from a rendered frame.
/// The document holds weak view references; views hold a weak document reference.
#[derive(Clone, Default)]
pub struct TextViewSelectionDocument(pub(crate) Rc<RefCell<DocumentState>>);

#[derive(Clone)]
pub(crate) struct SelectionDocumentBinding {
    pub(crate) document: Weak<RefCell<DocumentState>>,
    pub(crate) key: TextViewSelectionKey,
}

impl TextViewSelectionDocument {
    /// Create an inactive document without allocating any view or geometry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up an attached state without extending its lifetime in the document.
    pub fn member(&self, key: TextViewSelectionKey) -> Option<Entity<TextViewState>> {
        self.0
            .borrow()
            .members
            .get(&key)
            .filter(|member| member.valid)
            .and_then(|member| member.view.upgrade())
    }

    /// Attach a top-level member. Keys survive row movement; order is supplied
    /// by the application's logical transcript, independently of paint order.
    pub fn attach(
        &self,
        key: TextViewSelectionKey,
        order: u64,
        view: &Entity<TextViewState>,
        cx: &mut App,
    ) -> Result<(), TextViewSelectionError> {
        self.attach_inner(key, order, view, None, cx)
    }

    /// Attach a retained custom renderer inside a parent member. `parent_source`
    /// addresses its custom block in the parent's Markdown source.
    /// A selection within this child copies its partial text; a selection crossing
    /// its boundary takes the parent's custom block atomically, exactly once.
    /// Retain child states just as top-level states and remove/rebind them when
    /// their source identity changes.
    pub fn attach_child(
        &self,
        key: TextViewSelectionKey,
        parent: TextViewSelectionKey,
        parent_source: Range<usize>,
        view: &Entity<TextViewState>,
        cx: &mut App,
    ) -> Result<(), TextViewSelectionError> {
        let parent_range = {
            let state = self.0.borrow();
            let member = state
                .members
                .get(&parent)
                .ok_or(TextViewSelectionError::MissingMember)?;
            if !member.valid {
                return Err(TextViewSelectionError::MissingMember);
            }
            // Custom renderers run while their parent TextViewState is leased
            // for rendering. Use the committed index rather than re-reading it.
            member
                .rendered
                .as_ref()
                .ok_or(TextViewSelectionError::MissingMember)?
                .range_for_source(parent_source.clone())
                .ok_or(TextViewSelectionError::InvalidRange)?
        };
        self.attach_inner(
            key,
            0,
            view,
            Some((parent, parent_range, parent_source)),
            cx,
        )
    }

    /// Refresh a growing custom block's source range without changing its identity.
    pub fn set_child_source(
        &self,
        key: TextViewSelectionKey,
        source: Range<usize>,
        cx: &mut App,
    ) -> Result<(), TextViewSelectionError> {
        let parent = {
            let state = self.0.borrow();
            let child = state
                .members
                .get(&key)
                .filter(|member| member.valid)
                .ok_or(TextViewSelectionError::MissingMember)?;
            let parent = child
                .parent
                .as_ref()
                .map(|parent| parent.0)
                .ok_or(TextViewSelectionError::InvalidRange)?;
            let parent_member = state
                .members
                .get(&parent)
                .filter(|member| member.valid)
                .ok_or(TextViewSelectionError::MissingMember)?;
            let range = parent_member
                .rendered
                .as_ref()
                .and_then(|rendered| rendered.range_for_source(source.clone()))
                .ok_or(TextViewSelectionError::InvalidRange)?;
            (parent, range, source)
        };
        let mut state = self.0.borrow_mut();
        let member = state
            .members
            .get_mut(&key)
            .ok_or(TextViewSelectionError::MissingMember)?;
        if member.parent.as_ref() == Some(&parent) {
            return Ok(());
        }
        member.parent = Some(parent);
        drop(state);
        self.notify(cx);
        Ok(())
    }

    fn attach_inner(
        &self,
        key: TextViewSelectionKey,
        order: u64,
        view: &Entity<TextViewState>,
        parent: Option<(TextViewSelectionKey, Range<usize>, Range<usize>)>,
        cx: &mut App,
    ) -> Result<(), TextViewSelectionError> {
        if self.0.borrow().members.contains_key(&key)
            || view
                .read(cx)
                .retained_selection
                .as_ref()
                .is_some_and(|binding| binding.document.upgrade().is_some())
        {
            return Err(TextViewSelectionError::DuplicateMember);
        }
        if view.read(cx).format != super::TextViewFormat::Markdown {
            return Err(TextViewSelectionError::UnsupportedFormat);
        }
        self.0.borrow_mut().members.insert(
            key,
            Member {
                view: view.downgrade(),
                order,
                text: SharedString::default(),
                rendered: None,
                parent,
                valid: true,
            },
        );
        let binding = SelectionDocumentBinding {
            document: Rc::downgrade(&self.0),
            key,
        };
        view.update(cx, |state, cx| {
            state.selection_adapter.attach_document(binding.clone(), cx);
            state.retained_selection = Some(binding);
            state.synchronize_retained_text(cx);
        });
        if view.read(cx).parsed_error.is_some() {
            self.remove(key, cx);
            return Err(TextViewSelectionError::ParseFailed);
        }
        Ok(())
    }

    /// Update text and its parsed copy representation before returning, including
    /// for an offscreen member. Compatible appends retain fixed endpoints.
    pub fn set_text(
        &self,
        key: TextViewSelectionKey,
        text: &str,
        cx: &mut App,
    ) -> Result<(), TextViewSelectionError> {
        let view = self
            .0
            .borrow()
            .members
            .get(&key)
            .filter(|member| member.valid)
            .and_then(|member| member.view.upgrade())
            .ok_or(TextViewSelectionError::MissingMember)?;
        view.update(cx, |state, cx| state.set_text(text, cx));
        if view.read(cx).parsed_error.is_some() {
            return Err(TextViewSelectionError::ParseFailed);
        }
        Ok(())
    }

    /// Change a top-level member's logical order while preserving its identity.
    pub fn set_order(
        &self,
        key: TextViewSelectionKey,
        order: u64,
        cx: &mut App,
    ) -> Result<(), TextViewSelectionError> {
        self.0
            .borrow_mut()
            .members
            .get_mut(&key)
            .ok_or(TextViewSelectionError::MissingMember)?
            .order = order;
        self.notify(cx);
        Ok(())
    }

    /// Remove a member and its descendants synchronously. Removing an endpoint
    /// clears the selection. Removing an intermediate member removes its text.
    pub fn remove(&self, key: TextViewSelectionKey, cx: &mut App) {
        let mut keys = vec![key];
        let mut index = 0;
        while index < keys.len() {
            let parent = keys[index];
            keys.extend(self.0.borrow().members.iter().filter_map(|(key, member)| {
                (member.parent.as_ref().map(|parent| parent.0) == Some(parent)).then_some(*key)
            }));
            index += 1;
        }
        let (removed, invalidated) = {
            let mut state = self.0.borrow_mut();
            let invalidated = state
                .selection
                .is_some_and(|(a, b)| keys.contains(&a.member) || keys.contains(&b.member));
            if invalidated {
                state.selection = None;
            }
            let removed = keys
                .into_iter()
                .filter_map(|key| state.members.remove(&key))
                .collect::<Vec<_>>();
            (removed, invalidated)
        };
        if invalidated {
            crate::TextSelection::invalidate_document_positions(self, cx);
        }
        for member in removed {
            if let Some(view) = member.view.upgrade() {
                view.update(cx, |state, cx| {
                    state.retained_selection = None;
                    state.selection_adapter.detach_document(cx);
                    state.clear_selection(cx);
                    // Resume the ordinary background parser from the source
                    // accumulated while this member used synchronous parsing.
                    state.synchronize_retained_text(cx);
                });
            }
        }
        self.notify(cx);
    }

    /// Activate this document in a window. Switching documents clears the old
    /// selection and its geometry; inactive documents cannot supply copy text.
    pub fn activate(&self, window: &mut Window, cx: &mut App) {
        crate::TextSelection::activate_document(self.clone(), window, cx);
    }

    /// Deactivate this document and clear its selection.
    pub fn deactivate(&self, window: &mut Window, cx: &mut App) {
        crate::TextSelection::deactivate_document(self, window, cx);
    }

    /// Clear logical selection and cancel any window gesture that could restore it.
    pub fn clear(&self, cx: &mut App) {
        self.clear_from_window(cx);
        crate::TextSelection::invalidate_document_positions(self, cx);
    }

    pub(crate) fn clear_from_window(&self, cx: &mut App) {
        self.0.borrow_mut().selection = None;
        self.notify(cx);
    }

    /// Select validated logical positions. Direction is preserved for extension.
    pub fn select(
        &self,
        anchor: TextViewSelectionPosition,
        cursor: TextViewSelectionPosition,
        cx: &mut App,
    ) -> Result<(), TextViewSelectionError> {
        self.select_from_window(anchor, cursor, cx)?;
        crate::TextSelection::invalidate_document_positions(self, cx);
        Ok(())
    }

    pub(crate) fn select_from_window(
        &self,
        anchor: TextViewSelectionPosition,
        cursor: TextViewSelectionPosition,
        cx: &mut App,
    ) -> Result<(), TextViewSelectionError> {
        {
            let mut state = self.0.borrow_mut();
            if !state.valid_position(anchor) || !state.valid_position(cursor) {
                return Err(TextViewSelectionError::InvalidRange);
            }
            if state.selection == Some((anchor, cursor)) {
                return Ok(());
            }
            state.selection = Some((anchor, cursor));
        }
        self.notify(cx);
        Ok(())
    }

    /// Copy rendered text in logical order. Inactive documents return empty text.
    /// Each selected top-level member is separated by one additional newline,
    /// matching the window selection layer's existing join convention.
    pub fn selected_text(&self) -> String {
        self.0.borrow().selected_text()
    }

    pub(crate) fn has_selection(&self) -> bool {
        let state = self.0.borrow();
        state.active
            && state
                .selection
                .is_some_and(|(a, b)| a != b && state.valid_position(a) && state.valid_position(b))
    }

    pub(crate) fn anchor(&self) -> Option<TextViewSelectionPosition> {
        self.0.borrow().selection.map(|(a, _)| a)
    }

    pub(crate) fn notify(&self, cx: &mut App) {
        // Pointer publication can hold the window selection entity mutably.
        // Resolve its visible members after that update has finished. Hidden
        // members need no redraw and read the logical range when remounted.
        let weak = self.downgrade();
        cx.defer(move |cx| {
            let Some(document) = weak.upgrade().map(TextViewSelectionDocument) else {
                return;
            };
            for key in crate::TextSelection::painted_document_members(&document, cx) {
                if let Some(view) = document
                    .0
                    .borrow()
                    .members
                    .get(&key)
                    .and_then(|member| member.view.upgrade())
                {
                    cx.notify(view.entity_id());
                }
            }
        });
    }

    pub(crate) fn set_active(&self, active: bool, cx: &mut App) {
        {
            let mut state = self.0.borrow_mut();
            state.active = active;
            state.selection = None;
        }
        // Activation can retire the old window's geometry before a deferred
        // notification runs. Invalidate its retained views immediately.
        for member in self.0.borrow().members.values() {
            if let Some(view) = member.view.upgrade() {
                cx.notify(view.entity_id());
            }
        }
    }

    pub(crate) fn downgrade(&self) -> Weak<RefCell<DocumentState>> {
        Rc::downgrade(&self.0)
    }
}

impl DocumentState {
    fn valid_position(&self, pos: TextViewSelectionPosition) -> bool {
        self.ancestors(pos.member).iter().all(|key| {
            self.members
                .get(key)
                .is_some_and(|member| member.valid && member.view.upgrade().is_some())
        }) && self.members.get(&pos.member).is_some_and(|member| {
            member.valid
                && member.view.upgrade().is_some()
                && member.text.is_char_boundary(pos.offset)
        })
    }

    // Lift nested endpoints to the common parent. Crossing a custom renderer
    // includes its entire parent atom; staying within one child stays partial.
    fn ancestors(&self, mut key: TextViewSelectionKey) -> Vec<TextViewSelectionKey> {
        let mut result = vec![key];
        while let Some((parent, _, _)) = self
            .members
            .get(&key)
            .and_then(|member| member.parent.as_ref())
        {
            result.push(*parent);
            key = *parent;
        }
        result
    }

    fn lifted(&self, pos: TextViewSelectionPosition, target: TextViewSelectionKey) -> Range<usize> {
        let mut key = pos.member;
        let mut range = pos.offset..pos.offset;
        while key != target {
            let (parent, parent_range, _) = self.members[&key]
                .parent
                .as_ref()
                .expect("ancestor is reachable");
            range = parent_range.clone();
            key = *parent;
        }
        range
    }

    fn range_for(&self, key: TextViewSelectionKey) -> Option<Range<usize>> {
        if !self.active {
            return None;
        }
        let (a, b) = self.selection?;
        if !self.valid_position(a) || !self.valid_position(b) {
            return None;
        }
        let member = self.members.get(&key)?;
        if !member.valid || member.view.upgrade().is_none() {
            return None;
        }
        let aa = self.ancestors(a.member);
        let bb = self.ancestors(b.member);
        if let Some(common) = aa.iter().find(|key| bb.contains(key)) {
            let ar = self.lifted(a, *common);
            let br = self.lifted(b, *common);
            return (key == *common).then_some(ar.start.min(br.start)..ar.end.max(br.end));
        }
        if member.parent.is_some() {
            return None;
        }
        let ak = *aa.last().expect("endpoint has an ancestor");
        let bk = *bb.last().expect("endpoint has an ancestor");
        let ar = self.lifted(a, ak);
        let br = self.lifted(b, bk);
        let (first, start, last, end) =
            if (self.members[&ak].order, ak) < (self.members[&bk].order, bk) {
                (ak, ar.start, bk, br.end)
            } else {
                (bk, br.start, ak, ar.end)
            };
        let low = (self.members[&first].order, first);
        let high = (self.members[&last].order, last);
        (low..=high).contains(&(member.order, key)).then(|| if key == first { start } else { 0 }..if key == last { end } else { member.text.len() })
    }

    fn ranges(&self) -> Vec<(TextViewSelectionKey, Range<usize>)> {
        let mut ranges = self
            .members
            .keys()
            .filter_map(|key| self.range_for(*key).map(|range| (*key, range)))
            .collect::<Vec<_>>();
        ranges.sort_by_key(|(key, _)| (self.members[key].order, *key));
        ranges
    }

    fn selected_text(&self) -> String {
        self.ranges()
            .into_iter()
            .filter_map(|(key, range)| {
                let text = &self.members[&key].text[range];
                (!text.is_empty()).then_some(text.to_owned())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl SelectionDocumentBinding {
    pub(crate) fn parse_failed(&self, cx: &mut App) {
        self.invalidate(cx);
        if let Some(document) = self.document.upgrade() {
            if let Some(member) = document.borrow_mut().members.get_mut(&self.key) {
                member.valid = false;
            }
        }
    }

    pub(crate) fn invalidate(&self, cx: &mut App) {
        let Some(document) = self.document.upgrade() else {
            return;
        };
        let mut state = document.borrow_mut();
        let endpoint_inside = state.selection.is_some_and(|(a, b)| {
            state.ancestors(a.member).contains(&self.key)
                || state.ancestors(b.member).contains(&self.key)
        });
        if endpoint_inside || state.range_for(self.key).is_some() {
            state.selection = None;
            drop(state);
            let document = TextViewSelectionDocument(document);
            crate::TextSelection::invalidate_document_positions(&document, cx);
            document.notify(cx);
        }
    }

    pub(crate) fn select_local(&self, range: Range<usize>, cx: &mut App) {
        if let Some(document) = self.document.upgrade() {
            let _ = TextViewSelectionDocument(document).select(
                TextViewSelectionPosition {
                    member: self.key,
                    offset: range.start,
                },
                TextViewSelectionPosition {
                    member: self.key,
                    offset: range.end,
                },
                cx,
            );
        }
    }

    pub(crate) fn clear(&self, cx: &mut App) {
        if let Some(document) = self.document.upgrade() {
            TextViewSelectionDocument(document).clear(cx);
        }
    }

    pub(crate) fn range(&self) -> Option<Range<usize>> {
        let document = self.document.upgrade()?;
        let state = document.borrow();
        state.range_for(self.key)
    }

    pub(crate) fn update_text(&self, rendered: &RenderedText, append: bool, cx: &mut App) {
        let Some(document) = self.document.upgrade() else {
            return;
        };
        let mut state = document.borrow_mut();
        let previous_selection = state.selection;
        let Some(member) = state.members.get(&self.key) else {
            return;
        };
        let previous_text = member.text.clone();
        let new_text = rendered.as_str();
        let changed = member.text.as_ref() != new_text;
        if changed {
            let protected = state
                .selection
                .map(|(a, b)| {
                    [a, b]
                        .into_iter()
                        .filter(|pos| state.ancestors(pos.member).contains(&self.key))
                        .map(|pos| state.lifted(pos, self.key).end)
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            let compatible = append && member.text.get(..protected) == new_text.get(..protected);
            let endpoint_inside = state.selection.is_some_and(|(a, b)| {
                state.ancestors(a.member).contains(&self.key)
                    || state.ancestors(b.member).contains(&self.key)
            });
            if !compatible && (endpoint_inside || state.range_for(self.key).is_some()) {
                state.selection = None;
            }
            if !append {
                let descendants = state
                    .members
                    .keys()
                    .copied()
                    .filter(|key| *key != self.key && state.ancestors(*key).contains(&self.key))
                    .collect::<Vec<_>>();
                for key in descendants {
                    state
                        .members
                        .get_mut(&key)
                        .expect("descendant exists")
                        .valid = false;
                }
            }
        }
        let member = state.members.get_mut(&self.key).expect("member exists");
        member.text = rendered.shared_text();
        member.rendered = Some(rendered.clone());
        for member in state.members.values_mut() {
            if let Some((parent, range, source)) = &mut member.parent {
                if *parent == self.key {
                    if changed && previous_text.get(..range.end) != new_text.get(..range.end) {
                        member.valid = false;
                    }
                    if let Some(updated) = rendered.range_for_source(source.clone()) {
                        *range = updated;
                    } else {
                        member.valid = false;
                    }
                }
            }
        }
        if state
            .selection
            .is_some_and(|(a, b)| !state.valid_position(a) || !state.valid_position(b))
        {
            state.selection = None;
        }
        let changed_selection = previous_selection != state.selection;
        drop(state);
        if changed_selection {
            let document = TextViewSelectionDocument(document);
            crate::TextSelection::invalidate_document_positions(&document, cx);
            document.notify(cx);
        }
    }
}
