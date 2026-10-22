# Native desktop crate

`openchamber-native-desktop` is the GPUI Kit client. `main` initializes
diagnostics, assets, locale and theme, opens the window and hands application
lifetime to the `Workspace` entity. The first window runs on deterministic
synthetic data from the core and makes no network or provider calls.

## Ownership

- `workspace.rs` owns the application state of the offline client: the
  synthetic records, lifecycle ordering, sidebar presentation state, the
  selected session, one retained transcript and `ListState` per visited
  session, drafts, scroll memory and playback tasks. Views read it; actions
  mutate it through its methods.
- `sidebar_item.rs` copies the core row model into owned rows. The core model
  borrows session records and entities cannot hold borrows.
- `sidebar_view.rs`, `transcript_view.rs`, `composer_view.rs` and
  `workspace_view.rs` render. They dispatch to `Workspace` methods and never
  interpret records themselves.
- `theme.rs`, `locale.rs` and `assets.rs` load the generated resources in
  `resources/`; `bun run native:resources` owns their content.
- `diagnostics.rs` records timing markers without message or title text.

## Invariants

- Pointer, keyboard and accessibility activation reach the same `Workspace`
  method: rows use `on_click`, which GPUI also registers as the AccessKit Click
  action.
- Switching sessions saves the outgoing draft and scroll position under the
  identity captured when the outgoing session was selected, then restores the
  incoming ones before the next paint.
- A text delta remeasures only its own transcript row. The transcript follows
  the tail only while the reader is at the end.
- Dropping the `Workspace` drops every playback `Task`, which cancels its
  stream; closing the window during playback leaves no owned task running.
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

### Why `.test_support()` on rows and controls

GPUI Kit's headless test harness observes only elements marked with
`.test_support()`. The call is the identity in normal builds, so production
render chains keep their native types while `workspace_tests.rs` clicks rows
and presses keys through real hit testing.
