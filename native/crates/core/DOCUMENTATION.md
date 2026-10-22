# Native application core

`openchamber-native-core` owns the native desktop's application model: typed
identifiers, session records, display ownership, Chats membership, lifecycle
ordering, project grouping, the sidebar row projection, ordered transcripts,
drafts, scroll memory, load states and the synthetic workloads. It depends on
the standard library alone. JSON belongs to transport and fixture adapters;
rendering asks for a projection and dispatches an action.

## Ownership

- `ownership.rs` and `chat_projection.rs` port `sessionOwnership.ts` and
  `sessionChatProjection.ts`. `sidebar_rows.rs` ports the normal-mode Tree view
  of `sessionSidebarRowModel.ts`; `grouping.rs` ports `buildGroupedSessions`
  and `buildSidebarChatGroup`; `ordering.rs` ports `session-ordering.ts`.
- `transcript.rs` holds one session's messages in server order and reports the
  rows each mutation touched. User and assistant messages carry parts; every
  other OpenCode record (synthetic, system, skill, shell, compaction, switched
  agent, model or location, idle) is a notice with one line of text.
- `session_index.rs` ports the global session list of
  `useGlobalSessionsStore.ts` and `session-event-router.ts`: active and
  archived records, page merges, complete loads and live session changes.
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
- A tool result a history page left out (`PartContent::Tool::elided`) never
  replaces the whole result a transcript holds for the same call and status,
  through `replace` or `push`. A live transition with output, or the whole
  message, completes it.
- Lifecycle order changes only on live phase transitions or authoritative
  snapshots. Token arrivals and metadata writes never move a row.
- A failed load keeps its retained snapshot; only `LoadState::Complete` grants
  full-scope authority.
- A non-empty draft leaves the store only by an explicit send or clear.
- Synthetic workloads are deterministic: the same size yields the same records
  and text.

## Decisions

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
