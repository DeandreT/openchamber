// Copyright 2026 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

use crate::{
    Adapter, AdapterCallback, AppContext, Event, InterfaceSet, NodeIdOrRoot, ObjectEvent,
    PlatformNode, State, StateSet, WindowBounds,
};
use accesskit::{
    Action, ActionHandler, ActionRequest, Node, NodeId, Role, Tree, TreeId, TreeUpdate,
};
use std::sync::{Arc, Mutex};

struct EventCollector(Arc<Mutex<Vec<Event>>>);

impl AdapterCallback for EventCollector {
    fn register_interfaces(&self, _: &Adapter, _: accesskit_consumer::NodeId, _: InterfaceSet) {}
    fn unregister_interfaces(&self, _: &Adapter, _: accesskit_consumer::NodeId, _: InterfaceSet) {}
    fn emit_event(&self, _: &Adapter, event: Event) {
        self.0
            .lock()
            .expect("event collector is not poisoned")
            .push(event);
    }
}

struct NoOpActionHandler;

#[test]
fn keyboard_focus_transitions_between_entry_and_its_active_descendant() {
    let mut window = Node::new(Role::Window);
    window.set_children(vec![NodeId(1), NodeId(2)]);
    let mut entry = Node::new(Role::TextInput);
    entry.add_action(Action::Focus);
    entry.set_active_descendant(NodeId(2));
    let mut option = Node::new(Role::ListBoxOption);
    option.add_action(Action::Focus);
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut adapter = Adapter::new(
        &AppContext::new(None),
        EventCollector(events.clone()),
        TreeUpdate {
            nodes: vec![(NodeId(0), window), (NodeId(1), entry), (NodeId(2), option)],
            tree: Some(Tree::new(NodeId(0))),
            tree_id: TreeId::ROOT,
            focus: NodeId(1),
        },
        true,
        WindowBounds::default(),
        NoOpActionHandler,
    );
    let root = adapter.platform_node(adapter.root_id());
    let entry = adapter.platform_node(root.child_at_index(0).expect("root").expect("entry"));
    let option = adapter.platform_node(root.child_at_index(1).expect("root").expect("option"));
    for (focus, gained, lost) in [(NodeId(2), &option, &entry), (NodeId(1), &entry, &option)] {
        events.lock().expect("events").clear();
        adapter.update(TreeUpdate {
            nodes: vec![],
            tree: None,
            tree_id: TreeId::ROOT,
            focus,
        });
        assert!(gained.state().contains(State::Focused));
        assert!(!lost.state().contains(State::Focused));
        let transitions: Vec<_> = events
            .lock()
            .expect("events")
            .iter()
            .filter_map(|event| match event {
                Event::Object {
                    target: NodeIdOrRoot::Node(id),
                    event: ObjectEvent::StateChanged(State::Focused, focused),
                } => Some((*id, *focused)),
                _ => None,
            })
            .collect();
        assert_eq!(
            transitions,
            [(gained.id(), true), (lost.id(), false)],
            "actual keyboard owner changes emit both transitions despite unchanged resolved focus"
        );
        events.lock().expect("events").clear();
        adapter.update(TreeUpdate {
            nodes: vec![],
            tree: None,
            tree_id: TreeId::ROOT,
            focus,
        });
        assert!(
            !events.lock().expect("events").iter().any(|event| matches!(
                event,
                Event::Object {
                    event: ObjectEvent::StateChanged(State::Focused, _),
                    ..
                }
            )),
            "unchanged owner emits no duplicate focus transitions"
        );
    }
}

