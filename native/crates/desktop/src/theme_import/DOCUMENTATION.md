# Native theme conversion

## Ownership

This module converts standalone VS Code JSONC files to compact server theme
definitions and validates and resolves OpenChamber definitions. It ports
`packages/ui/src/lib/theme/{color,definition,syntax,readableColors}.ts`, the
`vscode/{import,adapter,adapt}.ts` pipeline and `agentColorPalette.ts`.
The Appearance owner handles file selection, catalog source resolution, server
publication, deletion, runtime ownership and theme application.

## Invariants

- File imports enforce the canonical 512 KiB UTF-8 limit and error codes.
  Includes and external token files fail explicitly until the caller resolves
  their package sources, as in the shared catalog path.
- Definitions retain authored colors and optional overrides. Resolution applies
  semantic defaults and syntax inheritance, then computes readable colors and
  the agent palette. Missing optional component colors remain absent in the flat
  result, matching native resource generation.
- Arithmetic uses the canonical hex/numeric RGB parser, alpha composition,
  16-step searches and OKLab coefficients. Legal CSS colors outside that parser
  remain strings, with the same `color-mix` fallbacks as the web resolver.
  `resolve_css_color` separately converts final paint values to native sRGB.
  A resolved definition is not a guarantee that the native painter supports all
  of its CSS values. The consumer reports unsupported paint values without
  substituting a different palette.
- Conversion is pure. Invalid input writes no file and changes no selected theme.
- `fixtures.json` is generated from the current TypeScript pipeline, with SHA-256
  source identities. Tests compare complete compact definitions, flattened
  resolved colors and agent palette order. Fixtures cover every built-in and
  synthetic import, validation, alpha, syntax, high-contrast and legacy CSS cases.
- `catalog_base_name` implements the catalog's font-style edition identity.
  Package ordering, appearance-scoped deduplication and invalid variant retention
  remain with the catalog caller.

## Final paint boundary

`resolve_css_color(value, dark, current_color)` returns unpremultiplied sRGB
channels and alpha in `[0, 1]`. It supports hex, all CSS named colors,
`transparent`, contextual `currentColor`, numeric and percentage RGB/RGBA,
HSL/HSLA, HWB, Lab/LCH, OKLab/OKLCH, `light-dark`, and `color` in sRGB,
linear sRGB, Display P3, Adobe RGB, ProPhoto RGB, Rec. 2020 and D50/D65 XYZ.
`color-mix` supports these interpolation spaces, premultiplied alpha, percentage
normalization and the four polar hue directions. Intermediate channels stay
extended; final sRGB output clips out-of-gamut channels.

Modern HSL/HWB components accept numbers on the same 0 to 100 scale as
percentages. Legacy comma-separated HSL requires percentages. HWB preserves
nonnegative whiteness and blackness above 100% until their sum is normalized.
Each `color-mix` color accepts one optional percentage before or after it.

Modern color functions accept `none` for alpha. It paints as zero outside a mix
and takes the other endpoint's alpha during interpolation. When both endpoints
have missing alpha, the result retains its interpolated color and missing alpha
for an enclosing mix. Legacy comma syntax rejects `none`.

Inherited foregrounds use `css_color_context`, which keeps their computed CSS
metadata for `currentColor`. Converting the foreground to RGBA first would turn
missing alpha into numeric zero before a token's interpolation. Only the final
token goes through `resolve_css_color` to produce native paint channels.

The following canonical-schema-accepted CSS semantics still return explicit
`PaintError::Unsupported` and remain parity gaps: relative colors, color math
such as `calc`, missing `none` color channels, system colors and CSS cascade
keywords, unknown/custom color spaces and nesting beyond 32 levels.
`currentColor` requires the consumer's inherited foreground. A browser may also
reject values that pass the deliberately permissive canonical schema, such as
five-digit hex; the native painter rejects these too.

Separate browser probes also show paint mismatches for translucent out-of-gamut
colors such as `color(display-p3 1 0 0 / 0.5)`. This remains an open qualification
gap. The retained `color-mix(in hsl, lch(60 0 40 / 0.7), #6699cc)` mismatch has a
focused regression against Chrome 157 paint over black and white in both schemes.
This case does not establish general cross-space or wide-gamut parity.

