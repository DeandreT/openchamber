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

## Decisions

### Why an opt-in confirmation option

The startup project picker returns focus to the composer after choosing any
project, including the current one. The option exposes that behavior without
changing other callers' unchanged-selection or veto behavior.

Remove this patch when a pinned upstream release offers equivalent behavior
and the native pointer, keyboard and accessibility regression tests pass.
