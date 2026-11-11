//! What a tool row shows, decided from the call's own input and metadata.
//!
//! Ports the decisions of `lib/opencode/tools.ts`, `lib/toolHelpers.ts`
//! (display names), `toolPresentation.tsx` (icons), `toolRenderUtils.ts`
//! (static and expandable rows) and the header helpers of `ToolPart.tsx`.
//! The transport reads the free-form JSON into [`ToolFacts`] once, so these
//! functions stay pure and every field a row reads is typed.

use std::borrow::Cow;

use crate::tool_names::normalize;

pub mod diagnostics;
pub mod question;
pub mod quick_open;
pub mod web_search;

/// The fields of a call's input the rows read (`inputSchema` in `tools.ts`).
/// Empty strings are absent, as `optionalText` reads them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolInput {
    /// `path`, else `filePath`, else `file_path`.
    pub path: Option<String>,
    pub command: Option<String>,
    /// A `subagent` call's prompt.
    pub prompt: Option<String>,
    pub description: Option<String>,
    pub agent: Option<String>,
    pub name: Option<String>,
    pub id: Option<String>,
    pub title: Option<String>,
    pub directory: Option<String>,
    pub search: Option<String>,
    pub provider: Option<String>,
    pub patch_text: Option<String>,
    pub pattern: Option<String>,
    pub query: Option<String>,
    pub url: Option<String>,
    pub code: Option<String>,
    /// A `write` call's file content.
    pub content: Option<String>,
    /// How many questions a `question` call asks.
    pub questions: usize,
    /// Those questions as the opened row prints them. An entry that is not a
    /// record is counted above and left out here.
    pub asked: Vec<question::AskedQuestion>,
    /// OpenChamber's own tools: the `action` they ran.
    pub action: Option<String>,
    /// The detail of an OpenChamber tool's `parameters` that tells calls apart.
    pub parameter_detail: Option<String>,
    /// The page an `openchamber_render` call shows.
    pub page: crate::rendered_page::PageInput,
}

/// One file a completed `edit` or `patch` call reported (`FileDiff.Info`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolFileDiff {
    pub file: String,
    pub patch: Option<String>,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    /// The call deleted this file (`status: "deleted"` or `type: "delete"`),
    /// so there is nothing to open.
    pub deleted: bool,
}

/// A tool an `execute` script called.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecuteToolCall {
    pub tool: String,
    pub status: Option<String>,
    /// The arguments as compact one-line JSON, cut at 160 characters.
    pub input: Option<String>,
}

/// The fields of a call's metadata the rows read (`metadataSchema`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolMetadata {
    pub files: Vec<ToolFileDiff>,
    /// A top-level `patch` or `diff` text, when no files are listed.
    pub patch: Option<String>,
    pub name: Option<String>,
    /// The background shell a `shell` call moved to (`metadata.shellID`).
    pub shell_id: Option<String>,
    pub tool_calls: Vec<ExecuteToolCall>,
    pub truncated: bool,
    pub output_path: Option<String>,
    /// LSP errors by file, in the order `metadata.diagnostics` lists them.
    pub diagnostics: Vec<diagnostics::FileDiagnostics>,
    /// The provider a `websearch` call searched with (`metadata.provider`).
    pub provider: Option<String>,
    /// OpenChamber's result for a render call: accepted, and the answer.
    pub page_result: crate::rendered_page::PageResult,
}

/// Everything a row reads about one call besides its status and result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolFacts {
    pub input: ToolInput,
    pub metadata: ToolMetadata,
    /// The failure's own message when the call failed.
    pub error: Option<String>,
    /// Every input field, one per line, for tools without a field of their
    /// own to print (`formatToolInput`'s fallback).
    pub listed_input: Option<String>,
}

impl ToolFacts {
    /// Takes what a later transition reports; what it leaves out stays.
    pub fn merge(&mut self, next: ToolFacts) {
        if next.input != ToolInput::default() {
            self.input = next.input;
        }
        if next.metadata != ToolMetadata::default() {
            self.metadata = next.metadata;
        }
        if next.error.is_some() {
            self.error = next.error;
        }
        if next.listed_input.is_some() {
            self.listed_input = next.listed_input;
        }
    }
}

