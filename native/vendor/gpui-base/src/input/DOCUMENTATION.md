# Input accessibility

## Ownership

The editing engine owns the accessible input node on `input-state`, which also
owns the keyboard focus handle and editing action handlers. Styled components
supply `InputAccessibility` presentation through `set_accessibility` during
render. The engine supplies default text-input semantics to unstyled callers.

## Invariants

- Each editor has one accessible input node attached to its existing keyboard
  focus handle. Frames and suffix controls keep their own dispatch nodes.
- Styled presentation carries the resolved role, label, placeholder and author
  ID. A missing role deliberately suppresses the editor's accessible node.
- Values come from the current rope only when accessibility is active or test
  support is enabled. A visual mask or secret content type suppresses the value.
- Focus refuses disabled inputs. SetValue checks current editability and uses
  `replace_all`, preserving normal validation, undo and change notifications.
- Non-secret inputs expose synthetic TextRun children and the active selection
  through AccessKit. Text runs use UTF-8 character lengths, keep CRLF atomic,
  preserve logical lines and link fragments of long visual lines. Empty documents
  retain an empty run so their Text interface remains available.
- Text snapshots are built only while accessibility is active. Selection actions
  validate node IDs, character indices, document revision and current text before using normal
  cursor movement. Readonly inputs allow selection; disabled or secret inputs
  reject it. Actions do not interrupt an in-progress IME composition.
  Run IDs include the document revision and byte range so positions from a
  replaced document or changed wrap segmentation stay invalid after redraw.
- Prepaint supplies TextRun bounds, character positions and advance widths from
  the same shaped layout used by cursor positioning. Coordinates include text
  alignment, continuation indentation, gutter width and scrolling. CRLF has one
  zero-width position at the end of its visual line.
  Bounds and character metrics are converted to physical window pixels, matching
  AccessKit's parent nodes at fractional display scales.
- Runs split at the current layout's soft-wrap boundaries. Same-line links join
  only fragments of the same visual line. Text outside the laid-out buffer lines
  retains logical navigation and selection but has no advertised geometry or
  soft-wrap boundaries. No estimated or zero-origin rectangle substitutes for
  missing geometry. Non-monotonic bidirectional runs also omit geometry.

## Decisions

### Why geometry is attached during prepaint

The editor's shaped layout is current during prepaint, before its parent emits
synthetic accessibility children. A frame-local shared snapshot receives those
coordinates and wrap boundaries. Reading paint-time `last_layout` during render
would publish the previous frame's positions after edits, resize or scrolling.

### Why semantics belong to the editor focus node

GPUI resolves accessible focus through the node tracking the focused handle.
An input role on an ancestor with a different handle leaves focus on the window
root. Moving semantics onto the existing editor node keeps keyboard dispatch
and suffix-control focus intact without tracking a handle twice.
