//! Accessibility support, provided by [AccessKit][accesskit].
//!
//! There are user-facing guide-level docs [here](crate::_accessibility).
//!
//! ## Architecture
//!
//! ```text
//!                              ┌────────────────────────────────┐   ┌─────────────────────┐
//!                           ┌─▶│ AccessKit Adapter (MacOS)      │◀─▶│ MacOS System APIs   │
//!                           │  └────────────────────────────────┘   └─────────────────────┘
//!                           │
//! ┌──────┐   ┌───────────┐  │  ┌────────────────────────────────┐   ┌─────────────────────┐
//! │ GPUI │◀─▶│ AccessKit │◀─┼─▶│ AccessKit Adapter (Windows)    │◀─▶│ Windows System APIs │
//! └──────┘   └───────────┘  │  └────────────────────────────────┘   └─────────────────────┘
//!                           │
//!                           │  ┌────────────────────────────────┐   ┌─────────────────────┐
//!                           └─▶│ AccessKit Adapter (Linux)      │◀─▶│ dbus                │
//!                              └────────────────────────────────┘   └─────────────────────┘
//! ```
//!
//! In order for GPUI apps to be usable for people using assistive technology,
//! we must do a few things:
//! - Inform the system when the UI changes meaningfully. This includes:
//!   - Reporting new/removed/changed UI elements
//!   - *Not* reporting irrelevant UI changes, e.g. an invisible `div()` being
//!     added.
//!   - Reporting the appearance and capabilities of each UI element. For example:
//!     - What does this piece of text say?
//!     - How far along is this progress bar?
//!     - Can this node be focused?
//!     - Can this node have a value directly assigned? (e.g. a slider)
//! - Allowing the system to interact with the UI by dispatching actions to
//!   nodes. Note that AccessKit has its own [`Action`] type, which is not the
//!   [`crate::Action`] trait.
//! - Activate and deactivate accessibility features when requested by the
//!   system.
//!
//! Activating and deactivating at the right time is trivial, so I won't go into
//! detail here. The other two are almost orthogonal in implementation.
//!
//! The state for both lives in the [`A11y`] struct in this module.
//!
//! ### Reporting UI changes
//!
//! Search editors can control sibling option rows through
//! `StatefulInteractiveElement::aria_active_descendant_for`. The row names the
//! editor's existing focus handle. At frame completion, the bridge resolves that
//! handle to the focused accessible node and sets its AccessKit active-descendant
//! relation. Keyboard focus remains on the editor; AccessKit's consumer resolves
//! platform focus to the option, including AT-SPI focused-state events.
//!
//! Claims are frame-local and require both nodes to be present. Absent, blurred,
//! self-referential or ambiguous claims emit no relation. Resolving after both
//! sibling subtrees render avoids a dependency on traversal order. The original
//! ancestor-based `aria_active_descendant` API keeps its existing behavior.
//!
//! Every frame, we build a [`TreeUpdate`] and send it to the platform-specific
//! adapter. A [`TreeUpdate`] is a representation of a subset of the UI tree.
//! When the adapter receives the update, it diffs it against the previous
//! update, and calls platform-specific APIs to inform screen readers about the
//! changes. Nodes may have been created, destroyed, or updated.
//!
//! Each node has an ID, and this ID *should* be stable across frames. If a
//! node's ID changes, then, from AccessKit's point of view, it is a different
//! node.
//!
//! We derive the node ID from the [`GlobalElementId`] in
//! [`GlobalElementId::accesskit_node_id`]. Nodes without [`GlobalElementId`]s
//! cannot produce an AccessKit [`NodeId`], and so are not included in the
//! accessibility tree. We try to warn when using accessibility APIs on
//! [`div()`] without setting an ID.
//!
//! This all happens in [`Drawable::prepaint`]. The [`A11y`] struct maintains a
//! stack of nodes during prepainting, which we can use to calculate the
//! [`NodeId`]s, and record parent-child relationships. Once all [`Element`]s in
//! a frame have been prepainted, we send the resulting [`TreeUpdate`] object to
//! the adapter and the screen reader can announce the changes.
//!
//! #### Synthetic children
//!
//! Additionally, some nodes can register "synthetic children" using
//! [`Element::a11y_synthetic_children`]. Normally, one accesskit node is pushed
//! for every [`Element`] with a role and id. However, sometimes a single
//! element may want to produce many accesskit nodes. These extra nodes are
//! referred to as "synthetic children" of the element providing a non-default
//! [`Element::a11y_synthetic_children`] implementation.
//!
//! The user is provided a builder-style API using [`A11ySubtreeBuilder`], which
//! allows them to create push nodes that are children of the current node, as
//! well as modify the current node itself.
//!
//! GPUI calls this callback *after* prepainting (and just before popping the
//! corresponding element), since this step may need prepaint information to be
//! available. In the future, we may want to add prepaint information more
//! generally to [`Element::write_a11y_info`], but for now that's not necessary.
//!
//! ### Responding to actions
//!
//! On adapter creation, we provide a callback to the adapter, which can be used
//! to dispatch actions. This callback forwards to [`A11y::action_listeners`], a
//! mapping from [`NodeId`]s to action handlers (basically just `Box<dyn
//! Fn()>`).
//!
//! This is populated in:
//! - [`Window::on_a11y_action`], which is called by:
//! - [`Interactivity::paint`], which is called by:
//! - [`StatefulInteractiveElement::on_a11y_action`], which is a public-facing API
//!
//! These are cleared at the start of a frame, and re-populated during painting.
//!
//! [`NodeId`]: accesskit::NodeId

use crate::*;

pub(crate) mod debug;

use crate::{App, Bounds, FocusId, Pixels, SharedString, Window};
use accesskit::{Action, NodeId, TreeUpdate};
use collections::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::hash::{Hash, Hasher};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

/// The fixed AccessKit node ID used for the root of every window's a11y tree.
pub(crate) const ROOT_NODE_ID: NodeId = NodeId(0);

/// An adapter event or an explicit same-window test policy injection.
///
/// Adapter events invoke the retained original callbacks without drawing or
/// pumping the scheduler. Policy injections are idle-only and preserve demand.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum A11yTestTransition {
    /// Invoke the original activation callback, including its flag write and
    /// notification. The driver discards its initial root-only handshake tree.
    Activate,
    /// Invoke the original deactivation callback, including its flag write and
    /// notification, without synchronizing the current frame's activity snapshot.
    Deactivate,
    /// While idle, force-disable this window's existing accessibility policy.
    /// Preserve requested adapter demand and refresh only if the policy changes.
    ForceDisable,
    /// While idle, clear this window's force-disabled accessibility policy.
    /// Preserve requested adapter demand and refresh only if the policy changes.
    ClearForceDisable,
}

/// Actual events and record visits during one drained observation interval.
///
/// Checked increments record real events and loop visits. Beginning observation
/// zeroes an epoch; each successful take drains its interval. Rollback never
/// subtracts discarded work. These counters measure neither total CPU work nor
/// total heap allocations.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct A11yLinkTestCounters {
    /// Entries into the observed window's actual `draw_roots` path, including draws
    /// that subsequently abort or send no platform tree.
    pub draws_started: u64,
    /// Draws reaching successful diagnostic commit after paint, frame bookkeeping
    /// and any applicable tree serialization/platform send have returned.
    /// Empty and inactive draws count; skipped or aborted draws do not.
    pub draws_accepted: u64,
    /// Newly created link registration nonce/record pairs, including discarded
    /// attempts. Cache reuse and marker writes do not create registrations.
    pub registrations: u64,
    /// Actual source-scope promotions into registered-link frame metadata.
    /// Each promotion counts once, including work later discarded by rollback.
    pub promoted_scopes: u64,
    /// Owned final-geometry closure invocations, counted immediately before call.
    /// A closure dropped without invocation contributes zero.
    pub clipper_calls: u64,
    /// Candidate, returned-fragment and captured-leaf visits in Core validation,
    /// finalization and visibility processing. Repeated visits count again.
    /// Excludes snapshot copying, Base glyph extraction and ordinary text work.
    pub fragment_visits: u64,
    /// Scope/source-context visits during setup, stack snapshots, promotion, mask
    /// traversal and ancestry checks, including unpromoted plain-prose scopes.
    /// Excludes copies returned by the observer.
    pub scope_visits: u64,
    /// Source-descriptor validation attempts, counted before weak upgrade.
    /// Failed upgrades count; short-circuited checks that never validate do not.
    pub source_checks: u64,
    /// Cached-owner/marker visits during link marking, deferred snapshots,
    /// post-child exclusion and marker journaling/rollback. Excludes the existing
    /// constant single-marker cache-hit check.
    pub cache_owner_visits: u64,
    /// Checkpoint stack/source entries, discarded nodes/registrations and marker
    /// journal entries visited by Core transaction handling. Includes successful
    /// checkpoint work; excludes ordinary accepted-node processing and existing
    /// hitbox/text/dispatch rollback.
    pub transaction_visits: u64,
    /// Actual constructions of the lazy registered-link frame arena.
    /// Counts neither unrelated arenas nor total heap allocations.
    pub link_frame_arenas_created: u64,
    /// Entries into forbidden raw prepaint/paint reuse or linked deferred replay.
    /// Counted before assertion or fail-closed rejection; valid cases require zero.
    pub raw_link_replays: u64,
    /// Duplicate nodes, unbalanced stacks, invalid/repeated focus claims,
    /// conflicting/invalid active-descendant claims, missing-focus fallbacks and
    /// each missing child stripped by repair. Counts before existing handling;
    /// expected link omission is excluded.
    pub tree_defect_events: u64,
    /// Draw-time calls forwarding a complete `TreeUpdate` to the platform window.
    /// Excludes debug capture and activation handshakes. Does not establish
    /// delivery by `TestWindow`, AT-SPI or an operating-system provider.
    pub tree_updates_sent: u64,
}

/// Numeric fragment identity and a weak permit from an actual registration.
///
/// Retains no strong source entity, layout or frame arena and grants no validity
/// authority beyond the existing permit checks.
#[cfg(feature = "test-support")]
#[non_exhaustive]
pub struct A11yLinkTestRegistration {
    /// Numeric ID of the real Link host captured for this registration.
    /// Recording this ID constructs no accessible node while accessibility is inactive.
    pub root: accesskit::NodeId,
    /// Weak permit for this actual registration. Cloning it retains no frame arena
    /// or source entity; normal frame/window/source/nonce checks still apply.
    pub frame: A11yLinkFrame,
    /// Original prefilter fragment keys and corresponding synthetic leaf IDs in
    /// candidate order. IDs may be recorded while accessibility is inactive.
    pub candidates: Vec<(usize, accesskit::NodeId)>,
    /// Original keys from the accepted finalizer output, in candidate order.
    /// Empty for source-invalid or unfinalized records. Later source invalidation
    /// does not rewrite this geometry record; use `A11yLinkFrame::activation_bounds`.
    pub surviving_keys: Vec<usize>,
    /// Actual finalized fragment keys and logical bounds copied at frame completion.
    /// Keys equal `surviving_keys` in the same order; each key identifies its leaf
    /// through `candidates`. Bounds copy the stored finalizer output without scaling.
    /// Uses the same finalized/source-valid guard as `surviving_keys`, including
    /// unpainted registrations. Source-invalid or unfinalized records are empty.
    /// Later source invalidation does not rewrite this accepted-frame snapshot.
    /// This metadata grants no paint, publication or activation validity; normal
    /// `A11yLinkFrame::activation_bounds` checks remain authoritative.
    /// Capture and read copies are excluded from the diagnostic event counters.
    pub surviving_fragments: Vec<(usize, Bounds<Pixels>)>,
}

/// A copied accepted-frame diagnostic record and drained interval counts.
///
/// Copies numeric metadata and weak permits. Reading it performs no source
/// validation, clipping, tree repair or frame acceptance.
#[cfg(feature = "test-support")]
#[non_exhaustive]
#[must_use]
pub struct A11yLinkTestObservation {
    /// Event/visit counts since begin or the preceding successful take.
    /// A successful take drains only these counters; rollback never subtracts work.
    pub interval: A11yLinkTestCounters,
    /// Sequence of the latest completed observed draw within the current epoch.
    /// `None` until one completes. Advances after successful end-of-draw bookkeeping
    /// and any applicable serialization/send, including empty and inactive draws.
    /// Resetting this sequence never changes real frame identity.
    pub accepted_draw: Option<u64>,
    /// Current adapter demand read directly from the real atomic flag at take time.
    /// May differ from the frame's activity snapshot or force-disabled policy.
    pub requested_active: bool,
    /// Latest completed observed frame's metadata, including hidden/unpainted
    /// registrations and excluding transaction-discarded records. Empty before an
    /// observed completion or after an accepted unmount. Replaces the preceding
    /// diagnostic frame without retaining a history.
    pub registrations: Vec<A11yLinkTestRegistration>,
}

#[cfg(feature = "test-support")]
struct LinkTestObservationState {
    counters: Rc<Cell<A11yLinkTestCounters>>,
    accepted_draw: Option<u64>,
    registrations: Vec<A11yLinkTestRegistration>,
    pending: Option<Vec<A11yLinkTestRegistration>>,
}

#[cfg(feature = "test-support")]
macro_rules! count_link_test {
    ($sink:expr, $field:ident) => {
        if let Some(sink) = $sink {
            let mut counters = sink.get();
            counters.$field = counters
                .$field
                .checked_add(1)
                .expect("link test counter overflow");
            sink.set(counters);
        }
    };
}
#[cfg(feature = "test-support")]
pub(super) use count_link_test;

