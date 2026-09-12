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
  aperture (`L = sqrt(2)a`); stem edges are `-19u/6` and `-13u/6`; the outer
  bowl has center `(-u/2,-17u/8)` and radius `19u/8`, while the counter has
  center `(-5u/4,-17u/8)` and radius `17u/8`. Their common horizontal axis and
  rational eighth-module dimensions reconstruct the printed plate much more
  closely than the former pair of congruent, diagonally offset circles. Each
  serif circle has radius `2u/3`.
- The square's center-to-lower-right chord remains the exact 45-degree
  construction diagonal; it is not itself a black contour. A support circle
  through the square center `C` and lower-right corner `D`, with sagitta
  `u/10`, determines the long outer arc of the leg.
- The visible outer contour starts at the bowl point
  `B = (-u/2 + 3sqrt(2)u/4,0)`, not at `C`. Its root width to the inner start
  `S` is therefore exactly `3sqrt(2)u/4`, approximately `1.061u`, matching the
  plate's approximately one-module width. A derived rounding
  circle joins `B` tangentially to the long outer arc. If that arc has center
  `O` and radius `R`, the bowl has center `O_b` and radius `r_b`, and
  `n = (B-O_b)/r_b`, then the rounding radius is
  `r_o = (R^2-|B-O|^2)/(2(R+(B-O) dot n))`. Its center is `B+r_o*n`; the other
  tangency is `T = O+R(B+r_o*n-O)/|B+r_o*n-O|`. No visual join parameter is
  fitted independently.
- The inner contour begins at `S = (-u/2,0)`. Its root circle has center
  `(-u/2,13u/64)` and radius `13u/64`, meeting the crossbar at `S` and the line
  `x = 3y/4 - 29u/32` at `(-53u/80,13u/40)`. The line's direction is therefore
  the exact rational `3:4:5` triangle, not an assumed 45-degree parallel.
- The terminal circle has center `(21551u/4160,-13563u/16640)`, radius
  `17833u/3328`, and is tangent to that line at `(143u/160,12u/5)`. It passes
  through `D = (9u/2,9u/2)`, where both leg contours meet with zero width. The
  support circles are part of the model but are not drawn as decorative
  construction circles.
- The complete R is emitted as one compound SVG path. Component contours are
  solved independently but never rasterized as adjacent fill objects, so no
  internal shared edge can appear as a seam.
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
