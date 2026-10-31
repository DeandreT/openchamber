//! Starter chips on the new-session view, ported from `lib/draftStarters.ts`
//! and `components/chat/useDraftStarters.ts`.
//!
//! A starter is a reference to a command or skill. The global list lives in
//! the settings document; a project's list is the team's shared starters
//! (the repository's `.openchamber/project.json`) followed by the user's
//! own. Only the user's own lists are ever written back, and every edit
//! keeps references this client cannot resolve, so they survive for clients
//! that can.

use std::collections::HashSet;

/// What a starter runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StarterKind {
    Command,
    Skill,
}

impl StarterKind {
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "command" => Some(Self::Command),
            "skill" => Some(Self::Skill),
            _ => None,
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Skill => "skill",
        }
    }
}

/// A pinned command or skill.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StarterRef {
    pub kind: StarterKind,
    pub name: String,
}

impl StarterRef {
    pub fn command(name: &str) -> Self {
        Self {
            kind: StarterKind::Command,
            name: name.to_owned(),
        }
    }

    /// `starterKey`.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind.as_wire(), self.name)
    }
}

/// Where a project starter is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StarterSource {
    /// The repository's shared file; only that file removes it.
    Shared,
    /// The user's own project settings.
    Personal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectStarter {
    pub starter: StarterRef,
    pub source: StarterSource,
}

/// One of OpenChamber's own commands, always available to pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltInStarter {
    pub name: &'static str,
    pub icon: &'static str,
    pub label_key: &'static str,
    pub command: &'static str,
}

/// `BUILTIN_STARTERS`, in the default order.
pub const BUILTIN_STARTERS: [BuiltInStarter; 8] = [
    BuiltInStarter {
        name: "explore",
        icon: "compass-3",
        label_key: "chat.draftPresets.explore.label",
        command: "/explore",
    },
    BuiltInStarter {
        name: "catch-up",
        icon: "history",
        label_key: "chat.draftPresets.catchup.label",
        command: "/catch-up",
    },
    BuiltInStarter {
        name: "weigh",
        icon: "scales-3",
        label_key: "chat.draftPresets.weigh.label",
        command: "/weigh",
    },
    BuiltInStarter {
        name: "plan-feature",
        icon: "survey",
        label_key: "chat.draftPresets.plan.label",
        command: "/plan-feature",
    },
    BuiltInStarter {
        name: "craft-goal",
        icon: "target",
        label_key: "chat.draftPresets.craftGoal.label",
        command: "/craft-goal",
    },
    BuiltInStarter {
        name: "schedule-task",
        icon: "calendar-schedule",
        label_key: "chat.draftPresets.scheduleTask.label",
        command: "/schedule-task",
    },
    BuiltInStarter {
        name: "debug",
        icon: "bug",
        label_key: "chat.draftPresets.debug.label",
        command: "/debug",
    },
    BuiltInStarter {
        name: "review",
        icon: "search-eye",
        label_key: "chat.draftPresets.review.label",
        command: "/workspace-review",
    },
];

/// `COMMAND_FALLBACK_ICON` and `SKILL_FALLBACK_ICON`.
pub const COMMAND_FALLBACK_ICON: &str = "terminal-box";
pub const SKILL_FALLBACK_ICON: &str = "book-open";

pub fn built_in(name: &str) -> Option<&'static BuiltInStarter> {
    BUILTIN_STARTERS.iter().find(|starter| starter.name == name)
}

/// The global list until the user edits it: every built-in.
pub fn default_global() -> Vec<StarterRef> {
    BUILTIN_STARTERS
        .iter()
        .map(|starter| StarterRef::command(starter.name))
        .collect()
}

/// `sanitizeStarterRefs`: entries with a known type and a nonblank trimmed
/// name, first occurrence of each.
pub fn sanitize<'a>(
    entries: impl IntoIterator<Item = (Option<&'a str>, Option<&'a str>)>,
) -> Vec<StarterRef> {
    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    for (kind, name) in entries {
        let Some(kind) = kind.and_then(StarterKind::from_wire) else {
            continue;
        };
        let name = name.map(str::trim).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let starter = StarterRef {
            kind,
            name: name.to_owned(),
        };
        if seen.insert(starter.key()) {
            refs.push(starter);
        }
    }
    refs
}