#[test]
fn editable_active_descendant_keeps_keyboard_focus_on_entry() {
    let mut window = Node::new(Role::Window);
    window.set_children(vec![NodeId(1), NodeId(2), NodeId(3)]);
    let mut input = Node::new(Role::TextInput);
    input.set_label("Search agents");
    input.add_action(Action::Focus);
    input.set_active_descendant(NodeId(2));
    let mut option = Node::new(Role::ListBoxOption);
    option.set_label("build");
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut adapter = Adapter::new(
        &AppContext::new(None),
        EventCollector(events.clone()),
        TreeUpdate {
            nodes: vec![
                (NodeId(0), window),
                (NodeId(1), input.clone()),
                (NodeId(2), option.clone()),
                (NodeId(3), option),
            ],
            tree: Some(Tree::new(NodeId(0))),
            tree_id: TreeId::ROOT,
            focus: NodeId(1),
        },
        true,
        WindowBounds::default(),
        NoOpActionHandler,
    );
    let root = adapter.platform_node(adapter.root_id());
    let entry = adapter.platform_node(root.child_at_index(0).expect("root").expect("entry"));
    let first = adapter.platform_node(root.child_at_index(1).expect("root").expect("first option"));
    let second = adapter.platform_node(
        root.child_at_index(2)
            .expect("root")
            .expect("second option"),
    );
    assert!(
        !first.state().contains(State::Focused),
        "the active option is not a second focus target"
    );
    assert!(
        !first
            .cache_node()
            .expect("option cache")
            .states
            .contains(State::Focused)
    );
    assert!(!second.state().contains(State::Focused));
    assert!(
        events
            .lock()
            .expect("events")
            .iter()
            .filter_map(|event| match event {
                Event::Object {
                    target: NodeIdOrRoot::Node(id),
                    event: ObjectEvent::StateChanged(State::Focused, true),
                } => Some(*id),
                _ => None,
            })
            .eq([entry.id()]),
        "initial focus notification names only the entry"
    );
    assert!(
        entry.state().contains(State::Focused),
        "active descendant does not remove keyboard focus from the editable entry"
    );
    assert!(
        entry
            .cache_node()
            .expect("cache")
            .states
            .contains(State::Focused)
    );
    events.lock().expect("events").clear();
    input.set_active_descendant(NodeId(3));
    adapter.update(TreeUpdate {
        nodes: vec![(NodeId(1), input.clone())],
        tree: None,
        tree_id: TreeId::ROOT,
        focus: NodeId(1),
    });
    {
        let recorded = events.lock().expect("events");
        assert!(recorded.iter().any(|event| matches!(event,
            Event::Object { target: NodeIdOrRoot::Node(id), event: ObjectEvent::ActiveDescendantChanged(child) }
                if *id == entry.id() && *child == second.id()
        )), "arrow navigation announces the active descendant on the entry");
        assert!(
            !recorded.iter().any(|event| matches!(
                event,
                Event::Object {
                    event: ObjectEvent::StateChanged(State::Focused, _),
                    ..
                }
            )),
            "highlight changes do not move focus"
        );
    }
    assert!(entry.state().contains(State::Focused));
    assert!(!first.state().contains(State::Focused));
    assert!(!second.state().contains(State::Focused));
    input.clear_active_descendant();
    events.lock().expect("events").clear();
    adapter.update(TreeUpdate {
        nodes: vec![(NodeId(1), input)],
        tree: None,
        tree_id: TreeId::ROOT,
        focus: NodeId(1),
    });
    assert!(
        entry.state().contains(State::Focused),
        "empty results retain entry focus"
    );
    assert!(
        !events.lock().expect("events").iter().any(|event| matches!(
            event,
            Event::Object {
                event: ObjectEvent::StateChanged(State::Focused, _),
                ..
            }
        )),
        "clearing the highlight does not blur the entry"
    );
    adapter.update_window_focus_state(false);
    assert!(
        !entry.state().contains(State::Focused),
        "inactive windows expose no focused entry"
    );
    adapter.update_window_focus_state(true);
    for focused in [false, true] {
        assert!(events.lock().expect("events").iter().any(|event| matches!(event,
            Event::Object { target: NodeIdOrRoot::Node(id), event: ObjectEvent::StateChanged(State::Focused, value) }
                if *id == entry.id() && *value == focused
        )), "entry receives host focus transitions");
    }
}

