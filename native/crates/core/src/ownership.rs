//! Display ownership of session directories by configured projects.
//!
//! Mirrors `createSessionOwnershipIndex` in
//! `packages/ui/src/components/session/sidebar/sessions/sessionOwnership.ts`.
//! Ownership changes where a session is displayed, never the session's real
//! directory used for routing.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::ids::{OpenCodeProjectId, ProjectId, SessionId, SpaceId};
use crate::path;
use crate::session::SessionRecord;

/// The client surface whose ownership rules apply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Surface {
    /// Web and desktop: worktrees, spaces, parent-directory matching and the
    /// canonical-project bridge apply.
    #[default]
    Desktop,
    /// VS Code: exact workspace-directory scope only.
    VsCode,
}

/// A project registered in OpenChamber.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredProject {
    pub id: ProjectId,
    pub path: String,
}

/// A linked worktree as discovered by the server's Git topology.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: String,
    pub label: Option<String>,
    pub name: Option<String>,
    pub branch: Option<String>,
}

impl WorktreeInfo {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }
}

/// Linked worktrees discovered for a configured project root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectWorktrees {
    pub project_path: String,
    pub worktrees: Vec<WorktreeInfo>,
}

/// Valid, unique projects from the settings document's project paths, as
/// `sanitizeProjects` in `packages/ui/src/stores/useProjectsStore.ts` keeps
/// them: a leading `~` expands to `home`, paths are normalized, and the first
/// entry for a path or identifier wins.
pub fn configured_projects<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    home: Option<&str>,
) -> Vec<ConfiguredProject> {
    let mut projects: Vec<ConfiguredProject> = Vec::new();
    for raw in paths {
        let expanded = expand_home(raw.trim(), home);
        let Some(path) = path::normalize(&expanded) else {
            continue;
        };
        let Some(id) = ProjectId::from_path(&path) else {
            continue;
        };
        if projects
            .iter()
            .any(|project| project.id == id || project.path == path)
        {
            continue;
        }
        projects.push(ConfiguredProject { id, path });
    }
    projects
}

fn expand_home(value: &str, home: Option<&str>) -> String {
    match (value, home.filter(|home| !home.is_empty())) {
        ("~", Some(home)) => home.to_owned(),
        (value, Some(home)) if value.starts_with("~/") || value.starts_with("~\\") => {
            format!("{home}{}", &value[1..])
        }
        (value, _) => value.to_owned(),
    }
}

/// The linked worktrees of `project_path` from the server's listing, as
/// `readProjectWorktrees` in `packages/ui/src/lib/worktrees/worktreeManager.ts`
/// derives them: the main checkout is left out, `refs/heads/` is dropped from
/// branches, the label is the branch, the name or the folder name, and the
/// list is sorted by label.
pub fn linked_worktrees(project_path: &str, listing: Vec<WorktreeInfo>) -> Vec<WorktreeInfo> {
    let project = path::normalize(project_path);
    let mut worktrees: Vec<WorktreeInfo> = listing
        .into_iter()
        .filter_map(|entry| {
            let worktree_path = path::normalize(&entry.path)?;
            let branch = entry
                .branch
                .as_deref()
                .map(|branch| branch.trim_start_matches("refs/heads/").trim().to_owned())
                .unwrap_or_default();
            let name = entry
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| folder_name(&worktree_path));
            let label = if branch.is_empty() {
                name.clone()
            } else {
                branch.clone()
            };
            Some(WorktreeInfo {
                path: worktree_path,
                label: Some(label),
                name: Some(name),
                branch: Some(branch),
            })
        })
        .filter(|worktree| Some(&worktree.path) != project.as_ref())
        .collect();
    worktrees.sort_by_key(|worktree| worktree.label.clone().unwrap_or_default().to_lowercase());
    worktrees
}

fn folder_name(directory: &str) -> String {
    directory
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(directory)
        .to_owned()
}

/// An isolated space and the project directory it was made for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceMark {
    pub id: SpaceId,
    /// The project's path inside the space; absent when the host lost the project.
    pub directory: Option<String>,
    /// The registered host project the space was made for.
    pub project_directory: Option<String>,
}

