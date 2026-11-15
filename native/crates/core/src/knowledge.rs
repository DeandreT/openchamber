//! Project knowledge: notes, todos and plans as the Project knowledge panel
//! reads them (`components/session/project-context/*`, `projectContextApi.ts`).
//!
//! Storage is the server's (`packages/web/server/lib/project-context`); this
//! module holds the records and the panel's display rules: note titles, the
//! todo order, the dependency rule an agent's `ready` list uses, the plan
//! markdown split and the panel-wide search.

use std::collections::HashSet;

use crate::search_rank;

/// The server's limits, which the composers enforce before sending.
pub const NOTE_BODY_MAX_LENGTH: usize = 3000;
pub const TODO_TEXT_MAX_LENGTH: usize = 1000;
pub const PLAN_TITLE_MAX_LENGTH: usize = 160;

/// Where a note came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NoteSource {
    #[default]
    Manual,
    Selection,
    Agent,
}

impl NoteSource {
    pub fn parse(value: &str) -> Self {
        match value {
            "selection" => Self::Selection,
            "agent" => Self::Agent,
            _ => Self::Manual,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub id: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub source: NoteSource,
    pub plan_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    pub id: String,
    pub text: String,
    pub completed: bool,
    pub created_at: i64,
    /// The plan this todo belongs to.
    pub plan_id: Option<String>,
    /// Todos of the same plan to complete first.
    pub depends_on: Vec<String>,
}

/// Ticked and total steps of a plan's `## Steps` checklist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanProgress {
    pub done: u32,
    pub total: u32,
}

impl PlanProgress {
    pub fn is_done(self) -> bool {
        self.done >= self.total
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PlanSource {
    #[default]
    Personal,
    Shared,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub source: PlanSource,
    pub progress: Option<PlanProgress>,
    /// The session the plan was last sent to with Implement or Improve.
    pub session_id: Option<String>,
}

/// One owner's stored knowledge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Knowledge {
    pub notes: Vec<Note>,
    pub todos: Vec<Todo>,
    pub plans: Vec<Plan>,
}

/// `splitNoteTitle`: the first non-empty line without its Markdown markers,
/// and the text after it.
pub fn split_note_title(body: &str) -> (String, String) {
    let lines: Vec<&str> = body.split('\n').collect();
    let Some(index) = lines.iter().position(|line| !line.trim().is_empty()) else {
        return (String::new(), String::new());
    };
    let mut title = lines[index].trim();
    let hashes = title.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&hashes)
        && title[hashes..].starts_with(|character: char| character.is_whitespace())
    {
        title = title[hashes..].trim_start();
    }
    if let Some(rest) = ["- ", "* ", "+ "]
        .iter()
        .find_map(|marker| title.strip_prefix(marker))
    {
        title = rest.trim_start();
    }
    if let Some(inner) = title
        .strip_prefix("**")
        .and_then(|rest| rest.strip_suffix("**"))
        .filter(|inner| !inner.is_empty())
    {
        title = inner;
    }
    (
        title.trim().to_owned(),
        lines[index + 1..].join("\n").trim().to_owned(),
    )
}

/// `parsePlanMarkdown`: a plan document's title and body, the way the
/// server splits a stored file.
pub fn parse_plan_markdown(raw: &str, fallback: &str) -> (String, String) {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = normalized.trim_start();
    if let Some(after) = trimmed.strip_prefix('#')
        && after.starts_with([' ', '\t'])
    {
        let line_end = after.find('\n').unwrap_or(after.len());
        let heading = after[..line_end].trim();
        if !heading.is_empty() {
            let body = after[line_end..].trim_start_matches('\n');
            return (heading.to_owned(), body.to_owned());
        }
    }
    let first = normalized
        .split('\n')
        .map(str::trim)
        .find(|line| !line.is_empty());
    let title = first
        .map(|line| line.trim_start_matches('#').trim())
        .filter(|title| !title.is_empty())
        .unwrap_or(fallback);
    (title.to_owned(), normalized.trim().to_owned())
}

/// Completed todos sink to the bottom; storage order is otherwise kept.
pub fn display_order(todos: &[Todo]) -> Vec<Todo> {
    todos
        .iter()
        .filter(|todo| !todo.completed)
        .chain(todos.iter().filter(|todo| todo.completed))
        .cloned()
        .collect()
}

/// `insertTodoBeforeCompleted`.
pub fn insert_before_completed(todos: &[Todo], todo: Todo) -> Vec<Todo> {
    let mut next = todos.to_vec();
    let index = next
        .iter()
        .position(|item| item.completed)
        .unwrap_or(next.len());
    next.insert(index, todo);
    next
}

/// The list a completion toggle shows: a completed todo moves to the end,
/// a reopened one before the completed ones.
pub fn toggled(todos: &[Todo], id: &str, completed: bool) -> Option<Vec<Todo>> {
    let todo = todos.iter().find(|todo| todo.id == id)?;
    if todo.completed == completed {
        return None;
    }
    let updated = Todo {
        completed,
        ..todo.clone()
    };
    let remaining: Vec<Todo> = todos.iter().filter(|todo| todo.id != id).cloned().collect();
    Some(if completed {
        let mut next = remaining;
        next.push(updated);
        next
    } else {
        insert_before_completed(&remaining, updated)
    })
}

/// Unfinished todos whose dependencies are all completed, the knowledge
/// tool's `ready` list. A dependency on a todo that no longer exists counts
/// as met, the same as a completed one.
pub fn ready_todos(todos: &[Todo]) -> Vec<&Todo> {
    let open: HashSet<&str> = todos
        .iter()
        .filter(|todo| !todo.completed)
        .map(|todo| todo.id.as_str())
        .collect();
    todos
        .iter()
        .filter(|todo| !todo.completed)
        .filter(|todo| {
            todo.depends_on
                .iter()
                .all(|dependency| !open.contains(dependency.as_str()))
        })
        .collect()
}

/// The panel's sections, in tab order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Section {
    #[default]
    Notes,
    Todos,
    Plans,
}

