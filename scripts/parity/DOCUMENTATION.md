# Desktop UI parity runner

Runs the scenarios of the [parity method](../../docs/rust-desktop/PARITY.md)
against the packaged Electron desktop (the reference) and the native desktop
(the candidate), and reports every difference.

```sh
bun run parity seed                       # build tmp/parity/seed once
bun run parity run                        # every scenario
bun run parity run --scenarios shell.startup --output /tmp/parity-run
```

`run` needs the packaged Electron build (`packages/electron/dist/...`), the
native release build (`bun run native:build`), the staged web UI and the
bundled OpenCode binary; the defaults point at the repository's build outputs.
On Linux it also needs KDE's `spectacle`, PyGObject with AT-SPI and `busctl`.

## Layout

| File | Owns |
| --- | --- |
| `runner.py` | Commands, seed copies, isolated servers, scenario steps, comparisons and the report. |
| `inventory.json` | The feature inventory: stable IDs, reference files, states, scenarios and coverage kind. Requirements only. |
| `scenarios.json` | Scenario steps. Rows are addressed by key prefix and title, never by seeded IDs. |
| `electron_adapter.py`, `electron_bridge.mjs` | Electron through CDP (`scripts/perf/cdp.mjs`): launch, input dispatch, rows, geometry and screenshots. |
| `native_adapter.py` | The native window through AT-SPI: launch, generated pointer and key input, accessibility actions, rows, geometry, and client-area capture. |
| `compare.py` | Pixel, row and geometry comparison. |
| `png.py` | PNG reading and writing with the standard library. |

## Fixtures and isolation

`seed` starts `scripts/perf/isolated-server.mjs` on an empty root with the
fixture provider and creates fixed sessions through
`scripts/perf/seed-long-session.mjs`, then snapshots the server state as
`<root>/seed`. Each client of each scenario gets its own copy of the seed, so a
mutation in one run never changes another's starting state. The native client
connects to an isolated server running on its copy; Electron starts its own
in-process server and OpenCode on its copy. Both copies pin the theme
(`useSystemTheme: false` and `themeId`); the native client reads the same theme
from `OPENCHAMBER_NATIVE_THEME`.

Both clients run at the standard client size of 1,280 by 800 logical pixels,
under X11 and at a fixed device scale: `--force-device-scale-factor` for
Electron and `GPUI_X11_SCALE_FACTOR` for GPUI. Electron's DevTools endpoint
cannot size its window; when its viewport differs from the requested size the
bridge emulates it and the launch record says `emulated: true`.

## Evidence

Each checkpoint records a client-area screenshot and the sidebar rows (key,
label, selected, expanded and logical rectangle). Electron rows come from
`[data-sidebar-row-key]` elements; native rows are the accessible elements of
the sidebar, whose accessible IDs are the same row keys. Electron is captured
with `Page.captureScreenshot`; the native window with spectacle's active-window
capture without decoration or shadow, which requires the window to be active.
Runs are serial and each client starts fresh, so the window under test is the
active one.

A scenario passes only when every checkpoint matches in size and pixels, rows
and geometry; geometry allows one physical pixel. `--tolerance` ignores
per-channel differences up to its value and is for reviewed rasterization
noise only. The report records `passed`, `failed` (the native client differs
or failed) or `blocked` (the reference failed), and coverage against the whole
inventory: a feature passes only when every scenario it lists passed. Reports,
screenshots and difference images go to `--output`, by default
`artifacts/parity-<time>`; they are run evidence and never part of the source.

The runner turns the session's accessibility flag on for the run and restores
its previous value.

## Adding a scenario

Give the feature an inventory entry first, with the reference files and the
states it covers, then add steps to `scenarios.json` that name it. The runner
refuses a scenario whose feature is not in the inventory and an inventory entry
that lists a scenario that does not exist. Steps are `click` (a row by zone
prefix and title), `key`, `type`, `settle` and `capture`.