/// OpenCode project metadata from an authoritative project read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCodeProject {
    pub id: OpenCodeProjectId,
    pub worktree: Option<String>,
}

/// What kind of configured scope owns a directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OwnerKind {
    Project,
    Worktree,
    Space(SpaceId),
}

/// The configured scope that owns a directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DirectoryOwner {
    pub project_id: ProjectId,
    pub project_root: String,
    pub scope_directory: String,
    pub kind: OwnerKind,
}

impl DirectoryOwner {
    /// Whether `candidate` takes precedence over `self` for the same directory.
    ///
    /// A project root outranks worktrees and spaces; otherwise the first kind
    /// registered stays. Within a kind the longer root wins, then the smaller
    /// project ID.
    fn is_replaced_by(&self, candidate: &DirectoryOwner) -> bool {
        if candidate.kind_rank() != self.kind_rank() {
            return candidate.kind == OwnerKind::Project;
        }
        match candidate.project_root.len().cmp(&self.project_root.len()) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => candidate.project_id < self.project_id,
        }
    }

    fn kind_rank(&self) -> u8 {
        match self.kind {
            OwnerKind::Project => 0,
            OwnerKind::Worktree => 1,
            OwnerKind::Space(_) => 2,
        }
    }
}

/// Everything ownership resolution reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct OwnershipInputs<'a> {
    pub sessions: &'a [SessionRecord],
    pub archived_sessions: &'a [SessionRecord],
    pub projects: &'a [ConfiguredProject],
    pub worktrees: &'a [ProjectWorktrees],
    pub authoritative_projects: &'a [OpenCodeProject],
    pub spaces: &'a [SpaceMark],
    pub surface: Surface,
}

/// Ownership of active and archived sessions by configured project.
#[derive(Debug, Default)]
pub struct SessionOwnershipIndex<'a> {
    by_session: HashMap<&'a SessionId, DirectoryOwner>,
    sessions_by_project: HashMap<ProjectId, Vec<&'a SessionRecord>>,
    archived_sessions_by_project: HashMap<ProjectId, Vec<&'a SessionRecord>>,
    sessions_by_scope: HashMap<String, HashSet<&'a SessionId>>,
    directory_resolutions: usize,
}

impl<'a> SessionOwnershipIndex<'a> {
    /// Resolves the owner of every active session and every archived root.
    ///
    /// The archive lists top-level sessions only: a subsession is restored with
    /// its parent, never alone.
    pub fn build(inputs: &OwnershipInputs<'a>) -> Self {
        let mut resolver = OwnerResolver::new(inputs);
        let mut index = Self::default();

        for session in inputs.sessions {
            let Some(owner) = resolver.owner_of_session(session) else {
                continue;
            };
            index
                .sessions_by_scope
                .entry(owner.scope_directory.clone())
                .or_default()
                .insert(&session.id);
            index
                .sessions_by_project
                .entry(owner.project_id.clone())
                .or_default()
                .push(session);
            index.by_session.insert(&session.id, owner);
        }
        for session in inputs.archived_sessions {
            if !session.is_root() {
                continue;
            }
            let Some(owner) = resolver.owner_of_session(session) else {
                continue;
            };
            index
                .archived_sessions_by_project
                .entry(owner.project_id.clone())
                .or_default()
                .push(session);
            index.by_session.insert(&session.id, owner);
        }
        index.directory_resolutions = resolver.resolved.len();
        index
    }

    pub fn owner(&self, session_id: &SessionId) -> Option<&DirectoryOwner> {
        self.by_session.get(session_id)
    }

    pub fn is_owned(&self, session_id: &SessionId) -> bool {
        self.by_session.contains_key(session_id)
    }

