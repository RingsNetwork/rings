//! Pacioli-derived construction of the central R.
//!
//! The historical layer is the nine-part mother square, contrast between a
//! full module and a half module, and the construction of R by modifying B.
//! Parameters that the surviving plate does not specify completely are kept
//! in the explicit Rings completion layer below.

use crate::model::Point;

/// A circle used as a construction primitive.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Circle {
    /// Circle center.
    pub(crate) center: Point,
    /// Circle radius.
    pub(crate) radius: f64,
}

impl Circle {
    fn point_at_offset(self, x: f64, y: f64) -> Point {
        Point {
            x: self.center.x + x,
            y: self.center.y + y,
        }
    }
}

/// A stem bounded by two vertical lines in the mother square.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StemGeometry {
    /// Left edge of the dominant stroke.
    pub(crate) left: f64,
    /// Right edge of the dominant stroke.
    pub(crate) right: f64,
    /// Cap line.
    pub(crate) top: f64,
    /// Baseline.
    pub(crate) bottom: f64,
}

/// A circle with the two points where it touches orthogonal boundaries.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TangentCircle {
    /// Source circle.
    pub(crate) circle: Circle,
    /// Tangency on a vertical stem edge.
    pub(crate) stem_tangent: Point,
    /// Tangency on the cap or baseline.
    pub(crate) edge_tangent: Point,
}

/// Three tangent circles that construct the bracketed serifs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SerifGeometry {
    /// Upper-left bracket.
    pub(crate) top_left: TangentCircle,
    /// Lower-left bracket.
    pub(crate) bottom_left: TangentCircle,
    /// Lower-right bracket.
    pub(crate) bottom_inner: TangentCircle,
}

/// Paired circles inherited from Pacioli's B construction.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BowlGeometry {
    /// Outer contour circle.
    pub(crate) outer: Circle,
    /// Inner counter circle, sharing the outer circle's horizontal axis.
    pub(crate) inner: Circle,
    /// Top tangency of the outer contour.
    pub(crate) outer_top: Point,
    /// Top tangency of the counter.
    pub(crate) inner_top: Point,
    /// Waist point on the outer contour.
    pub(crate) outer_waist: Point,
    /// Waist point on the counter.
    pub(crate) inner_waist: Point,
}

/// Curved leg constructed around the center-to-corner diagonal.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LegGeometry {
    /// Circle supporting the long, shallow part of the outer contour.
    pub(crate) outer_circle: Circle,
    /// Sagitta of the supporting arc measured from the square diagonal.
    pub(crate) outer_sagitta: f64,
    /// Start of the supporting chord at the square center.
    pub(crate) diagonal_start: Point,
    /// Circle rounding the bowl continuously into the outer leg.
    pub(crate) outer_rounding_circle: Circle,
    /// Start of the visible outer contour on the bowl.
    pub(crate) outer_start: Point,
    /// Tangency between the rounding circle and long outer arc.
    pub(crate) outer_join: Point,
    /// End of the outer contour at the lower-right corner.
    pub(crate) outer_end: Point,
    /// Inner contour built from two tangent arcs and a 3:4 line.
    pub(crate) inner: InnerLegGeometry,
}

/// Inner leg contour joined continuously to the bowl and terminal corner.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InnerLegGeometry {
    /// Circle tangent to the crossbar at the root and to the straight segment.
    pub(crate) root_circle: Circle,
    /// Circle tangent to the straight segment and passing through the corner.
    pub(crate) tip_circle: Circle,
    /// Root on the lower edge of the crossbar.
    pub(crate) root: Point,
    /// Tangency between the root arc and straight segment.
    pub(crate) root_tangent: Point,
    /// Tangency between the straight segment and tip arc.
    pub(crate) tip_tangent: Point,
    /// Shared terminal point at the lower-right corner.
    pub(crate) tip: Point,
}