/// What a row says after its name (`ToolDescription` in `tools.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDescription {
    /// A raw path the row shows relative to the session directory.
    Path(String),
    Text(String),
    Questions(usize),
    Files(usize),
    /// An `execute` script, described by the tools it called.
    Tools {
        calls: Vec<(String, usize)>,
        overflow: usize,
    },
}

const MAX_COMMAND_LENGTH: usize = 100;
const MAX_TEXT_LENGTH: usize = 120;
const MAX_DESCRIBED_TOOL_CALLS: usize = 4;

const OPENCHAMBER_TOOLS: [&str; 10] = [
    "openchamber",
    "openchamber_schedule",
    "openchamber_web",
    "openchamber_memory",
    "openchamber_knowledge",
    "openchamber_notify",
    "openchamber_render",
    "openchamber_image",
    "openchamber_secret",
    "openchamber_workflow",
];

pub fn is_shell(tool: &str) -> bool {
    normalize(tool) == "shell"
}

pub fn is_subagent(tool: &str) -> bool {
    normalize(tool) == "subagent"
}

pub fn is_execute(tool: &str) -> bool {
    normalize(tool) == "execute"
}

pub fn is_question(tool: &str) -> bool {
    normalize(tool) == "question"
}

pub fn is_file_change(tool: &str) -> bool {
    matches!(normalize(tool).as_ref(), "edit" | "write" | "patch")
}

/// Navigation tools get the compact one-line row and never show their result
/// (`isStaticTool`).
pub fn is_static(tool: &str) -> bool {
    matches!(normalize(tool).as_ref(), "read" | "skill")
}

/// Shell and subagent rows, and image generation, carry their duration.
pub fn shows_duration(tool: &str) -> bool {
    is_shell(tool) || is_subagent(tool) || tool == "openchamber_image"
}

/// Tools whose expanded body hides the input preview.
pub fn hides_input_preview(tool: &str) -> bool {
    OPENCHAMBER_TOOLS.contains(&tool)
        || matches!(normalize(tool).as_ref(), "patch" | "edit")
        || is_execute(tool)
}

/// The input as the expanded row prints it (`inputTextContent` in
/// `ToolExpandedContent`, `formatToolInput`); `None` when the row hides it.
pub fn input_text<'a>(tool: &str, facts: &'a ToolFacts) -> Option<Cow<'a, str>> {
    if hides_input_preview(tool) {
        return None;
    }
    let input = &facts.input;
    let specific = match normalize(tool).as_ref() {
        "shell" => input.command.as_deref(),
        "subagent" => input.prompt.as_deref().or(input.description.as_deref()),
        "write" => input
            .content
            .as_deref()
            .filter(|content| !content.is_empty()),
        _ => None,
    };
    let text = specific
        .or_else(|| input.content.as_deref().filter(|_| !is_shell(tool)))
        .map(Cow::Borrowed)
        .or_else(|| facts.listed_input.as_deref().map(Cow::Borrowed))?;
    (!text.trim().is_empty()).then_some(text)
}

/// The row's title (`getToolMetadata(...).displayName`). The reference keeps
/// these names in English in every locale.
pub fn display_name(tool: &str) -> Cow<'static, str> {
    let known = |name: &str| -> Option<&'static str> {
        Some(match name {
            "read" => "Read File",
            "write" => "Write File",
            "edit" => "Edit File",
            "patch" => "Apply Patch",
            "execute" => "Script",
            "shell" => "Shell Command",
            "grep" => "Search Files",
            "glob" => "Find Files",
            "subagent" => "Agent Task",
            "webfetch" => "Fetch URL",
            "websearch" => "Web Search",
            "codesearch" => "Code Search",
            "skill" => "Load Skill",
            "session_rename" => "Rename Session",
            "session_move" => "Move Session",
            "models" => "Search Models",
            "question" => "Question",
            "openchamber" => "OpenChamber",
            "openchamber_schedule" => "OpenChamber Schedule",
            "openchamber_web" => "OpenChamber Web",
            "openchamber_memory" => "OpenChamber Memory",
            "openchamber_knowledge" => "OpenChamber Knowledge",
            "openchamber_notify" => "OpenChamber Notify",
            "openchamber_render" => "OpenChamber Render",
            "openchamber_image" => "OpenChamber Image",
            "openchamber_secret" => "OpenChamber Secret",
            "plan_enter" => "Plan Mode",
            "plan_exit" => "Build Mode",
            "StructuredOutput" | "structuredoutput" => "Structured Output",
            _ => return None,
        })
    };
    if let Some(name) = known(tool).or_else(|| known(&normalize(tool))) {
        return Cow::Borrowed(name);
    }
    // `formatUnknownToolDisplayName`: separators become spaces, the first
    // character is upper case.
    let mut spaced = String::with_capacity(tool.len());
    let mut gap = false;
    for character in tool.trim().chars() {
        if matches!(character, '_' | '-') || character.is_whitespace() {
            gap = true;
            continue;
        }
        if gap && !spaced.is_empty() {
            spaced.push(' ');
        }
        gap = false;
        spaced.push(character);
    }
    let mut characters = spaced.chars();
    Cow::Owned(match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => spaced,
    })
}