// The feature-off expansion is the original iterator/value. No diagnostic
// closure, branch, field access or counter argument survives compilation.
#[cfg(feature = "test-support")]
macro_rules! link_test_visits {
    ($iter:expr, $sink:expr, $($field:ident),+) => {{
        let sink = $sink;
        ($iter).inspect(move |_| {
            $(crate::window::a11y::count_link_test!(sink.as_ref(), $field);)+
        })
    }};
}
#[cfg(not(feature = "test-support"))]
macro_rules! link_test_visits {
    ($iter:expr, $sink:expr, $($field:ident),+) => {
        $iter
    };
}
pub(super) use link_test_visits;

#[cfg(feature = "test-support")]
macro_rules! link_test_clone {
    ($value:expr, $sink:expr, $($field:ident),+) => {
        crate::window::a11y::link_test_visits!(($value).iter(), $sink, $($field),+).cloned().collect()
    };
}
#[cfg(not(feature = "test-support"))]
macro_rules! link_test_clone {
    ($value:expr, $sink:expr, $($field:ident),+) => {
        ($value).clone()
    };
}
pub(super) use link_test_clone;

#[cfg(feature = "test-support")]
macro_rules! link_test_value {
    ($value:expr, $sink:expr, $field:ident) => {{
        crate::window::a11y::count_link_test!($sink, $field);
        $value
    }};
}
#[cfg(not(feature = "test-support"))]
macro_rules! link_test_value {
    ($value:expr, $sink:expr, $field:ident) => {
        $value
    };
}

#[cfg(feature = "test-support")]
macro_rules! link_test_children_equal {
    ($left:expr, $right:expr, $sink:expr) => {{
        let left = $left;
        let right = $right;
        left.len() == right.len()
            && link_test_visits!(left.iter().zip(right.iter()), $sink, fragment_visits)
                .all(|(left, right)| left == right)
    }};
}
#[cfg(not(feature = "test-support"))]
macro_rules! link_test_children_equal {
    ($left:expr, $right:expr, $sink:expr) => {
        $left == $right
    };
}

#[cfg(feature = "test-support")]
impl Window {
    /// Submit a real adapter callback or an idle-only policy injection.
    ///
    /// Returns `false` without mutation for a removed/non-test window, missing
    /// retained callbacks or an unsupported draw phase. Adapter events permit
    /// idle, prepaint and paint; force-policy injections permit only idle.
    /// `true` means event submission, without accepting a frame or proving delivery.
    #[must_use]
    pub fn simulate_a11y_transition_for_test(&mut self, transition: A11yTestTransition) -> bool {
        if self.removed || !self.a11y_test_window {
            return false;
        }
        let phase = self.invalidator.inner.borrow().draw_phase;
        let phase_allowed = match transition {
            A11yTestTransition::Activate | A11yTestTransition::Deactivate => {
                matches!(
                    phase,
                    DrawPhase::None | DrawPhase::Prepaint | DrawPhase::Paint
                )
            }
            A11yTestTransition::ForceDisable | A11yTestTransition::ClearForceDisable => {
                phase == DrawPhase::None
            }
        };
        if !phase_allowed {
            return false;
        }
        let Some(callbacks) = self.a11y_test_callbacks.clone() else {
            return false;
        };
        match transition {
            A11yTestTransition::Activate => {
                let handshake = {
                    let callbacks = callbacks.lock().expect("test a11y callbacks poisoned");
                    (callbacks.activation)()
                };
                drop(handshake);
            }
            A11yTestTransition::Deactivate => {
                let callbacks = callbacks.lock().expect("test a11y callbacks poisoned");
                (callbacks.deactivation)();
            }
            A11yTestTransition::ForceDisable | A11yTestTransition::ClearForceDisable => {
                let disabled = transition == A11yTestTransition::ForceDisable;
                if self.a11y.force_disabled != disabled {
                    self.a11y.force_disabled = disabled;
                    self.refresh();
                }
            }
        }
        true
    }

    /// Start a diagnostic epoch without changing real frame or source identity.
    ///
    /// Returns `false` while drawing or for a removed/non-test window. Success
    /// zeroes diagnostic counters, accepted-draw sequence and candidate metadata.
    /// Existing permits, nonces, source guards, cache markers and focus stay unchanged.
    #[must_use]
    pub fn begin_a11y_link_observation_for_test(&mut self) -> bool {
        if self.removed
            || !self.a11y_test_window
            || self.invalidator.inner.borrow().draw_phase != DrawPhase::None
        {
            return false;
        }
        let counters = Rc::new(Cell::new(A11yLinkTestCounters::default()));
        self.a11y.nodes.test_counters = Some(Rc::downgrade(&counters));
        self.a11y_links.observation = Some(LinkTestObservationState {
            counters,
            accepted_draw: None,
            registrations: Vec::new(),
            pending: None,
        });
        true
    }

    /// Copy the latest observed accepted metadata and drain only event counts.
    ///
    /// Returns `None` without draining while drawing, for a removed/non-test window
    /// or when observation is disabled. A successful take preserves accepted-frame
    /// metadata and its sequence while draining the interval and reading demand.
    #[must_use]
    pub fn take_a11y_link_observation_for_test(&mut self) -> Option<A11yLinkTestObservation> {
        if self.removed
            || !self.a11y_test_window
            || self.invalidator.inner.borrow().draw_phase != DrawPhase::None
        {
            return None;
        }
        let observation = self.a11y_links.observation.as_ref()?;
        let registrations = observation
            .registrations
            .iter()
            .map(|registration| A11yLinkTestRegistration {
                root: registration.root,
                frame: registration.frame.clone(),
                candidates: registration.candidates.clone(),
                surviving_keys: registration.surviving_keys.clone(),
                surviving_fragments: registration.surviving_fragments.clone(),
            })
            .collect();
        Some(A11yLinkTestObservation {
            interval: observation.counters.take(),
            accepted_draw: observation.accepted_draw,
            requested_active: self.a11y.active_flag.load(Ordering::SeqCst),
            registrations,
        })
    }
}

/// A weak source incarnation and its synchronous validity check.
#[derive(Clone)]
pub struct A11yLinkSource {
    owner: AnyWeakEntity,
    incarnation: usize,
    validate: fn(AnyEntity, usize, &App) -> bool,
}

impl A11yLinkSource {
    /// Capture a source without extending the entity's lifetime.
    #[must_use]
    pub fn new(
        owner: AnyWeakEntity,
        incarnation: usize,
        validate: fn(AnyEntity, usize, &App) -> bool,
    ) -> Self {
        Self {
            owner,
            incarnation,
            validate,
        }
    }

    fn is_current(
        &self,
        cx: &App,
        #[cfg(feature = "test-support")] counters: Option<&Cell<A11yLinkTestCounters>>,
    ) -> bool {
        #[cfg(feature = "test-support")]
        count_link_test!(counters, source_checks);
        self.owner
            .upgrade()
            .is_some_and(|owner| (self.validate)(owner, self.incarnation, cx))
    }
}

/// A keyed fragment measured by the link's original layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct A11yLinkFragment {
    key: usize,
    bounds: Bounds<Pixels>,
}

impl A11yLinkFragment {
    /// Retain a stable candidate key and its logical bounds.
    #[must_use]
    pub fn new(key: usize, bounds: Bounds<Pixels>) -> Self {
        Self { key, bounds }
    }

    /// The key passed to `A11ySubtreeBuilder::synthetic_node_id`.
    pub fn key(&self) -> usize {
        self.key
    }

    /// The fragment's logical bounds.
    pub fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }
}

/// A weak permit for one registration in one completed window frame.
#[derive(Clone)]
#[must_use]
pub struct A11yLinkFrame {
    arena: Weak<RefCell<LinkArena>>,
    slot: usize,
    nonce: u64,
    window: WindowId,
}

impl A11yLinkFrame {
    /// Return the first surviving fragment while this painted frame is current.
    #[must_use]
    pub fn activation_bounds(&self, window: &Window, cx: &App) -> Option<Bounds<Pixels>> {
        if window.invalidator.inner.borrow().draw_phase != DrawPhase::None
            || self.window != window.handle.window_id()
        {
            return None;
        }
        let arena = self.arena.upgrade()?;
        if !window
            .a11y_links
            .accepted
            .as_ref()
            .is_some_and(|accepted| Rc::ptr_eq(accepted, &arena))
        {
            return None;
        }
        let arena = arena.borrow();
        let link = arena.links.get(self.slot)?;
        if link.nonce != self.nonce
            || !arena.link_is_current(
                self.slot,
                cx,
                #[cfg(feature = "test-support")]
                window.a11y_links.test_counters().as_deref(),
            )
        {
            return None;
        }
        link.fragments.first().map(|fragment| fragment.bounds)
    }
}

#[derive(Clone)]
pub(super) struct LinkSourceContext {
    source: A11yLinkSource,
    entry_valid: bool,
    promoted: Option<usize>,
}

enum LinkChild {
    Scope(usize),
    Link(usize),
}

struct LinkScope {
    source: A11yLinkSource,
    entry_valid: bool,
    parent: Option<usize>,
    mask: Option<ContentMask<Pixels>>,
    children: Vec<LinkChild>,
}

type LinkClip = Box<dyn FnOnce(ContentMask<Pixels>) -> Vec<A11yLinkFragment>>;

struct LinkRecord {
    nonce: u64,
    scope: Option<usize>,
    root: Option<NodeId>,
    focus: Option<FocusId>,
    dispatch: Option<DispatchNodeId>,
    candidates: Vec<A11yLinkFragment>,
    fragments: Vec<A11yLinkFragment>,
    clip: Option<LinkClip>,
    mask: ContentMask<Pixels>,
    scale: f32,
    nodes: Vec<(NodeId, accesskit::Node)>,
    node_ids: Vec<NodeId>,
    valid: bool,
    finalized: bool,
    paint_attempted: bool,
    painted: bool,
}

#[derive(Default)]
struct LinkArena {
    scopes: Vec<LinkScope>,
    links: Vec<LinkRecord>,
    roots: FxHashMap<NodeId, usize>,
    leaves: FxHashMap<NodeId, usize>,
    focus: FxHashMap<FocusId, usize>,
}

impl LinkArena {
    fn sources_current(
        &self,
        mut scope: Option<usize>,
        cx: &App,
        #[cfg(feature = "test-support")] counters: Option<&Cell<A11yLinkTestCounters>>,
    ) -> bool {
        while let Some(index) = scope {
            #[cfg(feature = "test-support")]
            count_link_test!(counters, scope_visits);
            let Some(source) = self.scopes.get(index) else {
                return false;
            };
            if !source.entry_valid
                || !source.source.is_current(
                    cx,
                    #[cfg(feature = "test-support")]
                    counters,
                )
            {
                return false;
            }
            scope = source.parent;
        }
        true
    }

    fn link_is_current(
        &self,
        slot: usize,
        cx: &App,
        #[cfg(feature = "test-support")] counters: Option<&Cell<A11yLinkTestCounters>>,
    ) -> bool {
        let Some(link) = self.links.get(slot) else {
            return false;
        };
        link.valid
            && link.finalized
            && link.painted
            && !link.fragments.is_empty()
            && self.sources_current(
                link.scope,
                cx,
                #[cfg(feature = "test-support")]
                counters,
            )
    }
}

/// Opt-in state. Ordinary scopes only use the inline stack; the arena is lazy.
#[derive(Default)]
pub(super) struct A11yLinks {
    #[cfg(feature = "test-support")]
    observation: Option<LinkTestObservationState>,
    sources: SmallVec<[LinkSourceContext; 4]>,
    pub(super) cached_owners: SmallVec<[Rc<Cell<bool>>; 4]>,
    pending: Option<Rc<RefCell<LinkArena>>>,
    accepted: Option<Rc<RefCell<LinkArena>>>,
    capture: Option<usize>,
    pub(super) next_nonce: u64,
    pub(super) painted_registrations: usize,
    deferred_mask: Option<ContentMask<Pixels>>,
    transaction_depth: usize,
    marker_journal: Vec<(Rc<Cell<bool>>, bool)>,
}

#[cfg(feature = "test-support")]
impl A11yLinks {
    pub(super) fn test_counters(&self) -> Option<Rc<Cell<A11yLinkTestCounters>>> {
        self.observation
            .as_ref()
            .map(|observation| observation.counters.clone())
    }
}

struct LinkCapture {
    depth: usize,
    parent: NodeId,
    gap: usize,
    root: Option<NodeId>,
    nodes: Vec<(NodeId, accesskit::Node)>,
    valid: bool,
}

struct LinkEdge {
    parent: NodeId,
    gap: usize,
    root: NodeId,
}

pub(super) struct LinkCheckpoint {
    source_scoped: bool,
    nonce: u64,
    had_arena: bool,
    sources: SmallVec<[LinkSourceContext; 4]>,
    scope_len: usize,
    link_len: usize,
    scope_children: SmallVec<[(usize, usize); 4]>,
    edge_len: usize,
    completed_len: usize,
    stack: SmallVec<[(NodeId, usize, usize); 16]>,
    stack_depth: usize,
    stack_top: Option<(NodeId, usize, usize)>,
    previous_retry_parent_depth: usize,
    capture: Option<(Option<NodeId>, usize, bool)>,
    host_capture: Option<(
        usize,
        Option<NodeId>,
        Option<FocusId>,
        Option<DispatchNodeId>,
        bool,
    )>,
    focus: Option<NodeId>,
    active_descendant: Option<NodeId>,
    claims_len: usize,
    last_focus_without_node: Option<FocusId>,
    next_focus: Option<FocusId>,
    marker_len: usize,
}

