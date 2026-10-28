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
On Linux it also needs `Xvfb`, `xauth`, `xdpyinfo`, `xdotool`, libX11,
PyGObject with AT-SPI, `busctl`, `dbus-run-session`, `xsettingsd` and fontconfig.

## Layout

| File | Owns |
| --- | --- |
| `runner.py` | Commands, seed copies, isolated servers, scenario steps, comparisons and the report. |
| `virtual_display.py` | The private display runs happen in, and `-- <command>` for probes that need one. |
| `inventory.json` | The feature inventory: stable IDs, reference files, states, scenarios and coverage kind. Requirements only. |
| `scenarios.json` | Scenario steps. Rows are addressed by key prefix and title, never by seeded IDs. |
| `electron_adapter.py`, `electron_bridge.mjs` | Electron through CDP (`scripts/perf/cdp.mjs`): launch, input dispatch, rows, geometry, screenshots, and read-only `observe` of layout and computed styles and `fonts` of the faces a node is drawn with, for porting. |
| `native_adapter.py` | The native window through AT-SPI: launch, activation, generated pointer and key input, accessibility actions, rows, geometry, and content capture. |
| `clipboard_probe.py` | Asynchronous clipboard ownership and exact text after a real copy action on the private display. |
| `native_platform.py` | Synthetic native window-manager and IBus checks with private profiles and service logs. |
| `native_accessibility.py` | Orca-generated control announcements and AT-SPI transcript exposure, with silent private speech services. |
| `native_workflow.py`, `workflow_proxy.py` | Connected native send/stop/restart checks and loopback-only loss injection before or after admission. |
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
reads the same theme from `OPENCHAMBER_NATIVE_THEME`. Each client runs with
its copy's own home and XDG directories, so lookups under them, such as the
file manager's icon, see the same files.
Both clients receive an allowlisted environment. Inherited provider credentials
and application configuration stay outside the test processes.

A scenario's optional `settings` object sets preferences in each disposable
copy before launch. Theme pins still take precedence. The original seed is
unchanged, and each client receives an independent copy of the same settings.

Both clients run at the standard client size of 1,280 by 800 logical pixels,
under X11 and at a fixed device scale: `--force-device-scale-factor` for
Electron and `GPUI_X11_SCALE_FACTOR` for GPUI. Electron's DevTools endpoint
cannot size its window; when its viewport differs from the requested size the
bridge emulates it and the launch record says `emulated: true`.
The native adapter resizes its owned X11 window and verifies the content
dimensions after readiness. It accounts for the client-side frame and fails
if the requested size cannot be reached.

## The private display

`run` re-executes itself inside an owned Xvfb server with a private X authority
file, runtime directory and D-Bus session. Each client verifies the Xvfb PID,
display, owner and private directory before starting. Chromium explicitly uses
X11; Wayland and inherited session-type variables are removed. The clients share
no screen, input, focus or accessibility bus with the desktop. The private bus
activates no desktop services. The session starts the accessibility bus and registry itself, and
`xsettingsd` publishes fontconfig's `system-ui` family as the GTK font, which
Chromium's `system-ui` follows on a desktop. When the run ends, every process
attached to the private bus is stopped. The session's work directory lives in
`XDG_RUNTIME_DIR` (else `/tmp`): the bus socket inside it must stay within
the 108-byte socket path limit whatever `TMPDIR` is. Xvfb has no window manager:
OS-level minimize, maximize and window-decoration behavior requires separate
isolated desktop checks.

Ad hoc probes that open windows use the same display:

```sh
python3 scripts/parity/virtual_display.py -- <command> [arguments]
```

Never start a compositor on a plain `dbus-run-session` for this: services it
activates escape to the user's desktop.

## Evidence

Each checkpoint records a screenshot of the application's content and the
sidebar rows (key, label, selected, expanded and logical rectangle). Electron
process output stays in its private `state/electron.log`, including when startup
fails before the first screenshot. Its rows come from `[data-sidebar-row-key]`
elements; a row whose list slot is the
placeholder of a sticky header reads the header painted over it, and its
disclosure state comes from its own toggle, never from a menu trigger inside
it. Native rows are the accessible elements of the sidebar, whose accessible
IDs are the same row keys; a row's own controls are not rows.

New-session draft IDs are allocated independently by each client. Their
comparison keys use the containing group and displayed draft ordinal; counts,
labels, states and rectangles still have to match. Checkpoint records retain
the original IDs. Session and project IDs remain exact.

