# Native desktop crate

`openchamber-native-desktop` is the GPUI Kit client. `main` initializes
diagnostics, assets, locale and theme, opens the window and hands application
lifetime to the `Workspace` entity. Without a configured server the window
runs on deterministic synthetic data from the core and makes no network or
provider calls. With `OPENCHAMBER_NATIVE_SERVER` it reads one OpenChamber
server through `openchamber-native-transport`. Existing selected sessions accept
prompts, steering while busy and interruption through that server. A new-session draft creates
a root session in its selected registered project before sending its first text
prompt. Attachments and worktree creation remain separate workflows.

## Ownership

- `managed_host.rs` owns the managed local server: discovery of the Electron
  package layout it runs from, the server's environment and bootstrap, the
  loopback port store and the handle `main` stops after the window closes.
  `managed_host_supervisor.rs` owns the server process on its own thread:
  spawn, readiness, crash restarts and the bounded stop.
  `managed_host_lifecycle.rs` is the pure state machine between them, and
  `managed_host_view.rs` draws its states. `owned_process.rs` owns process
  identity (PID plus start time), descendant snapshots and signals.
- `blocking_requests.rs` owns location-scoped permission/form stores, selected
  subtree routing, host bootstrap hints, exact reconciliation and independent
  mutation tasks. `request_dock.rs` draws one nonmodal permission-priority panel
  above the composer. `form_input.rs` owns retained typed drafts and field widgets;
  `form_copy.rs` exports the form's content through explicit clipboard actions.
- `session_goal.rs` owns the composer's goal arm, the goal writes (arm on
  send, Pause, Resume, the dialog's Start, Save and Remove goal) and the reads
  the goal controls need; `session_goal_view.rs` draws the target button, the
  objective counter, the goal row and the dialog. The goal itself is the
  server's: `metadata.openchamber.goal` on the session record, parsed by the
  transport's projection and replaced by every metadata patch.
- `review_flow.rs` owns the review dialog's state, both review starts and the
  projection of the server's auto-review runs; `review_view.rs` draws the
  loop's row and the dialog. The dialog shows the composer's model, variant
  and agent as the reviewer and has no pickers of its own.
- `permission_modes.rs` owns the composer's permission shield: the server's
  policy projection, a new-session draft's own choice, mode writes and the
  presentation holds around `BlockingRequests`. The server's
  `permission-auto-accept` runtime is the only responder.
- `workspace.rs` owns the application state: the global `SessionIndex`, the
  topology (projects, worktrees, Chats roots), lifecycle ordering, live
  status, sidebar presentation state, the selected session, one retained
  transcript and `ListState` per visited session with its history state,
  drafts, scroll memory, synthetic playback tasks and the server connection.
  Views read it; actions mutate it through its methods.
- `server_sync.rs` is the workspace's connected behavior: the connection
  sequence, global session, status and topology reads, history reads and live
  event delivery.
- `session_mutations.rs` owns selected-session prompt submission, steering and
  named-session abort orchestration. A composer send whose preparation would
  return it unchanged (text only, with no files, context, references, `@`
  mentions, `$skill` or `#snippet` tokens) becomes a send attempt with its
  optimistic message. Every other send goes to the journaled rich actor in
  `queue_local_send.rs`, which has no narrower fallback. `send_attempt.rs` retains, per attempt,
  the captured runtime, session, draft owner, prompt with its parts and context,
  selection, resolved model/variant, agent and delivery until an authoritative
  identity or definite rejection settles it. A session can hold several.
  `message_id.rs` produces the pinned OpenCode `msg_` identity layout. Its
  randomized suffix is a uniqueness aid and carries no authentication authority.
- `session_creation.rs` owns selected-project root-session creation, exact-ID
  observation and the durable handoff to a session-owned first send.
  `create_attempt.rs` retains the original draft, immutable create request and
  original prompt configuration. `session_id.rs` generates OpenCode's descending
  `ses_` identities using Unix milliseconds and the pinned 48-bit prefix layout.
- `mutation_journal.rs` owns the private versioned snapshot of drafts and pending
  sends and creations. Version 8 adds compact queue operation ownership and
  separately held spool chunks. Version 7 retains full draft content, immutable attachment
  descriptors, preparation and retirement intents, staging targets and starter
  captures. `draft_storage.rs` validates semantic content; `attachment_catalog.rs`
  validates byte ownership and captured targets without performing I/O.
  Version 6 adds the exact queue-reservation intent and
  captured composer text. Version 5 retains each send's request-supersession
  policy and server-reservation ownership. Versions 1 through 4 default
  supersession to true and reservation ownership to false. Version 4 stores whole prompts: text, attachment
  references, agent and skill references, metadata, the ordered context items
  with their IDs, the prompt ID and delivery. Versions 1 to 3 load as prompts
  without parts. Unsupported or malformed snapshots grant no writer.
  `journal_worker.rs` holds its exclusive file lock and serializes disk I/O
  on one thread, coalescing queued snapshots while preserving save barriers.
  `attachment_io.rs` uses the same worker and lock for the private SHA-256 spool.
  Only a validated load or durable save authorizes byte operations. Pending or
  uncertain snapshots grant no write or deletion authority. Read receipts retain
  their buffer reservation until consumed or dropped.
  `queue_capture.rs` binds local chunk labels, lengths and digests;
  `queue_storage.rs` owns enqueue, take, prefix and local-send transitions, native source
  roots and explicit retirement intents. Expanded payloads remain outside the
  journal once a verified companion owns them.
  `queue_payload_io.rs` implements committed chunk puts, reads and whole-stream
  verification through that worker. `queue_enqueue.rs` owns explicit native
  composer enqueue, its stable upload labels and request body, exact receipt
  checks and journaled server-delivery retry authorizations.
  `queue_take.rs` materializes complete sealed transfers and verifies separate
  attachment custody before acknowledgement. `draft_companion.rs` retains ordered
  original queue sources outside the native draft's narrower content model.
    `composer_document.rs` backs the existing textarea with source-aware edits,
    compact verified editing snapshots and exact source-local reference removals.
    Loaded source metadata supplies the existing attachment strip's companion
    file and reference chips. Undo layouts share this window-local projection;
    persisted edit bytes omit it and rebuild it from verified original sources.
   Its local read recovery never performs another server Take or acknowledgement.
   `composer_preparation.rs` plans explicit mentions under the captured owner and
   maps anchors through trimming and snippet expansion. `composer_delivery.rs`
   projects the exact source/literal document and coordinates operation-owned
   preparation. `composer_file_preparation.rs` reads typed file requests through
   the captured server, normalizes on the background executor and publishes exact
   complete bundles. `composer_payload.rs` persists and seals one combined
   prepared composer for enqueue and prefix use.
  `state_persistence.rs` hydrates before editing, schedules draft saves, waits for
  durable preparation before a POST, and drains the last snapshot on normal close.
  `attachment_retirement.rs` owns named native-root release requests and their
  handoff to durable local retirement intents after actor ownership ends.
- `composer_autocomplete.rs` owns the composer's pickers: `/` commands, `$`
  skills, `#` snippets and `@` agents, files, folders and sessions. A committed
  edit (never IME preedit, never a restored draft) resolves the trigger
  through the core's `autocomplete`; Up, Down, Ctrl+N/P, Enter, Tab and Escape
  go to an open picker before the editor. Catalogs are read per runtime and
  config directory, again when a picker opens more than five seconds after the
  last read; the `@` search waits 180 ms and keeps the last rows on screen
  until its answer. `composer_autocomplete_view.rs` draws the list above the
  composer's start edge with its keyboard hint; the selected row is the
  editor's active descendant. Snippet creation from the `#` list (the web's
  quick-create dialog) and the emoji picker are not ported.
- `composer_references.rs` resolves the session and message links in the
  composer's text before a send: sessions from the session index, messages by
  a whole-message read. A send waits for that read; each reference found joins
  the draft as a context item in link order, and a link it cannot resolve
  stays text with a notification.
- `message_queue.rs` owns the composer's side of the server-owned message
  queue: the projection reads (the host's on every event-stream connection,
  a space's once per connection when one of its sessions is selected),
  broadcasts, and the queue, edit, send, send-now, remove and reorder
  actions, plus Enter routing by the `followUpBehavior` setting.
  `queue_strip.rs` draws the queued strip above the editor. `starters.rs`
  owns the new-session starter chips: which resolve, activation through a
  built-in's magic prompts, and the global (settings), personal and shared
  (project setup) lists; `startup_view.rs` draws them and the add picker.
  `starter_capture.rs` retains each activation's full source draft, configuration,
  permission choice and converted parts independently of newer composer input.
- `attachment_files.rs`, `document_attachments.rs` and `attachment_images.rs`
  own format normalization, bounded Office/OpenDocument extraction and HEIC
  conversion. `attachment_intake.rs` admits complete regular-file or memory input
  before normalization. It performs no draft, spool or network mutation.
  Intake never publishes the picked or dropped file's original path. Electron
  sends that path only to its own local instance, and native connects only to
  externally owned hosts; a loopback URL can be a forward to another machine, so
  it is not proof of local ownership. The server receives the safe filename.
  `heic_worker.rs` runs HEIC decoding in the same executable's early worker branch
  under Linux address-space and elapsed-time limits, before desktop initialization.
- `send_recovery.rs` observes every step of one retained attempt (its context
  items and its prompt) through inbox and history reads.
  `recovery_view.rs` shows pending text separately from current typing, with
  Check status and Retry same message. A retry performs a fresh observation and
  reuses the captured request identity and configuration.
- `session_menu.rs` draws a session row's menu (the More button and a
  secondary click open the same items), its Pin to, Move to worktree and
  Move to folder submenus, the inline rename field and the archive/delete
  confirmation. `session_management.rs` decides what the menu offers and
  runs rename, Copy ID, pin, work state, archive, restore and delete against
  the captured runtime, reporting outcomes as notifications, with Undo after
  an archive. `session_ai_rename.rs` names a session on the small model,
  `session_warming.rs` mirrors the server's per-session warming,
  `session_export.rs` writes Export Markdown through the save dialog, and
  `worktree_actions.rs` moves a session tree to a worktree, a new worktree
  or (from `move_session_dialog.rs`) another project. `session_folders.rs`
  keeps the server's shared folders with this window's unsent changes over
  them and draws a folder's inline name field. `project_registration.rs`
  adds, renames and closes projects and draws the project header menu and
  the Add project dialog. Open run overview, Open in side panel and Open
  Mini Chat window are listed and only log that their feature is not
  ported; pin lists (custom sidebar sections), guest actions and the shared
  setup trust prompt are not ported either.
  `archive_view.rs` is the archive page the toolbar's Archive button opens in
  place of the chat; it lists archived sessions with Restore and Delete.
  The archive page's project buckets, search, dates and archived worktrees
  are not ported.
