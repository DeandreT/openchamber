use gpui::accesskit::{Node, NodeId, Role, TextPosition, TextSelection};
use gpui::{A11ySubtreeBuilder, SharedString};
use std::cell::Cell;
use std::collections::HashSet;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation as _;

struct TextRun {
    bytes: Range<usize>,
    line: usize,
    characters: Vec<u8>,
    words: Vec<u8>,
    id: Cell<Option<NodeId>>,
}

/// One rendered text snapshot. Actions validate it against the current document
/// before translating AccessKit positions back into editing-engine byte offsets.
pub(super) struct InputTextSnapshot {
    pub(super) text: SharedString,
    pub(super) revision: u64,
    runs: Vec<TextRun>,
    anchor: usize,
    focus: usize,
}

impl InputTextSnapshot {
    pub(super) fn new(text: SharedString, revision: u64, anchor: usize, focus: usize) -> Self {
        let word_starts: HashSet<usize> = text
            .split_word_bound_indices()
            .filter(|(_, word)| !word.trim_start().is_empty())
            .map(|(offset, _)| offset)
            .collect();
        let mut runs = Vec::new();
        let mut start = 0;
        let mut line = 0;
        let mut characters = Vec::new();
        let mut words = Vec::new();
        let mut chars = text.char_indices().peekable();
        while let Some((offset, character)) = chars.next() {
            if word_starts.contains(&offset) {
                words.push(
                    u8::try_from(characters.len()).expect("runs have at most 255 characters"),
                );
            }
            let mut length = character.len_utf8();
            let newline = character == '\n'
                || (character == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n'));
            if character == '\r' && newline {
                chars.next();
                length += 1;
            }
            characters.push(u8::try_from(length).expect("Unicode scalars and CRLF fit in u8"));
            let end = offset + length;
            // AccessKit word-start indices are u8. Link fragments on the same
            // logical line so a long line remains one navigable line.
            if newline || characters.len() == 255 {
                runs.push(TextRun {
                    bytes: start..end,
                    line,
                    characters: std::mem::take(&mut characters),
                    words: std::mem::take(&mut words),
                    id: Cell::new(None),
                });
                start = end;
                if newline {
                    line += 1;
                }
            }
        }
        // Keep an empty document and the caret after a trailing newline
        // addressable through the Text interface too.
        if start < text.len() || runs.is_empty() || text.ends_with('\n') {
            runs.push(TextRun {
                bytes: start..text.len(),
                line,
                characters,
                words,
                id: Cell::new(None),
            });
        }
        Self {
            text,
            revision,
            runs,
            anchor,
            focus,
        }
    }

    pub(super) fn emit(&self, builder: &mut A11ySubtreeBuilder) {
        for (index, run) in self.runs.iter().enumerate() {
            run.id.set(Some(builder.synthetic_node_id((
                "input-text",
                self.revision,
                index,
            ))));
        }
        for (index, run) in self.runs.iter().enumerate() {
            let mut node = Node::new(Role::TextRun);
            node.set_value(self.text[run.bytes.clone()].to_owned());
            node.set_character_lengths(run.characters.clone());
            node.set_word_starts(run.words.clone());
            if let Some(previous) = index.checked_sub(1).and_then(|index| self.runs.get(index))
                && previous.line == run.line
            {
                node.set_previous_on_line(
                    previous.id.get().expect("run IDs assigned before emission"),
                );
            }
            if let Some(next) = self.runs.get(index + 1)
                && next.line == run.line
            {
                node.set_next_on_line(next.id.get().expect("run IDs assigned before emission"));
            }
            builder.push_child(
                run.id.get().expect("run IDs assigned before emission"),
                node,
            );
        }
        builder.parent_node().set_text_selection(TextSelection {
            anchor: self.position(self.anchor),
            focus: self.position(self.focus),
        });
    }

    fn position(&self, offset: usize) -> TextPosition {
        let offset = offset.min(self.text.len());
        let run = self
            .runs
            .iter()
            .rev()
            .find(|run| run.bytes.start <= offset)
            .expect("every snapshot has a text run");
        let mut byte = run.bytes.start;
        let mut character_index = 0;
        for length in &run.characters {
            if byte + usize::from(*length) > offset {
                break;
            }
            byte += usize::from(*length);
            character_index += 1;
        }
        TextPosition {
            node: run.id.get().expect("positions are emitted after text runs"),
            character_index,
        }
    }

    pub(super) fn byte_offset(&self, position: TextPosition) -> Option<usize> {
        let run = self
            .runs
            .iter()
            .find(|run| run.id.get() == Some(position.node))?;
        let lengths = run.characters.get(..position.character_index)?;
        Some(
            run.bytes.start
                + lengths
                    .iter()
                    .map(|length| usize::from(*length))
                    .sum::<usize>(),
        )
    }
}
