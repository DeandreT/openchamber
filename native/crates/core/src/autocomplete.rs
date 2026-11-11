//! The composer's autocomplete: which picker the caret asks for
//! (`composer/language/triggers.ts`), what each picker lists and in which
//! order (`commandAutocompleteItems.ts`, `skillAutocompleteItems.ts`,
//! `SnippetAutocomplete.tsx`, `fileMentionResults.ts`,
//! `sessionMentionResults.ts`, `FileMentionAutocomplete.tsx`), and the edit a
//! choice makes (`ChatInput.tsx`'s select handlers).
//!
//! Offsets are UTF-8 byte offsets into the composer's text.

use std::cmp::Ordering;
use std::ops::Range;

use crate::search_rank::{self, SubsequenceMatch};
use crate::send_config::{AgentEntry, AgentMode};
use crate::session::SessionRecord;

/// The pickers, in the precedence the trigger rules give them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AutocompleteKind {
    Command,
    Skill,
    Snippet,
    Mention,
}

/// The picker a caret position opens and the text typed after its sigil.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    pub kind: AutocompleteKind,
    pub query: String,
}

/// Where the edit that moved the caret came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputSource {
    #[default]
    Typed,
    /// A paste, with the text it inserted.
    Paste,
}

/// What the trigger rules need beyond the text and caret.
pub struct TriggerContext<'a> {
    /// The `/btw` panel takes no mentions.
    pub mentions_enabled: bool,
    pub source: InputSource,
    /// The text the change inserted, when known.
    pub inserted: Option<&'a str>,
    /// Keeps an `@` query open past a space while it still names a session.
    pub continues_past_space: &'a dyn Fn(&str) -> bool,
    /// A picker selection completed the reference containing this `@` offset.
    pub is_completed: &'a dyn Fn(usize) -> bool,
}

impl std::fmt::Debug for TriggerContext<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TriggerContext")
            .field("mentions_enabled", &self.mentions_enabled)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Every spelling a `$` skill sigil can take: a CJK IME types `¥`, `￥` or `＄`.
const SKILL_SIGILS: [char; 4] = ['$', '¥', '￥', '＄'];

/// JavaScript's `\s`.
fn js_whitespace(character: char) -> bool {
    character.is_whitespace() || character == '\u{feff}'
}

fn boundary_before(text: &str, index: usize) -> bool {
    text[..index].chars().next_back().is_none_or(js_whitespace)
}

/// `resolveAutocompleteTrigger`: exactly one picker, the command palette
/// first, then skills, snippets and mentions.
pub fn resolve_trigger(text: &str, cursor: usize, context: &TriggerContext<'_>) -> Option<Trigger> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    command_palette(text, cursor)
        .or_else(|| inline_token(text, cursor, &SKILL_SIGILS, AutocompleteKind::Skill))
        .or_else(|| inline_token(text, cursor, &['#'], AutocompleteKind::Snippet))
        .or_else(|| mention(text, cursor, context))
}

/// A `/` in the first column, the caret inside the command word and no
/// argument typed yet.
fn command_palette(text: &str, cursor: usize) -> Option<Trigger> {
    if !text.starts_with('/') || text.contains(' ') {
        return None;
    }
    let end = text.find('\n').unwrap_or(text.len());
    if cursor > end {
        return None;
    }
    Some(Trigger {
        kind: AutocompleteKind::Command,
        query: text[1..end].to_owned(),
    })
}

/// The last sigil before the caret, in any of its spellings.
fn last_sigil(text: &str, sigils: &[char]) -> Option<(usize, usize)> {
    text.char_indices()
        .rev()
        .find(|(_, character)| sigils.contains(character))
        .map(|(index, character)| (index, character.len_utf8()))
}

fn inline_token(
    text: &str,
    cursor: usize,
    sigils: &[char],
    kind: AutocompleteKind,
) -> Option<Trigger> {
    let before = &text[..cursor];
    let (index, length) = last_sigil(before, sigils)?;
    if !boundary_before(before, index) {
        return None;
    }
    let query = &before[index + length..];
    if query.contains([' ', '\n']) {
        return None;
    }
    Some(Trigger {
        kind,
        query: query.to_owned(),
    })
}

