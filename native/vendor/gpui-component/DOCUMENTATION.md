# Local GPUI component patch

## Ownership

This directory vendors the published `gpui-component` 0.7.3 crate. Its
Apache-2.0 license remains in the package. Application controls use it through
the pinned `gpui-kit` dependency.

## Invariants

- `ComboboxState::confirm_on_reselect(true)` allows a single-select popup to
  confirm its current value and close without emitting a value-change event.
- The option defaults to false. Multiple selection and delegate rejection of
  a different value keep their existing behavior.
- Opted-in single selection opens with an empty query and the committed value
  highlighted, so Enter can repeat that choice.
- Both pointer/list confirmation and direct item selection apply the option.
- Combobox triggers expose their placeholder as their accessible name and their
  committed item titles as their value. Searchable list rows expose those titles
  and mark their highlighted row as the active descendant. Other list delegates
  opt into named active descendants through `accessibility_label`.
- A notification toast is named by its title, then its message, joined by a
  line break. Its first continuous accessible appearance is a polite live region
  until closing. Once an announced toast leaves the visible stack, it cannot become
  live again, including when it returns before its enter animation finishes.
  Closing toasts retain their name without a live setting. A toast with neither
  title nor message has no name and is not announced. Pointer, keyboard and
  dismissal behavior are unchanged.
- Comboboxes can refine their popup style and trigger gap independently, and
  searchable lists can hide the clear button. Defaults retain the package's
  existing appearance; the native project chooser supplies the web client's
  popup dimensions and controls.

## Decisions

### Why an opt-in confirmation option

The startup project picker returns focus to the composer after choosing any
project, including the current one. The option exposes that behavior without
changing other callers' unchanged-selection or veto behavior.

### Why toasts are polite live regions

The web toaster announces every toast, errors included, through one polite
`aria-live` region. The release's toast root has the `Alert` role but no name
or live setting, so nothing reaches a screen reader. Setting both on the toast
uses the adapter's existing announcement path; no second status element or
platform-specific event is needed. The live setting is written through the
element's own accessibility node because GPUI 0.3.9 has no live-region builder.

The adapter announces a named live node each time the node is added. A full
stack renders only its newest `max_items` toasts, so an older one can leave and
return before its enter transition finishes. Each notification records whether
it has contributed an accessible node and subsequently left the visible stack.
That second appearance stays silent regardless of animation timing. A toast
hidden before its first accessible frame remains eligible to announce, and a
replacement notification starts with its own announcement state.

Remove this patch when a pinned upstream release offers equivalent behavior
and the native pointer, keyboard and accessibility regression tests pass. The
toast announcement tests are in `vendor/gpui-base/input-tests`
(`notification_announcement`).
