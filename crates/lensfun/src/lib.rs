//! Lens profiles from the [lensfun](https://lensfun.github.io/) lens database.
//!
//! The database (XML files, CC BY-SA 3.0, maintained by the lensfun project) is never part of LightCraft: the engine
//! downloads a pinned copy when the user asks for it, and this crate reads it. No lensfun code is used; the correction
//! models are implemented from lensfun's published formulas (see `docs/lens-profiles.md`):
//!
//! - distortion `ptlens` `r_d = r_u·(a·r_u³ + b·r_u² + c·r_u + 1 − a − b − c)`, `poly3` `r_d = r_u·(1 − k1 + k1·r_u²)`
//!   and `poly5` `r_d = r_u·(1 + k1·r_u² + k2·r_u⁴)`, with `r = 1` at half the shorter side of the calibration frame
//!   (PanoTools' "largest circle that fits"). Like lensfun, the PTLens/poly3 scale `d = 1 − a − b − c` is divided
//!   out so the centre of the picture keeps its size.
//! - lateral chromatic aberration (TCA) `linear` `r_d = r_u·k` and `poly3` `r_d = r_u·(b·r_u² + c·r_u + v)` for the
//!   red and blue planes, in the same coordinates, applied to the distorted position.
//! - vignetting `pa` `C_d = C_s / (1 + k1·r² + k2·r⁴ + k3·r⁶)`, with `r = 1` at the corner (half diagonal).
//!
//! [`Lens::correction`] interpolates a lens's calibration for one shot (focal length, aperture, focus distance, the
//! camera's crop factor) and expresses it as an [`EmbeddedLens`](lightcraft_develop::EmbeddedLens), the per-photo
//! correction the develop pipeline already applies under "Enable Profile Corrections".
//!
//! Pure and platform-neutral: no file system or network (the engine reads the files and passes their text).

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod correct;
mod fit;
mod matcher;
mod parse;

pub use correct::{Shot, Unsupported};
pub use matcher::{LensMatch, Query, normalize};
pub use parse::ParseError;

/// A camera body: which mount it takes and its crop factor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Camera {
    pub maker: String,
    pub model: String,
    pub mount: String,
    /// Crop factor (35 mm diagonal / sensor diagonal).
    pub crop: f64,
}

/// What a lens projects: lensfun's radial models only describe rectilinear lenses here; fisheye and other projections
/// are listed but not corrected (they need a projection change, not a radial warp).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LensKind {
    #[default]
    Rectilinear,
    Other(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DistortionModel {
    Poly3 { k1: f64 },
    Poly5 { k1: f64, k2: f64 },
    PtLens { a: f64, b: f64, c: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Distortion {
    pub focal: f64,
    pub model: DistortionModel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TcaModel {
    Linear { kr: f64, kb: f64 },
    Poly3 { vr: f64, cr: f64, br: f64, vb: f64, cb: f64, bb: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tca {
    pub focal: f64,
    pub model: TcaModel,
}

/// One `pa` vignetting calibration point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vignetting {
    pub focal: f64,
    pub aperture: f64,
    /// Focus distance in metres.
    pub distance: f64,
    pub k: [f64; 3],
}

/// A lens and its calibration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lens {
    pub maker: String,
    pub model: String,
    pub mounts: Vec<String>,
    /// Crop factor of the camera the calibration shots were taken with.
    pub crop: f64,
    /// Long/short edge ratio of that camera (lensfun's default is 3:2).
    pub aspect: f64,
    pub kind: LensKind,
    /// Focal range in mm (from `<focal>` or the model name), when known.
    pub focal: Option<(f64, f64)>,
    pub distortion: Vec<Distortion>,
    pub tca: Vec<Tca>,
    pub vignetting: Vec<Vignetting>,
}

impl Lens {
    /// Whether the lens has any calibration this crate can apply.
    pub fn has_calibration(&self) -> bool {
        self.kind == LensKind::Rectilinear && (!self.distortion.is_empty() || !self.tca.is_empty() || !self.vignetting.is_empty())
    }
}

/// A mount and the mounts whose lenses also fit it (through adapters or because they are the same flange).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mount {
    pub name: String,
    pub compat: Vec<String>,
}

/// The parsed database: every file's cameras, lenses and mounts together.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Database {
    pub cameras: Vec<Camera>,
    pub lenses: Vec<Lens>,
    pub mounts: Vec<Mount>,
    /// Calibration entries skipped because this crate doesn't implement their model (e.g. Adobe's `acm`).
    pub skipped: usize,
}

impl Database {
    /// Parse one database file and add what it holds.
    pub fn add_xml(&mut self, xml: &str) -> Result<(), ParseError> {
        parse::add(self, xml)
    }

    /// Whether lenses for `lens_mount` fit a camera with `camera_mount` (same mount, or listed as compatible).
    pub fn mount_fits(&self, camera_mount: &str, lens_mount: &str) -> bool {
        if camera_mount.eq_ignore_ascii_case(lens_mount) {
            return true;
        }
        self.mounts.iter().filter(|m| m.name.eq_ignore_ascii_case(camera_mount)).any(|m| m.compat.iter().any(|c| c.eq_ignore_ascii_case(lens_mount)))
    }
}
