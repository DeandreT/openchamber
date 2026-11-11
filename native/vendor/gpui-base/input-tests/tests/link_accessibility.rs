//! Public behavior tests for the frozen native link contract and test addendum.
//! Test-owned elements observe real layout/paint or drive public transactions.
//! No clip, glyph, link-owner, source-validity, or permit algorithm is copied here.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use gpui::accesskit::{Action, ActionRequest, Node, NodeId, Rect, Role, TreeId, TreeUpdate};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    A11yLinkFragment, A11yLinkFrame, A11yLinkSource, A11yLinkTestObservation, A11yTestTransition,
    AnyElement, AnyEntity, AnyWeakEntity, App, AppContext as _, AvailableSpace, Bounds, ClickEvent,
    ContentMask, Context, Element, ElementId, Entity, FocusHandle, GlobalElementId,
    InputEvent as _, InspectorElementId, InteractiveElement as _, IntoElement, KeyDownEvent,
    KeyUpEvent, Keystroke, LayoutId, ParentElement as _, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, StyledText, TestAppContext,
    VisualTestContext, Window, deferred, div, point, px, size,
};
use gpui_base::text::{
    InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
    MarkdownPlugin, TextView, TextViewState, markdown_ast,
};

const SOURCE: &str = "[alpha](https://example.invalid/a)";
const URL: &str = "https://example.invalid/a";

#[derive(Clone, Debug)]
struct Activation {
    url: String,
    keyboard_bounds: Option<Bounds<Pixels>>,
    position: Point<Pixels>,
}

type Calls = Arc<Mutex<Vec<Activation>>>;

fn recorder(
    calls: &Calls,
) -> impl Fn(&SharedString, &ClickEvent, &mut Window, &mut App) + Send + Sync + 'static {
    let calls = calls.clone();
    move |url, event, _, _| {
        calls.lock().expect("activation log").push(Activation {
            url: url.to_string(),
            keyboard_bounds: match event {
                ClickEvent::Keyboard(event) => Some(event.bounds),
                _ => None,
            },
            position: event.position(),
        });
    }
}

#[derive(Clone, Copy, Debug)]
struct PaintSample {
    bounds: Bounds<Pixels>,
    mask: Bounds<Pixels>,
}

type Samples = Arc<Mutex<Vec<PaintSample>>>;

#[derive(Clone, Copy, PartialEq)]
enum TestPhase {
    Prepaint,
    Paint,
}

type PendingTransition = Arc<Mutex<Option<(TestPhase, A11yTestTransition)>>>;
type TransitionRecords = Arc<Mutex<Vec<(bool, bool, bool)>>>;

type KeyUpEdit = Rc<RefCell<Option<Box<dyn FnOnce(&mut Window, &mut App)>>>>;

/// This transparent observer delegates all child layout, prepaint and paint.
/// An inline observer sees the actual mask after TextView applies its final clip.
struct ObservePaint {
    child: Option<AnyElement>,
    samples: Samples,
    transition: Option<(PendingTransition, TransitionRecords)>,
}

impl ObservePaint {
    fn new(child: impl IntoElement, samples: &Samples) -> Self {
        Self {
            child: Some(child.into_any_element()),
            samples: samples.clone(),
            transition: None,
        }
    }
}

impl ObservePaint {
    fn transition(mut self, pending: &PendingTransition, records: &TransitionRecords) -> Self {
        self.transition = Some((pending.clone(), records.clone()));
        self
    }

    fn drive_transition(&self, phase: TestPhase, window: &mut Window) {
        let Some((pending, records)) = &self.transition else {
            return;
        };
        let event = {
            let mut pending = pending.lock().expect("one-shot transition");
            if pending.as_ref().is_some_and(|(when, _)| *when == phase) {
                pending.take().map(|(_, event)| event)
            } else {
                None
            }
        };
        if let Some(event) = event {
            let before = window.is_a11y_active();
            let submitted = window.simulate_a11y_transition_for_test(event);
            records.lock().expect("transition records").push((
                before,
                window.is_a11y_active(),
                submitted,
            ));
            assert!(window.take_a11y_link_observation_for_test().is_none());
            assert!(!window.begin_a11y_link_observation_for_test());
        }
    }
}

impl IntoElement for ObservePaint {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for ObservePaint {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let mut child = self.child.take().expect("one layout request");
        let layout = child.request_layout(window, cx);
        (layout, child)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        child: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.drive_transition(TestPhase::Prepaint, window);
        child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.drive_transition(TestPhase::Paint, window);
        self.samples
            .lock()
            .expect("paint observations")
            .push(PaintSample {
                bounds,
                mask: window.content_mask().bounds,
            });
        child.paint(window, cx);
    }
}

struct ObjectPlugin {
    samples: Samples,
}

impl MarkdownPlugin for ObjectPlugin {
    fn name(&self) -> &str {
        "object"
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        _: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        match node {
            markdown_ast::Node::InlineMath(math)
                if math.value == "object" || math.value == "neighbor" =>
            {
                Some(MarkdownNode::new("object", ()).text(math.value.clone()))
            }
            _ => None,
        }
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        _: &InlineRenderContext,
        _: &mut Window,
        _: &mut App,
    ) -> Option<InlineElement> {
        if node.as_text() == "neighbor" {
            return Some(InlineElement::new(
                div()
                    .id("inline-neighbor")
                    .role(Role::Button)
                    .aria_label("inline neighbor")
                    .tab_index(0)
                    .w(px(24.))
                    .h(px(18.)),
            ));
        }
        Some(InlineElement::new(ObservePaint::new(
            div().id("object-content").w(px(26.)).h(px(31.)),
            &self.samples,
        )))
    }
}

fn object_extensions(samples: &Samples) -> MarkdownExtensions {
    MarkdownExtensions::default().plugin(ObjectPlugin {
        samples: samples.clone(),
    })
}

struct Page {
    body: Entity<TextViewState>,
    retained_child: Option<Entity<TextViewState>>,
    extensions: MarkdownExtensions,
    calls: Calls,
    ancestor_clicks: Arc<AtomicUsize>,
    before: FocusHandle,
    after: FocusHandle,
    visible: bool,
    width: Pixels,
    max_lines: Option<usize>,
    viewport_height: Option<Pixels>,
    offset: Pixels,
    outer_samples: Samples,
    frame_samples: Samples,
    cache_group: Option<Entity<CacheGroup>>,
    rollback_panel: Option<(Entity<RollbackPanel>, Entity<PlainPanel>)>,
    code_neighbor: bool,
    transition: PendingTransition,
    transition_records: TransitionRecords,
    outer_retry: Option<(ElementFactory, Arc<AtomicUsize>, Arc<AtomicUsize>)>,
    unrelated_nodes: usize,
    defect_control: bool,
    list_fixture: Option<ListFixture>,
    deferred_rollback_group: Option<Entity<DeferredRollbackGroup>>,
    key_up_edit: KeyUpEdit,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let ancestor_clicks = self.ancestor_clicks.clone();
        let key_up_edit = self.key_up_edit.clone();
        let body = TextView::new(&self.body)
            .markdown_extensions(self.extensions.clone())
            .on_link_click(recorder(&self.calls))
            .selectable(true)
            .w_full()
            .when(self.code_neighbor, |this| {
                this.code_block_actions(|_, _, _| {
                    div()
                        .id("code-neighbor")
                        .role(Role::Button)
                        .aria_label("code neighbor")
                        .tab_index(0)
                        .size(px(16.))
                })
            })
            .when_some(self.max_lines, |this, lines| this.max_lines(lines));
        let content = if let Some(group) = &self.deferred_rollback_group {
            group
                .clone()
                .cached(StyleRefinement::default().w(self.width).h(px(250.)))
                .into_any_element()
        } else if let Some(fixture) = &self.list_fixture {
            let body = fixture.body.clone();
            let extensions = fixture.extensions.clone();
            let calls = self.calls.clone();
            let attempts = fixture.attempts.clone();
            let samples = fixture.samples.clone();
            gpui::list(fixture.state.clone(), move |_, _, _| {
                AutoscrollChild {
                    child: Some(
                        TextView::new(&body)
                            .markdown_extensions(extensions.clone())
                            .on_link_click(recorder(&calls))
                            .into_any_element(),
                    ),
                    attempts: attempts.clone(),
                    samples: samples.clone(),
                }
                .into_any_element()
            })
            .w(self.width)
            .h(px(60.))
            .into_any_element()
        } else if let Some((factory, attempts, accepted)) = &self.outer_retry {
            Retry {
                factory: factory.clone(),
                accepted_factory: None,
                attempts: attempts.clone(),
                nested: false,
                accepted_count: Some(accepted.clone()),
            }
            .into_any_element()
        } else if let Some((panel, sibling)) = &self.rollback_panel {
            div()
                .w(self.width)
                .flex()
                .flex_col()
                .child(
                    panel
                        .clone()
                        .cached(StyleRefinement::default().w(self.width).h(px(250.))),
                )
                .child(
                    sibling
                        .clone()
                        .cached(StyleRefinement::default().w(self.width).h(px(50.))),
                )
                .into_any_element()
        } else if let Some(group) = &self.cache_group {
            group
                .clone()
                .cached(StyleRefinement::default().w(self.width).h(px(250.)))
                .into_any_element()
        } else {
            div()
                .w(self.width)
                .when_some(self.viewport_height, |this, height| {
                    this.h(height).overflow_hidden()
                })
                .when(self.visible, |this| {
                    this.child(
                        div()
                            .relative()
                            .top(-self.offset)
                            .child(ObservePaint::new(body, &self.outer_samples)),
                    )
                })
                .into_any_element()
        };
        ObservePaint::new(
            div()
                .id("page")
                .size_full()
                .flex()
                .flex_col()
                .capture_key_up(move |event, window, cx| {
                    if event.keystroke.key == "enter" {
                        let edit = key_up_edit.borrow_mut().take();
                        if let Some(edit) = edit {
                            edit(window, cx);
                        }
                    }
                })
                // This is the test root's normal key-to-focus route. Link key
                // activation remains GPUI's real keydown/keyup click handling.
                .on_key_down(|event, window, cx| {
                    if event.keystroke.key == "tab" {
                        if event.keystroke.modifiers.shift {
                            window.focus_prev(cx);
                        } else {
                            window.focus_next(cx);
                        }
                        cx.stop_propagation();
                    }
                })
                .on_click(move |event, _, _| {
                    if matches!(event, ClickEvent::Keyboard(_)) {
                        ancestor_clicks.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .child(
                    div()
                        .id("before")
                        .role(Role::Button)
                        .aria_label("before")
                        .track_focus(&self.before)
                        .tab_index(0)
                        .flex_shrink_0()
                        .size(px(12.)),
                )
                .children((0..self.unrelated_nodes).map(|ix| {
                    div()
                        .id(("ordinary", ix))
                        .role(Role::Label)
                        .aria_label("unrelated ordinary node")
                        .h(px(0.))
                }))
                .child(
                    div()
                        .id("message")
                        .role(Role::Paragraph)
                        .aria_label("message body")
                        .child(content),
                )
                .when(self.defect_control, |this| {
                    this.child(
                        div()
                            .id("deliberate-defect")
                            .role(Role::Group)
                            .aria_label("repair positive control")
                            .a11y_synthetic_children(|builder| {
                                let missing = builder.synthetic_node_id(0usize);
                                builder.parent_node().push_child(missing);
                            }),
                    )
                })
                .child(
                    div()
                        .id("after")
                        .role(Role::Button)
                        .aria_label("after")
                        .track_focus(&self.after)
                        .tab_index(0)
                        .flex_shrink_0()
                        .size(px(12.)),
                ),
            &self.frame_samples,
        )
        .transition(&self.transition, &self.transition_records)
    }
}

fn setup<'a>(
    cx: &'a mut TestAppContext,
    source: &str,
    active: bool,
) -> (Entity<Page>, &'a mut VisualTestContext) {
    cx.update(gpui_component::init);
    cx.add_window_view(|window, cx| {
        window.set_scale_factor(1.25);
        assert!(window.begin_a11y_link_observation_for_test());
        if active {
            window.activate_accessibility_for_test();
        }
        Page {
            body: cx.new(|cx| TextViewState::markdown(source, cx)),
            retained_child: None,
            extensions: MarkdownExtensions::default(),
            calls: Arc::default(),
            ancestor_clicks: Arc::default(),
            before: cx.focus_handle().tab_stop(true),
            after: cx.focus_handle().tab_stop(true),
            visible: true,
            width: px(180.),
            max_lines: None,
            viewport_height: None,
            offset: px(0.),
            outer_samples: Arc::default(),
            frame_samples: Arc::default(),
            cache_group: None,
            rollback_panel: None,
            code_neighbor: false,
            transition: Arc::default(),
            transition_records: Arc::default(),
            outer_retry: None,
            unrelated_nodes: 0,
            defect_control: false,
            list_fixture: None,
            deferred_rollback_group: None,
            key_up_edit: Rc::default(),
        }
    })
}

fn draw(window: &mut Window, cx: &mut App) -> TreeUpdate {
    window.draw(cx).clear(cx);
    let tree = window
        .accessibility_tree_for_test()
        .expect("active tree")
        .clone();
    let observed = observation(window);
    assert!(observed.accepted_draw.is_some());
    assert_registration_geometry(&observed, &tree, window, cx);
    tree
}

fn assert_registration_geometry(
    observed: &A11yLinkTestObservation,
    tree: &TreeUpdate,
    window: &Window,
    cx: &App,
) {
    let scale = window.scale_factor();
    for registration in &observed.registrations {
        assert_eq!(
            registration
                .surviving_fragments
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>(),
            registration.surviving_keys,
            "actual finalized geometry retains every surviving key in order"
        );
        if let Some(bounds) = registration.frame.activation_bounds(window, cx) {
            let expected: Vec<_> = registration
                .surviving_keys
                .iter()
                .map(|key| {
                    registration
                        .candidates
                        .iter()
                        .find(|(candidate, _)| candidate == key)
                        .expect("surviving key belongs to this frame's actual candidates")
                        .1
                })
                .collect();
            assert_eq!(
                node(tree, registration.root).children(),
                expected.as_slice(),
                "same-frame candidate lookup supplies the exact emitted leaf IDs"
            );
            assert_eq!(
                bounds,
                registration
                    .surviving_fragments
                    .first()
                    .expect("a current painted permit has finalized geometry")
                    .1,
                "actual current permit matches the first stored logical fragment exactly"
            );
            let published = fragments(tree, registration.root);
            assert_eq!(
                published.len(),
                registration.surviving_fragments.len(),
                "every published fragment has actual finalized logical geometry"
            );
            assert_eq!(
                physical_rect(bounds, scale),
                published[0].1,
                "actual permit uses the first surviving fragment of this accepted frame"
            );
            for ((key, logical), (leaf, rect)) in
                registration.surviving_fragments.iter().zip(&published)
            {
                let expected_leaf = registration
                    .candidates
                    .iter()
                    .find(|(candidate, _)| candidate == key)
                    .expect("every finalized key belongs to this registration's actual candidates")
                    .1;
                assert_eq!(
                    *leaf, expected_leaf,
                    "same-frame key maps to the emitted leaf"
                );
                assert_eq!(
                    *rect,
                    physical_rect(*logical, scale),
                    "every emitted Rect exactly forward-encodes its stored logical fragment"
                );
            }
        }
    }
}

fn current_logical_fragments(
    tree: &TreeUpdate,
    root: NodeId,
    captured_scale: f32,
    window: &mut Window,
    cx: &App,
) -> Vec<(usize, Bounds<Pixels>)> {
    assert_eq!(window.scale_factor(), captured_scale);
    let observed = observation(window);
    assert!(observed.accepted_draw.is_some());
    assert_registration_geometry(&observed, tree, window, cx);
    let mut registrations = observed
        .registrations
        .iter()
        .filter(|registration| registration.root == root);
    let registration = registrations
        .next()
        .expect("actual accepted Link registration");
    assert!(
        registrations.next().is_none(),
        "exactly one accepted registration supplies this published root"
    );
    assert_eq!(node(tree, root).role(), Role::Link);
    let bounds = registration
        .frame
        .activation_bounds(window, cx)
        .expect("published scale geometry requires the actual current painted permit");
    assert_eq!(
        bounds,
        registration
            .surviving_fragments
            .first()
            .expect("current published Link has a finalized fragment")
            .1
    );
    registration.surviving_fragments.clone()
}

fn nested_uncapped_metrics_case(cx: &mut TestAppContext, object_only: bool) {
    let (page, cx) = setup(cx, "leading whole line\n\n```child\nretained\n```", true);
    cx.update(|window, cx| {
        let source = if object_only {
            "[$object$](https://example.invalid/metric-child)".to_owned()
        } else {
            format!(
                "[{}](https://example.invalid/metric-child)",
                "actually shaped words ".repeat(45)
            )
        };
        let child = cx.new(|cx| TextViewState::markdown(&source, cx));
        let retained = child.clone();
        let child_paints: Samples = Arc::default();
        let object_paints: Samples = Arc::default();
        let observed_child = child_paints.clone();
        let child_extensions = if object_only {
            object_extensions(&object_paints)
        } else {
            MarkdownExtensions::default()
        };
        let padding = Arc::new(Mutex::new(px(0.)));
        let render_padding = padding.clone();
        let calls = page.read(cx).calls.clone();
        let extensions = MarkdownExtensions::default()
            .block_parser(|node, _| {
                matches!(node, markdown_ast::Node::Code(_))
                    .then(|| MarkdownNode::new("metric-child", ()).text("retained child"))
            })
            .block_renderer("metric-child", move |_, _, _| {
                div()
                    .pt(*render_padding.lock().expect("measured padding"))
                    .child(ObservePaint::new(
                        TextView::new(&child)
                            .markdown_extensions(child_extensions.clone())
                            .on_link_click(recorder(&calls))
                            .w_full(),
                        &observed_child,
                    ))
            });
        page.update(cx, |page, cx| {
            page.extensions = extensions;
            cx.notify();
        });
        let full = draw(window, cx);
        let child_link = links(&full)
            .into_iter()
            .find(|id| node(&full, *id).url() == Some("https://example.invalid/metric-child"))
            .expect("real child Link in uncapped control");
        let first_bounds = if object_only {
            object_paints
                .lock()
                .expect("object paints")
                .last()
                .expect("actual measured object")
                .bounds
        } else {
            let rows = fragments(&full, child_link);
            assert!(
                rows.len() >= 4,
                "positive control: actual child text wraps across whole lines"
            );
            logical(rows[0].1, window)
        };
        assert!(first_bounds.size.height > px(0.));
        let mut crossed = false;
        // Each padding is derived from actual measured content. It only drives
        // real layout; the crossing oracle always uses the freshly shaped rows.
        'measured_padding: for fraction in [0.2, 0.4, 0.6, 0.8] {
            *padding.lock().expect("padding") = first_bounds.size.height * fraction;
            page.update(cx, |page, cx| {
                page.max_lines = None;
                cx.notify();
            });
            child_paints.lock().expect("child paint").clear();
            object_paints.lock().expect("object paint").clear();
            let unbounded = draw(window, cx);
            let baseline = *child_paints
                .lock()
                .expect("child paint")
                .last()
                .expect("uncapped child painted");
            let rows: Vec<_> = if object_only {
                vec![
                    object_paints
                        .lock()
                        .expect("object paint")
                        .last()
                        .expect("measured object painted")
                        .bounds,
                ]
            } else {
                fragments(&unbounded, child_link)
                    .into_iter()
                    .map(|(_, rect)| logical(rect, window))
                    .collect()
            };
            for cap in 2..=8 {
                page.update(cx, |page, cx| {
                    page.max_lines = Some(cap);
                    cx.notify();
                });
                child_paints.lock().expect("child paint").clear();
                let clipped = draw(window, cx);
                let child_paint = *child_paints
                    .lock()
                    .expect("child paint")
                    .last()
                    .expect("ordinary child painted");
                let outer = *page
                    .read(cx)
                    .outer_samples
                    .lock()
                    .expect("parent paint")
                    .last()
                    .expect("capped parent painted");
                assert_eq!(
                    child_paint.bounds, baseline.bounds,
                    "capping must not reshape or move the real child"
                );
                let crossing = rows.iter().enumerate().find(|(index, row)| {
                    (object_only || *index > 0)
                        && row.top() + row.size.height * 0.25 < outer.bounds.bottom()
                        && outer.bounds.bottom() < row.bottom() - row.size.height * 0.25
                });
                if let Some((_, row)) = crossing {
                    crossed = true;
                    assert!(
                        row.top() < outer.bounds.bottom() && outer.bounds.bottom() < row.bottom(),
                        "positive control: actual shaped/measured content crosses the real cap"
                    );
                    assert!(page.read(cx).body.read(cx).is_clamped());
                    assert!(
                        !retained.read(cx).is_clamped(),
                        "the actual leaf remains uncapped"
                    );
                    assert!(
                        child_paint.mask.bottom() <= row.top(),
                        "nearest capped parent must exclude the entire crossing child line/object"
                    );
                    assert!(
                        child_paint.mask.bottom() < outer.bounds.bottom(),
                        "the real final paint mask must be stricter than the cap box"
                    );
                    for id in links(&clipped) {
                        for (_, rect) in fragments(&clipped, id) {
                            assert!(logical(rect, window).bottom() <= child_paint.mask.bottom());
                        }
                    }
                    break 'measured_padding;
                }
            }
        }
        assert!(
            crossed,
            "fixture must reach a real cap-crossing line/object, never silently pass without one"
        );
    });
}

