# Local GPUI layout, text and quit contracts

## Ownership

This directory vendors the crates.io `gpui-pre` package at version `0.3.9`.
The starting source is the exact local registry package
`index.crates.io-1949cf8c6b5b557f/gpui-pre-0.3.9`. Its manifests, README,
resources and `LICENSE-APACHE` are retained. The package's upstream repository
is recorded in `Cargo.toml`.

The local patch owns an opt-in window layout policy, invalidation of GPUI's
retained virtual-list measurements, ASCII-space overflow handling in shaped
text, and cancellable quit preparation before the platform stops its event loop.
Application startup owns selecting the layout policy. Application views own
their close, persistence and retry behavior. The accessibility
bridge also supports explicit active-descendant
relationships between a focused search editor and sibling options, documented
in `src/window/a11y.rs`. `Window::hitbox_bounds_above` reports what the last
frame drew above a hitbox, for the desktop's native WebKit child windows.
Toolkit editing, transport and application row models remain outside this patch.

## Invariants

- `LayoutMode::DevicePixels` is the default. It keeps the original authored
  metric snapping, measured-size ceiling, layout-edge rounding and snapped
  element offsets.
- `window.set_layout_mode(LayoutMode::Logical)` keeps non-stroke authored
  lengths, measured sizes and layout bounds fractional in logical pixels.
  Taffy still computes in device-space units, with its own rounding disabled.
- Intrinsic text widths retain their shaped fractional advance in logical mode.
  Device-pixel mode keeps the original whole-logical-pixel ceiling. Text layout
  caches include the mode so a policy change cannot reuse the other width.
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

- Shaped-text wrapping does not break on overflow of an ASCII space. The next
  non-space character establishes its candidate before overflow is resolved.
  Spaces retain their advances and original byte indices; no text is rewritten.
  Non-ASCII whitespace, tabs, candidate tracking and line-clamp limits retain
  their existing rules. The editor's separate `LineWrapper` is unchanged.

- `App::on_request_quit` runs every active callback before an application quit
  can reach `Platform::quit`. A `false` return vetoes that request and retains
  the subscription. The event loop, windows and foreground tasks stay live.
- `App::quit(&self)` schedules one foreground preflight after the current
  update returns. Repeated queued requests are coalesced. A veto can start a
  normal asynchronous window close; the application reissues quit after close.
  Closing a window synchronously during preflight schedules a fresh check after
  the callbacks return. A veto without a closed window never retries itself.
- `Context::on_request_quit_in` captures a weak entity and the supplied window's
  identity. Released entities and closed windows permit quit even when another
  owner retains the subscription or entity. A busy live window vetoes the check.
- Consent records a revision that changes only when a window is successfully
  opened or a quit guard is registered. After deferred effects drain, the
  application checks that revision before calling `Platform::quit`. Stale consent
  queues a fresh foreground preflight; a veto never retries for a revision change
  alone. Unrelated effects and window removal preserve accepted consent.
- Accepted application quit commits immediately before `Platform::quit`, with
  the app borrow released before the platform call. Remaining platform idle work
  can open windows or register guards, but cannot revoke committed quit or request
  another platform stop. The final platform callback bypasses preflight and
  reaches shutdown when the app can be borrowed.
- An OS-requested platform callback without committed application quit runs the
  same preflight and revision check before committing to `App::shutdown`.
  Failure to borrow the app still returns `false`, including after commitment.
  Cancellation requires a live platform loop; external fatal loop termination
  cannot be recovered here.
- `on_app_quit` remains final, uncancellable cleanup. Its futures cannot rely
  on live windows or foreground entity updates. Shutdown removes those observers
  once and retains the existing shutdown timeout and error reporting.

- `Window::hitbox_bounds_above(id)` reads only the last rendered frame. It lists
  the hitboxes inserted after `id`, in insertion order, each intersected with
  its own content mask, and drops empty ones. A hitbox missing from that frame
  yields `None`. It changes no hit testing, painting or frame state.

