# Native application core

`openchamber-native-core` owns the native desktop's application model: typed
identifiers, session records, display ownership, Chats membership, lifecycle
ordering, project grouping, the sidebar row projection, ordered transcripts,
drafts, scroll memory, load states and the synthetic workloads. It depends on
the standard library alone. JSON belongs to transport and fixture adapters;
rendering asks for a projection and dispatches an action.

## Ownership

- `request_owner.rs`, `permission.rs` and `form.rs` define blocking requests,
  captured runtime/location ownership, explicit decisions and typed form values.
  `request_list.rs` carries validated siblings, scope/owner coverage and explicit
  entry issues. `pending_requests.rs` owns one location's pending lists and response lifecycle.
  It performs no I/O, automatic replies, retries or persistence.

- `permission_mode.rs` ports the permission modes of `permissionAutoAccept.ts`:
  the shield's cycle and the Safety gate, lineage resolution, snapshot ordering
  by revision (`PermissionPolicy`) and the hold-back bookkeeping of fresh
  requests the server may answer itself (`AutoAnswerHolds`). It answers nothing.

- `search_rank.rs` ports the web's list matchers: `rankByQuery` and
  `matchesRankQuery` (`lib/search/fuzzySearch.ts`), `matchesFuzzyQuery`, and
  `rankBySubsequence` with its path mode (`lib/search/subsequenceSearch.ts`).
- `autocomplete.rs` ports the composer's pickers: which one the caret opens
  (`composer/language/triggers.ts`, `fileMentionAutocompleteState.ts`), what
  each lists and in which order (commands, skills, snippets, and `@` agents,
  files and sessions), and the edit a choice makes. It performs no I/O.
- `session_reference.rs` ports `sessionReferences.ts` and the text side of
  `sessionLinks.ts`: link candidates in text, link keys, and the context text a
  session or message reference sends.
- `send_config.rs` ports default agent/model selection, an existing session's selection from its record, catalog grouping, composer labels and agent color allocation from the web client.
- Default effort follows the configured choice, a supported matching agent pin,
  then the model's remembered choice from server settings. A remembered explicit
  Default stays distinct from an absent entry when restoring session effort.
- `composer_selection.rs` retains owner-scoped choices and their creation handoff.
  Sessions opened before catalogs arrive fill only missing choices from later
  defaults. Catalog gaps leave restoration pending and preserve explicit picks.
- Session-index recency deferral compares all non-recency fields. Model, effort
  and agent changes apply immediately even when sidebar appearance is unchanged;
  later patches must build on those values before the session settles.
- `repository.rs` holds what the card reports about a repository,
  `project_actions.rs` which project action its row shows and with which
  glyph, and `tool_names.rs` OpenCode's tool name comparison, including
  which finished calls may change what Git reports.
