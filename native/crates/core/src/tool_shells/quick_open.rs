//! The file a file-changing row opens, and the line it opens it at
//! (`getPrimaryToolPath`, `resolveToolQuickOpenTarget` and
//! `extractFirstChangedLineFromDiff` in `toolDiffUtils.ts`).

use crate::path;
use crate::tool_names::normalize;
use crate::tool_shells::ToolFacts;

/// What a row's open action targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuickOpenTarget<'a> {
    /// The path as the call reported it, absolute or relative to the session
    /// directory.
    pub path: &'a str,
    /// The first changed line of the file's patch, 1-based.
    pub line: Option<u32>,
}

/// The one file a call changed: a `patch` call's first file it did not
/// delete, an `edit` call's reported file or else its input path, a `write`
/// call's input path. Every other tool has none.
#[must_use]
pub fn primary_path<'a>(tool: &str, facts: &'a ToolFacts) -> Option<&'a str> {
    match normalize(tool).as_ref() {
        "patch" => facts
            .metadata
            .files
            .iter()
            .find(|file| !file.deleted)
            .map(|file| file.file.as_str()),
        "edit" => facts
            .metadata
            .files
            .first()
            .map(|file| file.file.as_str())
            .or(facts.input.path.as_deref()),
        "write" => facts.input.path.as_deref(),
        _ => None,
    }
}

/// The file and line a row's open action goes to; `None` for a call that
/// changed no file it can name.
///
/// The line comes from the patch the opened row draws for that file: the
/// file's own patch, the only patch when just one file carries one, or the
/// call's top-level patch when no file does.
#[must_use]
pub fn target<'a>(tool: &str, facts: &'a ToolFacts) -> Option<QuickOpenTarget<'a>> {
    let path = primary_path(tool, facts)?;
    let mut patches = facts
        .metadata
        .files
        .iter()
        .filter_map(|file| Some((file.file.as_str(), file.patch.as_deref()?)));
    let first = patches.next();
    let patch = match first {
        None => facts.metadata.patch.as_deref(),
        Some((file, patch)) if file == path => Some(patch),
        Some((_, only)) => {
            let mut rest = patches.peekable();
            if rest.peek().is_none() {
                Some(only)
            } else {
                rest.find(|(file, _)| *file == path).map(|(_, patch)| patch)
            }
        }
    };
    Some(QuickOpenTarget {
        path,
        line: patch.and_then(first_changed_line),
    })
}

/// The new-file line of a patch's first added line, else the start of its
/// first hunk; `None` for text without a hunk header.
#[must_use]
pub fn first_changed_line(patch: &str) -> Option<u32> {
    let mut current: Option<u32> = None;
    let mut first_hunk: Option<u32> = None;
    for raw in patch.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(start) = hunk_start(line) {
            let start = start.max(1);
            current = Some(start);
            first_hunk.get_or_insert(start);
            continue;
        }
        let Some(new_line) = current else {
            continue;
        };
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
            continue;
        }
        if line.starts_with('+') {
            return Some(new_line);
        }
        if line.starts_with(' ') {
            current = Some(new_line.saturating_add(1));
        }
    }
    first_hunk
}

/// The new-file start of a `@@ -a,b +c,d @@` header.
fn hunk_start(line: &str) -> Option<u32> {
    let rest = spaced(line.strip_prefix("@@")?)?;
    let (_, rest) = range(rest.strip_prefix('-')?)?;
    let (start, rest) = range(spaced(rest)?.strip_prefix('+')?)?;
    spaced(rest)?.starts_with("@@").then_some(start)
}

/// `text` after its leading whitespace, when it has any.
fn spaced(text: &str) -> Option<&str> {
    let rest = text.trim_start();
    (rest.len() < text.len()).then_some(rest)
}

/// A `start` or `start,count` range: its start and what follows it.
fn range(text: &str) -> Option<(u32, &str)> {
    let (start, rest) = digits(text)?;
    let rest = match rest.strip_prefix(',').and_then(digits) {
        Some((_, after_count)) => after_count,
        None => rest,
    };
    Some((start.parse().ok()?, rest))
}

/// The leading ASCII digits of `text` and what follows them.
fn digits(text: &str) -> Option<(&str, &str)> {
    let end = text
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(text.len());
    (end > 0).then(|| text.split_at(end))
}

