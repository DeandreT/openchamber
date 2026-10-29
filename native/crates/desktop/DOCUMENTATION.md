# Native desktop crate

`openchamber-native-desktop` is the GPUI Kit client. `main` initializes
diagnostics, assets, locale and theme, opens the window and hands application
lifetime to the `Workspace` entity. Without a configured server the window
runs on deterministic synthetic data from the core and makes no network or
provider calls. With `OPENCHAMBER_NATIVE_SERVER` it reads one OpenChamber
server through `openchamber-native-transport`. Existing selected sessions accept
text prompts and interruption through that server. A new-session draft creates
a root session in its selected registered project before sending its first text
prompt. Attachments and worktree creation remain separate workflows.

## Ownership

- `blocking_requests.rs` owns location-scoped permission/form stores, selected
  subtree routing, host bootstrap hints, exact reconciliation and independent
  mutation tasks. `request_dock.rs` draws one nonmodal permission-priority panel
  above the composer. `form_input.rs` owns retained typed drafts and field widgets;
  `form_copy.rs` exports the form's content through explicit clipboard actions.
- `workspace.rs` owns the application state: the global `SessionIndex`, the
  topology (projects, worktrees, Chats roots), lifecycle ordering, live
  status, sidebar presentation state, the selected session, one retained
  transcript and `ListState` per visited session with its history state,
  drafts, scroll memory, synthetic playback tasks and the server connection.
  Views read it; actions mutate it through its methods.
- `server_sync.rs` is the workspace's connected behavior: the connection
  sequence, global session, status and topology reads, history reads and live
  event delivery.
- `session_mutations.rs` owns selected-session text submission and named-session
  abort orchestration. `send_attempt.rs` retains the captured runtime, session,
  draft owner, text, selection, resolved model/variant and agent until an
  authoritative identity or definite rejection settles the attempt.
  `message_id.rs` produces the pinned OpenCode `msg_` identity layout. Its
  randomized suffix is a uniqueness aid and carries no authentication authority.
- `session_creation.rs` owns selected-project root-session creation, exact-ID
  observation and the durable handoff to a session-owned first send.
  `create_attempt.rs` retains the original draft, immutable create request and
  original prompt configuration. `session_id.rs` generates OpenCode's descending
  `ses_` identities using Unix milliseconds and the pinned 48-bit prefix layout.
- `mutation_journal.rs` owns the private versioned snapshot of drafts and pending
  sends and creations. Version 2 reads version 1 without losing its drafts or
  sends; unsupported or malformed snapshots grant no writer.
  `journal_worker.rs` holds its exclusive file lock and serializes disk I/O
  on one thread, coalescing queued snapshots while preserving save barriers.
  `state_persistence.rs` hydrates before editing, schedules draft saves, waits for
  durable preparation before a POST, and drains the last snapshot on normal close.
- `send_recovery.rs` observes retained IDs through inbox and history reads.
  `recovery_view.rs` shows pending text separately from current typing, with
  Check status and Retry same message. A retry performs a fresh observation and
  reuses the captured request identity and configuration.
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
  composer and turn footers. Sidebar rows do not request provider logos.
- `project_chooser.rs` lists and searches registered projects for a new-session
  draft, using saved project labels. Choosing a project changes the draft's
  directory and catalog scope without creating a server session. Draft text
  stays with its runtime and directory. Failed discovery remains distinct from
  an empty project list, and a failed refresh keeps previously loaded projects.
- `draft_sidebar.rs` projects local new-session drafts into project-grouped
  sidebar rows. The first nonempty line supplies the preview. Opening and
  discarding a row use its captured runtime/directory key. Preview edits update
  one row; adding or removing a draft rebuilds the structural projection. This
  in-memory projection does not publish drafts to the server.
- `context_panel.rs` also reads the selected session's context fill against
  the limit `useContextWindowLimits` picks (the record's model, the newest
  answer's, the composer's) and draws its row and meter.
  The card yields its column below the web's required chat-area width. In
  that layout its header control opens an overlay, preserving transcript
  width; widening the window returns it to its inline position.
  While the inline card is hidden, the session header shows the project and
  branch from these same retained readouts. Rendering the metadata starts no
  additional requests.
- `project_section.rs` draws the card's Project section: the project's
  label, the attention callout, the branch and changed-files rows from the
  session directory's Git status, and the project action row. It reads the
  status when the directory comes into view (again once five seconds old) and
  after a burst of finished file-changing tool calls in an open session.
  Nested repositories, pull request rows and running or choosing actions are
  not ported yet.