- `source_board.rs` is the issues and pull requests board the sidebar's
  Issues and PRs button opens in place of the chat (the work status card
  hides with the chat); it and the archive page close each other, and
  selecting a session or a new draft closes it. One entity per runtime keeps
  the project, list, preview and a half-typed reply while closed. It picks
  the account through the binding and the hosts' current accounts, lists
  issues or change requests with search and the state and people filter,
  previews the highlighted item with its comments, and comments, closes or
  reopens, approves and requests changes. `source_board_view.rs` draws it.
  Trackers, discussions, a folder's several repositories, links other
  surfaces ask to show, label and reviewer editing, merge and ready for
  review, review threads and drafts, check runs, stacks, comment
  attachments, starting a session or a worktree (checkout) on an item, the
  narrow layout and the list's resize handle are not ported, nor the pull
  request rows of the Git surface and the work status card.
- `sidebar_search.rs` owns the toolbar's session search: the field under the
  toolbar, the match count and the filter that narrows the grouped sections
  and Chats before the row model runs (`core::session_search`). While a query
  is active the row model opens every container, applies no reveal limit and
  leaves drafts out. `display_mode_view.rs` draws the Display mode menu: the
  Sessions grouping submenu, saved as `sidebarGrouping`, and Collapse all and
  Expand all. Project sorting and display, detailed rows, branch names for
  worktrees, the sections manager, Select sessions and Mark all read are not
  ported. Drag reordering of projects, folders and sessions is not ported.
- `legacy_queue_reservation.rs` preserves exact text-only reservation recovery
  for retained journal attempts. It compares the original composer, ordered item
  identities and complete prompt configuration before allowing dispatch.
- `session_branching.rs` runs fork (a prompt's Fork, `/fork`), staged revert
  and Restore, `/btw` (create, follow-ups, collapse, promote, destroy) and
  subagent navigation (open a child, Return to parent, stop) against the
  captured runtime. `branching_view.rs` draws a prompt's hover actions, the
  reverted strip at the top of the composer, the `/btw` panel above it, a
  subagent call's and report's Open and Stop, and Return to parent. The
  goal-objective file copy of a fork, attachments and context chips on a
  restored prompt, and a fork from an assistant turn's footer are not ported.
- `connection.rs` holds the configured server, the connection state the views
  explain and the tasks of the current connection attempt. A dropped event
  stream shows nothing, as in the reference; the last valid state stays.
- `compatibility_view.rs` is `OpenCodeCompatibilityGate.tsx`: each ready
  connection checks the host's OpenCode, and only a confirmed incompatible
  answer replaces the window with the gate (Install or Update when the host
  can install, Check again, the guide link and Switch instance). A failed or
  unknown check keeps what was known. The gate lives on its connection, so a
  host switch drops it; a recovered host reconnects from the start.
- `about.rs` owns the footer's About dialog and its independent version reads.
  Reads use the captured connection generation; closing the dialog cancels its
  work. Unavailable OpenCode versions remain unknown, and an unavailable server
  version falls back to the native package version. Public project links use
  the existing link-opening boundary. The logo uses the canonical brand SVG.
- `authentication.rs` owns password entry, the sign-in gate, explicit trust
  choice and token recovery. `authentication_view.rs` draws the masked form.
  `credential_store.rs` serializes OS credential operations through `keyring`;
  saved entries use the canonical endpoint and verified server identity together.
- `host_profiles.rs` owns versioned, non-secret `hosts.json` profiles and
  conflict-checked atomic writes. `host_manager.rs` owns the direct-host picker,
  profile edits and endpoint changes. Credentials never enter the profile file.
  Direct HTTP(S) profiles are supported here; SSH, relay, request-header and
  pairing profiles remain separate native work. Add instance opens the native
  Remote instances Settings page through `settings_view.rs`, reusing the same
  profile model and write barrier. Window-opening actions are separate work.
- `shortcuts.rs` binds the configurable shortcuts that have a native command
  into GPUI's keymap, runs them and saves the user's map to the settings
  document. `shortcuts_settings.rs` is the Shortcuts page and its recording
  dialog. `app_menu.rs` is Electron's Windows and Linux application menu
  behind the title row's menu button, labelled from Electron's own menu
  dictionaries (`resources/menu-locales.json`).
- `pane_resize.rs` owns the sidebar's and the context panel's widths, their
  drag handles and the device-local `layout.json` (sidebar open, sidebar
  width, panel width) that restores them. The Files, Git, Diff and terminal
  surfaces share the panel width, as the reference's right zone does.
- `settings_view.rs` owns the retained Settings dialog, selected page and search
  field. `settings_navigation.rs` lists only implemented pages, in the
  reference's `pageOrder`, and indexes only implemented targets. General is
  the first page; reopening keeps the selected page and clears the search.
  Each page owns its scroll container and search anchors.
- `appearance.rs` owns process-local live theme, language, installed-font and
  interface-size preferences. `appearance_storage.rs` owns the validated v1
  `appearance.json` snapshot, atomic writes and compare-and-swap authority.
  `appearance_view.rs` retains the controls; `settings_picker.rs` confirms only
  a selection gesture. A picker dismissal leaves its value unchanged.
- `general_settings.rs` owns the server-backed Settings preferences: General's
  autosave, terminal shell and close-on-exit, and the Chat and Sessions fields
  (prompt navigator, follow-up behavior, starters, permission default, Chats
  section, In work, deletion dialog). Reads and serialized writes retain their
  runtime and client; the executor outlives a host switch. The workspace
  consumes live choices, including edits whose saves failed, and a newer
  topology read keeps unconfirmed choices over the server's values.
  `preference_pages.rs` draws the Chat (with its three tabs) and Sessions
  pages from that model and the shared save indicator; their remaining
  reference controls belong to features native does not have yet. Code line
  wrap on the Chat page goes through the workspace's own writer, shared with
  code cards, which reports its failure to the indicator.
- `effort_preferences.rs` owns model-scoped effort memory in the server profile.
  Explicit effort choices update effective memory immediately. Serialized writes
  retain their original runtime and preserve other models. Incoming settings
  overlay newer local choices before publication. Successful ordinary user sends
  carry memory-only submit captures; recovered and background sends cannot create
  these captures. Normal close drains writes and keeps failed edits open for
  explicit picker retry. App quit retains captured executors through its drain.
- `host_recovery.rs` owns failed saved-host startup. It shows no session data
  and leaves the file untouched. Refresh rereads the same file off the UI
  thread, then connects its default or opens the instance picker. Entering a
  recovered connection focuses the composer when authentication completes.
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
- `enterprise.rs` reads the fixed machine-policy locations, validates the
  existing policy-file schema and watches parent directories for replacement
  or removal. A failed read or watcher keeps native public-asset requests
  blocked. The request worker rereads the policy before starting a download;
  the watched snapshot controls presentation and invalidates remote logo cache
  entries when policy changes. Bundled logos require no network access.
  Watch registration is renewed after directory moves/removal or a rescan
  notice, including replacement at the same pathname. Policy reads never log
  document content or pinned credentials.
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
- `files_view.rs` owns the context panel's server surfaces: the rail's Files,
  Git and Diff buttons open one in place of the work status card, 640 px wide
  while the chat keeps 320 px, for the selected session's directory (else the
  draft's project, else the active project). Each surface is its own view
  entity, retained per runtime and directory. The same file holds the Files
  view: header with name and content search, the tree column
  (`file_tree_view.rs`: virtualized tree items with level, position and
  expanded state, arrow keys, Enter, Home, End and F2 rename) and Kit's code
  editor. `diff_view.rs` shows patches in Kit's read-only diff: one path's pane
  for the Git view and the Diff surface with every changed file of the
  Changed or Staged scope. `git_view.rs` is the Git surface: branch menu with
  create, rename, checkout and delete, fetch, pull and push as planned
  operations with cancel, staged and unstaged changes with stage, unstage and
  confirmed discard, the commit field and the selected change's diff.
  Persisted tree state, read-only files outside the workspace from links,
  hunk staging, identities, stash, merge and rebase, nested repository
  listings, the walkthrough and project action run and stop are not ported.
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
  beside the message actions (`message_actions.rs`). The
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
- `transcript_view.rs` owns the transcript list and each row's frame and
  dispatches a message's parts to one module per kind: `text_part_view.rs`
  (the prompt bubble and assistant Markdown), `reasoning_view.rs`,
  `tool_part_view.rs` and `attachment_part_view.rs` (file and agent-mention
  parts). Compaction records draw as a reasoning-style block whose body is
  the summary; `!command` records draw as the shell tool row.
- `tool_part_view.rs` draws tool rows as `ToolPart.tsx` does: icon (a chevron
  under the pointer), display name, description or justification, failure
  reason while folded, diff and line counts, a live or final duration for
  shell and subagent rows, and the background label of a shell call
  OpenCode moved to the background. Navigation tools (`read`, `skill`) stay
  one static line. Opened, a row shows the call's input, result (subagent
  Markdown, file diffs as plain colored lines, text output with Copy),
  truncation note and error beside the block line. Opening a row whose result
  a history page left out reads the whole message; the row says so after
  500 ms and offers Retry after a failure. Background shells' live output and
  Stop, file open actions, question cards, web search results, LSP
  diagnostics, guest tool presentations and pages drawn in place are not
  ported.