- `tool_shells.rs` decides what a tool row shows from the call's typed
  `ToolFacts` (`lib/opencode/tools.ts`, `toolPresentation.tsx`,
  `toolRenderUtils.ts` and `ToolPart.tsx`'s header helpers): the display
  name, icon, description, justification, diff and line counts, duration,
  which tools draw the compact static row, the input the expanded row prints,
  and the folded reasoning preview.
- `file_tree.rs` holds the Files surface's tree: one snapshot per listed
  folder, expanded and selected paths and the visible rows (folders first,
  then names in `localeCompare` order; dotfiles shown, `node_modules` left
  out until gitignored entries are shown). `file_draft.rs` ports
  `fileEditorContent.ts`, `fileStatChange.ts` and `fileContentPoller.ts`: the
  open file's loaded text, line ending, draft, one-snapshot saves and the
  poll steps. `git_changes.rs` splits a status into staged and unstaged rows,
  orders index mutations and status reads, builds network operation requests
  for the tracked remote and checks operation snapshots
  (`boundGitNetworkOperation.ts`).
- `source_board.rs` ports the repository source of the issues and pull
  requests board: kinds, state and people filters
  (`referencePickerItems.ts`), item keys and state looks, the read context
  a repository is read with (`getBoundSourceControlReadContexts`,
  `resolveSourceControlTarget`, `repositoryHostsOf`), and `BoardList`, the
  searched list with paging, refresh and the highlighted row
  (`useReferenceBrowser.ts`, `referenceCache.ts`).
- `schedule.rs` ports the scheduled task rules the editor and list show:
  cron parsing and the next-run preview of `lib/cron.ts` (five or six
  fields, names, `L`, `#` and the `@` shorthands, as `cron-parser` reads
  them), the row labels of `ScheduledTasksDialog.tsx` and the draft rules of
  `scheduledTaskEditorDraft.ts`. `time_zone.rs` parses compiled zoneinfo
  (TZif with its POSIX footer rule) and turns wall-clock times into instants;
  the caller reads the files. The server computes and enforces the real
  schedule. `tests/cron_parity.rs` checks the preview against next runs
  `cron-parser` 5.10.1 produced (`tests/fixtures/cron-next-runs.json`),
  across daylight-saving changes in five zones; a wall time the clock skips
  fires when the gap ends, as it does there.
- `knowledge.rs` holds Project knowledge records and the panel's rules:
  note titles, the todo order, the agent `ready` rule (a deleted dependency
  counts as met), the plan Markdown split and the panel-wide search.
- `provider_logo.rs` ports the provider logo lookup of `useProviderLogo.ts`
  (bundled candidates, then models.dev) and inverts an SVG's paint colors for
  dark themes, as the web's `dark:invert` does.
- `session_actions.rs` decides a session row's menu (the ported items, their
  order and when each is enabled), which sessions archive, delete and
  restore reach (`sessionLineage.ts` policies), `isRestorableOnItsOwn`, the
  archive page's list and the pending pin overlay. Track and Done belong to
  root work rows outside archives and Chats; `SidebarRow::Session::owns_work`
  is false inside worktree and space works.
- `ownership.rs` and `sidebar_collection.rs` own directory ownership and sidebar
  membership. Their reference is `sessionOwnership.ts` and the production
  collection in `sessionCollection.ts`. `sidebar_rows.rs` builds the normal Works view
   of `sessionSidebarRowModel.ts`, grouping worktree, space and session works under
  projects, with the section gaps of
  `sessionSidebarVirtualization.ts`; `grouping.rs` ports
  `buildGroupedSessions` and `buildSidebarChatGroup`; `ordering.rs` ports
  `session-ordering.ts`; `relative_time.rs` ports the sidebar's compact ages.
- The sidebar collection filters registered project/worktree and valid space
  directories before ownership. Only active managed Chats enter Chats; active
  temporary forks are excluded. Archived inputs retain the production archive
  selector's scoped records. Ownership receives project sessions and scoped
  archives, so active Chats cannot contribute canonical-project metadata.
- `transcript.rs` holds one session's messages in server order and reports the
  rows each mutation touched. An assistant message carries its step's
  agent, model, finish, error and token usage, and each assistant row knows
  its turn (`turn_position`); `turn_footer.rs` formats and fits the facts a
  finished turn shows. A compaction record carries its lifecycle, and
  `context_usage.rs` reads a session's context fill from the newest response
  that reported tokens, unknown after a finished compaction. Parts carry
  their times and steps their cost (`Usd`, billionths of a dollar so sums are
  exact), and `turn_stats.rs` ports `getLatestCompletedTurnStats` over them.
  `message_time.rs` writes a message's time as `formatTimestampForDisplay`
  does, from local calendar fields and a locale's exported month names and
  clocks. User and assistant messages carry parts; every
  other OpenCode record (synthetic, system, skill, shell, compaction, switched
  agent, model or location, idle) is a notice with one line of text.
- `branching.rs` decides where fork, staged revert and `/btw` cut a loaded
  transcript (`session-actions.ts`, `revertedMessageDockState.ts`,
  `btw.ts`): a prompt's cut starts at its leading synthetic context, a
  marker hides itself and what follows, `/fork` keeps the last finished turn
  and `/btw` forks at the newest step that finished with `stop`. A record
  carries its revert marker (`SessionRecord::revert`) and `/btw` links
  (`BtwMetadata`); a synthetic record carries its description and subagent
  run, dispatched-session result and workflow report (`SyntheticFacts`), and
  a tool call the child session it names. Each report starts a new turn boundary.
