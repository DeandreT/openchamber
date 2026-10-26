# List accessibility

## Ownership

ListState owns keyboard-highlighted row identity and binds named rows to the
query editor's accessible focus. SearchableListAdapter supplies item titles.
GPUI resolves the relation to AccessKit nodes at the end of each frame.

## Invariants

- Searchable lists retain keyboard focus on the actual query editor. Only the
  selected, rendered row with an accessibility label calls
  `aria_active_descendant_for` with that editor's focus handle.
- Non-searchable lists retain the ancestor-based `aria_active_descendant` path.
- Empty results, hidden initial/loading content and cleared selections produce
  no row claim. No cached relationship survives a frame without its row.
- Filtering, arrow navigation, confirmation and cancellation use the existing
  selected index and action dispatch. Accessibility does not move keyboard focus.

## Decisions

### Why searchable rows name the editor explicitly

The query editor and virtualized rows occupy sibling branches. GPUI's original
active-descendant marker requires a focused ancestor, so searchable rows use an
explicit focus-handle relationship. The bridge publishes the standard AccessKit
relation without copying the option name into the editor or adding another input.