- `reasoning_view.rs` draws reasoning blocks (`ReasoningTimelineBlock`):
  folded to a line with the title, busy dots while streaming and a one-line
  preview once settled; opened to the text in a 320 px scroll box.
  `expandReasoningWhileStreaming` keeps its web default, off, so a streaming
  block stays folded until the reader opens it.
- `message_actions.rs` draws the actions under a turn's last answer: Copy
  (every assistant text of the turn, in order), Continue with Fork from
  here, and More with Copy link (`openchamber://session/<id>?message=<id>`).
  They show with the pointer over the message and while a menu is open; the
  menus take the session menu's keys. Pin into context, Start new session
  and the More menu's export, read-aloud and plan items are not ported.
- `transcript_search.rs` is Ctrl+F's search bar while the server setting
  `messageSearchEnabled` is on (off by default, as on the web). Queries go to
  the server's message index after 200 ms, five pages of 200 at most, and
  start at the newest hit; Enter and Shift+Enter step, Escape closes. A hit
  scrolls to its message when the transcript holds it. Word highlighting and
  loading older history to reach a hit are not ported.
- `part_state.rs` keeps per open session what the reader opened (tool rows,
  hand-toggled blocks), the whole-message reads for left-out results and the
  folded reasoning previews. `transcript_clock.rs` is the once-a-second clock
  of live durations and busy dots.
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
  A `diff` fence is a proposal: its header says so and it has no wrap toggle.
- `markdown_math.rs` claims `$…$` and `$$…$$` math. A single-dollar span that
  reads as money stays prose, by the web tokenizer's rules. Formulas lay out
  with RaTeX and draw as SVG images with KaTeX's glyph outlines in the body
  color, cached per source, style and color; a formula that does not parse
  shows its source in the error color. `\(…\)` and `\[…\]` delimiters are
  not recognized (markdown-rs reads them as escaped brackets).