/// Complete rational construction of the central R.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphGeometry {
    /// Half-side of the mother square.
    pub(crate) square_half: f64,
    /// One ninth of the mother-square side.
    pub(crate) unit: f64,
    /// Full-module dominant stroke.
    pub(crate) stroke_width: f64,
    /// Half-module fine stroke.
    pub(crate) thin_stroke_width: f64,
    /// Vertical stem.
    pub(crate) stem: StemGeometry,
    /// Bracketed serif construction.
    pub(crate) serifs: SerifGeometry,
    /// Eccentric circular bowl.
    pub(crate) bowl: BowlGeometry,
    /// Curved, tapered leg.
    pub(crate) leg: LegGeometry,
}

impl GlyphGeometry {
    /// Derives the Pacioli mother square and Rings completion from the aperture.
    pub(crate) fn from_aperture(aperture_radius: f64) -> Self {
        let square_half = aperture_radius / 2.0_f64.sqrt();
        let unit = 2.0 * square_half / 9.0;
        let stem = stem(square_half, unit);
        let bowl = bowl(unit);
        Self {
            square_half,
            unit,
            stroke_width: unit,
            thin_stroke_width: unit / 2.0,
            stem,
            serifs: serifs(square_half, unit, stem),
            bowl,
            leg: leg(square_half, unit, bowl),
        }
    }

    /// Returns the dominant R stroke without constructing the complete glyph.
    pub(crate) fn stroke_width_for_aperture(aperture_radius: f64) -> f64 {
        2.0 * (aperture_radius / 2.0_f64.sqrt()) / 9.0
    }
}

fn stem(square_half: f64, unit: f64) -> StemGeometry {
    StemGeometry {
        left: -19.0 * unit / 6.0,
        right: -13.0 * unit / 6.0,
        top: -square_half,
        bottom: square_half,
    }
}

fn serifs(square_half: f64, unit: f64, stem: StemGeometry) -> SerifGeometry {
    let radius = 2.0 * unit / 3.0;
    let left_center_x = -square_half + radius;
    let inner_center_x = stem.right + radius;
    SerifGeometry {
        top_left: cap_circle(
            left_center_x,
            -square_half + radius,
            radius,
            stem.left,
            square_half,
        ),
        bottom_left: baseline_circle(
            left_center_x,
            square_half - radius,
            radius,
            stem.left,
            square_half,
        ),
        bottom_inner: baseline_circle(
            inner_center_x,
            square_half - radius,
            radius,
            stem.right,
            square_half,
        ),
    }
}

fn cap_circle(
    center_x: f64,
    center_y: f64,
    radius: f64,
    stem_edge: f64,
    cap: f64,
) -> TangentCircle {
    TangentCircle {
        circle: Circle {
            center: Point {
                x: center_x,
                y: center_y,
            },
            radius,
        },
        stem_tangent: Point {
            x: stem_edge,
            y: center_y,
        },
        edge_tangent: Point {
            x: center_x,
            y: -cap,
        },
    }
}

fn baseline_circle(
    center_x: f64,
    center_y: f64,
    radius: f64,
    stem_edge: f64,
    baseline: f64,
) -> TangentCircle {
    TangentCircle {
        circle: Circle {
            center: Point {
                x: center_x,
                y: center_y,
            },
            radius,
        },
        stem_tangent: Point {
            x: stem_edge,
            y: center_y,
        },
        edge_tangent: Point {
            x: center_x,
            y: baseline,
        },
    }
}

fn bowl(unit: f64) -> BowlGeometry {
    let outer = Circle {
        center: Point {
            x: -unit / 2.0,
            y: -17.0 * unit / 8.0,
        },
        radius: 19.0 * unit / 8.0,
    };
    let inner = Circle {
        center: Point {
            x: -5.0 * unit / 4.0,
            y: -17.0 * unit / 8.0,
        },
        radius: 17.0 * unit / 8.0,
    };
    BowlGeometry {
        outer,
        inner,
        outer_top: outer.point_at_offset(0.0, -outer.radius),
        inner_top: inner.point_at_offset(0.0, -inner.radius),
        outer_waist: Point {
            x: unit * (-0.5 + 3.0 * 2.0_f64.sqrt() / 4.0),
            y: 0.0,
        },
        inner_waist: inner.point_at_offset(0.0, inner.radius),
    }
}