Electron is captured with `Page.captureScreenshot`, which is its viewport. The
native window is raised and focused with `xdotool`, captured through Xlib
without a desktop screenshot service, and cut to the content root
(the `workspace` element) inside any client-side frame line; native geometry
is measured from the same root. Runs are serial and each client starts fresh.

A scenario passes only when every checkpoint matches in size and pixels, rows
and geometry; geometry allows one physical pixel, with 0.0001 physical pixels
for floating-point conversion error at that boundary. A zero-sized reference
target and an absent native node both count as hidden; a visible target missing
from either client still fails. `--tolerance` ignores
per-channel differences up to its value and is for reviewed rasterization
noise only. Glyph-edge noise is forgiven only at the checkpoints
`exceptions.json` lists, each with its region, reason and the user's
decision: there the reference's visible text boxes (text nodes' line boxes
and text fields, read after the screenshot) bound where a differing pixel may
count as noise, and only when each image's colour lies within the range the
other holds in the pixel's 3x3 neighbourhood (give or take `GLYPH_SLACK`
levels): the same ink within a pixel, as the geometry rule allows. Such pixels are reported as `glyphEdgePixels` and drawn
yellow in the difference image; moved, missing or recoloured glyphs leave
solid differences and still count. `compare_test.py` checks that split
(`python3 -m unittest compare_test`, from this directory); rerun it whenever
the comparison changes. The pinned local AccessKit patch supplies AT-SPI
expandable, expanded and collapsed states; row comparisons check these states.
A check the candidate's platform API cannot report is listed as unobserved,
never as a match, and so
is a pointer click the host dropped, which the native adapter replaces with the
target's accessibility action; a scenario that is otherwise clean is then
`incomplete`. The report records
`passed`, `incomplete`, `failed` (the native client differs or failed) or
`blocked` (the reference failed), and coverage against the whole inventory: a
feature passes only when every scenario it lists passed. Reports,
screenshots and difference images go to `--output`, by default
`artifacts/parity-<time>`; they are run evidence and never part of the source.
An existing output directory is refused so another run cannot overwrite its
evidence. The report fingerprints the native and Electron executables, Electron
package, web assets, OpenCode binary, seed, scenarios and exceptions. If these
inputs change before the run ends, every result is blocked.

The runner turns the session's accessibility flag on for the run and restores
its previous value.

`exceptions.json` also records approved shadow/gradient noise. At those
checkpoints only, the reference supplies the exterior shadow bounds of its
composer and work-status card and its visible `.chat-scroll-fade` bands.
Shadow bounds use the computed offset, spread and 1.5 times the blur radius.
The full card and composer rectangles, text, controls, icons and media are
excluded. Differences of at most two RGB levels per channel count separately
as `paintNoisePixels` and appear cyan in the diff image. Geometry and larger
differences remain strict. Other checkpoints use no paint exception.

The separately approved `composerGradientNoise` exception allows one RGB level
in blank pixels of the dark composer's vertical sheen, only at its named
checkpoints. Chromium dithers that low-alpha gradient in raster-tile coordinates,
which vary with its renderer settings. The reference accepts only the complete
single-layer vertical 1%-to-transparent-at-50% definition and supplies its
upper-half bounds and protection boxes. The candidate supplies a conservative
central region and protects its whole editor, named content and controls.
The gate intersects both regions and excludes protection boxes from both clients,
including reference text, caret, selections, borders and corners. Missing
candidate coverage, light composers and unknown gradient shapes get no allowance.
These pixels are reported separately as `composerGradientNoisePixels`; larger
differences, content and geometry remain failures.

## Native Linux platform checks

`native_platform.py` runs the synthetic native client on its own Xvfb display
with Openbox. It verifies maximize, restored content size, minimize and clean
close through real pointer input; an accessibility substitution fails the check.
Its selection check drags across Unicode messages, copies while the selected
rows are visible, scrolls them offscreen, then returns and compares both copies.
The optional IBus check exercises libpinyin preedit and commit, including Enter
committing a composition without submitting the draft. It does not qualify the
candidate popup or every IME and language.

```sh
python3 scripts/parity/native_platform.py --checks windows
python3 scripts/parity/native_platform.py --checks selection
python3 scripts/parity/native_platform.py --checks windows,ime
```

Openbox is required for these checks; `ime` also requires IBus, libpinyin and
`gsettings`. All service configuration and state is private to the run.
Reports and service logs go under a new directory in `native/target/tmp`, or an
empty directory supplied with `--output`. The run never uses the user's input
method or window manager. The display wrapper removes inherited IME addresses.
Platform and Orca child processes receive an allowlisted environment, without
provider credentials or inherited application configuration.