/// `withOfferedBuiltInStarters`: Craft a Goal and Schedule a Task join a
/// saved list that predates them, unless the saved markers say the user
/// already saw them, so a deliberate removal stays removed.
pub fn with_offered_built_ins(
    mut starters: Vec<StarterRef>,
    craft_goal_added: bool,
    schedule_task_added: bool,
) -> Vec<StarterRef> {
    let mut insert_after = |name: &str, after: &str| {
        if starters
            .iter()
            .any(|starter| starter.kind == StarterKind::Command && starter.name == name)
        {
            return;
        }
        let at = starters
            .iter()
            .position(|starter| starter.kind == StarterKind::Command && starter.name == after)
            .map_or(starters.len(), |anchor| anchor + 1);
        starters.insert(at, StarterRef::command(name));
    };
    if !craft_goal_added {
        insert_after("craft-goal", "plan-feature");
    }
    if !schedule_task_added {
        insert_after("schedule-task", "craft-goal");
    }
    starters
}

/// `normalizeStarterLabel`: "/simplify-code" reads "Simplify code".
pub fn normalize_label(name: &str) -> String {
    let base = name.strip_prefix('/').unwrap_or(name);
    let words: Vec<&str> = base
        .split(|character: char| character == '-' || character == '_' || character.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect();
    let base = words.join(" ");
    let mut characters = base.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => name.to_owned(),
    }
}

/// Which list a chip came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StarterGroup {
    Global,
    Project,
}

/// A chip's label: a locale key for a built-in, the normalized name else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StarterLabel {
    Key(&'static str),
    Text(String),
}

/// A starter this client can show and run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedStarter {
    /// `group:type:name`, the chip's identity.
    pub id: String,
    pub starter: StarterRef,
    pub group: StarterGroup,
    pub label: StarterLabel,
    pub icon: &'static str,
    /// What activation submits: the command or `$skill`.
    pub submit_text: String,
    /// Pinned in the repository's shared file.
    pub shared: bool,
}

/// What the client knows exists, so a starter that names nothing is hidden.
#[derive(Clone, Debug, Default)]
pub struct Available {
    pub commands: HashSet<String>,
    pub skills: HashSet<String>,
}

fn chip_id(group: StarterGroup, starter: &StarterRef) -> String {
    let group = match group {
        StarterGroup::Global => "global",
        StarterGroup::Project => "project",
    };
    format!("{group}:{}", starter.key())
}

/// The `resolve` of `useDraftStarters`: a built-in always resolves; another
/// command or skill only when `available` lists it.
pub fn resolve(
    starter: &StarterRef,
    group: StarterGroup,
    shared: bool,
    available: &Available,
) -> Option<ResolvedStarter> {
    let resolved = |label, icon, submit_text| ResolvedStarter {
        id: chip_id(group, starter),
        starter: starter.clone(),
        group,
        label,
        icon,
        submit_text,
        shared,
    };
    match starter.kind {
        StarterKind::Command => {
            if let Some(built_in) = built_in(&starter.name) {
                return Some(resolved(
                    StarterLabel::Key(built_in.label_key),
                    built_in.icon,
                    built_in.command.to_owned(),
                ));
            }
            available.commands.contains(&starter.name).then(|| {
                resolved(
                    StarterLabel::Text(normalize_label(&starter.name)),
                    COMMAND_FALLBACK_ICON,
                    format!("/{}", starter.name),
                )
            })
        }
        StarterKind::Skill => available.skills.contains(&starter.name).then(|| {
            resolved(
                StarterLabel::Text(normalize_label(&starter.name)),
                SKILL_FALLBACK_ICON,
                format!("${}", starter.name),
            )
        }),
    }
}

/// `submitPresetPrompt`: the starter, then the composer's trimmed draft. A
/// command takes it on the same line, because OpenCode reads a command's
/// arguments only there; a skill keeps it on the next line.
pub fn submission(starter: &ResolvedStarter, draft: &str) -> String {
    let draft = draft.trim();
    if draft.is_empty() {
        return starter.submit_text.clone();
    }
    let separator = match starter.starter.kind {
        StarterKind::Command => ' ',
        StarterKind::Skill => '\n',
    };
    format!("{}{separator}{draft}", starter.submit_text)
}

/// `addStarter` on one list; `None` when it is already there.
pub fn add(list: &[StarterRef], starter: StarterRef) -> Option<Vec<StarterRef>> {
    if list.contains(&starter) {
        return None;
    }
    let mut next = list.to_vec();
    next.push(starter);
    Some(next)
}