#[test]
fn ordinary_editors_and_noneditable_active_descendants_keep_existing_focus_behavior() {
    for (role, active_descendant) in [
        (Role::TextInput, false),
        (Role::MultilineTextInput, false),
        (Role::ListBox, true),
    ] {
        let mut window = Node::new(Role::Window);
        window.set_children(vec![NodeId(1), NodeId(2)]);
        let mut owner = Node::new(role);
        owner.add_action(Action::Focus);
        if active_descendant {
            owner.set_active_descendant(NodeId(2));
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let adapter = Adapter::new(
            &AppContext::new(None),
            EventCollector(events.clone()),
            TreeUpdate {
                nodes: vec![
                    (NodeId(0), window),
                    (NodeId(1), owner),
                    (NodeId(2), Node::new(Role::ListBoxOption)),
                ],
                tree: Some(Tree::new(NodeId(0))),
                tree_id: TreeId::ROOT,
                focus: NodeId(1),
            },
            true,
            WindowBounds::default(),
            NoOpActionHandler,
        );
        let root = adapter.platform_node(adapter.root_id());
        let owner = adapter.platform_node(root.child_at_index(0).expect("root").expect("owner"));
        let option = adapter.platform_node(root.child_at_index(1).expect("root").expect("option"));
        assert_eq!(
            owner.state().contains(State::Focused),
            !active_descendant,
            "{role:?}"
        );
        assert_eq!(
            option.state().contains(State::Focused),
            active_descendant,
            "{role:?}"
        );
        let expected = if active_descendant {
            option.id()
        } else {
            owner.id()
        };
        assert!(
            events
                .lock()
                .expect("events")
                .iter()
                .filter_map(|event| match event {
                    Event::Object {
                        target: NodeIdOrRoot::Node(id),
                        event: ObjectEvent::StateChanged(State::Focused, true),
                    } => Some(*id),
                    _ => None,
                })
                .eq([expected]),
            "{role:?} retains its original focus notification"
        );
    }
}

impl ActionHandler for NoOpActionHandler {
    fn do_action(&mut self, _: ActionRequest) {}
}

fn node(role: Role, expanded: Option<bool>) -> Node {
    let mut node = Node::new(role);
    node.add_action(Action::Focus);
    node.set_required();
    if let Some(expanded) = expanded {
        node.set_expanded(expanded);
    }
    node
}

fn build(node: Node) -> (Adapter, PlatformNode, Arc<Mutex<Vec<Event>>>) {
    let mut window = Node::new(Role::Window);
    window.set_children(vec![NodeId(1)]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let adapter = Adapter::new(
        &AppContext::new(None),
        EventCollector(Arc::clone(&events)),
        TreeUpdate {
            nodes: vec![(NodeId(0), window), (NodeId(1), node)],
            tree: Some(Tree::new(NodeId(0))),
            tree_id: TreeId::ROOT,
            focus: NodeId(0),
        },
        false,
        WindowBounds::default(),
        NoOpActionHandler,
    );
    let child_id = adapter
        .platform_node(adapter.root_id())
        .child_at_index(0)
        .expect("window resolves")
        .expect("window has a child");
    let child = adapter.platform_node(child_id);
    events
        .lock()
        .expect("event collector is not poisoned")
        .clear();
    (adapter, child, events)
}

fn assert_states(node: &PlatformNode, expansion: StateSet) {
    let mut expected = expansion;
    expected.insert(
        State::Enabled
            | State::Sensitive
            | State::Visible
            | State::Showing
            | State::Focusable
            | State::Required,
    );
    assert_eq!(
        node.state(),
        expected,
        "state query preserves unrelated states"
    );
    assert_eq!(
        node.cache_node().expect("node resolves").states,
        expected,
        "cache and state query agree"
    );
}

#[test]
fn initial_expanded_states() {
    for role in [Role::TreeItem, Role::Button, Role::DisclosureTriangle] {
        for (expanded, expected) in [
            (None, StateSet::empty()),
            (
                Some(false),
                StateSet::new(State::Expandable | State::Collapsed),
            ),
            (
                Some(true),
                StateSet::new(State::Expandable | State::Expanded),
            ),
        ] {
            let (_adapter, child, _events) = build(node(role, expanded));
            assert_states(&child, expected);
        }
    }
}

#[test]
fn expanded_state_transitions() {
    // Explicit expectations cover the six transitions and all three unchanged updates.
    let cases: &[(Option<bool>, Option<bool>, &[(State, bool)])] = &[
        (
            None,
            Some(false),
            &[(State::Expandable, true), (State::Collapsed, true)],
        ),
        (
            None,
            Some(true),
            &[(State::Expandable, true), (State::Expanded, true)],
        ),
        (
            Some(false),
            Some(true),
            &[(State::Collapsed, false), (State::Expanded, true)],
        ),
        (
            Some(true),
            Some(false),
            &[(State::Expanded, false), (State::Collapsed, true)],
        ),
        (
            Some(false),
            None,
            &[(State::Expandable, false), (State::Collapsed, false)],
        ),
        (
            Some(true),
            None,
            &[(State::Expandable, false), (State::Expanded, false)],
        ),
        (None, None, &[]),
        (Some(false), Some(false), &[]),
        (Some(true), Some(true), &[]),
    ];
    for &(before, after, expected) in cases {
        let mut data = node(Role::TreeItem, before);
        let (mut adapter, child, events) = build(data.clone());
        match after {
            Some(expanded) => data.set_expanded(expanded),
            None => data.clear_expanded(),
        }
        adapter.update(TreeUpdate {
            nodes: vec![(NodeId(1), data)],
            tree: None,
            tree_id: TreeId::ROOT,
            focus: NodeId(0),
        });
        let states = match after {
            None => StateSet::empty(),
            Some(false) => StateSet::new(State::Expandable | State::Collapsed),
            Some(true) => StateSet::new(State::Expandable | State::Expanded),
        };
        assert_states(&child, states);
        let events = events.lock().expect("event collector is not poisoned");
        assert_eq!(
            events.len(),
            expected.len(),
            "{before:?} -> {after:?}: {events:?}"
        );
        for &(state, value) in expected {
            let count = events
                .iter()
                .filter(|event| {
                    matches!(event,
                        Event::Object { target: NodeIdOrRoot::Node(id),
                            event: ObjectEvent::StateChanged(actual_state, actual_value) }
                            if *id == child.id() && *actual_state == state && *actual_value == value
                    )
                })
                .count();
            assert_eq!(count, 1, "{before:?} -> {after:?}: {state:?}={value}");
        }
    }
}