`ClipboardProbe` primes a private clipboard sentinel and waits for a new owner
and the exact returned text. It dispatches a copy once and fails on timeout;
re-reading an old clipboard cannot count as a passing copy check. GTK clipboard
initialization happens before the client opens so it cannot disturb input focus.
Timeout and completion cancel any outstanding clipboard read.

`native_accessibility.py` starts Orca and a private Speech Dispatcher with a
silent module. It checks generated speech for the project trigger, search field,
highlighted option and restored composer, plus rendered transcript text in the
AT-SPI tree. Audio-server addresses point at nonexistent private sockets; the
test never plays speech through the user's desktop. Audible playback and full
screen-reader browsing remain separate checks.

```sh
python3 scripts/parity/native_accessibility.py
```

It requires Orca and Speech Dispatcher. Reports and debug logs go into a new
directory under `native/target/tmp`, with the same isolated display and profile
rules as the window and IME checks.
Both native check commands accept `--scale 1`, `1.25`, `1.5` or `2`; the
logical viewport stays 1280 by 800 and the private display grows to fit it.

## Connected native workflow

`native_workflow.py` uses an independent copy of the parity seed and the local
fixture provider. It checks a real send and stop, exact Unicode draft restoration
after normal close, and that reopening does not resubmit the message.

```sh
python3 scripts/parity/native_workflow.py --scale 1.25
python3 scripts/parity/native_workflow.py --scale 1.25 --fault before
python3 scripts/parity/native_workflow.py --scale 1.25 --fault after
python3 scripts/parity/native_workflow.py --scale 1.25 --new-session
python3 scripts/parity/native_workflow.py --new-session --fault before --fault-operation create
python3 scripts/parity/native_workflow.py --new-session --fault after --fault-operation create
```

`--new-session` selects a registered project through the native picker and
checks that the first prompt creates exactly one session in that directory.
Creation response-loss checks restart with the pending operation, reject
automatic creation or prompt retries after failed observation, and require an
explicit retry to reuse the saved session identity. The proxy records creation
and prompt requests separately so a duplicate session cannot hide behind one
successful prompt.

The fault proxy accepts only a loopback upstream. It loses one prompt response
before forwarding or after the server answered, and temporarily blocks observation
reads and event delivery. After restarting the client, the check first proves that
a failed lookup sends nothing. It then permits a fresh check: an existing message
resolves without another POST; a missing message retries only through the explicit
action and keeps the original ID. The server-side user-message count must remain one.

Unicode is supplied through a real clipboard paste. AT-SPI string key synthesis
does not reliably preserve non-ASCII text on X11; it is not a valid Unicode input
probe. `ClipboardProbe.paste` serves the private clipboard and waits for the exact
visible result. These workflow checks qualify outcomes, not timing or visual parity.

## Adding a scenario

Give the feature an inventory entry first, with the reference files and the
states it covers, then add steps to `scenarios.json` that name it. The runner
refuses a scenario whose feature is not in the inventory and an inventory entry
that lists a scenario that does not exist. Steps are `click` (a row by zone
prefix and title), `clickTarget` (a named control with an Electron selector
and a native accessible ID), `key`, `type`, `wait`, `settle` and `capture`. A capture
can list `targets` from the scenario file's shared target table.

Prefer `wait` to `settle`: it names the observable state a checkpoint needs,
with one condition per client (a page expression for Electron; accessible
elements or a diagnostic marker emitted since the last input for the native
client) and a timeout. A reference that never reaches its state blocks the
scenario; a native client that never does fails it. Either failure captures
`wait-failed.png` and stops the remaining inputs, so a failed popup wait cannot
turn later typing into a composer submission.

Native waits can also match a named accessible node with
`{"named": {"name": "Search projects...", "role": "entry"}}`. Optional `id`
and `role` fields distinguish matching labels; `"present": false` waits for
the node to disappear. Typing starts only after the
popup's search field mounts.

Session-row clicks first require a settled client, then verify that the exact
requested row is selected. An unrelated selected row cannot satisfy this
postcondition. `runner_test.py` covers these input checks; run it from this
directory with `python3 -m unittest runner_test`.

Every capture first waits up to 15 s for the client to hold still for
750 ms: Electron's text and the boxes of its panels, sections, prompt ticks,
rows and messages, the native client's accessible rows. Electron must also
have no fetch or XHR request in flight; a panel whose data has not arrived can
hold still for longer than the quiet period. Panels that fill in after the
transcript would otherwise be captured midway. A reference that never holds
still blocks the scenario; a native client that never does fails it.
