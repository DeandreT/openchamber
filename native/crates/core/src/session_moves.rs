//! Where a session row's Move to worktree submenu can send the session.
//!
//! Ports `buildSessionWorktreeMenuTargets` and `getSessionWorktreeMenuState`
//! (`packages/ui/src/components/session/sidebar/sessionWorktreeMenu.ts`) and
//! the ordering of `requestSessionTreeMove`
//! (`packages/ui/src/lib/worktrees/sessionWorktreeMove.ts`): descendants move
//! before the root, and a rollback walks the moved sessions in reverse.

use std::cmp::Ordering;

use crate::ids::SessionId;
use crate::ownership::{ConfiguredProject, ProjectWorktrees, WorktreeInfo};
use crate::path;
use crate::session_actions::{SubmenuAction, SubmenuEntry, SubmenuItem, SubmenuLabel};

/// One place the submenu lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeTarget {
    pub path: String,
    /// What the line shows for a linked worktree.
    pub label: String,
    /// The repository's main checkout, shown as Main.
    pub primary: bool,
    /// The session is here now; the line is checked and disabled.
    pub current: bool,
}

/// `checkoutHoldsSubfolder`: `directory` lies strictly inside `checkout`.
pub fn checkout_holds_subfolder(checkout: &str, directory: &str) -> bool {
    let prefix = if checkout.ends_with('/') {
        checkout.to_owned()
    } else {
        format!("{checkout}/")
    };
    directory.starts_with(&prefix)
}

/// `checkoutHoldsDirectory`: `directory` is `checkout` or lies inside it.
pub fn checkout_holds_directory(checkout: &str, directory: &str) -> bool {
    directory == checkout || checkout_holds_subfolder(checkout, directory)
}

fn fallback_label(path: &str) -> String {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_owned()
}

fn worktree_label(worktree: &WorktreeInfo) -> String {
    [&worktree.label, &worktree.branch, &worktree.name]
        .into_iter()
        .flatten()
        .find(|text| !text.is_empty())
        .cloned()
        .unwrap_or_else(|| worktree.path.clone())
}

/// The sort label of `compareLinkedTargets`: branch, then name, label, path.
fn sort_label(worktree: &WorktreeInfo) -> &str {
    [&worktree.branch, &worktree.name, &worktree.label]
        .into_iter()
        .flatten()
        .find(|text| !text.is_empty())
        .map_or(worktree.path.as_str(), String::as_str)
}

