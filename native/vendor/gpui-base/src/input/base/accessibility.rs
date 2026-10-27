use gpui::accesskit::{Node, NodeId, Rect, Role, TextDirection, TextPosition, TextSelection};
use gpui::{A11ySubtreeBuilder, Bounds, Pixels, SharedString, point};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation as _;

use super::layout::LastLayout;

struct RunGeometry {
    bounds: Rect,
    positions: Vec<f32>,
    widths: Vec<f32>,
}

struct TextRun {
    bytes: Range<usize>,
    line: usize,
    characters: Vec<u8>,
    words: Vec<u8>,
    id: Cell<Option<NodeId>>,
    geometry: Option<RunGeometry>,
}

/// One rendered text snapshot. Actions validate it against the current document
/// before translating AccessKit positions back into editing-engine byte offsets.
pub(super) struct InputTextSnapshot {
    pub(super) text: SharedString,
    pub(super) revision: u64,
    runs: RefCell<Vec<TextRun>>,
    anchor: usize,
    focus: usize,
}

impl InputTextSnapshot {
    pub(super) fn new(text: SharedString, revision: u64, anchor: usize, focus: usize) -> Self {
        Self::with_breaks(text, revision, anchor, focus, &HashSet::new())
    }

    fn with_breaks(
        text: SharedString,
        revision: u64,
        anchor: usize,
        focus: usize,
        breaks: &HashSet<usize>,
    ) -> Self {
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
            // visual line so a long unwrapped line remains one navigable line.
            let visual_break = breaks.contains(&end);
            if newline || visual_break || characters.len() == 255 {
                runs.push(TextRun {
                    bytes: start..end,
                    line,
                    characters: std::mem::take(&mut characters),
                    words: std::mem::take(&mut words),
                    id: Cell::new(None),
                    geometry: None,
                });
                start = end;
                if newline || visual_break {
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
                geometry: None,
            });
        }
        Self {
            text,
            revision,
            runs: RefCell::new(runs),
            anchor,
            focus,
        }
    }

    /// Use only the current prepaint's shaped positions. Unlaid-out document
    /// ranges keep their text but have no advertised bounds.
    pub(super) fn set_layout(&self, layout: &LastLayout, bounds: Bounds<Pixels>, scale: f32) {
        let mut breaks = HashSet::new();
        for (line, start) in layout.lines.iter().zip(&layout.visible_line_byte_offsets) {
            let mut offset = *start;
            for wrapped in line
                .wrapped_lines
                .iter()
                .take(line.wrapped_lines.len().saturating_sub(1))
            {
                offset += wrapped.len;
                breaks.insert(offset);
            }
        }
        let rebuilt = Self::with_breaks(
            self.text.clone(),
            self.revision,
            self.anchor,
            self.focus,
            &breaks,
        );
        let mut runs = rebuilt.runs.into_inner();
        let mut top = layout.visible_top;
        let mut run_index = 0;
        for (line, start) in layout.lines.iter().zip(&layout.visible_line_byte_offsets) {
            let end = self.text[*start..]
                .find('\n')
                .map_or(self.text.len(), |end| start + end + 1);
            let text = &self.text[*start..end];
            let content_end = text
                .strip_suffix("\r\n")
                .or_else(|| text.strip_suffix('\n'))
                .unwrap_or(text)
                .len();
            while run_index < runs.len() && runs[run_index].bytes.start < *start {
                run_index += 1;
            }
            while let Some(run) = runs.get_mut(run_index) {
                if run.bytes.start >= end && !(run.bytes.is_empty() && *start == end) {
                    break;
                }
                let local_start = run.bytes.start - start;
                let local_end = (run.bytes.end - start).min(content_end);
                let origin = point(
                    bounds.origin.x + layout.line_number_width,
                    bounds.origin.y + top,
                );
                if let (Some(first), Some(last)) = (
                    line.position_for_index(local_start.min(content_end), layout, false),
                    line.position_for_index(local_end, layout, true),
                ) {
                    let mut positions = Vec::new();
                    let mut widths = Vec::new();
                    let mut offset = local_start;
                    for length in &run.characters {
                        let next = offset + usize::from(*length);
                        let Some(a) =
                            line.position_for_index(offset.min(content_end), layout, false)
                        else {
                            break;
                        };
                        let Some(b) = line.position_for_index(next.min(content_end), layout, true)
                        else {
                            break;
                        };
                        positions.push(f32::from(a.x - first.x) * scale);
                        widths.push(f32::from(b.x - a.x) * scale);
                        offset = next;
                    }
                    // AccessKit's single-direction run geometry cannot represent
                    // a non-monotonic bidi layout. Leave that geometry unsupported.
                    if positions.len() == run.characters.len()
                        && widths.iter().all(|width| *width >= 0.)
                        && last.y == first.y
                    {
                        run.geometry = Some(RunGeometry {
                            bounds: Rect::new(
                                f64::from(f32::from(origin.x + first.x) * scale),
                                f64::from(f32::from(origin.y + first.y) * scale),
                                f64::from(f32::from(origin.x + last.x) * scale),
                                f64::from(
                                    f32::from(origin.y + first.y + layout.line_height) * scale,
                                ),
                            ),
                            positions,
                            widths,
                        });
                    }
                }
                run_index += 1;
            }
            top += line.size(layout.line_height).height;
        }
        *self.runs.borrow_mut() = runs;
    }

    pub(super) fn emit(&self, builder: &mut A11ySubtreeBuilder) {
        let runs = self.runs.borrow();
        for run in runs.iter() {
            run.id.set(Some(builder.synthetic_node_id((
                "input-text",
                self.revision,
                run.bytes.start,
                run.bytes.end,
            ))));
        }
        for (index, run) in runs.iter().enumerate() {
            let mut node = Node::new(Role::TextRun);
            node.set_value(self.text[run.bytes.clone()].to_owned());
            node.set_character_lengths(run.characters.clone());
            node.set_word_starts(run.words.clone());
            if let Some(geometry) = &run.geometry {
                node.set_bounds(geometry.bounds);
                node.set_text_direction(TextDirection::LeftToRight);
                node.set_character_positions(geometry.positions.clone());
                node.set_character_widths(geometry.widths.clone());
            }
            if let Some(previous) = index.checked_sub(1).and_then(|index| runs.get(index))
                && previous.line == run.line
            {
                node.set_previous_on_line(
                    previous.id.get().expect("run IDs assigned before emission"),
                );
            }
            if let Some(next) = runs.get(index + 1)
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
        let runs = self.runs.borrow();
        let run = runs
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
        let runs = self.runs.borrow();
        let run = runs
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