/// `getFileMentionAutocompleteQuery`.
fn mention(text: &str, cursor: usize, context: &TriggerContext<'_>) -> Option<Trigger> {
    if !context.mentions_enabled
        || context.source == InputSource::Paste
            && context.inserted.is_some_and(|text| text.contains('@'))
    {
        return None;
    }
    let before = &text[..cursor];
    let at = before.rfind('@')?;
    if (context.is_completed)(at) || !boundary_before(before, at) {
        return None;
    }
    let query = &before[at + 1..];
    if query.contains('\n') {
        return None;
    }
    // A bare `@ ` is punctuation, not the start of a title.
    if query.contains(' ') && (query.starts_with(' ') || !(context.continues_past_space)(query)) {
        return None;
    }
    Some(Trigger {
        kind: AutocompleteKind::Mention,
        query: query.to_owned(),
    })
}

/// One wrapping step through `total` rows (`ArrowDown`/`ArrowUp`).
pub fn step(selected: usize, total: usize, down: bool) -> usize {
    if total == 0 {
        return 0;
    }
    if down {
        (selected + 1) % total
    } else {
        (selected + total - 1) % total
    }
}

/// An edit a choice makes: replace `range` and put the caret at `caret`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Insertion {
    pub range: Range<usize>,
    pub text: String,
    pub caret: usize,
}

impl Insertion {
    fn replace(range: Range<usize>, text: String) -> Self {
        let caret = range.start + text.len();
        Self { range, text, caret }
    }
}

/// `completeMentionSelection` and `handleSessionSelect`: `@name ` (or a
/// session's link) over the `@` query, or at the caret without one.
pub fn mention_insertion(text: &str, cursor: usize, reference: &str) -> Insertion {
    let start = text[..cursor].rfind('@').unwrap_or(cursor);
    Insertion::replace(start..cursor, format!("{reference} "))
}

/// `handleSkillSelect`: `$name ` over the sigil and query; None without a sigil.
pub fn skill_insertion(text: &str, cursor: usize, name: &str) -> Option<Insertion> {
    let (index, _) = last_sigil(&text[..cursor], &SKILL_SIGILS)?;
    Some(Insertion::replace(index..cursor, format!("${name} ")))
}

/// `handleSnippetSelect`: `#trigger ` over the last `#`, or at the caret.
pub fn snippet_insertion(text: &str, cursor: usize, trigger: &str) -> Insertion {
    let start = text[..cursor].rfind('#').unwrap_or(cursor);
    Insertion::replace(start..cursor, format!("#{trigger} "))
}

/// `handleCommandSelect`: the whole text becomes `/name `.
pub fn command_insertion(text: &str, name: &str) -> Insertion {
    Insertion::replace(0..text.len(), format!("/{name} "))
}

/// Where a command comes from; its row shows the matching badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandScope {
    Project,
    User,
}

/// A row of the command palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandItem {
    pub name: String,
    pub description: Option<String>,
    pub search_aliases: Vec<String>,
    /// OpenCode's `init` and `review`, and the composer's own commands.
    pub built_in: bool,
    /// OpenChamber's own commands carry the OpenChamber badge.
    pub open_chamber: bool,
    pub scope: Option<CommandScope>,
}

/// A row's badge (`AutocompleteRowDetail`'s scope).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowScope {
    User,
    Project,
    OpenCode,
    OpenChamber,
}

impl CommandItem {
    pub fn row_scope(&self) -> Option<RowScope> {
        if self.open_chamber {
            Some(RowScope::OpenChamber)
        } else if self.built_in {
            Some(RowScope::OpenCode)
        } else {
            match self.scope {
                Some(CommandScope::Project) => Some(RowScope::Project),
                Some(CommandScope::User) => Some(RowScope::User),
                None => None,
            }
        }
    }
}

/// Which of the composer's own commands apply (`CommandAutocomplete`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandAvailability {
    pub has_session: bool,
    /// A session, or a new-session draft.
    pub can_start_session: bool,
    /// Desktop with a session: the review handoff runs here.
    pub review_handoff: bool,
}