- `markdown_diagram.rs` claims ` ```mermaid ` fences: the diagram in the
  theme's colors with a Copy action for its source. It lays out on the
  background executor the first time a source and theme are seen; the source
  shows until then and when it does not parse. Pan, zoom and SVG download are
  not ported.
- `attachment_part_view.rs` draws a user message's files at the top of its
  bubble: name chips and 64 px image thumbnails. A thumbnail draws a `data:`
  image; any other address shows the failed state, as the web's `<img>` does
  for a `file:` URL. Opening an attachment is not ported.
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
  Raised Settings pickers use the short `.oc-raised` control recipe; their menus
  use the floating recipe. Control heights follow the live rem size.
- `baseline.rs` places text on Chromium's baseline (`css_text`), and
  `tracked_text.rs` draws text with CSS letter spacing. The new-session title
  uses one shaped line with a medium-weight run for the chosen project label,
  keeping the label in the same measured line as the surrounding text.
- `theme.rs`, `locale.rs` and `assets.rs` load the generated resources in
  `resources/`; `bun run native:resources` owns their content. `fonts.rs`
  resolves the web client's default font stacks against the installed fonts.
  The About logo embeds the canonical web brand SVG with its foreground
  changed to `currentColor`, so it follows the selected palette without
  maintaining a second drawing.
- `diagnostics.rs` records timing markers without message or title text:
  window creation, first content, usable composer, completed switch, sidebar
  rebuilds, synthetic playback and, when connected, the first painted session
  list and each visited session's first painted history.
  GPUI's own `log` records reach the same `tracing` subscriber.

- `terminal_view.rs` owns the terminal panel the rail's Terminal button and
  Ctrl+J open in the panel column: the tabs of the current directory (the
  selected session's, else a draft's project), one `TerminalSocket` per
  runtime, creates, closes, adoption, resize and keepalive requests, exit
  handling by `terminalCloseOnExit`, the status line and the Copy/Paste menu.
  Another panel surface taking the column hides it; its terminals keep
  running. Relay and isolated-space terminals are not ported.
- `terminal_surface.rs` is one terminal's entity: it paints the grid with
  box drawing and Powerline procedurally, fits the grid to its box, maps keys,
  IME, pointer, wheel and clipboard to the emulator's encoders and writes the
  result to its terminal. `terminal_emulator.rs` wraps libghostty-vt
  (`vendor/libghostty-vt-sys`) behind a renderer-independent API: replay,
  frames, scrollback, selection, links and the key, paste and mouse encoders.

## Invariants

- The managed host signals only processes it owns: the server it spawned, or a
  descendant recorded from `/proc` while that server still ran. Every signal
  re-reads PID and start time first and refuses a replaced process. An
  external server, a saved host and any unrelated process are never stopped.
- The managed server always listens on 127.0.0.1. It receives the inherited
  environment, so the enterprise policy file and variables reach it, minus the
  native client's own `OPENCHAMBER_NATIVE_*` settings and a UI password the
  desktop only passes with LAN access. Node mode and the launch variable never
  reach OpenCode or terminals.
- Quit closes the server's stdin, waits up to 35 s, then terminates and kills
  the recorded process groups and the server's own group. Nothing restarts a
  server once quit was requested. Restarts after a crash stop after three in a
  row; a failed host waits for Retry Local, which relaunches the app.
- Connection startup verifies server identity and authentication before opening
  the event stream or loading sessions. A malformed status cannot become either
  a successful login or a guessed password challenge. A client-token-only
  challenge offers no password form. Revoked event-stream authentication retires
  that connection generation and returns to the gate without discarding data.
- Passwords remain in the secret input and the captured login request only;
  they never enter drafts, journal snapshots, accessibility values or diagnostics.
  Trust this device is opt-in. Session-only login keeps the token in memory;
  trusted login saves it to the operating system's credential store. Failed
  storage reports a warning and has no file fallback. Storage errors discard
  backend diagnostic payloads because those can contain secret bytes.
- A stored credential belongs to both endpoint and server ID. Identity is
  probed before retrieval or dispatch. A missing stable server ID prevents
  persistence. Storage work runs off the UI thread in request order; cancellation
  cannot move a newer operation ahead of an older write still in progress.
- Sign-in respects the server's Retry-After delay and never retries a password
  automatically. A later user activation starts a new explicit attempt.
  A launch-supplied password is consumed by its first connection attempt.
  A successful login retains the issued token before any later request, so
  connection recovery uses the token instead of repeating the password POST.
- Removing a saved host first forgets this app's stored credentials for its
  endpoint, then removes the profile. The live client can finish current work.
  Credential removal failures retain the profile for retry. Endpoint-scoped
  generations invalidate earlier login persistence and cache insertion; a late
  successful login can finish its live connection but cannot restore a removed
  saved sign-in. Metadata search reads no password values.
- Switching hosts clears password input and its trust choice before replacing
  the connection. The new connection continues the generation sequence; old
  callbacks cannot match a replacement connection by a reused generation.
  Old executors remain alive while captured network work or durable mutation
  chains need them. Reads and subscriptions owned by the old connection stop.
  A queue take that has committed can still return its text to its original
  draft after the switch.
- Host profile writes compare the complete loaded snapshot under a file lock
  before replacing it. Conflicts require a fresh read; neither a failed load nor
  a malformed file grants permission to overwrite it. Default host selection
  references an existing profile, and deletion clears that reference.
  Normal close drains pending profile writes before the draft journal and keeps
  the window open when a profile write fails. Host mutation controls remain
  disabled during this drain; read-only profile loads need no close barrier.
- Appearance startup distinguishes absent, unreadable and malformed snapshots.
  A failed read grants no writer. Live edits remain visible after a failed save;
  Retry rereads explicitly and merges locally edited fields over the fresh
  snapshot before another conflict-checked write. Unedited fields retain another
  window's changes. Automatic writes never rebase a conflict.
- Appearance changes update the existing palette, locale, font bindings and
  Kit theme's root font size, which the root installs before each frame's layout.
  They preserve workspace and input entities. Font and locale
  changes remeasure retained lists with their logical anchors; placeholder changes
  do not replace input text. Environment overrides apply once at startup.
- Settings dismissal retains page models and accepted writes. Normal window close
  freezes preference changes, drains pending writes and keeps failed edits open
  for retry. A later host or journal failure releases the preference freeze.
  Settings cannot create new page owners during any active close barrier.
  A startup appearance read failure alone does not prevent closing an unedited app.
  Before freezing, window close and app quit capture valid committed size input
  through its owning window. Marked IME text remains provisional and is not saved.
  App quit drains independent I/O receipts because GPUI blocks the foreground
  session during shutdown. These drains retain network/credential runtimes and
  captured writes without requiring entity updates. Normal close keeps its UI
  failure/retry path; the toolkit's bounded shutdown deadline still applies to quit.
  Appearance Retry moves its focus to the retained selected-mode control before
  saving removes the button. A retry activated without focus leaves the current
  focus alone, preserving keyboard routing inside the dialog.
- General setting reads carry per-field mutation revisions. Missing or invalid
  fields cannot replace known consumer state. A failed write retains that field's
  live choice for explicit retry while other fields and runtimes can proceed.

- Switching away from a dirty file asks Save, Discard or Cancel. Only a
  successful save with no remaining edits or an explicit Discard replaces
  the editor. Autosave is enabled by default and follows the General preference;
  explicit Save remains available when autosave is off.
  Edits made during a save re-arm the debounce after completion.
- Editor completions use a rename-stable identity. Folder renames rewrite
  the open file's path prefix. Saves wait while the path is being renamed,
  and renames wait for an outstanding save of that path to finish.
  A lost rename response keeps writes blocked until source and destination
  are reconciled through unfiltered parent listings, with the destination
  present and the source absent.
  Those reads cover directories as well as files. An unresolved rename also
  blocks overlapping ancestor moves. Failed or ambiguous checks retain the draft;
  Refresh repeats the checks. A still-present source does not prove the timed-out
  operation stopped; an explicit repeat of the same rename can settle it.
  Each rename and its observations carry an operation revision. Reconciliation
  retires that revision, so a delayed retry answer cannot move the editor again.
  An initial read from an obsolete path clears
  its task and reads the editor's current path before applying content.
- Retained Files, Git and Diff views resolve the live connection's client
  and executor per request, including after authentication changes while
  hidden. A disconnected or different-runtime owner cannot issue requests.
  A disconnected autosave retains its pending state. Workspace readiness
  notifications re-arm it for the same runtime even when the view is hidden.
  Git operation reads stop after 3 consecutive failures; Refresh resumes
  reading the same operation ID. Failed cancellation remains visible.
- The Files surface protects a dirty draft itself, since the server's write
  is last-writer-wins: the open file's stat is read every 2 s while the
  surface is shown and a change replaces only a clean draft. A hidden
  surface polls nothing; expanded folders are listed again every 8 s, except
  folders over 1,000 entries. Changing the search query replaces its request,
  which ends the previous ripgrep on the server.
- Every Git mutation ends in a status read, failed or not, and no row shows a
  state before the server reports it. Push, pull and fetch never retry:
  a lost execute answer is resolved by reading the same operation ID.
- Each surface's status line is one `Role::Status` node with the newest
  outcome, so accessibility clients and scripts read it; errors put the
  server's explanation after the localized title.

- Routing readiness retains its last successful value on a failed same-runtime
  read. Initial connection generations and event-stream reconnections trigger
  reads; ordinary renders do not retry completed failures. OpenCode event replay
  does not replay routing readiness, so resumed connections also refresh it.
- The permission shield shows the selected session's resolved mode, or the
  new-session draft's own choice, else the server's `permissionDefaultMode`. It
  cycles Ask, Safety, Auto, skipping Safety unless `/api/routing` reports the
  safety net (`available && jevAvailable`). A draft's choice changes only the
  client. A session's choice is a PUT whose answered snapshot is applied; nothing
  changes optimistically and a failure keeps the last policy and shows the
  web's toggle-failed notification. Synthetic data leaves the shield inert.
- A session in an isolated space (`/spaces/<id>`, as `spaceIdOfDirectory`
  decides) has its mode in that space's own policy. The web reads it with
  `refreshSpaceModes` when the space's permission stream connects; native has
  no space streams, so it treats the mode as unknown: the shield shows Ask and
  stays inert, and the host's policy never holds the session's requests. A
  draft created in a space still writes its choice, routed to the space by the
  PUT's directory header; that answer is the space's policy and is not applied.
- The policy is read once per connection generation and on every event-stream
  connection, resumed or not, because policy broadcasts are not replayed. A
  failed read keeps the last valid policy. Reads and writes carry an operation
  generation; a runtime switch bumps it, so completions from the previous runtime
  are dropped while writes already sent still finish. A snapshot without a
  revision applies only from the newest operation.
- A draft's explicit choice moves to its creation at submit. After the session is
  verified, the handoff starts the mode PUT with the creation's captured client,
  beside the first send: the PUT neither waits for nor delays the prompt. Its
  failure is logged, as the web's `finishDraftSession` does, and the session then
  shows the server's policy, normally the Settings default the server wrote. The
  choice is in memory only: an app restart before the handoff leaves the server's
  default in place.
- A fresh `permission.asked` for a session whose mode the server answers without
  the user (Auto, or Safety with the safety net) is held: it stays in the
  request store but leaves `visible_requests` and `requests_for_send`, so no card
  shows and a text send does not reject it. `left-for-user` reveals it, a
  terminal event forgets it, and an early report shows its request on arrival.
  A request already in the store is never hidden again. Every stale reconnect,
  resumed or not, a reconcile and a runtime reset release every hold:
  `left-for-user` bypasses the hub's replay buffer, so a resumed replay may
  still have lost it. Because the server sends `ready` before its replay, a
  stale reconnect also opens a replay window. It closes, showing every
  request held inside it, once both the reconnect's policy read has settled
  (success or failure) and the event named by `ready`'s `replayThrough` has
  been applied. The read is a separate request and can settle mid-replay, so
  it does not mark the end alone. With no `replayThrough` (empty replay or
  `replayReset`) the read alone closes it; a second gap or a runtime reset
  supersedes an open window. A session move is no gap and keeps every hold. Within one
  event batch, session changes apply before each request event, so a new
  subagent's first request is decided on its parent's mode. After any applied snapshot (broadcast, read or write)
  and when a routing read loses the safety net, holds of sessions the server no
  longer answers without the user are released, because the server answers
  those requests `ignored` and never reports them left for the user. Native
  never answers a request because of the shield.
- A pending creation's selection is journaled once per destination, including
  choices made after opening that session from the sidebar. Handoff always retires
  the staging marker and preserves the destination's newer choices. The captured
  first-send request remains independent of these later edits.

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
- Session management keys every in-flight change by runtime and session.
  Rename, work state, archive, restore and delete change nothing until the
  server confirms. Results use the live-event index reducer, so its mutation
  revisions keep a delayed list read from undoing them. DELETE completions clean
  only the runtime and subtree captured before their request; descendants created
  during the request retain their local drafts and sends. A real `session.deleted`
  event resolves its current descendants before index removal. A completion for
  another runtime never edits the current index or derives its cleanup scope from
  replacement-runtime records.
  Pins change at once through `PendingPins`; each menu item names its
  target state, so a stale menu asking for what the row already shows writes
  nothing. Writes for one session run in click order and the overlay stays
  until the last settles, so a failure shows the server's state again.
  Every session notification's text is mirrored into the `session-notice`
  status node, which keeps it after the toast closes for automation; the toast
  announces it to screen readers. A rename cannot start on a session whose previous
  rename is in flight. The deletion dialog's Never ask follows the newest
  click at once, as the web's settings store does; saves run one at a time
  with the newest choice, and a failed save returns it to the stored choice.
  An Undo from another runtime reports a restore error and closes its toast.
  A vanished selection under the open archive page closes without navigating
  or moving focus. Archive takes the active subtree and
  delete the whole subtree, resolved when the dialog opens; each session
  succeeds or fails alone and the notification counts both. An archived or
  deleted selection closes without closing an open archive page; Undo
  restores exactly what the archive moved, reports only failures, reopens
  that session only while nothing else is selected, and keeps its toast with
  an error when the restore cannot start. A rename commits a trimmed, changed, nonempty title on
  Enter, the Save button or a press outside the field; Escape cancels, and
  neither key acts while an IME composition is active.
- A tree move goes descendants first and the root last, one request at a
  time, and checks before each step that none of the remaining sessions
  runs a turn. A failed step moves the already moved sessions back, newest
  first. A worktree created for the move is removed with its branch after a
  definite rejection and kept when the answer was lost, since OpenCode
  admits a move before it answers; a lost answer also rereads the topology.
  A quick worktree runs the project's setup commands except untrusted
  shared ones, and the move waits until Git has checked it out.
- Folder changes apply at once and go to the server in order, one batch at
  a time; the shown copy is the newest server copy with the unsent changes
  laid over it, and an older copy never replaces a newer one. A rejected
  batch is dropped and the stored copy read again; a failed one stays first
  in the queue and is retried with a delay up to 30 s and on every stream
  connection. The queue lives in memory: changes not yet sent are lost if
  the app quits before the server answers.
- Warming and folder state are read on every event-stream connection,
  because their broadcasts are not replayed. A warming snapshot older than
  the shown one is ignored.
- An AI rename saves nothing when the title, directory, archive state or
  revert marker changed while it generated.
- The transcript hides rows from the revert marker on; the optimistic marker
  outranks the record's. A revert stops a busy session, reverts descendants
  first, then stages; a refusal drops the marker and returns the composer to
  what it held unless the user typed over the restored prompt. A cut or
  marker change remeasures the session's rows. A commit trims the visited
  transcript before the batch clears the marker.
- A fork opens only while the reader is still where it started; its
  composer gets the forked prompt's text either way. A `/btw` panel exists
  while the parent's `btwSessionID` names a listed fork; its transcript loads
  without being selected. While the panel is expanded, Enter sends the
  composer to the fork with the boundary instruction; Escape collapses it
  before the double-Escape stop. A failed `/btw` start deletes the fork.
  Start and follow-up text stays in the captured persisted draft until
  admission succeeds; success clears it only if it still matches the sent
  text. Refused starts and newer typing preserve the draft.
  A failed revert restores only text it actually replaced, using its captured
  draft key even after a runtime switch. Cross-runtime completions do not
  change markers or show notifications in the current runtime.
- Fork events for locally creating parents do not read the raw fork. Other
  fork reads track intervening patches and read once more before publishing;
  patches received during that follow-up read apply in arrival order to its
  result. No further read is started. A record already listed by completion
  time always wins. Confirmed deletion invalidates the fork read. A callback
  requires its captured runtime, session and read revision to match a live
  request; missing state grants no authority. Read revisions survive runtime
  resets so an older callback cannot consume a replacement request.
- A committed revert retires the visited session's history generation and
  starts a fresh latest-page read. Older latest and pagination completions
  cannot settle history or send attempts, including when their whole page
  follows a boundary outside the loaded window.
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
- A runtime/session runs one send chain at a time, so selection, context and
  prompt requests of two sends never interleave, and at most one interrupt.
  Stop may start while a send is still saving or rejecting the requests it
  supersedes, as the web client's Stop is never gated; that send then posts
  nothing, returns to its draft (a retry stays uncertain) and says so, even
  when the interrupt answers before that send finishes saving. After
  a send's first selection or admission request leaves, Stop waits for it and
  says so with the same error notice as a failed stop. Other sessions proceed
  independently.
- A session holds any number of acknowledged attempts and at most one that is
  not (preparing, sending or uncertain). A new identity begins only when every
  retained attempt is acknowledged, so admission order is send order and an
  uncertain delivery still blocks new text. While the session is busy a plain
  send goes through only to supersede open requests (queueing is a separate
  path); a steer (`Delivery::Steer`) goes through and only while busy, as in
  the web composer. Abort can run beside acknowledged attempts.
- The server delivers idle queues. The client sends queued text only after
  an explicit activation. Enter first gives `/fork`, `/btw` and an open side conversation
  their text, as the web plans local commands and checks BTW before
  queueing; a branching command never takes the queue. Then, in a busy
  session with text in the composer, Enter queues (also while requests are
  open, which it leaves alone) or, with `followUpBehavior: steer`, steers in
  together with the queue, and Ctrl+Enter sends now with OpenCode's own
  delivery; otherwise Enter sends, taking the queue along, queued first and
  joined by blank lines. Merged sends reserve only the leading eligible
  prefix through the server owner's `reserve-prefix` operation. A nonmutating
  preview supplies its complete prompt; that prompt and reservation intent
  reach the journal before the reservation request leaves. Local admission
  checks the whole prospective journal, including unrelated retained drafts
  and attempts. A size refusal leaves storage usable and originals unreserved.
  A failed first write releases the local capture without requesting server
  ownership; storage recovery retains the composer's text. An in-flight,
  goal, scheduled or unjournalable item ends that prefix. Original payloads
  stay persisted on the server, which excludes later delivery until it observes
  admission of the reservation's stable prompt ID. Lost replies recover the
  same receipt, composer text, selection and delivery; context IDs derive from
  the stable prompt ID. No legacy destructive-take fallback runs against an
  older server. An empty or failed reservation sends nothing. A row message
  is taken out of the queue before it is sent or
  edited, so it never goes out twice. A row's send steers while busy with the
  selection it was queued with and, like the web's queued-only send, neither
  dismisses open requests nor waits for them. Composer enqueue publishes its
  complete native source and payload companion before consuming the matching
  draft. Newer typing and independent content remain. ArrowUp
  in an empty composer and Edit take the newest or chosen message into the
  composer after any typed text. A runtime switch drops the projection and
  obsolete projection completions; a taken message whose completion
  arrives after a switch goes back to its draft.
- Reserved sends remain pending under their saved identity after any refusal,
  cancellation or unknown outcome. Retry retains that identity and every
  context ID. They cannot be moved to an independent composer draft while
  the server owns their originals. A lost reservation reply uses Check status
  and Retry against the already-journaled intent, including after restart. It
  never returns the captured text to a second queue submission.
  If the server explicitly confirms that the prefix could not be acquired,
  the attempt is retired and only its captured composer text is restored.
  Row-send retries retain `supersedes: false` through restart and never
  reject open permissions or forms.
- An armed send saves its goal before its prompt. To an existing session, the
  objective file, the goal metadata and the first-turn reminder go first while
  the composer is read-only; the composer's send follows only if the same
  runtime, session and composer are still current, and otherwise the goal is
  removed again and the arm comes back. A new session's first prompt carries
  its goal in memory by prompt ID; its send chain saves the goal and the
  reminder after creation and before any admission, and a failed save returns
  the prompt to its draft with the arm. Queueing never consumes the arm. The
  arm is memory only, and a goal on the server comes back with its session
  record after a restart or a runtime switch.
- Goal status changes and dialog writes complete against the session and
  runtime they captured; a failure changes nothing on screen until the server
  reports the goal. Pause also stops the running turn, and removing a running
  goal stops it too.
- Auto-review runs are a projection of the server's, keyed by the owner of the
  session's directory (the host or an isolated space) and ordered by that
  owner's revision. Every event-stream connection rereads the host's runs and,
  once, the selected session's space; a failed read keeps what was shown. Stop
  and Dismiss remove the row at once and put it back with a reread when the
  server keeps the run. A runtime switch drops the projection.
- Goals and scheduled messages stay for the server to deliver through their own
  protocols. Ordinary rich messages use bounded payload transfers for Edit and
  Send. Their original captures, indexes, file descriptors and independent prompt
  parts remain retained through compact companion references. Inline attachment
  size does not turn a display preview into byte ownership or acknowledgement
  authority.
- Double Escape in the composer of a working session stops it: the first
  press arms a stop for that session for three seconds, a second within them
  stops it, and Escape elsewhere or in an idle session disarms it.
- A starter chip submits at once into the selected project, as Enter does:
  the starter, then the composer's text. The complete source draft leaves the
  composer at the click and becomes a separate persisted starter capture until
  creation takes ownership. Newer typing and attachments remain in their own
  draft. Ordinary creation waits while its source has a retained starter capture.
  A failed read restores source content after newer typing. Conflicting metadata
  or immutable identities retain both owners for explicit recovery. Hydration
  restores captures locally and never resumes activation. Local recovery remains
  available after a loaded journal write failure; dispatch still requires a
  durable snapshot. A built-in renders its visible
  prompt and instructions from that project's magic-prompt overrides, read
  for each activation over the generated built-in templates
  (`resources/magic-prompts.json`); a failed read sends nothing and shows the
  command's failure notice. The session receives the render, while a
  definite send failure gives back what the chip submitted ("/craft-goal
  idea"), as the web restores `presetText`. Only built-ins resolve, since
  native lists no commands or skills; unresolved saved entries stay
  untouched. Global and project edits show at once and save one at a time,
  the newest edit made during a save waiting as a pending one; global saves
  carry both offered-starter markers. A failed project save still writes its
  newest pending edit, retaining any shared-file change from the failed write.
  It re-reads only when no pending edit remains. A project list that failed to read
  shows nothing, is never written, and is read again after a wait that
  doubles from 2 s to at most 60 s.
- A send clears its draft and appends one optimistic user
  message with its exact client ID. It never creates an assistant or changes
  live status. For an ordinary unreserved send, a definite failure removes only that optimistic message and
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
  A retry observes again and resends only the missing steps in order: the
  context after the observed leading run, then the prompt. Context observed
  after a missing item marks the attempt out of order, with no retry. Recovery
  checks, tasks and rows are keyed by runtime, session and message ID.
- A confirmed deleted session takes its captured descendants' and its own legacy
  attempts, creation entries, checks and drafts with it in one journal save.
  Rich local sends become terminal `Deleted`, which grants only local cleanup
  authority and never claims message admission. Exact runtime/session actor tasks
  and unpublished RAM inputs are canceled. The document owner invalidates matching
  loads, saves and native generations before the composer can save that owner
  again. Independent runtimes and sessions retain their state.
- Deleted local-send cleanup uses the retained ledger's exact native, derived and
  companion descriptors, including Proposed holds, without reading source or
  prepared-input bytes. Incomplete or pristine preparation markers remain valid
  roots on Deleted operations, with no preparation, publication or dispatch
  authority. Consumer removal, exact hold removal and newly unowned attachment
  intents share a rollback-safe snapshot before any byte release. The local
   continuation retains its deletion cause after consumer removal. Explicit cleanup
   retry needs no connection or live session; hydration starts no work.
- The committed worker authority keeps Deleted preparation and object roots while
  separately denying their generic puts, downloaded puts, repairs and new
  verification receipts. Stored byte reads and release exclusions remain intact.
  The denial takes effect on a validated load or durable deletion save; an
  in-memory transition cannot replace committed worker authority.
- An attempt nothing can settle offers Move to composer on its row: one whose
  context is out of order, or whose retry the server definitely refused again
  (a 400, 401, 403 or 404, or expired authentication; for example a saved
  model that no longer exists). A lost answer on a retry shows the unknown
  notice and checks again instead. Never while a check, retry,
  interrupt or creation lookup runs, and never for an acknowledged attempt,
  which resolves by its echo. The action appends the prompt's text after any
  newer text in its original draft, drops the optimistic message and removes
  the attempt in one journal snapshot, so a restart shows either the attempt
  or the restored draft. It sends nothing.
- Journal validation rejects a whole snapshot that breaks a rule: two
  unacknowledged attempts in one session, a creation and an attempt for one
  session, a prompt or context ID reused within a runtime, a malformed prompt,
  or an attachment reference over 64 KiB (inline data must be uploaded first)
  or metadata over 64 KiB. A rejected file is left as it was.
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
- Queue captures retain their runtime, exact server directory, session, draft
  owner and frozen configuration. Version 7 loads with an empty queue ledger;
  older envelopes cannot carry version 8 fields. Hydration restores unresolved
  operations without starting network requests, inspecting chunks or releasing
  their server ownership. Runtime switching preserves the local ledger.
- New unified composer operations admit an empty preparation marker with their
  original source, before the first save. This distinguishes interrupted native-only
  preparation from older native-only operations. Input fingerprints, derived file
  plans and context IDs append under the original source seal; they never rewrite
  the captured intent, configuration or original file and context facts.
- A fresh file read requires an explicit parsed mention and resolved path in its
  exact source. Existing model-facing file names and reference URIs authorize no
  read. PDF and path-only mentions remain typed file references. Document and image
  mentions use the existing bounded raw intake and normalization pipeline.
- Every normalized mention bundle records its raw length and SHA-256, complete
  ordered outputs and exact pre-put holds before any output write. Publication
  requires the durable receipt and saves catalog custody before another bundle is
  processed. Independent successful bundles remain rooted when a file fails.
  Partial explicit retries compare the raw input and every normalized output,
  including cataloged siblings. Fully cataloged bundles use their held bytes.
- Prepared inputs bind the source seal, owner, editing generation/revision,
  configuration, complete context order and exact file descriptors. All derived
  files must be cataloged before input sealing. The shared payload envelope retains
  the original local capture bytes within the 50 MiB whole-object limit and 4 MiB
  metadata limit. Restore reads only its verified one-time binding and never runs
  mention resolution, conversion or snippet expansion again.
- Live native captures carry window-local consumption authority. Published captures
  reconstructed after restart preserve current input even when old counters and
  text happen to match. Advanced compound revisions retain their entire document,
  rich references and undo history while the captured send finishes independently.
- New acknowledged Edit Takes record an irreversible first consumer claim. That
  consumer keeps the original context IDs. Later consumers share the immutable
  source read-only and bind fresh context IDs through per-item occurrence records,
  including an explicit empty binding for a source with no context. Each record
  binds its flattened source index, original Take/item, consumer source seal and
  original payload/index digests. Captured intents and original Take proofs remain
  unchanged. Admission allocates and validates the complete mapping atomically.
- A claimed Take retains its spent original context IDs and first-consumer
  operation identity after consumer retirement. Failed admission rolls back the
  claim and mapping together. Earlier version-8 Takes without source-use history
  cannot authorize new claims; their existing captured retries retain their
  original IDs. Hydration never infers unused identities from a missing consumer.
- Composer consumption names its exact published consumer. Preserving newer input
  changes neither current roots nor editor history and needs no live draft
  membership. Clearing a captured current revision additionally requires its exact
  current companion order and published edit identity. Source matching alone
  cannot select a different consumer.
- Companion attachment controls name the original operation, companion object,
  item, reference kind and source-local ordinal. Equal names and URIs remain
  separate occurrences. Removal records only that occurrence's semantic exclusion
  and preserves textarea text, selection, undo and original Take bytes. These
  controls grant no path read, upload, staging or native attachment retirement.
  Known chip metadata survives an additional source read's loading or failed state;
  unavailable editing authority disables removal until local recovery succeeds.
- A queue chunk is distinct from a composer attachment. Its exact operation,
  object, generation, ordinal and descriptor must be durably held before a put.
  Each durable put receipt is journaled before whole-stream verification. Only
  the serialized file owner constructs a verification receipt after checking
  every chunk and the combined SHA-256. A published native companion retains
  prepared-file and source-document roots for attachment retirement checks.
- Queue payloads and native companions are bounded to 50 MiB, each chunk to
  20 MiB, and frozen outgoing bodies to 128 MiB. Request and reply metadata
  remain bounded to 4 MiB. The complete encoded journal retains its 16 MiB
  ceiling, including unrelated drafts and pending mutations. Candidate admission
  borrows retained state and returns a refused source unchanged.
- A take acknowledgement requires its exact committed local destination,
  verified capture chunks and separate attachment descriptors. Ordered file
  proofs come from sealed transport materialization and bind the original
  capture and index digests to each manifest blob or legacy file ordinal. The
  file owner compares local descriptors with those original byte bindings and
  rereads every file before issuing an acknowledgement permit.
  Failed or uncertain journal saves revoke that authority. Unknown enqueue,
  reservation and acknowledgement outcomes retain their original identities.
  Only terminal operations can enter explicit retirement; a durable retirement
  intent and current reference checks precede any named chunk release.
- Explicit Retry may resume a short unpublished spool file only under its exact
  committed operation, hold and full-byte descriptor. Initial writes retain
  collision refusal. Prefix download bindings are committed before a put and
  become immutable preparation roots; transient UI holds grant no repair authority.
  Operation retirement and all newly unowned attachment intents share a snapshot
  before either namespace releases bytes. Independent release failures retain
  their own intents while successful releases remain durable.
- Prefix Capturing proves reservation has not been attempted. Abandonment also
  requires definite local refusal or a fresh complete captured-owner snapshot
  establishing that the selected prefix is unavailable. The captured native draft
  must return to its original owner before operation removal; newer input and
  source-document groups remain. Restoration conflicts preserve every original
  owner and byte root. A reserved or uncertain reservation never uses this path,
  with one exception: the server's `queue_prefix_stale` refusal, which it sends
  only while the session holds no reservation, returns a previewed prefix the
  same way. A capture with an exact rich edit, or an owner with an open compound
  document, keeps the capture: that handback needs the compound owner's restore
  hook, which does not exist yet. A retry then repeats the refused reservation.
- Indexed payloads persist through the transport's sealed `body()` bytes.
  Native prefix comparison records contain typed identities and seals, so a
  display preview never becomes hash material. Preview and reservation compare
  ordered items, payload/index seals, context IDs and delivery independently of
  the queue revision. The frozen prompt precedes reservation in the save order.
- Composer enqueue uses payload-v1 and immutable prepared file bytes. Typed file
  references carry no upload or byte-read authority. Native draft file entries
  without prepared byte custody refuse before capture; their source stays in the
  composer. Protocol metadata and full-snapshot admission refuse before
  acquiring ownership. No path reads a file from a captured server path or agent
  path. Native semantic IDs and source-document grouping remain in the source
  companion; the enqueue receipt supplies official admission IDs.
- Queue uploads use validated safe basenames. A per-use manifest `agentPath`
  retains the exact model-facing name when it differs, including names that need
  an internal upload label. Windows and UNC separator handling follows the shared
  attachment identity helper; literal POSIX backslashes remain in model metadata.
  Prefix recovery accepts earlier captures without the field and validates any
  present identity against its captured local input without rewriting saved bytes.
- An unresolved enqueue retains its original exact owner, source, upload labels,
  payload and request body. Another enqueue from that draft waits. Unknown
  outcomes never append their text back as a new submission. Check status reads
  only the original request. Retry first checks it and repeats only the retained
  request when unknown. An accepted receipt cannot become fresh enqueue authority
  after a later missing lookup. Hydration starts no reads, uploads or retries.
- Accepted enqueues retain their immutable receipt and local roots through queued
  or uncertain delivery. Each explicit server-delivery retry first saves the
  next attempt number and exact authorization body. A lost answer retains that
  authorization; recovery observes or repeats the same attempt before another
  user activation can authorize a later one. Terminal receipt observations permit
  local retirement, followed by durable named-chunk release. Failed release keeps
  its retirement intent for an explicit local retry. No server upload is released
  by the native enqueue caller.
- Queue enqueue tasks survive projection resets and retain their original network
  runtime through disk waits. Close prevents further dispatch and drains accepted
  worker writes; a cancelled network request leaves its prewritten unknown state.
   Permissions and forms never enter queue captures or retry authorizations.
- Enqueue, Prefix and LocalSend finalizers return their captured native file and
  source-group identities after dropping their ephemeral roots. The attachment
  retirement owner considers only these named catalog entries and preserves all
  remaining draft, operation, consumer and preparation owners. Unwritten named
  requests stay pending through unavailable storage, close, capacity refusal and
  failed saves until the existing durable retirement intent owns the release.
  Logical groups without byte descriptors grant no release authority.
- Normal close first waits for actors with retained native file or source-group
  roots across all runtimes. Their task slots remain intact while existing close
  guards stop new admission and dispatch. Foreground finalizers release their roots
  and resume close. Newly pending retirement or failed editor saves keep the window
  and exclusive writer available for explicit retry before worker shutdown.
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

- Every tab with a server terminal stays attached while the window lives, and
  a surface the panel does not show keeps parsing its output and answering
  its queries without painting. Closing the panel or the window never stops a
  server terminal; only closing a tab (or an exit with `terminalCloseOnExit`)
  sends `DELETE`.
- A snapshot replays into a reset emulator at the size it was recorded at,
  then reflows to the fitted grid, with the PTY writer detached: queries in
  history are never answered. The server answers theme queries; this Ghostty
  revision leaves OSC 10/11 unanswered, so the shell gets one reply.
- A new terminal starts at the shown surface's fitted size, else 80 by 24. A
  resize reaches the PTY once the grid has held its size for 150 ms; a tab
  that is starting or adopted never sends the provisional size. Snapshots and
  successful resizes establish the server size separately from the emulator
  size. A fitted grid that differs from a new snapshot sends a resize even
  when its local size has not changed.
- Each terminal permits one HTTP resize in flight. Fitted-size changes
  coalesce until completion, then send the latest target. A snapshot does not
  release that gate; an acknowledgement from an older snapshot revision cannot
  overwrite the new authority and triggers a corrective latest-size write.
  Failed requests retry the latest target after 250, 500 and 1,000 ms, at most
  three times. Rendering cannot restart exhausted retries; a new target or
  snapshot can. Close and runtime invalidation cancel timers and owned HTTP.
  A resize declined for a nonrunning tab clears the pending proposal without
  retrying. The fitted target stays local until a restart event or running
  snapshot resumes reconciliation.
- Opening an empty terminal panel retains its directory-scoped focus request
  until the first active surface exists. A directory change or later user
  focus elsewhere cancels that request.
- The terminal client belongs to a runtime and connection generation.
  Connection restart and runtime switch dispose stale sockets synchronously,
  including while the panel is hidden. This never closes the server PTYs.
- Keys go through Ghostty's key encoder and pasted text through its paste
  encoder; the paste shortcut (Ctrl+Shift+V, Shift+Insert) and the menu share
  one path. Ctrl+C copies while text is selected and then clears the
  selection, so the next Ctrl+C reaches the shell. Ctrl with `=`, `-` or `0`
  stays with the app. While a composition is marked, keys belong to the input
  method; committed text reaches the PTY as text.
- The terminal context disables ancestor Root and workspace bindings except
  Ctrl+J. This includes Root Copy on Ctrl+C, Tab, Escape, Alt+arrow, Ctrl+K
  and Ctrl+K N. Ctrl+V, Ctrl+X, Ctrl+A, Ctrl+Z and Ctrl+W reach the shell;
  Kit input/list bindings are scoped to their own contexts and do not apply.
  Terminal selection copy and paste shortcuts stay with the surface handler.
- The program owns the mouse while it enables reporting; Shift selects
  instead. Ctrl+click opens an OSC 8 link or an `http(s)` URL of the logical
  line through the platform opener.

- Keymap entries the shortcuts own carry `shortcuts::OWNED` in their meta,
  including the terminal's shadowing `NoAction`s. Rebinding removes exactly
  those and adds the new set; the terminal's set is rebuilt after it, from
  the whole keymap, with the effective terminal toggle left to the app.
- The user's shortcut choice applies at once. A server read never replaces
  a change that is still saving; saves run one at a time, and a failed save
  keeps the choice and shows Save failed.
- Sidebar widths stay within 264 to 500 px. The panel keeps at least 320 px
  and leaves the chat 320 px. Layout writes run one at a time, so the last
  change is the one on disk; an unreadable or newer file reads the defaults.

## Decisions

### Why the managed server runs under the bundled Electron in Node mode

The desktop hosts the server in its main process; the native client has no
Node runtime, and porting the server is far larger than parity needs. The
packaged Electron executable with `ELECTRON_RUN_AS_NODE=1` reads `app.asar`,
sets `process.resourcesPath` so the server finds the bundled OpenCode, and
matches the ABI of the unpacked `node-pty`. A separate Node would need its own
native modules.

### Why end of input stops the server and not a signal

The crate forbids `unsafe` and has no libc binding, so it cannot install signal
handlers. When the native process dies in any way, including SIGKILL, the
kernel closes the server's stdin pipe and the bootstrap runs the server's own
graceful stop. Signals go through the root-owned `kill` program only for the
forced stop.

### Why descendants are recorded before the stop request

OpenCode and terminals run in their own process groups and are re-parented
once the server exits, after which nothing ties them to it. The snapshot taken
while the server still runs is what the forced stop may signal. A server that
crashed leaves its OpenCode to the next server's startup reaper, as in the
desktop.

### Why the port is the native profile's own

A stable port keeps the runtime key, and with it drafts and queued sends,
across launches, as `desktopLocalPort` does for the desktop. The desktop's
default 57123 stays the desktop's, or the two apps would move each other's
origin.

### Why a picker choice edits the textarea

A choice replaces the trigger and query as one undoable textarea edit, as the
web's `replaceRange` does. The draft, its journal and every send then see
ordinary composer text, so a picked `@path`, `$skill` or `#snippet` is
prepared at send exactly as one typed by hand. A separate chip model would
need its own journal fields and preparation path. A picked file mention is
not added to a confirmed-mention set the way the web's is, so a picked name
without a dot or slash, or with a space, still reads as plain text at send.