#[gpui::test]
fn capped_parent_receives_actual_glyph_spans_through_an_ordinary_uncapped_custom_textview(
    cx: &mut TestAppContext,
) {
    nested_uncapped_metrics_case(cx, false);
}

#[gpui::test]
fn capped_parent_receives_snapped_object_spans_through_an_ordinary_uncapped_custom_textview(
    cx: &mut TestAppContext,
) {
    nested_uncapped_metrics_case(cx, true);
}

const DEFERRED_ORDINARY_COLOR: u32 = 0x2a7b93;

fn scene_quads(
    window: &Window,
    color: u32,
) -> Vec<(Bounds<gpui::ScaledPixels>, Bounds<gpui::ScaledPixels>)> {
    let color: gpui::Background = gpui::rgb(color).into();
    window
        .painted_quads()
        .into_iter()
        .filter(|quad| quad.background == color)
        .map(|quad| (quad.bounds, quad.content_mask.bounds))
        .collect()
}

struct DeferredRollbackGroup {
    deferred: Entity<DeferredRollbackPanel>,
    ordinary: Entity<PlainPanel>,
    renders: Arc<AtomicUsize>,
}
impl Render for DeferredRollbackGroup {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.fetch_add(1, Ordering::SeqCst);
        div()
            // Cached bounds are offered as available space, not inherited as
            // root styles. Fill them so percentage-sized cached children have
            // a definite parent instead of an intrinsic percentage cycle.
            .size_full()
            .flex()
            .flex_col()
            .child(
                self.deferred
                    .clone()
                    .cached(StyleRefinement::default().w_full().h(px(80.))),
            )
            .child(
                self.ordinary
                    .clone()
                    .cached(StyleRefinement::default().w_full().h(px(50.))),
            )
    }
}

struct DeferredRollbackPanel {
    body: Entity<TextViewState>,
    calls: Calls,
    renders: Arc<AtomicUsize>,
    attempts: Arc<AtomicUsize>,
    paints: Samples,
    speculate: bool,
}
impl Render for DeferredRollbackPanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.fetch_add(1, Ordering::SeqCst);
        deferred(DiscardLinkDuringDeferredPrepaint {
            body: self.body.clone(),
            calls: self.calls.clone(),
            attempts: self.attempts.clone(),
            paints: self.paints.clone(),
            speculate: self.speculate,
        })
    }
}

struct DiscardLinkDuringDeferredPrepaint {
    body: Entity<TextViewState>,
    calls: Calls,
    attempts: Arc<AtomicUsize>,
    paints: Samples,
    speculate: bool,
}
impl IntoElement for DiscardLinkDuringDeferredPrepaint {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for DiscardLinkDuringDeferredPrepaint {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        Some("discarded-deferred-links".into())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let mut ordinary = ObservePaint::new(
            div()
                .id("surviving-deferred-ordinary")
                .role(Role::Label)
                .aria_label("surviving ordinary deferred content")
                .bg(gpui::rgb(DEFERRED_ORDINARY_COLOR))
                .w_full()
                .h(px(24.))
                .child("ordinary deferred content"),
            &self.paints,
        )
        .into_any_element();
        (ordinary.request_layout(window, cx), ordinary)
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        ordinary: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.speculate {
            let discarded: Result<(), ()> = window.transact(|window| {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                let mut linked = TextView::new(&self.body)
                    .on_link_click(recorder(&self.calls))
                    .into_any_element();
                linked.prepaint_as_root(
                    bounds.origin,
                    size(
                        AvailableSpace::Definite(bounds.size.width),
                        AvailableSpace::MinContent,
                    ),
                    window,
                    cx,
                );
                Err(())
            });
            assert!(discarded.is_err());
        }
        ordinary.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        ordinary: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        ordinary.paint(window, cx);
    }
}