/// The composer's own commands, in the palette's order, with the locale key
/// of each description: built-ins OpenCode answers, then OpenChamber's.
pub fn builtin_commands(
    availability: CommandAvailability,
) -> Vec<(&'static str, &'static str, bool)> {
    let CommandAvailability {
        has_session,
        can_start_session,
        review_handoff,
    } = availability;
    let mut commands = Vec::new();
    let mut push = |condition: bool, name, key, open_chamber| {
        if condition {
            commands.push((name, key, open_chamber));
        }
    };
    push(
        has_session,
        "init",
        "chat.commandAutocomplete.command.initDescription",
        false,
    );
    push(
        has_session,
        "undo",
        "chat.commandAutocomplete.command.undoDescription",
        false,
    );
    push(
        has_session,
        "redo",
        "chat.commandAutocomplete.command.redoDescription",
        false,
    );
    push(
        has_session,
        "timeline",
        "chat.commandAutocomplete.command.timelineDescription",
        false,
    );
    push(
        has_session,
        "compact",
        "chat.commandAutocomplete.command.compactDescription",
        false,
    );
    push(
        has_session,
        "btw",
        "chat.commandAutocomplete.command.btwDescription",
        true,
    );
    push(
        has_session,
        "fork",
        "chat.commandAutocomplete.command.forkDescription",
        true,
    );
    push(
        has_session,
        "summary",
        "chat.commandAutocomplete.command.summaryDescription",
        true,
    );
    push(
        can_start_session,
        "workspace-review",
        "chat.commandAutocomplete.command.workspaceReviewDescription",
        true,
    );
    push(
        review_handoff,
        "handoff-review",
        "chat.commandAutocomplete.command.handoffReviewDescription",
        true,
    );
    push(
        can_start_session,
        "plan-feature",
        "chat.commandAutocomplete.command.featurePlanDescription",
        true,
    );
    push(
        can_start_session,
        "craft-goal",
        "chat.commandAutocomplete.command.craftGoalDescription",
        true,
    );
    push(
        can_start_session,
        "schedule-task",
        "chat.commandAutocomplete.command.scheduleTaskDescription",
        true,
    );
    push(
        can_start_session,
        "catch-up",
        "chat.commandAutocomplete.command.catchUpDescription",
        true,
    );
    push(
        can_start_session,
        "debug",
        "chat.commandAutocomplete.command.debugDescription",
        true,
    );
    push(
        can_start_session,
        "weigh",
        "chat.commandAutocomplete.command.weighDescription",
        true,
    );
    push(
        can_start_session,
        "explore",
        "chat.commandAutocomplete.command.exploreDescription",
        true,
    );
    commands
}

/// A command the server lists, with what its config read found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCommand {
    pub name: String,
    pub description: Option<String>,
    pub scope: Option<CommandScope>,
    /// No command file or config entry: OpenCode or a plugin defines it.
    pub from_opencode: bool,
}

/// `isCommandOfferedInComposer`: OpenCode's own `/goal` duplicates Goal Mode.
pub fn offered_in_composer(command: &ServerCommand) -> bool {
    !(command.from_opencode && command.scope.is_none() && command.name == "goal")
}

fn add_search_aliases(mut winner: CommandItem, duplicate: &CommandItem) -> CommandItem {
    let mut aliases = winner.search_aliases.clone();
    let candidates = (winner.name != duplicate.name)
        .then(|| duplicate.name.clone())
        .into_iter()
        .chain(duplicate.description.clone())
        .chain(duplicate.search_aliases.iter().cloned());
    for alias in candidates {
        if Some(&alias) != winner.description.as_ref() && !aliases.contains(&alias) {
            aliases.push(alias);
        }
    }
    winner.search_aliases = aliases;
    winner
}

/// `mergeCommandAutocompleteItems`: a built-in wins over a server command of
/// the same case-sensitive name, which becomes its search alias.
pub fn merge_commands(built_ins: Vec<CommandItem>, commands: Vec<CommandItem>) -> Vec<CommandItem> {
    let mut merged: Vec<(CommandItem, u8)> = Vec::new();
    let precedence =
        |item: &CommandItem, from_built_ins: bool| u8::from(from_built_ins || item.built_in);
    let sources = built_ins
        .into_iter()
        .map(|item| (item, true))
        .chain(commands.into_iter().map(|item| (item, false)));
    for (item, from_built_ins) in sources {
        let rank = precedence(&item, from_built_ins);
        match merged
            .iter_mut()
            .find(|(existing, _)| existing.name == item.name)
        {
            None => merged.push((item, rank)),
            Some((existing, existing_rank)) => {
                *existing = if rank > *existing_rank {
                    add_search_aliases(item, existing)
                } else {
                    add_search_aliases(existing.clone(), &item)
                };
                *existing_rank = (*existing_rank).max(rank);
            }
        }
    }
    merged.into_iter().map(|(item, _)| item).collect()
}