### Why the picker stays at the composer's edge

The web measures the caret and places the list right above the line being
typed; before that measurement it anchors to the composer's start edge. The
native list keeps that anchor. The textarea exposes only the live caret's
bounds, and placing the list at the trigger needs the trigger's position.

### Why session references enter the draft at send

The web derives references from the text at send: a deleted link is not sent.
Native reads the links when a send starts and writes the references into the
draft's context items under a `session-reference:` identity, replacing the
previous set, so the existing rich send, its journal and its recovery carry
them with no new persisted field. A failed send returns them with its draft,
where they show as context chips; the next send replaces them from the text.

### Why Settings owns scrolling and confirmation

Settings uses the dialog's constrained content slot. Each page retains its own
scroller, so scrolling a page leaves navigation in place. The dialog's generic
confirmation is vetoed: single-line inputs propagate Enter after emitting their
field event, and accepting that confirmation would dismiss Settings after a save.
Explicit close and Escape keep the dialog's normal dismissal path.

### Why quit owns I/O separately from UI completion

GPUI blocks the foreground session while awaiting quit observers. A foreground
save task cannot finish there, even when an observer awaits it. Preference and
profile owners retain the raw I/O operation separately so quit can drain accepted
work and coalesced choices after windows disappear. General's drain also owns its
Tokio runtime; a cloned runtime handle alone cannot keep HTTP work running.