- `open_in_app.rs` finds the file manager's icon as Electron's Linux app
  discovery does, PNG files only, and opens the project folder with the
  platform's default handler; the control shows only for a server on this
  machine.
- `turn_stats_section.rs` draws the card's Turn stats from the core's
  `turn_stats`, recomputed while the session is idle and kept while it
  works; `context_panel.rs` holds the card's shared collapsible section and
  readout row.
- `prompt_rail.rs` draws the corner prompt indicator and the prompt-list
  panel. The indicator follows `scrollSpy.ts`'s reading line. Hover, click
  and the keyboard shortcut open the panel; arrows, Ctrl+N/P, Home and End
  move its highlight, and Enter or Space selects a prompt. The panel keeps
  its own scroll position and closes on selection, Escape, keyboard blur,
  outside click or session change. Earlier pages prepend without moving
  the transcript's anchor or the panel's highlighted message.
- `turn_footer.rs` draws the facts under a finished turn (model, effort,
  agent, duration, time) and fits them to the row as `useFactsFit` does,
  keeping the place of the message actions the web lays out for hover. The
  time is the step's completion in the local time zone (`chrono`), written
  with the locale's exported data; settings choose the clock.
  Fitted facts paint in one text row so fractional widths do not accumulate
  rounding between fragments. Narrow rows retain the element-based layout
  and model truncation. The fitted row exposes the combined text as its
  accessible label and mirrors fragment placement in right-to-left locales.
- `sidebar_item.rs` copies the core row model into owned rows. The core model
  borrows session records and entities cannot hold borrows. The default
  projection lists Works under each project. A worktree starts folded, opens
  its latest root session from the row, and expands its sessions from the
  chevron. A work listed in In work leaves its project; Pinned remains
  exclusive. Worktree expansion is shared across placements.
  The server's sidebar preferences select project or time grouping, control
  Chats/Pinned/In work visibility, and keep works in their projects when
  requested. Legacy Tree/List preferences resolve to the same grouping as
  the web client. Missing preference fields retain the previous value.
  Compact sessions use a shared leading kind/chevron slot and trailing pin
  and running-state marks. Resting rows show no recency date. The project
  separator paints inside a 16 px preceding gap; zone headers use 8 px gaps.
  The divider sits 8 px above the gap's end and mixes 10% foreground into border.
- `workspace_view.rs` lays out the frameless window: the sidebar column, the
  header with the window controls, the chat, the work status panel and the
  rail. `sidebar_view.rs`, `startup_view.rs`, `transcript_view.rs`,
  `composer_view.rs`, `model_controls.rs` and `context_panel.rs` render their
  parts. They dispatch
  to `Workspace` methods and never interpret records themselves.
- `markdown_view.rs` renders message Markdown through Kit's text view with
  the web's body color and block spacing. Headings (`markdown_view.rs`),
  lists (`markdown_list.rs`) and fenced code (`code_card.rs`) are custom
  blocks drawn to the web's `.markdown-content` rules; their inline content
  goes through nested text views with the same blocks. A selection crossing
  a custom block takes it whole. Code is highlighted with tree-sitter in the
  theme's syntax colors (`theme.rs` builds Kit's highlight theme), and the
  code wrap choice is the `codeBlockLineWrap` setting, saved through
  `server_sync.rs`. Code-card actions retain focus handles, participate in
  Tab navigation, and use GPUI's keyboard-generated clicks for Enter and
  Space. Pointer, keyboard and accessibility clicks use the same callback.
- `markdown_links.rs` owns link activation for ordinary and retained Markdown.
  HTTP(S) links open through the platform; application protocols require an
  explicit one-time confirmation bound to the current workspace, runtime and
  selected session. Session links and loaded-message targets stay in the native
  workspace. Unavailable file, fragment and message targets show an explanation
  without falling through to an external application.
  Left/middle clicks, keyboard activation and ordinary taps activate links;
  secondary clicks and touch long presses never open or navigate them.
- `transcript_selection.rs` owns one retained selection document per open
  session, with stable message/part identities and retained custom-block views.
  Text deltas update only their message; structural changes reconcile membership
  and logical order. Rendered geometry can expire without losing selected text.
  Custom children keep their identity while their source range grows. Session
  switches clear the outgoing selection before activating the incoming document.
  Each message exposes its rendered Markdown text as the accessible paragraph
  name; Markdown delimiters are not part of that name.
- `code_wrap_state.rs` orders code-wrap changes for one runtime. It permits
  one request at a time and coalesces pending toggles to their newest value.
  A settings read captures its revision and cannot overwrite later choices.
  Reconnecting to the same runtime preserves queued writes; switching
  runtimes or closing the window cancels their owned task. A failed save
  preserves the displayed choice and releases the queue without retrying.