/// The sprite icon of a tool row (`getToolIcon`).
pub fn icon(tool: &str) -> &'static str {
    let lowered: Cow<'_, str> = if tool.chars().any(char::is_uppercase) {
        Cow::Owned(tool.to_lowercase())
    } else {
        Cow::Borrowed(tool)
    };
    let tool = lowered.as_ref();
    let prefixed = |name: &str| {
        tool == name
            || tool
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('_'))
    };
    match tool {
        "reasoning" => "brain-ai-3",
        "image-preview" => "file-image",
        "mermaid-preview" | "plan_enter" => "file-list-2",
        "edit" | "patch" | "str_replace" | "str_replace_based_edit_tool" => "pencil",
        "write" | "create" | "file_write" => "file-edit",
        "read" | "view" | "file_read" | "cat" => "file-text",
        "execute" => "braces",
        "shell" | "bash" | "cmd" | "terminal" | "shell_command" => "terminal-box",
        "ls" | "dir" | "list_files" => "folder-6",
        "search" | "grep" | "find" | "ripgrep" => "menu-search",
        "glob" => "file-search",
        "fetch" | "curl" | "wget" | "webfetch" | "web-search" | "websearch" | "search_web"
        | "codesearch" | "google" | "bing" | "duckduckgo" | "perplexity" | "openchamber_web" => {
            "global"
        }
        "structuredoutput" | "structured_output" => "list-check-2",
        "skill" => "book",
        "subagent" => "ai-agent",
        "openchamber" => "openchamber",
        "openchamber_schedule" => "calendar-schedule",
        "openchamber_notify" => "notification-3",
        "openchamber_render" => "bar-chart-box",
        "openchamber_image" => "image",
        "openchamber_secret" => "shield-keyhole",
        "openchamber_knowledge" => "sticky-note",
        "openchamber_memory" => "brain-4",
        "openchamber_workflow" => "flow-chart",
        "question" => "survey",
        "plan_exit" => "task",
        _ if prefixed("linear") => "linear",
        _ if prefixed("cloudflare") || prefixed("claudflare") => "cloudflare",
        _ if tool.starts_with("git") => "git-branch",
        _ => "tools",
    }
}

fn clip(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn text(value: Option<&String>) -> Option<ToolDescription> {
    value.map(|value| ToolDescription::Text(clip(value, MAX_TEXT_LENGTH)))
}

fn first_line(value: &str) -> &str {
    value.split('\n').next().unwrap_or_default()
}

/// The files a `patch` call names in its own text (`patchInputFiles`).
pub fn patch_input_files(input: &ToolInput) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for line in input.patch_text.as_deref().unwrap_or_default().split('\n') {
        let line = line.trim();
        let file = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .map(str::trim);
        if let Some(file) = file
            && !file.is_empty()
            && !files.iter().any(|known| known == file)
        {
            files.push(file.to_owned());
        }
    }
    files
}