impl Section {
    pub const ALL: [Self; 3] = [Self::Notes, Self::Todos, Self::Plans];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notes => "notes",
            Self::Todos => "todos",
            Self::Plans => "plans",
        }
    }
}

/// The search's matches per section, against what the panel shows: notes by
/// body, todos by text, plans by title, without entries whose delete waits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub notes: usize,
    pub todos: usize,
    pub plans: usize,
}

impl Counts {
    pub fn of(self, section: Section) -> usize {
        match section {
            Section::Notes => self.notes,
            Section::Todos => self.todos,
            Section::Plans => self.plans,
        }
    }
}

pub fn note_matches(note: &Note, query: &str) -> bool {
    search_rank::matches_rank_query([Some(note.body.as_str())], query)
}

pub fn todo_matches(todo: &Todo, query: &str) -> bool {
    search_rank::matches_rank_query([Some(todo.text.as_str())], query)
}

pub fn plan_matches(plan: &Plan, query: &str) -> bool {
    search_rank::matches_rank_query([Some(plan.title.as_str())], query)
}

/// Counts with `hidden` (pending deletes) left out.
pub fn counts(knowledge: &Knowledge, query: &str, hidden: &HashSet<String>) -> Counts {
    Counts {
        notes: knowledge
            .notes
            .iter()
            .filter(|note| !hidden.contains(&note.id) && note_matches(note, query))
            .count(),
        todos: knowledge
            .todos
            .iter()
            .filter(|todo| todo_matches(todo, query))
            .count(),
        plans: knowledge
            .plans
            .iter()
            .filter(|plan| !hidden.contains(&plan.id) && plan_matches(plan, query))
            .count(),
    }
}

/// The tab a changed query moves to: only off a tab with no matches, onto
/// the first that has some.
pub fn follow_search(active: Section, counts: Counts) -> Option<Section> {
    if counts.of(active) > 0 {
        return None;
    }
    Section::ALL
        .into_iter()
        .find(|section| counts.of(*section) > 0)
}

/// Text cut to `limit` UTF-16 units without splitting a character, with line
/// breaks as spaces when the field is one line.
pub fn single_line(text: &str, limit: usize) -> String {
    let mut normalized = text.replace("\r\n", "\n").replace(['\r', '\n'], " ");
    let mut units = 0;
    let mut end = 0;
    for character in normalized.chars() {
        if units + character.len_utf16() > limit {
            break;
        }
        units += character.len_utf16();
        end += character.len_utf8();
    }
    normalized.truncate(end);
    normalized
}