    /// Active sessions owned by `project_id`, in input order.
    pub fn project_sessions(&self, project_id: &ProjectId) -> &[&'a SessionRecord] {
        self.sessions_by_project
            .get(project_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Archived root sessions owned by `project_id`, in input order.
    pub fn archived_project_sessions(&self, project_id: &ProjectId) -> &[&'a SessionRecord] {
        self.archived_sessions_by_project
            .get(project_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn sessions_by_project(&self) -> &HashMap<ProjectId, Vec<&'a SessionRecord>> {
        &self.sessions_by_project
    }

    /// Active session IDs owned by each scope directory.
    pub fn sessions_by_scope(&self) -> &HashMap<String, HashSet<&'a SessionId>> {
        &self.sessions_by_scope
    }

    /// How many distinct directories resolution visited.
    pub fn directory_resolutions(&self) -> usize {
        self.directory_resolutions
    }
}

/// Directory-to-owner resolution with memoized parent walks.
struct OwnerResolver {
    surface: Surface,
    by_directory: HashMap<String, DirectoryOwner>,
    /// `None` marks OpenCode project IDs with conflicting or unconfigured metadata.
    canonical_by_opencode_project: HashMap<OpenCodeProjectId, Option<DirectoryOwner>>,
    resolved: HashMap<String, Option<DirectoryOwner>>,
}

impl OwnerResolver {
    fn new(inputs: &OwnershipInputs<'_>) -> Self {
        let mut by_directory: HashMap<String, DirectoryOwner> = HashMap::new();
        let mut project_by_root: HashMap<String, &ConfiguredProject> = HashMap::new();
        let desktop = inputs.surface == Surface::Desktop;

        for project in inputs.projects {
            let Some(root) = path::normalize(&project.path) else {
                continue;
            };
            let keep_existing = project_by_root
                .get(&root)
                .is_some_and(|existing| existing.id <= project.id);
            if !keep_existing {
                project_by_root.insert(root.clone(), project);
            }
            register(
                &mut by_directory,
                DirectoryOwner {
                    project_id: project.id.clone(),
                    project_root: root.clone(),
                    scope_directory: root,
                    kind: OwnerKind::Project,
                },
            );
        }

        if desktop {
            for entry in inputs.worktrees {
                let Some(root) = path::normalize(&entry.project_path) else {
                    continue;
                };
                let Some(project) = project_by_root.get(&root) else {
                    continue;
                };
                for worktree in &entry.worktrees {
                    let Some(directory) = path::normalize(&worktree.path) else {
                        continue;
                    };
                    register(
                        &mut by_directory,
                        DirectoryOwner {
                            project_id: project.id.clone(),
                            project_root: root.clone(),
                            scope_directory: directory,
                            kind: OwnerKind::Worktree,
                        },
                    );
                }
            }

            // A space whose project is not registered here owns nothing.
            for space in inputs.spaces {
                let (Some(root), Some(directory)) = (
                    path::normalize_optional(space.project_directory.as_deref()),
                    path::normalize_optional(space.directory.as_deref()),
                ) else {
                    continue;
                };
                let Some(project) = project_by_root.get(&root) else {
                    continue;
                };
                register(
                    &mut by_directory,
                    DirectoryOwner {
                        project_id: project.id.clone(),
                        project_root: root,
                        scope_directory: directory,
                        kind: OwnerKind::Space(space.id.clone()),
                    },
                );
            }
        }

        let mut canonical = CanonicalOwners::new(&project_by_root);
        let mut authoritative_ids: HashSet<&OpenCodeProjectId> = HashSet::new();
        for project in inputs.authoritative_projects {
            if project.id.as_str().is_empty() {
                continue;
            }
            authoritative_ids.insert(&project.id);
            canonical.register(&project.id, project.worktree.as_deref());
        }
        let all_sessions = inputs.sessions.iter().chain(inputs.archived_sessions);
        for session in all_sessions {
            let Some(project) = &session.project else {
                continue;
            };
            let Some(id) = project.id.as_ref().filter(|id| !id.as_str().is_empty()) else {
                continue;
            };
            if authoritative_ids.contains(id) {
                continue;
            }
            let configured_root = path::normalize_optional(project.worktree.as_deref())
                .is_some_and(|root| project_by_root.contains_key(&root));
            if configured_root {
                canonical.register(id, project.worktree.as_deref());
            }
        }

        Self {
            surface: inputs.surface,
            by_directory,
            canonical_by_opencode_project: canonical.owners,
            resolved: HashMap::new(),
        }
    }

    fn owner_of_session(&mut self, session: &SessionRecord) -> Option<DirectoryOwner> {
        let exact = session
            .resolved_directory()
            .and_then(|directory| self.owner_of_directory(directory));
        if exact.is_some() || self.surface == Surface::VsCode {
            return exact;
        }
        let project_id = session.opencode_project_id()?;
        self.canonical_by_opencode_project
            .get(project_id)
            .cloned()
            .flatten()
    }

    fn owner_of_directory(&mut self, directory: String) -> Option<DirectoryOwner> {
        if let Some(owner) = self.resolved.get(&directory) {
            return owner.clone();
        }
        if self.surface == Surface::VsCode {
            let owner = self.by_directory.get(&directory).cloned();
            self.resolved.insert(directory, owner.clone());
            return owner;
        }

        let mut visited: Vec<String> = Vec::new();
        let mut current = Some(directory);
        let mut owner = None;
        while let Some(candidate) = current {
            if let Some(memoized) = self.resolved.get(&candidate) {
                owner = memoized.clone();
                break;
            }
            if let Some(found) = self.by_directory.get(&candidate) {
                owner = Some(found.clone());
                visited.push(candidate);
                break;
            }
            current = path::parent(&candidate);
            visited.push(candidate);
        }
        for directory in visited {
            self.resolved.insert(directory, owner.clone());
        }
        owner
    }
}

fn register(owners: &mut HashMap<String, DirectoryOwner>, candidate: DirectoryOwner) {
    let directory = candidate.scope_directory.clone();
    let replace = owners
        .get(&directory)
        .is_none_or(|existing| existing.is_replaced_by(&candidate));
    if replace {
        owners.insert(directory, candidate);
    }
}

/// Bridges OpenCode project IDs to configured roots through canonical worktrees.
///
/// Only metadata whose canonical worktree is itself a configured root can
/// bridge the two namespaces. Conflicting metadata stays unresolved, and the
/// catch-all `global` project never bridges: every session whose directory
/// resolves to no configured root shares it.
struct CanonicalOwners<'p> {
    project_by_root: &'p HashMap<String, &'p ConfiguredProject>,
    owners: HashMap<OpenCodeProjectId, Option<DirectoryOwner>>,
}

impl<'p> CanonicalOwners<'p> {
    fn new(project_by_root: &'p HashMap<String, &'p ConfiguredProject>) -> Self {
        Self {
            project_by_root,
            owners: HashMap::new(),
        }
    }