/// What a row says after its name (`toolDescription`).
pub fn description(tool: &str, facts: &ToolFacts) -> Option<ToolDescription> {
    let input = &facts.input;
    let metadata = &facts.metadata;
    let name = normalize(tool);
    match name.as_ref() {
        "shell" => input
            .command
            .as_deref()
            .map(|command| ToolDescription::Text(clip(first_line(command), MAX_COMMAND_LENGTH))),
        "execute" => describe_execute(&metadata.tool_calls, input.code.as_deref()),
        "subagent" => text(input.description.as_ref()).or_else(|| text(input.agent.as_ref())),
        "patch" => {
            let reported: Vec<String> = metadata
                .files
                .iter()
                .map(|file| file.file.clone())
                .collect();
            let files = if reported.is_empty() {
                patch_input_files(input)
            } else {
                reported
            };
            if files.len() > 1 {
                return Some(ToolDescription::Files(files.len()));
            }
            files.into_iter().next().map(ToolDescription::Path)
        }
        "edit" | "write" | "read" => input
            .path
            .clone()
            .or_else(|| metadata.files.first().map(|file| file.file.clone()))
            .map(ToolDescription::Path),
        "grep" | "glob" => text(input.pattern.as_ref()),
        "webfetch" => text(input.url.as_ref()),
        "websearch" => text(input.query.as_ref()),
        "skill" => text(metadata.name.as_ref())
            .or_else(|| text(input.name.as_ref()))
            .or_else(|| text(input.id.as_ref())),
        "session_rename" => text(input.title.as_ref()),
        "session_move" => input.directory.clone().map(ToolDescription::Path),
        "models" => text(input.search.as_ref()).or_else(|| text(input.provider.as_ref())),
        "question" => (input.questions > 0).then_some(ToolDescription::Questions(input.questions)),
        name if OPENCHAMBER_TOOLS.contains(&name) => {
            let label: Vec<&str> = [input.action.as_deref(), input.parameter_detail.as_deref()]
                .into_iter()
                .flatten()
                .collect();
            (!label.is_empty())
                .then(|| ToolDescription::Text(clip(&label.join(" · "), MAX_COMMAND_LENGTH)))
        }
        _ => text(input.description.as_ref())
            .or_else(|| input.path.clone().map(ToolDescription::Path)),
    }
}

fn describe_execute(calls: &[ExecuteToolCall], code: Option<&str>) -> Option<ToolDescription> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for call in calls {
        match counts.iter_mut().find(|(name, _)| *name == call.tool) {
            Some((_, count)) => *count += 1,
            None => counts.push((call.tool.clone(), 1)),
        }
    }
    if counts.is_empty() {
        return code.map(|code| ToolDescription::Text(clip(first_line(code), MAX_COMMAND_LENGTH)));
    }
    let overflow = counts.len().saturating_sub(MAX_DESCRIBED_TOOL_CALLS);
    counts.truncate(MAX_DESCRIBED_TOOL_CALLS);
    Some(ToolDescription::Tools {
        calls: counts,
        overflow,
    })
}

/// The input's own `description`, which a row shows in place of its
/// description for tools other than shell and file changes with a path.
pub fn justification<'a>(tool: &str, facts: &'a ToolFacts) -> Option<&'a str> {
    let name = normalize(tool);
    if matches!(name.as_ref(), "shell" | "patch") {
        return None;
    }
    if matches!(name.as_ref(), "edit" | "write")
        && matches!(description(tool, facts), Some(ToolDescription::Path(_)))
    {
        return None;
    }
    facts.input.description.as_deref()
}

/// Lines a file-changing call added and removed (`parseDiffStats`); `None`
/// when any file's counts are unknown.
pub fn diff_stats(tool: &str, metadata: &ToolMetadata) -> Option<(u64, u64)> {
    if !matches!(normalize(tool).as_ref(), "edit" | "patch") {
        return None;
    }
    if !metadata.files.is_empty() {
        let mut added = 0;
        let mut removed = 0;
        for file in &metadata.files {
            added += file.additions?;
            removed += file.deletions?;
        }
        return Some((added, removed));
    }
    let patch = metadata.patch.as_deref()?;
    let mut added = 0;
    let mut removed = 0;
    for line in patch.split('\n') {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        }
        if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added > 0 || removed > 0).then_some((added, removed))
}

/// The lines a `write` call writes (`parseWriteLineCount`).
pub fn write_line_count(tool: &str, input: &ToolInput) -> Option<usize> {
    if normalize(tool) != "write" {
        return None;
    }
    let content = input
        .content
        .as_deref()
        .filter(|content| !content.is_empty())?;
    Some(content.bytes().filter(|byte| *byte == b'\n').count() + 1)
}

