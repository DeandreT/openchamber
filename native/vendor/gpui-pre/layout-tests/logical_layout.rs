//! Public-API regressions for the local logical-layout policy.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    AppContext as _, AvailableSpace, Bounds, Context, Element as _, Entity, IntoElement,
    LayoutMode, ListAlignment, ListOffset, ListState, ParentElement as _, Pixels, Render, Style,
    Styled as _, TestAppContext, VisualTestContext, Window, canvas, div, fill, list, point, px,
    size,
};

#[gpui::test]
fn intrinsic_text_width_obeys_the_layout_policy(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    for scale in [1.0, 1.25, 1.5] {
        for mode in [LayoutMode::DevicePixels, LayoutMode::Logical] {
            cx.update(|window, _| {
                window.set_layout_mode(mode);
                window.set_scale_factor(scale);
            });
            cx.draw(
                point(px(0.), px(0.)),
                size(px(100.), px(100.)),
                |window, cx| {
                    let mut text = div().text_size(px(10.1)).child("Local").into_any();
                    let measured = text.layout_as_root(
                        size(AvailableSpace::MinContent, AvailableSpace::MinContent),
                        window,
                        cx,
                    );
                    // NoopTextSystem advances five BMP characters by 5 * 10.1 * 0.6.
                    let expected = match mode {
                        LayoutMode::Logical => 30.3,
                        LayoutMode::DevicePixels => (31.0_f32 * scale).ceil() / scale,
                    };
                    assert!(
                        (f32::from(measured.width) - expected).abs() < 0.0001,
                        "intrinsic text width at scale {scale} in {mode:?}: {measured:?}"
                    );
                    div()
                },
            );
        }
    }
}

#[gpui::test]
fn independent_rows_preserve_500_logical_strides(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    cx.update(|window, _| assert_eq!(window.layout_mode(), LayoutMode::DevicePixels));

    for (scale, device_height) in [(1.0, 34.0), (1.25, 33.6), (1.5, 50.0 / 1.5)] {
        for mode in [LayoutMode::DevicePixels, LayoutMode::Logical] {
            cx.update(|window, _| {
                window.set_layout_mode(mode);
                window.set_scale_factor(scale);
            });
            cx.draw(
                point(px(0.), px(0.)),
                size(px(100.), px(100.)),
                |window, cx| {
                    let mut total = px(0.);
                    for _ in 0..500 {
                        let mut row = div()
                            .py(px(1.))
                            .child(div().w(px(100.)).h(px(32.)))
                            .into_any();
                        let measured = row.layout_as_root(
                            size(AvailableSpace::MinContent, AvailableSpace::MinContent),
                            window,
                            cx,
                        );
                        let expected = if mode == LayoutMode::Logical {
                            34.0
                        } else {
                            device_height
                        };
                        assert_eq!(measured.height, px(expected), "independent row height");
                        total += measured.height;
                    }
                    if mode == LayoutMode::Logical {
                        assert_eq!(
                            total,
                            px(17_000.),
                            "500 rows must retain their authored extent"
                        );
                    }
                    div()
                },
            );
        }
    }
}

