# Local Linux input and clipboard patches

## Ownership

This directory vendors `gpui-pre-linux` 0.3.9 from crates.io, with its original
package metadata and Apache-2.0 license. The patches change the X11 consumer of
the local `zed-xim` decoder and the X11/Wayland clipboard readers. The platform
clipboard boundary decodes copied file paths; the application owns attachment
validation and file content reads.

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
- Only an advertised `text/uri-list` or `x-special/gnome-copied-files` clipboard
  format produces `ClipboardEntry::ExternalPaths`. Plain text paths and URLs
  remain text. File lists take precedence over image previews and filename text.
  When both file-list formats exist, `text/uri-list` takes precedence.
- The shared clipboard parser uses the existing `url::Url` file-path converter.
  It accepts URI comments, LF/CRLF, percent-encoded names and Unix non-UTF-8 path
  bytes. Paths must be absolute and local; remote authorities, other schemes,
  malformed escapes, query/fragment suffixes and NUL paths are skipped.
- Invalid individual URI entries leave valid siblings in input order. Invalid
  payload UTF-8, an invalid GNOME operation, an empty usable list or an exceeded
  metadata bound rejects the list. X11 returns a conversion error and Wayland
  returns `None`; neither falls back to the file manager's filename text.
- File-list metadata is bounded to 16 MiB during transport and parsing, with at
  most 4096 nonempty, non-comment URI entries. Invalid entries count toward that
  entry bound. Exceeding either bound rejects the whole payload without
  truncation. These bounds do not limit individual file sizes.
- GNOME `copy` and `cut` both expose paths without moving, deleting or reading
  the selected files. Clipboard writers retain their existing behavior.
- PRIMARY remains the separate selection clipboard and keeps its existing
  text/image handling. Internal-item metadata and offer cache invalidation stay
  with the existing clipboard owners. Ordinary image/text precedence is unchanged
  when no file-list format is advertised.

- An X11 `FocusOut` whose detail is `NotifyInferior` keeps the window active.
  Keyboard focus moved into one of its child windows, such as an embedded
  WebKit page; the window's own keyboard, compose and IME state still resets
  as for any focus loss. Every other `FocusOut` deactivates the window.

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

### Why file MIME presence controls paste interpretation

File managers also advertise selected filenames as text and may offer image
previews. The file-list MIME is the authoritative indication of copied files.
Rejecting an unusable advertised list avoids inserting its filename fallback as
message text. X11's legacy conversion fallback skips file-list targets, so a
text-only owner receives no additional synchronous conversion requests.

### Why clipboard decoding shares the existing URL converter

Both native drop readers in `src/linux/{x11,wayland}/client.rs` decode UTF-8 URI
lists with `Url::parse`, `to_file_path` and per-entry filtering. The shared
clipboard helper follows that failure isolation and uses the same converter for
percent decoding and Unix path bytes. Its input checks reject URL parser repairs
that could otherwise manufacture an absolute path from `file:relative` or strip
raw whitespace from a filename.

### Why file-list metadata has transport bounds

Clipboard content comes from another process. Limiting the parser alone would
still allow an unbounded Wayland pipe or X11 INCR transfer to allocate before
parsing. The 16 MiB aggregate metadata budget bounds those reads, while the 4096
entry limit matches the native application's admission bound. The existing
general-purpose image/text readers and clipboard timeouts remain unchanged.

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

Inline tests in `clipboard_files` cover both MIME types, local/remote mixtures,
malformed and empty lists, URI comments, Unicode, non-UTF-8 Unix path round trips
and both metadata bounds. Clipboard module tests cover Wayland MIME selection
and X11 property type, encoding, partial replies and cumulative INCR limits.
Runtime qualification uses external clipboard owners on owned private X11 and
Wayland displays. It covers file-list precedence, unusable-list failure, INCR or
pipe transfer, ordinary image/text paste, PRIMARY separation, internal metadata,
and changing clipboard owners or offers between reads.

### Why focus inside a child window keeps the window active

The desktop hosts web pages as X11 child windows of the GPUI window. A click in
the page moves X focus to the child, and the parent receives `FocusOut` with
detail `NotifyInferior`. Treating that as deactivation drew the whole window as
inactive while the user typed in the page, which a browser view in Electron
never does.