fn leg(square_half: f64, unit: f64, bowl: BowlGeometry) -> LegGeometry {
    let diagonal_start = Point { x: 0.0, y: 0.0 };
    let outer_end = Point {
        x: square_half,
        y: square_half,
    };
    let outer_sagitta = unit / 10.0;
    let diagonal_component = 1.0 / 2.0_f64.sqrt();
    let outer_radius = square_half * square_half / (4.0 * outer_sagitta) + outer_sagitta / 2.0;
    let outer_center_offset = outer_radius - outer_sagitta;
    let outer_circle = Circle {
        center: Point {
            x: square_half / 2.0 + outer_center_offset * diagonal_component,
            y: square_half / 2.0 - outer_center_offset * diagonal_component,
        },
        radius: outer_radius,
    };
    let outer_start = bowl.outer_waist;
    let outer_rounding_circle = outer_root_rounding(bowl.outer, outer_circle, outer_start);
    let outer_join = internal_tangent_point(outer_circle, outer_rounding_circle);

    let root = Point {
        x: -unit / 2.0,
        y: 0.0,
    };
    let root_radius = 13.0 * unit / 64.0;
    let root_circle = Circle {
        center: Point {
            x: root.x,
            y: root.y + root_radius,
        },
        radius: root_radius,
    };

    let root_tangent =
        root_circle.point_at_offset(-4.0 * root_radius / 5.0, 3.0 * root_radius / 5.0);
    let tip_tangent = Point {
        x: 143.0 * unit / 160.0,
        y: 12.0 * unit / 5.0,
    };
    let tip_radius = 17833.0 * unit / 3328.0;
    let tip_circle = Circle {
        center: Point {
            x: 21551.0 * unit / 4160.0,
            y: -13563.0 * unit / 16640.0,
        },
        radius: tip_radius,
    };
    LegGeometry {
        outer_circle,
        outer_sagitta,
        diagonal_start,
        outer_rounding_circle,
        outer_start,
        outer_join,
        outer_end,
        inner: InnerLegGeometry {
            root_circle,
            tip_circle,
            root,
            root_tangent,
            tip_tangent,
            tip: outer_end,
        },
    }
}

fn outer_root_rounding(bowl: Circle, outer: Circle, root: Point) -> Circle {
    let normal = Point {
        x: (root.x - bowl.center.x) / bowl.radius,
        y: (root.y - bowl.center.y) / bowl.radius,
    };
    let root_from_outer = Point {
        x: root.x - outer.center.x,
        y: root.y - outer.center.y,
    };
    let squared_distance =
        root_from_outer.x * root_from_outer.x + root_from_outer.y * root_from_outer.y;
    let projection = root_from_outer.x * normal.x + root_from_outer.y * normal.y;
    let radius =
        (outer.radius * outer.radius - squared_distance) / (2.0 * (outer.radius + projection));
    Circle {
        center: Point {
            x: root.x + radius * normal.x,
            y: root.y + radius * normal.y,
        },
        radius,
    }
}

fn internal_tangent_point(outer: Circle, inner: Circle) -> Point {
    let centers = Point {
        x: inner.center.x - outer.center.x,
        y: inner.center.y - outer.center.y,
    };
    let distance = centers.x.hypot(centers.y);
    Point {
        x: outer.center.x + outer.radius * centers.x / distance,
        y: outer.center.y + outer.radius * centers.y / distance,
    }
}

#[cfg(test)]
mod tests {
    use super::GlyphGeometry;
    use super::Point;

    const EPSILON: f64 = 1e-9;

