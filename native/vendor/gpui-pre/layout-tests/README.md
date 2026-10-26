# Logical layout public-API tests

This standalone package tests the local `gpui-pre` patch as a dependency with
`test-support`. It does not compile GPUI's upstream library unit-test module.
The tests use GPUI's test platform, empty views and painted rectangles. They
require no display server, font assets, GUI launch or extra test dependencies.

Run from `native/`, so Cargo uses its pinned toolchain, after other native builds
have finished:

```sh
CARGO_TARGET_DIR="$PWD/target" cargo test --offline \
  --manifest-path vendor/gpui-pre/layout-tests/Cargo.toml \
  --test logical_layout
```

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
