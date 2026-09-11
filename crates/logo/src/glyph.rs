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

/// Paired eccentric circles inherited from Pacioli's B construction.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BowlGeometry {
    /// Outer contour circle.
    pub(crate) outer: Circle,
    /// Inner counter circle, congruent and offset from the outer circle.
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

/// The inner leg boundary constructed from two arcs and their common tangent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InnerLegGeometry {
    /// Circle rounding the leg root.
    pub(crate) root_circle: Circle,
    /// Circle resolving the leg into the lower-right corner.
    pub(crate) tip_circle: Circle,
    /// Point where the leg leaves the waist.
    pub(crate) root: Point,
    /// Tangency from the root circle to the straight segment.
    pub(crate) root_tangent: Point,
    /// Tangency from the straight segment to the tip circle.
    pub(crate) tip_tangent: Point,
    /// Terminal point at the lower-right corner.
    pub(crate) tip: Point,
}

/// Curved leg from the center to the lower-right corner.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LegGeometry {
    /// Circle whose quarter arc is the outer contour.
    pub(crate) outer_circle: Circle,
    /// Start of the outer contour at the square center.
    pub(crate) outer_start: Point,
    /// End of the outer contour at the lower-right corner.
    pub(crate) outer_end: Point,
    /// Inner contour.
    pub(crate) inner: InnerLegGeometry,
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
        Self {
            square_half,
            unit,
            stroke_width: unit,
            thin_stroke_width: unit / 2.0,
            stem,
            serifs: serifs(square_half, unit, stem),
            bowl: bowl(unit),
            leg: leg(square_half, unit),
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
    let radius = 5.0 * unit / 2.0;
    let outer = Circle {
        center: Point {
            x: -unit / 2.0,
            y: -2.0 * unit,
        },
        radius,
    };
    let inner = Circle {
        center: Point {
            x: -3.0 * unit / 2.0,
            y: -3.0 * unit / 2.0,
        },
        radius,
    };
    BowlGeometry {
        outer,
        inner,
        outer_top: outer.point_at_offset(0.0, -radius),
        inner_top: inner.point_at_offset(0.0, -radius),
        outer_waist: Point { x: unit, y: 0.0 },
        inner_waist: Point {
            x: unit / 2.0,
            y: 0.0,
        },
    }
}

fn leg(square_half: f64, unit: f64) -> LegGeometry {
    let outer_start = Point { x: 0.0, y: 0.0 };
    let outer_end = Point {
        x: square_half,
        y: square_half,
    };
    let root_circle = Circle {
        center: Point {
            x: unit,
            y: 3.0 * unit / 4.0,
        },
        radius: 3.0 * unit / 4.0,
    };
    let tip_circle = Circle {
        center: Point {
            x: square_half,
            y: square_half - unit / 4.0,
        },
        radius: unit / 4.0,
    };
    let (root_tangent, tip_tangent) = inner_common_tangent(root_circle, tip_circle);
    LegGeometry {
        outer_circle: Circle {
            center: Point {
                x: square_half,
                y: 0.0,
            },
            radius: square_half,
        },
        outer_start,
        outer_end,
        inner: InnerLegGeometry {
            root_circle,
            tip_circle,
            root: Point { x: unit, y: 0.0 },
            root_tangent,
            tip_tangent,
            tip: outer_end,
        },
    }
}

fn inner_common_tangent(first: Circle, second: Circle) -> (Point, Point) {
    let delta = Point {
        x: second.center.x - first.center.x,
        y: second.center.y - first.center.y,
    };
    let distance = delta.x.hypot(delta.y);
    let axis = Point {
        x: delta.x / distance,
        y: delta.y / distance,
    };
    let radial_projection = (first.radius - second.radius) / distance;
    let tangent_projection = (1.0 - radial_projection.powi(2)).sqrt();
    let left_normal = Point {
        x: -axis.y,
        y: axis.x,
    };
    let normal = Point {
        x: radial_projection * axis.x + tangent_projection * left_normal.x,
        y: radial_projection * axis.y + tangent_projection * left_normal.y,
    };
    (
        first.point_at_offset(first.radius * normal.x, first.radius * normal.y),
        second.point_at_offset(second.radius * normal.x, second.radius * normal.y),
    )
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
    fn bowl_uses_congruent_eccentric_circles() {
        let glyph = GlyphGeometry::from_aperture(100.0);
        let bowl = glyph.bowl;
        assert!((bowl.outer.radius - 5.0 * glyph.unit / 2.0).abs() < EPSILON);
        assert!((bowl.outer.radius - bowl.inner.radius).abs() < EPSILON);
        assert!((bowl.inner.center.x - bowl.outer.center.x + glyph.unit).abs() < EPSILON);
        assert!((bowl.inner.center.y - bowl.outer.center.y - glyph.unit / 2.0).abs() < EPSILON);
        assert!(
            (bowl.outer_waist.x - bowl.inner_waist.x - glyph.thin_stroke_width).abs() < EPSILON
        );
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
    fn leg_is_a_quarter_circle_closed_by_a_biarc_common_tangent() {
        let glyph = GlyphGeometry::from_aperture(100.0);
        let leg = glyph.leg;
        assert!(
            (distance(leg.outer_circle.center, leg.outer_start) - leg.outer_circle.radius).abs()
                < EPSILON
        );
        assert!(
            (distance(leg.outer_circle.center, leg.outer_end) - leg.outer_circle.radius).abs()
                < EPSILON
        );
        assert!(
            (distance(leg.inner.root_circle.center, leg.inner.root) - leg.inner.root_circle.radius)
                .abs()
                < EPSILON
        );
        assert!(
            (distance(leg.inner.tip_circle.center, leg.inner.tip) - leg.inner.tip_circle.radius)
                .abs()
                < EPSILON
        );

        let tangent = subtract(leg.inner.tip_tangent, leg.inner.root_tangent);
        let root_radius = subtract(leg.inner.root_tangent, leg.inner.root_circle.center);
        let tip_radius = subtract(leg.inner.tip_tangent, leg.inner.tip_circle.center);
        assert!(dot(tangent, root_radius).abs() < EPSILON);
        assert!(dot(tangent, tip_radius).abs() < EPSILON);
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
