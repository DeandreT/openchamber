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
- Logical intrinsic horizontal measurements preserve their round-trip lower
  bound using the adjacent finite device value when available. Only a positive
  finite width with a positive finite scale, no known width and MinContent or
  MaxContent available width can receive this correction. Definite width
  constraints, measured heights and the default device-pixel policy keep their
  conversion; exceptional values retain the existing result.
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

### Why intrinsic widths preserve their round-trip lower bound

An intrinsic text advance becomes a finite wrapping constraint in a later
measurement. Floating-point scale conversion can otherwise make that constraint
smaller than the advance it came from and split a fitting word. Advancing only
a contracting intrinsic width to the adjacent device value preserves the bound
without rounding fractional row heights or enlarging a caller's definite width.

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

## Frame-local text-link accessibility

### Ownership

`window/a11y.rs` owns opt-in text-link scopes, staged accessible nodes, geometry
validation and weak frame permits. Base owns measured fragment candidates, the
final shaped-text hit predicate and the original local Link host. Window frame
completion owns acceptance and retirement. The existing dispatch tree and view
cache keep their ordinary ownership.

### Invariants

- A source descriptor retains a weak entity, an incarnation and a function
  pointer. Every enclosing TextView contributes a source guard. A false entry
  latch stays false for that frame; later parsing cannot revive its links.
- Both scope methods run synchronously during prepaint. Only paths containing
  registered links acquire scope records in a lazily allocated window arena.
  Closing the outer scope visits its recorded children once and drops each clip
  closure after at most one call. Source-only deferred ancestry carries no
  inherited post-prepaint clipping.
- A registration captures one zero-area real Link host and its read-only,
  actionless Label leaves. Candidate keys use the existing synthetic-ID hash.
  Final geometry must retain candidate order and keys, have positive finite
  bounds, and fit both the candidate and effective mask. Base retains the actual
  layout and repeats its existing hit predicate on the final clipped centre.
- Captured nodes stay outside the ordinary completed-node buffer until frame
  completion. Reserved parent gaps preserve original sibling order. The existing
  serialization walk merges visible edges and clears references to omitted links.
- Local paint uses the host's original position and dispatch namespace. Its weak
  permit checks the pending arena, window, slot, nonce, final geometry and live
  sources. A host paints at most once. Hidden hosts install no local listeners or
  Tab stops. Publication also requires successful local paint and current sources.
- Outside drawing, activation requires the last accepted arena and returns the
  first surviving fragment. Frame acceptance, rollback and source replacement
  revoke old permits independently of parsed-document identity. An accepted empty
  frame retires the previous arena even when accessibility is inactive.
- Registered roots gate explicit accessible actions, built-in Focus and direct
  Window focus. Tab searches use a local cursor and skip invalid registered links
  before changing focus. Fragment actions and registered-Link synthetic pointer
  fallback are rejected. Completion removes invalid focus associations without
  deleting dispatch descendants and blurs only the invalid host that still owns
  window focus. Application focus-lost handlers retain their existing route.
- Every transaction takes a constant-size entry checkpoint. An entry with no
  source scope records only scalar positions, stack depth and its open parent's
  identity/child lengths. Source-active entries additionally snapshot their open
  source/builder depth. Retryable ordinary child edges stay in a private suffix
  until their ancestor completes. A changed nonrewound registration nonce makes
  an enclosing attempt link-relevant even if it started before TextView prepaint
  or an inner rollback already removed all new link records.
- A link-relevant Err removes its discarded ordinary and staged node IDs,
  including InlineObject, and restores parent suffixes, source promotion, focus
  claims and marker mutations before identical-ID retry. Balanced prepaint can
  change only the entry's top open parent, so unscoped rollback needs no depth
  scan or per-ordinary-node journal. An unscoped attempt with no link registration
  retains the existing ordinary transaction behavior. Earlier completed nodes
  and unrelated maps remain in place. Registration nonces never rewind.
- Each cached view state retains one shared marker. Registrations mark all active
  cached owners, including owners saved by deferred draws. Marked branches cannot
  replay raw prepaint or paint. A link-only miss preserves descendant cache keys
  and ordinary cached siblings; ordinary dirty/key/refresh misses retain their
  existing refresh behavior. A fresh no-link render clears the marker.