fn link_leaf_id(root: NodeId, key: usize) -> NodeId {
    let mut hasher = std::hash::DefaultHasher::default();
    root.0.hash(&mut hasher);
    key.hash(&mut hasher);
    NodeId(hasher.finish())
}

fn positive_finite(bounds: Bounds<Pixels>) -> bool {
    [
        bounds.origin.x.0,
        bounds.origin.y.0,
        bounds.right().0,
        bounds.bottom().0,
        bounds.size.width.0,
        bounds.size.height.0,
    ]
    .into_iter()
    .all(f32::is_finite)
        && bounds.size.width > Pixels::ZERO
        && bounds.size.height > Pixels::ZERO
}

fn link_rect(bounds: Bounds<Pixels>, scale: f32) -> accesskit::Rect {
    accesskit::Rect {
        x0: (bounds.origin.x.0 * scale) as f64,
        y0: (bounds.origin.y.0 * scale) as f64,
        x1: (bounds.right().0 * scale) as f64,
        y1: (bounds.bottom().0 * scale) as f64,
    }
}

impl Window {
    pub(crate) fn with_a11y_link_cached_owner<R>(
        &mut self,
        marker: Rc<Cell<bool>>,
        prepaint: impl FnOnce(&mut Window) -> R,
    ) -> R {
        self.a11y_links.cached_owners.push(marker);
        let result = prepaint(self);
        self.a11y_links.cached_owners.pop();
        result
    }

    pub(super) fn checkpoint_a11y_links(&mut self) -> LinkCheckpoint {
        let source_scoped = !self.a11y_links.sources.is_empty();
        let arena = self.a11y_links.pending.as_ref().map(|arena| arena.borrow());
        let scope_children = if source_scoped {
            link_test_visits!(
                self.a11y_links.sources.iter(),
                self.a11y_links.test_counters(),
                transaction_visits,
                scope_visits
            )
            .filter_map(|source| {
                let index = source.promoted?;
                Some((index, arena.as_ref()?.scopes[index].children.len()))
            })
            .collect()
        } else {
            SmallVec::new()
        };
        let host_capture = self.a11y_links.capture.and_then(|slot| {
            let link = &arena.as_ref()?.links[slot];
            Some((slot, link.root, link.focus, link.dispatch, link.valid))
        });
        let checkpoint = LinkCheckpoint {
            source_scoped,
            nonce: self.a11y_links.next_nonce,
            had_arena: arena.is_some(),
            sources: link_test_clone!(
                self.a11y_links.sources,
                self.a11y_links.test_counters(),
                transaction_visits,
                scope_visits
            ),
            scope_len: arena.as_ref().map_or(0, |arena| arena.scopes.len()),
            link_len: arena.as_ref().map_or(0, |arena| arena.links.len()),
            scope_children,
            edge_len: self.a11y.nodes.link_edges.len(),
            completed_len: self.a11y.nodes.all_nodes.len(),
            stack: if source_scoped {
                self.a11y
                    .nodes
                    .ids_stack
                    .iter()
                    .enumerate()
                    .map(|(index, id)| {
                        #[cfg(feature = "test-support")]
                        count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                        (
                            *id,
                            self.a11y.nodes.nodes_stack[index].children().len(),
                            self.a11y.nodes.pending_children[index]
                                .as_ref()
                                .map_or(0, Vec::len),
                        )
                    })
                    .collect()
            } else {
                SmallVec::new()
            },
            stack_depth: self.a11y.nodes.ids_stack.len(),
            previous_retry_parent_depth: self.a11y.nodes.retry_parent_depth,
            stack_top: if source_scoped {
                None
            } else {
                self.a11y.nodes.ids_stack.last().map(|id| {
                    #[cfg(feature = "test-support")]
                    count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                    (
                        *id,
                        self.a11y
                            .nodes
                            .nodes_stack
                            .last()
                            .expect("parallel node stack")
                            .children()
                            .len(),
                        self.a11y
                            .nodes
                            .pending_children
                            .last()
                            .and_then(Option::as_ref)
                            .map_or(0, Vec::len),
                    )
                })
            },
            capture: self
                .a11y
                .nodes
                .link_capture
                .as_ref()
                .map(|capture| (capture.root, capture.nodes.len(), capture.valid)),
            host_capture,
            focus: self.a11y.nodes.focus,
            active_descendant: self.a11y.nodes.active_descendant,
            claims_len: self.a11y.active_descendant_owners.len(),
            last_focus_without_node: self.a11y.last_focus_without_node,
            next_focus: self.next_frame.focus,
            marker_len: self.a11y_links.marker_journal.len(),
        };
        self.a11y_links.transaction_depth += 1;
        self.a11y.nodes.retry_parent_depth = checkpoint.stack_depth;
        checkpoint
    }

    pub(super) fn finish_a11y_link_transaction(
        &mut self,
        checkpoint: LinkCheckpoint,
        rollback: bool,
    ) {
        // A descendant registration makes every enclosing attempt relevant,
        // including an outer List transaction entered before any source scope.
        // Inner rollback never rewinds this clock.
        let relevant = checkpoint.source_scoped || self.a11y_links.next_nonce != checkpoint.nonce;
        if rollback && relevant {
            let mut discarded = Vec::new();
            for (id, _) in self.a11y.nodes.all_nodes.drain(checkpoint.completed_len..) {
                #[cfg(feature = "test-support")]
                count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                discarded.push(id);
            }
            if let Some(capture) = &mut self.a11y.nodes.link_capture {
                if let Some((root, len, valid)) = checkpoint.capture {
                    for (id, _) in capture.nodes.drain(len..) {
                        #[cfg(feature = "test-support")]
                        count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                        #[cfg(feature = "test-support")]
                        if Some(id) != capture.root {
                            count_link_test!(self.a11y_links.test_counters(), fragment_visits);
                        }
                        discarded.push(id);
                    }
                    capture.root = root;
                    capture.valid = valid;
                } else {
                    for (id, _) in capture.nodes.drain(..) {
                        #[cfg(feature = "test-support")]
                        count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                        #[cfg(feature = "test-support")]
                        if Some(id) != capture.root {
                            count_link_test!(self.a11y_links.test_counters(), fragment_visits);
                        }
                        discarded.push(id);
                    }
                }
            }
            if checkpoint.capture.is_none() {
                self.a11y.nodes.link_capture = None;
            }
            if let Some(arena) = &self.a11y_links.pending {
                let mut arena = arena.borrow_mut();
                while arena.links.len() > checkpoint.link_len {
                    #[cfg(feature = "test-support")]
                    count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                    let link = arena.links.pop().expect("discarded registration suffix");
                    if let Some(root) = link.root {
                        if arena.roots.get(&root) == Some(&arena.links.len()) {
                            arena.roots.remove(&root);
                        }
                        for id in link.node_ids {
                            #[cfg(feature = "test-support")]
                            count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                            #[cfg(feature = "test-support")]
                            if id != root {
                                count_link_test!(self.a11y_links.test_counters(), fragment_visits);
                            }
                            if arena.leaves.get(&id) == Some(&arena.links.len()) {
                                arena.leaves.remove(&id);
                            }
                            discarded.push(id);
                        }
                    }
                    if let Some(focus) = link.focus {
                        if arena.focus.get(&focus) == Some(&arena.links.len()) {
                            arena.focus.remove(&focus);
                        }
                    }
                }
                arena.scopes.truncate(checkpoint.scope_len);
                for (scope, len) in checkpoint.scope_children {
                    #[cfg(feature = "test-support")]
                    count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                    #[cfg(feature = "test-support")]
                    count_link_test!(self.a11y_links.test_counters(), scope_visits);
                    arena.scopes[scope].children.truncate(len);
                }
                if let Some((slot, root, focus, dispatch, valid)) = checkpoint.host_capture {
                    let link = &mut arena.links[slot];
                    link.root = root;
                    link.focus = focus;
                    link.dispatch = dispatch;
                    link.valid = valid;
                }
            }
            self.a11y_links.capture = checkpoint.host_capture.map(|capture| capture.0);
            self.a11y_links.sources = checkpoint.sources;
            if !checkpoint.had_arena {
                self.a11y_links.pending = None;
            }
            self.a11y.nodes.link_edges.truncate(checkpoint.edge_len);
            let depth = checkpoint.stack_depth;
            for id in self.a11y.nodes.ids_stack.drain(depth..) {
                #[cfg(feature = "test-support")]
                count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                discarded.push(id);
            }
            self.a11y.nodes.nodes_stack.truncate(depth);
            self.a11y.nodes.pending_children.truncate(depth);
            if checkpoint.source_scoped {
                for (index, entry) in checkpoint.stack.into_iter().enumerate() {
                    self.restore_a11y_link_stack_entry(index, entry);
                }
            } else if let Some(entry) = checkpoint.stack_top {
                // Balanced prepaint cannot touch any earlier open ancestor.
                self.restore_a11y_link_stack_entry(depth - 1, entry);
            }
            for id in discarded {
                #[cfg(feature = "test-support")]
                count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                self.retire_a11y_link_node(id);
            }
            self.a11y.nodes.focus = checkpoint.focus;
            self.a11y.nodes.active_descendant = checkpoint.active_descendant;
            self.a11y
                .active_descendant_owners
                .truncate(checkpoint.claims_len);
            self.a11y.last_focus_without_node = checkpoint.last_focus_without_node;
            self.next_frame.focus = checkpoint.next_focus;
            while self.a11y_links.marker_journal.len() > checkpoint.marker_len {
                let (marker, previous) = self
                    .a11y_links
                    .marker_journal
                    .pop()
                    .expect("marker journal suffix");
                #[cfg(feature = "test-support")]
                count_link_test!(self.a11y_links.test_counters(), transaction_visits);
                #[cfg(feature = "test-support")]
                count_link_test!(self.a11y_links.test_counters(), cache_owner_visits);
                marker.set(previous);
            }
        }
        self.a11y_links.transaction_depth -= 1;
        self.a11y.nodes.retry_parent_depth = checkpoint.previous_retry_parent_depth;
        if self.a11y_links.transaction_depth == 0 {
            #[cfg(feature = "test-support")]
            {
                let counters = self.a11y_links.test_counters();
                for _ in self.a11y_links.marker_journal.drain(..) {
                    count_link_test!(counters.as_ref(), transaction_visits);
                }
            }
            #[cfg(not(feature = "test-support"))]
            self.a11y_links.marker_journal.clear();
        }
    }

    fn restore_a11y_link_stack_entry(
        &mut self,
        index: usize,
        (id, original_len, pending_len): (NodeId, usize, usize),
    ) {
        #[cfg(feature = "test-support")]
        count_link_test!(self.a11y_links.test_counters(), transaction_visits);
        #[cfg(feature = "test-support")]
        if self.a11y.nodes.ids_stack.get(index) != Some(&id) {
            count_link_test!(self.a11y_links.test_counters(), tree_defect_events);
        }
        debug_assert_eq!(
            self.a11y.nodes.ids_stack.get(index),
            Some(&id),
            "balanced prepaint retains its open ancestors"
        );
        let Some(node) = self.a11y.nodes.nodes_stack.get(index) else {
            return;
        };
        #[cfg(feature = "test-support")]
        if node.children().len() != original_len {
            count_link_test!(self.a11y_links.test_counters(), tree_defect_events);
        }
        debug_assert_eq!(
            node.children().len(),
            original_len,
            "retryable children stay in the private suffix"
        );
        if let Some(children) = self.a11y.nodes.pending_children[index].as_mut() {
            children.truncate(pending_len);
        }
    }

    fn retire_a11y_link_node(&mut self, id: NodeId) {
        self.a11y.focus_ids.remove(&id);
        self.a11y.node_bounds.remove(&id);
        self.a11y.action_listeners.remove(&id);
        self.a11y.nodes.seen_ids.remove(&id);
        self.a11y.nodes.omitted_link_ids.insert(id);
        if self.a11y.nodes.focus == Some(id) {
            self.a11y.nodes.focus = None;
        }
        if self.a11y.nodes.active_descendant == Some(id) {
            self.a11y.nodes.active_descendant = None;
        }
        #[cfg(debug_assertions)]
        self.a11y.nodes.node_info.remove(&id);
    }

    pub(super) fn begin_a11y_link_frame(&mut self) {
        #[cfg(feature = "test-support")]
        if let Some(observation) = &mut self.a11y_links.observation {
            observation.pending = None;
        }
        self.a11y.nodes.omitted_link_ids.clear();
        if let Some(arena) = self.a11y_links.accepted.clone() {
            let arena = arena.borrow();
            for id in arena.roots.keys().chain(link_test_visits!(
                arena.leaves.keys(),
                self.a11y_links.test_counters(),
                fragment_visits
            )) {
                self.retire_a11y_link_node(*id);
            }
        }
        // Dropping an abandoned pending arena invalidates every weak permit.
        self.a11y_links.pending = None;
        self.a11y_links.sources.clear();
        self.a11y_links.cached_owners.clear();
        self.a11y_links.capture = None;
        self.a11y_links.painted_registrations = 0;
        self.a11y_links.deferred_mask = None;
        self.a11y_links.transaction_depth = 0;
        self.a11y_links.marker_journal.clear();
    }

