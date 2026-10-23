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
- `send_catalog.rs` holds the composer's catalogs per configuration directory
  and resolves what a send would use through the core's `send_config`: the
  directory's default selection, refined by a selected session's record;
  `model_controls.rs` draws the effort, model and agent controls from it and
  hides their labels by the controls' width as the web's container query
  does.
- `provider_logos.rs` resolves, loads and caches provider logos for the
  composer, the sidebar rows and the turn footers.
- `context_panel.rs` also reads the selected session's context fill against
  the limit `useContextWindowLimits` picks (the record's model, the newest
  answer's, the composer's) and draws its row and meter.
- `prompt_rail.rs` draws the prompt navigator at the transcript's end edge:
  one tick per prompt, the current one longer, following the reading line
  as `scrollSpy.ts` does, in the narrow clipping gutter when the message
  column reaches under the full one. A scroll that moves the current prompt
  redraws it.
- `turn_footer.rs` draws the facts under a finished turn (model, effort,
  agent, duration) and fits them to the row as `useFactsFit` does, keeping
  the place of the message actions the web lays out for hover. The time fact
  waits for a time-zone source.
- `sidebar_item.rs` copies the core row model into owned rows. The core model
  borrows session records and entities cannot hold borrows.
- `workspace_view.rs` lays out the frameless window: the sidebar column, the
  header with the window controls, the chat, the work status panel and the
  rail. `sidebar_view.rs`, `startup_view.rs`, `transcript_view.rs`,
  `composer_view.rs`, `model_controls.rs` and `context_panel.rs` render their
  parts. They dispatch
  to `Workspace` methods and never interpret records themselves.
- `chrome.rs` owns the shared controls (sprite glyphs, quiet icon buttons),
  the drag region and window controls of the frameless window and the
  content size inside a client-side frame, and holds a new window at its
  requested content size; `logical.rs` maps start and end sides to the layout
  direction; `elevation.rs` mirrors the design system's ring and shadow
  recipes.
- `baseline.rs` places text on Chromium's baseline (`css_text`), and
  `tracked_text.rs` draws text with CSS letter spacing.
- `theme.rs`, `locale.rs` and `assets.rs` load the generated resources in
  `resources/`; `bun run native:resources` owns their content. `fonts.rs`
  resolves the web client's default font stacks against the installed fonts.
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
  Linux AT-SPI alike. The content root is `workspace`.
- The window opens on the new-session view with nothing selected. A selected
  session that leaves the list returns the window there; it never selects an
  unrelated session. The new-session draft has its own key, so its text
  survives visiting sessions.
- Layout uses logical sides: English is laid out as measured from the
  reference and right-to-left locales mirror it. Window controls keep their
  physical order and the right edge in every language, as in the web client.
- The content keeps the reference size inside a client-side frame: the
  window grows by the frame line, and sizes the web client takes from the
  viewport (`6vh`) come from `chrome::content_size`. For two seconds after
  opening, a floating window whose content is off that size is resized by
  the error on each frame; tiled, maximized and full-screen windows are left
  to the window manager.
- Text sized with `css_text` sits where Chromium puts it; text in an element
  with other children or a background gets its own `css_text` element, since
  the correction moves the whole element.
- The transcript's last row, shown or not, carries the tail spacer: the
  docked composer's resting slot plus a gap, growing only once the composer
  grows past an allowance (`composerTailInset.ts`). The slot is measured
  while painting, and a new inset remeasures that row.
- A provider logo is bundled, loading, drawn or absent. Bundled logos draw at
  once; any other is fetched once for the app's lifetime, keeps its box while
  loading and is left out when the fetch or the SVG fails, as the web drops a
  broken image. A fetch cut short by a closing connection is tried again.

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

### Why the timeline rows split their accessible node and pointer target

Zone and project headers paint across the full sidebar while their list slot
keeps the list padding, and a session row's margins belong to its slot but not
to its click target. The accessible node keeps the slot, so automation and
screen readers see the same rectangles as the web client's row elements, and
`activation::on_activate_split` registers the pointer action on the painted
target and the accessibility action on the node.

### Why fonts resolve at startup

The web client's default stacks name families by exact name and then fall
back to the platform's generic family; on Linux that is fontconfig's
`system-ui`. GPUI Kit names IBM Plex Sans, which is usually not installed.
`fc-match` takes about 15 ms, so it runs on a thread started first in `main`
and joins just before the theme is applied.

### Why text moves onto Chromium's baseline

Chromium rounds a font's ascent and descent to whole pixels before centering
them in the line box and floors the half-leading; GPUI centers the exact
metrics and rounds the glyph origin. For the same text in the same box the two
can sit a pixel apart, which the parity runner measures at every size whose
rounding differs (for Noto Sans: 12/12, 12/16, 12.5/18 and 30/36). `css_text`
computes the difference from the resolved font's metrics and moves the text
element by it, so it holds for any font.

### Why letter spacing is a custom element

GPUI's text elements have no letter spacing. `tracked_text` shapes the line
normally, keeping the font's kerning, and paints each glyph moved by the
spacing of the characters before it, as Chromium adds spacing after every
character. Text that does not fit on one line, or that has right-to-left
characters, falls back to ordinary wrapped text without spacing.

### Why logos are inverted in the SVG

The web draws provider logos with `dark:invert`. GPUI's image pipeline has no
color filter, and inverting rasterized pixels would need an image crate the
workspace does not use. Inverting the SVG's paint colors gives the same
drawing for the logos in use, which paint in hex colors, black, white or
`currentColor`; `openchamber_native_core::provider_logo` documents the limits.

### Why the window's first size is held

GPUI publishes the client-side shadow band with the first frame. If KWin
already manages the window then, it grows the window by the band; otherwise it
takes the requested size as the window with the band and shrinks the content
by 40 pixels. No single request is right in both orders, so the window
corrects itself from the measured content size until the compositor settles.

### Why `.test_support()` on rows and controls

GPUI Kit's headless test harness observes only elements marked with
`.test_support()`. The call is the identity in normal builds, so production
render chains keep their native types while `workspace_tests.rs` clicks rows
and presses keys through real hit testing.