#[gpui::test]
fn measured_layout_and_painted_fractional_translations(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    for (scale, device_size) in [
        (1.0, size(px(13.), px(17.))),
        (1.25, size(px(13.6), px(17.6))),
        (1.5, size(px(20.0 / 1.5), px(26.0 / 1.5))),
    ] {
        for mode in [LayoutMode::DevicePixels, LayoutMode::Logical] {
            cx.update(|window, _| {
                window.set_layout_mode(mode);
                window.set_scale_factor(scale);
            });
            for translation in [-0.25, 0.25, 0.75] {
                let origin = point(px(translation), px(translation));
                let painted = Rc::new(Cell::new(None));
                let recorded = painted.clone();
                cx.draw(origin, size(px(100.), px(100.)), |window, cx| {
                    let id = window.request_measured_layout(Style::default(), |_, _, _, _| {
                        size(px(13.5), px(17.25))
                    });
                    window.compute_layout(
                        id,
                        size(AvailableSpace::MinContent, AvailableSpace::MinContent),
                        cx,
                    );
                    let expected = match mode {
                        LayoutMode::Logical => size(px(13.5), px(17.25)),
                        LayoutMode::DevicePixels => size(
                            px((13.5 * scale).ceil() / scale),
                            px((17.25 * scale).ceil() / scale),
                        ),
                    };
                    assert_eq!(
                        window.layout_bounds(id).size,
                        expected,
                        "measured public bounds"
                    );
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            recorded.set(Some(bounds));
                            window.paint_quad(fill(bounds, gpui::black()));
                            if mode == LayoutMode::DevicePixels {
                                assert_eq!(bounds.origin, window.pixel_snap_point(origin));
                            }
                        },
                    )
                    .w(px(13.5))
                    .h(px(17.25))
                });
                let bounds = painted.get().expect("canvas must reach the paint phase");
                if mode == LayoutMode::Logical {
                    assert_eq!(bounds, Bounds::new(origin, size(px(13.5), px(17.25))));
                } else {
                    assert_eq!(
                        bounds.size, device_size,
                        "default authored-size snapping is preserved"
                    );
                }
            }
        }
    }
}

type PaintedRows = Rc<RefCell<Vec<(usize, Bounds<Pixels>)>>>;

struct RowsView {
    state: ListState,
    calls: Rc<Cell<usize>>,
    painted: PaintedRows,
}

impl Render for RowsView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let calls = self.calls.clone();
        let painted = self.painted.clone();
        list(self.state.clone(), move |index, _, _| {
            calls.set(calls.get() + 1);
            let painted = painted.clone();
            div()
                .py(px(1.))
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            painted.borrow_mut().push((index, bounds));
                            window.paint_quad(fill(bounds, gpui::black()));
                        },
                    )
                    .w_full()
                    .h(px(32.)),
                )
                .into_any()
        })
        .size_full()
    }
}

fn draw_rows(
    cx: &mut VisualTestContext,
    view: &Entity<RowsView>,
    calls: &Rc<Cell<usize>>,
) -> Vec<(usize, Bounds<Pixels>)> {
    let painted = cx.update(|_, cx| view.read(cx).painted.clone());
    painted.borrow_mut().clear();
    let before = calls.get();
    cx.draw(
        point(px(0.25), px(0.25)),
        size(px(100.), px(100.)),
        |_, _| view.clone().into_any_element(),
    );
    assert!(
        calls.get() - before < 20,
        "a 10,000-row list must render a bounded visible range"
    );
    let rows = painted.borrow().clone();
    assert!(
        rows.len() >= 2,
        "the viewport must actually paint consecutive rows"
    );
    rows
}