    pub(super) fn complete_a11y_link_frame(&mut self, cx: &mut App) {
        #[cfg(feature = "test-support")]
        let mut observed_registrations = self.a11y_links.observation.as_ref().map(|_| Vec::new());
        let pending = self.a11y_links.pending.clone();
        let mut focused_link_survives = false;
        let mut focused_link_seen = self.a11y_links.accepted.as_ref().is_some_and(|arena| {
            self.focus
                .is_some_and(|focus| arena.borrow().focus.contains_key(&focus))
        });
        if let Some(arena) = &pending {
            let mut arena = arena.borrow_mut();
            #[cfg(feature = "test-support")]
            let arena = &mut *arena;
            #[cfg(feature = "test-support")]
            let observed_roots = &arena.roots;
            let mut source_valid = Vec::with_capacity(arena.scopes.len());
            for scope in &arena.scopes {
                #[cfg(feature = "test-support")]
                count_link_test!(self.a11y_links.test_counters(), scope_visits);
                let parent_valid = scope.parent.is_none_or(|parent| source_valid[parent]);
                source_valid.push(
                    parent_valid
                        && scope.entry_valid
                        && scope.source.is_current(
                            cx,
                            #[cfg(feature = "test-support")]
                            self.a11y_links.test_counters().as_deref(),
                        ),
                );
            }
            for link in &mut arena.links {
                #[cfg(feature = "test-support")]
                {
                    if let Some(registrations) = &mut observed_registrations
                        && let Some(root) = link.root
                    {
                        registrations.push(A11yLinkTestRegistration {
                            root,
                            frame: A11yLinkFrame {
                                arena: Rc::downgrade(
                                    pending.as_ref().expect("current pending arena"),
                                ),
                                slot: *observed_roots.get(&root).expect("registered root slot"),
                                nonce: link.nonce,
                                window: self.handle.window_id(),
                            },
                            candidates: link
                                .candidates
                                .iter()
                                .map(|fragment| (fragment.key, link_leaf_id(root, fragment.key)))
                                .collect(),
                            surviving_keys: if link.finalized
                                && link.scope.is_some_and(|scope| source_valid[scope])
                            {
                                link.fragments.iter().map(|fragment| fragment.key).collect()
                            } else {
                                Vec::new()
                            },
                            surviving_fragments: if link.finalized
                                && link.scope.is_some_and(|scope| source_valid[scope])
                            {
                                link.fragments
                                    .iter()
                                    .map(|fragment| (fragment.key, fragment.bounds))
                                    .collect()
                            } else {
                                Vec::new()
                            },
                        });
                    }
                }
                link.valid &= link.finalized
                    && link.painted
                    && !link.fragments.is_empty()
                    && link.scope.is_some_and(|scope| source_valid[scope]);
                if self.focus.is_some() && link.focus == self.focus {
                    focused_link_seen = true;
                    focused_link_survives |= link.valid;
                }
                if let Some(root) = link.root {
                    let surviving: FxHashSet<NodeId> = if link.valid {
                        link_test_visits!(
                            link.fragments.iter(),
                            self.a11y_links.test_counters(),
                            fragment_visits
                        )
                        .map(|fragment| link_leaf_id(root, fragment.key))
                        .collect()
                    } else {
                        FxHashSet::default()
                    };
                    for id in &link.node_ids {
                        #[cfg(feature = "test-support")]
                        if *id != root {
                            count_link_test!(self.a11y_links.test_counters(), fragment_visits);
                        }
                        if *id != root && !surviving.contains(id) {
                            self.retire_a11y_link_node(*id);
                        }
                    }
                    if link.valid {
                        if self.a11y.is_active() {
                            self.a11y.nodes.all_nodes.append(&mut link.nodes);
                        }
                    } else {
                        self.retire_a11y_link_node(root);
                    }
                } else {
                    for id in &link.node_ids {
                        #[cfg(feature = "test-support")]
                        count_link_test!(self.a11y_links.test_counters(), fragment_visits);
                        self.retire_a11y_link_node(*id);
                    }
                }
                if !link.valid {
                    link.nodes.clear();
                    link.clip = None;
                    if let Some(focus) = link.focus {
                        self.next_frame.dispatch_tree.remove_focus_id(focus);
                        if self.next_frame.focus == Some(focus) {
                            self.next_frame.focus = None;
                        }
                    }
                }
            }
        }
        // Completion is the only irreversible focus edge. Application focus-lost
        // observers keep their normal dispatch route after the frame swap.
        if focused_link_seen && !focused_link_survives {
            self.blur(cx);
        }
        self.a11y_links.accepted = self
            .a11y_links
            .pending
            .take()
            .filter(|arena| !arena.borrow().links.is_empty());
        #[cfg(feature = "test-support")]
        if let Some(observation) = &mut self.a11y_links.observation {
            observation.pending =
                Some(observed_registrations.expect("enabled observation snapshot"));
        }
    }

    #[cfg(feature = "test-support")]
    pub(super) fn finish_a11y_link_observation(&mut self) {
        if let Some(observation) = &mut self.a11y_links.observation
            && let Some(registrations) = observation.pending.take()
        {
            count_link_test!(Some(observation.counters.as_ref()), draws_accepted);
            observation.accepted_draw = Some(
                observation
                    .accepted_draw
                    .unwrap_or(0)
                    .checked_add(1)
                    .expect("observed draw sequence overflow"),
            );
            observation.registrations = registrations;
        }
    }

    pub(super) fn has_accepted_a11y_links(&self) -> bool {
        self.a11y_links.accepted.is_some()
    }

    pub(super) fn a11y_link_focus_allowed(&self, focus: FocusId, cx: &App) -> bool {
        if self
            .a11y_links
            .pending
            .as_ref()
            .is_some_and(|arena| arena.borrow().focus.contains_key(&focus))
        {
            return false;
        }
        let Some(arena) = &self.a11y_links.accepted else {
            return true;
        };
        let arena = arena.borrow();
        let Some(slot) = arena.focus.get(&focus) else {
            return true;
        };
        arena.link_is_current(
            *slot,
            cx,
            #[cfg(feature = "test-support")]
            self.a11y_links.test_counters().as_deref(),
        )
    }

    pub(super) fn focus_a11y_link_tab(&mut self, reverse: bool, cx: &mut App) {
        let mut cursor = self.focus;
        for _ in 0..self.rendered_frame.tab_stops.tab_stop_count() {
            let handle = if reverse {
                self.rendered_frame.tab_stops.prev(cursor.as_ref())
            } else {
                self.rendered_frame.tab_stops.next(cursor.as_ref())
            };
            let Some(handle) = handle else {
                break;
            };
            cursor = Some(handle.id);
            if self.a11y_link_focus_allowed(handle.id, cx) {
                self.focus(&handle, cx);
                break;
            }
        }
    }

    pub(super) fn a11y_link_action_frame(
        &self,
        id: NodeId,
        cx: &App,
    ) -> Option<Option<A11yLinkFrame>> {
        if self.a11y_links.pending.as_ref().is_some_and(|arena| {
            let arena = arena.borrow();
            arena.roots.contains_key(&id) || arena.leaves.contains_key(&id)
        }) {
            return None;
        }
        let Some(arena) = &self.a11y_links.accepted else {
            return Some(None);
        };
        let records = arena.borrow();
        if records.leaves.contains_key(&id) {
            return None;
        }
        let Some(slot) = records.roots.get(&id).copied() else {
            return Some(None);
        };
        if self.invalidator.inner.borrow().draw_phase != DrawPhase::None
            || !records.link_is_current(
                slot,
                cx,
                #[cfg(feature = "test-support")]
                self.a11y_links.test_counters().as_deref(),
            )
        {
            return None;
        }
        Some(Some(A11yLinkFrame {
            arena: Rc::downgrade(arena),
            slot,
            nonce: records.links[slot].nonce,
            window: self.handle.window_id(),
        }))
    }

    /// Prepaint a TextView and retain its final additional paint mask.
    pub fn with_a11y_link_clip_scope<R>(
        &mut self,
        source: A11yLinkSource,
        cx: &mut App,
        prepaint: impl FnOnce(&mut Window, &mut App) -> (R, Option<ContentMask<Pixels>>),
    ) -> R {
        let phase_valid = self.invalidator.inner.borrow().draw_phase == DrawPhase::Prepaint;
        debug_assert!(phase_valid, "link clip scopes require prepaint");
        #[cfg(feature = "test-support")]
        count_link_test!(self.a11y_links.test_counters(), scope_visits);
        self.a11y_links.sources.push(LinkSourceContext {
            entry_valid: phase_valid
                && source.is_current(
                    cx,
                    #[cfg(feature = "test-support")]
                    self.a11y_links.test_counters().as_deref(),
                ),
            source,
            promoted: None,
        });
        let (result, mask) = prepaint(self, cx);
        let scope = self
            .a11y_links
            .sources
            .pop()
            .expect("synchronous link scope");
        #[cfg(feature = "test-support")]
        count_link_test!(self.a11y_links.test_counters(), scope_visits);
        if let Some(index) = scope.promoted {
            let arena = self
                .a11y_links
                .pending
                .as_ref()
                .expect("promoted scope arena")
                .clone();
            arena.borrow_mut().scopes[index].mask = mask;
            #[cfg(feature = "test-support")]
            count_link_test!(self.a11y_links.test_counters(), scope_visits);
            if self.a11y_links.sources.is_empty() {
                self.finalize_a11y_link_scope(index, cx);
            }
        }
        result
    }