## Decisions

### Why overlays are read from hitboxes

A native child window always covers GPUI's drawing, so the desktop cuts the
window's shape wherever GPUI draws above it. Menus, popovers and dialogs are
deferred or later-painted and `occlude` their area, so the hitboxes inserted
after the child's own are exactly what sits on top. Tracking every overlay in
application code was the alternative; it delayed each overlay by 70 to 330 ms
in the composition probe and missed overlays the application did not know of.


### Why quit preparation precedes the platform quit call

Linux stops its event loop before invoking the final quit callback. Shutdown
then removes windows and blocks while polling final cleanup futures. Window and
foreground-dependent preparation must veto quit first and use the application's
normal close path while that loop is still running.

### Why preflight runs after the current update

A quit action can run inside the same window and entity that must prepare for
close. A foreground preflight releases those update borrows before invoking the
guard and preserves the existing shared-reference `App::quit` API. The platform
quit call also runs outside the app borrow so a synchronous platform callback
can complete shutdown. A veto is automatically rechecked only after a decreased
window count, which handles reentrant last-window close without looping on a
persistent veto.

### Why consent tracks new windows and quit guards

Preflight callbacks can defer opening a window or registering another guard.
The outer update drains those effects after the callbacks return. Consent must
cover the resulting windows and guards before the platform can stop. A revision
captured before callbacks and checked after effect draining detects these new
obligations without invalidating consent for unrelated notifications or defers.

### Why quit commits at the platform call

Linux signals an irreversible loop stop in `Platform::quit`, then can drain
remaining idle callbacks before its final quit callback. Rechecking consent
after that stop can skip shutdown with no live loop available to retry it.
Committing after the last revision check preserves cancellable preparation and
makes final cleanup independent of changes made by that remaining idle work.

### Why ASCII-space overflow waits for the next character

A word can fit while its following space exceeds the width. Breaking on that
space uses the previous candidate and moves the fitting word to the next line.
Waiting for the next character preserves the word and its following break
opportunity. The guard tests only U+0020 so it does not redefine Unicode
whitespace or non-breaking-space behavior.

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

The `text_wrap` public-API tests use `WindowTextSystem::shape_text` with
`NoopTextSystem` to cover fitting words, repeated, leading and trailing ASCII
spaces, unchanged non-ASCII whitespace and tabs, byte indices and line clamps.
They exercise the real wrap implementation with deterministic glyph advances.

The `app::quit_tests` unit tests use deterministic platform windows, gated
foreground drains and an instrumented quit callback. They cover default consent,
persistent vetoes, subscription release, repeated requests, failed-close retry,
synchronous last-window close, OS-requested quit and borrowed-app rejection.
They also cover deferred new-window consent invalidation without a new guard,
and OS consent invalidation after a guard is registered by a deferred effect.
They do not certify an operating system's event loop or clipboard handover.

The public quit tests use `TestAppContext::platform_quit_count` to observe
accepted platform requests and `simulate_platform_quit` to deliver the registered
callback separately. These controls preserve the live test scheduler and allow
late idle window creation and guard registration before final cleanup. The tests
check exactly-once final cleanup, default consent, OS vetoes and borrowed-app
rejection through the real app callback without upstream unit-test font assets.

These are local fork patches. Remove the logical-layout changes when the pinned
upstream toolkit supplies equivalent opt-in logical geometry, translation and
list-invalidation behavior. Remove the ASCII-space guard when the pinned
upstream shaped-text wrapper preserves fitting words across trailing-space
overflow with the same text, index and line-clamp guarantees. Keep the focused
regressions when evaluating either replacement.

Remove the quit patch when the pinned upstream toolkit supplies cancellable
preflight before platform loop termination and preserves window-dependent
asynchronous close, repeated-request and synchronous-close behavior.