- `chrome.rs` owns the shared controls (sprite glyphs, quiet icon buttons),
  the drag region and window controls of the frameless window and the
  content size inside a client-side frame, and holds a new window at its
  requested content size; `logical.rs` maps start and end sides to the layout
  direction; `elevation.rs` mirrors the design system's ring and shadow
  recipes.
- `elevation.rs` converts CSS blur radii to Gaussian sigma and reverses CSS
  shadow order for GPUI's painter. The sidebar's inward shadow is a separate
  overlay inside its divider, so it paints above the content in both directions.
- `baseline.rs` places text on Chromium's baseline (`css_text`), and
  `tracked_text.rs` draws text with CSS letter spacing. The new-session title
  uses one shaped line with a medium-weight run for the chosen project label,
  keeping the label in the same measured line as the surrounding text.
- `theme.rs`, `locale.rs` and `assets.rs` load the generated resources in
  `resources/`; `bun run native:resources` owns their content. `fonts.rs`
  resolves the web client's default font stacks against the installed fonts.
- `diagnostics.rs` records timing markers without message or title text:
  window creation, first content, usable composer, completed switch, sidebar
  rebuilds, synthetic playback and, when connected, the first painted session
  list and each visited session's first painted history.
  GPUI's own `log` records reach the same `tracing` subscriber.

## Invariants

- Blocking-request actions retain the runtime, directory, actual owner and ID.
  A child shown under its parent still replies to the child. Location-owned forms
  keep the `global` sentinel and their captured directory. New-session drafts
  never show the request dock. Session moves hide stale old-directory cards.
- The host status snapshot supplies optional identity hints only. Full lists are
  read for the selected session's directory and descendant directories with known
  pending requests; hints in unopened unrelated projects start no location. Global
  hints have no directory and grant no route: the selected location's full list
  recovers its own global forms. Invalid hints cannot clear
  requests. Independently decoded list siblings publish, and only complete
  whole-location lists can remove absent requests.
  Exact lookups consume host hints only after the store accepts their result.
  Invalidation leaves rejected hints available to the next scoped refresh.
- Replies are explicit and locked independently per request. Always is available
  only with server-supplied save patterns. Unknown outcomes keep the request
  locked through list refreshes. Check status performs an exact read and never
  resends a decision; only a subsequent user activation can retry. Terminal
  tombstones prevent delayed lists, events and exact reads from resurrecting cards.
- Reconnect gaps, malformed request events and location shutdown invalidate
  retained action authority. Runtime changes preserve captured in-flight work and
  in-memory drafts under their original keys. Window close drops local tasks;
  there is no permission journal or automatic approval, rejection or cancellation.
- A text-send activation captures the open requests it supersedes. After durable
  preparation it rejects permissions and dismisses forms before posting text.
  That capture precedes creation and retry observation; their asynchronous
  completions cannot add newly arrived requests to the cancellation scope.
  A failed or uncertain cancellation prevents the text POST and preserves the
  failed request and original prompt, appending it after newer owner text when
  needed. Successful siblings remain settled. Requests arriving during that chain
  block dispatch rather than being silently cancelled in the background.
  A runtime switch during cancellation retains the prompt because the original
  session index and live request authority are unavailable. Pending requests in
  that runtime remain blockers without consulting the newly selected index.
- Form drafts retain typed defaults, exact option values, custom multiselect
  answers and step position in memory. Conditional visibility removes inactive
  answers from submission. An external field is acknowledged when its active
  step is shown, and opens a URL only on explicit activation through the existing
  native link policy. Regex and format validation remain server-owned.
- The dock grows upward with a 12 px composer gap. Its measured height contributes
  to transcript-tail clearance without moving the input. Steps, choices and
  mutations use stable request-based accessibility IDs; keyboard and pointer
  actions share command handlers. Enter advances a text field; Ctrl/Cmd+Enter
  submits, with an active IME preedit suppressing both paths.
  The nonmodal dialog exposes its label, expanded state, selected step and checked
  choices. Only its body scrolls. Next and Submit share focus through the final
  step, and Other remains an edit action for the custom answer.
- Native automation can call `reply_request`, `check_request`, `set_form_step`,
  `choose_form_option`, `advance_form` and `submit_form` with
  captured request keys. These handlers enforce the same locks and ownership as
  the dock. Text widgets expose `form-input-{id}-{field}` accessibility identities
  for ordinary native editing. Form values and approval state never enter the mutation journal.
