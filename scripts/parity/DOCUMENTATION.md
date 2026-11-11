# Desktop UI parity runner

Runs the scenarios of the [parity method](../../docs/rust-desktop/PARITY.md)
against the packaged Electron desktop (the reference) and the native desktop
(the candidate), and reports every difference.

```sh
bun run parity seed                       # build tmp/parity/seed once
bun run parity run                        # every scenario
bun run parity run --scenarios shell.startup --output /tmp/parity-run
bun run parity run --require-complete-inventory --output /tmp/parity-replacement
```

`run` needs the packaged Electron build (`packages/electron/dist/...`), the
native release build (`bun run native:build`), the staged web UI and the
bundled OpenCode binary; the defaults point at the repository's build outputs.
On Linux it also needs `Xvfb`, `xauth`, `xdpyinfo`, `xdotool`, libX11,
PyGObject with AT-SPI, `busctl`, `dbus-run-session`, `xsettingsd` and fontconfig.

## Layout

| File | Owns |
| --- | --- |
| `runner.py` | Commands, seed copies, fixture projects, isolated servers, scenario steps, comparisons and the report. |
| `steps.py` | The step vocabulary: key chords and their CDP events, accessible-role mapping, fault rules, expectation predicates and step validation. Pure; `steps_test.py` covers it. |
| `virtual_display.py` | The private display runs happen in, and `-- <command>` for probes that need one. |
| `inventory.json` | The feature inventory: stable IDs, reference files, states, scenarios and coverage kind. Requirements only. |
| `scenarios.json` | Scenario steps. Rows are addressed by key prefix and title, never by seeded IDs. |
| `step_probes.json` | The runner's own self-checks, one probe per group of step kinds, run with `--scenarios-file`. They declare no proofs. |
| `electron_adapter.py`, `electron_bridge.mjs` | Electron through CDP (`scripts/perf/cdp.mjs`): launch and restart, input dispatch, rows, geometry, screenshots, Fetch-domain faults, file-chooser answers, server reads, and read-only `observe` of layout and computed styles and `fonts` of the faces a node is drawn with, for porting. |
| `fault_rules.mjs` | The bridge's fault matching, the same rules `steps.fault_matches` applies natively. |
| `request_tracker.mjs` | Fetch/XHR ownership by frame and document loader, so a discarded document cannot keep a new one busy. |
| `native_adapter.py` | The native window through AT-SPI: launch and restart, activation, generated pointer and key input, accessibility actions, rows, geometry, content capture, and the scenario services the runner attaches. |
| `fault_proxy.py` | The native `fault` step: a loopback proxy applying scenario fault rules between the native client and its server. |
| `ime_fixture.py` | A private IBus with libpinyin for native `ime` steps. |
| `drop_source.py` | An XDND drag source window for native `drop` steps. |
| `clipboard_probe.py` | Asynchronous clipboard ownership and exact text after a real copy action on the private display. |
| `native_platform.py` | Synthetic native window-manager and IBus checks with private profiles and service logs. |
| `native_accessibility.py` | Orca-generated control announcements, AT-SPI transcript exposure, disabled menu states and toast announcements, with silent private speech services. |
| `native_workflow.py`, `workflow_proxy.py` | Connected native send/stop/restart checks and loopback-only loss injection before or after admission. |
| `native_files.py` | Connected Files checks on the Git fixture project: tree and gitignored entries, keyboard navigation, name and content search with an invalid pattern, CRLF edit and save, autosave, external changes to clean and dirty files, a deleted open file, a rename conflict and a large file, each checked on disk. |
| `native_git.py`, `git_http_remote.py` | Connected Git and Diff checks: staged and unstaged scopes, stage, unstage, confirmed discard, a stage failing on `index.lock`, commit, branches, push, fetch and pull as planned operations and a cancelled push. The fixture's bare remote is served through `git http-backend` on loopback, because the server plans network operations only for http, https and ssh remotes. |
| `native_git_picker.py` | Actual branch-search dispatch with default or configured New session bindings: arrows, Ctrl+N/P, Escape with trigger focus restoration, retained draft ownership, no mutation requests and ordinary shortcuts after the picker closes. The escape mode isolates dismissal. |
| `native_source_board.py`, `github_fixture.py` | Connected source board checks against GitHub: the board before signing in, the server's device flow, the issue list and preview, search, an empty filter, a comment, a close held while pending, an approval, and a failed list read and a failed comment with their recovery, each checked in the fixture's repository. The real server reaches the fixture through its own `HTTPS_PROXY` support and trusts its throwaway CA through `NODE_EXTRA_CA_CERTS`; the fixture tunnels only github.com and api.github.com and answers the provider's REST and GraphQL calls from an in-memory `acme/widgets`. `github_fixture_test.py` checks its search, detail and REST shapes and that it tunnels nothing but GitHub. |
| `native_knowledge_schedules.py` | Scheduled tasks and Project knowledge against an isolated server: an empty task list, a daily task saved in a fixed timezone, an invalid cron refused before any request, pause and resume, run now opening the run's session, delete; then a note written and edited, a todo added and completed, a plan written and implemented in a new session the plan links to, and search following to its matches. Every write is checked in the server's own records. |
| `native_session_management.py` | Connected rename, pin, archive, restore and delete through the native session menu, with one injected write failure. |
| `native_goals_review.py` | Session goals and the review loop on the `--workflows` fixture: arming through the target button, the goal saved before its prompt, running, the progress check, completion, Pause, recovery after a restart, Resume, a blocked goal and Remove goal; `/handoff-review` starting an automatic review that runs handoff, review, fixes and re-review to a clean end, Dismiss, and a second run stopped. Server reads confirm every state. |
| `native_selections.py` | Real-pointer model, agent and effort selection, unsent restart persistence and exact completed-turn configuration. Server settings reads verify named effort and explicit Default while retaining another model's entry. |
| `native_authentication.py`, `secret_service_fixture.py` | Password sign-in, secret accessibility, session-only and trusted restart, HTTP token revocation and retained drafts, against a protected real server and a private encrypted KDE Secret Service. |
| `native_hosts.py` | Two isolated direct servers, saved profiles, default-host startup, runtime switching and per-server draft restoration. |
| `native_host_recovery.py` | Invalid and unreadable host files, preserved bytes, explicit retry and restored composer focus on the recovered default server. |
| `native_policy.py` | Public-asset attempts observed through a rejecting loopback proxy, with enterprise mode on and off. No request reaches the public destination. |
| `native_about.py` | Footer About activation, both server version rows, closing and retained composer text. |
| `native_appearance.py` | Native Settings navigation and search, live mode/language controls, retained draft and focus, local save failure/retry and restart persistence. |
| `native_general_settings.py` | General's server-backed Files autosave and terminal controls, independent failed edits, explicit retry and captured-runtime writes. |
| `native_settings_pages.py` | Chat and Sessions pages: pointer navigation and tabs, search to the focused control, desktop-surface saves, a disabled choice, an injected failed save with explicit retry, and restored values after a relaunch. |
| `native_notifications_voice.py` | Notifications and dictation on a real server: a stub `org.freedesktop.Notifications` on the private bus records `Notify` calls and sends the click; the Test button, a saved template on a finished turn's notification, a click opening the notifying session from another one, and no notification once Settings turns them off. An ALSA `file` plugin plays a tone as the microphone and the server's OpenAI-compatible transcription points at a loopback fixture: record and Insert, a failed transcription with Retry, Cancel, and the mic gone once Voice turns voice input off. Microphone permission denial is not reproduced. |
| `native_usage.py` | The Usage page on a real server without provider credentials: one read per provider, the not-configured state, the display switch and a refresh that reads every provider again. |
| `native_reduced_motion.py` | A stub desktop portal on the private bus; native reads `org.freedesktop.appearance reduced-motion` at startup for either value. |
| `native_terminal_appearance.py` | Live theme and code-font changes on an existing terminal, with painted output, exact copied selection, process/socket identity and continued input. |
| `native_webview.py` | The Browser surface and its WebKit host: an address typed into the toolbar, page-initiated and typed navigations the policy refuses, a hostile page's attempts at message handlers, the server API and the shim scheme, the client's password variable kept out of WebKit processes, a dialog painted above the dimmed page with page input blocked, focus moving between page and address field, the browser profile's cookies kept across a restart under the client's config directory, and WebKit processes ending with the window. X11 only. |
| `native_terminal.py` | Connected terminal checks behind a proxy that records terminal commands and severs the terminal socket: create and close, Unicode and IBus input, keys, resize, bracketed and large paste, scrollback, reconnect pacing, exit, lifetime and cleanup. Capture programs in the terminal record the exact bytes the PTY received. |
| `native_attachments.py` | Native Attach-menu and clipboard-file checks, exact byte staging, failure/retry, removal and normal-restart capture. Reads unchanged attachment ownership in journal versions 7 and 8; queue metadata supplies no attachment-byte authority. Picker, drop, conversion and paired Electron parity require separate checks. |
| `portal_fixture.py` | Direct GTK backend and portal frontend on the wrapper's private bus. Atomic glibc `pidfd_spawn` retains child custody through startup failure. Requires an active `Interrupts` scope through `close()`. Explicit Documents mode adds only the installed Documents and PermissionStore helpers, private data and an attested FUSE mount. No desktop activation or other helpers are allowed. Service readiness alone does not qualify chooser interaction. |
| `compare.py` | Pixel, row and geometry comparison. |
| `png.py` | PNG reading and writing with the standard library. |

