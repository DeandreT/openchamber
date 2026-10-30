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

- `send_config.rs` ports default agent/model selection, an existing session's selection from its record, catalog grouping, composer labels and agent color allocation from the web client.
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
- `provider_logo.rs` ports the provider logo lookup of `useProviderLogo.ts`
  (bundled candidates, then models.dev) and inverts an SVG's paint colors for
  dark themes, as the web's `dark:invert` does.
- `session_actions.rs` decides a session row's menu (the ported items, their
  order and when each is enabled), which sessions archive, delete and
  restore reach (`sessionLineage.ts` policies), `isRestorableOnItsOwn`, the
  archive page's list and the pending pin overlay. Track and Done belong to
  root work rows outside archives and Chats; `SidebarRow::Session::owns_work`
  is false inside worktree and space works.
- `ownership.rs` and `chat_projection.rs` port `sessionOwnership.ts` and
  `sessionChatProjection.ts`. `sidebar_rows.rs` builds the normal Works view
   of `sessionSidebarRowModel.ts`, grouping worktree, space and session works under
  projects, with the section gaps of
  `sessionSidebarVirtualization.ts`; `grouping.rs` ports
  `buildGroupedSessions` and `buildSidebarChatGroup`; `ordering.rs` ports
  `session-ordering.ts`; `relative_time.rs` ports the sidebar's compact ages.
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
- `draft.rs` and `scroll_memory.rs` hold client-owned state keyed by the
  runtime and the session or draft identity captured when it was created.
- `synthetic.rs` produces the documented offline workloads. It is part of the
  core so the desktop, tests and measurement tools share one definition.

The desktop crate owns timers, focus, text layout and windows. The transport
crate owns wire parsing and authority checks, and hands the core records and
`TranscriptEvent`s.

## Invariants

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
- Chats keeps unowned root conversations and their active descendants. Missing,
  archived and `/btw` parents never promote children; rootless cycles are never
  reached.
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
- A tool result a history page left out (`PartContent::Tool::elided`) never
  replaces the whole result a transcript holds for the same call and status,
  through `replace` or `push`. A live transition with output, or the whole
  message, completes it.
- Lifecycle order changes only on live phase transitions or authoritative
  snapshots. Token arrivals and metadata writes never move a row.
- A failed load keeps its retained snapshot; only `LoadState::Complete` grants
  full-scope authority.
- A non-empty draft leaves the store only by an explicit send or clear.
- Default selections preserve configured identifiers through catalog gaps. Only
  remembered and automatic fallback choices skip models hidden in the picker.
- Agent color allocation consumes a theme-prepared ordered palette. Its indices
  refer to keyword, type, function, number, string, operator and variable;
  configured colors accept six-digit hex except the migration placeholder.
- Synthetic workloads are deterministic: the same size yields the same records
  and text.

## Decisions

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