/// `appendWithLineBreaks`: `next` after `base` with a blank line between,
/// and a blank line after it so the next addition starts its own paragraph.
pub fn append_with_line_breaks(base: &str, next: &str) -> String {
    let separator = if base.is_empty() || base.ends_with("\n\n") {
        ""
    } else if base.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let trailing = if next.ends_with("\n\n") {
        ""
    } else if next.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{base}{separator}{next}{trailing}")
}

/// What "send to the current session" puts in the composer.
pub fn fenced_todo(text: &str) -> String {
    format!("```md\n{text}\n```")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_follow_the_composer_rule() {
        assert_eq!(append_with_line_breaks("", "one"), "one\n\n");
        assert_eq!(append_with_line_breaks("one\n\n", "two"), "one\n\ntwo\n\n");
        assert_eq!(append_with_line_breaks("one", "two\n"), "one\n\ntwo\n\n");
        assert_eq!(fenced_todo("Ship"), "```md\nShip\n```");
    }

    fn todo(id: &str, completed: bool, depends_on: &[&str]) -> Todo {
        Todo {
            id: id.to_owned(),
            text: id.to_owned(),
            completed,
            created_at: 0,
            plan_id: Some("plan".to_owned()),
            depends_on: depends_on.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    #[test]
    fn note_titles_drop_markdown_markers() {
        assert_eq!(
            split_note_title("\n# Release\nShip it\n\n"),
            ("Release".to_owned(), "Ship it".to_owned())
        );
        assert_eq!(split_note_title("- **Bold**").0, "Bold");
        assert_eq!(split_note_title("#hashtag").0, "#hashtag");
        assert_eq!(split_note_title("  \n "), (String::new(), String::new()));
    }

    #[test]
    fn plan_markdown_splits_like_the_server() {
        assert_eq!(
            parse_plan_markdown("# Launch\r\n\r\n## Steps\n- [ ] a", "Plan"),
            ("Launch".to_owned(), "## Steps\n- [ ] a".to_owned())
        );
        assert_eq!(
            parse_plan_markdown("Intro line\nmore", "Plan"),
            ("Intro line".to_owned(), "Intro line\nmore".to_owned())
        );
        assert_eq!(parse_plan_markdown("  ", "Plan").0, "Plan");
    }

    #[test]
    fn completion_moves_todos_around_the_completed_block() {
        let todos = vec![
            todo("a", false, &[]),
            todo("b", false, &[]),
            todo("c", true, &[]),
        ];
        let done = toggled(&todos, "a", true).expect("changes");
        assert_eq!(
            done.iter().map(|todo| todo.id.as_str()).collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
        let reopened = toggled(&done, "c", false).expect("changes");
        assert_eq!(
            reopened
                .iter()
                .map(|todo| todo.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
        assert!(toggled(&todos, "c", true).is_none(), "no change, no write");
        assert_eq!(
            display_order(&[todo("x", true, &[]), todo("y", false, &[])])
                .iter()
                .map(|todo| todo.id.as_str())
                .collect::<Vec<_>>(),
            ["y", "x"]
        );
    }

    #[test]
    fn ready_todos_wait_for_open_dependencies_only() {
        let todos = vec![
            todo("design", true, &[]),
            todo("build", false, &["design"]),
            todo("ship", false, &["build"]),
            todo("announce", false, &["gone"]),
        ];
        let ready: Vec<&str> = ready_todos(&todos)
            .iter()
            .map(|todo| todo.id.as_str())
            .collect();
        assert_eq!(ready, ["build", "announce"]);
    }

    #[test]
    fn search_follows_to_the_first_section_with_matches() {
        let knowledge = Knowledge {
            notes: vec![Note {
                id: "n".into(),
                body: "Release checklist".into(),
                created_at: 0,
                updated_at: 0,
                source: NoteSource::Manual,
                plan_id: None,
            }],
            todos: vec![todo("Write release notes", false, &[])],
            plans: Vec::new(),
        };
        let counts = counts(&knowledge, "release", &HashSet::new());
        assert_eq!((counts.notes, counts.todos, counts.plans), (1, 1, 0));
        assert_eq!(follow_search(Section::Plans, counts), Some(Section::Notes));
        assert_eq!(follow_search(Section::Todos, counts), None);
        let hidden: HashSet<String> = ["n".to_owned()].into();
        assert_eq!(super::counts(&knowledge, "release", &hidden).notes, 0);
        assert_eq!(single_line("a\nb\r\nc", 4), "a b ");
    }
}