## Fixtures and isolation

The Git HTTP fixture decodes chunked transfer bodies and gzip content coding
before calling `git http-backend`. Releasing a held receive-pack request
returns an error without executing it, including after client disconnection.
Bodies are limited to 16 MiB of encoded data and chunk framing, 32 MiB of
decoded data and 16 KiB of trailers. Gzip decoding is incremental and bounded;
socket reads time out after 5 seconds without progress. Invalid bodies close
the connection without reaching the Git backend.
The Git discard scenario verifies both the working tree and the staged version
of an `MM` file. Files qualification includes Save, Discard and Cancel on a
dirty switch, including an explicitly saved empty draft.

The permission/form driver uses Ctrl+Enter for its superseding send. Plain
Enter follows the queue/steer preference while a turn waits for a response.

Authentication qualification also requires `ksecretd` and `secret-tool`. Its
private Secret Service creates an encrypted wallet with synthetic data under
the new output root and owns the first-use dialog on the test display. It uses
neither the desktop bus nor the user's wallet. The server password is fixed
synthetic fixture data; the driver never reads or logs a stored client token.
Socket fault proxies preserve the client's Host and Origin headers while
routing TCP to their loopback upstream. Replacing Host alone makes a valid
native origin fail the server's authenticated socket checks.
Host selection assertions follow the session ID while topology moves its row
from Chats to a project. A stale display-row key cannot establish a different
session's selection. Saved-host checks use a private native profile directory;
they never read or change the user's host list.
Queue qualification holds its synthetic base turn through the provider's turn
gate, then releases it for delivery checks. Assertions still observe the real
server's queue, live activity and message admission.

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
dimensions after the application's initial 2-second size hold. It observes
content within the existing readiness budget while the application owns its
startup size. Captures verify the requested physical content size both before
and after reading the window, including fractional scale and client-side frame.
A wrong-sized window cannot produce a successful checkpoint.

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
`/tmp/opencode` when available, otherwise the system temporary directory. The
bus socket inside it must stay within
the 108-byte socket path limit whatever `TMPDIR` is. Xvfb has no window manager:
OS-level minimize, maximize and window-decoration behavior requires separate
isolated desktop checks.