    #[test]
    fn mother_square_is_nine_modules_and_inscribed_in_aperture() {
        let aperture = 100.0;
        let glyph = GlyphGeometry::from_aperture(aperture);
        assert!((2.0 * glyph.square_half - 9.0 * glyph.unit).abs() < EPSILON);
        assert!((2.0_f64.sqrt() * glyph.square_half - aperture).abs() < EPSILON);
        assert!((glyph.stroke_width - glyph.unit).abs() < EPSILON);
        assert!((glyph.thin_stroke_width - glyph.unit / 2.0).abs() < EPSILON);
    }

    #[test]
    fn bowl_uses_rational_circles_on_one_axis() {
        let glyph = GlyphGeometry::from_aperture(100.0);
        let bowl = glyph.bowl;
        assert!((bowl.outer.radius - 19.0 * glyph.unit / 8.0).abs() < EPSILON);
        assert!((bowl.inner.radius - 17.0 * glyph.unit / 8.0).abs() < EPSILON);
        assert!(
            (bowl.inner.center.x - bowl.outer.center.x + 3.0 * glyph.unit / 4.0).abs() < EPSILON
        );
        assert!((bowl.inner.center.y - bowl.outer.center.y).abs() < EPSILON);
        assert!((bowl.outer.radius - bowl.inner.radius - glyph.unit / 4.0).abs() < EPSILON);
        assert!(
            (distance(bowl.outer.center, bowl.outer_waist) - bowl.outer.radius).abs() < EPSILON
        );
        assert!(
            (distance(bowl.inner.center, bowl.inner_waist) - bowl.inner.radius).abs() < EPSILON
        );
        assert!((bowl.outer_top.y - glyph.stem.top).abs() < EPSILON);
    }

    #[test]
    fn serif_circles_are_tangent_to_stem_and_square_edges() {
        let glyph = GlyphGeometry::from_aperture(100.0);
        for serif in [
            glyph.serifs.top_left,
            glyph.serifs.bottom_left,
            glyph.serifs.bottom_inner,
        ] {
            assert!(
                (distance(serif.circle.center, serif.stem_tangent) - serif.circle.radius).abs()
                    < EPSILON
            );
            assert!(
                (distance(serif.circle.center, serif.edge_tangent) - serif.circle.radius).abs()
                    < EPSILON
            );
        }
        assert!((glyph.serifs.top_left.edge_tangent.y - glyph.stem.top).abs() < EPSILON);
        assert!((glyph.serifs.bottom_left.edge_tangent.y - glyph.stem.bottom).abs() < EPSILON);
        assert!((glyph.serifs.bottom_inner.edge_tangent.y - glyph.stem.bottom).abs() < EPSILON);
    }

