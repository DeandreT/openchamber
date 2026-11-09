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
- `WindowExt::open_dialog_with_handle` returns a weak, root-scoped
  `DialogLayerHandle`. Targeted close removes exactly that opening; closed,
  replaced-root and cross-window handles change nothing. Removing a covered
  dialog preserves the top dialog's focus, input registration and selection,
  and bypasses the removed dialog in surviving focus-return chains.
- Handled dialogs keep that identity through their normal dismissal callbacks,
  including a callback that opens a newer modal. Targeted closes restore focus
  immediately when topmost. Imperative close invokes no dialog callbacks.
  Existing `open_dialog`, `close_dialog` and deferred top-close behavior remain
  unchanged for callers that do not request a handle.
- Kit dialog wrappers, Base hosts and focus traps use their retained focus
  identity for render keys. Visual stack offsets and draw priorities still use
  the current layer position, so closing a lower layer preserves keyed controls
  in the surviving dialogs.
- Deferred focus returns require their current cancellation token and the
  surviving target dialog. Opening another dialog consumes that return, and
  removing the target cancels it. An older timer cannot focus a removed layer
  or clear a newer pending return. Valid legacy returns keep their existing delay.
- `WindowExt::observe_modal_state` reports subsequent covered/uncovered
  transitions from the actual window's dialog and sheet state. It sends no
  initial callback, ignores unchanged coverage and toast notifications, and
  rejects notifications from a replaced root. The returned `Subscription` owns
  observation; dropping it stops callbacks. Close/open changes in one update
  deliver only the resulting authoritative coverage.
- Custom-rendered `PopupMenuItem::ElementItem` rows may supply an explicit
  `aria_label`. The menu-item role receives that name; visible content,
  shortcut hints, activation and disabled/checked behavior stay unchanged.
  Existing unlabeled custom items retain their default behavior.

## Decisions

### Why custom menu rows accept an explicit name

The menu-item role cannot derive a name from arbitrary rendered elements.
Callers already own their translated action labels, so an optional name keeps
custom shortcut layout intact without inferring text from descendants or
changing existing icon-only custom menus.

### Why targeted dialog handles belong to the window root

Native folder-trust admission can finish or reset under a newer modal. Its
handle pairs the owning root with the opening's retained focus identity, so
completion removes its own layer and cannot close another modal. Base's
`DialogHandle` controls open state without identifying a registered Kit layer.
The native admission tests exercise this contract through real Kit windows.

### Why modal coverage is observed on the root

Admission navigation must wait until every dialog and sheet releases interaction.
Observing the owning `WindowState` gives that continuation an authoritative
uncovered transition without polling or exposing the private modal stack.

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