- `session_index.rs` ports the global session list of
  `useGlobalSessionsStore.ts` and `session-event-router.ts`: active and
  archived records, page merges, complete loads and live session changes.
  A record carries the session's current model and agent as the server last
  reported them.
- `session.rs` retains an explicit review kind and the reference prompt-readonly
  policy. Review metadata overrides a stale parent ID; an ordinary child's
  prompting preference overrides the surface readonly flag. Root and review
  sessions retain the surface flag.
- `sidebar_work.rs` assigns sessions to Works entries from the project
   sections. A linked worktree or isolated space is one group work, even empty;
  elsewhere each root and its descendants form one work. Archives are
  excluded. The first matching session in lifecycle order determines each
   work's position. `tests/sidebar_work_parity.rs` compares membership and
   ordering with the web's `works/sidebarWorks.ts` implementation.
   Group works retain their `GroupLabel`, so the shared expandable row keeps
   the space's name and icon. Empty spaces use that same work row and count
   toward the project's reveal limit.
  Works may be grouped under projects or in one list by time. The time list
  has no reveal limit. A work listed in In work leaves its project unless
  `keep_work_in_place` is set; a pinned session is exclusive to Pinned while
  that zone is shown.
- `ownership.rs` also derives configured projects from settings paths and a
  project's linked worktrees from the server's listing, as the web client
  does.
- `message_queue.rs` ports the projection of `messageQueueStore.ts`: the
  server-owned queue keyed by runtime, normalized directory and session, the
  owner (host or isolated space) that answers for each directory, which
  messages a client may take itself (`takeable`), and the follow-up behavior
  setting. `draft_starters.rs` ports the starter lists of
  `draftStarters.ts` and `useDraftStarters.ts`; `magic_prompt.rs` ports the
  prompt-pair commands' parsing, variables and template rendering.
- `session_goal.rs` ports the goal's validation, presentation and first-turn
  reminder (`sessionGoalMetadata.ts`, `sessionGoalPresentation.ts`,
  `sessionGoalActions.ts`); objective lengths count UTF-16 units as the web
  does. `auto_review.rs` ports the auto-review projection of
  `useAutoReviewStore.ts`: runs keyed by owner and reviewed session, never
  going back to an older revision, a full snapshot replacing only its owner's
  runs older than itself, and a space never answering for a host chat.
- `draft.rs` and `scroll_memory.rs` hold client-owned state keyed by the
  runtime and the session or draft identity captured when it was created.
- `draft_content.rs` holds semantic attachments, agent mentions, skills, opaque
  metadata JSON and context items. Attachment bytes, source paths, spool records
  and materialization into wire requests belong to the desktop and transport.
  Transport reexports the core's mention and skill records through its existing
  `prompt_parts` module and serializes them with private wire adapters.
- `synthetic.rs` produces the documented offline workloads. It is part of the
  core so the desktop, tests and measurement tools share one definition.

The desktop crate owns timers, focus, text layout and windows. The transport
crate owns wire parsing and authority checks, and hands the core records and
`TranscriptEvent`s.

- `terminal_tabs.rs` holds each working directory's terminal tabs: their
  order, the active tab, the server terminal each shows and its lifecycle
  (`Starting`, `Running`, `Exited`, `StartFailed`, `Ended`). It adopts the
  server's interactive terminals from an authoritative list. The server owns
  the terminals; tabs live for the window's lifetime and are not persisted.
- `shortcuts.rs` ports `packages/ui/src/lib/shortcuts` (`config.ts`,
  `schema.ts`, `bindings.ts`) and the recording rules of
  `ShortcutRecordingDialog.tsx`: the declared actions in Settings order, the
  binding spelling (`mod`, at most two chords), display per platform,
  conflicts, effective bindings from the settings document's
  `shortcutOverrides`, and the recorder's state and verdict. It decides no
  dispatch; the desktop binds what it has commands for.

## Invariants

- A board list read applies only while it is the newest read of the list.
  A failed first page is a failure, never an empty list; a failed refresh
  keeps the listed rows and says so; disconnected and no-repository answers
  are their own states. A next page appends only rows not already listed.