/// `path` as an absolute path: itself when it already is one, else joined to
/// the session `directory` (`toAbsoluteFilePath`). `None` when a relative
/// path has no directory to join.
#[must_use]
pub fn absolute_path(path: &str, directory: &str) -> Option<String> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    if path.starts_with('/') || drive {
        return path::normalize(path);
    }
    let directory = path::normalize(directory)?;
    let relative = path.strip_prefix("./").unwrap_or(path);
    path::normalize(&format!("{}/{relative}", directory.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_shells::{ToolFileDiff, ToolInput};

    const PATCH: &str =
        "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -10,3 +12,4 @@ fn a()\n context\n-old\n+new\n";

    fn file(name: &str, patch: Option<&str>) -> ToolFileDiff {
        ToolFileDiff {
            file: name.to_owned(),
            patch: patch.map(str::to_owned),
            ..ToolFileDiff::default()
        }
    }

    #[test]
    fn finds_the_first_added_line_or_the_hunk_start() {
        assert_eq!(first_changed_line(PATCH), Some(13));
        assert_eq!(
            first_changed_line("@@ -4 +7 @@\r\n-removed only\r\n"),
            Some(7),
            "a hunk without an added line opens at its start"
        );
        assert_eq!(
            first_changed_line("@@ -0,0 +0,0 @@\n"),
            Some(1),
            "an empty file's hunk opens at line 1"
        );
        assert_eq!(first_changed_line("+not a diff"), None);
        assert_eq!(first_changed_line("@@ broken @@\n+x"), None);
        assert_eq!(first_changed_line(""), None);
    }

    #[test]
    fn each_tool_names_its_own_primary_file() {
        let mut facts = ToolFacts {
            input: ToolInput {
                path: Some("input.rs".to_owned()),
                ..ToolInput::default()
            },
            ..ToolFacts::default()
        };
        assert_eq!(primary_path("write", &facts), Some("input.rs"));
        assert_eq!(primary_path("edit", &facts), Some("input.rs"));
        assert_eq!(primary_path("patch", &facts), None);
        assert_eq!(primary_path("shell", &facts), None);
        assert_eq!(primary_path("subagent", &facts), None);
        facts.metadata.files = vec![
            ToolFileDiff {
                deleted: true,
                ..file("gone.rs", None)
            },
            file("kept.rs", None),
        ];
        assert_eq!(primary_path("patch", &facts), Some("kept.rs"));
        assert_eq!(primary_path("edit", &facts), Some("gone.rs"));
        assert_eq!(primary_path("write", &facts), Some("input.rs"));
    }

    #[test]
    fn the_line_comes_from_the_patch_drawn_for_the_file() {
        let other = "@@ -1 +40 @@\n+x\n";
        let mut facts = ToolFacts::default();
        facts.metadata.files = vec![file("a.rs", Some(other)), file("b.rs", Some(PATCH))];
        assert_eq!(
            target("patch", &facts),
            Some(QuickOpenTarget {
                path: "a.rs",
                line: Some(40)
            })
        );
        facts.metadata.files = vec![file("a.rs", None), file("b.rs", Some(PATCH))];
        assert_eq!(
            target("patch", &facts).and_then(|target| target.line),
            Some(13),
            "the only patch stands in for the primary file"
        );
        facts.metadata.files = vec![
            file("a.rs", None),
            file("b.rs", Some(PATCH)),
            file("c.rs", Some(other)),
        ];
        assert_eq!(
            target("patch", &facts),
            Some(QuickOpenTarget {
                path: "a.rs",
                line: None
            }),
            "two other files' patches say nothing about this one"
        );
        facts.metadata.files = Vec::new();
        facts.metadata.patch = Some(other.to_owned());
        facts.input.path = Some("w.rs".to_owned());
        assert_eq!(
            target("edit", &facts),
            Some(QuickOpenTarget {
                path: "w.rs",
                line: Some(40)
            })
        );
        assert_eq!(target("read", &facts), None);
    }

    #[test]
    fn relative_paths_join_the_session_directory() {
        assert_eq!(
            absolute_path("src/a.rs", "/repo/").as_deref(),
            Some("/repo/src/a.rs")
        );
        assert_eq!(
            absolute_path("./a.rs", "/repo").as_deref(),
            Some("/repo/a.rs")
        );
        assert_eq!(
            absolute_path(" /abs/a.rs ", "/repo").as_deref(),
            Some("/abs/a.rs")
        );
        assert_eq!(
            absolute_path("c:\\w\\a.rs", "").as_deref(),
            Some("C:/w/a.rs")
        );
        assert_eq!(absolute_path("a.rs", ""), None);
        assert_eq!(absolute_path("  ", "/repo"), None);
    }
}
