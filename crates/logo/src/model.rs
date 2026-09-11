//! Pure geometric model for the Rings construction mark.

use std::error::Error;
use std::f64::consts::PI;
use std::fmt;

/// A validated construction failure.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum GeometryError {
    /// A rotational count cannot define the required polygons.
    InvalidConstructionCount,
    /// The bore count does not divide the tooth count.
    IncommensurateBorePhase,
    /// The gear circles are not strictly ordered.
    InvalidRadiusOrder,
    /// A bore intersects the central aperture.
    BoreIntersectsAperture,
    /// A bore crosses the gear root circle.
    BoreCrossesRoot,
    /// The involute sampling bound is too low.
    CoarseInvolute,
}

impl fmt::Display for GeometryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidConstructionCount => "teeth and bores must define non-zero polygons",
            Self::IncommensurateBorePhase => "bore count must divide tooth count",
            Self::InvalidRadiusOrder => "gear radii must be strictly ordered",
            Self::BoreIntersectsAperture => "a bore intersects the central aperture",
            Self::BoreCrossesRoot => "a bore crosses the root circle",
            Self::CoarseInvolute => "involute sampling bound must be at least twelve",
        };
        formatter.write_str(message)
    }
}

impl Error for GeometryError {}

/// Dimensionless inputs derived from one module.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Spec {
    /// The sole linear unit.
    pub(crate) module: f64,
    /// Number of involute teeth.
    pub(crate) teeth: u32,
    /// Standard spur-gear pressure angle.
    pub(crate) pressure_angle_degrees: f64,
    /// Number of bores on the regular polygon.
    pub(crate) hole_count: u32,
}

impl Spec {
    /// Returns the canonical Rings construction.
    pub(crate) const fn rings() -> Self {
        Self {
            module: 10.0,
            teeth: 30,
            pressure_angle_degrees: 20.0,
            hole_count: 5,
        }
    }
}

/// A Cartesian point in logo construction space.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Point {
    /// Horizontal coordinate.
    pub(crate) x: f64,
    /// Vertical coordinate.
    pub(crate) y: f64,
}

/// The geometric construction of the central R.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphGeometry {
    /// Golden ratio inherited from the regular pentagon.
    pub(crate) golden_ratio: f64,
    /// Horizontal center of the vertical stem.
    pub(crate) stem_x: f64,
    /// Symmetric cap distance above and below the origin.
    pub(crate) cap: f64,
    /// Horizontal tangent point of the semicircular bowl.
    pub(crate) bowl_x: f64,
    /// Vertical tangent point of the bowl and middle bar.
    pub(crate) middle_y: f64,
    /// Radius of the exact semicircular bowl.
    pub(crate) bowl_radius: f64,
    /// Start of the pentagon-derived diagonal.
    pub(crate) leg_start: Point,
    /// End of the pentagon-derived diagonal.
    pub(crate) leg_end: Point,
    /// Uniform glyph stroke.
    pub(crate) stroke_width: f64,
    /// Bore whose radial axis determines the diagonal leg.
    pub(crate) leg_bore_index: u32,
    /// Diagonal angle aligned with the lower-right bore.
    pub(crate) leg_angle_degrees: f64,
}

/// Validated radii, phases, and curves for one mark.
#[derive(Debug)]
pub(crate) struct GearModel {
    /// Original construction inputs.
    pub(crate) spec: Spec,
    /// Standard pitch radius.
    pub(crate) pitch_radius: f64,
    /// Involute base radius.
    pub(crate) base_radius: f64,
    /// Addendum radius.
    pub(crate) outer_radius: f64,
    /// Dedendum radius.
    pub(crate) root_radius: f64,
    /// Central aperture radius.
    pub(crate) aperture_radius: f64,
    /// Circumradius of the regular bore polygon.
    pub(crate) hole_orbit: f64,
    /// Radius shared by all bores.
    pub(crate) hole_radius: f64,
    /// Mechanical outline width, derived from the central glyph stroke.
    pub(crate) gear_outline_width: f64,
    /// Construction guide width, derived from the mechanical outline.
    pub(crate) construction_guide_width: f64,
    /// Angular pitch between adjacent teeth.
    pub(crate) tooth_pitch: f64,
    /// Rotation from the tooth center to an involute base point.
    pub(crate) flank_rotation: f64,
    /// Final involute parameter at the addendum.
    pub(crate) outer_involute_parameter: f64,
    /// Samples per flank, derived from the rotational counts.
    pub(crate) flank_samples: u32,
}