- Pointer, keyboard and accessibility activation reach the same `Workspace`
  method: every control registers its action through
  `activation::on_activate`, which installs both the pointer click and the
  AccessKit Click handler. Controls inside a Markdown custom block render
  without the workspace's context; they install both handlers themselves and
  reach the workspace through an action.
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
- Runtime changes save the outgoing composer under its captured key before
  replacing runtime state, then restore the incoming draft in that same
  transition. No input event can snapshot the previous text under the new key;
  project selection never moves text between drafts.
- Creation captures its root directory, registered project, runtime, session ID,
  first-message ID, text and configuration once. Its first POST waits for a durable
  journal barrier. Creation consumes only the submitted text synchronously; later
  identical typing remains an independent draft. Completion selects the new session
  only while the original view is unchanged and has no newer text.
- Creation accepts only the exact saved identity and normalized absolute directory.
  An unexplained canonical alias remains blocked. Unknown outcomes retain their
  immutable request across restart. Check status reads without submitting; Retry
  same message performs a fresh observation before continuing or repeating the
  identical create request. No creation or prompt retries run automatically.
  A successful Check status durably records confirmed existence before offering
  continuation, so a later 404 cannot reopen creation after restart.
- The handoff removes the creation and inserts its session-owned send in one
  journal snapshot before any selection or prompt POST. The first send compares
  returned model/agent selection and omits unchanged selections; its retained
  prompt keeps the original configuration for retries. Auto is omitted at creation
  and always restored through OpenChamber's model-switch route before prompting.
- Explicit session deletion dominates delayed creation completions. A 404 after
  confirmed existence is terminal, including after restart and during first-send
  recovery. Neither case recreates that identity. The retained recovery row keeps
  the saved text; an independent new draft can create a different identity.
- Connected mutation tasks belong to the window, independently of the selected
  runtime. A completion restores only its captured draft owner. Closing the
  window cancels local requests without stopping the server or its turn. Before
  contacting the server, a preparation barrier durably records the attempt.
- A runtime/session permits one in-flight send or abort chain. Other sessions
  proceed independently. A send clears its draft and appends one optimistic user
  message with its exact client ID. It never creates an assistant or changes
  live status. A definite failure removes only that optimistic message and
  restores its text and selection. Newer owner text remains first, followed by
  two newlines and the failed text.
- Existing-session prompts use the explicit session metadata policy from
  `chatPromptReadOnly.ts`. Children default to read-only because native does not
  read the browser-local `allowPromptingSubagentSessions` preference. A review
  requires `metadata.openchamber.kind == "review"` and a nonblank
  `originalSessionID`; titles and agent names grant no exception. Fresh sends,
  explicit retries and editor presentation share the policy. Dispatch rechecks
  current metadata after its durable preparation and preserves unsent text when
  permission was revoked. A runtime switch retains the captured runtime's policy.
- Acknowledgement establishes inbox admission. The conservative send gate stays
  held until an exact-ID user message arrives through events or history. A lost
  response retains an explicit uncertain attempt and cannot trigger a resend.
  An omitted history item cannot establish rejection or discard attempted text.
  Authoritative content replaces optimistic parts, including plugin-prepared
  content. Admission observed before an HTTP failure prevents false rollback.
- Drafts and attempts retain their runtime/directory/session identities on disk.
  Startup converts interrupted preparations and sends into uncertain attempts;
  it never submits them automatically. The prepared snapshot holds the submitted
  text in the attempt rather than duplicating it as an unsent draft. Newer typing
  remains a separate draft, even when its text equals the previous prompt.
- A failed lookup never establishes absence. NotObserved permits only an explicit
  same-ID retry; failed retries retain uncertainty about the original request.
  An exact-ID history lookup applies its authoritative message to the matching
  visited transcript before clearing recovery. A failed full-history refresh
  preserves that message. Lookup results preserve target-message changes and
  deletions newer than the lookup's captured transcript revision; changes to
  unrelated messages do not prevent applying the result.
  An authoritative user event resolves a retained send even when its transcript
  has not been opened. Send and abort ownership remain distinct.
- Missing storage differs from malformed, unreadable or locked storage. A failed
  load grants no writer and leaves the file intact. Save errors and unconfirmed
  directory synchronization pause sending and leave current memory available.
  A failed close retains the worker and exclusive lock for an explicit retry.
  Normal close releases the lock only after its final save is durable. A stopped
  worker cannot reopen over another process's writes using retained memory.
  Application-quit hooks also request a flush within GPUI's
  bounded shutdown window. Forced termination can lose the latest debounced
  draft edits, but every dispatched prompt already has a durable attempt.