    #[test]
    fn leg_obeys_pacioli_arc_tangencies_and_terminal_taper() {
        let glyph = GlyphGeometry::from_aperture(100.0);
        let leg = glyph.leg;
        let inner = leg.inner;
        assert!(
            (distance(leg.outer_circle.center, leg.diagonal_start) - leg.outer_circle.radius).abs()
                < EPSILON
        );
        assert!(
            (distance(leg.outer_circle.center, leg.outer_end) - leg.outer_circle.radius).abs()
                < EPSILON
        );
        let chord_midpoint = midpoint(leg.diagonal_start, leg.outer_end);
        let arc_midpoint = Point {
            x: chord_midpoint.x - leg.outer_sagitta / 2.0_f64.sqrt(),
            y: chord_midpoint.y + leg.outer_sagitta / 2.0_f64.sqrt(),
        };
        assert!(
            (distance(leg.outer_circle.center, arc_midpoint) - leg.outer_circle.radius).abs()
                < EPSILON
        );
        assert!((distance(chord_midpoint, arc_midpoint) - leg.outer_sagitta).abs() < EPSILON);

        assert!((leg.outer_start.x - glyph.bowl.outer_waist.x).abs() < EPSILON);
        assert!((leg.outer_start.y - glyph.bowl.outer_waist.y).abs() < EPSILON);
        assert!(
            (distance(glyph.bowl.outer.center, leg.outer_start) - glyph.bowl.outer.radius).abs()
                < EPSILON
        );
        assert!(
            (distance(leg.outer_rounding_circle.center, leg.outer_start)
                - leg.outer_rounding_circle.radius)
                .abs()
                < EPSILON
        );
        assert!(
            (distance(glyph.bowl.outer.center, leg.outer_rounding_circle.center)
                - glyph.bowl.outer.radius
                - leg.outer_rounding_circle.radius)
                .abs()
                < EPSILON
        );
        assert!(
            (distance(leg.outer_circle.center, leg.outer_rounding_circle.center)
                - leg.outer_circle.radius
                + leg.outer_rounding_circle.radius)
                .abs()
                < EPSILON
        );
        assert!(
            (distance(leg.outer_rounding_circle.center, leg.outer_join)
                - leg.outer_rounding_circle.radius)
                .abs()
                < EPSILON
        );
        assert!(
            (distance(leg.outer_circle.center, leg.outer_join) - leg.outer_circle.radius).abs()
                < EPSILON
        );
        assert!(
            (leg.outer_start.x - inner.root.x - 3.0 * 2.0_f64.sqrt() * glyph.unit / 4.0).abs()
                < EPSILON
        );

        assert!((distance(leg.diagonal_start, inner.root) - glyph.unit / 2.0).abs() < EPSILON);
        assert!((inner.root.x + glyph.unit / 2.0).abs() < EPSILON);
        assert!(inner.root.y.abs() < EPSILON);
        assert!(
            (distance(inner.root_circle.center, inner.root) - inner.root_circle.radius).abs()
                < EPSILON
        );
        assert!((inner.root_circle.center.x - inner.root.x).abs() < EPSILON);
        assert!((inner.root_circle.radius - 13.0 * glyph.unit / 64.0).abs() < EPSILON);
        assert!((inner.root_tangent.x + 53.0 * glyph.unit / 80.0).abs() < EPSILON);
        assert!((inner.root_tangent.y - 13.0 * glyph.unit / 40.0).abs() < EPSILON);

        let tangent = subtract(inner.tip_tangent, inner.root_tangent);
        let root_radius = subtract(inner.root_tangent, inner.root_circle.center);
        let tip_radius = subtract(inner.tip_tangent, inner.tip_circle.center);
        assert!((4.0 * tangent.x - 3.0 * tangent.y).abs() < EPSILON);
        assert!(dot(tangent, root_radius).abs() < EPSILON);
        assert!(dot(tangent, tip_radius).abs() < EPSILON);
        for point in [inner.root_tangent, inner.tip_tangent] {
            assert!((point.x - 3.0 * point.y / 4.0 + 29.0 * glyph.unit / 32.0).abs() < EPSILON);
        }
        assert!((inner.tip_tangent.x - 143.0 * glyph.unit / 160.0).abs() < EPSILON);
        assert!((inner.tip_tangent.y - 12.0 * glyph.unit / 5.0).abs() < EPSILON);
        assert!((inner.tip_circle.radius - 17833.0 * glyph.unit / 3328.0).abs() < EPSILON);

        assert!((inner.tip.x - leg.outer_end.x).abs() < EPSILON);
        assert!((inner.tip.y - leg.outer_end.y).abs() < EPSILON);
        assert!(
            (distance(inner.tip_circle.center, inner.tip) - inner.tip_circle.radius).abs()
                < EPSILON
        );
    }

    fn midpoint(first: Point, second: Point) -> Point {
        Point {
            x: (first.x + second.x) / 2.0,
            y: (first.y + second.y) / 2.0,
        }
    }

    fn distance(first: Point, second: Point) -> f64 {
        (first.x - second.x).hypot(first.y - second.y)
    }

    fn subtract(first: Point, second: Point) -> Point {
        Point {
            x: first.x - second.x,
            y: first.y - second.y,
        }
    }

    fn dot(first: Point, second: Point) -> f64 {
        first.x * second.x + first.y * second.y
    }
}
