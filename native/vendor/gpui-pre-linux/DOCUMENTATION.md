# Local X11 input recovery and preedit patch

## Ownership

This directory vendors `gpui-pre-linux` 0.3.9 from crates.io, with its original
package metadata and Apache-2.0 license. The patch changes the X11 consumer of
the local `zed-xim` decoder.

## Invariants

- A malformed XIM text callback logs its decoding error and leaves the XIM
  connection available for the next event. It emits no replacement text.
- Transport and protocol errors retain the existing connection teardown and
  ordinary-keyboard fallback behavior.
- The decoder owns text validation and synchronous protocol replies. The
  platform consumes its typed `ClientError::DecodeText` without treating it as
  an input-server crash.
- The XIM handler retains the server's preedit buffer. `chg_first` and
  `chg_length` count Unicode characters, including supplementary characters
  and combining marks separately. Each valid edit publishes the complete
  resulting preedit string to GPUI.
- Invalid ranges leave that buffer and the pending callback unchanged, log
  an error without text content, and keep the connection. Feedback-only
  updates leave text unchanged; rendering feedback and caret positioning
  retain the existing platform behavior.
- Start resets preedit state. Done and the platform's focus-cancellation
  reset clear any remaining uncommitted preedit. Active-preedit callbacks use
  the window recorded at Start or the first Draw. Done releases that owner; commits outside an
  active preedit use the current input window.
- Commit delivers text once and unmarks it. The protocol buffer remains
  available for a following cleanup Draw; an empty post-commit Draw does
  not edit the committed text. A nonempty remaining preedit starts marking
  the remaining composition.
- Empty preedit updates delete only an existing marked range. They cannot
  delete an ordinary selection after commit or focus movement.

## Decisions

### Why text errors keep the connection

The XIM decoder can reject one invalid callback while continuing the protocol.
Discarding its connection would turn one malformed string into loss of all
later composition in that window.

### Why preedit state survives commit until cleanup

IBus sends Commit before deleting the old preedit with a character-indexed
Draw and then Done. Keeping the server buffer allows that deletion to be
validated while keeping committed application text separate. XIM's
[callback semantics](https://www.x.org/releases/current/doc/libX11/libX11/libX11.html#Input_Method_Callback_Semantics)
also permit incremental insertion, replacement and deletion.

### Validation and removal

Run `python3 native/vendor/gpui-pre-linux/tests/run_xim_handler.py` from the
repository root after building native release dependencies. It compiles only
the actual callback module against existing artifacts, without rebuilding
GPUI or starting a display. Inline tests cover Unicode edits, invalid-range
recovery, cancellation, commit cleanup and window ownership. This focused
check does not compile the client/window integration.

Qualify the rebuilt application with IBus on an owned private display.
Clipboard probes must wait for the clipboard to change after sending the
shortcut. XIM forwards ordinary shortcuts asynchronously, so a fixed short
sleep can read the old clipboard before the copy event reaches the app.

Remove this patch when the pinned platform handles recoverable text errors,
incremental preedit and lifecycle cleanup equivalently, then rerun the
decoder and real private-display IME checks.