/// `String.prototype.localeCompare` for the names pickers sort: case folds
/// first, then the original spelling breaks the tie.
fn locale_compare(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| b.cmp(a))
}

fn ranked_then_rest<T: Clone>(items: &[T], ranked: Vec<usize>) -> Vec<T> {
    let mut seen = vec![false; items.len()];
    let mut out = Vec::with_capacity(items.len());
    for index in ranked {
        seen[index] = true;
        out.push(items[index].clone());
    }
    out.extend(
        items
            .iter()
            .zip(seen)
            .filter(|(_, seen)| !seen)
            .map(|(item, _)| item.clone()),
    );
    out
}

/// `rankCommandAutocompleteItems`: fuzzy matches on the name, description or
/// aliases, sorted by name, the name matches ranked first.
pub fn rank_commands(commands: &[CommandItem], query: &str) -> Vec<CommandItem> {
    let mut filtered: Vec<CommandItem> = commands
        .iter()
        .filter(|command| {
            query.is_empty()
                || search_rank::matches_fuzzy_query(&command.name, query)
                || command
                    .description
                    .as_deref()
                    .is_some_and(|description| search_rank::matches_fuzzy_query(description, query))
                || command
                    .search_aliases
                    .iter()
                    .any(|alias| search_rank::matches_fuzzy_query(alias, query))
        })
        .cloned()
        .collect();
    filtered.sort_by(|a, b| locale_compare(&a.name, &b.name));
    if query.is_empty() {
        return filtered;
    }
    let ranked =
        search_rank::rank_by_query(&filtered, query, |command| vec![Some(&command.name)], true);
    ranked_then_rest(&filtered, ranked)
}

/// A skill the `$` picker offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillItem {
    pub name: String,
    pub project: bool,
    pub description: Option<String>,
}

/// `rankSkillAutocompleteItems`: project skills first; in each group name
/// matches rank like the slash menu, then fuzzy-only hits.
pub fn rank_skills(skills: &[SkillItem], query: &str) -> Vec<SkillItem> {
    let query = query.trim();
    let mut matches: Vec<SkillItem> = skills
        .iter()
        .filter(|skill| query.is_empty() || search_rank::matches_fuzzy_query(&skill.name, query))
        .cloned()
        .collect();
    matches.sort_by(|a, b| locale_compare(&a.name, &b.name));
    let rank_group = |group: Vec<SkillItem>| {
        if query.is_empty() {
            return group;
        }
        let ranked =
            search_rank::rank_by_query(&group, query, |skill| vec![Some(&skill.name)], true);
        ranked_then_rest(&group, ranked)
    };
    let (project, rest): (Vec<_>, Vec<_>) = matches.into_iter().partition(|skill| skill.project);
    let mut ordered = rank_group(project);
    ordered.extend(rank_group(rest));
    ordered
}

/// A snippet the `#` picker offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnippetItem {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
    pub content: String,
    pub project: bool,
}

impl SnippetItem {
    /// `snippetPreview`: the description or content on one line, 120 characters.
    pub fn preview(&self) -> String {
        let source = self
            .description
            .as_deref()
            .filter(|description| !description.is_empty())
            .unwrap_or(&self.content);
        source
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(120)
            .collect()
    }

    /// The token a choice writes: the alias typed, else the name.
    pub fn trigger(&self, query: &str) -> String {
        if self.aliases.iter().any(|alias| alias == query) {
            query.to_owned()
        } else {
            self.name.clone()
        }
    }
}

/// The `#` picker's list: name or alias fuzzy matches, project snippets
/// first, then by name.
pub fn filter_snippets(snippets: &[SnippetItem], query: &str) -> Vec<SnippetItem> {
    let query = query.trim();
    let mut matches: Vec<SnippetItem> = snippets
        .iter()
        .filter(|snippet| {
            query.is_empty()
                || search_rank::matches_fuzzy_query(&snippet.name, query)
                || snippet
                    .aliases
                    .iter()
                    .any(|alias| search_rank::matches_fuzzy_query(alias, query))
        })
        .cloned()
        .collect();
    matches.sort_by(|a, b| {
        b.project
            .cmp(&a.project)
            .then_with(|| locale_compare(&a.name, &b.name))
    });
    matches
}