`OPENCHAMBER_PARITY_RUNTIME_ROOT` optionally names an existing, owned mode-0700
absolute directory for private runtime files. Its resolved pathname must be at
most 72 bytes to leave room for Unix socket names. The resolved path may contain
only ASCII letters, digits, `/`, `.`, `_` and `-`, so it is safe in the private
D-Bus address and configuration XML. A short directory under
`native/target/tmp` can use the repository filesystem. The default remains
`/tmp/opencode` when present. Xvfb still uses its normal `/tmp` lock and X11
socket, and startup fails if either cannot initialize. Authentication, private
bus isolation and process-ownership checks remain unchanged.
The interaction server still requires its fresh root directly under
`/tmp/opencode`; this option does not change server root admission.

Xvfb readiness requires a successful `xdpyinfo` probe inside the 10-second
startup budget. An individual timed-out probe can retry within that budget;
timeout alone never marks the display ready.

Cleanup binds each matching same-user private-bus process to a Linux pidfd,
then rechecks its start time and membership before signalling it. Both graceful
and forced signals use that descriptor, so PID reuse cannot redirect them to
a replacement process. The wrapper refuses startup when pidfd creation or
signalling is unavailable, including calls through `run_inside`. Cleanup closes
every bound descriptor and preserves session and Xvfb shutdown if bus cleanup
fails.