- A folder has at most one listing in flight. A mutation's listing replaces
  it, a poll never does, and a listing from before a filter change is
  dropped. A failed listing keeps the folder's last entries and reports its
  failure; a failed first root listing is not an empty folder.
- An open file's dirty draft is never replaced by an external change. A clean
  file takes a change only after two reads agree, and a read started before a
  load, swap or save is dropped. A save writes one snapshot, so edits made
  while it runs stay dirty; autosave never writes an empty draft over a
  non-empty file, and a read-only file never saves. A file that went missing
  with a dirty draft keeps the draft; a clean one shows the failure and
  reloads once it is back.
- Line endings follow the file: the editor holds `\n`, and a save writes the
  ending the file mostly used when it loaded.
- Only the newest status read applies, and a failed read keeps the last
  status. Index mutations run one at a time in the order asked.
- A network operation request names the tracked remote (the longest remote
  name the tracking ref starts with) and its branch; pull and push without
  tracking, a detached HEAD, a missing or stale remote grant and a push over
  an anonymous grant are refusals, never a guessed remote. A finished
  operation never changes its outcome and completed steps never disappear.

- Blocking-request keys include runtime, remote directory, actual session or
  location owner, and a typed permission/form ID. A location-owned form is never
  assigned to the selected session. Permission requests always have a real session.
- Each permission/form list has its own complete baseline and load sequence.
  A failed envelope preserves retained entries. `RequestList` retains explicit
  completeness through record mapping; an incomplete response publishes validated
  siblings without removing any held record. Owner-subset lists are also merge-only.
  Only a complete whole-location list grants removal authority. Events and
  mutations newer than a read survive its completion.
  A terminal tombstone prevents delayed snapshots and replayed asks from restoring
  that identity for the lifetime of the location owner.
- Each explicit reply has an independent attempt. Definite refusal releases only
  that request; an unknown outcome requires an exact owner/ID pending read before
  another reply. A terminal event wins over late failures. Invalidation makes held
  entries non-actionable until fresh data arrives and invalidates old read/attempt tokens.
- Form conditions reference earlier editable fields. Inactive answers never
  activate dependent fields and must not be submitted. External fields have no
  default acknowledgement. Numeric answers are finite; wire numeric sentinels
  are normalized to absent bounds/defaults by transport.
- Form defaults follow Electron's `initialFormValues` and `fieldAnswer`: free
  strings use ECMAScript whitespace trimming and empty strings/empty multiselects
  supply no answer. Exact selected option values remain unchanged, including
  empty strings and whitespace. `evaluate(defaults())` removes inactive defaults
  before submission. ECMAScript trims BOM and does not trim U+0085.
- `FormAnswer` is a map newtype whose Debug hides keys and values. Form values,
  fields, defaults, metadata, titles, cancellation feedback and permission resources
  use redacted Debug implementations. Enclosing stores retain useful IDs and kinds
  without printing request content.
- Form validation checks value types, choices, required fields, bounds and UTF-16
  string lengths. Regexp patterns and string formats return
  `RequiresServerValidation` after the supported checks pass. This is neither a
  valid-answer verdict nor a reason to invent a replacement pattern engine; the
  transport can submit the explicit answer for authoritative server validation.
  These optional local checks exceed Electron's required-presence checks; the
  desktop must not present them as existing Electron pattern/format validation.

- A session's mode is the nearest explicit mode up its parent chain; an unknown
  parent or a cycle ends at Ask. A stored Safety displays and cycles as Ask while
  the safety net is unavailable.
- A policy snapshot older than the held revision never applies, and one without
  a revision applies only while no revisioned snapshot was held. Equal revisions
  apply again, so a broadcast echo of a write is harmless.
- A held request is keyed by session and permission ID, so a report reveals only
  its own session's request. Reports that precede their request are kept in a
  bounded queue of 100 and consumed once.

- Display ownership never rewrites a session's real directory. Routing, pin
  identity and drafts use `SessionRecord::directory`.
- OpenCode's `global` project ID never bridges to a configured root, and
  conflicting canonical metadata stays unresolved.