    /// Stage one zero-area Link host and its measured synthetic fragment leaves.
    pub fn with_a11y_link_subtree<R>(
        &mut self,
        fragments: Vec<A11yLinkFragment>,
        clip: impl FnOnce(ContentMask<Pixels>) -> Vec<A11yLinkFragment> + 'static,
        cx: &mut App,
        prepaint: impl FnOnce(A11yLinkFrame, &mut Window, &mut App) -> R,
    ) -> R {
        let phase_valid = self.invalidator.inner.borrow().draw_phase == DrawPhase::Prepaint;
        let context_valid =
            phase_valid && !self.a11y_links.sources.is_empty() && self.a11y_links.capture.is_none();
        debug_assert!(
            context_valid,
            "a link subtree requires a source scope and cannot nest"
        );
        if self.a11y_links.capture.is_some() {
            if let Some(slot) = self.a11y_links.capture {
                if let Some(arena) = &self.a11y_links.pending {
                    arena.borrow_mut().links[slot].valid = false;
                }
            }
            return prepaint(
                A11yLinkFrame {
                    arena: Weak::new(),
                    slot: 0,
                    nonce: 0,
                    window: self.handle.window_id(),
                },
                self,
                cx,
            );
        }

        let mut keys = FxHashSet::default();
        let prepaint_mask = self.content_mask();
        let candidates_valid = !fragments.is_empty()
            && link_test_visits!(
                fragments.iter(),
                self.a11y_links.test_counters(),
                fragment_visits
            )
            .all(|fragment| {
                positive_finite(fragment.bounds)
                    && positive_finite(fragment.bounds.intersect(&prepaint_mask.bounds))
                    && keys.insert(fragment.key)
            });
        debug_assert!(
            candidates_valid,
            "link candidates must be positive, finite and distinctly keyed"
        );
        #[cfg(feature = "test-support")]
        let allocation_counters = self.a11y_links.test_counters();
        let arena = self
            .a11y_links
            .pending
            .get_or_insert_with(|| {
                #[cfg(feature = "test-support")]
                count_link_test!(allocation_counters.as_ref(), link_frame_arenas_created);
                Rc::new(RefCell::new(LinkArena::default()))
            })
            .clone();
        // Promote only the unrecorded suffix of the active path.
        let first_unpromoted = link_test_visits!(
            self.a11y_links.sources.iter(),
            self.a11y_links.test_counters(),
            scope_visits
        )
        .rposition(|source| source.promoted.is_some())
        .map_or(0, |index| index + 1);
        let mut parent = first_unpromoted.checked_sub(1).and_then(|index| {
            link_test_value!(
                self.a11y_links.sources[index].promoted,
                self.a11y_links.test_counters(),
                scope_visits
            )
        });
        {
            let mut records = arena.borrow_mut();
            for context in &mut self.a11y_links.sources[first_unpromoted..] {
                #[cfg(feature = "test-support")]
                count_link_test!(allocation_counters.as_ref(), scope_visits);
                let index = records.scopes.len();
                records.scopes.push(LinkScope {
                    source: context.source.clone(),
                    entry_valid: context.entry_valid,
                    parent,
                    mask: None,
                    children: Vec::new(),
                });
                #[cfg(feature = "test-support")]
                count_link_test!(allocation_counters.as_ref(), promoted_scopes);
                if let Some(parent) = parent {
                    records.scopes[parent]
                        .children
                        .push(LinkChild::Scope(index));
                }
                context.promoted = Some(index);
                parent = Some(index);
            }
        }
        self.a11y_links.next_nonce = self
            .a11y_links
            .next_nonce
            .checked_add(1)
            .expect("link registration nonce exhausted");
        let nonce = self.a11y_links.next_nonce;
        let mask = self
            .a11y_links
            .deferred_mask
            .map_or(self.content_mask(), |mask| {
                mask.intersect(&self.content_mask())
            });
        let slot = arena.borrow().links.len();
        {
            let mut records = arena.borrow_mut();
            records.links.push(LinkRecord {
                nonce,
                scope: parent,
                root: None,
                focus: None,
                dispatch: None,
                candidates: fragments,
                fragments: Vec::new(),
                clip: Some(Box::new(clip)),
                mask,
                scale: self.scale_factor(),
                nodes: Vec::new(),
                node_ids: Vec::new(),
                valid: candidates_valid && context_valid,
                finalized: false,
                paint_attempted: false,
                painted: false,
            });
            #[cfg(feature = "test-support")]
            count_link_test!(self.a11y_links.test_counters(), registrations);
            if let Some(parent) = parent {
                records.scopes[parent].children.push(LinkChild::Link(slot));
            }
        }
        for index in 0..self.a11y_links.cached_owners.len() {
            #[cfg(feature = "test-support")]
            count_link_test!(self.a11y_links.test_counters(), cache_owner_visits);
            let marker = self.a11y_links.cached_owners[index].clone();
            self.set_a11y_link_cache_marker(&marker, true);
        }
        self.a11y_links.capture = Some(slot);
        if self.a11y.is_active() {
            self.a11y.nodes.begin_link_capture();
        }
        let frame = A11yLinkFrame {
            arena: Rc::downgrade(&arena),
            slot,
            nonce,
            window: self.handle.window_id(),
        };
        let result = prepaint(frame, self, cx);
        self.a11y_links.capture = None;
        let capture = self
            .a11y
            .is_active()
            .then(|| self.a11y.nodes.end_link_capture());
        let mut records = arena.borrow_mut();
        let link = &mut records.links[slot];
        if let Some(capture) = capture {
            link.valid &= capture.valid && capture.root == link.root;
            link.root = capture.root;
            link.node_ids = capture
                .nodes
                .iter()
                .map(|(id, _)| {
                    #[cfg(feature = "test-support")]
                    if Some(*id) != capture.root {
                        count_link_test!(self.a11y_links.test_counters(), fragment_visits);
                    }
                    *id
                })
                .collect();
            link.nodes = capture.nodes;
            let root = link.root;
            let expected: Vec<NodeId> = root
                .map(|root| {
                    link_test_visits!(
                        link.candidates.iter(),
                        self.a11y_links.test_counters(),
                        fragment_visits
                    )
                    .map(|fragment| link_leaf_id(root, fragment.key))
                    .collect()
                })
                .unwrap_or_default();
            let expected_set: FxHashSet<NodeId> = link_test_visits!(
                expected.iter(),
                self.a11y_links.test_counters(),
                fragment_visits
            )
            .copied()
            .collect();
            link.valid &= link.nodes.len() == expected.len() + 1;
            for (id, node) in &link.nodes {
                if Some(*id) == root {
                    link.valid &= node.role() == accesskit::Role::Link
                        && link_test_children_equal!(
                            node.children(),
                            expected.as_slice(),
                            self.a11y_links.test_counters()
                        );
                } else {
                    #[cfg(feature = "test-support")]
                    count_link_test!(self.a11y_links.test_counters(), fragment_visits);
                    let actionless = (0..=u8::MAX).map_while(Action::n).all(|action| {
                        !node.supports_action(action) && !node.child_supports_action(action)
                    });
                    link.valid &= expected_set.contains(id)
                        && node.role() == accesskit::Role::Label
                        && node.is_read_only()
                        && node.children().is_empty()
                        && actionless
                        && node.custom_actions().is_empty()
                        && node.active_descendant().is_none();
                }
            }
        }
        link.valid &= link.root.is_some() && link.focus.is_some() && link.dispatch.is_some();
        debug_assert!(
            link.valid,
            "link capture requires exactly one zero-area Link and its read-only fragment leaves"
        );
        let root = link.root;
        let focus = link.focus;
        if !self.a11y.is_active()
            && let Some(root) = root
        {
            link.node_ids.push(root);
            link.node_ids.extend(
                link_test_visits!(
                    link.candidates.iter(),
                    self.a11y_links.test_counters(),
                    fragment_visits
                )
                .map(|fragment| link_leaf_id(root, fragment.key)),
            );
        }
        let leaf_ids: Vec<NodeId> = link_test_visits!(
            link.node_ids.iter().copied().filter(|id| Some(*id) != root),
            self.a11y_links.test_counters(),
            fragment_visits
        )
        .collect();
        if let Some(root) = root {
            records.roots.insert(root, slot);
        }
        if let Some(focus) = focus {
            records.focus.insert(focus, slot);
        }
        for id in leaf_ids {
            #[cfg(feature = "test-support")]
            count_link_test!(self.a11y_links.test_counters(), fragment_visits);
            records.leaves.insert(id, slot);
        }
        result
    }

    /// Paint the original host locally if its pending-frame permit is valid.
    #[must_use]
    pub fn paint_a11y_link_subtree(
        &mut self,
        frame: &A11yLinkFrame,
        cx: &mut App,
        paint: impl FnOnce(&mut Window, &mut App),
    ) -> bool {
        let phase_valid = self.invalidator.inner.borrow().draw_phase == DrawPhase::Paint;
        debug_assert!(phase_valid, "link hosts require local paint");
        if !phase_valid || frame.window != self.handle.window_id() {
            return false;
        }
        let Some(arena) = frame.arena.upgrade() else {
            return false;
        };
        if !self
            .a11y_links
            .pending
            .as_ref()
            .is_some_and(|pending| Rc::ptr_eq(pending, &arena))
        {
            return false;
        }
        {
            let mut records = arena.borrow_mut();
            let Some(link) = records.links.get(frame.slot) else {
                return false;
            };
            if link.nonce != frame.nonce {
                return false;
            }
            debug_assert!(!link.paint_attempted, "a link host may paint only once");
            if link.paint_attempted {
                return false;
            }
            let valid = link.valid
                && link.finalized
                && !link.fragments.is_empty()
                && records.sources_current(
                    link.scope,
                    cx,
                    #[cfg(feature = "test-support")]
                    self.a11y_links.test_counters().as_deref(),
                );
            let link = &mut records.links[frame.slot];
            link.paint_attempted = true;
            if !valid {
                link.valid = false;
                return false;
            }
        }
        paint(self, cx);
        let mut records = arena.borrow_mut();
        records.links[frame.slot].painted = true;
        self.a11y_links.painted_registrations += 1;
        true
    }

    fn finalize_a11y_link_scope(&mut self, root: usize, cx: &App) {
        let Some(arena) = &self.a11y_links.pending else {
            return;
        };
        let mut arena = arena.borrow_mut();
        let mut work = vec![(root, None::<ContentMask<Pixels>>, true)];
        while let Some((scope, inherited_mask, inherited_valid)) = work.pop() {
            #[cfg(feature = "test-support")]
            count_link_test!(self.a11y_links.test_counters(), scope_visits);
            let scope_record = &arena.scopes[scope];
            let valid = inherited_valid
                && scope_record.entry_valid
                && scope_record.source.is_current(
                    cx,
                    #[cfg(feature = "test-support")]
                    self.a11y_links.test_counters().as_deref(),
                );
            let mask = match (inherited_mask, scope_record.mask) {
                (Some(parent), Some(local)) => Some(parent.intersect(&local)),
                (parent, local) => parent.or(local),
            };
            // Taking the child list keeps traversal linear and leaves no history tree.
            let children = std::mem::take(&mut arena.scopes[scope].children);
            for child in children {
                match child {
                    LinkChild::Scope(child) => work.push((child, mask, valid)),
                    LinkChild::Link(slot) => {
                        let link = &mut arena.links[slot];
                        let clip = link.clip.take();
                        if link.finalized {
                            debug_assert!(false, "link finalized twice");
                            link.valid = false;
                            continue;
                        }
                        link.finalized = true;
                        link.valid &= valid;
                        let effective = mask.map_or(link.mask, |mask| mask.intersect(&link.mask));
                        let Some(clip) = clip.filter(|_| link.valid) else {
                            continue;
                        };
                        #[cfg(feature = "test-support")]
                        count_link_test!(self.a11y_links.test_counters(), clipper_calls);
                        let fragments = clip(effective);
                        let mut candidate_index = 0;
                        for fragment in &fragments {
                            #[cfg(feature = "test-support")]
                            count_link_test!(self.a11y_links.test_counters(), fragment_visits);
                            while candidate_index < link.candidates.len()
                                && link_test_value!(
                                    link.candidates[candidate_index].key,
                                    self.a11y_links.test_counters(),
                                    fragment_visits
                                ) != fragment.key
                            {
                                candidate_index += 1;
                            }
                            if candidate_index == link.candidates.len()
                                || !positive_finite(fragment.bounds)
                                || fragment
                                    .bounds
                                    .intersect(&link.candidates[candidate_index].bounds)
                                    != fragment.bounds
                                || fragment.bounds.intersect(&effective.bounds) != fragment.bounds
                            {
                                link.valid = false;
                                break;
                            }
                            candidate_index += 1;
                        }
                        if !link.valid || fragments.is_empty() {
                            link.valid = false;
                            continue;
                        }
                        link.fragments = fragments;
                        let Some(root) = link.root else {
                            link.valid = false;
                            continue;
                        };
                        if !self.a11y.is_active() {
                            continue;
                        }
                        let mut union = link.fragments[0].bounds;
                        let surviving: FxHashMap<NodeId, Bounds<Pixels>> = link_test_visits!(
                            link.fragments.iter(),
                            self.a11y_links.test_counters(),
                            fragment_visits
                        )
                        .map(|fragment| {
                            union = union.union(&fragment.bounds);
                            (link_leaf_id(root, fragment.key), fragment.bounds)
                        })
                        .collect();
                        link.nodes.retain_mut(|(id, node)| {
                            if *id == root {
                                node.set_bounds(link_rect(union, link.scale));
                                node.set_children(
                                    link_test_visits!(
                                        link.fragments.iter(),
                                        self.a11y_links.test_counters(),
                                        fragment_visits
                                    )
                                    .map(|fragment| link_leaf_id(root, fragment.key))
                                    .collect::<Vec<_>>(),
                                );
                                true
                            } else if let Some(bounds) = surviving.get(id) {
                                #[cfg(feature = "test-support")]
                                count_link_test!(self.a11y_links.test_counters(), fragment_visits);
                                node.set_bounds(link_rect(*bounds, link.scale));
                                true
                            } else {
                                #[cfg(feature = "test-support")]
                                count_link_test!(self.a11y_links.test_counters(), fragment_visits);
                                false
                            }
                        });
                        self.a11y.node_bounds.insert(root, link.fragments[0].bounds);
                    }
                }
            }
        }
    }

    pub(super) fn capture_a11y_link_focus(&mut self, focus: FocusId) -> bool {
        let Some(slot) = self.a11y_links.capture else {
            return true;
        };
        let root = self.element_id_stack.global_id().accesskit_node_id();
        let dispatch = self.next_frame.dispatch_tree.active_node_id();
        let Some(arena) = &self.a11y_links.pending else {
            return false;
        };
        let mut arena = arena.borrow_mut();
        let link = &mut arena.links[slot];
        if link.root.is_some() {
            #[cfg(feature = "test-support")]
            count_link_test!(self.a11y_links.test_counters(), tree_defect_events);
            link.valid = false;
            return false;
        }
        link.root = Some(root);
        link.focus = Some(focus);
        link.dispatch = dispatch;
        true
    }

    pub(crate) fn set_a11y_link_cache_marker(&mut self, marker: &Rc<Cell<bool>>, value: bool) {
        let previous = marker.replace(value);
        if previous != value && self.a11y_links.transaction_depth > 0 {
            self.a11y_links
                .marker_journal
                .push((marker.clone(), previous));
            #[cfg(feature = "test-support")]
            count_link_test!(self.a11y_links.test_counters(), transaction_visits);
            #[cfg(feature = "test-support")]
            count_link_test!(self.a11y_links.test_counters(), cache_owner_visits);
        }
    }

    pub(super) fn a11y_link_sources_snapshot(&self) -> SmallVec<[LinkSourceContext; 4]> {
        link_test_visits!(
            self.a11y_links.sources.iter(),
            self.a11y_links.test_counters(),
            scope_visits
        )
        .map(|context| LinkSourceContext {
            source: context.source.clone(),
            entry_valid: context.entry_valid,
            promoted: None,
        })
        .collect()
    }