/// Subagents listed before the user types; the rest are a keystroke away.
pub const AGENTS_SHOWN_WITHOUT_QUERY: usize = 6;
/// Sessions `@` lists at most.
pub const SESSION_MENTION_LIMIT: usize = 5;
/// File and folder rows `@` lists at most.
pub const FILE_RESULT_LIMIT: usize = 20;
/// The server's file hits per search, and its folder hits.
pub const FILE_SEARCH_LIMIT: usize = 80;
pub const FOLDER_SEARCH_LIMIT: usize = 20;

/// `@`'s agents: visible subagents by name, ranked against the query; six
/// before anything is typed. A query holding a space lists sessions alone.
/// Returns the rows and whether more agents match than are shown.
pub fn rank_mention_agents(agents: &[AgentEntry], query: &str) -> (Vec<AgentEntry>, bool) {
    if query.contains(' ') {
        return (Vec::new(), false);
    }
    let mut subagents: Vec<AgentEntry> = agents
        .iter()
        .filter(|agent| {
            !agent.hidden && matches!(agent.mode, Some(AgentMode::Subagent | AgentMode::All))
        })
        .cloned()
        .collect();
    subagents.sort_by(|a, b| locale_compare(&a.name, &b.name));
    let ranked = search_rank::rank_by_query(
        &subagents,
        query,
        |agent| {
            vec![
                Some(&agent.name),
                agent.display_name.as_deref(),
                agent.description.as_deref(),
            ]
        },
        true,
    );
    let all: Vec<AgentEntry> = ranked
        .into_iter()
        .map(|index| subagents[index].clone())
        .collect();
    if query.trim().is_empty() && all.len() > AGENTS_SHOWN_WITHOUT_QUERY {
        (all[..AGENTS_SHOWN_WITHOUT_QUERY].to_vec(), true)
    } else {
        (all, false)
    }
}

/// `tokenizeMentionQuery`.
pub fn mention_tokens(query: &str) -> Vec<String> {
    let trimmed = query.trim();
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    trimmed
        .trim_start_matches('/')
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// `mentionServerQuery`: the server takes one term, the longest token; the
/// rest filter here. Empty when nothing searchable was typed.
pub fn mention_server_query(query: &str) -> String {
    mention_tokens(query)
        .into_iter()
        .fold(String::new(), |longest, token| {
            if token.chars().count() > longest.chars().count() {
                token
            } else {
                longest
            }
        })
}

/// Whether a mention query needs a server search: something typed, and no
/// space, which only a session title holds.
pub fn mention_searches(query: &str) -> bool {
    !mention_server_query(query).is_empty() && !query.contains(' ')
}

/// A file or folder `@` can reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRow {
    pub path: String,
    pub relative_path: String,
    pub directory: bool,
}

impl FileRow {
    /// What the mention writes: the project-relative path.
    pub fn mention_path(&self) -> &str {
        let relative = self.relative_path.trim();
        if relative.is_empty() {
            self.path.rsplit('/').next().unwrap_or(&self.path)
        } else {
            relative
        }
    }
}

/// `rankFileMentionResults`: folders and files merged, best match first by
/// letters in order against the relative path, every token required.
pub fn rank_file_results(files: &[FileRow], folders: &[FileRow], query: &str) -> Vec<FileRow> {
    let merged: Vec<FileRow> = folders.iter().chain(files).cloned().collect();
    let tokens = mention_tokens(query);
    if tokens.is_empty() {
        return merged.into_iter().take(FILE_RESULT_LIMIT).collect();
    }
    search_rank::rank_by_subsequence(
        &merged,
        &tokens.join(" "),
        |row| row.relative_path.as_str(),
        FILE_RESULT_LIMIT,
        true,
    )
    .into_iter()
    .map(|(index, _): (usize, SubsequenceMatch)| merged[index].clone())
    .collect()
}

