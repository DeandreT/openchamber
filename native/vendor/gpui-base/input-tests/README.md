# Input accessibility tests

Run from the repository root:

```sh
cargo test --offline --manifest-path native/vendor/gpui-base/input-tests/Cargo.toml --test input_accessibility
cargo test --offline --manifest-path native/vendor/gpui-base/input-tests/Cargo.toml --test active_descendant
cargo test --offline --manifest-path native/vendor/gpui-base/input-tests/Cargo.toml --test editable_text
```

These tests render the local gpui-base and gpui-component inputs on GPUI's
in-memory test platform. The existing `test_support` observer reads accessible
roles, names and values from the rendered elements and tracks their real focus
handles. It also checks keyboard editing and the SetValue editability guard.

The active-descendant tests activate GPUI's real tree builder through its
test-support API, without connecting to a platform service. They inspect the
AccessKit relation emitted by searchable lists, arrow navigation, empty results,
cleared selections, lost editor focus and sibling render order. The standalone
package patches GPUI to the local source so the new element API is exercised.

These tests do not exercise AT-SPI delivery or Orca speech. Qualify those with
the native app on an isolated display.

Editable-text tests inspect the real emitted TextRun nodes, Unicode and CRLF
character lengths, long-line links, empty text, selection direction, platform
selection actions, text updates and secret-value suppression. They compare
character positions with the editor's fresh prepaint caret coordinates across
soft wraps and Unicode offsets, check selection after resize, reject obsolete
run IDs, and verify that scrolling replaces geometry without assigning bounds
to unlaid-out ranges. These coordinates come from GPUI's in-memory layout test
platform; platform font shaping and AT-SPI extents still require native testing.
