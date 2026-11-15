//! The LSP errors a file-changing call reported for the file it changed
//! (`getToolDiagnosticSection` in `ToolPart.tsx`).

use crate::tool_shells::{self, ToolFacts, quick_open};

/// How many errors a row lists before it counts the rest.
pub const MAX_PER_FILE: usize = 5;

/// One LSP error, at its 1-based position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDiagnostic {
    pub message: String,
    pub line: u32,
    pub character: u32,
}

/// The errors `metadata.diagnostics` lists under one key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDiagnostics {
    /// The key exactly as the call reported it.
    pub path: String,
    /// Errors only; warnings and hints are left out when the facts are read.
    pub diagnostics: Vec<ToolDiagnostic>,
}

/// What the row draws: the file, its first errors and how many more it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticSection<'a> {
    /// The file relative to the session directory.
    pub display_path: String,
    pub diagnostics: &'a [ToolDiagnostic],
    pub remaining: usize,
}

/// The errors of the file a file-changing call changed; `None` when it
/// reported none for that file.
///
/// The call's primary file is looked up by its path as reported and then by
/// its absolute path, as the reference does.
#[must_use]
pub fn section<'a>(
    tool: &str,
    facts: &'a ToolFacts,
    directory: &str,
) -> Option<DiagnosticSection<'a>> {
    if !tool_shells::is_file_change(tool) || facts.metadata.diagnostics.is_empty() {
        return None;
    }
    let path = display_path(quick_open::primary_path(tool, facts)?);
    if path.is_empty() {
        return None;
    }
    let absolute = if path.starts_with('/') {
        path.clone()
    } else {
        collapse_slashes(&format!("{}/{path}", display_path(directory)))
    };
    let listed = &facts.metadata.diagnostics;
    let entry = listed
        .iter()
        .find(|entry| entry.path == path)
        .or_else(|| listed.iter().find(|entry| entry.path == absolute))?;
    if entry.diagnostics.is_empty() {
        return None;
    }
    let shown = entry.diagnostics.len().min(MAX_PER_FILE);
    Some(DiagnosticSection {
        display_path: if path.starts_with('/') {
            tool_shells::relative_path(&path, &display_path(directory)).into_owned()
        } else {
            path
        },
        diagnostics: &entry.diagnostics[..shown],
        remaining: entry.diagnostics.len() - shown,
    })
}

/// `normalizeDisplayPath`: forward slashes, none doubled, none trailing.
fn display_path(value: &str) -> String {
    let collapsed = collapse_slashes(&value.trim().replace('\\', "/"));
    if collapsed.is_empty() || collapsed == "/" {
        return collapsed;
    }
    collapsed.trim_end_matches('/').to_owned()
}

fn collapse_slashes(value: &str) -> String {
    let mut collapsed = String::with_capacity(value.len());
    for character in value.chars() {
        if character == '/' && collapsed.ends_with('/') {
            continue;
        }
        collapsed.push(character);
    }
    collapsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_shells::{ToolFileDiff, ToolInput};

    fn errors(count: u32) -> Vec<ToolDiagnostic> {
        (1..=count)
            .map(|line| ToolDiagnostic {
                message: format!("error {line}"),
                line,
                character: 1,
            })
            .collect()
    }

    fn edit(path: &str, listed: Vec<FileDiagnostics>) -> ToolFacts {
        let mut facts = ToolFacts {
            input: ToolInput {
                path: Some(path.to_owned()),
                ..ToolInput::default()
            },
            ..ToolFacts::default()
        };
        facts.metadata.diagnostics = listed;
        facts
    }

    #[test]
    fn lists_five_errors_and_counts_the_rest() {
        let facts = edit(
            "/repo/src/a.rs",
            vec![FileDiagnostics {
                path: "/repo/src/a.rs".to_owned(),
                diagnostics: errors(7),
            }],
        );
        let section = section("edit", &facts, "/repo/").expect("the file has errors");
        assert_eq!(section.display_path, "src/a.rs");
        assert_eq!(section.diagnostics.len(), 5);
        assert_eq!(section.diagnostics[4].message, "error 5");
        assert_eq!(section.remaining, 2);
    }

    #[test]
    fn a_relative_file_finds_its_absolute_key() {
        let facts = edit(
            "src\\a.rs",
            vec![
                FileDiagnostics {
                    path: "/repo/src/b.rs".to_owned(),
                    diagnostics: errors(3),
                },
                FileDiagnostics {
                    path: "/repo/src/a.rs".to_owned(),
                    diagnostics: errors(1),
                },
            ],
        );
        let section = section("write", &facts, "/repo").expect("the absolute key matches");
        assert_eq!(section.display_path, "src/a.rs");
        assert_eq!(section.diagnostics.len(), 1);
        assert_eq!(section.remaining, 0);
    }

    #[test]
    fn other_files_and_other_tools_show_nothing() {
        let listed = vec![FileDiagnostics {
            path: "/repo/other.rs".to_owned(),
            diagnostics: errors(2),
        }];
        assert_eq!(
            section("edit", &edit("/repo/a.rs", listed.clone()), "/repo"),
            None,
            "another file's errors are not this file's"
        );
        assert_eq!(
            section("shell", &edit("/repo/other.rs", listed.clone()), "/repo"),
            None
        );
        let warnings_only = vec![FileDiagnostics {
            path: "/repo/a.rs".to_owned(),
            diagnostics: Vec::new(),
        }];
        assert_eq!(
            section("edit", &edit("/repo/a.rs", warnings_only), "/repo"),
            None
        );
        let mut patch = edit("/repo/other.rs", listed);
        patch.metadata.files = vec![ToolFileDiff {
            file: "/repo/other.rs".to_owned(),
            deleted: true,
            ..ToolFileDiff::default()
        }];
        assert_eq!(
            section("patch", &patch, "/repo"),
            None,
            "a patch that only deleted files has no primary file"
        );
    }
}