Isolated Electron launches set `GTK_IM_MODULE=gtk-im-context-simple`. GTK's
automatic IBus selection can deadlock startup even after inherited input-method
variables are removed. The native IME checks keep their own explicit private
IBus configuration.

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
`blocked` (the reference failed), and coverage against the whole inventory. A
feature passes only when every linked scenario passed and their explicit proofs
cover every required behavior and state. Reports,
screenshots and difference images go to `--output`, by default
`artifacts/parity-<time>`; they are run evidence and never part of the source.
An existing output directory is refused so another run cannot overwrite its
evidence. The report fingerprints the native and Electron executables, Electron
package, web assets, OpenCode binary, seed, scenarios, inventory and exceptions.
The validated scenario and inventory snapshots must match their initial byte
fingerprints before clients start. If any input changes before the final check,
every result is blocked and grants no proof.
File digests are standard SHA-256. The `sha256-tree-v1` directory digest hashes
each sorted relative POSIX path, a zero byte, then that file's raw SHA-256 digest.

The Electron settle check tracks each fetch or XHR by its CDP frame and loader.
A committed navigation retires only known requests from superseded documents;
requests already started for the incoming loader and requests without a known
owner stay pending. Frame removal clears that frame, while a process swap keeps
incoming-document work. This handles same-URL reloads without excluding routes
or relaxing the quiet period and deadline.

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

## Feature requirements and scenario proofs

`inventory.json` keeps stable feature IDs, reference owners, `states` and
`requiredBehaviors`. These are requirements, independent of whether an
implementation or test exists. Keep activation, resulting behavior and
persistence separate from `appearance`. For example, the header's menu needs
activation and action outcomes; its startup image proves only appearance.
Do not remove states or behaviors to make an existing scenario complete a feature.

Each scenario in `scenarios.json` has a `proofs` object keyed by feature ID:

```json
"proofs": {
  "shell.header": {"behaviors": ["appearance"], "states": ["default"]}
}
```

List only behavior and states that the scenario's actions and checked outcomes
establish. A click alone does not establish its mutation's outcome. A captured
closed menu does not establish opening it, and saved startup order does not
establish ordering after a live move or reload. One scenario can prove parts of
several features, but each feature must link that scenario in its `scenarios` list.
The scenario's `feature` remains its primary owner; it grants no implicit proof.
Contract validation rejects unknown requirements, unknown feature/scenario IDs
and unlinked proofs before launching clients.
Unknown names in `--scenarios` are rejected before a run can report success with
no executed checks.

`inventory_coverage` unions declarations only from scenarios whose final status
is `passed`. Failed, blocked, skipped and incomplete scenarios grant no proof.
It uses the validated scenario snapshot that the run executed, without rereading
declarations from disk after the final input check.
The private report retains its `byFeature` status map and adds
`missingRequirements`, listing uncovered behaviors and states for every feature.
With some passed evidence but uncovered requirements, a feature is `incomplete`.
With no executed evidence it is `not run`, including a feature with no scenarios.
A failed or blocked linked scenario keeps the feature failed even when other
scenarios cover its requirements.

`run --require-complete-inventory` adds a full-inventory exit gate. It returns
1 unless every feature has status `passed` under this protocol, with all required
behaviors and states covered by passed, validated scenario proofs. An empty
inventory fails too. `--scenarios` still selects what runs; omitted scenarios
remain unproved and the gate still checks the whole inventory. Failed, blocked,
incomplete, skipped and substituted-input scenarios grant no proof. Manual entries
remain `manual` and cannot pass this automated gate.

Without the flag, a focused run keeps its existing exit contract: 0 when its
executed scenarios pass and its inputs stay unchanged, even when inventory
coverage is incomplete; 1 for a scenario or input-integrity failure. Invalid
contracts and unknown scenario names return 2 before client execution in either
mode. Gate failures print a bounded list of feature IDs, statuses and missing
behaviors/states; `report.json` retains every missing requirement. Coverage uses
the executed declaration snapshots, so replacing files after fingerprinting
cannot add proofs or remove requirements from that report. Deterministic gate
regressions live in `runner_test.py`.