fn deferred_all_links_rollback_case(cx: &mut TestAppContext, active: bool) {
    let (page, cx) = setup(cx, SOURCE, active);
    cx.update(|window, cx| {
        let group_renders = Arc::new(AtomicUsize::new(0));
        let panel_renders = Arc::new(AtomicUsize::new(0));
        let sibling_renders = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::new(AtomicUsize::new(0));
        let paints: Samples = Arc::default();
        let body = page.read(cx).body.clone();
        let calls = page.read(cx).calls.clone();
        let panel = cx.new(|_| DeferredRollbackPanel {
            body,
            calls,
            renders: panel_renders.clone(),
            attempts: attempts.clone(),
            paints: paints.clone(),
            speculate: true,
        });
        let sibling = cx.new(|_| PlainPanel {
            renders: sibling_renders.clone(),
        });
        let group = cx.new(|_| DeferredRollbackGroup {
            deferred: panel.clone(),
            ordinary: sibling,
            renders: group_renders.clone(),
        });
        page.update(cx, |page, cx| {
            page.deferred_rollback_group = Some(group);
            cx.notify();
        });
        assert!(window.begin_a11y_link_observation_for_test());
        window.draw(cx).clear(cx);
        let first = observation(window);
        assert_eq!(first.interval.draws_accepted, 1);
        assert_eq!(
            first.interval.registrations, 1,
            "one real Base Link was prepainted in the failed deferred transaction"
        );
        assert_eq!(
            first.interval.clipper_calls, 1,
            "the real source scope finalized before rollback"
        );
        assert!(first.interval.transaction_visits > 0);
        assert!(
            first.registrations.is_empty(),
            "all Link attempts rolled back; accepted frame has none"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(group_renders.load(Ordering::SeqCst), 1);
        assert_eq!(panel_renders.load(Ordering::SeqCst), 1);
        assert_eq!(sibling_renders.load(Ordering::SeqCst), 1);
        assert_eq!(
            paints.lock().expect("ordinary paint").len(),
            1,
            "the retained ordinary child actually painted after deferred rollback"
        );
        let painted = *paints
            .lock()
            .expect("ordinary paint")
            .last()
            .expect("one paint");
        assert!(
            !painted.bounds.intersect(&painted.mask).is_empty(),
            "ordinary deferred content must receive visible actual layout bounds: {painted:?}"
        );
        let first_quads = scene_quads(window, DEFERRED_ORDINARY_COLOR);
        assert_eq!(
            first_quads.len(),
            1,
            "positive scene control: actual visible ordinary deferred paint; observed {painted:?}"
        );
        assert!(!first_quads[0].0.intersect(&first_quads[0].1).is_empty());
        // This unchanged draw is the genuine Core2 replay reproduction. With
        // Core3, nonrewound registration ranges require a speculative rerender.
        window.draw(cx).clear(cx);
        let attempted = observation(window);
        assert_eq!(attempted.interval.draws_accepted, 1);
        assert_eq!(attempted.interval.registrations, 1);
        assert_eq!(attempted.interval.clipper_calls, 1);
        assert!(attempted.interval.transaction_visits > 0);
        assert!(attempted.registrations.is_empty());
        assert_eq!(group_renders.load(Ordering::SeqCst), 2);
        assert_eq!(panel_renders.load(Ordering::SeqCst), 2);
        assert_eq!(sibling_renders.load(Ordering::SeqCst), 1);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(paints.lock().expect("ordinary paint").len(), 2);
        assert_eq!(scene_quads(window, DEFERRED_ORDINARY_COLOR), first_quads);

        // A real notified state update ends speculation. Accept an ordinary-only
        // frame before measuring reuse; a source notification may refresh siblings.
        panel.update(cx, |panel, cx| {
            panel.speculate = false;
            cx.notify();
        });
        window.draw(cx).clear(cx);
        let recovered = observation(window);
        assert_eq!(recovered.interval.draws_accepted, 1);
        assert_eq!(recovered.interval.registrations, 0);
        assert_eq!(recovered.interval.clipper_calls, 0);
        assert!(recovered.registrations.is_empty());
        assert_eq!(group_renders.load(Ordering::SeqCst), 3);
        assert_eq!(panel_renders.load(Ordering::SeqCst), 3);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(paints.lock().expect("ordinary paint").len(), 3);
        assert_eq!(scene_quads(window, DEFERRED_ORDINARY_COLOR), first_quads);
        let stable = (
            group_renders.load(Ordering::SeqCst),
            panel_renders.load(Ordering::SeqCst),
            sibling_renders.load(Ordering::SeqCst),
            attempts.load(Ordering::SeqCst),
            paints.lock().expect("ordinary paint").len(),
        );
        for _ in 0..3 {
            window.draw(cx).clear(cx);
            let next = observation(window);
            assert_eq!(next.interval.draws_accepted, 1);
            assert_eq!(next.interval.registrations, 0);
            assert_eq!(next.interval.clipper_calls, 0);
            assert!(next.registrations.is_empty());
            assert_eq!(group_renders.load(Ordering::SeqCst), stable.0);
            assert_eq!(panel_renders.load(Ordering::SeqCst), stable.1);
            assert_eq!(sibling_renders.load(Ordering::SeqCst), stable.2);
            assert_eq!(attempts.load(Ordering::SeqCst), stable.3);
            assert_eq!(
                paints.lock().expect("ordinary paint").len(),
                stable.4,
                "cache replay retains the scene without executing the child's paint again"
            );
            assert_eq!(
                scene_quads(window, DEFERRED_ORDINARY_COLOR),
                first_quads,
                "ordinary deferred paint must survive every unchanged cached draw"
            );
        }
        assert_eq!(call_count(&page, cx), 0);
    });
}

#[gpui::test]
fn cached_deferred_all_link_attempts_rollback_retains_ordinary_content_when_active(
    cx: &mut TestAppContext,
) {
    deferred_all_links_rollback_case(cx, true);
}

#[gpui::test]
fn cached_deferred_all_link_attempts_rollback_retains_ordinary_content_when_inactive(
    cx: &mut TestAppContext,
) {
    deferred_all_links_rollback_case(cx, false);
}

fn observation(window: &mut Window) -> A11yLinkTestObservation {
    let result = window
        .take_a11y_link_observation_for_test()
        .expect("idle observed test window");
    assert_eq!(
        result.interval.tree_defect_events, 0,
        "valid trees must not rely on repair"
    );
    assert_eq!(
        result.interval.raw_link_replays, 0,
        "registered branches must not raw-replay"
    );
    result
}

fn node(tree: &TreeUpdate, id: NodeId) -> &Node {
    &tree
        .nodes
        .iter()
        .find(|(candidate, _)| *candidate == id)
        .expect("referenced node in completed tree")
        .1
}

fn links(tree: &TreeUpdate) -> Vec<NodeId> {
    tree.nodes
        .iter()
        .filter(|(_, node)| node.role() == Role::Link)
        .map(|(id, _)| *id)
        .collect()
}

fn link(tree: &TreeUpdate, label: &str) -> NodeId {
    let matches: Vec<_> = links(tree)
        .into_iter()
        .filter(|id| node(tree, *id).label() == Some(label))
        .collect();
    assert_eq!(matches.len(), 1, "exactly one parsed link named {label}");
    matches[0]
}

fn fragments(tree: &TreeUpdate, id: NodeId) -> Vec<(NodeId, Rect)> {
    node(tree, id)
        .children()
        .iter()
        .map(|id| {
            let leaf = node(tree, *id);
            assert_eq!(leaf.role(), Role::Label);
            assert!(
                leaf.is_read_only(),
                "fragment leaves carry the required read-only property"
            );
            assert!(!leaf.supports_action(Action::Click));
            assert!(!leaf.supports_action(Action::Focus));
            let bounds = leaf.bounds().expect("fragment geometry");
            assert!(bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0);
            (*id, bounds)
        })
        .collect()
}

fn request(window: &mut Window, cx: &mut App, target: NodeId, action: Action) {
    window.dispatch_accessibility_action_for_test(
        ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: target,
            data: None,
        },
        cx,
    );
}

fn key(window: &mut Window, cx: &mut App, key: &str) {
    let keystroke = Keystroke::parse(key).expect("test keystroke");
    window.dispatch_event(
        KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        }
        .to_platform_input(),
        cx,
    );
    window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), cx);
}

fn logical(rect: Rect, window: &Window) -> Bounds<Pixels> {
    let scale = f64::from(window.scale_factor());
    Bounds::from_corners(
        point(px((rect.x0 / scale) as f32), px((rect.y0 / scale) as f32)),
        point(px((rect.x1 / scale) as f32), px((rect.y1 / scale) as f32)),
    )
}

// Publication multiplies each actual f32 edge before widening to f64.
// Compare in that space without dividing or reconstructing a logical size.
fn physical_rect(bounds: Bounds<Pixels>, scale: f32) -> Rect {
    Rect::new(
        f64::from(f32::from(bounds.left()) * scale),
        f64::from(f32::from(bounds.top()) * scale),
        f64::from(f32::from(bounds.right()) * scale),
        f64::from(f32::from(bounds.bottom()) * scale),
    )
}

fn call_count(page: &Entity<Page>, cx: &App) -> usize {
    page.read(cx).calls.lock().expect("activation log").len()
}

fn focus_link(page: &Entity<Page>, window: &mut Window, cx: &mut App, label: &str) -> NodeId {
    let tree = draw(window, cx);
    let id = link(&tree, label);
    request(window, cx, id, Action::Focus);
    assert!(!page.read(cx).before.is_focused(window));
    let focused = draw(window, cx); // Install real focused key listeners.
    assert_eq!(focused.focus, id);
    id
}

#[gpui::test]
fn real_tree_click_focus_enter_and_fragment_pointer_use_the_application_handler(
    cx: &mut TestAppContext,
) {
    let (page, cx) = setup(cx, SOURCE, true);
    let pointer = cx.update(|window, cx| {
        let id = focus_link(&page, window, cx, "alpha");
        let tree = window.accessibility_tree_for_test().expect("tree").clone();
        assert_eq!(node(&tree, id).url(), Some(URL));
        assert!(node(&tree, id).supports_action(Action::Click));
        let visible = fragments(&tree, id);
        assert!(!visible.is_empty());
        let message = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == Role::Paragraph && node.label() == Some("message body")
            })
            .expect("application-owned paragraph");
        assert!(
            message.1.children().contains(&id),
            "link retains its original message ancestry"
        );
        request(window, cx, visible[0].0, Action::Click);
        assert_eq!(
            call_count(&page, cx),
            0,
            "read-only fragment has no Click fallback"
        );
        request(window, cx, visible[0].0, Action::Focus);
        assert_eq!(
            draw(window, cx).focus,
            id,
            "fragment cannot receive keyboard focus"
        );
        request(window, cx, id, Action::Click);
        assert_eq!(call_count(&page, cx), 1, "explicit accessibility Click");
        key(window, cx, "enter");
        assert_eq!(call_count(&page, cx), 2, "real keydown plus keyup");
        let calls = page.read(cx).calls.lock().expect("activation log");
        for call in calls.iter() {
            assert_eq!(call.url, URL);
            assert_eq!(call.keyboard_bounds, Some(logical(visible[0].1, window)));
        }
        logical(visible[0].1, window).center()
    });
    cx.simulate_mouse_move(pointer, None, Default::default());
    cx.simulate_click(pointer, Default::default());
    cx.update(|_, cx| {
        let calls = page.read(cx).calls.lock().expect("activation log");
        assert_eq!(
            calls.len(),
            3,
            "zero-area host does not intercept the real text hit"
        );
        assert_eq!(calls[2].url, URL);
        assert!(
            calls[2].keyboard_bounds.is_none(),
            "pointer callback remains a mouse event"
        );
        assert_eq!(calls[2].position, pointer);
    });
}

fn reject_before_redraw(
    cx: &mut TestAppContext,
    edit: impl FnOnce(&Entity<Page>, &mut App) + 'static,
) {
    let (page, cx) = setup(cx, SOURCE, true);
    cx.update(|window, cx| {
        let old_id = focus_link(&page, window, cx, "alpha");
        let old_handle = window.focused(cx).expect("focused link handle");
        let captured = Rc::new(RefCell::new(None));
        let captured_edit = captured.clone();
        let edit_page = page.clone();
        let edit_handle = old_handle.clone();
        let pending = page.read(cx).key_up_edit.clone();
        *pending.borrow_mut() = Some(Box::new(move |window, cx| {
            assert!(edit_handle.is_focused(window), "old Link receives Enter");
            let observed = observation(window);
            let accepted_draw = observed.accepted_draw.expect("current accepted frame");
            let frame = observed
                .registrations
                .into_iter()
                .find(|entry| entry.root == old_id)
                .expect("old Link is registered immediately before the captured edit")
                .frame;
            assert!(frame.activation_bounds(window, cx).is_some());
            // Capture runs after key-up's predispatch draw check, before the
            // existing Link's bubbling key-up click handler.
            let paints = edit_page
                .read(cx)
                .frame_samples
                .lock()
                .expect("frames")
                .len();
            edit(&edit_page, cx);
            *captured_edit.borrow_mut() = Some((paints, accepted_draw, frame));
        }));
        key(window, cx, "enter");
        assert!(
            pending.borrow().is_none(),
            "one-shot capture consumed the edit"
        );
        let (paints, accepted_draw, old_frame) = captured
            .borrow_mut()
            .take()
            .expect("positive control: root capture actually mutated the source");
        assert!(old_frame.activation_bounds(window, cx).is_none());
        assert_eq!(
            call_count(&page, cx),
            0,
            "old focused keyboard handler refuses"
        );
        assert_eq!(
            page.read(cx).ancestor_clicks.load(Ordering::SeqCst),
            0,
            "rejection must consume the link-addressed keyboard click"
        );
        request(window, cx, old_id, Action::Click);
        assert_eq!(
            call_count(&page, cx),
            0,
            "old explicit accessibility handler refuses"
        );
        let before = page.read(cx).before.clone();
        before.focus(window, cx);
        request(window, cx, old_id, Action::Focus);
        assert!(
            before.is_focused(window),
            "built-in Focus rejects the retired source"
        );
        window.focus(&old_handle, cx);
        assert!(
            before.is_focused(window),
            "direct FocusHandle route also rejects"
        );
        window.focus_next(cx);
        assert!(
            page.read(cx).after.is_focused(window),
            "Tab skips stale link without entering it"
        );
        window.focus_prev(cx);
        assert!(before.is_focused(window), "reverse Tab also skips it");
        assert_eq!(
            page.read(cx).frame_samples.lock().expect("frames").len(),
            paints
        );
        let unchanged = observation(window);
        assert_eq!(unchanged.accepted_draw, Some(accepted_draw));
        assert_eq!(unchanged.interval.draws_started, 0);
        assert_eq!(unchanged.interval.draws_accepted, 0);
        assert!(old_frame.activation_bounds(window, cx).is_none());
        assert_eq!(call_count(&page, cx), 0);
        assert_eq!(page.read(cx).ancestor_clicks.load(Ordering::SeqCst), 0);
    });
}

#[gpui::test]
fn ordinary_replacement_before_redraw_revokes_every_input_route(cx: &mut TestAppContext) {
    reject_before_redraw(cx, |page, cx| {
        page.read(cx)
            .body
            .clone()
            .update(cx, |state, cx| state.set_text("replacement", cx));
    });
}

#[gpui::test]
fn real_aba_replacement_does_not_revive_old_actions(cx: &mut TestAppContext) {
    reject_before_redraw(cx, |page, cx| {
        page.read(cx).body.clone().update(cx, |state, cx| {
            state.set_text("different document B", cx);
            assert!(
                state
                    .rendered_text()
                    .as_str()
                    .contains("different document B"),
                "positive control: B really becomes the committed parsed document"
            );
            state.set_text(SOURCE, cx);
            assert!(
                state.rendered_text().as_str().contains("alpha"),
                "positive control: A is actually reparsed after B"
            );
        });
    });
}