### Why Chat and Sessions share General's settings model and not their own

All three pages write fields of the same server settings document. One model
per runtime gives them one serialized write chain, one failure and Retry state,
and the close and quit drains General already proves; a second model would
need its own drains and could race General's writes on the same document. The
Settings dialog lists only pages with implemented controls, so no page opens
empty.

### Why reduced motion comes from GPUI Kit and not a native module

GPUI Kit's base layer reads the desktop's `reduced-motion` setting from the XDG
portal at startup, follows its change signal, and gates every kit transition,
spring and presence on `App::reduce_motion`. The desktop crate draws no
animation of its own, so a second reader would only duplicate that flag. An
animation added here must check `cx.reduce_motion()` as the kit's do.

### Why appearance has one live owner

The palette, locale and fonts affect existing editors and retained transcripts.
Updating their shared bindings preserves those entities and their selections;
recreating the workspace would discard editing and scroll state.

### Why preference retries merge individual fields

A second window can change an unrelated preference before a failed write is
retried. Explicit fieldwise merging preserves those changes while retaining the
current window's edited values. Compare-and-swap still rejects a newer race.

### Why public-asset requests reread machine policy

Filesystem notifications can arrive late or require a rescan. The watched
snapshot keeps the interface current, while the request worker reads the
policy again before network I/O. This keeps a stale interface snapshot from
authorizing a download after an administrator enables the restriction.