impl GearModel {
    /// Validates a specification and derives its complete geometry.
    pub(crate) fn new(spec: Spec) -> Result<Self, GeometryError> {
        if spec.teeth == 0 || spec.hole_count < 2 {
            return Err(GeometryError::InvalidConstructionCount);
        }
        let pressure_angle = spec.pressure_angle_degrees.to_radians();
        let pitch_radius = spec.module * f64::from(spec.teeth) / 2.0;
        let base_radius = pitch_radius * pressure_angle.cos();
        let outer_radius = pitch_radius + spec.module;
        let root_radius = pitch_radius - 1.25 * spec.module;
        let aperture_radius = 2.0 * f64::from(spec.hole_count) * spec.module;
        let hole_orbit = aperture_radius + 2.0 * spec.module;
        let hole_radius = f64::from(spec.hole_count - 1) * spec.module / f64::from(spec.hole_count);
        let glyph_stroke_width = aperture_radius / f64::from(spec.hole_count);
        let teeth_per_bore_sector = spec.teeth / spec.hole_count;
        let gear_outline_width = glyph_stroke_width / f64::from(teeth_per_bore_sector);
        let construction_guide_width = gear_outline_width / f64::from(spec.hole_count);
        let flank_samples = (spec.teeth / spec.hole_count) * (spec.hole_count - 1);

        validate(
            &spec,
            Radii {
                pitch: pitch_radius,
                base: base_radius,
                outer: outer_radius,
                root: root_radius,
                aperture: aperture_radius,
                hole_orbit,
                hole: hole_radius,
            },
            flank_samples,
        )?;

        let tooth_pitch = 2.0 * PI / f64::from(spec.teeth);
        let pitch_involute = pressure_angle.tan() - pressure_angle;
        let half_tooth_at_pitch = PI / (2.0 * f64::from(spec.teeth));
        let flank_rotation = half_tooth_at_pitch + pitch_involute;
        let outer_involute_parameter = ((outer_radius / base_radius).powi(2) - 1.0).sqrt();

        Ok(Self {
            spec,
            pitch_radius,
            base_radius,
            outer_radius,
            root_radius,
            aperture_radius,
            hole_orbit,
            hole_radius,
            gear_outline_width,
            construction_guide_width,
            tooth_pitch,
            flank_rotation,
            outer_involute_parameter,
            flank_samples,
        })
    }

    /// Returns one exact point on a construction circle.
    pub(crate) fn point(radius: f64, angle: f64) -> Point {
        Point {
            x: radius * angle.cos(),
            y: radius * angle.sin(),
        }
    }

    /// Returns radius and polar angle for one involute parameter.
    pub(crate) fn polar_involute(&self, parameter: f64) -> (f64, f64) {
        (
            self.base_radius * (1.0 + parameter.powi(2)).sqrt(),
            parameter - parameter.atan(),
        )
    }

    /// Returns the bore centers as a regular polygon.
    pub(crate) fn holes(&self) -> impl Iterator<Item = Point> + '_ {
        let sector = 2.0 * PI / f64::from(self.spec.hole_count);
        (0..self.spec.hole_count)
            .map(move |index| Self::point(self.hole_orbit, -PI / 2.0 + f64::from(index) * sector))
    }

    /// Returns the central R from the aperture and pentagonal golden ratio.
    pub(crate) fn glyph(&self) -> GlyphGeometry {
        let golden_ratio = (1.0 + 5.0_f64.sqrt()) / 2.0;
        let bore_sector = 360.0 / f64::from(self.spec.hole_count);
        let leg_bore_index = self.spec.hole_count / 2;
        let leg_angle_degrees = -90.0 + f64::from(leg_bore_index) * bore_sector;
        let leg_angle = leg_angle_degrees.to_radians();
        let cap = self.aperture_radius / golden_ratio;
        let leg_start = Point { x: 0.0, y: 0.0 };
        let leg_end = Point {
            x: cap / leg_angle.tan(),
            y: cap,
        };

        GlyphGeometry {
            golden_ratio,
            stem_x: -self.aperture_radius / 2.0,
            cap,
            bowl_x: 0.0,
            middle_y: 0.0,
            bowl_radius: cap / 2.0,
            leg_start,
            leg_end,
            stroke_width: self.aperture_radius / f64::from(self.spec.hole_count),
            leg_bore_index,
            leg_angle_degrees,
        }
    }

    /// Returns the number of teeth in each bore sector.
    pub(crate) fn teeth_per_bore_sector(&self) -> u32 {
        self.spec.teeth / self.spec.hole_count
    }
}

#[derive(Clone, Copy)]
struct Radii {
    pitch: f64,
    base: f64,
    outer: f64,
    root: f64,
    aperture: f64,
    hole_orbit: f64,
    hole: f64,
}