/// `removeStarter` on one list.
pub fn remove(list: &[StarterRef], starter: &StarterRef) -> Vec<StarterRef> {
    list.iter()
        .filter(|entry| *entry != starter)
        .cloned()
        .collect()
}

/// `reorder` on one list: `from` moves to where `to` is (`arrayMove`).
/// `None` when either is missing or they are the same.
pub fn reorder(list: &[StarterRef], from: &StarterRef, to: &StarterRef) -> Option<Vec<StarterRef>> {
    let from_index = list.iter().position(|entry| entry == from)?;
    let to_index = list.iter().position(|entry| entry == to)?;
    if from_index == to_index {
        return None;
    }
    let mut next = list.to_vec();
    let moved = next.remove(from_index);
    next.insert(to_index, moved);
    Some(next)
}

/// The user's own project starters, without the shared ones.
pub fn personal(project: &[ProjectStarter]) -> Vec<StarterRef> {
    project
        .iter()
        .filter(|entry| entry.source == StarterSource::Personal)
        .map(|entry| entry.starter.clone())
        .collect()
}

/// The shared project starters.
pub fn shared(project: &[ProjectStarter]) -> Vec<StarterRef> {
    project
        .iter()
        .filter(|entry| entry.source == StarterSource::Shared)
        .map(|entry| entry.starter.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing_keeps_the_first_valid_entry_of_each() {
        let refs = sanitize([
            (Some("command"), Some(" explore ")),
            (Some("command"), Some("explore")),
            (Some("skill"), Some("")),
            (Some("prompt"), Some("x")),
            (None, Some("y")),
            (Some("skill"), Some("review")),
        ]);
        assert_eq!(
            refs,
            [
                StarterRef::command("explore"),
                StarterRef {
                    kind: StarterKind::Skill,
                    name: "review".to_owned()
                }
            ]
        );
    }

    #[test]
    fn missing_built_ins_join_after_their_anchor_until_offered() {
        let saved = vec![
            StarterRef::command("plan-feature"),
            StarterRef::command("debug"),
        ];
        let names = |list: Vec<StarterRef>| list.into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert_eq!(
            names(with_offered_built_ins(saved.clone(), false, false)),
            ["plan-feature", "craft-goal", "schedule-task", "debug"]
        );
        assert_eq!(
            names(with_offered_built_ins(saved, true, true)),
            ["plan-feature", "debug"]
        );
    }

    #[test]
    fn labels_and_submissions_follow_the_reference() {
        assert_eq!(normalize_label("/simplify-code"), "Simplify code");
        assert_eq!(normalize_label("a__b  c"), "A b c");
        assert_eq!(normalize_label("-"), "-");
        let available = Available::default();
        let explore = resolve(
            &StarterRef::command("explore"),
            StarterGroup::Global,
            false,
            &available,
        )
        .expect("built-in");
        assert_eq!(explore.id, "global:command:explore");
        assert_eq!(submission(&explore, "  my idea \n"), "/explore my idea");
        assert_eq!(submission(&explore, "   "), "/explore");
        let review = resolve(
            &StarterRef::command("review"),
            StarterGroup::Global,
            false,
            &available,
        )
        .expect("built-in");
        assert_eq!(review.submit_text, "/workspace-review");
        let skill = StarterRef {
            kind: StarterKind::Skill,
            name: "lint".to_owned(),
        };
        assert!(resolve(&skill, StarterGroup::Project, false, &available).is_none());
        let available = Available {
            skills: ["lint".to_owned()].into(),
            ..Available::default()
        };
        let skill = resolve(&skill, StarterGroup::Project, true, &available).expect("listed");
        assert_eq!(submission(&skill, "x"), "$lint\nx");
        assert!(skill.shared);
    }

    #[test]
    fn edits_keep_unresolved_entries() {
        let custom = StarterRef::command("custom");
        let list = vec![
            StarterRef::command("explore"),
            custom.clone(),
            StarterRef::command("debug"),
        ];
        let moved = reorder(
            &list,
            &StarterRef::command("debug"),
            &StarterRef::command("explore"),
        )
        .expect("both listed");
        assert_eq!(moved[0].name, "debug");
        assert!(moved.contains(&custom));
        assert!(add(&list, custom.clone()).is_none());
        assert_eq!(remove(&list, &StarterRef::command("explore")).len(), 2);
    }
}