### Why the panel surfaces are their own entities

Typing in the Files editor and polling the open file would re-render the
whole workspace if their state lived in `Workspace`. Separate entities
re-render only themselves, and keeping one per runtime and directory brings a
dirty draft back after a session switch, as the web's retained views do. The
activation helper and `IconButton` are generic over the owning view for this.

### Why the source board resolves after the workspace update

The sidebar button opens the board inside the workspace's own update, and
the board reads the workspace's connection for every request so a reconnect
or runtime switch is picked up. Reading it there would panic on the
re-entrant read, so opening a project defers its first resolution until the
update ends; the board shows its loading state meanwhile.

### Why the Files surface polls and does not watch

OpenCode's `file.watch` would shorten the poll to 30 s, but native has no
watch subscription yet. A 2 s stat poll gives the same protection with one
small request; the content is read only when size or mtime moved.

### Why uncertain replies need exact reads

A location list describes current pending coverage but does not identify the
outcome of a particular response attempt. The store keeps uncertainty through
list refreshes and releases it only after an exact owner/ID read. A failed exact
read keeps the card locked and leaves unrelated requests actionable.

### Why held requests stay in the request store

The web keeps a held `permission.asked` out of its directory store and replays
the event later. Native holds it as presentation metadata over the existing
`PendingRequests` owner instead: list reads, settlement and tombstones keep one
source of truth, and revealing a request needs no replay path. A second pending
map would let the two disagree about whether a request exists.

### Why a draft's mode is not journaled

The journal holds what must be replayed to finish a user's send. A permission
mode is server policy with a server-side default; losing an explicit draft choice
to a restart leaves a valid, visible mode, while adding it would change the
journal format for every creation. The choice lives beside the creation in memory
and is written at the handoff.

### Why Markdown links use an application handler

Kit's default handler sends every link directly to the platform. The application
handler keeps session navigation inside its owning runtime and applies the web
client's scheme exclusions and explicit application-link confirmation. A missing
native route cannot become an external launch.

### Why an unresolvable attempt goes back to the composer and not away

An uncertain attempt blocks new text in its session and survives restarts.
When no observation or retry can ever settle it, the user needs a way out, but
deleting it would lose text the server may never have seen. Returning it to
its draft keeps the text and lets the user decide whether to send it again.

### Why acknowledged attempts no longer block the next send

