//! Pure geometric model for the Rings construction mark.

use std::error::Error;
use std::f64::consts::PI;
use std::fmt;

use crate::glyph::GlyphGeometry;

/// A validated construction failure.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum GeometryError {
    /// The sole linear module is not finite and positive.
    InvalidModule,
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
            Self::InvalidModule => "module must be finite and positive",
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
        if !spec.module.is_finite() || spec.module <= 0.0 {
            return Err(GeometryError::InvalidModule);
        }
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
        let glyph_stroke_width = GlyphGeometry::stroke_width_for_aperture(aperture_radius);
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

    /// Returns the central R from its nine-part mother square.
    pub(crate) fn glyph(&self) -> GlyphGeometry {
        GlyphGeometry::from_aperture(self.aperture_radius)
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
    fn glyph_stroke_derives_from_a_nine_part_inscribed_square() {
        let model = GearModel::new(Spec::rings());
        assert!(model.is_ok());
        if let Ok(model) = model {
            let glyph = model.glyph();
            assert!((2.0 * glyph.square_half - 9.0 * glyph.unit).abs() < 1e-9);
            assert!((2.0_f64.sqrt() * glyph.square_half - model.aperture_radius).abs() < 1e-9);
            assert!((glyph.stroke_width - glyph.unit).abs() < 1e-9);
        }
    }
}