- Chats lists managed roots and builds their descendants from the same admitted
  hierarchy as projects. Project descendants remain reachable across directories.
  Lifecycle ordering precedes ownership buckets, and cycles emit each record once.
- Server order is transcript order. Identifier spelling never reorders rows.
  An unchanged event returns `TranscriptChange::Unchanged`, and an event for an
  unknown message returns `NeedsReconcile`.
- Every change to the session index records a mutation revision, removals
  included even when the session was already absent. A load never overrides a
  session changed after the revision it started at, so a delayed page cannot
  restore a deleted session or move one back. Only the newest load completes,
  and only a complete load grants full-list authority.
- A patch needs a listed record, and a change older than the held record is
  ignored. An update that moves only the session's recency waits until its
  turn settles, so streaming never rebuilds the sidebar.
- `Transcript::materialize` merges a history read the way
  `materializeSessionSnapshots` does: held messages win unless the read
  reports a completion they lack, streamed text that runs ahead of the read
  stays, finished tool calls never return to an active state, and messages
  changed after the read was requested follow it. Held messages the read
  omits stay when they precede its oldest message (older pages) and go when
  they follow it (deletions).
- A delta for a part the transcript never saw start returns
  `NeedsReconcile` instead of building text without its beginning. The first
  delta after a full-text snapshot appends only what the snapshot lacks.
- Message deletions retain their revision for the transcript's lifetime,
  including IDs not loaded yet. History reads and older pages discard those
  records when the deletion is newer than the read. A later authoritative
  live record may restore the ID.
- A committed revert (`Transcript::truncate_from`) removes the boundary and
  every later message and records each removal, so a read started before the
  commit cannot restore them. A boundary the transcript does not hold trims
  everything only when the session's own marker names it. An empty transcript
  retains the commit boundary so an in-flight history read drops that boundary
  and its following records when it materializes. A pre-commit page without
  the boundary has unknown position relative to the cut and returns
  `NeedsReconcile` without changing retained history. Reads started after the
  commit retain their authority.
- A running call's child comes only from live progress; a history read of the
  running call keeps the child the transcript already holds.
- A tool result a history page left out (`PartContent::Tool::elided`) never
  replaces the whole result a transcript holds for the same call and status,
  through `replace` or `push`. A live transition with output, or the whole
  message, completes it. `complete_elided` fills in only calls still held as
  shells, from a whole result of the same status, and leaves every other part
  of the message as the transcript holds it.
- A tool transition merges the facts it reports into the held ones: input,
  metadata, error and listed input each change only when the transition
  carries them, so a result without input keeps the call's input.
- Lifecycle order changes only on live phase transitions or authoritative
  snapshots. Token arrivals and metadata writes never move a row.
- A failed load keeps its retained snapshot; only `LoadState::Complete` grants
  full-scope authority.
- A non-empty draft leaves the store only by an explicit send or clear.
- Files, context, agent references, skills and metadata all make a draft retained.
  Only trimmed non-empty text, files or context make it sendable. Caret selection
  uses UTF-8 byte offsets; agent mention ranges use UTF-16 code units.
- Captured-content consumption requires unchanged text and removes only complete
  matching values. New or changed files and context remain, including changed
  payloads under the same stable identity. A new textual mention refuses the
  consumption because its range still needs the retained text.
- Failed restoration preserves newer typing, selection and independent content.
  Different non-empty texts join with a blank line; equal texts remain single.
  Appended mention ranges move by the prefix's checked UTF-16 length. Conflicting
  top-level metadata, changed files or context with the same identity, and offset
  overflow refuse the merge without changing either input. The caller keeps the
  retained attempt until that conflict is resolved.
- Materialization preserves full draft content and retains the existing
  destination-wins rule when a session draft already exists.
- Draft diagnostics expose stable attachment/context identities and counts while
  hiding text, references, filenames, agent/skill names and metadata values.
- A queue projection never goes back to an older revision: per queue, and
  per runtime and owner for complete snapshots. A snapshot replaces only its
  owner's queues and keeps those a newer answer changed. An owner speaks only
  for its own directories. Nothing in the projection is persisted.
- Starter edits keep entries this client cannot resolve; only the user's own
  lists change, and shared project starters keep their place in front.
