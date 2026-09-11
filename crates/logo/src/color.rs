//! Palette construction in OKLCH with discrete contrast constraints.

use std::error::Error;
use std::fmt;

use crate::model::GearModel;

const LIGHTNESS_STEPS: u32 = 10_000;

/// A palette construction failure.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ColorError {
    /// No point on the constrained lightness grid meets the contrast target.
    UnsatisfiedContrast(&'static str),
}

impl fmt::Display for ColorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsatisfiedContrast(role) => {
                write!(
                    formatter,
                    "the {role} color cannot meet its contrast target"
                )
            }
        }
    }
}

impl Error for ColorError {}

/// Whether paint is constrained against white or black.
#[derive(Clone, Copy, Debug)]
enum Ground {
    Light,
    Dark,
}

impl Ground {
    const fn luminance(self) -> f64 {
        match self {
            Self::Light => 1.0,
            Self::Dark => 0.0,
        }
    }
}

/// A solved palette role and its reproducible color coordinates.
#[derive(Clone, Debug)]
pub(crate) struct Paint {
    pub(crate) hex: String,
    pub(crate) lightness: f64,
    pub(crate) chroma: f64,
    pub(crate) hue: f64,
    pub(crate) contrast: f64,
    pub(crate) target_contrast: f64,
}

/// Four semantic colors derived from the construction counts.
#[derive(Clone, Debug)]
pub(crate) struct Palette {
    pub(crate) primary: Paint,
    pub(crate) accent: Paint,
    pub(crate) signal: Paint,
    pub(crate) guide: Paint,
}

impl Palette {
    pub(crate) fn light(model: &GearModel) -> Result<Self, ColorError> {
        Self::solve(model, Ground::Light)
    }

    pub(crate) fn dark(model: &GearModel) -> Result<Self, ColorError> {
        Self::solve(model, Ground::Dark)
    }

    fn solve(model: &GearModel, ground: Ground) -> Result<Self, ColorError> {
        let teeth = f64::from(model.spec.teeth);
        let holes = f64::from(model.spec.hole_count);
        let golden_ratio = model.glyph().golden_ratio;
        let rust_hue = 360.0 / (2.0 * holes);
        let signal_hue = rust_hue + 180.0;
        let accent_chroma = (holes - 1.0) / teeth;
        let guide_chroma = 1.0 / teeth;
        let neutral_chroma = guide_chroma / golden_ratio.powi(2);

        Ok(Self {
            primary: solve_paint("primary", ground, neutral_chroma, rust_hue, 7.0)?,
            accent: solve_paint("accent", ground, accent_chroma, rust_hue, 4.5)?,
            signal: solve_paint("signal", ground, accent_chroma, signal_hue, 4.5)?,
            guide: solve_paint("guide", ground, guide_chroma, signal_hue, 3.0)?,
        })
    }
}

fn solve_paint(
    role: &'static str,
    ground: Ground,
    chroma: f64,
    hue: f64,
    target_contrast: f64,
) -> Result<Paint, ColorError> {
    let steps: Box<dyn Iterator<Item = u32>> = match ground {
        Ground::Light => Box::new((0..=LIGHTNESS_STEPS).rev()),
        Ground::Dark => Box::new(0..=LIGHTNESS_STEPS),
    };

    for step in steps {
        let lightness = f64::from(step) / f64::from(LIGHTNESS_STEPS);
        let rgb = oklch_to_srgb(lightness, chroma, hue);
        let contrast = contrast_ratio(relative_luminance(rgb), ground.luminance());
        if contrast >= target_contrast {
            return Ok(Paint {
                hex: rgb.hex(),
                lightness,
                chroma,
                hue,
                contrast,
                target_contrast,
            });
        }
    }

    Err(ColorError::UnsatisfiedContrast(role))
}

#[derive(Clone, Copy, Debug)]
struct Srgb {
    red: u8,
    green: u8,
    blue: u8,
}

impl Srgb {
    fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.red, self.green, self.blue)
    }
}

fn oklch_to_srgb(lightness: f64, chroma: f64, hue_degrees: f64) -> Srgb {
    let hue = hue_degrees.to_radians();
    let a = chroma * hue.cos();
    let b = chroma * hue.sin();
    let l_root = lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_root = lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_root = lightness - 0.089_484_177_5 * a - 1.291_485_548 * b;
    let l = l_root.powi(3);
    let m = m_root.powi(3);
    let s = s_root.powi(3);

    Srgb {
        red: encode_channel(4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s),
        green: encode_channel(-1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s),
        blue: encode_channel(-0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701 * s),
    }
}

fn encode_channel(linear: f64) -> u8 {
    let encoded = if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn relative_luminance(color: Srgb) -> f64 {
    0.2126 * decode_channel(color.red)
        + 0.7152 * decode_channel(color.green)
        + 0.0722 * decode_channel(color.blue)
}

fn decode_channel(channel: u8) -> f64 {
    let encoded = f64::from(channel) / 255.0;
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

fn contrast_ratio(first: f64, second: f64) -> f64 {
    let lighter = first.max(second);
    let darker = first.min(second);
    (lighter + 0.05) / (darker + 0.05)
}

#[cfg(test)]
mod tests {
    use super::Palette;
    use crate::model::{GearModel, Spec};

    #[test]
    fn every_role_meets_its_contrast_constraint() {
        let model = GearModel::new(Spec::rings());
        assert!(model.is_ok());
        if let Ok(model) = model {
            for palette in [Palette::light(&model), Palette::dark(&model)] {
                assert!(palette.is_ok());
                if let Ok(palette) = palette {
                    for paint in [
                        palette.primary,
                        palette.accent,
                        palette.signal,
                        palette.guide,
                    ] {
                        assert!(paint.contrast >= paint.target_contrast);
                    }
                }
            }
        }
    }

    #[test]
    fn hues_are_derived_from_the_pentagonal_sector() {
        let model = GearModel::new(Spec::rings());
        assert!(model.is_ok());
        if let Ok(model) = model {
            let palette = Palette::light(&model);
            assert!(palette.is_ok());
            if let Ok(palette) = palette {
                assert!((palette.accent.hue - 36.0).abs() < f64::EPSILON);
                assert!((palette.signal.hue - 216.0).abs() < f64::EPSILON);
                let expected_chroma =
                    f64::from(model.spec.hole_count - 1) / f64::from(model.spec.teeth);
                assert!((palette.accent.chroma - expected_chroma).abs() < f64::EPSILON);
            }
        }
    }
}