`coverage: "automated"` describes the intended evidence method. It does not claim
that scenarios exist. Use `coverage: "manual"` only for a real manual procedure,
recorded in `manualCheck`; that feature remains `manual` in this automated report.
Native-only platform and workflow receipts do not automatically complete a
matched-client feature. Their relevant outcomes need explicit evidence in the
feature's required comparisons. Manual platform results remain in private reports.

The full inventory includes shell, sessions, transcript, motion, composer, agent
workflow, files/source control, browser/terminal, settings, other panels, runtime
and restricted states. Broad entries retain the full behavior of their reference
owners and can be split into finer feature IDs as scenarios are added. Existing
IDs and requirements remain stable. The supported configuration matrix in
`docs/rust-desktop/PARITY.md` remains a separate replacement requirement; a
feature passing in one run does not qualify other platforms, locales or scales.

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
AT-SPI tree. In synthetic mode it opens a session row menu and requires that
only Rename exposes `ENABLED`. It then starts an isolated server from the parity
seed behind the `ActionFaults` proxy, fails one pin write and requires exactly
one polite AT-SPI `object:announcement` with the toast text, recorded by its own
listener in `announcements.jsonl`. After the toast closes, Orca's log must
speak its text exactly once. Audio-server
addresses point at nonexistent private sockets; the test never plays speech
through the user's desktop. Audible playback and full
screen-reader browsing remain separate checks.

```sh
python3 scripts/parity/native_accessibility.py
```

It requires Orca, Speech Dispatcher and the parity seed with the isolated
server's dependencies (`--root`, `--dist`, `--opencode-binary`, as for the
session management check). Reports and debug logs go into a new
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

## Native session management

`native_session_management.py` renames, pins, archives, undoes an archive,
restores and deletes real sessions of a parity-seed copy through the native
sidebar menu, the archive page and the archive notification's Undo, and reads
each outcome back from the isolated server's API:

```sh
python3 scripts/parity/native_session_management.py --binary <release binary>
```

The pin, its menu item and Undo are driven through accessibility actions, the
rest through pointer clicks and typed keys. The `ActionFaults` proxy fails one
unpin write with HTTP 500; the check requires the failure text in the
`session-notice` status node, an unchanged server pin and a pinned row before the
retried unpin succeeds. The last step puts a session on the fixture's
`think-30s` model, sends two prompts, deletes the session while a send is
retained and requires that no recovery row or journal attempt remains;
`--skip-pending-send` leaves it out. Output goes to a new directory under
`/tmp/opencode`.

## Native permissions and question forms

`native_interactions.py` runs independent `reject`, `once`, `always`, `question`,
`dismiss` and `supersede` checks against the real local fixture provider and
OpenCode engine:

```sh
python3 scripts/parity/native_interactions.py --scale 1.25
python3 scripts/parity/native_interactions.py --checks reject,question --timeout 20
```

Every check gets a new fixture root directly under `/tmp/opencode`. The output
directory also lives there and must be new. `--binary`, `--dist` and
`--opencode-binary` default to the parity runner's build paths. The report records
the binary fingerprints and rejects a binary change during the run.

Session creation uses the owned API. The native composer submits every prompt;
permission buttons and question controls supply every response. HTTP reads verify
ownership, exact command resources, terminal states and tool output. Only a
completed shell tool with exit zero and the exact output counts as success;
the command argument containing the marker cannot pass. The always check repeats
the command and restarts the native client against the same private server.
The supersede check submits a new composer message while a question is pending,
then verifies that the old form is cancelled and the new turn gets its own form.

Targets are `permission-{requestID}-{once|always|reject}`,
`form-{formID}-{next|back|submit|cancel}`, `form-input-{formID}-{fieldKey}` and
`form-option-{formID}-{fieldKey}-{zeroBasedOrdinal}`. The question check selects
Beta, selects One and Two, navigates back and forward, and pastes Unicode text.

The command uses the owned Xvfb wrapper, private process environments and working
directories. Signal handling covers partial startup and cleanup. Each case keeps
its fixture identity, pending request/form, screenshots, native logs and process
cleanup receipt; `report.json` records failures and input substitutions. No HTTP
permission or form reply is sent. Focused tests run with
`python3 -m unittest native_interactions_test` from `scripts/parity`.