/// `localeCompare` with `sensitivity: 'base'` for the ASCII labels Git
/// produces: case-insensitive, then the raw text as a stable tie-break.
fn compare_base(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// The submenu's places: the repository's main checkout first, then the
/// linked worktrees by branch. A checkout that holds the project from
/// outside (the repository root around a project at `repo/app`) is left
/// out, unless the session sits there now. The session's own directory is
/// always listed, as the current place.
///
/// `discovered` is the project's linked worktrees as the topology read
/// lists them; `current` is the worktree the session's row belongs to.
pub fn worktree_targets(
    project_path: Option<&str>,
    discovered: &[WorktreeInfo],
    source_directory: Option<&str>,
    current: Option<&WorktreeInfo>,
) -> Vec<WorktreeTarget> {
    let project_path = path::normalize_optional(project_path);
    let source = path::normalize_optional(source_directory)
        .or_else(|| path::normalize_optional(current.map(|worktree| worktree.path.as_str())));
    let primary = project_path.clone();
    let mut targets: Vec<(WorktreeTarget, Option<WorktreeInfo>)> = Vec::new();
    let mut push = |target: WorktreeTarget, info: Option<WorktreeInfo>| {
        if targets.iter().any(|(known, _)| known.path == target.path) {
            return;
        }
        if let Some(project) = &project_path
            && Some(&target.path) != source.as_ref()
            && checkout_holds_subfolder(&target.path, project)
        {
            return;
        }
        targets.push((target, info));
    };
    for worktree in discovered {
        let Some(path) = path::normalize(&worktree.path) else {
            continue;
        };
        push(
            WorktreeTarget {
                label: worktree_label(worktree),
                primary: primary.as_ref() == Some(&path),
                current: source.as_ref() == Some(&path),
                path,
            },
            Some(worktree.clone()),
        );
    }
    if let Some(primary) = &primary {
        push(
            WorktreeTarget {
                path: primary.clone(),
                label: fallback_label(primary),
                primary: true,
                current: source.as_ref() == Some(primary),
            },
            None,
        );
    }
    if let Some(source) = &source {
        let current_path = current.and_then(|worktree| path::normalize(&worktree.path));
        let label = match current {
            Some(worktree) if current_path.as_ref() == Some(source) => {
                let label = worktree_label(worktree);
                if label == worktree.path {
                    fallback_label(source)
                } else {
                    label
                }
            }
            _ => fallback_label(source),
        };
        push(
            WorktreeTarget {
                path: source.clone(),
                label,
                primary: primary.as_ref() == Some(source),
                current: true,
            },
            None,
        );
    }
    let (mut primaries, mut linked): (Vec<_>, Vec<_>) =
        targets.into_iter().partition(|(target, _)| target.primary);
    primaries.sort_by(|(a, _), (b, _)| compare_base(&a.path, &b.path));
    linked.sort_by(|(a, a_info), (b, b_info)| {
        let label = |target: &WorktreeTarget, info: &Option<WorktreeInfo>| {
            info.as_ref()
                .map_or(target.label.clone(), |info| sort_label(info).to_owned())
        };
        compare_base(&label(a, a_info), &label(b, b_info))
            .then_with(|| compare_base(&a.path, &b.path))
    });
    primaries
        .into_iter()
        .chain(linked)
        .map(|(target, _)| target)
        .collect()
}

/// The Move to worktree submenu: each place (the current one checked and
/// disabled), a note when the list could not be read and nothing is known,
/// then New worktree after a separator.
pub fn worktree_submenu(
    targets: &[WorktreeTarget],
    load_failed: bool,
    enabled: bool,
) -> Vec<SubmenuEntry> {
    let mut entries: Vec<SubmenuEntry> = targets
        .iter()
        .map(|target| {
            SubmenuEntry::Item(SubmenuItem {
                enabled: enabled && !target.current,
                checked: target.current,
                title: Some(target.path.clone()),
                ..SubmenuItem::new(
                    SubmenuAction::MoveToWorktree {
                        path: target.path.clone(),
                    },
                    if target.primary {
                        SubmenuLabel::Key("sessions.sidebar.session.moveToWorktree.main")
                    } else {
                        SubmenuLabel::Text(target.label.clone())
                    },
                )
            })
        })
        .collect();
    if load_failed && targets.is_empty() {
        entries.push(SubmenuEntry::Note(
            "sessions.sidebar.session.moveToWorktree.loadFailed",
        ));
    }
    entries.push(SubmenuEntry::Separator);
    entries.push(SubmenuEntry::Item(SubmenuItem {
        icon: Some("add"),
        enabled,
        ..SubmenuItem::new(
            SubmenuAction::NewWorktree,
            SubmenuLabel::Key("sessions.sidebar.session.menu.newWorktree"),
        )
    }));
    entries
}

/// The order a tree moves in: descendants first, the root last, so a
/// failure leaves the root where the user still finds it.
pub fn move_order(root: &SessionId, descendants: &[SessionId]) -> Vec<SessionId> {
    let mut order: Vec<SessionId> = descendants
        .iter()
        .filter(|id| *id != root)
        .cloned()
        .collect();
    order.push(root.clone());
    order
}

/// `resolveProjectRef`: the project owning the longest worktree that holds
/// `directory`, else the longest project path that holds it.
pub fn project_for_directory<'a>(
    projects: &'a [ConfiguredProject],
    worktrees: &[ProjectWorktrees],
    directory: &str,
) -> Option<&'a ConfiguredProject> {
    let directory = path::normalize(directory)?;
    let mut best: Option<(&'a ConfiguredProject, usize)> = None;
    for listing in worktrees {
        for worktree in &listing.worktrees {
            let Some(worktree_path) = path::normalize(&worktree.path) else {
                continue;
            };
            if !checkout_holds_directory(&worktree_path, &directory)
                || best.is_some_and(|(_, length)| worktree_path.len() <= length)
            {
                continue;
            }
            if let Some(owner) = project_holding(projects, &listing.project_path) {
                best = Some((owner, worktree_path.len()));
            }
        }
    }
    best.map(|(project, _)| project)
        .or_else(|| project_holding(projects, &directory))
}

