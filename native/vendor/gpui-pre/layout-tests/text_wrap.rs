//! Public-API regressions for ASCII-space overflow in shaped text.

use std::sync::Arc;

use gpui::{NoopTextSystem, TextRun, TextSystem, WindowTextSystem, px};

fn assert_wraps(text: &str, width: f32, line_clamp: Option<usize>, expected: &[usize]) {
    let text_system = WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(NoopTextSystem))));
    let runs = [TextRun {
        len: text.len(),
        ..TextRun::default()
    }];
    // NoopTextSystem advances each BMP character by 6 px at this font size.
    let lines = text_system
        .shape_text(
            text.to_owned().into(),
            px(10.),
            &runs,
            Some(px(width)),
            line_clamp,
        )
        .expect("the deterministic text system must shape the test text");
    assert_eq!(lines.len(), 1, "the fixture has no hard line breaks");
    let line = &lines[0];
    assert_eq!(
        line.text.as_ref(),
        text,
        "wrapping must retain every character"
    );
    assert_eq!(
        line.len(),
        text.len(),
        "wrapping must retain the UTF-8 byte length"
    );

    let mut indices = Vec::new();
    for run in &line.unwrapped_layout.runs {
        for glyph in &run.glyphs {
            indices.push(glyph.index);
        }
    }
    let expected_indices: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
    assert_eq!(
        indices, expected_indices,
        "shaping must retain original byte indices"
    );

    let mut boundaries = Vec::new();
    for boundary in line.wrap_boundaries() {
        let glyph = &line.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix];
        boundaries.push(glyph.index);
    }
    assert_eq!(
        boundaries, expected,
        "wrap byte indices for {text:?}, clamp {line_clamp:?}"
    );
}

#[test]
fn fitting_word_stays_when_its_following_space_overflows() {
    assert_wraps("a b c", 18., None, &[4]);
    assert_wraps("é b c", 18., None, &[5]);
}

#[test]
fn repeated_spaces_wait_for_the_next_word() {
    assert_wraps("a b   c", 18., None, &[6]);
}

#[test]
fn trailing_spaces_do_not_create_extra_lines() {
    assert_wraps("a b ", 18., None, &[]);
    assert_wraps("a b   ", 18., None, &[]);
    assert_wraps("     ", 18., None, &[]);
}

#[test]
fn leading_spaces_keep_their_advances() {
    assert_wraps("  a b c", 18., None, &[4]);
    assert_wraps("    a", 18., None, &[4]);
}

#[test]
fn non_ascii_spaces_and_tabs_keep_existing_overflow_behavior() {
    // This preserves the toolkit's existing boundaries, including its NBSP
    // behavior; it does not introduce a Unicode line-breaking policy.
    // LineWrapper classifies both non-breaking spaces as word characters,
    // so the complete word moves. A tab remains a break opportunity.
    for (separator, boundary) in [('\u{00a0}', 2), ('\u{202f}', 2), ('\t', 3)] {
        assert_wraps(&format!("a b{separator}c"), 18., None, &[boundary]);
    }
}

#[test]
fn line_clamp_limits_the_corrected_breaks() {
    assert_wraps("a b c d e", 18., None, &[4, 8]);
    assert_wraps("a b c d e", 18., Some(1), &[]);
    assert_wraps("a b c d e", 18., Some(2), &[4]);
    assert_wraps("a b c d e", 18., Some(3), &[4, 8]);
    assert_wraps("a b   ", 18., Some(2), &[]);
}