A check passes only after native and fixture cleanup succeed. Any pointer input
replaced by an accessibility action makes an otherwise successful check
`incomplete`, with a nonzero exit. `completed.png` captures the terminal UI before
cleanup; the report carries the final outcome.

## Adding a scenario

Give the feature an inventory entry first, with the reference files and the
states it covers, then add steps to `scenarios.json` that name it. The runner
refuses a scenario whose feature is not in the inventory and an inventory entry
that lists a scenario that does not exist. Steps are `click` (a row by zone
prefix and title), `clickTarget` (a named control with an Electron selector
and a native accessible ID), `type`, `wait`, `settle` and `capture`, plus the
kinds under *Steps* below. A capture can list `targets` from the scenario
file's shared target table. Every step is validated before any client starts.

Prefer `wait` to `settle`: it names the observable state a checkpoint needs,
with one condition per client (a page expression for Electron; accessible
elements or a diagnostic marker emitted since the last input for the native
client) and a timeout. A reference that never reaches its state blocks the
scenario; a native client that never does fails it. Either failure captures
`wait-failed.png` and stops the remaining inputs, so a failed popup wait cannot
turn later typing into a composer submission.

Native waits can also match a named accessible node with
`{"named": {"name": "Search projects...", "role": "entry"}}`. Optional `id`
and `role` fields distinguish matching labels, `"prefix": true` matches the
start of the name, and `"present": false` waits for the node to disappear.
Typing starts only after the popup's search field mounts. Both clients accept
`{"focusedText": text}`: the focused editable's exact text (a field's value or
a contenteditable's text without its trailing line break).

Session-row clicks first require a settled client, then verify that the exact
requested row is selected. An unrelated selected row cannot satisfy this
postcondition. `runner_test.py` covers these input checks; run it from this
directory with `python3 -m unittest runner_test`.

Electron's server runs inside the app, so quitting Electron also ends its
OpenCode and any running turn; the native client's isolated server keeps
running across a native restart. A scenario that restarts during a turn
checks what both clients share, such as the server delivering a queued
message exactly once, and not the turn still running afterwards.
Both transcripts are virtualized: only rows near the bottom exist, so a wait
on a message's text works only for messages still in view. A held
`think-30s` turn keeps a new prompt at the bottom.

Every capture first waits up to 15 s for the client to hold still for
750 ms: Electron's text and the boxes of its panels, sections, prompt ticks,
rows and messages, the native client's accessible rows. Electron must also
have no fetch or XHR request in flight; a panel whose data has not arrived can
hold still for longer than the quiet period. Panels that fill in after the
transcript would otherwise be captured midway. A reference that never holds
still blocks the scenario; a native client that never does fails it.

## Steps

Each step names one kind. A step that cannot run on a client fails that
client (blocked for Electron, failed for native); it is never replaced by a
weaker path, except the pointer clicks already described, which are recorded
as substitutions. The services a step needs (clipboard, fault proxy, IBus,
drop source, portal) start only for scenarios that use it, so other scenarios
launch exactly as before.

