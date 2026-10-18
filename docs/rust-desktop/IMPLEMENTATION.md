# Native Rust desktop implementation stages

Implement the [native architecture](ARCHITECTURE.md) in independently runnable
stages. Each stage adds a complete behavior and its failure paths. Add crates and
dependencies when their first behavior needs them.

## 1. Offline Windows client

Create `native/Cargo.toml`, a toolchain pin and lockfile, with core and desktop
crates. Keep Cargo artifacts separate from browser and Linux builds. Document
the native build commands in `native/README.md` when they exist and add package
scripts as the repository's command entry points. Keep existing release and
installation commands intact.

Implement:

- Typed session/message identities, ordered transcripts, draft ownership and
  the normalized sidebar projection. Port the pure projection with differential
  fixtures from the TypeScript implementation. Extend coverage for real directory
  classification before claiming full sidebar parity.
- A GPUI window with compact sidebar, selected conversation, variable-height
  Markdown rows and retained multiline composer. Submission appends a synthetic
  user message and starts a bounded deterministic response stream.
- Stable row and input entities; session switching restores draft and scroll
  state. Shutdown cancels the fixture stream and releases application tasks.
- Theme resources derived from canonical JSON and locale resources derived from
  the canonical dictionaries, with parity checks. Use the existing compact
  spacing and text sizes. Keep logical layout correct in LTR and RTL.
- Local diagnostics that distinguish window creation, content paint, usable
  composer and completed switch. Record counts and timing without message text.

Use 2,301 sessions and a 120-turn rich conversation as the representative
synthetic workload. Include short and mixed-height conversations and a
23,010-session stress case. Fixtures contain synthetic content and fixed Unicode
streams; identify their ordered content and revisions in private measurement
receipts.

Acceptance checks:

1. The core builds and tests without GPUI or network dependencies. Its projection
   matches ordered TypeScript results for empty inputs, fallback roots, owned
   children, missing/archived/temporary parents, cycles and ownership changes.
2. The release executable opens on native Windows. Session selection changes the
   correct transcript. Repeated selection preserves unsent drafts and focuses
   the intended control.
3. The last row remains reachable. Prepend, streaming and resize preserve a
   reader's anchor. Return to a session restores the exact logical-pixel offset,
   including a nonzero offset inside a tall row. The first visible frame and
   later frames show no unexplained movement.
4. Copy matches the selected Unicode text, including across messages and
   virtualized rows. IME, multiline paste, undo and submit/newline shortcuts work
   in a real window. A stream does not steal focus or destroy composition.
5. Keyboard and Windows accessibility actions reach the same commands. Verify
   English and a mirrored RTL layout, light/dark themes and Windows DPI changes.
6. Complete fixture playback matches the expected text and ordering. Close
   during playback leaves no owned task or process alive.

A failed native scroll, selection, IME or accessibility check is an implementation
finding. Resolve it or keep that capability explicitly unqualified before
expanding the prototype into a daily-use client.

## 2. Read-only server connection

Resolve and document the Rust/OpenCode SDK boundary described in the architecture
before adding official engine calls. Use the repository-pinned official client
and schema as the compatibility source. Add the transport crate with Serde,
Tokio, Reqwest and incremental event decoding.

Connect only to an explicitly configured isolated OpenChamber server during
development. Implement service identity, authentication, capabilities, global
session paging, selected history and live event delivery. Preserve the server's
path prefix and session metadata overlays. Keep partial, empty, malformed and
failed results distinct.

Test request fidelity against the official client, including directory encoding,
query/body fields, authentication and cancellation. Test fragmented UTF-8/SSE,
partial pages, duplicate and unrelated events, unknown event types, stream gaps,
out-of-order snapshot completion, runtime switches and authentication failure.
No read can bootstrap every known directory as a side effect.

Observe an external session move and an idle event without sending a prompt.
The session stays visible, routes to the authoritative destination and remains
correct after reconnect and reload. Include failed topology discovery.

## 3. Interactive agent workflow

Add prompt submission, abort, permission and form responses through typed
operations. Capture send configuration at dispatch, preserve attachments and
reconcile ambiguous results before retry. Keep a failed send's recovery scoped
to its original draft. State-changing commands remain unavailable until their
error and reconciliation paths work.