- Deferred draws save weak source ancestry and cached-owner markers, including
  inherited contexts. Actual deferred prepaint installs these stacks and uses the
  window mask plus its explicit deferred mask as its link geometry baseline.
  Nested deferred links can mark an ancestor cache state after that owner returns.
  Raw prepaint ranges compare the nonrewound registration clock. An aborted inner
  registration cannot make a cache state written by that attempt appear link-free
  merely because its marker was restored. After actual deferred prepaint, a changed
  clock re-marks its captured cached owners before their stack is restored, including
  when an inner Err discarded every registration. These writes join any still-open
  transaction's marker journal. Paint ranges audit actual local paints.

### Why publication follows local paint

The host's ordinary paint establishes its Tab stop and listeners in the original
order. Accepting staged metadata at frame completion joins geometry, source
validity and actual painting without moving the host into a later draw queue.

### Why link caches retain shared owner markers

A deferred child can register after its cached ancestor has returned. Sharing the
retained marker updates that ancestor's next cache decision immediately. Keeping
link-only misses separate from ordinary refresh misses preserves unrelated cached
siblings beneath the marked ancestor.

A deferred range containing any registration attempt cannot be replayed. Its
captured cached owners retain that same exclusion until fresh prepaint, including
when an inner rollback leaves only ordinary deferred content. Post-child marking
keeps owner cache decisions consistent with the completed range's monotonic clock.

### Test-support observation and adapter events

The `test-support` feature adds three Window methods and four diagnostic types.
They are absent when the feature is off. Deterministic TestWindows created with
accessibility allowed retain their original adapter callbacks in a Send-compatible
Mutex wrapper. `simulate_a11y_transition_for_test` invokes the original Activate
or Deactivate callback during idle/prepaint/paint without synchronizing the frame
snapshot or pumping the scheduler. ForceDisable and ClearForceDisable explicitly
inject the existing same-window policy only while idle, preserving adapter demand
and refreshing only for a changed policy. Birth-disabled windows have no callback
seam and retain their separate keyboard test path.

`begin_a11y_link_observation_for_test` starts an idle-only diagnostic epoch.
`take_a11y_link_observation_for_test` copies the latest observed accepted numeric
candidate/key metadata and weak frame permits, then drains only interval counters.
Until an actual observed draw completes, accepted_draw is unknown and its list is
empty. No-link and inactive completed draws still advance the diagnostic sequence.
An unmount replaces the list with empty metadata. Reading diagnostics never runs
a source validator, clipper or repair, or changes any real frame, nonce, source,
focus, cache marker or tree. Observation retains no strong arena/entity/layout or
historical frame list. Unused observation has no counter sink or metadata vectors.

`surviving_fragments` copies actual finalized keys and logical bounds with the same
finalized/source-valid guard as `surviving_keys`, including unpainted registrations.
It performs no scale conversion or geometry reconstruction and grants no live
activation or publication authority. Capture and read copy only numeric values;
both remain excluded from the diagnostic event/visit counters.

Counters use checked increments at real event and loop-visit sites. They cover
draw starts/acceptance, registrations, scope promotions, actual clipper calls,
fragment/source/scope/cache-owner and checkpoint/rollback work, actual link-arena
creation, rejected raw replay attempts, tree defects before rejection/repair and
actual draw-time platform tree sends. Repeated visits count repeatedly. Rollback
does not undo diagnostic counts. Snapshot copying and ordinary text work are
excluded. Scalar truncation of numeric edge vectors performs no per-edge walk and
adds no invented visits. Link-arena creation counts do not measure total heap
allocation. Source audit and application render counters remain separate evidence.
Adapter handshake returns and retained debug trees do not establish a platform
tree send or AT delivery.

Test-mode `skip_drawing` acknowledges pending draw dirtiness and returns the usual
arena-clear token before draw-profiler, platform, input-handler or frame work. It
preserves the last rendered dispatch, scene, hitboxes, Tab/focus state, accepted
link arena and diagnostic metadata. It advances no draw/link counters and emits
no synthetic focus loss. Before the first frame it leaves the original empty
state. Production mode retains the normal drawing path.