#[gpui::test]
fn virtual_list_keeps_exact_anchors_and_invalidates_mode_and_scale(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let state = ListState::new(10_000, ListAlignment::Top, px(0.));
    let calls = Rc::new(Cell::new(0));
    let view = cx.update(|_, cx| {
        cx.new(|_| RowsView {
            state: state.clone(),
            calls: calls.clone(),
            painted: Rc::new(RefCell::new(Vec::new())),
        })
    });
    cx.update(|window, _| window.set_scale_factor(1.25));

    for (mode, scale, expected_height) in [
        (LayoutMode::Logical, 1.25, 34.0),
        (LayoutMode::Logical, 1.5, 34.0),
        (LayoutMode::DevicePixels, 1.5, 50.0 / 1.5),
        (LayoutMode::DevicePixels, 1.25, 33.6),
    ] {
        // Warm an offscreen row, then return to the anchor before changing geometry.
        state.scroll_to(ListOffset {
            item_ix: 120,
            offset_in_item: px(0.25),
        });
        draw_rows(cx, &view, &calls);
        state.scroll_to(ListOffset {
            item_ix: 20,
            offset_in_item: px(0.25),
        });
        draw_rows(cx, &view, &calls);
        assert!(
            state.bounds_for_item(120).is_some(),
            "ordinary scrolling retains measured bounds"
        );
        cx.update(|window, _| {
            let current = window.layout_mode();
            window.set_layout_mode(current);
        });
        draw_rows(cx, &view, &calls);
        assert!(
            state.bounds_for_item(120).is_some(),
            "repeating the policy retains measurements"
        );

        cx.update(|window, _| {
            window.set_layout_mode(mode);
            window.set_scale_factor(scale);
        });
        let rows = draw_rows(cx, &view, &calls);
        let anchor = state.logical_scroll_top();
        assert_eq!(
            anchor.item_ix, 20,
            "geometry changes preserve the anchored row"
        );
        assert_eq!(
            anchor.offset_in_item,
            px(0.25),
            "geometry changes preserve the exact offset"
        );
        assert!(
            state.bounds_for_item(120).is_none(),
            "old offscreen measurements must be invalidated"
        );
        let slot = state
            .bounds_for_item(20)
            .expect("visible row has measured bounds");
        assert_eq!(
            slot.size.height,
            px(expected_height),
            "the virtualizer must retain correct heights"
        );
        assert_eq!(rows[0].0, 20);
        assert_eq!(rows[1].0, 21);
        if mode == LayoutMode::Logical {
            assert_eq!(
                rows[0].1.top(),
                px(1.),
                "fractional origin and anchor offset cancel exactly"
            );
            assert_eq!(
                rows[1].1.top(),
                px(35.),
                "painted rows retain the authored 34 px stride"
            );
            assert_eq!(rows[0].1.size.height, px(32.));
        }
    }

    cx.update(|window, _| window.set_layout_mode(LayoutMode::Logical));
    state.scroll_to_end();
    let rows = draw_rows(cx, &view, &calls);
    let last = rows
        .iter()
        .find(|(index, _)| *index == 9_999)
        .expect("the final row must be reachable");
    assert_eq!(
        last.1.bottom(),
        px(99.25),
        "the final row ends before its 1 px bottom padding"
    );
}

struct CachedRows(Entity<RowsView>);

impl Render for CachedRows {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.0
            .clone()
            .cached(gpui::StyleRefinement::default().size(px(100.)))
    }
}

#[gpui::test]
fn policy_changes_refresh_a_cached_view_after_a_completed_window_frame(cx: &mut TestAppContext) {
    let calls = Rc::new(Cell::new(0));
    let painted: PaintedRows = Rc::new(RefCell::new(Vec::new()));
    let state = ListState::new(10_000, ListAlignment::Top, px(0.));
    let window = cx.add_window(|window, cx| {
        window.set_scale_factor(1.25);
        CachedRows(cx.new(|_| RowsView {
            state: state.clone(),
            calls: calls.clone(),
            painted: painted.clone(),
        }))
    });
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    let initial_calls = calls.get();
    assert!(
        initial_calls > 0,
        "the initial frame must render the actual list"
    );
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    assert_eq!(
        calls.get(),
        initial_calls,
        "an unchanged frame must reuse the cached view"
    );

    for (mode, scale, first_top, second_top) in [
        (LayoutMode::Logical, 1.25, 1.0, 35.0),
        (LayoutMode::Logical, 1.5, 1.0, 35.0),
        (LayoutMode::DevicePixels, 1.25, 0.8, 33.6 + 0.8),
    ] {
        painted.borrow_mut().clear();
        let before = calls.get();
        cx.update_window(window.into(), |_, window, cx| {
            window.set_layout_mode(mode);
            if window.scale_factor() != scale {
                window.set_scale_factor(scale);
            }
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert!(
            calls.get() > before,
            "geometry changes must invalidate cached-view reuse"
        );
        assert!(
            calls.get() - before < 20,
            "refresh must keep list rendering bounded"
        );
        let rows = painted.borrow();
        assert!(
            rows.len() >= 2,
            "the refreshed view must actually paint rows"
        );
        assert_eq!(rows[0].1.top(), px(first_top));
        assert_eq!(rows[1].1.top(), px(second_top));
        assert_eq!(state.logical_scroll_top().item_ix, 0);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(0.));
    }
}