`css_opacity` exposes canonical `withOpacity` for consumer component defaults.
Hex and numeric RGB replace their alpha; unmeasured CSS values use `color-mix`
and preserve their existing alpha through mixing.

`paint-fixtures.json` records an owned Chrome instance's DOM-computed colors
painted over black and white in an sRGB canvas, in both color schemes. It
includes browser identity and the reference document's SHA-256. Explicitly
unsupported cases remain marked as such and do not count as paint parity passes.
The byte comparison allows one channel level for browser painting quantization.
`currentcolor-fixtures.json` separately records Chrome's computed foreground and
token styles for same-element and inherited background, border and text colors.
It distinguishes foreground alpha `none` from numeric zero through real custom
properties and CSS inheritance. Its owned probe script is retained with the
central qualification evidence under `theme-converter/missing-alpha-v2`.

## JSON representation limits

The Rust JSON boundary rejects unpaired UTF-16 surrogate escapes, non-finite
number results such as `1e400`, and documents beyond serde_json's default
recursion limit. JavaScript `JSON.parse` can accept these representations, even
in fields the canonical schema later discards. Filename truncation that would
split a surrogate pair also returns `invalid`, rather than replacing the name's
character. These are explicit input parity gaps; ordinary valid Unicode JSON
and the canonical fixture cases retain their complete values.

## Decisions

### Why reuse generated default colors and not copy a fallback palette

The VS Code adapter starts from the canonical OpenChamber light or dark default.
Reading those defaults from the bundled `resources/themes.json` keeps import
fallbacks aligned with the native resource generation already used by the app.

### Why preserve CSS strings and not normalize all colors to hex

The canonical definition schema accepts CSS functions and keywords. Its color
arithmetic deliberately preserves values it cannot measure. Normalizing at this
boundary would change both stored definitions and the canonical resolved output.

### Why final paint conversion is separate from import arithmetic

The canonical color helper measures only hex and numeric RGB. Expanding that
helper to understand CSS functions would change saved imports and readable text
selection. Final painting resolves the preserved CSS after those decisions.

### Why missing alpha remains separate from zero alpha

A numeric zero participates in premultiplied interpolation as transparent. A
missing alpha takes the other color's alpha. Keeping its marker through nested
mixes preserves the color that an enclosing mix can make visible.

### Why Lab conversion uses Chromium's rounded D50 constants

Chromium uses a rounded Lab reference white and Skia's ICC 16.16 sRGB D50 matrix.
Their small neutral-axis offset determines the converted hue of an achromatic
LCH endpoint in HSL interpolation. Using the higher-precision CSS sample matrices
changes the retained browser paint by 21 green channel levels. Lab conversion in
both directions uses the same white and matrix; other conversion paths retain
their existing coefficients. No achromatic tolerance is increased.

The source reference is Chromium `0835bd4f05c3b3f6479fa98a6223ebaf13f3d1af`,
`ui/gfx/color_conversions.cc`, and its pinned Skia
`bfa7ff9b8db9eefbfa2a45d15b620592a9f36f59`, `include/core/SkColorSpace.h`.
The source snapshot explains the retained Chrome 157 behavior; the browser probe
records its major version but does not identify its exact source revision.

## Fixture regeneration

From the repository root, run Bun on
`native/crates/desktop/src/theme_import/generate-fixtures.ts`. When the checkout
has no installed dependencies, `THEME_FIXTURE_DEPENDENCIES` can name an existing
`.bun` directory containing Zod 4.3.6 and strip-json-comments 5.0.3. The script
always imports theme source from the current checkout. It performs no install.

For paint fixtures, run Node on `generate-paint-fixtures.mjs` with a Chrome
executable and a fresh task-owned local staging directory. It opens an isolated
headless profile over a private CDP pipe, evaluates synthetic CSS and closes its
browser. Its staging profile and HTML are disposable after evidence retention.