/// `resolveProjectForDirectory`: the longest project path holding
/// `directory`.
pub fn project_holding<'a>(
    projects: &'a [ConfiguredProject],
    directory: &str,
) -> Option<&'a ConfiguredProject> {
    let directory = path::normalize(directory)?;
    projects
        .iter()
        .filter(|project| {
            path::normalize(&project.path)
                .is_some_and(|root| checkout_holds_directory(&root, &directory))
        })
        .max_by_key(|project| project.path.len())
}

const ADJECTIVES: [&str; 33] = [
    "artful", "bionic", "cosmic", "disco", "focal", "groovy", "jammy", "kinetic", "lunar", "noble",
    "bold", "brave", "calm", "eager", "gentle", "happy", "keen", "lively", "merry", "swift",
    "warm", "wise", "bright", "clever", "daring", "agile", "crisp", "fresh", "lucid", "quick",
    "sharp", "vivid", "zealous",
];

const NOUNS: [&str; 29] = [
    "aardvark",
    "beaver",
    "chipmunk",
    "dolphin",
    "falcon",
    "gopher",
    "hedgehog",
    "jackal",
    "koala",
    "lemur",
    "mongoose",
    "narwhal",
    "otter",
    "pangolin",
    "quokka",
    "raccoon",
    "salamander",
    "toucan",
    "walrus",
    "yak",
    "zebra",
    "badger",
    "condor",
    "dingo",
    "egret",
    "ferret",
    "gecko",
    "heron",
    "iguana",
];

/// `generateBranchSlug`: an adjective and a noun, such as `agile-chipmunk`.
/// `pick` returns a uniformly random index below its argument.
pub fn generate_branch_slug(mut pick: impl FnMut(usize) -> usize) -> String {
    let adjective = ADJECTIVES[pick(ADJECTIVES.len()) % ADJECTIVES.len()];
    let noun = NOUNS[pick(NOUNS.len()) % NOUNS.len()];
    format!("{adjective}-{noun}")
}

/// A project's worktree setup as `GET /api/projects/:id/config` reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeSetup {
    /// The merged commands that run.
    pub commands: Vec<String>,
    pub shared_commands: Vec<String>,
    pub personal_commands: Vec<String>,
    /// The personal commands replace the shared ones.
    pub personal_replaces: bool,
    /// The shared commands were trusted for their current content, or there
    /// is nothing to trust.
    pub shared_trusted: bool,
}

/// The commands a new worktree runs (`resolveWorktreeSetupCommands`).
/// Untrusted shared commands never run: the reference asks first, and
/// without that answer only the user's own commands do.
pub fn worktree_setup_commands(setup: &WorktreeSetup) -> &[String] {
    if setup.shared_commands.is_empty() || setup.personal_replaces || setup.shared_trusted {
        &setup.commands
    } else {
        &setup.personal_commands
    }
}