    pub(super) fn with_deferred_a11y_links<R>(
        &mut self,
        mut sources: SmallVec<[LinkSourceContext; 4]>,
        owners: SmallVec<[Rc<Cell<bool>>; 4]>,
        mask: Option<ContentMask<Pixels>>,
        cx: &mut App,
        prepaint: impl FnOnce(&mut Window, &mut App) -> R,
    ) -> R {
        for source in &mut sources {
            #[cfg(feature = "test-support")]
            count_link_test!(self.a11y_links.test_counters(), scope_visits);
            source.entry_valid &= source.source.is_current(
                cx,
                #[cfg(feature = "test-support")]
                self.a11y_links.test_counters().as_deref(),
            );
        }
        let previous_sources = std::mem::replace(&mut self.a11y_links.sources, sources);
        let previous_owners = std::mem::replace(&mut self.a11y_links.cached_owners, owners);
        let actual_mask = mask.map_or(self.content_mask(), |mask| {
            self.content_mask().intersect(&mask)
        });
        let previous_mask = self.a11y_links.deferred_mask.replace(actual_mask);
        let registration_start = self.a11y_links.next_nonce;
        let result = prepaint(self, cx);
        if let Some(root) = self.a11y_links.sources.first().and_then(|source| {
            link_test_value!(
                source.promoted,
                self.a11y_links.test_counters(),
                scope_visits
            )
        }) {
            self.finalize_a11y_link_scope(root, cx);
        }
        // An inner rollback can restore markers while this deferred range still
        // contains a registration attempt and therefore cannot be replayed.
        if self.a11y_links.next_nonce != registration_start {
            for index in 0..self.a11y_links.cached_owners.len() {
                #[cfg(feature = "test-support")]
                count_link_test!(self.a11y_links.test_counters(), cache_owner_visits);
                let marker = self.a11y_links.cached_owners[index].clone();
                self.set_a11y_link_cache_marker(&marker, true);
            }
        }
        self.a11y_links.sources = previous_sources;
        self.a11y_links.cached_owners = previous_owners;
        self.a11y_links.deferred_mask = previous_mask;
        result
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Window {
    /// Activate accessibility tree construction on the in-memory test platform.
    /// This does not connect to a platform accessibility service.
    pub fn activate_accessibility_for_test(&mut self) {
        self.a11y.active_flag.store(true, Ordering::SeqCst);
        self.a11y.sync_active_flag();
        self.refresh();
    }

    /// The last tree produced by the real accessibility bridge in a test window.
    pub fn accessibility_tree_for_test(&self) -> Option<&TreeUpdate> {
        self.a11y.debug.last_tree_update()
    }

    /// Dispatch a platform accessibility request through the normal action path.
    pub fn dispatch_accessibility_action_for_test(
        &mut self,
        request: accesskit::ActionRequest,
        cx: &mut App,
    ) {
        self.handle_a11y_action(request, cx);
    }
}

/// A listener for an accessibility action on a specific node.
pub(crate) type A11yActionListener =
    Box<dyn FnMut(Option<&accesskit::ActionData>, &mut Window, &mut App) + 'static>;

/// Per-window accessibility state.
///
/// Manages the AccessKit tree that is built each frame and the mappings
/// needed to dispatch incoming action requests back to the right elements.
pub(crate) struct A11y {
    /// Whether accessibility has been [forcibly disabled] for this window.
    ///
    /// [forcibly disabled]: crate::Application::new_inaccessible
    force_disabled: bool,
    /// Whether a11y features have been requested by the system.
    ///
    /// Updated by AccessKit using callbacks provided to the adapter. Can change
    /// halfway through a frame.
    active_flag: Arc<AtomicBool>,
    /// Whether a11y features are active for *this specific frame*.
    ///
    /// At the start of each frame, we load [`Self::active_flag`] (using
    /// [`Self::sync_active_flag`]) and use this to determine whether we
    /// should construct a [`TreeUpdate`] for this frame. It's important that
    /// this value is stable within a frame, because the builder API exposed by
    /// this type maintains a stack of nodes and each must be pushed and popped
    /// exactly once.
    ///
    /// At the end of the frame, we re-call [`Self::sync_active_flag`] to
    /// determine whether we should actually send the finished [`TreeUpdate`].
    active_this_frame: bool,
    pub(crate) nodes: A11yNodeBuilder,
    pub(crate) focus_ids: FxHashMap<NodeId, FocusId>,
    /// Row-side claims resolved after sibling subtrees have finished rendering.
    active_descendant_owners: SmallVec<[(NodeId, FocusId); 1]>,
    pub(crate) node_bounds: FxHashMap<NodeId, Bounds<Pixels>>,
    pub(crate) action_listeners: FxHashMap<NodeId, Vec<(Action, A11yActionListener)>>,
    /// The window's title, used to label the root node so assistive
    /// technology can tell windows apart.
    window_title: Option<SharedString>,
    /// The focus id we most recently reported as having no accessibility node,
    /// used to log at most once per focus change rather than every frame.
    last_focus_without_node: Option<FocusId>,
    /// Retains the last tree update (and, in debug builds, per-node provenance)
    /// so it can be dumped via [`crate::Window::debug_a11y_tree_json`].
    debug: debug::A11yDebug,
    /// Maps a view's [`EntityId`] to its `Render` type name
    #[cfg(debug_assertions)]
    pub(crate) view_type_names: FxHashMap<EntityId, &'static str>,
}

impl A11y {
    pub(crate) fn new(
        active_flag: Arc<AtomicBool>,
        force_disabled: bool,
        window_title: Option<SharedString>,
    ) -> Self {
        Self {
            force_disabled,
            active_flag,
            active_this_frame: false,
            nodes: A11yNodeBuilder::new(),
            focus_ids: FxHashMap::default(),
            active_descendant_owners: SmallVec::new(),
            node_bounds: FxHashMap::default(),
            action_listeners: FxHashMap::default(),
            window_title,
            last_focus_without_node: None,
            debug: debug::A11yDebug::default(),
            #[cfg(debug_assertions)]
            view_type_names: FxHashMap::default(),
        }
    }

    /// Logs (once per focus change) that the focused element is not exposed to
    /// assistive technology because it has no accessibility node. When this
    /// happens, screen readers fall back to announcing the whole window instead
    /// of the focused element. The fix is to give the element both an
    /// `.id(...)` and a `.role(...)`.
    pub(crate) fn note_focus_without_node(&mut self, focus_id: FocusId, reason: &str) {
        if self.last_focus_without_node != Some(focus_id) {
            self.last_focus_without_node = Some(focus_id);
            log::info!(
                "a11y: focused element ({focus_id:?}) has no accessibility node \
                 ({reason}); assistive technology will announce the whole window \
                 instead. Give it both an `.id(...)` and a `.role(...)` to expose it."
            );
        }
    }

    pub(crate) fn set_window_title(&mut self, title: impl Into<SharedString>) {
        self.window_title = Some(title.into());
    }

    /// Ensures that [`Self::is_active`] returns up to date information.
    ///
    /// See the docs for [`Self::active_flag`] and [`Self::active_this_frame`]
    /// for more commentary.
    pub(crate) fn sync_active_flag(&mut self) {
        self.active_this_frame = self.is_enabled() && self.active_flag.load(Ordering::SeqCst);
    }

    pub(crate) fn is_enabled(&self) -> bool {
        !self.force_disabled
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active_this_frame
    }

    pub(crate) fn set_focusable(&mut self, node_id: NodeId, focus_id: FocusId) {
        self.focus_ids.insert(node_id, focus_id);
    }

    /// Report `node_id` as the currently-focused node, if it is present in the
    /// tree.
    ///
    /// Must only be called once per frame.
    pub(crate) fn set_focus(&mut self, node_id: NodeId) {
        // A focused node must have been registered as focusable this frame.
        if !self.focus_ids.contains_key(&node_id) {
            #[cfg(feature = "test-support")]
            count_link_test!(self.nodes.test_counters(), tree_defect_events);
            if cfg!(debug_assertions) {
                panic!("set_focus called for a node that was not registered with set_focusable");
            } else {
                log::warn!(
                    "a11y: set_focus called for a node that was not registered with \
                     set_focusable ({node_id:?})"
                );
            }
        }
        if self.nodes.has_node(node_id) {
            // The focused element is properly exposed; reset the dedup so a
            // later focus on a node-less element logs again.
            self.last_focus_without_node = None;
            self.nodes.set_focus(node_id);
        } else {
            // The element registered a focus handle and an id, but never got a
            // node because it has no role.
            if let Some(focus_id) = self.focus_ids.get(&node_id).copied() {
                self.note_focus_without_node(focus_id, "it has an id but no role");
            }
        }
    }

    pub(crate) fn set_active_descendant(&mut self, node_id: NodeId) {
        // The active descendant must be a descendant of the focused container,
        // not the focused node itself.
        if self.nodes.node_is_focused(node_id) {
            #[cfg(feature = "test-support")]
            count_link_test!(self.nodes.test_counters(), tree_defect_events);
            if cfg!(debug_assertions) {
                panic!("set_active_descendant called on the focused node");
            } else {
                log::warn!("a11y: set_active_descendant called on the focused node ({node_id:?})");
            }
            return;
        }
        #[cfg(feature = "test-support")]
        if !self.nodes.has_node(node_id) && !self.nodes.omitted_link_ids.contains(&node_id) {
            count_link_test!(self.nodes.test_counters(), tree_defect_events);
        }
        if self.nodes.has_node(node_id) && self.nodes.focus_is_ancestor_of_current() {
            self.nodes.set_active_descendant(node_id);
        }
    }

    /// Clear per-frame state and push the root node to start a new frame.
    pub(crate) fn begin_frame(&mut self) {
        self.focus_ids.clear();
        self.active_descendant_owners.clear();
        self.node_bounds.clear();
        self.action_listeners.clear();
        self.nodes.begin_frame(self.window_title.as_ref());
    }

    /// Finalize the tree and produce a [`TreeUpdate`] for the platform adapter.
    pub(crate) fn end_frame(&mut self, frame: debug::FrameDebugInfo) -> TreeUpdate {
        let mut update = self.nodes.finalize();
        let explicit_descendant = self.apply_active_descendant_relation(&mut update);
        self.debug.capture(
            &update,
            self.nodes.focus,
            explicit_descendant.or(self.nodes.active_descendant),
            self.window_title.as_ref(),
            frame,
        );
        #[cfg(debug_assertions)]
        self.debug.capture_node_info(&self.nodes.node_info);
        update
    }

    pub(crate) fn debug_tree_json(&self) -> Option<String> {
        self.debug.to_json()
    }

    pub(crate) fn set_active_descendant_for(&mut self, node: NodeId, owner: FocusId) {
        self.active_descendant_owners.push((node, owner));
    }

    fn apply_active_descendant_relation(&self, update: &mut TreeUpdate) -> Option<NodeId> {
        let owner = self.nodes.focus?;
        let focus_id = self.focus_ids.get(&owner)?;
        let mut claims = self
            .active_descendant_owners
            .iter()
            .filter(|(node, focus)| {
                #[cfg(feature = "test-support")]
                if *node == owner && focus == focus_id {
                    count_link_test!(self.nodes.test_counters(), tree_defect_events);
                }
                *node != owner && focus == focus_id
            });
        let &(target, _) = claims.next()?;
        if claims.any(|(node, _)| *node != target) {
            #[cfg(feature = "test-support")]
            count_link_test!(self.nodes.test_counters(), tree_defect_events);
            log::warn!("a11y: multiple active descendants claimed for one focused owner");
            return None;
        }
        if !update.nodes.iter().any(|(id, _)| *id == target) {
            #[cfg(feature = "test-support")]
            if !self.nodes.omitted_link_ids.contains(&target) {
                count_link_test!(self.nodes.test_counters(), tree_defect_events);
            }
            return None;
        }
        let (_, node) = update.nodes.iter_mut().find(|(id, _)| *id == owner)?;
        node.set_active_descendant(target);
        // AccessKit resolves the relation to platform focus. Keep the tree's
        // keyboard focus on the editor so its text and editing actions survive.
        update.focus = owner;
        Some(target)
    }
}

/// Builder API for synthetic children. See the docs for
/// [`Element::a11y_synthetic_children`].
pub struct A11ySubtreeBuilder<'a> {
    parent_id: NodeId,
    nodes: &'a mut A11yNodeBuilder,
    /// Provenance of the real element whose `a11y_synthetic_children` is
    /// running.
    #[cfg(debug_assertions)]
    creator: debug::NodeCreator,
}

impl<'a> A11ySubtreeBuilder<'a> {
    pub(crate) fn new(parent_id: NodeId, nodes: &'a mut A11yNodeBuilder) -> Self {
        Self {
            parent_id,
            nodes,
            #[cfg(debug_assertions)]
            creator: debug::NodeCreator::default(),
        }
    }

    #[cfg(debug_assertions)]
    pub(crate) fn with_creator(mut self, creator: debug::NodeCreator) -> Self {
        self.creator = creator;
        self
    }

    /// Derive a [`NodeId`] for a synthetic child.
    ///
    /// The generated ID is based on the hash of `key`, as well as the parent's
    /// ID. This means that `key`s must be unique within the same
    /// [`Element::a11y_synthetic_children`] call, but may be duplicated across
    /// different calls.
    pub fn synthetic_node_id(&self, key: impl Hash) -> NodeId {
        let mut hasher = std::hash::DefaultHasher::default();
        self.parent_id.0.hash(&mut hasher);
        key.hash(&mut hasher);
        NodeId(hasher.finish())
    }

    /// Append a synthetic leaf node as a child of this element's node.
    ///
    /// Returns `false` if a node with this id is already present in the tree,
    /// in which case the node is discarded.
    pub fn push_child(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        let pushed = self.nodes.push_leaf(id, node);
        #[cfg(debug_assertions)]
        if pushed {
            self.nodes.record_node_info(
                id,
                debug::NodeDebugInfo {
                    synthetic: true,
                    view: self.creator.view,
                    element_id: self.creator.element_id.clone(),
                    source_location: self.creator.source_location,
                },
            );
        }
        pushed
    }