/// A path relative to the session directory (`getRelativePath`).
pub fn relative_path<'a>(path: &'a str, directory: &str) -> Cow<'a, str> {
    let path = path.trim().trim_end_matches('/');
    let directory = directory.trim().trim_end_matches('/');
    if directory.is_empty() {
        return Cow::Borrowed(path);
    }
    if path == directory {
        return Cow::Borrowed(".");
    }
    match path
        .strip_prefix(directory)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        Some(rest) => Cow::Borrowed(rest),
        None => Cow::Borrowed(path),
    }
}

/// A run's duration as the row prints it: tenths once finished, whole
/// seconds while it runs (`formatDuration`).
pub fn format_duration(start: i64, end: Option<i64>, now: i64) -> String {
    match end {
        None => format!("{}s", (now - start).max(0) / 1000),
        Some(end) => {
            let seconds = (end - start).max(0) as f64 / 1000.0;
            format!("{:.1}s", seconds.max(0.1))
        }
    }
}

// The notice a background call hands the model before its instruction not to
// poll; only the notice is worth showing (`backgroundShellPart.ts`).
const MODEL_INSTRUCTION_START: &str = "\n\nYou will be notified automatically";

/// A background shell call's own result without the instruction to the model.
pub fn without_model_instruction(output: &str) -> &str {
    output
        .find(MODEL_INSTRUCTION_START)
        .map_or(output, |index| &output[..index])
}

/// Reasoning text without quote markers and invisible lines
/// (`cleanReasoningText`).
pub fn clean_reasoning(text: &str) -> String {
    if is_invisible(text) {
        return String::new();
    }
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| {
            line.strip_prefix("> ")
                .or_else(|| line.strip_prefix('>'))
                .unwrap_or(line)
                .trim_end()
        })
        .filter(|line| !is_invisible(line))
        .collect();
    lines.join("\n").trim().to_owned()
}

/// Whether text holds nothing a reader sees (`isInvisibleText`): only
/// whitespace and zero-width characters.
pub fn is_invisible(text: &str) -> bool {
    text.chars().all(|character| {
        character.is_whitespace()
            || matches!(
                character,
                '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
            )
    })
}

const SUMMARY_MAX_CHARS: usize = 80;

/// The one-line preview a folded reasoning block shows
/// (`getReasoningSummary`): Markdown stripped, whitespace collapsed, cut at a
/// word boundary before 80 characters.
pub fn reasoning_summary(text: &str) -> String {
    let stripped = strip_markdown(text);
    let flat: Vec<&str> = stripped.split_whitespace().collect();
    let flat = flat.join(" ");
    if flat.chars().count() <= SUMMARY_MAX_CHARS {
        return flat;
    }
    let limit = flat
        .char_indices()
        .nth(SUMMARY_MAX_CHARS)
        .map_or(flat.len(), |(index, _)| index);
    // `lastIndexOf(' ', 80)` also accepts a space at the limit itself.
    let search_end = (limit + 1).min(flat.len());
    let end = match flat[..search_end].rfind(' ') {
        Some(space) if space > 0 => space,
        _ => limit,
    };
    format!("{}…", flat[..end].trim_end())
}

fn strip_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text
        .replace("<!-- -->", "")
        .replace("<!---->", "")
        .split('\n')
    {
        let mut line = line;
        // Headings, blockquote markers, fences and rules.
        let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
        if (1..=6).contains(&hashes) && line[hashes..].starts_with(char::is_whitespace) {
            line = line[hashes..].trim_start();
        }
        line = line
            .strip_prefix("> ")
            .or_else(|| line.strip_prefix('>'))
            .unwrap_or(line);
        let trimmed = line.trim();
        if trimmed.starts_with("```")
            || (trimmed.len() >= 3
                && trimmed.chars().all(|c| matches!(c, '-' | '*' | '_'))
                && trimmed
                    .chars()
                    .all(|c| c == trimmed.chars().next().unwrap_or('-')))
        {
            out.push('\n');
            continue;
        }
        out.push_str(&strip_inline(line));
        out.push('\n');
    }
    out.trim().to_owned()
}