/// `buildSdkStartCommand`: the trimmed commands joined with `&&`, with the
/// root project path substituted.
pub fn start_command(commands: &[String], root: &str) -> Option<String> {
    let joined = commands
        .iter()
        .map(|command| command.trim())
        .filter(|command| !command.is_empty())
        .map(|command| {
            command
                .replace("${ROOT_PROJECT_PATH}", root)
                .replace("$ROOT_PROJECT_PATH", root)
                .replace("${ROOT_WORKTREE_PATH}", root)
                .replace("$ROOT_WORKTREE_PATH", root)
        })
        .collect::<Vec<_>>()
        .join(" && ");
    (!joined.trim().is_empty()).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree(path: &str, branch: &str) -> WorktreeInfo {
        WorktreeInfo {
            branch: Some(branch.to_owned()),
            ..WorktreeInfo::new(path)
        }
    }

    fn paths(targets: &[WorktreeTarget]) -> Vec<&str> {
        targets.iter().map(|target| target.path.as_str()).collect()
    }

    #[test]
    fn main_comes_first_then_linked_worktrees_by_branch() {
        let targets = worktree_targets(
            Some("/repo"),
            &[
                worktree("/repo/.worktrees/zeta", "zeta"),
                worktree("/repo/.worktrees/alpha", "Alpha"),
            ],
            Some("/repo/.worktrees/zeta"),
            None,
        );
        assert_eq!(
            paths(&targets),
            ["/repo", "/repo/.worktrees/alpha", "/repo/.worktrees/zeta"]
        );
        assert!(targets[0].primary && !targets[0].current);
        assert!(targets[2].current);
        assert_eq!(targets[1].label, "Alpha");
    }

    #[test]
    fn a_checkout_around_the_project_is_left_out_unless_the_session_is_there() {
        let around = [worktree("/repo", "main")];
        let targets = worktree_targets(Some("/repo/app"), &around, Some("/repo/app"), None);
        assert_eq!(
            paths(&targets),
            ["/repo/app"],
            "the root would leave the project"
        );
        let there = worktree_targets(Some("/repo/app"), &around, Some("/repo"), None);
        assert!(paths(&there).contains(&"/repo"));
    }

    #[test]
    fn the_session_directory_is_always_listed_as_current() {
        let targets = worktree_targets(Some("/repo"), &[], Some("/elsewhere/wt"), None);
        assert_eq!(paths(&targets), ["/repo", "/elsewhere/wt"]);
        assert!(targets[1].current);
        assert_eq!(targets[1].label, "wt");
    }

    #[test]
    fn the_submenu_disables_the_current_place_and_notes_a_failed_read() {
        let targets = worktree_targets(
            Some("/repo"),
            &[worktree("/repo/.worktrees/a", "a")],
            Some("/repo"),
            None,
        );
        let entries = worktree_submenu(&targets, true, true);
        assert!(!entries[0].is_enabled(), "the session is already there");
        assert!(entries[1].is_enabled());
        assert!(
            !entries
                .iter()
                .any(|entry| matches!(entry, SubmenuEntry::Note(_))),
            "a failed refresh over a known list says nothing"
        );
        let empty = worktree_submenu(&[], true, true);
        assert!(matches!(empty[0], SubmenuEntry::Note(_)));
        assert_eq!(
            empty.last().and_then(SubmenuEntry::action),
            Some(&SubmenuAction::NewWorktree)
        );
        let busy = worktree_submenu(&targets, false, false);
        assert!(busy.iter().all(|entry| !entry.is_enabled()));
    }

    #[test]
    fn descendants_move_before_their_root() {
        let root = SessionId::from("root");
        let order = move_order(
            &root,
            &[
                SessionId::from("a"),
                SessionId::from("root"),
                SessionId::from("b"),
            ],
        );
        let ids: Vec<&str> = order.iter().map(SessionId::as_str).collect();
        assert_eq!(ids, ["a", "b", "root"]);
    }

    fn project(id: &str, path: &str) -> ConfiguredProject {
        ConfiguredProject {
            id: crate::ids::ProjectId::from(id),
            path: path.to_owned(),
        }
    }

    #[test]
    fn a_worktree_directory_belongs_to_the_project_that_lists_it() {
        let projects = [project("app", "/repo"), project("sub", "/repo/sub")];
        let worktrees = [ProjectWorktrees {
            project_path: "/repo".to_owned(),
            worktrees: vec![WorktreeInfo::new("/elsewhere/wt")],
        }];
        let owner = project_for_directory(&projects, &worktrees, "/elsewhere/wt/src");
        assert_eq!(owner.map(|project| project.path.as_str()), Some("/repo"));
        let nested = project_for_directory(&projects, &worktrees, "/repo/sub/x");
        assert_eq!(
            nested.map(|project| project.path.as_str()),
            Some("/repo/sub")
        );
        assert!(project_for_directory(&projects, &worktrees, "/tmp").is_none());
    }

    #[test]
    fn generated_branch_names_pair_an_adjective_and_a_noun() {
        let mut picks = [25, 2].into_iter();
        assert_eq!(
            generate_branch_slug(|_| picks.next().expect("two picks")),
            "agile-chipmunk"
        );
    }

    #[test]
    fn untrusted_shared_setup_commands_never_run() {
        let setup = WorktreeSetup {
            commands: vec!["shared".into(), "mine".into()],
            shared_commands: vec!["shared".into()],
            personal_commands: vec!["mine".into()],
            personal_replaces: false,
            shared_trusted: false,
        };
        assert_eq!(worktree_setup_commands(&setup), ["mine"]);
        let trusted = WorktreeSetup {
            shared_trusted: true,
            ..setup.clone()
        };
        assert_eq!(worktree_setup_commands(&trusted), ["shared", "mine"]);
    }

    #[test]
    fn start_commands_join_and_substitute_the_root() {
        assert_eq!(
            start_command(
                &[
                    " cp $ROOT_PROJECT_PATH/.env . ".into(),
                    "".into(),
                    "bun i".into()
                ],
                "/repo"
            )
            .as_deref(),
            Some("cp /repo/.env . && bun i")
        );
        assert_eq!(start_command(&["  ".into()], "/repo"), None);
    }
}