fn validate(spec: &Spec, radii: Radii, flank_samples: u32) -> Result<(), GeometryError> {
    if !spec.teeth.is_multiple_of(spec.hole_count) {
        return Err(GeometryError::IncommensurateBorePhase);
    }
    if !(radii.root < radii.base && radii.base < radii.pitch && radii.pitch < radii.outer) {
        return Err(GeometryError::InvalidRadiusOrder);
    }
    if radii.aperture >= radii.hole_orbit - radii.hole {
        return Err(GeometryError::BoreIntersectsAperture);
    }
    if radii.hole_orbit + radii.hole >= radii.root {
        return Err(GeometryError::BoreCrossesRoot);
    }
    if flank_samples < 12 {
        return Err(GeometryError::CoarseInvolute);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::GearModel;
    use super::Spec;

    #[test]
    fn bore_polygon_and_tooth_phase_are_commensurate() {
        let model = GearModel::new(Spec::rings());
        assert!(model.is_ok());
        if let Ok(model) = model {
            assert!(model.spec.teeth.is_multiple_of(model.spec.hole_count));
            assert_eq!(model.teeth_per_bore_sector(), 6);
            for hole in model.holes() {
                assert!((hole.x.hypot(hole.y) - model.hole_orbit).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn secondary_dimensions_are_derived_from_rotational_counts() {
        let model = GearModel::new(Spec::rings());
        assert!(model.is_ok());
        if let Ok(model) = model {
            let bores = f64::from(model.spec.hole_count);
            assert!((model.aperture_radius - 2.0 * bores * model.spec.module).abs() < 1e-9);
            assert!(
                (model.hole_orbit - model.aperture_radius - 2.0 * model.spec.module).abs() < 1e-9
            );
            assert!((model.hole_radius - (bores - 1.0) * model.spec.module / bores).abs() < 1e-9);
            assert_eq!(
                model.flank_samples,
                model.teeth_per_bore_sector() * (model.spec.hole_count - 1)
            );
        }
    }

    #[test]
    fn stroke_hierarchy_matches_tooth_and_bore_counts() {
        let model = GearModel::new(Spec::rings());
        assert!(model.is_ok());
        if let Ok(model) = model {
            let glyph = model.glyph();
            assert!(
                (model.gear_outline_width
                    - glyph.stroke_width / f64::from(model.teeth_per_bore_sector()))
                .abs()
                    < 1e-9
            );
            assert!(
                (model.construction_guide_width
                    - model.gear_outline_width / f64::from(model.spec.hole_count))
                .abs()
                    < 1e-9
            );
            assert!(
                (glyph.stroke_width / model.construction_guide_width - f64::from(model.spec.teeth))
                    .abs()
                    < 1e-9
            );
            assert!(
                (model.gear_outline_width / model.construction_guide_width
                    - f64::from(model.spec.hole_count))
                .abs()
                    < 1e-9
            );
        }
    }

    #[test]
    fn glyph_bowl_is_tangent_and_leg_aligns_with_lower_right_bore() {
        let model = GearModel::new(Spec::rings());
        assert!(model.is_ok());
        if let Ok(model) = model {
            let glyph = model.glyph();
            assert!((2.0 * glyph.bowl_radius - (glyph.cap + glyph.middle_y)).abs() < 1e-9);
            assert!((glyph.cap * glyph.golden_ratio - model.aperture_radius).abs() < 1e-9);
            assert!((glyph.stem_x + model.aperture_radius / 2.0).abs() < 1e-9);
            assert!(glyph.bowl_x.abs() < 1e-9);
            assert!(glyph.middle_y.abs() < 1e-9);
            assert!(glyph.leg_start.x.abs() < 1e-9 && glyph.leg_start.y.abs() < 1e-9);
            assert!(
                (glyph.stroke_width - model.aperture_radius / f64::from(model.spec.hole_count))
                    .abs()
                    < 1e-9
            );
            assert!((glyph.leg_angle_degrees - 54.0).abs() < 1e-9);
            let angle = (glyph.leg_end.y - glyph.leg_start.y)
                .atan2(glyph.leg_end.x - glyph.leg_start.x)
                .to_degrees();
            assert!((angle - glyph.leg_angle_degrees).abs() < 1e-9);
            let mut aligned_bore_found = false;
            for (index, bore) in (0..model.spec.hole_count).zip(model.holes()) {
                if index == glyph.leg_bore_index {
                    let leg_x = glyph.leg_end.x - glyph.leg_start.x;
                    let leg_y = glyph.leg_end.y - glyph.leg_start.y;
                    let cross_product = leg_x * bore.y - leg_y * bore.x;
                    let dot_product = leg_x * bore.x + leg_y * bore.y;
                    assert!(cross_product.abs() < 1e-9);
                    assert!(dot_product > 0.0);
                    aligned_bore_found = true;
                }
            }
            assert!(aligned_bore_found);
        }
    }
}