- Retained Markdown uses the same parser extensions before and after painting.
  Copy includes unpainted intermediate messages exactly once, preserves selected
  whitespace and uses fixed UTF-8 endpoints across compatible appends and reflow.
  Deleted endpoints clear selection; deleted intermediate messages stop
  contributing immediately. Offscreen members have no hitboxes.
- Layout uses logical sides: English is laid out as measured from the
  reference and right-to-left locales mirror it. Window controls keep their
  physical order and the right edge in every language, as in the web client.
- Sidebar section headers use the reference's 15 px bold type and 22 px line
  inside a 30 px row. Resting session titles and container labels use opaque
  Oklab mixes of foreground and muted foreground, matching `oc-title-rest` and
  `oc-title-container`; transparency would change their color with the row fill.
- The main window selects `LayoutMode::Logical` before its first layout. Lists
  retain fractional logical measurements and translations until final painting,
  so independently rounded rows cannot accumulate display-scale drift.
- The content keeps the reference size inside a client-side frame: the
  window grows by the frame line, and sizes the web client takes from the
  viewport (`6vh`) come from `chrome::content_size`. For two seconds after
  opening, a floating window whose content is off that size is resized by
  the error on each frame; tiled, maximized and full-screen windows are left
  to the window manager.
- Text sized with `css_text` sits where Chromium puts it; text in an element
  with other children or a background gets its own `css_text` element, since
  the correction moves the whole element.
- Linux uses grayscale text antialiasing to match Chromium's desktop rendering.
  Other platforms retain their platform rendering mode until separately qualified.
- The transcript's last row, shown or not, carries the tail spacer: the
  docked composer's resting slot plus a gap, growing only once the composer
  grows past an allowance (`composerTailInset.ts`). The slot is measured
  while painting, and a new inset remeasures that row.
- The transcript paints a 48 px top fade when scrolled and a bottom fade
  covering the measured composer slot plus 48 px. These non-interactive
  overlays sit below the prompt navigator and composer. Their paint reads
  current list geometry so scroll-only frames update them without changing
  the list's layout or selection targets.
- A provider logo is bundled, loading, drawn or absent. Bundled logos draw at
  once; any other is fetched once for the app's lifetime, keeps its box while
  loading and is left out when the fetch or the SVG fails, as the web drops a
  broken image. A fetch cut short by a closing connection is tried again.

## Decisions

### Why uncertain replies need exact reads

A location list describes current pending coverage but does not identify the
outcome of a particular response attempt. The store keeps uncertainty through
list refreshes and releases it only after an exact owner/ID read. A failed exact
read keeps the card locked and leaves unrelated requests actionable.

### Why Markdown links use an application handler

Kit's default handler sends every link directly to the platform. The application
handler keeps session navigation inside its owning runtime and applies the web
client's scheme exclusions and explicit application-link confirmation. A missing
native route cannot become an external launch.

### Why an admission gate remains after acknowledgement

The client retains one attempt per runtime/session until authoritative message
content replaces its optimistic item. It blocks a new identity while delivery is
uncertain; an explicit retry uses the same saved ID. Abort can run after the send
request finishes and leaves running status to the live channel. New-session
creation is owned by `session_creation.rs`; attachments, queue controls and
selection controls are outside this module's text-only contract.

### Why creation has a durable handoff to ordinary sends

Before creation, the draft belongs to a local project and has no verified server
session. After verification, one atomic snapshot gives the captured message a
session owner. Existing send recovery can then reconcile its exact message ID
without ever returning to session creation. The original configuration remains
separate from the reduced selection requests, because OpenCode emits an agent
switch notice even when the selected agent is unchanged.

### Why disk writes have a separate worker

File creation, synchronization and rename can block. One worker owns the journal
and keeps only one in-flight and one pending snapshot. A newer full snapshot can
satisfy older barriers, but each send rechecks its identity and writer epoch
before dispatch. This preserves mutation order without making typing wait on disk.

Close waits for the save receipt before sealing the worker. Releasing its lock
after a failed save would let another process write newer data that a retry from
retained memory could overwrite.

### Why mutation completion survives a runtime switch

The captured client sends to the original endpoint, and the result belongs to
the original draft owner even after the visible composer changes. Dropping a
mutation merely because selection changes could lose both its result and the
submitted text. Each window retains the task and owner until completion.

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

### Why the interface font falls back to Arial's family

The web's interface stack ends in `sans-serif`, which Chromium maps to Arial
and fontconfig to Arial or a metric twin such as Liberation Sans. A glyph the
interface face lacks (such as `↑`) comes from that family before any
per-character fallback, so `fonts.rs` resolves it the same way, accepting only
Arial and its metric twins as Skia does, and the workspace root sets it as the
interface font's first fallback.

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
