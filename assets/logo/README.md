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
- The central R follows Luca Pacioli's construction grammar: a mother square of
  side `L`, divided into nine modules `u = L/9`; a dominant stroke `u`; a fine
  stroke `u/2 = L/18`; R derived from B; paired circular contours; and bracketed
  serifs located by tangent circles. This is a modular, high-contrast serif
  letter rather than a constant-width stroked symbol.
- Pacioli's surviving plate does not specify enough measurements to reproduce
  every contour uniquely. The generator therefore names its deterministic
  choices the **Rings completion**: the mother square is inscribed in the
  aperture (`L = sqrt(2)a`); stem edges are `-19u/6` and `-13u/6`; the paired
  bowl circles have radius `5u/2` and centers `(-u/2,-2u)` and
  `(-3u/2,-3u/2)`; each serif circle has radius `2u/3`.
- The leg is not assigned an arbitrary angle or rounded with added tangent
  circles. As in the plate, it occupies the region between the mother-square
  diagonal from the center to the lower-right corner and one exact circular
  arc with the same endpoints. The arc is a quarter circle centered at the
  square's right midpoint, so both boundaries resolve at the corner without a
  fitted tip or root.
- Stroke hierarchy is `30:5:1`, directly matching teeth, bores, and one
  construction unit. With `w_R = u = sqrt(2)a/9` and six teeth per bore sector,
  the gear and bore outlines are `w_G = w_R/6`; construction guides are
  `w_C = w_G/5`.
- Palette hues are derived from the pentagon: rust is `72deg/2 = 36deg`; signal
  cyan is its 180-degree complement. Chroma derives from tooth and bore counts.
  Lightness is solved on a `1/10000` OKLCH grid against WCAG contrast targets of
  7:1, 4.5:1, and 3:1 on exact white or black.

The historical interpretation is based on Pacioli's 1509 alphabet plate and
modern geometric analysis of its modular construction. The repository makes a
strict distinction between that evidence and the Rings completion recorded in
`rings-spec.json`; changing a completion parameter changes generated assets and
their invariant tests.

- [Pacioli, *De divina proportione* (1509), Metropolitan Museum of Art](https://www.metmuseum.org/art/collection/search/336656)
- [Geometric analysis of Pacioli's alphabet, Politecnico di Milano](https://re.public.polimi.it/handle/11311/1061101)

The gear motif originates in the
[Rust logo artwork](https://github.com/rust-lang/rust-artwork/tree/main/logo),
which is distributed under the Creative Commons Attribution license. The Rings
construction is independently regenerated as an involute gear and replaces the
center letterform with the calculated Rings R. This project is not endorsed by
or affiliated with the Rust Foundation.

“Rust” and the Rust logo are trademarks of the Rust Foundation.
