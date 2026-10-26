//! Retained Markdown state and logical selection for one session's transcript.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::text::{
    MarkdownExtensions, TextView, TextViewSelectionDocument, TextViewSelectionKey, TextViewState,
};
use gpui_kit::{App, AppContext, Entity, WeakEntity};
use openchamber_native_core::ids::{MessageId, PartId};
use openchamber_native_core::transcript::{
    Message, PartContent, Role, Transcript, TranscriptChange,
};

use crate::markdown_view::{self, Author, Nesting};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MemberId {
    User(MessageId),
    Part(MessageId, PartId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ChildId {
    start: usize,
    ordinal: usize,
    code: bool,
}

/// The view and custom-block views stay alive while their message exists.
pub(crate) struct RetainedText {
    pub(crate) text: Entity<TextViewState>,
    document: TextViewSelectionDocument,
    key: TextViewSelectionKey,
    keys: Rc<Cell<u64>>,
    children: HashMap<ChildId, Entity<RetainedText>>,
    extensions: Arc<MarkdownExtensions>,
}

fn next_key(keys: &Cell<u64>) -> TextViewSelectionKey {
    let key = keys.get();
    keys.set(
        key.checked_add(1)
            .expect("selection member IDs are not exhausted"),
    );
    TextViewSelectionKey(key)
}

impl RetainedText {
    fn new(
        document: TextViewSelectionDocument,
        keys: Rc<Cell<u64>>,
        source: &str,
        nesting: Option<Nesting>,
        cx: &mut App,
    ) -> Entity<Self> {
        let key = next_key(&keys);
        cx.new(|cx| {
            let owner = cx.entity().downgrade();
            let extensions = nesting.map_or_else(
                || Arc::new(MarkdownExtensions::default()),
                |nesting| markdown_view::extensions(nesting, Some(owner)),
            );
            let text = cx.new(|cx| {
                let mut text = TextViewState::markdown(source, cx);
                text.set_markdown_extensions(extensions.clone(), cx);
                text
            });
            Self {
                text,
                document,
                key,
                keys,
                children: HashMap::new(),
                extensions,
            }
        })
    }

    fn element(&self) -> TextView {
        TextView::new(&self.text).markdown_extensions(self.extensions.as_ref().clone())
    }

    fn update_text(&mut self, source: &str, cx: &mut App) {
        if let Err(error) = self.document.set_text(self.key, source, cx) {
            tracing::warn!(%error, "retained Markdown could not be updated");
        }
        let removed: Vec<ChildId> = self
            .children
            .iter()
            .filter(|(_, child)| self.document.member(child.read(cx).key).is_none())
            .map(|(id, _)| id.clone())
            .collect();
        for id in removed {
            if let Some(child) = self.children.remove(&id) {
                self.document.remove(child.read(cx).key, cx);
            }
        }
    }
}

/// A custom block asks its retained parent for its own retained text view.
pub(crate) fn child(
    owner: &WeakEntity<RetainedText>,
    source: Range<usize>,
    ordinal: usize,
    text: &str,
    nesting: Option<Nesting>,
    cx: &mut App,
) -> Option<TextView> {
    owner
        .update(cx, |parent, cx| {
            let id = ChildId {
                start: source.start,
                ordinal,
                code: nesting.is_none(),
            };
            if let Some(child) = parent.children.get(&id) {
                let key = child.read(cx).key;
                if let Err(error) = parent.document.set_child_source(key, source.clone(), cx) {
                    tracing::warn!(%error, "custom Markdown source range could not be updated");
                    return None;
                }
                child.update(cx, |child, cx| child.update_text(text, cx));
                return Some(child.read(cx).element());
            }
            let child = RetainedText::new(
                parent.document.clone(),
                parent.keys.clone(),
                text,
                nesting,
                cx,
            );
            let state = child.read(cx).text.clone();
            let key = child.read(cx).key;
            if let Err(error) = parent
                .document
                .attach_child(key, parent.key, source, &state, cx)
            {
                tracing::warn!(%error, "custom Markdown selection could not be attached");
                return None;
            }
            let element = child.read(cx).element();
            parent.children.insert(id, child);
            Some(element)
        })
        .ok()
        .flatten()
}

struct Member {
    state: Entity<RetainedText>,
    order: u64,
}

/// Its lifetime and IDs are scoped to one OpenSession, including its runtime.
pub(crate) struct TranscriptSelection {
    pub(crate) document: TextViewSelectionDocument,
    keys: Rc<Cell<u64>>,
    members: HashMap<MemberId, Member>,
    rows: HashMap<MessageId, Vec<MemberId>>,
}

impl TranscriptSelection {
    pub(crate) fn new(transcript: &Transcript, cx: &mut App) -> Self {
        let mut selection = Self {
            document: TextViewSelectionDocument::new(),
            keys: Rc::new(Cell::new(1)),
            members: HashMap::new(),
            rows: HashMap::new(),
        };
        selection.sync(transcript, &TranscriptChange::Reset, cx);
        selection
    }

    pub(crate) fn view(
        &self,
        message: &MessageId,
        part: Option<&PartId>,
        cx: &App,
    ) -> Option<TextView> {
        let id = part.map_or_else(
            || MemberId::User(message.clone()),
            |part| MemberId::Part(message.clone(), part.clone()),
        );
        self.members
            .get(&id)
            .map(|member| member.state.read(cx).element())
    }

    pub(crate) fn sync(
        &mut self,
        transcript: &Transcript,
        change: &TranscriptChange,
        cx: &mut App,
    ) {
        match change {
            TranscriptChange::Unchanged | TranscriptChange::NeedsReconcile => return,
            TranscriptChange::Updated { row } => {
                if let Some(message) = transcript.message(*row) {
                    self.sync_row(message, *row, cx);
                }
                return;
            }
            _ => {}
        }
        let mut live = HashSet::new();
        for (row, message) in transcript.messages().iter().enumerate() {
            live.insert(message.info.id.clone());
            self.sync_row(message, row, cx);
        }
        let removed: Vec<MessageId> = self
            .rows
            .keys()
            .filter(|id| !live.contains(*id))
            .cloned()
            .collect();
        for message in removed {
            if let Some(members) = self.rows.remove(&message) {
                for member in members {
                    self.remove(&member, cx);
                }
            }
        }
    }

    fn sync_row(&mut self, message: &Message, row: usize, cx: &mut App) {
        let mut desired = Vec::new();
        match message.info.role {
            Role::User => {
                let id = MemberId::User(message.info.id.clone());
                self.sync_member(&id, &message.plain_text(), row, 0, Author::User, cx);
                desired.push(id);
            }
            Role::Assistant => {
                for (index, part) in message.parts.iter().enumerate() {
                    if let PartContent::Text { text } = &part.content {
                        let id = MemberId::Part(message.info.id.clone(), part.id.clone());
                        self.sync_member(&id, text, row, index, Author::Assistant, cx);
                        desired.push(id);
                    }
                }
            }
            _ => {}
        }
        if let Some(previous) = self.rows.insert(message.info.id.clone(), desired.clone()) {
            for id in previous {
                if !desired.contains(&id) {
                    self.remove(&id, cx);
                }
            }
        }
    }

    fn sync_member(
        &mut self,
        id: &MemberId,
        source: &str,
        row: usize,
        part: usize,
        author: Author,
        cx: &mut App,
    ) {
        let order = ((row as u64) << 32) | part as u64;
        if let Some(member) = self.members.get_mut(id) {
            member
                .state
                .update(cx, |state, cx| state.update_text(source, cx));
            if member.order != order {
                if let Err(error) = self
                    .document
                    .set_order(member.state.read(cx).key, order, cx)
                {
                    tracing::warn!(%error, "Markdown selection order could not be updated");
                }
                member.order = order;
            }
            return;
        }
        let state = RetainedText::new(
            self.document.clone(),
            self.keys.clone(),
            source,
            Some(Nesting {
                author,
                ..Nesting::default()
            }),
            cx,
        );
        let key = state.read(cx).key;
        let text = state.read(cx).text.clone();
        if let Err(error) = self.document.attach(key, order, &text, cx) {
            tracing::warn!(%error, "Markdown selection could not be attached");
        }
        self.members.insert(id.clone(), Member { state, order });
    }

    fn remove(&mut self, id: &MemberId, cx: &mut App) {
        if let Some(member) = self.members.remove(id) {
            self.document.remove(member.state.read(cx).key, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::base::text::TextViewSelectionPosition;

    #[gpui_kit::test]
    fn growing_custom_blocks_keep_one_child_and_its_selection(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            let document = TextViewSelectionDocument::new();
            let parent = RetainedText::new(
                document.clone(),
                Rc::new(Cell::new(1)),
                "# first",
                Some(Nesting::default()),
                cx,
            );
            let key = parent.read(cx).key;
            document
                .attach(key, 0, &parent.read(cx).text.clone(), cx)
                .expect("parent attaches");
            let owner = parent.downgrade();
            child(&owner, 0..7, 0, "first", Some(Nesting::default()), cx).expect("child attaches");
            let key = parent
                .read(cx)
                .children
                .values()
                .next()
                .expect("child exists")
                .read(cx)
                .key;
            document.activate(window, cx);
            document
                .select(
                    TextViewSelectionPosition {
                        member: key,
                        offset: 0,
                    },
                    TextViewSelectionPosition {
                        member: key,
                        offset: 5,
                    },
                    cx,
                )
                .expect("selection is valid");
            for source in ["# first next", "# first next again"] {
                parent.update(cx, |parent, cx| parent.update_text(source, cx));
                child(
                    &owner,
                    0..source.len(),
                    0,
                    &source[2..],
                    Some(Nesting::default()),
                    cx,
                )
                .expect("growing child attaches");
                assert_eq!(
                    parent.read(cx).children.len(),
                    1,
                    "streaming does not retain old child versions"
                );
                let current = parent
                    .read(cx)
                    .children
                    .values()
                    .next()
                    .expect("child exists")
                    .read(cx)
                    .key;
                assert_eq!(current, key, "a growing block keeps its identity");
                assert_eq!(document.selected_text(), "first");
            }
        });
    }
}
