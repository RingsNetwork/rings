# Rings logo assets

The geometric construction is the Rings logo. `rings.svg` and `rings-white.svg`
contain the same coordinates with mathematically derived light- and
dark-background palettes. `rings-spec.json` records the inputs, derived values,
OKLCH coordinates, and measured contrast ratios.

Do not edit generated assets by hand. The source of truth is the dependency-free
`rings-logo` crate:

```sh
cargo run -p rings-logo -- generate
cargo run -p rings-logo -- check
```

## Construction

- One module, `m = 10`, determines every length.
- The 30-tooth involute gear uses pressure angle `20deg`, pitch radius `mz/2`,
  addendum `m`, and dedendum `1.25m`.
- Five bores lie on a regular pentagon. Each 72-degree sector therefore spans
  exactly six teeth. Aperture, bore orbit, bore radius, flank sample count, and
  canvas margin derive from the tooth count, bore count, and `m`; they are not
  additional fitted constants.
- The central R is derived only from the aperture radius `a`, the pentagon's
  golden ratio `phi`, and its 72-degree sector: height `a/phi`, stem axis
  `-a/2`, semicircular bowl radius `a/(2phi)`, stroke `a/5`, and leg angle
  `3/4 * 72deg = 54deg`. Its final leg coordinate is the resulting line
  intersection, not a selected point.
- Palette hues are derived from the pentagon: rust is `72deg/2 = 36deg`; signal
  cyan is its 180-degree complement. Chroma derives from tooth and bore counts.
  Lightness is solved on a `1/10000` OKLCH grid against WCAG contrast targets of
  7:1, 4.5:1, and 3:1 on exact white or black.

The gear motif originates in the
[Rust logo artwork](https://github.com/rust-lang/rust-artwork/tree/main/logo),
which is distributed under the Creative Commons Attribution license. The Rings
construction is independently regenerated as an involute gear and replaces the
center letterform with the calculated Rings R. This project is not endorsed by
or affiliated with the Rust Foundation.

“Rust” and the Rust logo are trademarks of the Rust Foundation.