#[gpui::test]
fn parser_configuration_replacement_revokes_without_a_draw(cx: &mut TestAppContext) {
    reject_before_redraw(cx, |page, cx| {
        let extensions = MarkdownExtensions::default()
            .parser_revision(77)
            .frontmatter();
        page.read(cx).body.clone().update(cx, |state, cx| {
            state.set_markdown_extensions(Arc::new(extensions), cx);
        });
    });
}

#[gpui::test]
fn same_text_noop_and_pending_append_keep_the_accepted_prefix_usable(cx: &mut TestAppContext) {
    let (page, cx) = setup(cx, SOURCE, true);
    cx.update(|window, cx| {
        let id = focus_link(&page, window, cx, "alpha");
        let link_focus = window.focused(cx).expect("link focus");
        let body = page.read(cx).body.clone();
        body.update(cx, |state, cx| state.set_text(SOURCE, cx));
        page.read(cx).before.clone().focus(window, cx);
        request(window, cx, id, Action::Focus);
        assert!(
            link_focus.is_focused(window),
            "no-op preserves accessible Focus"
        );
        page.read(cx).before.clone().focus(window, cx);
        key(window, cx, "tab");
        assert!(
            link_focus.is_focused(window),
            "no-op preserves the existing Tab stop"
        );
        request(window, cx, id, Action::Click);
        key(window, cx, "enter");
        assert_eq!(
            call_count(&page, cx),
            2,
            "same-text no-op retains accepted ownership"
        );
        body.update(cx, |state, cx| state.push_str("\n\nnew tail", cx));
        request(window, cx, id, Action::Click);
        key(window, cx, "enter");
        assert_eq!(
            call_count(&page, cx),
            4,
            "append-only work preserves committed-prefix authority"
        );
        assert_eq!(
            link(&draw(window, cx), "alpha"),
            id,
            "a later draw keeps the parsed prefix's stable host identity"
        );
    });
    // The old and new frames can share a NodeId. Public actions address that
    // current ID, so this cannot assert rejection of the private old permit.
}

#[gpui::test]
fn pending_large_replacement_frame_cannot_revive_when_parsing_finishes(cx: &mut TestAppContext) {
    let (page, cx) = setup(cx, SOURCE, true);
    let pending_frames = cx.update(|window, cx| {
        let id = focus_link(&page, window, cx, "alpha");
        let body = page.read(cx).body.clone();
        // Size deliberately exceeds the documented 4 KiB synchronous cutoff.
        // No scheduler work is pumped inside this update.
        let replacement = format!("NEW DOCUMENT\n\n{}", "replacement prose ".repeat(600));
        body.update(cx, |state, cx| state.set_text(&replacement, cx));
        assert!(
            body.read(cx).rendered_text().as_str().contains("alpha"),
            "positive control: old committed content is still present"
        );
        let pending = draw(window, cx);
        assert!(
            links(&pending).is_empty(),
            "entry-invalid old document publishes no Link"
        );
        request(window, cx, id, Action::Click);
        assert_eq!(call_count(&page, cx), 0);
        let observed = observation(window);
        let accepted_draw = observed.accepted_draw.expect("accepted pending frame");
        let pending_frame = observed
            .registrations
            .into_iter()
            .find(|entry| entry.root == id)
            .expect("old committed link actually registered while pending")
            .frame;
        assert!(pending_frame.activation_bounds(window, cx).is_none());
        (
            id,
            page.read(cx).frame_samples.lock().expect("frames").len(),
            pending_frame,
            accepted_draw,
        )
    });
    // Earlier draws supply all initial positives. This public test-App gate is
    // permanent, so no later manual or scheduler draw can accept a new frame.
    cx.skip_drawing();
    cx.run_until_parked();
    cx.update(|window, cx| {
        let body = page.read(cx).body.clone();
        assert!(
            body.read(cx)
                .rendered_text()
                .as_str()
                .starts_with("NEW DOCUMENT"),
            "positive control: the actual parse result has committed"
        );
        assert_eq!(
            page.read(cx).frame_samples.lock().expect("frames").len(),
            pending_frames.1,
            "no intervening accepted draw is allowed in the entry-latch experiment"
        );
        let unchanged = observation(window);
        assert_eq!(unchanged.accepted_draw, Some(pending_frames.3));
        assert_eq!(unchanged.interval.draws_started, 0);
        assert_eq!(unchanged.interval.draws_accepted, 0);
        assert!(pending_frames.2.activation_bounds(window, cx).is_none());
        request(window, cx, pending_frames.0, Action::Focus);
        request(window, cx, pending_frames.0, Action::Click);
        key(window, cx, "enter");
        assert!(
            pending_frames.2.activation_bounds(window, cx).is_none(),
            "false scope-entry latch cannot revive when parse readiness later becomes true"
        );
        assert_eq!(
            call_count(&page, cx),
            0,
            "parse completion cannot revive the pending frame"
        );
        assert_eq!(
            page.read(cx).frame_samples.lock().expect("frames").len(),
            pending_frames.1
        );
        let unchanged = observation(window);
        assert_eq!(unchanged.accepted_draw, Some(pending_frames.3));
        assert_eq!(unchanged.interval.draws_started, 0);
        assert_eq!(unchanged.interval.draws_accepted, 0);
    });
}

#[gpui::test]
fn equal_urls_wrapping_code_and_objects_keep_distinct_owners_and_single_stops(
    cx: &mut TestAppContext,
) {
    let source = "[first](https://example.invalid/a) $neighbor$ [second](https://example.invalid/a)\n\n\
        ```text\ncode action neighbor\n```\n\n\
        [long **bold words** `code` $object$ continuing through several wrapped lines](https://example.invalid/b)";
    let (page, cx) = setup(cx, source, true);
    cx.update(|window, cx| {
        let samples = Arc::default();
        page.update(cx, |page, cx| {
            page.extensions = object_extensions(&samples);
            page.code_neighbor = true;
            cx.notify();
        });
        let tree = draw(window, cx);
        let ids = links(&tree);
        assert_eq!(
            ids.len(),
            3,
            "splits through styled text/code/object retain one owner"
        );
        let first = link(&tree, "first");
        let second = link(&tree, "second");
        assert_ne!(first, second, "equal URLs do not merge parsed owners");
        assert_eq!(node(&tree, first).url(), node(&tree, second).url());
        let wrapped = *ids
            .iter()
            .find(|id| **id != first && **id != second)
            .expect("wrapped owner");
        assert!(
            fragments(&tree, wrapped).len() > 1,
            "real shaping creates several fragments"
        );
        let before = page.read(cx).before.clone();
        before.focus(window, cx);
        let mut focused = Vec::new();
        let inline = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("inline neighbor"))
            .expect("real inline control")
            .0;
        let code = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("code neighbor"))
            .expect("real code action control")
            .0;
        for _ in 0..5 {
            key(window, cx, "tab");
            focused.push(draw(window, cx).focus);
        }
        assert_eq!(
            focused,
            // InlineFlow's existing controls paint before its local metadata
            // host group. The group is never deferred or moved by finalization.
            vec![inline, first, second, code, wrapped],
            "one stop per link and original local order around real inline/code controls"
        );
        key(window, cx, "tab");
        assert!(page.read(cx).after.is_focused(window));
    });
}

#[gpui::test]
fn final_max_lines_mask_controls_partial_paragraph_and_object_semantics(cx: &mut TestAppContext) {
    let source = "[lead](https://example.invalid/lead)\n\n\
        [many words wrap through the later paragraph $object$ and continue across several lines to its end](https://example.invalid/tail)";
    let (page, cx) = setup(cx, source, true);
    cx.update(|window, cx| {
        let samples: Samples = Arc::default();
        page.update(cx, |page, cx| {
            page.extensions = object_extensions(&samples);
            cx.notify();
        });
        let full = draw(window, cx);
        let full_links = links(&full);
        let tail = *full_links
            .iter()
            .find(|id| node(&full, **id).url() == Some("https://example.invalid/tail"))
            .expect("tail link in unclamped document");
        request(window, cx, tail, Action::Focus);
        draw(window, cx);
        let tail_focus = window.focused(cx).expect("focused later paragraph");
        let mut witnessed_stricter_mask = false;
        let mut witnessed_hidden_link = false;
        let mut witnessed_filtered_candidate = false;
        for cap in 1..=7 {
            samples.lock().expect("samples").clear();
            page.update(cx, |page, cx| {
                page.max_lines = Some(cap);
                cx.notify();
            });
            drop(observation(window));
            window.draw(cx).clear(cx);
            let clipped = window
                .accessibility_tree_for_test()
                .expect("clipped complete tree")
                .clone();
            let observed = observation(window);
            assert_eq!(
                observed.interval.clipper_calls, observed.interval.registrations,
                "valid geometric clipping calls every actual finalizer once, including hidden roots"
            );
            assert_registration_geometry(&observed, &clipped, window, cx);
            witnessed_filtered_candidate |= observed
                .registrations
                .iter()
                .any(|entry| entry.candidates.len() > entry.surviving_keys.len());
            let object = *samples
                .lock()
                .expect("samples")
                .last()
                .expect("object actually painted");
            let outer = *page
                .read(cx)
                .outer_samples
                .lock()
                .expect("outer")
                .last()
                .expect("TextView paint");
            if object.mask.bottom() < outer.bounds.bottom() {
                witnessed_stricter_mask = true;
            }
            for id in links(&clipped) {
                for (_, rect) in fragments(&clipped, id) {
                    let bounds = logical(rect, window);
                    assert!(bounds.top() >= object.mask.top());
                    assert!(
                        bounds.bottom() <= object.mask.bottom(),
                        "fragment must fit the real final paint mask, not merely the capped box"
                    );
                    assert!(bounds.left() >= object.mask.left());
                    assert!(bounds.right() <= object.mask.right());
                }
            }
            for old in &full_links {
                if !links(&clipped).contains(old) {
                    witnessed_hidden_link = true;
                    if *old == tail {
                        assert!(
                            !tail_focus.is_focused(window),
                            "final clip revokes hidden focused link"
                        );
                    }
                    let before = page.read(cx).before.clone();
                    before.focus(window, cx);
                    request(window, cx, *old, Action::Focus);
                    assert!(before.is_focused(window));
                    let count = call_count(&page, cx);
                    request(window, cx, *old, Action::Click);
                    assert_eq!(call_count(&page, cx), count);
                }
            }
            page.read(cx).before.clone().focus(window, cx);
            for _ in 0..links(&clipped).len() {
                key(window, cx, "tab");
            }
            key(window, cx, "tab");
            assert!(
                page.read(cx).after.is_focused(window),
                "final hidden geometry cannot add a Tab stop"
            );
        }
        assert!(
            witnessed_stricter_mask,
            "positive control must encounter the whole-line clip regression"
        );
        assert!(
            witnessed_hidden_link,
            "at least one fully hidden parsed owner must be exercised"
        );
        assert!(
            witnessed_filtered_candidate,
            "real final clipping must remove an original candidate"
        );
    });
}

#[gpui::test]
fn current_first_visible_fragment_supplies_activation_bounds_after_real_parent_clipping(
    cx: &mut TestAppContext,
) {
    let source = "[many words continue across several actual wrapped lines for the geometry oracle](https://example.invalid/a)";
    let (page, cx) = setup(cx, source, true);
    cx.update(|window, cx| {
        let full = draw(window, cx);
        let id = links(&full)[0];
        let original = fragments(&full, id);
        assert!(
            original.len() >= 3,
            "positive control: several shaped lines"
        );
        let second = logical(original[1].1, window);
        // Drive a real overflow-hidden parent. The offset is measured from
        // the shaped output; it is not a replacement rectangle for a candidate.
        let outer = *page
            .read(cx)
            .outer_samples
            .lock()
            .expect("outer")
            .last()
            .expect("paint");
        page.update(cx, |page, cx| {
            page.viewport_height = Some(outer.bounds.size.height);
            page.offset = second.top() - outer.bounds.top();
            cx.notify();
        });
        let clipped = draw(window, cx);
        let survivors = fragments(&clipped, id);
        assert!(!survivors.is_empty());
        assert!(
            page.read(cx).offset > px(0.),
            "fixture actually scrolls shaped content"
        );
        // This asserts the public representative against the current tree.
        // It does not inspect candidate keys before this frame's finalizer.
        request(window, cx, id, Action::Click);
        let calls = page.read(cx).calls.lock().expect("calls");
        assert_eq!(
            calls.last().expect("activation").keyboard_bounds,
            Some(logical(survivors[0].1, window))
        );
    });
}

#[gpui::test]
fn hiding_a_focused_link_blurs_only_after_the_accepted_draw(cx: &mut TestAppContext) {
    let (page, cx) = setup(cx, SOURCE, true);
    cx.update(|window, cx| {
        let id = focus_link(&page, window, cx, "alpha");
        let old_focus = window.focused(cx).expect("link focus");
        page.update(cx, |page, cx| {
            page.visible = false;
            cx.notify();
        });
        assert!(
            old_focus.is_focused(window),
            "no speculative prepaint focus side effect"
        );
        assert!(links(&draw(window, cx)).is_empty());
        assert!(
            !old_focus.is_focused(window),
            "accepted unmount retires focus"
        );
        assert!(!page.read(cx).before.is_focused(window));
        assert!(
            !page.read(cx).after.is_focused(window),
            "no automatic replacement focus"
        );
        // Page still owns body; this is an unmount test, not entity destruction.
        assert!(
            page.read(cx)
                .body
                .read(cx)
                .rendered_text()
                .as_str()
                .contains("alpha")
        );
        request(window, cx, id, Action::Focus);
        request(window, cx, id, Action::Click);
        assert_eq!(call_count(&page, cx), 0);
        let before = page.read(cx).before.clone();
        before.focus(window, cx);
        key(window, cx, "tab");
        assert!(page.read(cx).after.is_focused(window));
    });
}

