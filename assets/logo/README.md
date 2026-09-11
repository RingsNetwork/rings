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
- The central R is an outlined shape, not a centerline with an SVG stroke. Its
  em square has side `s = 2a/phi`, so all four corners remain inside the
  aperture. Its major stroke is `w_R = s/9`. The stem occupies
  `[-s/2+w_R, -s/2+2w_R]`.
- The bowl is the difference of two circles centered at `(0, -2s/9)`, with
  radii `5s/18` and `s/6`. Their radial difference is exactly `w_R`; their
  intersections with `-s/2`, `-7s/18`, `-s/18`, and `s/18` determine the top
  and middle bars.
- The tail chord joins the square center to its lower-right corner, fixing its
  angle at 45 degrees. Its curved boundary is a circular arc with sagitta
  `w_R` and radius `chord^2/(8w_R) + w_R/2`, so it tapers to the corner without
  a fitted Bezier control point.
- Stroke hierarchy is `30:5:1`, directly matching teeth, bores, and one
  construction unit. With six teeth per bore sector, the gear and bore outlines
  are `w_G = w_R/6`; construction guides are `w_C = w_G/5`.
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

The R takes its square-and-circle premise and `1:9` major-stroke proportion
from Luca Pacioli's 1509
[*De divina proportione* construction](https://commons.wikimedia.org/wiki/File:Luca_Pacioli,_De_divina_proportione,_Letter_R.jpg).
It is a new Rings construction rather than a tracing of the historical glyph.
