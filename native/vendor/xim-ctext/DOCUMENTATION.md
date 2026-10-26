# COMPOUND_TEXT decoder patch

## Ownership

This is `xim-ctext` 0.3.0 from crates.io, used by the native X11 XIM client.
It owns conversion of COMPOUND_TEXT bytes into Unicode. The sibling
[`zed-xim` patch](../zed-xim/DOCUMENTATION.md) owns protocol error handling.

The source archive SHA-256 is
`2ac61a7062c40f3c37b6e82eeeef835d5cc7824b632a72784a89b3963c33284c`.
`Cargo.toml.orig` and `.cargo_vcs_info.json` retain upstream provenance.
The upstream MIT license is included from the repository revision named in
`.cargo_vcs_info.json`; the published crate omits that file. The unused binary,
registry marker and nested lockfile are omitted.

## Invariants

- GL and GR designations are independent. Initial text is ASCII and Latin-1,
  as required by the [COMPOUND_TEXT specification](https://www.x.org/releases/current/doc/xorg-docs/ctext/ctext.html).
- GB2312 accepts its assigned cells only. Its middle-dot and horizontal-bar
  mappings differ from GBK and are preserved explicitly.
- UTF-8 extension segments accept the return sequence or end of input and
  preserve every character. Short segments cannot underflow.
- Truncated, mixed-width and undecodable input returns an error. A failed
  decode returns no partial text and inserts no replacement characters.
- Supported designations are ASCII, Latin-1, GB2312, JIS X0208 and the UTF-8
  extension. Other designations, extended segments and direction controls
  return errors. This is not a complete implementation of all X11 encodings.

## Decisions

### Why keep decoding in this crate

XIM supplies encoded preedit, reset and commit text before application input
handling. Decoding here gives each callback the same strict conversion and
preserves the crate's `no_std` compatibility. The existing `encoding_rs`
dependency supplies mapping tables.

### Removal

Remove this directory and its Cargo patch when the pinned dependency handles
these encodings and malformed sequences with equivalent tests. Run
`cargo test --offline -p xim-ctext -p zed-xim --lib` from `native/`, then qualify
IBus/libpinyin preedit and commit on an owned private X11 display. The tests
include synthetic IBus wire bytes, independent GL/GR transitions, Unicode
segments and invalid-input recovery.