    /// A mutable reference to the parent node.
    pub fn parent_node(&mut self) -> &mut accesskit::Node {
        self.nodes
            .current_node_mut()
            .expect("A11ySubtreeBuilder exists only while its element's node is on the stack")
    }
}

pub(crate) struct A11yNodeBuilder {
    #[cfg(feature = "test-support")]
    test_counters: Option<Weak<Cell<A11yLinkTestCounters>>>,
    ids_stack: SmallVec<[NodeId; 16]>,
    nodes_stack: SmallVec<[accesskit::Node; 16]>,
    // Retryable children of open ancestors stay separate until that ancestor pops.
    // A rollback truncates only this suffix, without copying accepted siblings.
    pending_children: SmallVec<[Option<Vec<NodeId>>; 16]>,
    retry_parent_depth: usize,
    /// This is the exact type required by accesskit, so we can't just make it a
    /// `HashMap<NodeId, Node>` to remove the need for `seen_ids`
    all_nodes: Vec<(NodeId, accesskit::Node)>,
    seen_ids: FxHashSet<NodeId>,
    link_capture: Option<LinkCapture>,
    link_edges: Vec<LinkEdge>,
    omitted_link_ids: FxHashSet<NodeId>,
    /// The node that GPUI considers focused. Note that this may be different to
    /// what is reported to accesskit - see [`Self::active_descendant`]
    focus: Option<NodeId>,
    /// If a node calls `.aria_active_descendant()`, AND an ancestor is focused,
    /// override it as the focused node. This supports the "active descendant"
    /// pattern, which allows a focused container to act as if a descendant is
    /// focused.
    active_descendant: Option<NodeId>,
    #[cfg(debug_assertions)]
    node_info: FxHashMap<NodeId, debug::NodeDebugInfo>,
}

impl A11yNodeBuilder {
    #[cfg(feature = "test-support")]
    fn test_counters(&self) -> Option<Rc<Cell<A11yLinkTestCounters>>> {
        self.test_counters.as_ref().and_then(Weak::upgrade)
    }

    fn new() -> Self {
        Self {
            #[cfg(feature = "test-support")]
            test_counters: None,
            ids_stack: SmallVec::new(),
            nodes_stack: SmallVec::new(),
            pending_children: SmallVec::new(),
            retry_parent_depth: 0,
            all_nodes: Vec::new(),
            seen_ids: FxHashSet::default(),
            link_capture: None,
            link_edges: Vec::new(),
            omitted_link_ids: FxHashSet::default(),
            focus: None,
            active_descendant: None,
            #[cfg(debug_assertions)]
            node_info: FxHashMap::default(),
        }
    }

    /// Records provenance for a node already pushed this frame. Debug builds only.
    #[cfg(debug_assertions)]
    pub(crate) fn record_node_info(&mut self, id: NodeId, info: debug::NodeDebugInfo) {
        self.node_info.insert(id, info);
    }

    #[must_use]
    fn can_push(&mut self, id: NodeId) -> bool {
        #[cfg(feature = "test-support")]
        if self.ids_stack.is_empty() {
            count_link_test!(self.test_counters(), tree_defect_events);
        }
        debug_assert!(!self.ids_stack.is_empty(), "node pushed before push_root");

        if !self.seen_ids.insert(id) {
            #[cfg(feature = "test-support")]
            count_link_test!(self.test_counters(), tree_defect_events);
            debug_assert!(
                false,
                "Duplicate a11y node id: {id:?}. In a release build, this node would be silently discarded from the a11y tree."
            );
            return false;
        }

        self.omitted_link_ids.remove(&id);

        true
    }

    /// Push a new node onto the stack. It becomes a child of the current
    /// top-of-stack node.
    ///
    /// Returns `true` if the node was successfully pushed.
    pub(crate) fn push(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        if !self.can_push(id) {
            return false;
        }

        let captured_root = if let Some(capture) = &mut self.link_capture {
            if capture.root.is_none() && self.ids_stack.len() == capture.depth {
                capture.root = Some(id);
                capture.valid &= node.role() == accesskit::Role::Link
                    && node
                        .bounds()
                        .is_some_and(|bounds| bounds.x0 == bounds.x1 && bounds.y0 == bounds.y1);
                self.link_edges.push(LinkEdge {
                    parent: capture.parent,
                    gap: capture.gap,
                    root: id,
                });
                true
            } else {
                capture.valid = false;
                false
            }
        } else {
            false
        };
        if !captured_root {
            self.append_child(id);
        }
        self.ids_stack.push(id);
        self.nodes_stack.push(node);
        self.pending_children.push(None);
        true
    }

    /// Add a leaf node as a child of the current top-of-stack node, without
    /// pushing it onto the stack. Semantically equivalent to a [`Self::push`]
    /// followed by a [`Self::pop`].
    ///
    /// Returns `true` if the node was successfully pushed.
    pub(crate) fn push_leaf(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        if !self.can_push(id) {
            return false;
        }

        self.append_child(id);
        if let Some(capture) = &mut self.link_capture {
            capture.valid &= self.ids_stack.len() == capture.depth + 1;
            capture.nodes.push((id, node));
        } else {
            self.all_nodes.push((id, node));
        }
        true
    }

    pub(crate) fn current_node_mut(&mut self) -> Option<&mut accesskit::Node> {
        // Synthetic callbacks run after child prepaint, immediately before pop.
        self.flush_current_children();
        self.nodes_stack.last_mut()
    }

    fn append_child(&mut self, id: NodeId) {
        if let Some(parent) = self.nodes_stack.last_mut() {
            let pending = self
                .pending_children
                .last_mut()
                .expect("parallel child stack");
            if self.ids_stack.len() == self.retry_parent_depth || pending.is_some() {
                pending.get_or_insert_with(Vec::new).push(id);
            } else {
                parent.push_child(id);
            }
        }
    }

    fn current_child_count(&self) -> usize {
        self.nodes_stack
            .last()
            .map_or(0, |node| node.children().len())
            + self
                .pending_children
                .last()
                .and_then(|children| children.as_ref())
                .map_or(0, Vec::len)
    }

    fn flush_current_children(&mut self) {
        let Some(pending) = self.pending_children.last_mut().and_then(Option::take) else {
            return;
        };
        if pending.is_empty() {
            return;
        }
        if let Some(node) = self.nodes_stack.last_mut() {
            let children = if node.children().is_empty() {
                pending
            } else {
                let mut children = node.children().to_vec();
                children.extend(pending);
                children
            };
            node.set_children(children);
        }
    }

    /// Pop the current node off the stack and finalize it into the all_nodes
    /// list.
    pub(crate) fn pop(&mut self) {
        #[cfg(feature = "test-support")]
        if self.ids_stack.len() <= 1 {
            count_link_test!(self.test_counters(), tree_defect_events);
        }
        debug_assert!(self.ids_stack.len() > 1, "pop would remove the root node");

        self.flush_current_children();
        self.pending_children.pop();
        if let (Some(id), Some(node)) = (self.ids_stack.pop(), self.nodes_stack.pop()) {
            if let Some(capture) = &mut self.link_capture {
                capture.nodes.push((id, node));
            } else {
                self.all_nodes.push((id, node));
            }
        }
    }

    fn begin_link_capture(&mut self) {
        debug_assert!(self.link_capture.is_none());
        self.link_capture = Some(LinkCapture {
            depth: self.ids_stack.len(),
            parent: self.ids_stack.last().copied().unwrap_or(ROOT_NODE_ID),
            gap: self.current_child_count(),
            root: None,
            nodes: Vec::new(),
            valid: true,
        });
    }

    fn end_link_capture(&mut self) -> LinkCapture {
        let mut capture = self.link_capture.take().expect("active link capture");
        #[cfg(feature = "test-support")]
        if self.ids_stack.len() != capture.depth {
            count_link_test!(self.test_counters(), tree_defect_events);
        }
        capture.valid &= self.ids_stack.len() == capture.depth && capture.root.is_some();
        capture
    }

    /// Push the root node to start a new frame.
    fn begin_frame(&mut self, window_title: Option<&SharedString>) {
        self.all_nodes.clear();
        self.ids_stack.clear();
        self.nodes_stack.clear();
        self.pending_children.clear();
        self.retry_parent_depth = 0;
        self.seen_ids.clear();
        self.link_capture = None;
        self.link_edges.clear();
        self.omitted_link_ids.clear();
        #[cfg(debug_assertions)]
        self.node_info.clear();
        let mut root_node = accesskit::Node::new(accesskit::Role::Window);
        if let Some(title) = window_title {
            root_node.set_label(title.to_string());
        }

        self.ids_stack.push(ROOT_NODE_ID);
        self.nodes_stack.push(root_node);
        self.pending_children.push(None);
        self.focus = None;
        self.active_descendant = None;
    }

    /// Returns whether a node with the given ID has been pushed in this frame.
    pub(crate) fn has_node(&self, id: NodeId) -> bool {
        id == ROOT_NODE_ID || self.seen_ids.contains(&id)
    }

    /// Returns whether `id` is the node currently reported as focused.
    pub(crate) fn node_is_focused(&self, id: NodeId) -> bool {
        self.focus == Some(id)
    }

    pub(crate) fn focus_is_ancestor_of_current(&self) -> bool {
        let Some(focus) = self.focus else {
            return false;
        };

        // The current node is on top of the stack; everything below it is an
        // ancestor.
        let ancestor_count = self.ids_stack.len().saturating_sub(1);
        self.ids_stack[..ancestor_count].contains(&focus)
    }

    pub(crate) fn set_active_descendant(&mut self, id: NodeId) {
        if self
            .active_descendant
            .is_some_and(|existing| existing != id)
        {
            #[cfg(feature = "test-support")]
            count_link_test!(self.test_counters(), tree_defect_events);
            if cfg!(debug_assertions) {
                panic!("active descendant claimed by multiple nodes in one frame");
            } else {
                log::warn!(
                    "a11y: multiple nodes claimed the active descendant this frame; \
                     using last-wins ({id:?})"
                );
            }
        }
        self.active_descendant = Some(id);
    }

    pub(crate) fn set_focus(&mut self, id: NodeId) {
        if self.focus.is_some() {
            #[cfg(feature = "test-support")]
            count_link_test!(self.test_counters(), tree_defect_events);
            if cfg!(debug_assertions) {
                panic!("set_focus called more than once in a single frame");
            } else {
                log::warn!(
                    "a11y: set_focus called more than once in a single frame; \
                     using last-wins ({id:?})"
                );
            }
        }
        self.focus = Some(id);
    }

    fn finalize(&mut self) -> TreeUpdate {
        // Stack should contain only the root node
        #[cfg(feature = "test-support")]
        if self.ids_stack.len() != 1 || self.ids_stack.first() != Some(&ROOT_NODE_ID) {
            count_link_test!(self.test_counters(), tree_defect_events);
        }
        debug_assert_eq!(self.ids_stack.len(), 1);
        debug_assert_eq!(self.ids_stack[0], ROOT_NODE_ID);

        if self.ids_stack.len() != 1 {
            log::error!(
                "a11y: Stack imbalance at end of frame: expected 1 (root), got {}. \
                 Some elements may have pushed without popping.",
                self.ids_stack.len()
            );
        }

        // Pop remaining nodes (should just be the root).
        while !self.ids_stack.is_empty() {
            self.flush_current_children();
            self.pending_children.pop();
            if let (Some(id), Some(node)) = (self.ids_stack.pop(), self.nodes_stack.pop()) {
                self.all_nodes.push((id, node));
            }
        }

        let focus = match self.active_descendant {
            Some(id) if self.has_node(id) => id,
            Some(id) => {
                #[cfg(feature = "test-support")]
                count_link_test!(self.test_counters(), tree_defect_events);
                if cfg!(debug_assertions) {
                    panic!("active_descendant set to {id:?}, which is not in the tree");
                } else {
                    log::warn!("active_descendant set to {id:?}, which is not in the tree");
                    self.focus.unwrap_or(ROOT_NODE_ID)
                }
            }

            _ => self.focus.unwrap_or(ROOT_NODE_ID),
        };

        let nodes = std::mem::take(&mut self.all_nodes);
        let update = TreeUpdate {
            nodes,
            tree: Some(accesskit::Tree::new(ROOT_NODE_ID)),
            tree_id: accesskit::TreeId::ROOT,
            focus,
        };

        let mut edges: FxHashMap<NodeId, Vec<(usize, NodeId)>> = FxHashMap::default();
        for edge in self.link_edges.drain(..) {
            if self.seen_ids.contains(&edge.root) {
                edges
                    .entry(edge.parent)
                    .or_default()
                    .push((edge.gap, edge.root));
            }
        }
        Self::repair_tree_update(
            update,
            edges,
            &self.omitted_link_ids,
            #[cfg(feature = "test-support")]
            self.test_counters().as_deref(),
        )
    }