Add session creation, rename, archive, move, fork and revert in bounded changes.
Carry over current read-only session behavior, queued prompts, staged-revert
semantics and partial-move cleanup. Use server-owned operations for worktrees and
Git. Test a complete create, stream, permission, abort, revisit and move workflow
against the isolated server.

## 4. Files, terminal and embedded browser

Add file navigation, code/diff views and the existing server's terminal transport.
Validate keyboard input, Unicode, resize, scrollback and bracketed paste against
the current emulator contract.

Build a separate WebView composition test before integrating browser-dependent
features. Prove menu/dialog overlays, focus, resize/clipping, browser control,
authenticated assets and profile isolation on Windows. Then implement executable
HTML answers, previews and SDK panels with their existing capability boundaries.
Test blocked navigation and attempted access to credentials or native functions.

Browser and terminal support qualify independently. A working Markdown renderer
does not complete either stage. Record platform gaps explicitly before taking on
macOS or Linux release support.

## 5. Native host and release

Complete the feature inventory against the supported Electron workflow, including
settings, source control, project knowledge, schedules, goals, shortcuts, menus,
notifications, remote hosts and runtime switching. Each feature retains its
server or platform owner until a qualified Rust implementation replaces it.

Define managed server lifetime, process ownership and shutdown before bundling
local services. Port OpenChamber server modules in complete contract-sized units
when ownership moves to Rust. An externally connected server remains supported.
Preserve enterprise policy, relay encryption, provider ownership and host trust
checks in every applicable path.

Add package identity, signed distribution, updater behavior, deep links and any
required profile migration. A real install/update/rollback and shutdown exercise
is required before replacing an installed application. Qualify each supported
platform separately. Use existing organization-managed release infrastructure;
this migration does not require a new pipeline.

## Performance contract

Measure release builds, serially, with the same ordered workload. Separate the
offline renderer experiment from the connected full application comparison. An
offline UI excludes backend cost and cannot establish a full-app startup or
memory gain.

| Dimension | End condition or metric | Acceptance rule |
| --- | --- | --- |
| Cold/warm startup | Process launch to visible transcript and usable composer | Report each cohort; a shell window alone does not end the wait |
| Session switching | Selection input to correct revealed transcript | Median improvement of at least 20% in both opposing orders, with no p95 regression, before claiming a switching benefit |
| Short-session parity | Same input-to-visible boundary | Absolute median and p95 regression at most one 16.7 ms frame |
| Typing and streaming | Input/delta to painted result; missed frame budget | Target p95 at or below 16.7 ms at 60 Hz; unchanged output, focus and text selection |
| Scroll stability | Message anchor and offset from reveal onward | Preserve the intended anchor within physical-pixel rounding; investigate every larger shift |
| Memory | Whole-process private bytes and working set after the same navigation sequence | Compare equivalent process ownership and retained content; report CPU/GPU accounting limits |
| CPU | Headed process CPU and presented frames during the same idle/active intervals | Include the renderer, GPU and owned host processes; prove frame liveness |

These targets define qualification. Record baseline spread
before comparing candidates. Use A/B/B/A and B/A/A/B ordering, preserve failed
attempts and report median, p95 and maximum with sample counts. Budget compliance
and correctness are separate requirements.

Existing profiling commands remain the baseline tools. Extend their measurement
contracts for native windows with Python orchestration and native diagnostics.
Do not compare browser JS heap with Rust process memory or Rust core timing with
an operation that includes transport and rendering. Freshly rerun the baseline
for each accepted improvement.

## Repository checks and evidence

For source commits, run the repository's `check:changed` gate on the final diff
and the applicable native formatting, Clippy, tests and release build. Add native
commands to `package.json` when the workspace lands. Generated resources need a
drift check; dependency changes need affected builds and workspace checks.
Keep default builds practical by compiling only the dependencies the stage uses.

Keep durable architecture, crate ownership and repeatable build procedures in
the repository. Retain local screenshots, measurement samples, process receipts
and failed-attempt logs outside deliverable source. Record checks that could not
run explicitly. Compilation alone does not qualify native rendering, networking,
accessibility, performance or installation.
