# Layout, text wrapping and quit public-API tests

This standalone package tests the local `gpui-pre` patch as a dependency with
`test-support`. It does not compile GPUI's upstream library unit-test module.
The tests use GPUI's test platform, empty views, painted rectangles and its
deterministic `NoopTextSystem` through `WindowTextSystem::shape_text`. They
require no display server, font assets, GUI launch or extra test dependencies.
Intrinsic text measurement checks fractional logical widths against the default
rounded policy using the test platform's deterministic glyph advances.

Run from `native/`, so Cargo uses its pinned toolchain, after other native builds
have finished:

```sh
CARGO_TARGET_DIR="$PWD/target" cargo test --offline \
  --manifest-path vendor/gpui-pre/layout-tests/Cargo.toml \
  --test logical_layout
```

The shaped-text regressions run separately:

```sh
CARGO_TARGET_DIR="$PWD/target" cargo test --offline --locked \
  --manifest-path vendor/gpui-pre/layout-tests/Cargo.toml \
  --test text_wrap
```

They cover ASCII-space overflow, repeated, leading and trailing spaces,
unchanged NBSP, narrow NBSP and tab handling, UTF-8 byte indices and line clamps.
They require no platform fonts and do not substitute a copied wrap algorithm.

The quit lifecycle regressions run separately, after other native builds finish:

```sh
CARGO_TARGET_DIR="$PWD/target" cargo test --offline --locked \
  --manifest-path vendor/gpui-pre/layout-tests/Cargo.toml \
  --test quit
```

These tests call `App::quit` through the public test context and register the
real quit subscriptions. Standard-library futures and explicit wakeups gate
foreground save completion without timers, additional dependencies or platform
fonts. Coverage includes persistent vetoes, callback delivery and request
coalescing, foreground-dependent close, failed-save retry, synchronous
last-window close, retained closed-window subscriptions and released entities.
Deferred window creation and guard registration must receive a fresh consent
check before quit can proceed. Unrelated deferred effects preserve consent.
`TestAppContext::quit` is teardown and bypasses preflight; it is not used to
request application quit in these scenarios.

The test platform records `Platform::quit` calls and retains the real `on_quit`
callback. `TestAppContext::platform_quit_count` observes accepted requests;
`simulate_platform_quit` delivers the callback explicitly without stopping the
test scheduler. Before an application quit, delivery models an OS-requested quit
and preserves veto and borrowed-app rejection. After an accepted platform quit,
delivery models final post-loop notification. Late idle window creation and guard
registration cannot cancel that notification or skip exactly-once final cleanup.
OS-requested quit still rechecks guards registered by deferred effects before
committing to shutdown.

This target exercises the real app callback, final cleanup, view updates and
window lifetime. It does not prove that an operating system stops its loop,
honors a platform callback veto or hands over its clipboard. Those require
private-platform runtime qualification. The separate in-module quit tests remain
outside this target.

The first run resolves the standalone package's lockfile from the local cache.
Retain that generated `Cargo.lock` and add `--locked` on subsequent runs.
The dependency is pinned to `gpui-pre` 0.3.9 and patched to the parent directory.
An unavailable cached dependency is an offline resolution failure; it does not
authorize downloading dependencies.

Coverage includes the default policy, 500 independently measured 34 px rows at
1x, 1.25x and 1.5x, measured leaf bounds, negative and positive fractional
translations, and real virtual-list paint callbacks. Public item bounds verify
mode/scale invalidation, exact anchors, retained measurements on ordinary
scrolls, and final-row reachability. Render-call assertions bound work for a
10,000-row list.

The list is owned by a retained `Render` entity because its paint phase registers
scroll notifications against the current view. A separate full-window test
uses a cached child view across completed frames. It checks cache reuse before
a policy change and verifies fresh row rendering after mode and scale changes.

Canvas callbacks verify the geometry supplied to painting and submit real quads.
These tests do not read GPU pixels or certify rasterization, fonts, borders or
application visual parity. The existing in-module unit tests remain intact.
