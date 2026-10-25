//! Replays Works membership and ordering captured from the web implementation.

use std::collections::{BTreeMap, HashSet};

use openchamber_native_core::grouping::{
    DraftTarget, GroupLabel, ProjectSection, SessionGroup, SessionNode,
};
use openchamber_native_core::ids::{ProjectId, SessionId, SpaceId};
use openchamber_native_core::ownership::ConfiguredProject;
use openchamber_native_core::session::SessionRecord;
use openchamber_native_core::sidebar_work::SidebarWorkIndex;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    work_scenarios: Vec<Scenario>,
}

#[derive(Deserialize)]
struct Scenario {
    name: String,
    input: Input,
    expected: Expected,
}

#[derive(Deserialize)]
struct Input {
    projects: Vec<Project>,
    order: Vec<String>,
    include: Vec<String>,
}

#[derive(Deserialize)]
struct Project {
    id: String,
    groups: Vec<Group>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Root,
    Worktree,
    Space,
    Archive,
}

#[derive(Deserialize)]
struct Group {
    id: String,
    kind: Kind,
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    #[serde(default)]
    children: Vec<Node>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Expected {
    by_session: BTreeMap<String, String>,
    worktrees: Vec<String>,
    ordered: Vec<String>,
}

fn collect(node: &Node, records: &mut BTreeMap<String, SessionRecord>) {
    records
        .entry(node.id.clone())
        .or_insert_with(|| SessionRecord::new(node.id.as_str(), "/fixture"));
    for child in &node.children {
        collect(child, records);
    }
}

fn node<'a>(source: &Node, records: &'a BTreeMap<String, SessionRecord>) -> SessionNode<'a> {
    SessionNode {
        session: records
            .get(&source.id)
            .expect("fixture nodes were collected"),
        children: source
            .children
            .iter()
            .map(|child| node(child, records))
            .collect(),
    }
}

#[test]
fn works_match_reference_membership_and_order() {
    let fixture: Fixture = serde_json::from_str(include_str!("fixtures/sidebar-projection.json"))
        .expect("generated reference fixtures parse");
    assert!(
        !fixture.work_scenarios.is_empty(),
        "Works scenarios must be present"
    );
    for scenario in fixture.work_scenarios {
        let mut records = BTreeMap::new();
        for project in &scenario.input.projects {
            for group in &project.groups {
                for source in &group.nodes {
                    collect(source, &mut records);
                }
            }
        }
        for id in &scenario.input.order {
            records
                .entry(id.clone())
                .or_insert_with(|| SessionRecord::new(id.as_str(), "/fixture"));
        }
        let projects: Vec<ConfiguredProject> = scenario
            .input
            .projects
            .iter()
            .map(|project| ConfiguredProject {
                id: ProjectId::from(project.id.as_str()),
                path: format!("/projects/{}", project.id),
            })
            .collect();
        let sections: Vec<ProjectSection<'_>> = scenario
            .input
            .projects
            .iter()
            .zip(&projects)
            .map(|(source, project)| ProjectSection {
                project,
                groups: source
                    .groups
                    .iter()
                    .map(|group| SessionGroup {
                        id: group.id.clone(),
                        label: match group.kind {
                            Kind::Root => GroupLabel::ProjectRoot { branch: None },
                            Kind::Worktree => GroupLabel::Worktree {
                                label: group.id.clone(),
                            },
                            Kind::Space => GroupLabel::Space {
                                name: Some(group.id.clone()),
                            },
                            Kind::Archive => GroupLabel::Archived,
                        },
                        directory: Some(format!("{}/{}", project.path, group.id)),
                        is_main: matches!(group.kind, Kind::Root),
                        is_archived_bucket: matches!(group.kind, Kind::Archive),
                        space: matches!(group.kind, Kind::Space)
                            .then(|| SpaceId::from(group.id.as_str())),
                        draft_target: DraftTarget::Project,
                        sessions: group
                            .nodes
                            .iter()
                            .map(|source| node(source, &records))
                            .collect(),
                    })
                    .collect(),
            })
            .collect();
        let index = SidebarWorkIndex::new(&sections);
        let order: Vec<&SessionRecord> =
            scenario.input.order.iter().map(|id| &records[id]).collect();
        let included: HashSet<&str> = scenario.input.include.iter().map(String::as_str).collect();
        let actual = Expected {
            by_session: records
                .keys()
                .filter_map(|id| {
                    index
                        .work_for(&SessionId::from(id.as_str()))
                        .map(|work| (id.clone(), work.key()))
                })
                .collect(),
            worktrees: index.worktrees().map(|work| work.key()).collect(),
            ordered: index
                .ordered(&order, |session| included.contains(session.id.as_str()))
                .iter()
                .map(|work| work.key())
                .collect(),
        };
        assert_eq!(actual, scenario.expected, "{}", scenario.name);
    }
}