    fn register(&mut self, id: &OpenCodeProjectId, worktree: Option<&str>) {
        if id.as_str() == "global" {
            return;
        }
        let candidate = path::normalize_optional(worktree).and_then(|root| {
            self.project_by_root
                .get(&root)
                .map(|project| DirectoryOwner {
                    project_id: project.id.clone(),
                    project_root: root.clone(),
                    scope_directory: root,
                    kind: OwnerKind::Project,
                })
        });
        match self.owners.get_mut(id) {
            None => {
                self.owners.insert(id.clone(), candidate);
            }
            Some(existing) => {
                let agrees = matches!(
                    (&*existing, &candidate),
                    (Some(existing), Some(candidate)) if existing.project_root == candidate.project_root
                );
                if !agrees {
                    *existing = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_projects_are_expanded_normalized_and_unique() {
        let projects = configured_projects(
            [
                "~/work/app/",
                "",
                "/srv/api",
                "/home/me/work/app",
                "  ",
                "/srv/api/",
            ],
            Some("/home/me"),
        );
        let paths: Vec<&str> = projects
            .iter()
            .map(|project| project.path.as_str())
            .collect();
        assert_eq!(paths, ["/home/me/work/app", "/srv/api"]);
        assert_eq!(
            projects[0].id,
            ProjectId::from_path("/home/me/work/app").expect("valid path")
        );
    }

    #[test]
    fn linked_worktrees_leave_out_the_main_checkout() {
        let listing = vec![
            WorktreeInfo {
                branch: Some("main".to_owned()),
                ..WorktreeInfo::new("/repo")
            },
            WorktreeInfo {
                branch: Some("refs/heads/zeta".to_owned()),
                ..WorktreeInfo::new("/repo-trees/z/")
            },
            WorktreeInfo::new("/repo-trees/Alpha"),
        ];
        let worktrees = linked_worktrees("/repo/", listing);
        let labels: Vec<Option<&str>> = worktrees
            .iter()
            .map(|worktree| worktree.label.as_deref())
            .collect();
        assert_eq!(labels, [Some("Alpha"), Some("zeta")]);
        assert_eq!(worktrees[1].path, "/repo-trees/z");
    }
    use crate::session::SessionProjectMetadata;

    fn project(id: &str, path: &str) -> ConfiguredProject {
        ConfiguredProject {
            id: ProjectId::from(id),
            path: path.to_owned(),
        }
    }

    #[test]
    fn matches_a_directory_inside_a_project_root() {
        let sessions = [SessionRecord::new("s1", "/projects/app/src")];
        let projects = [project("app", "/projects/app")];
        let index = SessionOwnershipIndex::build(&OwnershipInputs {
            sessions: &sessions,
            projects: &projects,
            ..OwnershipInputs::default()
        });
        let owner = index.owner(&SessionId::from("s1")).expect("owned by app");
        assert_eq!(owner.project_id, ProjectId::from("app"));
        assert_eq!(owner.scope_directory, "/projects/app");
    }

    #[test]
    fn prefers_the_project_root_over_a_worktree_registered_at_the_same_path() {
        let sessions = [SessionRecord::new("s1", "/w/feature")];
        let projects = [
            project("app", "/projects/app"),
            project("feature", "/w/feature"),
        ];
        let worktrees = [ProjectWorktrees {
            project_path: "/projects/app".to_owned(),
            worktrees: vec![WorktreeInfo::new("/w/feature")],
        }];
        let index = SessionOwnershipIndex::build(&OwnershipInputs {
            sessions: &sessions,
            projects: &projects,
            worktrees: &worktrees,
            ..OwnershipInputs::default()
        });
        let owner = index.owner(&SessionId::from("s1")).expect("owned");
        assert_eq!(owner.project_id, ProjectId::from("feature"));
        assert_eq!(owner.kind, OwnerKind::Project);
    }

    #[test]
    fn vs_code_matches_exact_directories_only() {
        let sessions = [
            SessionRecord::new("exact", "/projects/app"),
            SessionRecord::new("nested", "/projects/app/src"),
        ];
        let projects = [project("app", "/projects/app")];
        let index = SessionOwnershipIndex::build(&OwnershipInputs {
            sessions: &sessions,
            projects: &projects,
            surface: Surface::VsCode,
            ..OwnershipInputs::default()
        });
        assert!(index.is_owned(&SessionId::from("exact")));
        assert!(!index.is_owned(&SessionId::from("nested")));
    }

    #[test]
    fn the_global_project_never_bridges_to_a_configured_root() {
        let mut session = SessionRecord::new("s1", "/elsewhere");
        session.project = Some(SessionProjectMetadata {
            id: Some(OpenCodeProjectId::from("global")),
            worktree: Some("/projects/app".to_owned()),
        });
        let sessions = [session];
        let projects = [project("app", "/projects/app")];
        let authoritative = [OpenCodeProject {
            id: OpenCodeProjectId::from("global"),
            worktree: Some("/projects/app".to_owned()),
        }];
        let index = SessionOwnershipIndex::build(&OwnershipInputs {
            sessions: &sessions,
            projects: &projects,
            authoritative_projects: &authoritative,
            ..OwnershipInputs::default()
        });
        assert!(!index.is_owned(&SessionId::from("s1")));
    }

    #[test]
    fn conflicting_canonical_metadata_stays_unresolved() {
        let authoritative = [
            OpenCodeProject {
                id: OpenCodeProjectId::from("oc"),
                worktree: Some("/projects/app".to_owned()),
            },
            OpenCodeProject {
                id: OpenCodeProjectId::from("oc"),
                worktree: Some("/projects/other".to_owned()),
            },
        ];
        let mut session = SessionRecord::new("s1", "/tmp/x");
        session.project_id = Some(OpenCodeProjectId::from("oc"));
        let sessions = [session];
        let projects = [
            project("app", "/projects/app"),
            project("other", "/projects/other"),
        ];
        let index = SessionOwnershipIndex::build(&OwnershipInputs {
            sessions: &sessions,
            projects: &projects,
            authoritative_projects: &authoritative,
            ..OwnershipInputs::default()
        });
        assert!(!index.is_owned(&SessionId::from("s1")));
    }
}