    /// Accesskit panics on invalid [`TreeUpdate`]s. This function defensively
    /// checks invariants that accesskit panics on, and tries to fix them.
    fn repair_tree_update(
        mut update: TreeUpdate,
        mut edges: FxHashMap<NodeId, Vec<(usize, NodeId)>>,
        omitted: &FxHashSet<NodeId>,
        #[cfg(feature = "test-support")] counters: Option<&Cell<A11yLinkTestCounters>>,
    ) -> TreeUpdate {
        let node_ids: FxHashSet<NodeId> = update.nodes.iter().map(|(id, _)| *id).collect();

        // Focus must point to a node in the tree.
        if !node_ids.contains(&update.focus) {
            #[cfg(feature = "test-support")]
            count_link_test!(counters, tree_defect_events);
            log::error!(
                "a11y: Focused node {:?} is not in the tree ({} nodes). \
                 Falling back to root. This is a bug in the a11y tree builder.",
                update.focus,
                update.nodes.len()
            );
            update.focus = ROOT_NODE_ID;
        }

        // Every child reference must point to a node in the update.
        for (id, node) in &mut update.nodes {
            if node
                .active_descendant()
                .is_some_and(|target| omitted.contains(&target))
            {
                node.clear_active_descendant();
            }
            if let Some(links) = edges.remove(id) {
                let ordinary = node.children();
                let mut children = Vec::with_capacity(ordinary.len() + links.len());
                let mut links = links.into_iter().peekable();
                for gap in 0..=ordinary.len() {
                    while links.peek().is_some_and(|(position, _)| *position == gap) {
                        children.push(links.next().expect("peeked edge").1);
                    }
                    if let Some(child) = ordinary.get(gap) {
                        children.push(*child);
                    }
                }
                debug_assert!(
                    links.next().is_none(),
                    "reserved parent gap must survive prepaint"
                );
                node.set_children(children);
            }
            let has_invalid_child = node
                .children()
                .iter()
                .any(|child_id| !node_ids.contains(child_id));
            if has_invalid_child {
                let children = node.children();
                let invalid_count = link_test_visits!(
                    children
                        .iter()
                        .filter(|child_id| !node_ids.contains(child_id)),
                    counters,
                    tree_defect_events
                )
                .count();
                log::error!(
                    "a11y: Node {:?} references {} children not present in the tree. \
                     Stripping invalid child references.",
                    id,
                    invalid_count
                );
                let valid: Vec<NodeId> = children
                    .iter()
                    .copied()
                    .filter(|child_id| node_ids.contains(child_id))
                    .collect();
                node.set_children(valid);
            }
        }

        update
    }
}

#[cfg(test)]
mod tests {
    // Import specific items rather than glob-importing `super`, which would pull
    // in gpui's own `test` attribute macro and shadow the standard one.
    use super::{A11y, A11yNodeBuilder, ROOT_NODE_ID};
    use crate::FocusId;
    use accesskit::{NodeId, Role};
    use std::sync::{Arc, atomic::AtomicBool};

    fn test_node() -> accesskit::Node {
        accesskit::Node::new(Role::GenericContainer)
    }

    fn new_builder() -> A11yNodeBuilder {
        let mut builder = A11yNodeBuilder::new();
        builder.begin_frame(None);
        builder
    }

    fn new_a11y() -> A11y {
        let mut a11y = A11y::new(Arc::new(AtomicBool::new(true)), false, None);
        a11y.begin_frame();
        a11y
    }

    fn sibling_relation_frame(
        a11y: &mut A11y,
        focused: Option<NodeId>,
        target: Option<NodeId>,
        render_target: bool,
        target_first: bool,
    ) -> accesskit::TreeUpdate {
        let editor = NodeId(1);
        let other = NodeId(4);
        a11y.begin_frame();
        let render_row = |a11y: &mut A11y| {
            if let Some(target) = target {
                a11y.set_active_descendant_for(target, FocusId::default());
                if render_target {
                    assert!(
                        a11y.nodes
                            .push(target, accesskit::Node::new(Role::ListItem))
                    );
                    a11y.nodes.pop();
                }
            }
        };
        if target_first {
            render_row(a11y);
        }
        assert!(
            a11y.nodes
                .push(editor, accesskit::Node::new(Role::TextInput))
        );
        a11y.set_focusable(editor, FocusId::default());
        if focused == Some(editor) {
            a11y.set_focus(editor);
        }
        a11y.nodes.pop();
        assert!(a11y.nodes.push(other, accesskit::Node::new(Role::Button)));
        if focused == Some(other) {
            // Only the editor is registered with the claimed handle.
            a11y.nodes.set_focus(other);
        }
        a11y.nodes.pop();
        if !target_first {
            render_row(a11y);
        }
        a11y.end_frame(Default::default())
    }

    fn editor_descendant(update: &accesskit::TreeUpdate) -> Option<NodeId> {
        update
            .nodes
            .iter()
            .find(|(id, _)| *id == NodeId(1))
            .expect("editor remains in the accessible tree")
            .1
            .active_descendant()
    }

    #[test]
    fn explicit_active_descendant_resolves_siblings_in_either_render_order() {
        for target_first in [false, true] {
            let mut a11y = new_a11y();
            let update = sibling_relation_frame(
                &mut a11y,
                Some(NodeId(1)),
                Some(NodeId(2)),
                true,
                target_first,
            );
            assert_eq!(
                update.focus,
                NodeId(1),
                "keyboard focus stays on the editor"
            );
            assert_eq!(editor_descendant(&update), Some(NodeId(2)));
        }
    }

    #[test]
    fn explicit_active_descendant_changes_and_clears_on_each_frame() {
        let mut a11y = new_a11y();
        for target in [Some(NodeId(2)), Some(NodeId(3)), None, Some(NodeId(2))] {
            let update = sibling_relation_frame(&mut a11y, Some(NodeId(1)), target, true, false);
            assert_eq!(update.focus, NodeId(1));
            assert_eq!(editor_descendant(&update), target);
        }
        let removed =
            sibling_relation_frame(&mut a11y, Some(NodeId(1)), Some(NodeId(2)), false, false);
        assert_eq!(
            editor_descendant(&removed),
            None,
            "missing rows cannot leave a dangling relation"
        );
        for focus in [None, Some(NodeId(4))] {
            let update = sibling_relation_frame(&mut a11y, focus, Some(NodeId(2)), true, false);
            assert_eq!(
                editor_descendant(&update),
                None,
                "an unfocused editor cannot claim a row"
            );
            assert_eq!(update.focus, focus.unwrap_or(ROOT_NODE_ID));
        }
    }

    #[test]
    fn explicit_active_descendant_rejects_missing_owner_and_self_reference() {
        let mut a11y = new_a11y();
        assert!(a11y.nodes.push(NodeId(2), test_node()));
        a11y.nodes.pop();
        a11y.set_active_descendant_for(NodeId(2), FocusId::default());
        let update = a11y.end_frame(Default::default());
        assert_eq!(update.focus, ROOT_NODE_ID);
        assert!(
            update
                .nodes
                .iter()
                .all(|(_, node)| node.active_descendant().is_none())
        );

        a11y.begin_frame();
        assert!(a11y.nodes.push(NodeId(1), test_node()));
        a11y.set_focusable(NodeId(1), FocusId::default());
        a11y.set_focus(NodeId(1));
        a11y.set_active_descendant_for(NodeId(1), FocusId::default());
        a11y.nodes.pop();
        let update = a11y.end_frame(Default::default());
        assert_eq!(editor_descendant(&update), None);
    }

    #[test]
    fn explicit_active_descendant_rejects_ambiguous_rows() {
        let mut a11y = new_a11y();
        assert!(a11y.nodes.push(NodeId(1), test_node()));
        a11y.set_focusable(NodeId(1), FocusId::default());
        a11y.set_focus(NodeId(1));
        a11y.nodes.pop();
        for id in [NodeId(2), NodeId(3)] {
            assert!(a11y.nodes.push(id, test_node()));
            a11y.nodes.pop();
            a11y.set_active_descendant_for(id, FocusId::default());
        }
        let update = a11y.end_frame(Default::default());
        assert_eq!(editor_descendant(&update), None);
        assert_eq!(update.focus, NodeId(1));
    }

    #[test]
    fn accessibility_enabled_is_independent_of_activation() {
        for force_disabled in [false, true] {
            for active in [false, true] {
                let mut a11y = A11y::new(Arc::new(AtomicBool::new(active)), force_disabled, None);
                a11y.sync_active_flag();

                assert_eq!(a11y.is_enabled(), !force_disabled);
                assert_eq!(a11y.is_active(), !force_disabled && active);
            }
        }
    }

    #[test]
    fn active_descendant_honored_when_container_focused() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container);
        assert!(builder.push(item, test_node()));

        // The item is on top of the stack; the focused container is its
        // ancestor, so the claim is honored.
        assert!(builder.focus_is_ancestor_of_current());
        builder.set_active_descendant(item);

        builder.pop(); // item
        builder.pop(); // container
        let update = builder.finalize();
        assert_eq!(update.focus, item);
    }

    #[test]
    fn active_descendant_honored_for_deep_descendant() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let group = NodeId(2);
        let item = NodeId(3);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container);
        assert!(builder.push(group, test_node()));
        assert!(builder.push(item, test_node()));

        // The item is a grandchild of the focused container; depth doesn't
        // matter, the focused ancestor is still on the stack.
        assert!(builder.focus_is_ancestor_of_current());
        builder.set_active_descendant(item);

        builder.pop(); // item
        builder.pop(); // group
        builder.pop(); // container
        let update = builder.finalize();
        assert_eq!(update.focus, item);
    }

    #[test]
    fn active_descendant_ignored_when_focus_in_other_subtree() {
        let mut builder = new_builder();
        let focused_container = NodeId(1);
        let focused_leaf = NodeId(2);
        let other_container = NodeId(3);
        let other_item = NodeId(4);

        // First subtree holds real focus.
        assert!(builder.push(focused_container, test_node()));
        assert!(builder.push(focused_leaf, test_node()));
        builder.set_focus(focused_leaf);
        builder.pop(); // focused_leaf
        builder.pop(); // focused_container

        // Second subtree: its item would claim the active descendant, but the
        // focus is not on any of its ancestors, so the gate rejects it.
        assert!(builder.push(other_container, test_node()));
        assert!(builder.push(other_item, test_node()));
        assert!(!builder.focus_is_ancestor_of_current());
        builder.pop(); // other_item
        builder.pop(); // other_container

        let update = builder.finalize();
        assert_eq!(update.focus, focused_leaf);
    }

    #[test]
    fn active_descendant_ignored_when_nothing_focused() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        assert!(builder.push(item, test_node()));

        // Nothing is focused (focus defaults to the root window node), so the
        // gate rejects the claim.
        assert!(!builder.focus_is_ancestor_of_current());
        builder.pop();
        builder.pop();

        let update = builder.finalize();
        assert_eq!(update.focus, ROOT_NODE_ID);
    }

    #[test]
    fn regular_focus_used_when_no_active_descendant() {
        let mut builder = new_builder();
        let focused = NodeId(1);

        assert!(builder.push(focused, test_node()));
        builder.set_focus(focused);
        builder.pop();

        let update = builder.finalize();
        assert_eq!(update.focus, focused);
    }

    #[test]
    fn focus_is_ancestor_excludes_self_and_non_ancestors() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container);

        // With the focused container itself on top, it is not its own (strict)
        // ancestor, so the gate is false.
        assert!(!builder.focus_is_ancestor_of_current());

        assert!(builder.push(item, test_node()));
        // Now the focused container is a strict ancestor of the item on top.
        assert!(builder.focus_is_ancestor_of_current());

        builder.pop();
        builder.pop();
    }

    // The double-claim guard panics only in debug builds; in release it falls
    // back to last-wins with a warning.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "active descendant claimed by multiple nodes")
    )]
    fn multiple_active_descendant_claims_panic_in_debug() {
        let mut builder = new_builder();
        builder.set_active_descendant(NodeId(1));
        builder.set_active_descendant(NodeId(2));
    }

    // Setting focus twice in one frame means two elements both claimed window
    // focus; that panics in debug and falls back to last-wins in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "set_focus called more than once")
    )]
    fn setting_focus_twice_panics_in_debug() {
        let mut builder = new_builder();
        builder.set_focus(NodeId(1));
        builder.set_focus(NodeId(2));
    }

    // Focusing a node that was never registered as focusable is a bug: panic in
    // debug, warn in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "was not registered with set_focusable")
    )]
    fn set_focus_without_set_focusable() {
        let mut a11y = new_a11y();
        let node = NodeId(1);
        assert!(a11y.nodes.push(node, test_node()));
        // set_focusable was never called for `node`.
        a11y.set_focus(node);
    }

    // The focused node cannot also be its own active descendant: panic in
    // debug, warn in release.
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "on the focused node"))]
    fn set_active_descendant_on_focused_node() {
        let mut a11y = new_a11y();
        let node = NodeId(1);
        assert!(a11y.nodes.push(node, test_node()));
        a11y.set_focusable(node, FocusId::default());
        a11y.set_focus(node);
        a11y.set_active_descendant(node);
    }

    // Two sibling children of a focused container both claim the active
    // descendant (both pass the focus gate). The second claim is a bug: panic
    // in debug, last-wins + warn in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "active descendant claimed by multiple nodes")
    )]
    fn two_siblings_claiming_active_descendant() {
        let mut a11y = new_a11y();
        let container = NodeId(1);
        let first = NodeId(2);
        let second = NodeId(3);

        assert!(a11y.nodes.push(container, test_node()));
        a11y.set_focusable(container, FocusId::default());
        a11y.set_focus(container);

        assert!(a11y.nodes.push(first, test_node()));
        a11y.set_active_descendant(first);
        a11y.nodes.pop(); // first

        assert!(a11y.nodes.push(second, test_node()));
        a11y.set_active_descendant(second);
        a11y.nodes.pop(); // second

        a11y.nodes.pop(); // container
    }

    // Node A is focused; node C (a child of the unfocused node B) claims the
    // active descendant. The final tree must still report A as focused.
    #[test]
    fn active_descendant_in_unfocused_subtree_keeps_real_focus() {
        let mut a11y = new_a11y();
        let a = NodeId(1);
        let b = NodeId(2);
        let c = NodeId(3);

        assert!(a11y.nodes.push(a, test_node()));
        a11y.set_focusable(a, FocusId::default());
        a11y.set_focus(a);
        a11y.nodes.pop(); // a

        assert!(a11y.nodes.push(b, test_node()));
        assert!(a11y.nodes.push(c, test_node()));
        a11y.set_active_descendant(c);
        a11y.nodes.pop(); // c
        a11y.nodes.pop(); // b

        let update = a11y.end_frame(Default::default());
        assert_eq!(update.focus, a);
    }
}