fn inside(directory: Option<&str>, root: &str) -> bool {
    directory.is_some_and(|directory| {
        directory == root
            || directory
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The session being written in and subagent runs are never offered.
fn mentionable(session: &SessionRecord, current: Option<&str>) -> bool {
    session.parent_id.is_none()
        && Some(session.id.as_str()) != current
        && !session.title.trim().is_empty()
}

/// `rankSessionMentions`: sessions whose title holds the query, across every
/// project, the open project's first, best match then most recent first.
pub fn rank_session_mentions<'a>(
    sessions: &[&'a SessionRecord],
    query: &str,
    current: Option<&str>,
    project_root: Option<&str>,
) -> Vec<&'a SessionRecord> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let mut candidates: Vec<&SessionRecord> = sessions
        .iter()
        .copied()
        .filter(|session| mentionable(session, current))
        .collect();
    candidates.sort_by_key(|session| std::cmp::Reverse(session.time.updated));
    let ranked: Vec<&SessionRecord> = search_rank::rank_by_query(
        &candidates,
        query,
        |session| vec![Some(&session.title)],
        false,
    )
    .into_iter()
    .map(|index| candidates[index])
    .collect();
    let Some(root) = project_root else {
        return ranked.into_iter().take(SESSION_MENTION_LIMIT).collect();
    };
    let (in_project, elsewhere): (Vec<_>, Vec<_>) = ranked
        .into_iter()
        .partition(|session| inside(session.directory.as_deref(), root));
    in_project
        .into_iter()
        .chain(elsewhere)
        .take(SESSION_MENTION_LIMIT)
        .collect()
}

/// `hasSessionMention`: whether any session title holds the query.
pub fn has_session_mention(
    sessions: &[&SessionRecord],
    query: &str,
    current: Option<&str>,
) -> bool {
    !query.trim().is_empty()
        && sessions.iter().any(|session| {
            mentionable(session, current)
                && search_rank::matches_rank_query([Some(session.title.as_str())], query)
        })
}

/// What a mention list shows (`mentionListState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListState {
    Rows,
    Loading,
    Empty,
}

pub fn list_state(rows: usize, pending: bool) -> ListState {
    if rows > 0 {
        ListState::Rows
    } else if pending {
        ListState::Loading
    } else {
        ListState::Empty
    }
}

/// Where the selection lands when a mention list's rows change: a new
/// query's answer starts at its best match; new rows for the same query keep
/// the row the user was on.
pub fn carried_selection<T: PartialEq>(
    previous: &[T],
    previous_selected: usize,
    next: &[T],
    same_query: bool,
) -> usize {
    if !same_query {
        return 0;
    }
    previous
        .get(previous_selected)
        .and_then(|selected| next.iter().position(|row| row == selected))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::SessionId;
    use crate::session::{SessionKind, SessionTime};

    fn resolve(text: &str, cursor: usize) -> Option<Trigger> {
        resolve_trigger(
            text,
            cursor,
            &TriggerContext {
                mentions_enabled: true,
                source: InputSource::Typed,
                inserted: None,
                continues_past_space: &|query| query.starts_with("release"),
                is_completed: &|_| false,
            },
        )
    }

    fn trigger(kind: AutocompleteKind, query: &str) -> Option<Trigger> {
        Some(Trigger {
            kind,
            query: query.to_owned(),
        })
    }

    #[test]
    fn the_command_palette_takes_only_a_leading_slash_without_arguments() {
        assert_eq!(
            resolve("/rev", 4),
            trigger(AutocompleteKind::Command, "rev")
        );
        assert_eq!(resolve("/", 1), trigger(AutocompleteKind::Command, ""));
        assert_eq!(resolve("/review now", 7), None);
        assert_eq!(resolve("/rev\nmore", 9), None);
        assert_eq!(
            resolve("/rev\nmore", 3),
            trigger(AutocompleteKind::Command, "rev")
        );
        assert_eq!(resolve("a /rev", 6), None);
    }

    #[test]
    fn skills_and_snippets_open_at_a_word_boundary_until_a_space() {
        assert_eq!(
            resolve("use $rev", 8),
            trigger(AutocompleteKind::Skill, "rev")
        );
        assert_eq!(
            resolve("use ￥rev", "use ￥rev".len()),
            trigger(AutocompleteKind::Skill, "rev")
        );
        assert_eq!(resolve("a$b", 3), None);
        assert_eq!(resolve("$rev done", 9), None);
        assert_eq!(
            resolve("see #sig", 8),
            trigger(AutocompleteKind::Snippet, "sig")
        );
        assert_eq!(resolve("issue#1", 7), None);
    }

    #[test]
    fn mentions_follow_boundaries_spaces_pastes_and_completions() {
        assert_eq!(
            resolve("see @src", 8),
            trigger(AutocompleteKind::Mention, "src")
        );
        assert_eq!(resolve("a@b.com", 7), None);
        assert_eq!(resolve("@ x", 3), None);
        assert_eq!(
            resolve("@release no", 11),
            trigger(AutocompleteKind::Mention, "release no")
        );
        assert_eq!(resolve("@other no", 9), None);
        assert_eq!(resolve("@a\nb", 4), None);
        let pasted = resolve_trigger(
            "@src",
            4,
            &TriggerContext {
                mentions_enabled: true,
                source: InputSource::Paste,
                inserted: Some("@src"),
                continues_past_space: &|_| false,
                is_completed: &|_| false,
            },
        );
        assert_eq!(pasted, None);
        let completed = resolve_trigger(
            "@src/a.ts x",
            11,
            &TriggerContext {
                mentions_enabled: true,
                source: InputSource::Typed,
                inserted: None,
                continues_past_space: &|_| true,
                is_completed: &|start| start == 0,
            },
        );
        assert_eq!(completed, None);
    }

    #[test]
    fn choices_replace_the_query_and_put_the_caret_after_a_space() {
        let text = "see @sr tail";
        assert_eq!(
            mention_insertion(text, 7, "@src/a.ts"),
            Insertion {
                range: 4..7,
                text: "@src/a.ts ".to_owned(),
                caret: 14
            }
        );
        assert_eq!(
            skill_insertion("use $re", 7, "review")
                .expect("a sigil before the caret")
                .text,
            "$review "
        );
        assert_eq!(skill_insertion("use", 3, "review"), None);
        assert_eq!(snippet_insertion("x #si", 5, "sig").range, 2..5);
        assert_eq!(command_insertion("/rev", "review").text, "/review ");
        assert_eq!(command_insertion("/rev", "review").caret, 8);
    }

    fn command(name: &str, description: Option<&str>, built_in: bool) -> CommandItem {
        CommandItem {
            name: name.to_owned(),
            description: description.map(str::to_owned),
            search_aliases: Vec::new(),
            built_in,
            open_chamber: false,
            scope: None,
        }
    }

    #[test]
    fn built_ins_win_name_collisions_and_keep_the_other_as_alias() {
        let merged = merge_commands(
            vec![command("init", Some("Create AGENTS.md"), true)],
            vec![
                command("init", Some("Custom init"), false),
                command("deploy", None, false),
            ],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].description.as_deref(), Some("Create AGENTS.md"));
        assert_eq!(merged[0].search_aliases, ["Custom init"]);
        assert_eq!(merged[1].name, "deploy");
    }

    #[test]
    fn commands_sort_by_name_and_rank_name_matches_first() {
        let commands = vec![
            command("review", Some("Review changes"), true),
            command("compact", Some("Summarize the session"), true),
            command("deploy", Some("compact build"), false),
        ];
        let names =
            |items: Vec<CommandItem>| items.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert_eq!(
            names(rank_commands(&commands, "")),
            ["compact", "deploy", "review"]
        );
        assert_eq!(
            names(rank_commands(&commands, "comp")),
            ["compact", "deploy"]
        );
    }

    #[test]
    fn project_skills_lead_and_names_rank_within_groups() {
        let skill = |name: &str, project: bool| SkillItem {
            name: name.to_owned(),
            project,
            description: None,
        };
        let skills = [
            skill("zeta", true),
            skill("review-pr", false),
            skill("alpha", true),
            skill("pr", false),
        ];
        let names: Vec<String> = rank_skills(&skills, "")
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert_eq!(names, ["alpha", "zeta", "pr", "review-pr"]);
        let names: Vec<String> = rank_skills(&skills, "pr")
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert_eq!(names, ["pr", "review-pr"]);
    }

    #[test]
    fn snippets_list_project_first_and_choose_the_typed_alias() {
        let snippet = |name: &str, aliases: &[&str], project: bool| SnippetItem {
            name: name.to_owned(),
            aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
            description: None,
            content: "Body\n  text".to_owned(),
            project,
        };
        let snippets = [snippet("sig", &["s"], false), snippet("bug", &[], true)];
        let names: Vec<String> = filter_snippets(&snippets, "")
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(names, ["bug", "sig"]);
        assert_eq!(snippets[0].trigger("s"), "s");
        assert_eq!(snippets[0].trigger("si"), "sig");
        assert_eq!(snippets[0].preview(), "Body text");
    }

    fn agent(name: &str, mode: AgentMode, hidden: bool) -> AgentEntry {
        AgentEntry {
            name: name.to_owned(),
            display_name: None,
            mode: Some(mode),
            hidden,
            model: None,
            color: None,
            description: None,
        }
    }

    #[test]
    fn mention_agents_are_visible_subagents_six_before_a_query() {
        let mut agents: Vec<AgentEntry> = (0..8)
            .map(|index| agent(&format!("agent{index}"), AgentMode::Subagent, false))
            .collect();
        agents.push(agent("build", AgentMode::Primary, false));
        agents.push(agent("secret", AgentMode::Subagent, true));
        let (rows, more) = rank_mention_agents(&agents, "");
        assert_eq!(rows.len(), 6);
        assert!(more);
        // The typo-tolerant fallback lists the near names after the match.
        let (rows, more) = rank_mention_agents(&agents, "agent7");
        assert_eq!(rows[0].name, "agent7");
        assert_eq!(rows.len(), 8);
        assert!(!more);
        assert!(rank_mention_agents(&agents, "build").0.is_empty());
        assert!(rank_mention_agents(&agents, "a b").0.is_empty());
    }

    #[test]
    fn mention_queries_search_the_longest_token() {
        assert_eq!(mention_tokens("./Solo Team"), ["solo", "team"]);
        assert_eq!(mention_server_query("team solo-is-a"), "solo-is-a");
        assert!(!mention_searches("release notes"));
        assert!(!mention_searches(""));
        assert!(mention_searches("src"));
    }

    fn row(relative: &str, directory: bool) -> FileRow {
        FileRow {
            path: format!("/root/{relative}"),
            relative_path: relative.to_owned(),
            directory,
        }
    }

    #[test]
    fn file_results_merge_folders_and_files_by_match_quality() {
        let files = [
            row("solo-is-a-team-size/index.md", false),
            row("software-developer/index.md", false),
        ];
        let folders = [
            row("machine-learning/tensorflow/", true),
            row("solo-is-a-team-size/", true),
        ];
        let ranked: Vec<String> = rank_file_results(&files, &folders, "solo")
            .into_iter()
            .map(|row| row.relative_path)
            .collect();
        assert_eq!(
            &ranked[..2],
            ["solo-is-a-team-size/", "solo-is-a-team-size/index.md"]
        );
        assert!(!ranked.contains(&"machine-learning/tensorflow/".to_owned()));
        let ranked = rank_file_results(&files, &[], "team solo");
        assert_eq!(ranked.len(), 1);
    }

    fn session(id: &str, title: &str, updated: i64, directory: &str) -> SessionRecord {
        SessionRecord {
            id: SessionId::new(id),
            parent_id: None,
            directory: Some(directory.to_owned()),
            title: title.to_owned(),
            time: SessionTime {
                created: 0,
                updated,
                idle: None,
                archived: None,
            },
            project_id: None,
            project: None,
            kind: SessionKind::default(),
            overlay: Default::default(),
            model: None,
            agent: None,
            revert: None,
            btw: Default::default(),
            goal: None,
            cost: None,
            context_pins: Vec::new(),
        }
    }

    #[test]
    fn session_mentions_rank_the_open_project_first_and_skip_the_current() {
        let a = session("ses_a", "Release notes", 1, "/elsewhere");
        let b = session("ses_b", "Release plan", 2, "/repo");
        let c = session("ses_c", "Release current", 3, "/repo");
        let sessions = [&a, &b, &c];
        let ranked: Vec<&str> =
            rank_session_mentions(&sessions, "release", Some("ses_c"), Some("/repo"))
                .into_iter()
                .map(|session| session.id.as_str())
                .collect();
        assert_eq!(ranked, ["ses_b", "ses_a"]);
        assert!(rank_session_mentions(&sessions, " ", None, None).is_empty());
        assert!(has_session_mention(&sessions, "release no", None));
        assert!(!has_session_mention(&sessions, "relase", None));
    }

    #[test]
    fn selection_wraps_and_follows_the_same_row_across_answers() {
        assert_eq!(step(0, 3, false), 2);
        assert_eq!(step(2, 3, true), 0);
        assert_eq!(step(0, 0, true), 0);
        assert_eq!(carried_selection(&["a", "b"], 1, &["c", "b"], true), 1);
        assert_eq!(carried_selection(&["a", "b"], 1, &["c", "b"], false), 0);
        assert_eq!(list_state(0, true), ListState::Loading);
        assert_eq!(list_state(0, false), ListState::Empty);
    }
}
