# Fractional logical layout

## Ownership

This directory vendors the crates.io `gpui-pre` package at version `0.3.9`.
The starting source is the exact local registry package
`index.crates.io-1949cf8c6b5b557f/gpui-pre-0.3.9`. Its manifests, README,
resources and `LICENSE-APACHE` are retained. The package's upstream repository
is recorded in `Cargo.toml`.

The local patch owns an opt-in window layout policy and invalidation of GPUI's
retained virtual-list measurements. Application startup owns selecting the
policy. The accessibility bridge also supports explicit active-descendant
relationships between a focused search editor and sibling options, documented
in `src/window/a11y.rs`. Toolkit editing, transport and application row models
remain outside this patch.

## Invariants

- `LayoutMode::DevicePixels` is the default. It keeps the original authored
  metric snapping, measured-size ceiling, layout-edge rounding and snapped
  element offsets.
- `window.set_layout_mode(LayoutMode::Logical)` keeps non-stroke authored
  lengths, measured sizes and layout bounds fractional in logical pixels.
  Taffy still computes in device-space units, with its own rounding disabled.
- Independent virtual-list roots return logical heights. The list's existing
  sum tree and visible-row placement accumulate these heights without rounding.
  Row indices, ordering and logical scroll offsets retain their existing meaning.
- Element translations remain fractional until painting. Existing paint
  primitives still snap final absolute bounds and clipping masks.
- Logical-mode shadow shapes retain fractional spread and inset-hole edges.
  Drop-shadow radii include their spread; clip masks retain conservative rounding.
- Border widths retain the original stroke snapping in both layout and paint.
  Logical mode does not promise Chromium-equivalent border thickness. Compare
  border-consuming content geometry separately at fractional display scales.
- Scroll-padding calculations use the same metric policy as layout.
- Configure the policy before the first layout. A later change must occur
  between frames; changing it during drawing panics. A changed policy clears
  the layout engine and refreshes cached views. Repeating the same policy is a no-op.
- Lists invalidate retained item sizes when their mode or scale changes,
  before inferred sizing reads their height summary. They preserve focus handles
  and logical scroll anchors. Existing pending scroll and follow-tail rules remain
  authoritative. Width invalidation retains its existing behavior.
- Invalidation scans retained list records once per geometry transition.
  Ordinary scrolling adds only a constant-time mode/scale comparison and retains
  the existing visible-range and overdraw work.

## Decisions

### Why logical measurements must survive independently laid-out roots

At scale 1.25 a 34 logical px row spans 42.5 device pixels. Rounding every root's
height before the virtualizer adds them accumulates an error at every row.
Keeping logical heights lets final painting snap the actual translated edges.

### Why the policy belongs to the window

Virtual rows and deferred elements request layout outside the root view's render
call. A window policy covers these operations consistently and prevents a frame
from mixing geometry conventions. Existing GPUI consumers keep their default.

### Why stroke handling remains independent

GPUI intentionally gives thin strokes physical-pixel widths. Changing that policy
would alter borders throughout the application. This patch addresses non-stroke
geometry and preserves the existing border layout and painting agreement.

## Validation and maintenance

Focused tests live alongside `taffy`, `window`, `elements/list` and `elements/div`.
The standalone [public-API test package](layout-tests/README.md) exercises the
patched library through `test-support` without compiling upstream library unit
tests. Its public bounds and paint callbacks cover measurement, translations,
virtual-list anchors and geometry invalidation without font assets or a GUI.
They cover authored lengths, measured extents, independent 34 px row roots,
fractional positive and negative translations, final paint snapping, scroll
padding, default behavior, and mode/scale invalidation with exact logical anchors.
The virtual-list transition test also bounds row-render calls while scrolling a
10,000-row list and checks that ordinary scrolling retains offscreen measurements.

Application qualification must retain the 1x geometry baseline and compare 1.25x
and 1.5x captures using the existing physical-pixel allowance and approved glyph
and shadow exceptions. Font baseline correction and application-owned geometry
caches are separate responsibilities. This patch does not certify platform or
visual parity.

Remove the patch when the pinned upstream toolkit supplies equivalent opt-in
logical geometry, translation and list-invalidation behavior. Keep the focused
regressions when evaluating that replacement.