/// Inline code, emphasis markers and link targets.
fn strip_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '`' | '*' | '_' => {}
            '[' => {
                let mut label = String::new();
                let mut closed = false;
                for next in characters.by_ref() {
                    if next == ']' {
                        closed = true;
                        break;
                    }
                    label.push(next);
                }
                if closed && characters.peek() == Some(&'(') {
                    for next in characters.by_ref() {
                        if next == ')' {
                            break;
                        }
                    }
                    out.push_str(&label);
                } else {
                    out.push('[');
                    out.push_str(&label);
                    if closed {
                        out.push(']');
                    }
                }
            }
            _ => out.push(character),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(input: ToolInput) -> ToolFacts {
        ToolFacts {
            input,
            ..ToolFacts::default()
        }
    }

    #[test]
    fn names_known_tools_and_spaces_unknown_ones() {
        assert_eq!(display_name("shell"), "Shell Command");
        assert_eq!(display_name("opencode.models"), "Search Models");
        assert_eq!(display_name("my_custom-tool"), "My custom tool");
        assert_eq!(display_name("jira_search"), "Jira search");
    }

    #[test]
    fn picks_icons_like_the_reference() {
        assert_eq!(icon("shell"), "terminal-box");
        assert_eq!(icon("Glob"), "file-search");
        assert_eq!(icon("linear_issue"), "linear");
        assert_eq!(icon("git_status"), "git-branch");
        assert_eq!(icon("unknown"), "tools");
    }

    #[test]
    fn describes_each_builtin_from_its_input() {
        let shell = facts(ToolInput {
            command: Some("cargo test\n--all".into()),
            ..ToolInput::default()
        });
        assert_eq!(
            description("shell", &shell),
            Some(ToolDescription::Text("cargo test".into()))
        );
        let read = facts(ToolInput {
            path: Some("/repo/src/main.rs".into()),
            ..ToolInput::default()
        });
        assert_eq!(
            description("read", &read),
            Some(ToolDescription::Path("/repo/src/main.rs".into()))
        );
        let question = facts(ToolInput {
            questions: 2,
            ..ToolInput::default()
        });
        assert_eq!(
            description("question", &question),
            Some(ToolDescription::Questions(2))
        );
        assert_eq!(description("glob", &ToolFacts::default()), None);
    }

    #[test]
    fn a_patch_names_its_files_before_and_after_it_reports_them() {
        let mut patch = facts(ToolInput {
            patch_text: Some(
                "*** Begin Patch\n*** Update File: a.rs\n*** Add File: b.rs\n*** Update File: a.rs"
                    .into(),
            ),
            ..ToolInput::default()
        });
        assert_eq!(patch_input_files(&patch.input), ["a.rs", "b.rs"]);
        assert_eq!(
            description("patch", &patch),
            Some(ToolDescription::Files(2))
        );
        patch.metadata.files = vec![ToolFileDiff {
            file: "c.rs".into(),
            ..ToolFileDiff::default()
        }];
        assert_eq!(
            description("patch", &patch),
            Some(ToolDescription::Path("c.rs".into()))
        );
    }

    #[test]
    fn an_execute_row_counts_the_tools_its_script_called() {
        let mut execute = facts(ToolInput {
            code: Some("await tools.a()\nmore".into()),
            ..ToolInput::default()
        });
        assert_eq!(
            description("execute", &execute),
            Some(ToolDescription::Text("await tools.a()".into()))
        );
        execute.metadata.tool_calls = ["a", "b", "a", "c", "d", "e"]
            .iter()
            .map(|tool| ExecuteToolCall {
                tool: (*tool).into(),
                ..ExecuteToolCall::default()
            })
            .collect();
        assert_eq!(
            description("execute", &execute),
            Some(ToolDescription::Tools {
                calls: vec![
                    ("a".into(), 2),
                    ("b".into(), 1),
                    ("c".into(), 1),
                    ("d".into(), 1)
                ],
                overflow: 1,
            })
        );
    }

    #[test]
    fn diff_stats_never_show_a_partial_total() {
        let mut metadata = ToolMetadata {
            files: vec![
                ToolFileDiff {
                    file: "a".into(),
                    additions: Some(3),
                    deletions: Some(1),
                    ..ToolFileDiff::default()
                },
                ToolFileDiff {
                    file: "b".into(),
                    additions: Some(2),
                    deletions: Some(0),
                    ..ToolFileDiff::default()
                },
            ],
            ..ToolMetadata::default()
        };
        assert_eq!(diff_stats("edit", &metadata), Some((5, 1)));
        metadata.files[1].deletions = None;
        assert_eq!(diff_stats("edit", &metadata), None);
        let patch = ToolMetadata {
            patch: Some("--- a\n+++ b\n+x\n-y\n+z".into()),
            ..ToolMetadata::default()
        };
        assert_eq!(diff_stats("patch", &patch), Some((2, 1)));
        assert_eq!(diff_stats("shell", &patch), None);
    }

    #[test]
    fn counts_written_lines() {
        let input = ToolInput {
            content: Some("a\nb\nc".into()),
            ..ToolInput::default()
        };
        assert_eq!(write_line_count("write", &input), Some(3));
        assert_eq!(write_line_count("edit", &input), None);
    }

    #[test]
    fn justification_skips_shell_and_path_rows() {
        let described = facts(ToolInput {
            description: Some("Run the tests".into()),
            command: Some("cargo test".into()),
            ..ToolInput::default()
        });
        assert_eq!(justification("shell", &described), None);
        assert_eq!(
            justification("jira_search", &described),
            Some("Run the tests")
        );
    }

    #[test]
    fn paths_show_relative_to_the_session_directory() {
        assert_eq!(relative_path("/repo/src/a.rs", "/repo"), "src/a.rs");
        assert_eq!(relative_path("/repo", "/repo/"), ".");
        assert_eq!(relative_path("/other/a.rs", "/repo"), "/other/a.rs");
        assert_eq!(relative_path("/repository/a", "/repo"), "/repository/a");
    }

    #[test]
    fn durations_show_tenths_once_finished() {
        assert_eq!(format_duration(0, None, 2_900), "2s");
        assert_eq!(format_duration(0, Some(1_250), 0), "1.2s");
        assert_eq!(format_duration(0, Some(10), 0), "0.1s");
    }

    #[test]
    fn background_notices_drop_the_instruction_to_the_model() {
        assert_eq!(
            without_model_instruction(
                "Started in the background.\n\nYou will be notified automatically when"
            ),
            "Started in the background."
        );
    }

    #[test]
    fn reasoning_summaries_strip_markdown_and_cut_at_a_word() {
        assert_eq!(
            reasoning_summary("## Plan\n**Read** the `config` and [docs](http://x)."),
            "Plan Read the config and docs."
        );
        let long = "word ".repeat(30);
        let summary = reasoning_summary(&long);
        assert!(summary.ends_with('…'));
        assert!(summary.chars().count() <= SUMMARY_MAX_CHARS + 1);
        assert_eq!(
            clean_reasoning("> first\n>second\n\u{200B}\n"),
            "first\nsecond"
        );
        assert!(is_invisible(" \u{200B}\n"));
    }

    #[test]
    fn prints_the_input_each_tool_shows() {
        let mut shell = facts(ToolInput {
            command: Some("ls -la".into()),
            content: Some("ignored".into()),
            ..ToolInput::default()
        });
        shell.listed_input = Some("Command: ls -la".into());
        assert_eq!(input_text("shell", &shell).as_deref(), Some("ls -la"));
        assert_eq!(
            input_text("edit", &shell),
            None,
            "edit rows hide their input"
        );
        let subagent = facts(ToolInput {
            prompt: Some("Find the bug".into()),
            description: Some("Bug hunt".into()),
            ..ToolInput::default()
        });
        assert_eq!(
            input_text("subagent", &subagent).as_deref(),
            Some("Find the bug")
        );
        let listed = ToolFacts {
            listed_input: Some("Query: x".into()),
            ..ToolFacts::default()
        };
        assert_eq!(
            input_text("jira_search", &listed).as_deref(),
            Some("Query: x")
        );
        assert_eq!(input_text("glob", &ToolFacts::default()), None);
    }

    #[test]
    fn later_facts_keep_what_they_leave_out() {
        let mut held = facts(ToolInput {
            command: Some("ls".into()),
            ..ToolInput::default()
        });
        held.merge(ToolFacts {
            error: Some("exit 1".into()),
            ..ToolFacts::default()
        });
        assert_eq!(held.input.command.as_deref(), Some("ls"));
        assert_eq!(held.error.as_deref(), Some("exit 1"));
    }
}