fn nested_extensions(
    child: &Entity<TextViewState>,
    calls: &Calls,
    depth: usize,
) -> MarkdownExtensions {
    let child = child.clone();
    let calls = calls.clone();
    MarkdownExtensions::default()
        .block_parser(|node, _| {
            matches!(node, markdown_ast::Node::Code(_))
                .then(|| MarkdownNode::new("nested", ()).text("retained child"))
        })
        .block_renderer("nested", move |_, _, _| {
            let mut element = TextView::new(&child)
                .on_link_click(recorder(&calls))
                .into_any_element();
            for _ in 0..depth {
                element = deferred(element).into_any_element();
            }
            element
        })
}

fn nested_source_case(cx: &mut TestAppContext, depth: usize) {
    let (page, cx) = setup(cx, "```child\nretained\n```", true);
    cx.update(|window, cx| {
        let child = cx.new(|cx| TextViewState::markdown(SOURCE, cx));
        let calls = page.read(cx).calls.clone();
        page.update(cx, |page, cx| {
            page.extensions = nested_extensions(&child, &calls, depth);
            page.retained_child = Some(child);
            cx.notify();
        });
        let id = focus_link(&page, window, cx, "alpha");
        page.read(cx)
            .body
            .clone()
            .update(cx, |state, cx| state.set_text("new parent", cx));
        key(window, cx, "enter");
        request(window, cx, id, Action::Click);
        assert_eq!(
            call_count(&page, cx),
            0,
            "nearest child is unchanged; ancestor must still revoke"
        );
        assert!(
            page.read(cx)
                .retained_child
                .as_ref()
                .expect("retained child")
                .read(cx)
                .rendered_text()
                .as_str()
                .contains("alpha")
        );
    });
}

#[gpui::test]
fn retained_nested_child_cannot_bypass_a_replaced_ancestor(cx: &mut TestAppContext) {
    nested_source_case(cx, 0);
}
#[gpui::test]
fn deferred_text_view_inherits_the_original_ancestor_source_guard(cx: &mut TestAppContext) {
    nested_source_case(cx, 1);
}
#[gpui::test]
fn nested_deferred_text_view_keeps_inherited_source_ancestry(cx: &mut TestAppContext) {
    nested_source_case(cx, 2);
}

#[gpui::test]
fn inactive_keyboard_then_activation_builds_a_fresh_real_tree(cx: &mut TestAppContext) {
    let (page, cx) = setup(cx, SOURCE, false);
    cx.update(|window, cx| {
        assert!(
            !window.is_a11y_active(),
            "positive control: accessibility starts inactive"
        );
        window.draw(cx).clear(cx);
        let before = page.read(cx).before.clone();
        before.focus(window, cx);
        key(window, cx, "tab");
        assert!(!before.is_focused(window));
        assert!(!page.read(cx).after.is_focused(window));
        window.draw(cx).clear(cx);
        key(window, cx, "enter");
        assert_eq!(
            call_count(&page, cx),
            1,
            "inactive a11y retains keyboard permits"
        );
        window.activate_accessibility_for_test();
        let fresh = draw(window, cx);
        assert_eq!(links(&fresh).len(), 1);
        assert_eq!(node(&fresh, fresh.focus).role(), Role::Link);
    });
}

#[gpui::test]
fn force_disabled_windows_still_apply_source_guards_to_keyboard(cx: &mut TestAppContext) {
    cx.disable_accessibility();
    let (page, cx) = setup(cx, SOURCE, false);
    cx.update(|window, cx| {
        assert!(
            !window.is_a11y_enabled(),
            "positive control: this window is force-disabled"
        );
        window.draw(cx).clear(cx);
        page.read(cx).before.clone().focus(window, cx);
        key(window, cx, "tab");
        window.draw(cx).clear(cx);
        key(window, cx, "enter");
        assert_eq!(call_count(&page, cx), 1, "force-disabled positive control");
        page.read(cx)
            .body
            .clone()
            .update(cx, |state, cx| state.set_text("replacement", cx));
        key(window, cx, "enter");
        assert_eq!(
            call_count(&page, cx),
            1,
            "same guard applies without AccessKit nodes"
        );
        page.read(cx).before.clone().focus(window, cx);
        key(window, cx, "tab");
        assert!(page.read(cx).after.is_focused(window));
    });
}

/// Fresh actual elements for failed attempts avoid reusing a painted Drawable.
/// The parent TextView's source scope encloses these public transactions.
struct Retry {
    factory: ElementFactory,
    accepted_factory: Option<ElementFactory>,
    attempts: Arc<AtomicUsize>,
    nested: bool,
    accepted_count: Option<Arc<AtomicUsize>>,
}

type ElementFactory = Arc<dyn Fn() -> AnyElement + Send + Sync>;

impl IntoElement for Retry {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Retry {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        Some("same-retry-host".into())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let mut child = (self.accepted_factory.as_ref().unwrap_or(&self.factory))();
        let layout = child.request_layout(window, cx);
        (layout, child)
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut attempt = |window: &mut Window| {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let mut child = (self.factory)();
            child.prepaint_as_root(
                bounds.origin,
                size(
                    AvailableSpace::Definite(bounds.size.width),
                    AvailableSpace::Definite(bounds.size.height),
                ),
                window,
                cx,
            );
        };
        let outer: Result<(), ()> = window.transact(|window| {
            if self.nested {
                let inner: Result<(), ()> = window.transact(|window| {
                    attempt(window);
                    Err(())
                });
                assert!(inner.is_err());
            }
            attempt(window);
            Err(())
        });
        assert!(outer.is_err());
        if let Some(count) = &self.accepted_count {
            count.fetch_add(1, Ordering::SeqCst);
        }
        child.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        child: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        child.paint(window, cx);
    }
}

#[gpui::test]
fn nested_err_then_same_id_retry_rolls_back_real_inline_objects_and_preserves_siblings(
    cx: &mut TestAppContext,
) {
    let source = "[earlier](https://example.invalid/earlier)\n\n```retry\nchild\n```\n\n[afterward](https://example.invalid/afterward)";
    let (page, cx) = setup(cx, source, true);
    cx.update(|window, cx| {
        let child = cx.new(|cx| {
            TextViewState::markdown("[retry $object$ text](https://example.invalid/retry)", cx)
        });
        let samples = Arc::default();
        let object_extensions = object_extensions(&samples);
        let calls = page.read(cx).calls.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_renderer = attempts.clone();
        let factory = Arc::new(move || {
            TextView::new(&child)
                .markdown_extensions(object_extensions.clone())
                .on_link_click(recorder(&calls))
                .into_any_element()
        });
        let extensions = MarkdownExtensions::default()
            .block_parser(|node, _| {
                matches!(node, markdown_ast::Node::Code(_))
                    .then(|| MarkdownNode::new("retry", ()).text("retry child"))
            })
            .block_renderer("retry", move |_, _, _| Retry {
                factory: factory.clone(),
                accepted_factory: None,
                attempts: attempts_renderer.clone(),
                nested: true,
                accepted_count: None,
            });
        page.update(cx, |page, cx| {
            page.extensions = extensions;
            cx.notify();
        });
        let tree = draw(window, cx);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "both discarded attempts really ran"
        );
        assert_eq!(links(&tree).len(), 3);
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|(_, node)| node.role() == Role::GenericContainer
                    && node.label() == Some("object"))
                .count(),
            1,
            "ordinary InlineObject from failed attempts must not remain"
        );
        let unique: std::collections::HashSet<_> = tree.nodes.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            unique.len(),
            tree.nodes.len(),
            "completed tree has unique IDs"
        );
        let before = page.read(cx).before.clone();
        before.focus(window, cx);
        for label in ["earlier", "retry object text", "afterward"] {
            let id = link(&tree, label);
            key(window, cx, "tab");
            let current = draw(window, cx);
            assert_eq!(
                current.focus, id,
                "successful earlier and later edges retain order"
            );
            request(window, cx, id, Action::Click);
        }
        assert_eq!(call_count(&page, cx), 3);
        // Debug duplicate-ID failure is observable as a panic. A silent tree
        // repair/log fallback needs the additional diagnostics seam in COVERAGE.
    });
}

const ROLLBACK_ORDINARY_COLOR: u32 = 0x8c5d31;

struct RollbackPanel {
    parent: Entity<TextViewState>,
    extensions: MarkdownExtensions,
    renders: Arc<AtomicUsize>,
    speculate: Arc<AtomicBool>,
    paints: Samples,
}

impl Render for RollbackPanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.fetch_add(1, Ordering::SeqCst);
        ObservePaint::new(
            TextView::new(&self.parent)
                .markdown_extensions(self.extensions.clone())
                .w_full(),
            &self.paints,
        )
    }
}

#[gpui::test]
fn discarded_links_do_not_poison_a_cached_owners_marker(cx: &mut TestAppContext) {
    let (page, cx) = setup(cx, SOURCE, true);
    cx.update(|window, cx| {
        let child = cx.new(|cx| TextViewState::markdown(SOURCE, cx));
        let factory = Arc::new(move || TextView::new(&child).into_any_element());
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_renderer = attempts.clone();
        let speculate = Arc::new(AtomicBool::new(true));
        let speculate_renderer = speculate.clone();
        let ordinary: ElementFactory = Arc::new(|| {
            div()
                .bg(gpui::rgb(ROLLBACK_ORDINARY_COLOR))
                .child("ordinary accepted content")
                .into_any_element()
        });
        let extensions = MarkdownExtensions::default()
            .block_parser(|node, _| {
                matches!(node, markdown_ast::Node::Code(_))
                    .then(|| MarkdownNode::new("discard", ()).text("ordinary accepted content"))
            })
            .block_renderer("discard", move |_, _, _| {
                if speculate_renderer.load(Ordering::SeqCst) {
                    Retry {
                        factory: factory.clone(),
                        accepted_factory: Some(ordinary.clone()),
                        attempts: attempts_renderer.clone(),
                        nested: true,
                        accepted_count: None,
                    }
                    .into_any_element()
                } else {
                    ordinary()
                }
            });
        let renders = Arc::new(AtomicUsize::new(0));
        let sibling_renders = Arc::new(AtomicUsize::new(0));
        let paints: Samples = Arc::default();
        let panel = cx.new(|cx| RollbackPanel {
            parent: cx.new(|cx| TextViewState::markdown("```discard\nchild\n```", cx)),
            extensions,
            renders: renders.clone(),
            speculate,
            paints: paints.clone(),
        });
        let sibling = cx.new(|_| PlainPanel {
            renders: sibling_renders.clone(),
        });
        page.update(cx, |page, cx| {
            page.rollback_panel = Some((panel.clone(), sibling));
            cx.notify();
        });
        assert!(window.begin_a11y_link_observation_for_test());
        window.draw(cx).clear(cx);
        let first_observed = observation(window);
        assert_eq!(first_observed.interval.draws_accepted, 1);
        assert_eq!(first_observed.interval.registrations, 2);
        // The enclosing TextView scope is still open when both attempts roll
        // back, so their owned finalizers are dropped before any clipper call.
        assert_eq!(first_observed.interval.clipper_calls, 0);
        assert!(first_observed.interval.transaction_visits > 0);
        assert!(first_observed.registrations.is_empty());
        let first = window
            .accessibility_tree_for_test()
            .expect("active tree")
            .clone();
        assert!(
            links(&first).is_empty(),
            "only discarded attempts registered links"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "nested and outer failed attempts ran"
        );
        let baseline = renders.load(Ordering::SeqCst);
        assert!(baseline > 0, "the cached owner actually rendered");
        assert_eq!(sibling_renders.load(Ordering::SeqCst), 1);
        assert_eq!(paints.lock().expect("ordinary paint").len(), 1);
        let first_quads = scene_quads(window, ROLLBACK_ORDINARY_COLOR);
        assert_eq!(
            first_quads.len(),
            1,
            "actual ordinary accepted scene positive"
        );
        assert!(!first_quads[0].0.intersect(&first_quads[0].1).is_empty());

        // The monotonic registration range includes the two discarded attempts.
        // An unchanged draw must rerender that owner without affecting its sibling.
        window.draw(cx).clear(cx);
        let attempted = observation(window);
        assert_eq!(attempted.interval.draws_accepted, 1);
        assert_eq!(attempted.interval.registrations, 2);
        assert_eq!(attempted.interval.clipper_calls, 0);
        assert!(attempted.interval.transaction_visits > 0);
        assert!(attempted.registrations.is_empty());
        assert!(links(window.accessibility_tree_for_test().expect("active tree")).is_empty());
        assert_eq!(renders.load(Ordering::SeqCst), baseline + 1);
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
        assert_eq!(sibling_renders.load(Ordering::SeqCst), 1);
        assert_eq!(paints.lock().expect("ordinary paint").len(), 2);
        assert_eq!(scene_quads(window, ROLLBACK_ORDINARY_COLOR), first_quads);

        panel.update(cx, |panel, cx| {
            panel.speculate.store(false, Ordering::SeqCst);
            cx.notify();
        });
        window.draw(cx).clear(cx);
        let recovered = observation(window);
        assert_eq!(recovered.interval.draws_accepted, 1);
        assert_eq!(recovered.interval.registrations, 0);
        assert_eq!(recovered.interval.clipper_calls, 0);
        assert!(recovered.registrations.is_empty());
        assert!(links(window.accessibility_tree_for_test().expect("active tree")).is_empty());
        assert_eq!(renders.load(Ordering::SeqCst), baseline + 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
        assert_eq!(paints.lock().expect("ordinary paint").len(), 3);
        assert_eq!(scene_quads(window, ROLLBACK_ORDINARY_COLOR), first_quads);
        let stable = (
            renders.load(Ordering::SeqCst),
            sibling_renders.load(Ordering::SeqCst),
            attempts.load(Ordering::SeqCst),
            paints.lock().expect("ordinary paint").len(),
        );
        for _ in 0..3 {
            window.draw(cx).clear(cx);
            let next = observation(window);
            assert_eq!(next.interval.draws_accepted, 1);
            assert_eq!(next.interval.registrations, 0);
            assert_eq!(next.interval.clipper_calls, 0);
            assert!(next.registrations.is_empty());
            assert!(links(window.accessibility_tree_for_test().expect("active tree")).is_empty());
            assert_eq!(
                renders.load(Ordering::SeqCst),
                stable.0,
                "ordinary-only recovery restores stable owner reuse"
            );
            assert_eq!(
                attempts.load(Ordering::SeqCst),
                stable.2,
                "ordinary-only reuse must not execute speculative children again"
            );
            assert_eq!(sibling_renders.load(Ordering::SeqCst), stable.1);
            assert_eq!(paints.lock().expect("ordinary paint").len(), stable.3);
            assert_eq!(scene_quads(window, ROLLBACK_ORDINARY_COLOR), first_quads);
        }
    });
}

struct LinkPanel {
    body: Entity<TextViewState>,
    calls: Calls,
    renders: Arc<AtomicUsize>,
    deferred_depth: usize,
    show: bool,
    max_lines: Option<usize>,
}

impl Render for LinkPanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.fetch_add(1, Ordering::SeqCst);
        let mut child = div()
            .when(self.show, |this| {
                this.child(
                    TextView::new(&self.body)
                        .on_link_click(recorder(&self.calls))
                        .w_full()
                        .when_some(self.max_lines, |this, cap| this.max_lines(cap)),
                )
            })
            .into_any_element();
        for _ in 0..self.deferred_depth {
            child = deferred(child).into_any_element();
        }
        child
    }
}

