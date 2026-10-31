# Offline libghostty-vt build

## Ownership

This directory vendors `libghostty-vt-sys` 0.2.0 from crates.io (MIT OR
Apache-2.0, `github.com/uzaaft/libghostty-rs`). `src/` is the published crate
unchanged: its checked-in bindings were generated from C headers identical to
Ghostty `9f62873bf195e4d8a762d768a1405a5f2f7b1697`, the revision the web
terminal runs as WebAssembly (`packages/ui/src/lib/ghostty/vendor/VERSION`).
The manifest drops the bindgen tool and the pkg-config path, and `build.rs` is
rewritten. The safe `libghostty-vt` crate stays the published 0.2.0 and is
pinned with `=` in the workspace, so the two halves always match.

`native/scripts/fetch-ghostty.py` fills the build's inputs; the desktop's
`terminal_emulator.rs` is the only consumer.

## Invariants

- The build never runs Git and never reaches the network. It reads the Ghostty
  source tarball and Zig's package cache from `native/.cache/ghostty`, or the
  directory `OPENCHAMBER_GHOSTTY_CACHE` names.
- The tarball's SHA-256 must equal `GHOSTTY_TARBALL_SHA256`; any other file
  fails the build. The fetch script checks the same digest, and it checks every
  Zig package against the hash in Ghostty's `build.zig.zon.json` (Zig names a
  fetched package by the hash of its contents).
- Existing package directories are rehashed by both the fetch script and the
  build using `zig fetch <local-directory>` into a temporary cache. Package
  IDs include dotted semantic versions as well as opaque hash-only names.
  Every listed directory must exist and its recomputed ID must match exactly.
  A corrupt directory must be removed explicitly before fetching it again.
  Local-directory verification performs no network request.
- Zig is exactly 0.15.2, from `ZIG` or else `zig` on `PATH`. Another version
  fails with the version found.
- Zig always builds `ReleaseFast` with `-Dcpu=baseline`, whatever the Cargo
  profile, and links the static archive.
- `kitty-graphics` and `link-dynamic` exist only because the safe crate
  forwards them; enabling either fails the build. Depend on `libghostty-vt`
  with `default-features = false`.
- The pin moves only together with the web build's pin, and only to a revision
  whose headers match these bindings, or with regenerated bindings.

## Decisions

### Why a vendored -sys crate and not crate 0.2.2

0.2.2 builds Ghostty `a887df42`, which answers OSC 10/11/4 color queries
itself. The server already answers them, so the shell would get two replies.
Its build script also clones Ghostty from GitHub into every fresh `OUT_DIR`, so
an offline build fails. 0.2.0's bindings match the reference pin exactly.

### Why always ReleaseFast

The published build script picks Zig's Debug mode whenever Cargo's `DEBUG` is
true, which includes the dev profile and nextest. A Debug Ghostty spends
milliseconds per line of output: a 20,000-line scrollback test timed out at
two minutes, where ReleaseFast takes tens of milliseconds.

### Why `-Dcpu=baseline`

Without it Zig targets the build machine's CPU features (AVX2 on the machine
that measured this), so a binary built there can fault on an older CPU.

### Why a fetched cache and not committed archives

The source tarball and the 36 Zig packages are about 600 MB, and a prebuilt
archive per platform would put a binary on the supply chain. The fetch runs
once per cache directory and is checked by digest; builds after it are
offline.

### Why the digest is computed in build.rs

A SHA-256 crate would be a new build dependency in the lock file for one
check. The build script hashes the file itself, with no external tool.

## Validation and removal

```sh
python3 native/scripts/fetch-ghostty.py
ZIG=~/.local/share/tooling/zig/0.15.2/zig cargo build --locked -p libghostty-vt
```

A fresh target directory then builds inside `unshare -rn`, with no network
at all. The emulator tests in
`crates/desktop/src/terminal_emulator.rs` cover replay isolation, theme-query
silence, keys, paste, scrollback and selection against this build.

Remove the vendored crate once a published `libghostty-vt-sys` builds the
pinned revision from a local, digest-checked source with an optimized Zig
build in every profile.
