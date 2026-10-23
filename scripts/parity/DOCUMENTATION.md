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
On Linux it also needs KDE's `spectacle`, `xdotool`, PyGObject with AT-SPI,
`busctl`, `kwin_wayland`, `dbus-run-session`, `xsettingsd` and fontconfig.

## Layout

| File | Owns |
| --- | --- |
| `runner.py` | Commands, seed copies, isolated servers, scenario steps, comparisons and the report. |
| `virtual_display.py` | The private display runs happen in, and `-- <command>` for probes that need one. |
| `inventory.json` | The feature inventory: stable IDs, reference files, states, scenarios and coverage kind. Requirements only. |
| `scenarios.json` | Scenario steps. Rows are addressed by key prefix and title, never by seeded IDs. |
| `electron_adapter.py`, `electron_bridge.mjs` | Electron through CDP (`scripts/perf/cdp.mjs`): launch, input dispatch, rows, geometry, screenshots, and read-only `observe` of layout and computed styles and `fonts` of the faces a node is drawn with, for porting. |
| `native_adapter.py` | The native window through AT-SPI: launch, activation, generated pointer and key input, accessibility actions, rows, geometry, and content capture. |
| `compare.py` | Pixel, row and geometry comparison. |
| `png.py` | PNG reading and writing with the standard library. |

## Fixtures and isolation

`seed` starts `scripts/perf/isolated-server.mjs` on an empty root with the
fixture provider and creates fixed sessions through
`scripts/perf/seed-long-session.mjs`, then snapshots the server state as
`<root>/seed`. Each client of each scenario gets its own copy of the seed, so a
mutation in one run never changes another's starting state. The native client
connects to an isolated server running on its copy; Electron starts its own
in-process server and OpenCode on its copy, with a fixture provider of its own
and the same OpenCode configuration the isolated server uses
(`isolatedOpencodeConfig` in `scripts/perf/isolated-server.mjs`). Both copies
pin the theme (`useSystemTheme: false`, `themeId`, the derived `themeVariant`
and that variant's theme ID, all of which the web reads); the native client
reads the same theme from `OPENCHAMBER_NATIVE_THEME`.

Both clients run at the standard client size of 1,280 by 800 logical pixels,
under X11 and at a fixed device scale: `--force-device-scale-factor` for
Electron and `GPUI_X11_SCALE_FACTOR` for GPUI. Electron's DevTools endpoint
cannot size its window; when its viewport differs from the requested size the
bridge emulates it and the launch record says `emulated: true`.

## The private display

`run` re-executes itself inside a headless KWin (`kwin_wayland --virtual`)
with its own XWayland and its own D-Bus session, so the clients share no
screen, input, focus or accessibility bus with the desktop. `--display
session` runs on the desktop's own display instead. The private bus activates
no services: KWin's startup would otherwise start a desktop portal and a
secret service on it, which outlive the run and ask the user to unlock their
keyring. The session starts the accessibility bus and registry itself, and
`xsettingsd` publishes fontconfig's `system-ui` family as the GTK font, which
Chromium's `system-ui` follows on a desktop. When the run ends, every process
attached to the private bus is stopped. The session's work directory lives in
`XDG_RUNTIME_DIR` (else `/tmp`): the bus socket inside it must stay within
the 108-byte socket path limit whatever `TMPDIR` is.

Ad hoc probes that open windows use the same display:

```sh
python3 scripts/parity/virtual_display.py -- <command> [arguments]
```

Never start a compositor on a plain `dbus-run-session` for this: services it
activates escape to the user's desktop.

## Evidence

Each checkpoint records a screenshot of the application's content and the
sidebar rows (key, label, selected, expanded and logical rectangle). Electron
rows come from `[data-sidebar-row-key]` elements; a row whose list slot is the
placeholder of a sticky header reads the header painted over it, and its
disclosure state comes from its own toggle, never from a menu trigger inside
it. Native rows are the accessible elements of the sidebar, whose accessible
IDs are the same row keys; a row's own controls are not rows.

Electron is captured with `Page.captureScreenshot`, which is its viewport. The
native window is activated with `xdotool`, captured with spectacle's
active-window capture without the frame's shadow, and cut to the content root
(the `workspace` element) inside any client-side frame line; native geometry
is measured from the same root. Runs are serial and each client starts fresh.

A scenario passes only when every checkpoint matches in size and pixels, rows
and geometry; geometry allows one physical pixel. `--tolerance` ignores
per-channel differences up to its value and is for reviewed rasterization
noise only. A check the candidate's platform API cannot report (AT-SPI carries
no expand or collapse state) is listed as unobserved, never as a match, and so
is a pointer click the host dropped (KWin on Wayland ignores synthetic X input
without a remote-control grant), which the native adapter replaces with the
target's accessibility action; a scenario that is otherwise clean is then
`incomplete`. The report records
`passed`, `incomplete`, `failed` (the native client differs or failed) or
`blocked` (the reference failed), and coverage against the whole inventory: a
feature passes only when every scenario it lists passed. Reports,
screenshots and difference images go to `--output`, by default
`artifacts/parity-<time>`; they are run evidence and never part of the source.

The runner turns the session's accessibility flag on for the run and restores
its previous value.

## Adding a scenario

Give the feature an inventory entry first, with the reference files and the
states it covers, then add steps to `scenarios.json` that name it. The runner
refuses a scenario whose feature is not in the inventory and an inventory entry
that lists a scenario that does not exist. Steps are `click` (a row by zone
prefix and title), `key`, `type`, `wait`, `settle` and `capture`. A capture
can list `targets` from the scenario file's shared target table.

Prefer `wait` to `settle`: it names the observable state a checkpoint needs,
with one condition per client (a page expression for Electron; accessible
elements or a diagnostic marker emitted since the last input for the native
client) and a timeout. A reference that never reaches its state blocks the
scenario; a native client that never does fails it, and its evidence is still
captured.

Every capture first waits up to 15 s for the client to hold still for
750 ms: Electron's text and the boxes of its panels, sections, prompt ticks,
rows and messages, the native client's accessible rows. Panels that fill in after the
transcript would otherwise be captured midway. A reference that never holds
still blocks the scenario; a native client that never does fails it.
