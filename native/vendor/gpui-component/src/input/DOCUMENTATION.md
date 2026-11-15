# Styled input accessibility

## Ownership

The styled input resolves accessible names, content-type roles, placeholder
fallbacks and secret-value policy. It passes these to the gpui-base editor using
`InputAccessibility`. The editor owns the accessible input node and its actions.

## Invariants

- The outer InputBase frame is presentational. Its separate focus handle still
  tracks focus inside suffix controls for styling and dispatch.
- Input, Textarea and Editor share the same metadata forwarding path. Explicit
  role overrides and accessible IDs apply to the actual editor focus node.
- Password and new-password content types remain secret when the user turns
  off the visual mask. Mask-pattern placeholders do not become fallback labels.

## Decisions

### Why the frame has no input role

The editor's keyboard focus handle determines accessible focus. Giving the
frame another input role creates a second input and leaves the focused editor
unnamed. Keeping the frame presentational exposes one editable control while
preserving prefix, suffix and context-menu behavior.