| Step | Electron (CDP) | Native (owned display) |
| --- | --- | --- |
| `key: "ctrl+shift+z"` | `Input.dispatchKeyEvent`: each modifier pressed, the key, the modifiers released, with the DOM key, code and virtual key code. Named keys carry no text; characters do unless a command modifier is held. | A bare key through AT-SPI key synthesis, as before; a chord through `xdotool key` (XTest). |
| `paste: "text"` | The runner's GTK clipboard serves the exact text while Ctrl+V reads it. | The same clipboard and chord. |
| `clickNamed: {"name", "role"}` | The one unignored node with that accessible name and ARIA role in Chromium's accessibility tree, clicked at its centre once nothing covers it. | The one node with that AT-SPI name and a role the ARIA role maps to (`steps.ATSPI_ROLES`), clicked like `click`. |
| `hover: "<target>"` | A `mouseMoved` to the target's centre. | The real pointer moved there; a pointer that does not arrive fails, since hover has no accessibility equivalent. |
| `drag: {"from", "to"}` | Pressed at the `from` target's centre, moved in ten steps onto the `to` target's centre and released. | The same through XTest; drag has no accessibility equivalent. |
| `gate: "hold"` / `"release"` | The fixture provider's turn gate: while held, a delayed model's turn (`think-30s`) waits for the release instead of its delay. | The same, on the isolated server's fixture provider. |
| `restart: {}` | SIGTERM, which runs Electron's own quit teardown, then a new launch on the same profile, keeping fault rules and file-chooser interception. `{"after": "exit"}` waits for an exit an earlier step caused. | SIGTERM, or the same wait, then a new launch on the same profile, server and services. |
| `expect` | Server reads from the bridge with the page's own API base URL and token, which never leave the bridge. | Reads straight from the isolated server, never through the fault proxy. |
| `fault`, `release` | The Fetch domain pauses page requests at the request stage (`before`) or response stage (`after`). | `fault_proxy.py` between the client and its server. |
| `drop: {"target", "files"}` | `Input.dispatchDragEvent` with the files' paths: a drag into the renderer, not an X11 XDND session. | A GTK source window offers `text/uri-list`; XTest presses there and drags onto the target, so the drop arrives through XDND. |
| `pick: {"files"}` | Page file-chooser interception, answered with `DOM.setFileInputFiles` once the page opened a chooser. | The portal fixture's GTK chooser: its location entry takes the path. |
| `ime: {"compose"}` / `{"commit"}` | `Input.imeSetComposition` for each key, then `Input.insertText`. | Pinyin keys typed into a private IBus libpinyin engine over XIM; Space takes the first candidate, Return the raw keys. |

The native client has no SIGTERM handler: `restart: {}` ends it by the signal
(exit -15) while Electron quits cleanly (exit 0). A scenario about state that
survives a normal quit closes the window through its own control
(`clickTarget: "window-close"`, after which both clients exit 0) and then uses
`{"after": "exit"}`; each client's `restarts` record says how it ended.

`paste` takes `{"text", "until": {"electron", "native"}}` to name its own
observable result; by default the focused editable must contain the text.
Chromium answers a Ctrl+V only after reading the clipboard, which the
runner's own GTK loop serves, so the chord is sent without waiting and its
answer is checked while the loop runs.