Each attempt stays until authoritative content replaces its optimistic item,
because an echo can lag its admission. Blocking every send on that echo, as one
slot per session did, would make steering impossible while the first prompt's
turn runs. An acknowledged attempt is already ordered on the server, so only an
uncertain or in-flight one still blocks a new identity; an explicit retry uses
the same saved ID. Abort can run after the send request finishes and leaves
running status to the live channel. New-session creation is owned by
`session_creation.rs`; queue controls live in `message_queue.rs`.

### Why deleted local sends retain a distinct terminal state

Confirmed session deletion ends dispatch authority even when preparation never
published complete inputs. Retaining the original source and partial byte roots
under `Deleted` lets local cleanup resume after restart or a failed save without
inventing server admission. Named released native roots go through the shared
attachment retirement owner, which preserves independent consumers and saves
deletion intent before releasing bytes.

### Why merged queue sends retain server ownership

Removing items one at a time allows the server to deliver a later item while
the client still holds an earlier one. Even an atomic take without retained
ownership can lose its payload in a dropped response. The existing queue
owner therefore reserves a prefix durably, retains its immutable receipt and
blocks later delivery until the stable prompt ID is admitted. Native journals
that identity and retries it without creating an independent draft copy.

### Why an attempt's draft may differ from its prompt

A definite failure gives an unreserved attempt's draft back to the composer, and the
journal stores the draft beside the prompt. A starter sends its rendered
prompt but gives back what the chip submitted, so the user can edit and send
it again in the form they chose; requiring the two to match would lose that
form. A reserved merged send retains the delivered text in its journaled draft
and keeps server ownership until admission, so recovery uses the same prompt ID.

### Why repeated submissions bind working context identities

Original Take payloads and indexes retain the byte and acknowledgement proof.
Independent later submissions receive fresh context IDs in working local captures
so server deduplication cannot treat newer input as a replay of an earlier send.
The durable claim keeps original IDs spent even after that first consumer retires.

### Why goal messages are left to the server

Taking a queued goal message and sending it as an ordinary prompt would drop
its goal: the queue's payload transfer and durable enqueue routes refuse goal
items. Native leaves such a message queued, and the server starts the goal
when it delivers it. For the same reason a native enqueue never consumes the
composer's arm; the arm waits for the next direct send.

### Why an existing session's goal is saved before the send starts

The web awaits the goal write between its optimistic message and the prompt.
Native sends from two pipelines, the plain attempt and the journaled rich
actor, and neither can wait on a network step before its durable preparation.
Saving the goal and its reminder first, with the composer read-only, keeps both
pipelines unchanged and keeps the order the server's loop relies on: goal,
reminder, prompt. A send that cannot follow its goal removes the goal, since a
goal without its prompt would be audited against the previous turn.

### Why the small model is read when a goal is about to start

The web reads the small model's availability when the composer mounts.
Native reads it when the target button arms a goal or the create dialog opens,
so an idle composer costs no request and the startup reads stay those the
recovery contracts list. An unknown answer allows a goal, as on the web; an
answer that arrives unavailable disarms the button.

### Why Open selects the review session

The web opens the review session in a read-only context panel tab. Native has
no chat tab in the context panel, so Open and a finished manual review select
the review session in the chat area, which the sidebar lists like any session.

### Why Edit appends to the composer instead of replacing it

The web replaces the composer's text with the edited message. Native puts it
after any typed text, as a failed send does, because replacing would discard
what the user typed with no way back.

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

### Why close waits for native-root callbacks

A worker flush drains accepted I/O before foreground actor callbacks consume its
receipts. Those callbacks can still release a removed attachment's last temporary
root. Close waits for that ownership transition before sealing the worker, so the
resulting named retirement request can retain its writer or reach durable custody.

### Why one `ListState` per visited session

GPUI Kit 0.7.3's `MessageScroller` keeps its `ListState` private, so a pixel
offset inside a row cannot be saved or restored through it. The transcript uses
GPUI's `list` with a retained `ListState` per session: returning to a session
reuses the state that still holds its anchor message and offset, and
`ScrollMemory` records the same anchor by message ID for sessions whose state
is rebuilt.

### Why menu items without a native feature log instead of hiding

The row menu lists every item the reference lists, so its layout and
keyboard order match. Open run overview, Open in side panel and Open Mini
Chat window have no native feature behind them yet; they log a warning. Hiding
them was the alternative, but that would make the menu differ for every
session until those features land.

### Why worktree moves run step by step on the window

Each move step checks the live status of the sessions still to move before
it sends, as the reference does, so a session that starts a turn during a
long setup is not moved while it runs. One network future for the whole tree
would have been simpler, but live status is window state.

### Why folders sit in the sidebar state

The row projection reads only `SidebarState` and the grouped sections. The
folder copy and the collapse marks join it there, so the core builds folder
rows without another input. The desktop's folder store stays the authority
and copies its map in on every change.

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

### Why RaTeX for math

The web renders math with KaTeX. RaTeX implements KaTeX's layout rules and
ships KaTeX's fonts, so formulas keep KaTeX's spacing and glyphs without a
JavaScript engine. Its SVG output with embedded glyph outlines needs no font
setup in GPUI's SVG renderer.

### Why diagrams lay out off the main thread

`mermaid-rs-renderer` measures labels with system fonts, loading the font
database the first time, which takes longer than a frame. A diagram's first
layout runs on the background executor and the fence shows its source until
it lands; later frames read the cached image. The renderer keeps a copy of the
measured font under `$XDG_CACHE_HOME/mmdr`.

### Why one transcript clock that stops itself

The web runs live durations and busy dots on one shared clock
(`useDurationTicker`) so a page pays for one redraw a second. The native
clock is marked by each frame that draws a live indicator and redraws only
if the last frame drew one, so it stops on its own once no running row is on
screen, without each row tracking its own timer or visibility.

### Why opening a row lets go of the end

A transcript following its end keeps the end in view as rows grow, so a tool
row or reasoning block opened near the end would grow upward and push the
header the reader clicked out of view. Opening one pauses following and pins
the current top row and offset first (`dispatchRowExpand` on the web); the
content opens below the header, and returning to the end resumes following.
GPUI's own pause leaves the scroll anchored at the end, so the top is pinned
explicitly.

### Why a left-out result fills in only shelled calls

A whole-message read can return after live events changed the same message.
Pushing the read would roll those changes back, so `complete_elided` copies
only the calls still held as shells, and only from a whole result of the same
status; the rest of the message stays as the live channel left it.

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

### Why only pins are optimistic

The web client flips a pin before the server answers and waits for every
other session change. A pin is a reversible display preference, while an
archive or delete removes the row and closes the open session; showing those
before confirmation would need a rollback that restores selection and
scroll. The pending pin is an overlay outside the index, so a live event
carrying older metadata cannot flip the row back while the write is in
flight, and settling the overlay can never leave a stale value behind.

### Why a revert marker is optimistic and a fork is not

The web hides a revert's messages before the server answers, because the
user just asked for exactly that view, and keeps the messages in the store
behind the marker. Native keeps the marker as an overlay keyed by runtime and
session, so a refusal only drops the overlay; no message moves. A fork
creates a session the client cannot name before the answer, so it waits.

### Why `/btw` retains drafts through admission

BTW start and follow-up text remains in the normal persisted draft until the
send succeeds, so closing during admission does not lose the question.
There is no automatic replay into a temporary fork after restart. Success
clears only the unchanged captured text; subsequent edits remain unsent.

### Why `.test_support()` on rows and controls

GPUI Kit's headless test harness observes only elements marked with
`.test_support()`. The call is the identity in normal builds, so production
render chains keep their native types while `workspace_tests.rs` clicks rows
and presses keys through real hit testing.

### Why the terminal context disables workspace keys

GPUI matches bindings before key-down handlers. Root Copy can therefore
consume Ctrl+C when the transcript retains a selection. After registration,
the terminal disables bindings applicable to its ancestor context stack with
`NoAction`, including multi-key prefixes. The effective Toggle terminal chord
(Ctrl+J by default) remains an app shortcut, matching
`handleTerminalShortcutCapture` in `useKeyboardShortcuts.ts`.

### Why scrollback is budgeted in bytes

Ghostty bounds scrollback by page memory, not rows, despite its option's
name; the reference passes 10,000 and keeps about 600 rows. The surface sizes
the budget from the columns (16 bytes per column and 256 per row, for at
least 100 columns), which keeps at least 10,000 rows. A narrower grid keeps
more, since rows are pruned by whole pages.

### Why the panel keeps every terminal attached

The web client detaches hidden tabs and keeps a 512 KiB replay projection to
restore them. Native keeps each tab's emulator and attachment, so switching
tabs repaints at once and a hidden terminal answers its program's queries.

### Why the shortcuts rebuild GPUI's keymap

GPUI matches key bindings before key-down handlers, and the terminal relies on
that order to shadow app chords. A dispatcher in a key-down handler, as the
web's, would run after the terminal and the inputs had already taken the key.
So each effective binding becomes a GPUI binding, and a changed set replaces
the marked entries. Only actions with a native command are bound: a binding
for a missing feature would swallow a key the focused control should get.

### Why the menu shows Electron's accelerators

Electron's popup menu lists its template's accelerators as hints; the keys go
to the renderer's configurable shortcuts. Native copies the labels as text, so
a rebound shortcut does not change the menu, as in Electron. Items whose
feature is not ported stay in the menu, disabled, so its structure matches
the reference.

### Why the layout lives on the device

The web keeps the sidebar and panel sizes in its persisted UI store, outside
the settings document, so each device has its own. Native writes the same
three values to its private folder instead of the server.
