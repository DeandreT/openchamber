# Local AT-SPI state patches

## Ownership

This directory vendors the published `accesskit_atspi_common` 0.19.1 crate.
The native workspace selects it through `[patch.crates-io]`. Its source and
normalized Cargo manifest come from the registry release; the license texts
are included from the AccessKit repository because the crate archive omits them.
`Cargo.toml.orig` and `.cargo_vcs_info.json` retain the release provenance.

The local source changes cover expansion, disabled state and editable-entry
focus in `src/node.rs`, focus notifications in `src/adapter.rs`, and the
regression tests registered by `src/lib.rs`. The Unix adapter owns D-Bus
serialization and delivery; application controls own their AccessKit properties.

## Invariants

- An absent `expanded` property adds no expansion-related AT-SPI states.
- `Some(false)` exposes `Expandable` and `Collapsed`; `Some(true)` exposes
  `Expandable` and `Expanded`.
- The existing `notify_state_changes` state-set difference emits added and
  removed states, including clearing the property. Unchanged states emit nothing.
- State queries and cache records use the same mapping. Unrelated states remain
  intact. No dependency versions or public interfaces change for this patch.
- A disabled control whose role has no read-only support, such as a button or
  menu item, exposes neither `Enabled` nor `Sensitive`. Enabling or disabling it
  emits both state changes. Roles with read-only support keep the release's
  mapping: a read-only or disabled one exposes `ReadOnly` without `Enabled`.
- A named node with an AccessKit live setting emits one `Announcement` with its
  name and politeness when it is added or renamed. This is the release's
  behavior; the regression test guards the toast announcements that rely on it.
- An editable entry is the sole AT-SPI `Focused` node while its AccessKit active
  descendant identifies the highlighted option. The option is not focused.
  Highlight changes emit `ActiveDescendantChanged` on the entry without focus
  transitions. Host focus loss clears entry focus; text-selection notifications
  continue to come from the keyboard editor. Non-editable active-descendant
  controls and ordinary editors retain their original focus behavior.
- Tree updates compare the actual keyboard owner as well as resolved focus.
  Moving between an entry and its active descendant emits both focus-state
  transitions even when the consumer's resolved node remains unchanged. Updates
  with an unchanged owner emit no duplicate focus transitions.

Run the vendored regression tests from `native/` with
`cargo test --locked -p accesskit_atspi_common`. They are dependency tests, so
the native workspace's default test selection does not run them. Run
`cargo test --locked -p accesskit_unix` for the pinned Unix adapter tests.

## Decisions

### Why editable entries retain keyboard focus with an active descendant

AccessKit's consumer reports accessible focus on the active descendant. Editable
pickers need the named entry to remain the sole focus target so assistive clients
can identify the text editor. State queries, cache records and focus events use
that entry consistently; highlight changes use the entry's active-descendant
event. This translation is restricted to editable entries with a valid active
descendant and does not move keyboard focus.

### Why disabled controls drop `Enabled` instead of gaining `ReadOnly`

The release adds `Enabled` and `Sensitive` to every node outside the read-only
roles, so a disabled menu item reads as operable to Orca and automation. ATK and
AT-SPI clients treat a missing `Sensitive` as unavailable; `ReadOnly` would
describe an uneditable value, which a button does not have. Read-only roles keep
their existing states so text fields and checkboxes report what they did before.

### Why the native workspace vendors the translation crate

The pinned release drops the expanded property when producing AT-SPI states.
Patching its mapper fixes both queries and notifications for every consumer
without maintaining a second event path in the application.

Remove this directory and the workspace patch when the native dependency graph
resolves an upstream release with equivalent expansion, disabled-state and
editable-focus mapping. Verify initial states, every transition, property
clearing and unchanged updates against that release before removing the local
regression coverage.