- Default selections preserve configured identifiers through catalog gaps. Only
  remembered and automatic fallback choices skip models hidden in the picker.
- Agent color allocation consumes a theme-prepared ordered palette. Its indices
  refer to keyword, type, function, number, string, operator and variable;
  configured colors accept six-digit hex except the migration placeholder.
- Synthetic workloads are deterministic: the same size yields the same records
  and text.

- Terminal adoption takes only an authoritative list: a failed read never
  reaches `TerminalTabs::adopt`, because an empty list ends every running tab.
  Adoption skips project-action terminals and never moves a tab that is still
  starting. A create that completes after its tab closed is refused by
  `started`, so the caller closes the orphaned terminal. Default labels number
  one above the highest open "Terminal N", so they stay unique after closes.

- Shortcut overrides keep the reference's `Record<string, string>` shape.
  An override that does not parse falls back to the default; the unassigned
  value removes a binding; internal actions ignore overrides. Every declared
  default is valid and no two defaults collide (`shortcuts::tests`).

## Decisions

### Why typed tool facts and not raw JSON in the core

The web reads a call's input and metadata with zod schemas at render time.
The core has no JSON dependency, so the transport reads the fields the rows
use into `ToolFacts` once per record, each field on its own as the schemas
do. Rows then compare and draw typed fields without parsing on every frame.
The one JSON-shaped output, the generic field listing an unknown tool's
expanded row prints, is made in the transport as `listed_input`.

### Why Fuse's fallback is an edit distance

`rankByQuery` and `matchesFuzzyQuery` fall back to Fuse.js for typos. With
`ignoreLocation`, Fuse's Bitap search accepts a pattern whose fewest
insertions, deletions and substitutions against some substring of the text,
over the pattern's length, stay under the threshold; that score, floored at
0.001 and raised to the field norm, orders the fuzzy tail. `search_rank.rs`
computes the same quantity with Sellers' algorithm, so the core keeps no
dependency and the match set is the same. Fuse's split of patterns longer
than 32 characters is not reproduced; picker queries are shorter.

### Why draft metadata stays opaque

The core depends on the standard library alone. Transport and persistence adapters
own JSON validation and serialization; the core compares retained JSON strings
exactly and refuses conflicting values without inventing a merge of their keys.

### Why failed content can remain owned by its attempt

A stable attachment or context identity can carry an edited payload by the time a
send fails. Choosing either value would discard content, so restoration returns a
typed conflict and leaves the retained attempt and current draft unchanged.

### Why blocking requests are kept outside the send journal

Pending permissions and forms belong to live server state. Replaying a retained
approval after restart would perform a new mutation without a current user action.
The core retains only in-memory request state; reconnects re-read the captured
location and terminal IDs remain tombstoned until that owner is dropped.

### Why form validation can require the server

The core uses the standard library alone. JavaScript regexp and date/URI format
semantics cannot be reproduced by a small Rust substitute. The validation result
names the affected field keys explicitly, and the HTTP API remains authoritative.

### Why differential fixtures and not hand-written expectations

`tests/sidebar_parity.rs` replays scenarios whose expected results come from
the TypeScript reference (`packages/ui/src/lib/native/sidebarParityFixtures.ts`).
Hand-written expectations would encode this port's reading of the reference
instead of the reference itself. The TypeScript test fails when the checked-in
fixture is stale, so a reference change surfaces in both languages.

### Why byte order and not `localeCompare` for ID tie-breaks

The reference breaks ownership and ordering ties with `localeCompare`, whose
result depends on the JavaScript engine's locale data. IDs are ASCII and the
fixtures cover the tie cases, where byte order and `localeCompare` agree. Byte
order keeps the core free of locale tables.

### Why borrowed views and not owned copies in projections

Ownership, Chats and row projections borrow `SessionRecord`s from the caller's
slice. A projection over 23,010 sessions then allocates only its indexes, and
the borrow checker prevents a projection from outliving the records it
describes.

### Why pins come from the record overlay

The reference matches pins by a runtime, directory and session key built from
the session's own metadata. The core reads `SessionOverlay::pinned_at` from the
same record, which yields the same membership without rebuilding the key set.