struct PlainPanel {
    renders: Arc<AtomicUsize>,
}
impl Render for PlainPanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.fetch_add(1, Ordering::SeqCst);
        div().child("ordinary cached sibling")
    }
}

struct CacheGroup {
    linked: Entity<LinkPanel>,
    ordinary: Entity<PlainPanel>,
    renders: Arc<AtomicUsize>,
}
impl Render for CacheGroup {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.fetch_add(1, Ordering::SeqCst);
        div()
            .flex()
            .flex_col()
            .child(
                self.linked
                    .clone()
                    .cached(StyleRefinement::default().w_full().h(px(150.))),
            )
            .child(
                self.ordinary
                    .clone()
                    .cached(StyleRefinement::default().w_full().h(px(50.))),
            )
    }
}

fn cache_case(cx: &mut TestAppContext, depth: usize) {
    let (page, cx) = setup(cx, SOURCE, false);
    cx.update(|window, cx| {
        let group_renders = Arc::new(AtomicUsize::new(0));
        let linked_renders = Arc::new(AtomicUsize::new(0));
        let plain_renders = Arc::new(AtomicUsize::new(0));
        let body = page.read(cx).body.clone();
        let calls = page.read(cx).calls.clone();
        let linked = cx.new(|_| LinkPanel {
            body, calls, renders: linked_renders.clone(), deferred_depth: depth, show: true,
            max_lines: None,
        });
        let ordinary = cx.new(|_| PlainPanel { renders: plain_renders.clone() });
        let group = cx.new(|_| CacheGroup {
            linked: linked.clone(), ordinary, renders: group_renders.clone(),
        });
        page.update(cx, |page, cx| { page.cache_group = Some(group); cx.notify(); });
        window.draw(cx).clear(cx);
        let inactive_counts = (
            group_renders.load(Ordering::SeqCst), linked_renders.load(Ordering::SeqCst),
            plain_renders.load(Ordering::SeqCst),
        );
        window.draw(cx).clear(cx);
        assert_eq!(group_renders.load(Ordering::SeqCst), inactive_counts.0 + 1);
        assert_eq!(linked_renders.load(Ordering::SeqCst), inactive_counts.1 + 1);
        assert_eq!(plain_renders.load(Ordering::SeqCst), inactive_counts.2,
            "selective cache exclusion also works before a11y activation");
        window.activate_accessibility_for_test();
        let initial = draw(window, cx);
        assert_eq!(links(&initial).len(), 1);
        let baseline = (
            group_renders.load(Ordering::SeqCst), linked_renders.load(Ordering::SeqCst),
            plain_renders.load(Ordering::SeqCst),
        );
        assert!(baseline.0 > 0 && baseline.1 > 0 && baseline.2 > 0,
            "positive controls: every counted view rendered");
        for round in 1..=3 {
            let current = draw(window, cx);
            assert_eq!(links(&current).len(), 1, "fresh registered host remains published");
            assert_eq!(group_renders.load(Ordering::SeqCst), baseline.0 + round);
            assert_eq!(linked_renders.load(Ordering::SeqCst), baseline.1 + round);
            assert_eq!(plain_renders.load(Ordering::SeqCst), baseline.2,
                "link-only miss cannot force ordinary cached siblings to render");
        }
        let old = links(&initial)[0];
        let body = page.read(cx).body.clone();
        body.update(cx, |state, cx| state.set_text(
            "[fresh many words wrap over several lines with narrower width](https://example.invalid/fresh)", cx));
        let count = call_count(&page, cx);
        request(window, cx, old, Action::Click);
        assert_eq!(call_count(&page, cx), count, "source replacement also revokes a cached host before redraw");
        let replacement = draw(window, cx);
        assert_eq!(links(&replacement).len(), 1);
        assert_eq!(node(&replacement, links(&replacement)[0]).url(), Some("https://example.invalid/fresh"));
        page.update(cx, |page, cx| { page.width = px(95.); cx.notify(); });
        assert_eq!(links(&draw(window, cx)).len(), 1, "actual cache bounds change still rebuilds links");
        linked.update(cx, |panel, cx| { panel.max_lines = Some(1); cx.notify(); });
        let clipped = draw(window, cx);
        assert!(page.read(cx).body.read(cx).is_clamped(), "cached source truly clips");
        assert!(links(&clipped).len() <= 1);
        linked.update(cx, |panel, cx| { panel.show = false; cx.notify(); });
        assert!(links(&draw(window, cx)).is_empty());
        draw(window, cx); // Allow the ancestor's previously true marker to clear.
        let after_removal = group_renders.load(Ordering::SeqCst);
        draw(window, cx);
        assert_eq!(group_renders.load(Ordering::SeqCst), after_removal,
            "ordinary caching resumes after link removal");
    });
}

#[gpui::test]
fn link_only_cache_misses_preserve_ordinary_sibling_render_counts(cx: &mut TestAppContext) {
    cache_case(cx, 0);
}
#[gpui::test]
fn deferred_link_discovery_marks_cached_owners_after_their_prepaint_returns(
    cx: &mut TestAppContext,
) {
    cache_case(cx, 1);
}
#[gpui::test]
fn nested_deferred_link_discovery_preserves_ordinary_sibling_reuse(cx: &mut TestAppContext) {
    cache_case(cx, 2);
}

#[gpui::test]
fn prose_copy_and_fragment_geometry_survive_scale_and_reflow(cx: &mut TestAppContext) {
    let source = "plain before [many words wrap through several lines](https://example.invalid/a) plain after";
    let (page, cx) = setup(cx, source, true);
    cx.update(|window, cx| {
        let first_scale = window.scale_factor();
        assert_eq!(first_scale, 1.25);
        let before = draw(window, cx);
        let id = links(&before)[0];
        assert_eq!(links(&before), vec![id], "the actual source has one Link");
        let first = fragments(&before, id);
        assert!(first.len() > 1, "actual text wraps into multiple fragments");
        let first_logical = current_logical_fragments(&before, id, first_scale, window, cx);
        assert_eq!(first.len(), first_logical.len());
        let paragraphs: Vec<_> = before
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == Role::Paragraph)
            .map(|(_, node)| node.label().map(str::to_owned))
            .collect();
        assert!(
            !paragraphs.is_empty(),
            "existing paragraph semantics remain"
        );
        let body = page.read(cx).body.clone();
        let copy = body.read(cx).rendered_text().as_str().to_owned();
        body.update(cx, |state, cx| state.select_all(cx));
        let selection = body.read(cx).selected_text();
        assert!(!selection.is_empty());
        window.set_scale_factor(2.);
        let next_scale = window.scale_factor();
        assert_eq!(next_scale, 2.);
        let scaled = draw(window, cx);
        assert_eq!(
            links(&scaled),
            vec![id],
            "the current parsed root is retained"
        );
        let physical = fragments(&scaled, id);
        assert_eq!(first.len(), physical.len());
        let scaled_logical = current_logical_fragments(&scaled, id, next_scale, window, cx);
        assert_eq!(physical.len(), scaled_logical.len());
        assert_eq!(
            first_logical, scaled_logical,
            "all actual surviving keys and logical Bounds remain exactly equal across scales"
        );
        assert_eq!(
            first.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            physical.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "all actual emitted leaf IDs retain their order across scales"
        );
        page.update(cx, |page, cx| {
            page.width = px(95.);
            cx.notify();
        });
        let narrow = draw(window, cx);
        assert!(
            fragments(&narrow, id).len() > physical.len(),
            "actual reshaping wraps more lines"
        );
        assert_eq!(body.read(cx).rendered_text().as_str(), copy);
        assert_eq!(body.read(cx).selected_text(), selection);
        assert_eq!(
            narrow
                .nodes
                .iter()
                .filter(|(_, node)| node.role() == Role::Paragraph)
                .map(|(_, node)| node.label().map(str::to_owned))
                .collect::<Vec<_>>(),
            paragraphs
        );
    });
}

#[gpui::test]
fn plain_prose_publishes_no_links_or_extra_tab_stops(cx: &mut TestAppContext) {
    let (page, cx) = setup(cx, "ordinary prose without links", true);
    cx.update(|window, cx| {
        assert!(links(&draw(window, cx)).is_empty());
        page.read(cx).before.clone().focus(window, cx);
        key(window, cx, "tab");
        assert!(page.read(cx).after.is_focused(window));
        assert_eq!(call_count(&page, cx), 0);
    });
}

#[gpui::test]
fn accepted_base_permits_are_weak_frame_scoped_and_reject_a_foreign_window(
    cx: &mut TestAppContext,
) {
    let (page, cx) = setup(cx, SOURCE, true);
    let foreign = cx.update(|window, cx| {
        draw(window, cx);
        let old = observation(window).registrations.remove(0);
        assert!(old.frame.activation_bounds(window, cx).is_some());
        page.read(cx)
            .body
            .clone()
            .update(cx, |state, cx| state.push_str("\n\nappend tail", cx));
        assert!(
            old.frame.activation_bounds(window, cx).is_some(),
            "accepted prefix remains authoritative"
        );
        draw(window, cx);
        let new = observation(window).registrations.remove(0);
        assert_eq!(new.root, old.root, "stable NodeId is not frame authority");
        assert!(old.frame.activation_bounds(window, cx).is_none());
        assert!(new.frame.activation_bounds(window, cx).is_some());
        page.read(cx)
            .body
            .clone()
            .update(cx, |state, cx| state.set_text("replacement", cx));
        assert!(
            new.frame.activation_bounds(window, cx).is_none(),
            "replacement revokes before redraw"
        );
        page.read(cx)
            .body
            .clone()
            .update(cx, |state, cx| state.set_text(SOURCE, cx));
        draw(window, cx);
        let mounted = observation(window).registrations.remove(0).frame;
        page.update(cx, |page, cx| {
            page.visible = false;
            cx.notify();
        });
        draw(window, cx);
        assert!(mounted.activation_bounds(window, cx).is_none());
        assert!(observation(window).registrations.is_empty());
        assert!(
            page.read(cx)
                .body
                .read(cx)
                .rendered_text()
                .as_str()
                .contains("alpha")
        );
        page.update(cx, |page, cx| {
            page.visible = true;
            cx.notify();
        });
        draw(window, cx);
        let foreign = observation(window).registrations.remove(0).frame;
        assert!(foreign.activation_bounds(window, cx).is_some());
        foreign
    });
    let (_, other) = setup(cx, SOURCE, true);
    other.update(|window, cx| {
        draw(window, cx);
        assert!(
            foreign.activation_bounds(window, cx).is_none(),
            "window token must match"
        );
    });
}

#[gpui::test]
fn observer_drain_and_epoch_reset_do_not_mutate_the_actual_accepted_permit(
    cx: &mut TestAppContext,
) {
    let (_, cx) = setup(cx, SOURCE, true);
    cx.update(|window, cx| {
        draw(window, cx);
        let permit = observation(window).registrations.remove(0).frame;
        assert!(permit.activation_bounds(window, cx).is_some());
        assert!(window.begin_a11y_link_observation_for_test());
        let before = observation(window);
        assert_eq!(before.interval, Default::default());
        assert_eq!(before.accepted_draw, None);
        assert!(before.registrations.is_empty());
        assert!(
            permit.activation_bounds(window, cx).is_some(),
            "begin resets diagnostics only"
        );
        drop(observation(window)); // Isolate subsequent draw from the explicit guard check.
        window.draw(cx).clear(cx);
        let first = observation(window);
        assert_eq!(first.interval.draws_started, 1);
        assert_eq!(first.interval.draws_accepted, 1);
        assert_eq!(first.interval.registrations, 1);
        assert_eq!(first.interval.clipper_calls, 1);
        assert!(first.interval.fragment_visits > 0 && first.interval.scope_visits > 0);
        assert_eq!(first.accepted_draw, Some(1));
        let second = observation(window);
        assert_eq!(
            second.interval,
            Default::default(),
            "take drains intervals only"
        );
        assert_eq!(second.accepted_draw, first.accepted_draw);
        assert_eq!(second.registrations[0].root, first.registrations[0].root);
        assert_eq!(
            second.registrations[0].candidates,
            first.registrations[0].candidates
        );
        assert!(
            permit.activation_bounds(window, cx).is_none(),
            "real accepted draw still retires old arena"
        );
    });
}

