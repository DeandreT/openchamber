//! Replays the TypeScript reference projection's recorded scenarios.
//!
//! The fixture comes from `packages/ui/src/lib/native/sidebarParityFixtures.ts`
//! (`bun run native:fixtures`); its own test fails when the fixture is stale.

use std::collections::BTreeMap;

use openchamber_native_core::chat_projection;
use openchamber_native_core::chat_roots::ChatRoots;
use openchamber_native_core::ids::{OpenCodeProjectId, ProjectId, SessionId, SpaceId};
use openchamber_native_core::ownership::{
    ConfiguredProject, DirectoryOwner, OpenCodeProject, OwnerKind, OwnershipInputs,
    ProjectWorktrees, SessionOwnershipIndex, SpaceMark, Surface, WorktreeInfo,
};
use openchamber_native_core::session::{
    SessionKind, SessionOverlay, SessionProjectMetadata, SessionRecord, SessionTime,
};
use serde::Deserialize;

const FIXTURE: &str = include_str!("fixtures/sidebar-projection.json");

#[derive(Deserialize)]
struct Fixture {
    home: String,
    scenarios: Vec<Scenario>,
}

#[derive(Deserialize)]
struct Scenario {
    name: String,
    input: Input,
    expected: Expected,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    surface: String,
    sessions: Vec<FixtureSession>,
    archived_sessions: Vec<FixtureSession>,
    projects: Vec<FixtureProject>,
    worktrees: Vec<FixtureWorktrees>,
    authoritative_projects: Vec<FixtureOpenCodeProject>,
    spaces: Vec<FixtureSpace>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureSession {
    id: String,
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
    #[serde(rename = "projectID")]
    project_id: Option<String>,
    directory: Option<String>,
    project: Option<FixtureSessionProject>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    btw: bool,
}

#[derive(Deserialize)]
struct FixtureSessionProject {
    id: Option<String>,
    worktree: Option<String>,
}

#[derive(Deserialize)]
struct FixtureProject {
    id: String,
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureWorktrees {
    project_path: String,
    worktree_paths: Vec<String>,
}

#[derive(Deserialize)]
struct FixtureOpenCodeProject {
    id: String,
    worktree: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureSpace {
    id: String,
    directory: Option<String>,
    project_directory: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Expected {
    owners: BTreeMap<String, ExpectedOwner>,
    sessions_by_project: BTreeMap<String, Vec<String>>,
    archived_sessions_by_project: BTreeMap<String, Vec<String>>,
    sessions_by_scope: BTreeMap<String, Vec<String>>,
    directory_resolutions: usize,
    chat_sessions: Vec<String>,
    project_sessions: Vec<String>,
    project_sessions_by_project: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ExpectedOwner {
    project_id: String,
    project_root: String,
    scope_directory: String,
    kind: String,
    space_id: Option<String>,
}

impl From<&DirectoryOwner> for ExpectedOwner {
    fn from(owner: &DirectoryOwner) -> Self {
        let (kind, space_id) = match &owner.kind {
            OwnerKind::Project => ("project", None),
            OwnerKind::Worktree => ("worktree", None),
            OwnerKind::Space(id) => ("space", Some(id.as_str().to_owned())),
        };
        Self {
            project_id: owner.project_id.as_str().to_owned(),
            project_root: owner.project_root.clone(),
            scope_directory: owner.scope_directory.clone(),
            kind: kind.to_owned(),
            space_id,
        }
    }
}

fn session(fixture: &FixtureSession) -> SessionRecord {
    SessionRecord {
        id: SessionId::from(fixture.id.as_str()),
        parent_id: fixture.parent_id.as_deref().map(SessionId::from),
        directory: fixture.directory.clone(),
        title: fixture.id.clone(),
        time: SessionTime {
            created: 1,
            updated: 1,
            idle: None,
            archived: fixture.archived.then_some(2),
        },
        project_id: fixture.project_id.as_deref().map(OpenCodeProjectId::from),
        project: fixture
            .project
            .as_ref()
            .map(|project| SessionProjectMetadata {
                id: project.id.as_deref().map(OpenCodeProjectId::from),
                worktree: project.worktree.clone(),
            }),
        kind: if fixture.btw {
            SessionKind::Btw
        } else {
            SessionKind::Conversation
        },
        overlay: SessionOverlay::default(),
    }
}

fn ids(sessions: &[&SessionRecord]) -> Vec<String> {
    sessions
        .iter()
        .map(|session| session.id.as_str().to_owned())
        .collect()
}

fn run(input: &Input, roots: &ChatRoots) -> Expected {
    let sessions: Vec<SessionRecord> = input.sessions.iter().map(session).collect();
    let archived_sessions: Vec<SessionRecord> =
        input.archived_sessions.iter().map(session).collect();
    let projects: Vec<ConfiguredProject> = input
        .projects
        .iter()
        .map(|project| ConfiguredProject {
            id: ProjectId::from(project.id.as_str()),
            path: project.path.clone(),
        })
        .collect();
    let worktrees: Vec<ProjectWorktrees> = input
        .worktrees
        .iter()
        .map(|entry| ProjectWorktrees {
            project_path: entry.project_path.clone(),
            worktrees: entry.worktree_paths.iter().map(WorktreeInfo::new).collect(),
        })
        .collect();
    let authoritative_projects: Vec<OpenCodeProject> = input
        .authoritative_projects
        .iter()
        .map(|project| OpenCodeProject {
            id: OpenCodeProjectId::from(project.id.as_str()),
            worktree: project.worktree.clone(),
        })
        .collect();
    let spaces: Vec<SpaceMark> = input
        .spaces
        .iter()
        .map(|space| SpaceMark {
            id: SpaceId::from(space.id.as_str()),
            directory: space.directory.clone(),
            project_directory: space.project_directory.clone(),
        })
        .collect();
    let surface = match input.surface.as_str() {
        "vscode" => Surface::VsCode,
        "desktop" => Surface::Desktop,
        other => panic!("fixture surface must be desktop or vscode, got {other}"),
    };

    let ownership = SessionOwnershipIndex::build(&OwnershipInputs {
        sessions: &sessions,
        archived_sessions: &archived_sessions,
        projects: &projects,
        worktrees: &worktrees,
        authoritative_projects: &authoritative_projects,
        spaces: &spaces,
        surface,
    });
    let chats = chat_projection::project(&sessions, &ownership, Some(roots), surface);

    let all_sessions = sessions.iter().chain(&archived_sessions);
    let owners = all_sessions
        .filter_map(|session| {
            let owner = ownership.owner(&session.id)?;
            Some((session.id.as_str().to_owned(), ExpectedOwner::from(owner)))
        })
        .collect();
    let bucket_ids = |buckets: &std::collections::HashMap<ProjectId, Vec<&SessionRecord>>| {
        buckets
            .iter()
            .map(|(id, bucket)| (id.as_str().to_owned(), ids(bucket)))
            .collect()
    };
    let archived_sessions_by_project = projects
        .iter()
        .filter_map(|project| {
            let bucket = ownership.archived_project_sessions(&project.id);
            (!bucket.is_empty()).then(|| (project.id.as_str().to_owned(), ids(bucket)))
        })
        .collect();
    let sessions_by_scope = ownership
        .sessions_by_scope()
        .iter()
        .map(|(scope, members)| {
            let mut members: Vec<String> =
                members.iter().map(|id| id.as_str().to_owned()).collect();
            members.sort();
            (scope.clone(), members)
        })
        .collect();

    Expected {
        owners,
        sessions_by_project: bucket_ids(ownership.sessions_by_project()),
        archived_sessions_by_project,
        sessions_by_scope,
        directory_resolutions: ownership.directory_resolutions(),
        chat_sessions: ids(&chats.chat_sessions),
        project_sessions: ids(&chats.project_sessions),
        project_sessions_by_project: bucket_ids(&chats.project_sessions_by_project),
    }
}

#[test]
fn native_projection_matches_every_reference_scenario() {
    let fixture: Fixture = serde_json::from_str(FIXTURE).expect("fixture is valid JSON");
    let roots = ChatRoots::from_home_info(Some(&fixture.home), None, None, None)
        .expect("fixture home is usable");
    assert!(
        fixture.scenarios.len() >= 10,
        "the fixture must cover the documented scenario set"
    );
    for scenario in &fixture.scenarios {
        let actual = run(&scenario.input, &roots);
        assert_eq!(
            actual, scenario.expected,
            "scenario `{}` diverged",
            scenario.name
        );
    }
}