`expect` reads one authoritative source and polls it until `timeout`
(15 s): `{"api": "/api/..."}` with an optional `select` path of keys and
indexes, where `"*"` maps the rest of the path over a list and keeps what it
finds (a further `"*"` adds to the same flat list); `{"file": "{fixture}/path"}` as exact UTF-8 text, or `"encoding":
"bytes"` for its SHA-256 and size, or None when absent; `{"git": [command,
...]}` running one read-only Git command (`steps.GIT_READS`) in `{fixture}`
or `cwd`, without system or global configuration and with optional locks off;
`{"fault": id, "hits": n}`; `{"gate": "held"|"waiting"}`, the turn gate's
state and the number of turns it holds. Predicates are `equals`, `contains` (substring,
list member or structural subset), `length` and `exists`, after an optional
`where` subset filter on a list. A non-2xx answer or a failed Git command
never holds, so a failed read cannot pass as an empty result. Paths take
`{fixture}`, `{project}` (the seed's project) and `{session:<title>}`, the one
session with that title; `select` keys take `{session:<title>}` too, for
records keyed by session ID. The observed value is kept in the client's `waits`.

A fault is `{"id", "method", "path", "when", "action", "status", "times"}`.
`path` is a glob where `*` is the only wildcard and matches across `/`; query
strings never match; it may differ per client (`{"electron", "native"}`) and
take `{session:<title>}`. `when: "before"` keeps the request from the server,
`"after"` lets the server answer first. `fail` answers with `status` (500 by
default) and a JSON error body; before the server answers, Electron's response
carries the request's own origin in its CORS headers so the page sees the
status, not a CORS failure. `drop` closes the connection; `hold` keeps the
request until `release`, after which later matches pass but still count. A
held request the client abandoned is not sent on. `times` (default 1) limits
the hits. WebSocket traffic, such as the terminal, cannot be faulted this way
on either client. Each checkpoint records `faultHits`.

`drop` and `pick` files are `{"name", "text"}`: each step's files are written
into a new directory of their own under the client's run directory, so both
clients receive the same names and bytes.

`pick` chooses exactly one file and cannot cancel: the native chooser's
location entry takes one path, and CDP can answer an intercepted chooser but
not dismiss it the way a user does. On Electron it covers HTML file inputs,
which the composer's Attach files uses; pickers that go through the main
process's `dialog.showOpenDialog` (directories, theme import) are not
intercepted. Native `pick` enables the portal fixture's approved Documents mode.
It starts only `/usr/lib/xdg-permission-store` and `/usr/lib/xdg-document-portal`
in addition to the GTK backend and frontend, all with private data directories
and exact pidfd custody. Documents must report the fixture's own `runtime/doc`
mount, with a fresh same-user `fuse.portal` mount identity and a FUSE descriptor
in its verified process. Cleanup records the mount identity and requires that
mount to disappear after shutdown. A lingering or replaced mount is an explicit
cleanup failure; the fixture performs no guessed or global unmount. The private
bus keeps its empty activation directory. Unexpected portal services and helper
warnings still prevent readiness. These checks require installed FUSE tooling
and access to `/dev/fuse`; they never install packages or use the desktop portal.

Without explicit Documents mode the fixture retains its two-process contract
and rejects Documents and PermissionStore owners. A startup prerequisite failure
does not establish that a chooser opened or failed during user interaction.
Portal subprocess timeouts are reported as client failures, allowing later
independent scenarios to run. Process, mount or shutdown errors still fail the
cleanup verdict, including an unverified mount that disappears during shutdown.

`ime: {"compose": "nihao", "preedit": "你好"}` leaves a composition open;
with `preedit`, Electron ends on that marked text and both clients must show
it, so a capture of the composing state compares like with like. `{"commit":
"candidate", "text": "你好"}` or `{"commit": "raw"}` commits it, and both
clients must then contain the committed text. Electron's composition comes from
CDP, not from an OS input method: its launch keeps `gtk-im-context-simple`
because GTK's IBus selection can deadlock a private launch.

A target's native half is an accessible ID, or `{"name", "role"?,
"prefix"?}` with an optional `"within"` query of the same form, for controls
whose IDs the server assigns, such as queued rows and permission cards: the
one node with that name (or name prefix), inside the one ancestor matching
`within` when it is given.
Roles there are AT-SPI role names. The Electron half of such a target uses a
positional selector, since the rows' order is part of the scenario.

Scenario options: `"provider": "interactions"` runs both clients' fixture
provider in its interaction mode (the shell and question models) with the
OpenCode configuration that asks permission for the fixture's shell command
(`isolated-server.mjs --provider-mode interactions`). `"fixture": "git"` gives each client a fresh copy of
`isolated-server.mjs --git-fixture`'s repository at one fixed path under the
seed root (`fixtures/git/fixture-repo`, with its bare remote beside it) and
makes it the active project. The path is the same for both clients, so
project IDs and row keys match; clients run serially and each starts from a
new copy. `"model": "provider/model"` sets the profile's `defaultModel`
preference in each copy, which new sessions start on. The server keeps that
preference authoritative: a value written only to `settings.json` is replaced
at startup.

Each client's result keeps its `waits` (including expectation observations,
drop outcomes and picker modes) and `restarts` (exit and relaunch records),
also when the scenario has no capture.

### Why the step services start per scenario and not per run

A proxy in front of the native server, an input method on the display or a
portal on the bus changes what the clients see. Starting them only for the
scenarios whose steps need them keeps every other scenario's launch identical
to the comparisons it was reviewed with.

### Why faults sit in the Fetch domain for Electron and not in a proxy

Electron runs its own server in process and the page reaches it at an address
chosen at startup, with a token injected into the page. Pausing the page's
own requests in CDP faults exactly the requests the reference makes, without
relaunching Electron against an external server or changing its address.

### Why `pick` intercepts the page's chooser and not `dialog.showOpenDialog`

The composer's Attach files opens an HTML file input. Chromium shows that
chooser without the main process's `dialog` module, so a stub there would
never be called; the page-level interception is the path the reference
actually takes.

`step_probes.json` exercises every step kind on both clients:

```sh
bun run parity run --scenarios-file scripts/parity/step_probes.json --output /tmp/opencode/<new>
```

With another scenario file, the inventory's links to `scenarios.json` IDs
still resolve, and a probe cannot declare a proof: no feature links a probe.
