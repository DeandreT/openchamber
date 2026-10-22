# Native desktop crate

`openchamber-native-desktop` is the GPUI Kit client. `main` initializes
diagnostics, assets, locale and theme, opens the window and hands application
lifetime to the `Workspace` entity. Without a configured server the window
runs on deterministic synthetic data from the core and makes no network or
provider calls. With `OPENCHAMBER_NATIVE_SERVER` it reads one OpenChamber
server through `openchamber-native-transport`; the connected client is
read-only until prompt submission has its error and reconciliation paths.

## Ownership

- `workspace.rs` owns the application state: the global `SessionIndex`, the
  topology (projects, worktrees, Chats roots), lifecycle ordering, live
  status, sidebar presentation state, the selected session, one retained
  transcript and `ListState` per visited session with its history state,
  drafts, scroll memory, synthetic playback tasks and the server connection.
  Views read it; actions mutate it through its methods.
- `server_sync.rs` is the workspace's connected behavior: the connection
  sequence, global session, status and topology reads, history reads and live
  event delivery.
- `connection.rs` holds the configured server, the connection state the views
  explain and the tasks of the current connection attempt.
- `network.rs` owns the Tokio runtime. GPUI tasks await `NetworkTask`s, and
  dropping one aborts its network work.
- `sidebar_item.rs` copies the core row model into owned rows. The core model
  borrows session records and entities cannot hold borrows.
- `sidebar_view.rs`, `transcript_view.rs`, `composer_view.rs` and
  `workspace_view.rs` render. They dispatch to `Workspace` methods and never
  interpret records themselves.
- `theme.rs`, `locale.rs` and `assets.rs` load the generated resources in
  `resources/`; `bun run native:resources` owns their content.
- `diagnostics.rs` records timing markers without message or title text:
  window creation, first content, usable composer, completed switch, sidebar
  rebuilds, synthetic playback and, when connected, the first painted session
  list and each visited session's first painted history.
  GPUI's own `log` records reach the same `tracing` subscriber.

## Invariants

- Pointer, keyboard and accessibility activation reach the same `Workspace`
  method: every control registers its action through
  `activation::on_activate`, which installs both the pointer click and the
  AccessKit Click handler.
- Timing markers end when the frame that shows the result paints. A zero-size
  canvas at the end of the workspace tree emits them, so they measure the
  client's own work and do not wait for compositor presentation.
- Switching sessions saves the outgoing draft and scroll position under the
  identity captured when the outgoing session was selected, then restores the
  incoming ones before the next paint.
- A text delta remeasures only its own transcript row. The transcript follows
  the tail only while the reader is at the end.
- Dropping the `Workspace` drops every playback `Task`, which cancels its
  stream; closing the window during playback leaves no owned task running.
  Dropping it also drops the connection: requests and the subscription end,
  and the network runtime shuts down without blocking the UI thread. The
  server and any running turn are unaffected.
- Every connection attempt has a generation. A result checks the generation
  that requested it and is dropped when a newer attempt replaced it. A
  different server identity at the same endpoint is a different runtime:
  sessions, topology and transcripts are replaced, while drafts and scroll
  positions keep their original runtime keys.
- The global reads start once the event stream is live, so events during a
  read reconcile against its baseline instead of falling between the two.
  When the stream reconnects without replaying from the last delivered event,
  the client rereads sessions, running state, topology and every visited
  transcript. A failed read keeps what the client showed.
- Visited transcripts merge history reads with `Transcript::materialize`. A
  live event that refers to a message or part the transcript lacks schedules
  a read of that session unless one is in flight.
- Each part of topology discovery (Chats roots, projects, each project's
  worktrees) fails alone and keeps what it held, so a failed discovery leaves
  sessions reachable through their fallback rows.
- Every visible string comes from `resources/locales.json`, every color from
  `resources/themes.json` and every icon from `resources/icons.json`.
- Interactive elements carry a stable `accessibility_id` equal to their row key
  or control name, so automation finds them by ID on Windows UI Automation and
  Linux AT-SPI alike.

## Decisions

### Why one `ListState` per visited session

GPUI Kit 0.7.3's `MessageScroller` keeps its `ListState` private, so a pixel
offset inside a row cannot be saved or restored through it. The transcript uses
GPUI's `list` with a retained `ListState` per session: returning to a session
reuses the state that still holds its anchor message and offset, and
`ScrollMemory` records the same anchor by message ID for sessions whose state
is rebuilt.

### Why owned row snapshots

The core sidebar projection borrows records, which keeps a 23,010-session
projection to index allocations. GPUI entities are `'static`, so each rebuild
copies the fields a row paints into `SidebarItem`. Rebuilds happen on
structural changes, never on text deltas.

### Why the sprite icons and not Kit's Lucide set

Parity requires the Electron client's glyphs. The resource generator exports
the web UI's Remix sprite paths as standalone SVG documents, and
`AppAssets` serves them next to GPUI Kit's own assets.

### Why an explicit accessibility Click handler

Without one, GPUI answers an AccessKit Click by synthesizing a mouse click at
the node's centre, and `on_click` drops it unless the last real pointer
position hovered the element. Screen readers and automation would then
activate nothing. `on_activate` registers the workspace action for both paths.

### Why markers end at paint and not at the next frame

`Window::on_next_frame` runs only when another frame is drawn. A covered or
idle window draws none, so a marker scheduled there could wait indefinitely
and inflate a measurement. Painting the frame is the moment the requested
transcript exists; presentation timing belongs to the platform.

### Why a Tokio runtime beside GPUI's executor

Reqwest and tokio-tungstenite need Tokio. Network work runs on two Tokio
workers; GPUI tasks await the results without blocking either executor. Tokio
channels and join handles complete from any executor, so the bridge is a join
handle that aborts on drop and nothing more.

### Why the transcript skips most notice roles

`packages/ui/src/components/chat/lib/timelineRoles.ts` hides synthetic,
system, skill, idle and switch records and shows compaction and shell records
as notices. The native transcript keeps every record for identity and order
and renders the same subset.

### Why `.test_support()` on rows and controls

GPUI Kit's headless test harness observes only elements marked with
`.test_support()`. The call is the identity in normal builds, so production
render chains keep their native types while `workspace_tests.rs` clicks rows
and presses keys through real hit testing.