#[gpui::test]
fn actual_finalizer_work_does_not_multiply_with_unrelated_ordinary_nodes(cx: &mut TestAppContext) {
    let (page, cx) = setup(cx, SOURCE, true);
    cx.update(|window, cx| {
        let mut baseline = None;
        for count in [0, 200, 800] {
            page.update(cx, |page, cx| {
                page.unrelated_nodes = count;
                cx.notify();
            });
            assert!(window.begin_a11y_link_observation_for_test());
            window.draw(cx).clear(cx);
            let current = observation(window);
            let work = current.interval;
            assert_eq!(work.draws_accepted, 1);
            assert_eq!(work.registrations, 1);
            assert_eq!(work.clipper_calls, work.registrations);
            assert!(work.promoted_scopes > 0 && work.fragment_visits > 0 && work.scope_visits > 0);
            assert!(
                !current.registrations[0].candidates.is_empty(),
                "real shaped positive control"
            );
            let actual = (
                work.registrations,
                work.clipper_calls,
                work.promoted_scopes,
                work.fragment_visits,
                work.scope_visits,
                work.source_checks,
                work.transaction_visits,
            );
            if let Some(baseline) = baseline {
                assert_eq!(actual, baseline);
            } else {
                baseline = Some(actual);
            }
        }
    });
}

#[gpui::test]
fn no_link_draws_create_no_link_arena_or_promoted_metadata(cx: &mut TestAppContext) {
    let (_, cx) = setup(cx, "plain prose", true);
    cx.update(|window, cx| {
        for _ in 0..3 {
            assert!(window.begin_a11y_link_observation_for_test());
            window.draw(cx).clear(cx);
            let current = observation(window).interval;
            assert_eq!(current.draws_accepted, 1);
            assert_eq!(current.registrations, 0);
            assert_eq!(current.promoted_scopes, 0);
            assert_eq!(current.clipper_calls, 0);
            assert_eq!(current.link_frame_arenas_created, 0);
            assert!(
                current.scope_visits > 0,
                "ordinary scope setup is counted separately"
            );
        }
    });
}

#[gpui::test]
fn missing_child_repair_has_an_independent_observer_positive_control(cx: &mut TestAppContext) {
    let (page, cx) = setup(cx, SOURCE, true);
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.defect_control = true;
            cx.notify();
        });
        assert!(window.begin_a11y_link_observation_for_test());
        window.draw(cx).clear(cx);
        let defective = window
            .take_a11y_link_observation_for_test()
            .expect("observed repair");
        assert_eq!(
            defective.interval.tree_defect_events, 1,
            "exactly one deliberate missing-child edge"
        );
        assert_eq!(defective.interval.raw_link_replays, 0);
        let tree = window.accessibility_tree_for_test().expect("repaired tree");
        let repaired = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("repair positive control"))
            .expect("real parent of deliberate defect");
        assert!(repaired.1.children().is_empty());
        page.update(cx, |page, cx| {
            page.defect_control = false;
            cx.notify();
        });
        window.draw(cx).clear(cx);
        assert_eq!(
            observation(window).interval.tree_defect_events,
            0,
            "valid interval is quiet after drain"
        );
    });
}

fn transition_cycle(cx: &mut TestAppContext, policy: bool) {
    let (page, cx) = setup(cx, SOURCE, false);
    cx.update(|window, cx| {
        assert!(window.simulate_a11y_transition_for_test(A11yTestTransition::Activate));
        draw(window, cx);
        let active = observation(window).registrations.remove(0);
        let disable = if policy {
            A11yTestTransition::ForceDisable
        } else {
            A11yTestTransition::Deactivate
        };
        assert!(window.simulate_a11y_transition_for_test(disable));
        assert_eq!(window.is_a11y_enabled(), !policy);
        drop(observation(window));
        window.draw(cx).clear(cx);
        let inactive = observation(window);
        assert_eq!(inactive.interval.draws_accepted, 1);
        assert_eq!(inactive.interval.tree_updates_sent, 0);
        assert_eq!(
            inactive.requested_active, policy,
            "policy injection preserves adapter demand"
        );
        assert!(!window.is_a11y_active());
        assert!(active.frame.activation_bounds(window, cx).is_none());
        assert!(
            inactive.registrations[0]
                .frame
                .activation_bounds(window, cx)
                .is_some()
        );
        let count = call_count(&page, cx);
        page.read(cx).before.clone().focus(window, cx);
        request(window, cx, active.root, Action::Focus);
        request(window, cx, active.root, Action::Click);
        assert!(page.read(cx).before.is_focused(window));
        assert_eq!(
            call_count(&page, cx),
            count,
            "inactive old accessible fallbacks are gone"
        );
        key(window, cx, "tab");
        window.draw(cx).clear(cx);
        key(window, cx, "enter");
        assert_eq!(
            call_count(&page, cx),
            count + 1,
            "current visible keyboard link remains valid"
        );
        let enable = if policy {
            A11yTestTransition::ClearForceDisable
        } else {
            A11yTestTransition::Activate
        };
        assert!(window.simulate_a11y_transition_for_test(enable));
        drop(observation(window));
        window.draw(cx).clear(cx);
        let restored = observation(window);
        assert!(window.is_a11y_enabled() && window.is_a11y_active());
        assert!(restored.requested_active);
        assert_eq!(restored.interval.tree_updates_sent, 1);
        assert_eq!(restored.interval.draws_accepted, 1);
        request(window, cx, restored.registrations[0].root, Action::Click);
        assert_eq!(call_count(&page, cx), count + 2);
    });
}

#[gpui::test]
fn real_adapter_deactivation_and_reactivation_clean_the_same_window(cx: &mut TestAppContext) {
    transition_cycle(cx, false);
}
#[gpui::test]
fn force_policy_injection_and_clear_preserve_adapter_demand_in_the_same_window(
    cx: &mut TestAppContext,
) {
    transition_cycle(cx, true);
}

fn midframe_case(cx: &mut TestAppContext, phase: TestPhase) {
    let (page, cx) = setup(cx, SOURCE, false);
    cx.update(|window, cx| {
        for (start, event, end) in [
            (false, A11yTestTransition::Activate, true),
            (true, A11yTestTransition::Deactivate, false),
        ] {
            assert_eq!(window.is_a11y_active(), start);
            page.read(cx)
                .transition_records
                .lock()
                .expect("records")
                .clear();
            *page.read(cx).transition.lock().expect("one shot") = Some((phase, event));
            assert!(window.begin_a11y_link_observation_for_test());
            window.draw(cx).clear(cx);
            let crossed = observation(window);
            assert_eq!(
                *page.read(cx).transition_records.lock().expect("records"),
                vec![(start, start, true)],
                "real callback cannot mutate the frame's start snapshot"
            );
            assert_eq!(crossed.interval.draws_accepted, 1);
            assert_eq!(
                crossed.interval.tree_updates_sent, 0,
                "no partial tree at either crossing"
            );
            assert_eq!(crossed.requested_active, end);
            window.draw(cx).clear(cx);
            let fresh = observation(window);
            assert_eq!(fresh.interval.draws_accepted, 1);
            assert_eq!(fresh.interval.tree_updates_sent, u64::from(end));
            assert_eq!(window.is_a11y_active(), end);
        }
    });
}

#[gpui::test]
fn real_callback_crossings_during_prepaint_send_only_the_next_complete_tree(
    cx: &mut TestAppContext,
) {
    midframe_case(cx, TestPhase::Prepaint);
}
#[gpui::test]
fn real_callback_crossings_during_paint_send_only_the_next_complete_tree(cx: &mut TestAppContext) {
    midframe_case(cx, TestPhase::Paint);
}

#[gpui::test]
fn force_policy_injection_is_rejected_while_drawing(cx: &mut TestAppContext) {
    let (page, cx) = setup(cx, SOURCE, true);
    cx.update(|window, cx| {
        for phase in [TestPhase::Prepaint, TestPhase::Paint] {
            *page.read(cx).transition.lock().expect("one shot") =
                Some((phase, A11yTestTransition::ForceDisable));
            page.read(cx)
                .transition_records
                .lock()
                .expect("records")
                .clear();
            draw(window, cx);
            assert!(window.is_a11y_enabled());
            assert_eq!(
                *page.read(cx).transition_records.lock().expect("records"),
                vec![(true, true, false)]
            );
        }
    });
}

// This separate Core protocol fixture uses a real whole shaped run. It does
// not implement Base's glyph extraction, parsed source guard or line-safe clip.
struct ListFixture {
    state: gpui::ListState,
    body: Entity<TextViewState>,
    extensions: MarkdownExtensions,
    attempts: Arc<AtomicUsize>,
    samples: Samples,
}

struct AutoscrollChild {
    child: Option<AnyElement>,
    attempts: Arc<AtomicUsize>,
    samples: Samples,
}
impl IntoElement for AutoscrollChild {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for AutoscrollChild {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        Some("actual-autoscroll-child".into())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let mut child = self.child.take().expect("actual list child");
        (child.request_layout(window, cx), child)
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        child.prepaint(window, cx);
        self.samples
            .lock()
            .expect("actual autoscroll request")
            .push(PaintSample {
                bounds,
                mask: window.content_mask().bounds,
            });
        window.request_autoscroll(bounds);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        child: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        child.paint(window, cx);
    }
}

fn actual_list_autoscroll_case(cx: &mut TestAppContext, active: bool) {
    let (page, cx) = setup(cx, SOURCE, active);
    cx.update(|window, cx| {
        let source = format!(
            "[prefix $object$ {}](https://example.invalid/a)",
            "actual wrapped words ".repeat(35)
        );
        let fixture = ListFixture {
            state: gpui::ListState::new(1, gpui::ListAlignment::Top, px(0.)).measure_all(),
            body: cx.new(|cx| TextViewState::markdown(&source, cx)),
            extensions: object_extensions(&Arc::default()),
            attempts: Arc::default(),
            samples: Arc::default(),
        };
        let attempts = fixture.attempts.clone();
        let samples = fixture.samples.clone();
        let state = fixture.state.clone();
        page.update(cx, |page, cx| {
            page.list_fixture = Some(fixture);
            cx.notify();
        });
        assert!(window.begin_a11y_link_observation_for_test());
        window.draw(cx).clear(cx);
        let observed = observation(window);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "real List::prepaint_items failed then retried"
        );
        let samples = samples.lock().expect("actual requests");
        assert_eq!(samples.len(), 2);
        assert!(
            samples[0].bounds.bottom() > state.viewport_bounds().bottom(),
            "the genuine first child bounds caused the List autoscroll Err"
        );
        assert!(
            samples[0].bounds.size.height > state.viewport_bounds().size.height,
            "real shaping exceeds the actual list viewport"
        );
        assert!(observed.interval.transaction_visits > 0);
        assert_eq!(observed.interval.registrations, 2);
        assert_eq!(
            observed.registrations.len(),
            1,
            "only the accepted actual Base registration survives"
        );
        assert!(
            observed.registrations[0]
                .frame
                .activation_bounds(window, cx)
                .is_some()
        );
        if active {
            let tree = window
                .accessibility_tree_for_test()
                .expect("actual list tree");
            assert_eq!(links(tree).len(), 1);
            assert_eq!(
                tree.nodes
                    .iter()
                    .filter(|(_, node)| node.role() == Role::GenericContainer
                        && node.label() == Some("object"))
                    .count(),
                1
            );
        }
        drop(samples);
        page.read(cx).before.clone().focus(window, cx);
        key(window, cx, "tab");
        let focus = window.focused(cx).expect("current list link");
        window.draw(cx).clear(cx);
        drop(observation(window));
        assert!(focus.is_focused(window));
        key(window, cx, "enter");
        assert_eq!(
            call_count(&page, cx),
            1,
            "actual List retry preserves visible keyboard activation"
        );
    });
}

#[gpui::test]
fn actual_list_autoscroll_retries_base_object_and_link_with_active_accessibility(
    cx: &mut TestAppContext,
) {
    actual_list_autoscroll_case(cx, true);
}
#[gpui::test]
fn actual_list_autoscroll_retries_base_object_and_link_with_inactive_accessibility(
    cx: &mut TestAppContext,
) {
    actual_list_autoscroll_case(cx, false);
}

struct ProtocolOwner;
struct ProtocolLog {
    frames: Vec<A11yLinkFrame>,
}

fn protocol_source(owner: AnyEntity, incarnation: usize, _: &App) -> bool {
    incarnation == 0 && owner.downcast::<ProtocolOwner>().is_ok()
}

struct ProtocolElement {
    text: StyledText,
    source: A11yLinkSource,
    log: Entity<ProtocolLog>,
}

struct ProtocolHost {
    element: AnyElement,
    frame: A11yLinkFrame,
}

