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