impl IntoElement for ProtocolElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for ProtocolElement {
    type RequestLayoutState = ();
    type PrepaintState = ProtocolHost;
    fn id(&self) -> Option<ElementId> {
        Some("protocol-shaped-run".into())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.text.request_layout(id, inspector, window, cx)
    }
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> ProtocolHost {
        self.text
            .prepaint(id, inspector, bounds, &mut (), window, cx);
        let layout = self.text.layout().clone();
        let actual = layout.bounds().intersect(&window.content_mask().bounds);
        assert!(
            actual.size.width > px(0.) && actual.size.height > px(0.),
            "real shaped protocol candidate"
        );
        let center_hit = layout.index_for_position(actual.center());
        assert!(
            center_hit.is_ok_and(|index| (0.."protocol guard".len()).contains(&index)),
            "actual intrinsic protocol candidate center must hit its text: bounds={actual:?}, hit={center_hit:?}"
        );
        let scale = window.scale_factor();
        window.with_a11y_link_clip_scope(self.source.clone(), cx, |window, cx| {
            let host = window.with_a11y_link_subtree(
                vec![A11yLinkFragment::new(0, actual)],
                move |mask| {
                    let visible = actual.intersect(&mask.bounds);
                    if visible.size.width > px(0.)
                        && visible.size.height > px(0.)
                        && layout
                            .index_for_position(visible.center())
                            .ok()
                            .is_some_and(|index| (0.."protocol guard".len()).contains(&index))
                    {
                        vec![A11yLinkFragment::new(0, visible)]
                    } else {
                        Vec::new()
                    }
                },
                cx,
                |frame, window, cx| {
                    self.log.update(cx, |log, _| log.frames.push(frame.clone()));
                    let callback = frame.clone();
                    let mut element = div()
                        .id("protocol-link")
                        .role(Role::Link)
                        .aria_label("protocol guard")
                        .tab_index(0)
                        .w(px(0.))
                        .h(px(0.))
                        .on_click(move |event, window, cx| {
                            if matches!(event, ClickEvent::Keyboard(_)) {
                                cx.stop_propagation();
                                assert!(callback.activation_bounds(window, cx).is_some());
                            }
                        })
                        .a11y_synthetic_children(move |builder| {
                            let mut leaf = Node::new(Role::Label);
                            leaf.set_read_only();
                            leaf.set_bounds(Rect::new(
                                f64::from(f32::from(actual.left()) * scale),
                                f64::from(f32::from(actual.top()) * scale),
                                f64::from(f32::from(actual.right()) * scale),
                                f64::from(f32::from(actual.bottom()) * scale),
                            ));
                            let id = builder.synthetic_node_id(0usize);
                            builder.push_child(id, leaf);
                        })
                        .into_any_element();
                    element.prepaint_as_root(
                        actual.origin,
                        size(
                            AvailableSpace::Definite(px(0.)),
                            AvailableSpace::Definite(px(0.)),
                        ),
                        window,
                        cx,
                    );
                    ProtocolHost { element, frame }
                },
            );
            (host, None)
        })
    }
    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        host: &mut ProtocolHost,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .paint(id, inspector, bounds, &mut (), &mut (), window, cx);
        _ = window
            .paint_a11y_link_subtree(&host.frame, cx, |window, cx| host.element.paint(window, cx));
    }
}

fn outer_unscoped_transaction_case(cx: &mut TestAppContext, active: bool) {
    let (page, cx) = setup(cx, SOURCE, active);
    cx.update(|window, cx| {
        let body = cx.new(|cx| {
            TextViewState::markdown("[retry $object$ text](https://example.invalid/a)", cx)
        });
        let extensions = object_extensions(&Arc::default());
        let owner = cx.new(|_| ProtocolOwner);
        let log = cx.new(|_| ProtocolLog { frames: Vec::new() });
        let protocol_log = log.clone();
        let calls = page.read(cx).calls.clone();
        let factory: ElementFactory = Arc::new(move || {
            div()
                .flex()
                .flex_col()
                .child(
                    TextView::new(&body)
                        .markdown_extensions(extensions.clone())
                        .on_link_click(recorder(&calls)),
                )
                .child(div().flex().child(ProtocolElement {
                    text: StyledText::new("protocol guard"),
                    source: A11yLinkSource::new(
                        AnyWeakEntity::from(owner.downgrade()),
                        0,
                        protocol_source,
                    ),
                    log: protocol_log.clone(),
                }))
                .into_any_element()
        });
        let failed = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::new(AtomicUsize::new(0));
        page.update(cx, |page, cx| {
            // This wrapper is a child of a plain Paragraph, with NO enclosing
            // TextView or Core source scope at Window::transact entry.
            page.outer_retry = Some((factory, failed.clone(), accepted.clone()));
            cx.notify();
        });
        assert!(window.begin_a11y_link_observation_for_test());
        window.draw(cx).clear(cx);
        let initial = observation(window);
        assert_eq!(failed.load(Ordering::SeqCst), 1);
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "positive control: two actual prepaint attempts"
        );
        assert_eq!(
            initial.interval.registrations, 4,
            "Base and Core hosts in each actual attempt"
        );
        assert!(initial.interval.transaction_visits > 0);
        assert_eq!(
            initial.registrations.len(),
            2,
            "discarded attempts are absent from accepted metadata"
        );
        let permits = log.read(cx).frames.clone();
        assert_eq!(permits.len(), 2);
        assert!(
            permits[0].activation_bounds(window, cx).is_none(),
            "discarded same-ID permit rejects"
        );
        assert!(
            permits[1].activation_bounds(window, cx).is_some(),
            "accepted same-ID protocol permit succeeds"
        );
        let base = initial.registrations[0].root;
        assert!(
            initial.registrations[0]
                .frame
                .activation_bounds(window, cx)
                .is_some()
        );
        if active {
            let tree = window
                .accessibility_tree_for_test()
                .expect("active accepted tree");
            assert_eq!(node(tree, base).url(), Some(URL));
            assert_eq!(
                tree.nodes
                    .iter()
                    .filter(|(_, node)| node.role() == Role::GenericContainer
                        && node.label() == Some("object"))
                    .count(),
                1
            );
        }
        page.read(cx).before.clone().focus(window, cx);
        key(window, cx, "tab");
        let visible_focus = window.focused(cx).expect("accepted real Base focus");
        assert!(!page.read(cx).after.is_focused(window));
        window.draw(cx).clear(cx);
        drop(observation(window));
        assert!(
            visible_focus.is_focused(window),
            "failed old registration cannot strip the visible retry's FocusId"
        );
        key(window, cx, "enter");
        assert_eq!(
            call_count(&page, cx),
            1,
            "real Base Enter works after unscoped retry"
        );
        if active {
            page.read(cx).before.clone().focus(window, cx);
            request(window, cx, base, Action::Focus);
            assert!(visible_focus.is_focused(window));
            request(window, cx, base, Action::Click);
            assert_eq!(
                call_count(&page, cx),
                2,
                "accessible Click also routes to the accepted Base host"
            );
        } else {
            page.read(cx).before.clone().focus(window, cx);
            window.focus(&visible_focus, cx);
            assert!(
                visible_focus.is_focused(window),
                "inactive current handle accepts direct Focus"
            );
            let current = observation(window).registrations.remove(0).frame;
            let point = current
                .activation_bounds(window, cx)
                .expect("current real fragment")
                .center();
            window.dispatch_event(
                gpui::MouseDownEvent {
                    position: point,
                    button: gpui::MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                gpui::MouseUpEvent {
                    position: point,
                    button: gpui::MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
            assert_eq!(
                call_count(&page, cx),
                2,
                "inactive pointer Click keeps the actual text route"
            );
        }
        drop(observation(window));
        let mut rollback_work = None;
        for count in [0, 250, 900] {
            page.update(cx, |page, cx| {
                page.unrelated_nodes = count;
                cx.notify();
            });
            assert!(window.begin_a11y_link_observation_for_test());
            window.draw(cx).clear(cx);
            let observed = observation(window).interval;
            assert_eq!(observed.registrations, 4);
            assert!(observed.transaction_visits > 0);
            if let Some(work) = rollback_work {
                assert_eq!(
                    observed.transaction_visits, work,
                    "unrelated accepted prefix cannot multiply checkpoint/discarded-suffix visits"
                );
            } else {
                rollback_work = Some(observed.transaction_visits);
            }
        }
    });
}

#[gpui::test]
fn outer_transaction_started_without_source_rolls_back_active_base_object_and_link(
    cx: &mut TestAppContext,
) {
    outer_unscoped_transaction_case(cx, true);
}
#[gpui::test]
fn outer_transaction_started_without_source_preserves_inactive_retry_focus(
    cx: &mut TestAppContext,
) {
    outer_unscoped_transaction_case(cx, false);
}

/// Escape through the existing deferred queue with an explicit mask obtained
/// from this actual child's layout. No synthetic candidate geometry is supplied.
struct MaskedDeferred {
    child: Option<AnyElement>,
    masks: Samples,
}
impl IntoElement for MaskedDeferred {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for MaskedDeferred {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (
            self.child
                .as_mut()
                .expect("deferred child")
                .request_layout(window, cx),
            (),
        )
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) {
        self.masks.lock().expect("deferred mask").push(PaintSample {
            bounds,
            mask: bounds,
        });
        window.defer_draw(
            self.child.take().expect("deferred child"),
            window.element_offset(),
            1,
            Some(ContentMask { bounds }),
        );
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }
}

#[gpui::test]
fn escaped_deferred_textview_uses_actual_explicit_mask_without_parent_final_clip(
    cx: &mut TestAppContext,
) {
    let (page, cx) = setup(cx, "first $object$\n\n```escape\nchild\n```", true);
    cx.update(|window, cx| {
        let parent_samples = Arc::default();
        let explicit_masks: Samples = Arc::default();
        let masks = explicit_masks.clone();
        let child = cx.new(|cx| TextViewState::markdown(
            "[escaped actual text wraps over several measured lines](https://example.invalid/escaped)", cx));
        let calls = page.read(cx).calls.clone();
        let extensions = object_extensions(&parent_samples)
            .block_parser(|node, _| matches!(node, markdown_ast::Node::Code(_))
                .then(|| MarkdownNode::new("escape", ()).text("escaped child")))
            .block_renderer("escape", move |_, _, _| MaskedDeferred {
                child: Some(TextView::new(&child).on_link_click(recorder(&calls)).into_any_element()),
                masks: masks.clone(),
            });
        page.update(cx, |page, cx| { page.extensions = extensions; page.max_lines = Some(1); cx.notify(); });
        let tree = draw(window, cx);
        assert!(page.read(cx).body.read(cx).is_clamped());
        let parent_mask = parent_samples.lock().expect("parent paint").last().expect("actual inline paint").mask;
        let explicit = explicit_masks.lock().expect("explicit mask").last().expect("actual deferred prepaint").mask;
        let escaped = links(&tree);
        assert_eq!(escaped.len(), 1, "real deferred child escapes its ancestor's final paint mask");
        let fragments = fragments(&tree, escaped[0]);
        assert!(fragments.iter().any(|(_, rect)| logical(*rect, window).top() >= parent_mask.bottom()),
            "positive control: visible deferred content lies beyond the parent's real clip");
        for (_, rect) in fragments {
            let bounds = logical(rect, window);
            assert!(bounds.left() >= explicit.left() && bounds.right() <= explicit.right());
            assert!(bounds.top() >= explicit.top() && bounds.bottom() <= explicit.bottom());
        }
        request(window, cx, escaped[0], Action::Click);
        assert_eq!(call_count(&page, cx), 1);
    });
}

struct ReplaceAncestor {
    child: Option<AnyElement>,
    parent: Entity<TextViewState>,
    armed: Arc<AtomicUsize>,
}
impl IntoElement for ReplaceAncestor {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for ReplaceAncestor {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let mut child = self.child.take().expect("real nested child");
        (child.request_layout(window, cx), child)
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        child: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self
            .armed
            .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.parent.update(cx, |state, cx| {
                state.set_text("ancestor changed during deferred prepaint", cx)
            });
        }
        child.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        child: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        child.paint(window, cx);
    }
}

#[gpui::test]
fn ancestor_replacement_during_actual_deferred_prepaint_revokes_the_new_child_frame(
    cx: &mut TestAppContext,
) {
    let (page, cx) = setup(cx, "```deferred\nchild\n```", true);
    cx.update(|window, cx| {
        let parent = page.read(cx).body.clone();
        let child = cx.new(|cx| TextViewState::markdown(SOURCE, cx));
        let retained = child.clone();
        let armed = Arc::new(AtomicUsize::new(0));
        let arm_renderer = armed.clone();
        let calls = page.read(cx).calls.clone();
        let extensions = MarkdownExtensions::default()
            .block_parser(|node, _| {
                matches!(node, markdown_ast::Node::Code(_))
                    .then(|| MarkdownNode::new("deferred", ()).text("retained child"))
            })
            .block_renderer("deferred", move |_, _, _| {
                deferred(ReplaceAncestor {
                    child: Some(
                        TextView::new(&child)
                            .on_link_click(recorder(&calls))
                            .into_any_element(),
                    ),
                    parent: parent.clone(),
                    armed: arm_renderer.clone(),
                })
            });
        page.update(cx, |page, cx| {
            page.extensions = extensions;
            cx.notify();
        });
        let baseline = draw(window, cx);
        let old = link(&baseline, "alpha");
        armed.store(1, Ordering::SeqCst);
        assert!(window.begin_a11y_link_observation_for_test());
        window.draw(cx).clear(cx);
        let revoked = observation(window);
        assert_eq!(
            armed.load(Ordering::SeqCst),
            2,
            "the actual deferred prepaint mutation fired"
        );
        assert!(
            revoked.interval.registrations > 0,
            "actual retained child still prepainted"
        );
        assert!(
            revoked
                .registrations
                .iter()
                .all(|entry| entry.frame.activation_bounds(window, cx).is_none())
        );
        assert!(
            links(
                window
                    .accessibility_tree_for_test()
                    .expect("accepted revoked tree")
            )
            .is_empty()
        );
        assert!(retained.read(cx).rendered_text().as_str().contains("alpha"));
        request(window, cx, old, Action::Click);
        assert_eq!(call_count(&page, cx), 0);
    });
}
