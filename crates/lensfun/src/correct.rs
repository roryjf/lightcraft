//! A lens's calibration for one shot, as the per-photo correction the pipeline applies.
//!
//! Units: lensfun measures distortion and TCA radii with `r = 1` at half the shorter side of the calibration
//! camera's frame and vignetting radii with `r = 1` at its corner. A photo from a camera with crop factor `c` (its
//! diagonal is `43.27 mm / c`) has its corner at `r = c_cal·√(1 + aspect²)/c` and `r = c_cal/c` in those units, where
//! `c_cal` and `aspect` are the calibration camera's. The pipeline's [`EmbeddedWarp`] / [`EmbeddedVignette`] measure
//! from the image centre with `r = 1` at the photo's corner; the curves are sampled on that radius and fitted.
//!
//! Calibration is interpolated linearly between the nearest calibrated focal lengths (distortion, TCA) and, for
//! vignetting, between the nearest focal lengths, apertures (in stops) and focus distances (in dioptres). Outside the
//! calibrated range the nearest entry is used.

use lightcraft_develop::{EmbeddedLens, EmbeddedVignette, EmbeddedWarp};
use lightcraft_geom::Point;

use crate::fit::least_squares;
use crate::{DistortionModel, Lens, LensKind, TcaModel, Vignetting};

/// One photo: what the calibration is interpolated for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shot {
    /// Focal length (mm).
    pub focal: f64,
    /// f-number; without it no vignetting correction is made.
    pub aperture: Option<f64>,
    /// Focus distance (m); unknown counts as far.
    pub distance: Option<f64>,
    /// The camera's crop factor (of its whole sensor).
    pub crop: f64,
    /// The sensor's own long/short ratio, when known. A picture of another shape (4:3 or 16:9 from a 3:2 sensor) is
    /// a crop of it: its diagonal is shorter, so its crop factor is larger.
    pub sensor_aspect: Option<f64>,
    /// The (oriented) image size in pixels; only its shape matters.
    pub width: f64,
    pub height: f64,
}

impl Shot {
    /// The picture's own crop factor: the sensor's, raised when the picture is a narrower or wider crop of it (the
    /// short side is kept for a narrower shape, the long side for a wider one).
    pub fn picture_crop(&self) -> f64 {
        let image = self.width.max(self.height) / self.width.min(self.height).max(1e-9);
        match self.sensor_aspect.filter(|a| a.is_finite() && *a >= 1.0) {
            Some(sensor) if (image - sensor).abs() > 0.02 => {
                let ratio = if image < sensor {
                    (sensor * sensor + 1.0).sqrt() / (image * image + 1.0).sqrt()
                } else {
                    (1.0 + 1.0 / (sensor * sensor)).sqrt() / (1.0 + 1.0 / (image * image)).sqrt()
                };
                self.crop * ratio
            }
            _ => self.crop,
        }
    }
}

/// Why no correction could be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unsupported {
    /// A fisheye or other non-rectilinear lens.
    Projection(String),
    /// Nothing calibrated that applies to this shot.
    NoCalibration,
    /// Focal length, crop factor or size missing or not positive.
    BadShot,
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unsupported::Projection(p) => write!(f, "{p} lenses are not corrected (only rectilinear ones)"),
            Unsupported::NoCalibration => write!(f, "the profile has no calibration for this lens"),
            Unsupported::BadShot => write!(f, "the photo's focal length or size is unknown"),
        }
    }
}

/// Radial samples on `[0, 1]` (photo corner = 1).
const SAMPLES: usize = 96;

/// `r_d / r_u` of a distortion model with the centre scale divided out, at lensfun radius `x`.
fn distortion_ratio(m: &DistortionModel, x: f64) -> f64 {
    let safe = |d: f64| if d.abs() < 1e-6 { 1.0 } else { d };
    match *m {
        DistortionModel::PtLens { a, b, c } => {
            let d = safe(1.0 - a - b - c);
            1.0 + (c / (d * d)) * x + (b / (d * d * d)) * x * x + (a / (d * d * d * d)) * x * x * x
        }
        DistortionModel::Poly3 { k1 } => {
            let d = safe(1.0 - k1);
            1.0 + (k1 / (d * d * d)) * x * x
        }
        DistortionModel::Poly5 { k1, k2 } => 1.0 + k1 * x * x + k2 * x.powi(4),
    }
}

/// TCA scale of the red (`blue = false`) or blue plane at lensfun radius `x`.
fn tca_ratio(m: &TcaModel, x: f64, blue: bool) -> f64 {
    match (*m, blue) {
        (TcaModel::Linear { kr, .. }, false) => kr,
        (TcaModel::Linear { kb, .. }, true) => kb,
        (TcaModel::Poly3 { vr, cr, br, .. }, false) => br * x * x + cr * x + vr,
        (TcaModel::Poly3 { vb, cb, bb, .. }, true) => bb * x * x + cb * x + vb,
    }
}

/// The entries at the calibrated focal lengths around `f` and the weight of the upper one.
fn bracket<T: Copy>(items: &[(f64, T)], f: f64) -> Option<(T, T, f64)> {
    let below = items.iter().filter(|(k, _)| *k <= f).max_by(|a, b| a.0.total_cmp(&b.0));
    let above = items.iter().filter(|(k, _)| *k >= f).min_by(|a, b| a.0.total_cmp(&b.0));
    match (below, above) {
        (Some(&(k0, v0)), Some(&(k1, v1))) => {
            let t = if (k1 - k0).abs() < 1e-9 { 0.0 } else { (f - k0) / (k1 - k0) };
            Some((v0, v1, t.clamp(0.0, 1.0)))
        }
        (Some(&(_, v)), None) | (None, Some(&(_, v))) => Some((v, v, 0.0)),
        (None, None) => None,
    }
}

/// Fit `y(r)` (sampled on `[0, 1]`) with `Σ c_p·r^p` over `powers`.
fn fit_curve(y: impl Fn(f64) -> f64, powers: &[i32]) -> Option<Vec<f64>> {
    let mut a = Vec::with_capacity(SAMPLES * powers.len());
    let mut b = Vec::with_capacity(SAMPLES);
    for i in 0..SAMPLES {
        let r = i as f64 / (SAMPLES - 1) as f64;
        a.extend(powers.iter().map(|p| r.powi(*p)));
        b.push(y(r));
    }
    least_squares(&a, &b, SAMPLES, powers.len())
}

/// Vignetting coefficients interpolated for one shot.
fn vignetting_at(points: &[Vignetting], focal: f64, aperture: f64, distance: f64) -> Option<[f64; 3]> {
    let stops = |n: f64| n.max(0.5).log2();
    let dioptre = |d: f64| 1.0 / d.max(0.05);
    let lerp = |a: [f64; 3], b: [f64; 3], t: f64| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
    let at_focal = |f: f64| -> Option<[f64; 3]> {
        let here: Vec<&Vignetting> = points.iter().filter(|p| (p.focal - f).abs() < 1e-9).collect();
        let mut by_stop: Vec<(f64, f64)> = here.iter().map(|p| (stops(p.aperture), p.aperture)).collect();
        by_stop.sort_by(|a, b| a.0.total_cmp(&b.0));
        by_stop.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-9);
        let (n0, n1, t) = bracket(&by_stop, stops(aperture))?;
        let at_aperture = |n: f64| -> Option<[f64; 3]> {
            let mut by_d: Vec<(f64, [f64; 3])> = here.iter().filter(|p| (p.aperture - n).abs() < 1e-9).map(|p| (dioptre(p.distance), p.k)).collect();
            by_d.sort_by(|a, b| a.0.total_cmp(&b.0));
            let (k0, k1, t) = bracket(&by_d, dioptre(distance))?;
            Some(lerp(k0, k1, t))
        };
        Some(lerp(at_aperture(n0)?, at_aperture(n1)?, t))
    };
    let mut focals: Vec<(f64, f64)> = points.iter().map(|p| (p.focal, p.focal)).collect();
    focals.sort_by(|a, b| a.0.total_cmp(&b.0));
    focals.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-9);
    let (f0, f1, t) = bracket(&focals, focal)?;
    Some(lerp(at_focal(f0)?, at_focal(f1)?, t))
}

impl Lens {
    /// This lens's correction for `shot`, in the pipeline's terms (centred, `r = 1` at the photo's corner).
    pub fn correction(&self, shot: &Shot) -> Result<EmbeddedLens, Unsupported> {
        if let LensKind::Other(p) = &self.kind {
            return Err(Unsupported::Projection(p.clone()));
        }
        let ok = |v: f64| v.is_finite() && v > 0.0;
        if !(ok(shot.focal) && ok(shot.crop) && ok(shot.width) && ok(shot.height) && ok(self.crop) && ok(self.aspect)) {
            return Err(Unsupported::BadShot);
        }
        // the photo's corner in lensfun's distortion and vignetting units
        let crop = shot.picture_crop();
        let s_dist = self.crop * (1.0 + self.aspect * self.aspect).sqrt() / crop;
        let s_vig = self.crop / crop;
        let centre = Point::new(0.5, 0.5);
        let radius = shot.width.hypot(shot.height) / 2.0 / shot.width.max(shot.height);

        let dist: Vec<(f64, DistortionModel)> = self.distortion.iter().map(|d| (d.focal, d.model)).collect();
        let tca: Vec<(f64, TcaModel)> = self.tca.iter().map(|t| (t.focal, t.model)).collect();
        let dist_at = bracket(&dist, shot.focal);
        let tca_at = bracket(&tca, shot.focal);
        let g = |x: f64| match dist_at {
            Some((m0, m1, t)) => distortion_ratio(&m0, x) * (1.0 - t) + distortion_ratio(&m1, x) * t,
            None => 1.0,
        };
        let h = |x: f64, blue: bool| match tca_at {
            Some((m0, m1, t)) => tca_ratio(&m0, x, blue) * (1.0 - t) + tca_ratio(&m1, x, blue) * t,
            None => 1.0,
        };
        let warp = if dist_at.is_some() || tca_at.is_some() {
            // per plane: source radius / output radius; TCA scales the distorted radius of red and blue
            let plane = |ch: usize| {
                fit_curve(
                    |r| {
                        let x = r * s_dist;
                        let gd = g(x);
                        match ch {
                            0 => gd * h(x * gd, false),
                            2 => gd * h(x * gd, true),
                            _ => gd,
                        }
                    },
                    &[0, 1, 2, 3, 4, 5, 6],
                )
            };
            match (plane(0), plane(1), plane(2)) {
                (Some(r), Some(gr), Some(b)) => {
                    // a correction that would fetch pixels from beyond the frame (pincushion) leaves empty corners:
                    // zoom in just enough that the border samples inside the picture
                    let edge = shot.width.min(shot.height) / 2.0 / (shot.width.hypot(shot.height) / 2.0);
                    let eval = |c: &[f64], r: f64| c.iter().enumerate().map(|(p, k)| k * r.powi(p as i32)).sum::<f64>();
                    let mut reach = 1.0f64;
                    for i in 0..=32 {
                        let rr = edge + (1.0 - edge) * i as f64 / 32.0;
                        for c in [&r, &gr, &b] {
                            reach = reach.max(eval(c, rr));
                        }
                    }
                    let to_planes = |c: &[f64]| -> ([f64; 6], [f64; 3]) {
                        let k = |i: usize| c.get(i).copied().unwrap_or(0.0) / reach;
                        ([k(0), k(2), k(4), k(6), 0.0, 0.0], [k(1), k(3), k(5)])
                    };
                    let (pr, or) = to_planes(&r);
                    let (pg, og) = to_planes(&gr);
                    let (pb, ob) = to_planes(&b);
                    Some(EmbeddedWarp { planes: [pr, pg, pb], odd: [or, og, ob], center: centre, radius })
                }
                _ => None,
            }
        } else {
            None
        };

        let vignette = shot.aperture.filter(|n| ok(*n)).and_then(|n| {
            let k = vignetting_at(&self.vignetting, shot.focal, n, shot.distance.filter(|d| ok(*d)).unwrap_or(1000.0))?;
            let attenuation = |r: f64| {
                let x2 = (r * s_vig).powi(2);
                1.0 + k[0] * x2 + k[1] * x2 * x2 + k[2] * x2 * x2 * x2
            };
            // a model that darkens the corner to (almost) nothing can't be undone: leave vignetting alone
            if (0..SAMPLES).any(|i| attenuation(i as f64 / (SAMPLES - 1) as f64) < 0.05) {
                return None;
            }
            let c = fit_curve(|r| 1.0 / attenuation(r) - 1.0, &[2, 4, 6, 8, 10])?;
            let k = |i: usize| c.get(i).copied().unwrap_or(0.0);
            Some(EmbeddedVignette { k: [k(0), k(1), k(2), k(3), k(4)], center: centre, radius })
        });

        if warp.is_none() && vignette.is_none() {
            return Err(Unsupported::NoCalibration);
        }
        Ok(EmbeddedLens { warp, vignette })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::tests::sample;
    use crate::{Distortion, Tca};

    /// The pipeline's evaluation of a fitted plane: source radius / output radius at `r` (corner = 1).
    fn plane_ratio(w: &EmbeddedWarp, ch: usize, r: f64) -> f64 {
        let p = w.planes[ch];
        let o = w.odd[ch];
        let r2 = r * r;
        p[0] + p[1] * r2 + p[2] * r2 * r2 + p[3] * r2 * r2 * r2 + r * (o[0] + o[1] * r2 + o[2] * r2 * r2)
    }

    fn shot(focal: f64, aperture: Option<f64>, crop: f64) -> Shot {
        Shot { focal, aperture, distance: None, crop, sensor_aspect: None, width: 5184.0, height: 3456.0 }
    }

    #[test]
    fn ptlens_distortion_is_exact_in_the_pipeline_basis() {
        let lens = Lens {
            model: "T".into(),
            crop: 1.613,
            aspect: 1.5,
            distortion: vec![Distortion { focal: 15.0, model: DistortionModel::PtLens { a: 0.0184, b: -0.03224, c: -0.03581 } }],
            ..Default::default()
        };
        let c = lens.correction(&shot(15.0, None, 1.613)).unwrap();
        let w = c.warp.unwrap();
        assert!(c.vignette.is_none(), "no aperture, no vignetting");
        let s = 1.613 * (1.0f64 + 2.25).sqrt() / 1.613;
        let half_diag_px = 5184.0f64.hypot(3456.0) / 2.0;
        for i in 0..=100 {
            let r = i as f64 / 100.0;
            let want = distortion_ratio(&DistortionModel::PtLens { a: 0.0184, b: -0.03224, c: -0.03581 }, r * s);
            let err_px = (plane_ratio(&w, 1, r) - want).abs() * r * half_diag_px;
            assert!(err_px < 1e-6, "r {r}: {err_px} px");
        }
        assert_eq!(w.planes[0], w.planes[1], "no TCA: every plane alike");
        assert_eq!((w.center, w.radius), (Point::new(0.5, 0.5), 5184.0f64.hypot(3456.0) / 2.0 / 5184.0));
    }

    #[test]
    fn poly3_has_the_centre_scale_divided_out() {
        // lensfun's poly3 r_d = r·(1 − k1 + k1·r²) scaled to keep the centre: 1 + k1/(1−k1)³·r²
        let k1 = -0.05;
        assert!((distortion_ratio(&DistortionModel::Poly3 { k1 }, 0.0) - 1.0).abs() < 1e-15);
        let d = 1.0 - k1;
        assert!((distortion_ratio(&DistortionModel::Poly3 { k1 }, 0.7) - (1.0 + k1 / (d * d * d) * 0.49)).abs() < 1e-15);
        // ptlens with only a b term is the same curve as the equivalent poly3
        let pt = DistortionModel::PtLens { a: 0.0, b: k1, c: 0.0 };
        assert!((distortion_ratio(&pt, 0.7) - distortion_ratio(&DistortionModel::Poly3 { k1 }, 0.7)).abs() < 1e-15);
    }

    #[test]
    fn tca_scales_red_and_blue_around_the_distorted_radius() {
        let lens = Lens {
            model: "T".into(),
            crop: 1.0,
            aspect: 1.5,
            distortion: vec![Distortion { focal: 50.0, model: DistortionModel::Poly5 { k1: -0.02, k2: 0.001 } }],
            tca: vec![Tca { focal: 50.0, model: TcaModel::Linear { kr: 1.0005, kb: 0.9995 } }],
            ..Default::default()
        };
        let w = lens.correction(&shot(50.0, None, 1.0)).unwrap().warp.unwrap();
        for r in [0.0, 0.3, 0.7, 1.0] {
            let g = plane_ratio(&w, 1, r);
            assert!((plane_ratio(&w, 0, r) - g * 1.0005).abs() < 1e-9 && (plane_ratio(&w, 2, r) - g * 0.9995).abs() < 1e-9);
        }
    }

    #[test]
    fn interpolates_between_calibrated_focal_lengths() {
        let db = sample();
        let lens = &db.lenses[0];
        let at = |f: f64| plane_ratio(&lens.correction(&shot(f, None, 1.6)).unwrap().warp.unwrap(), 1, 1.0);
        let (w15, w85, w50) = (at(15.0), at(85.0), at(50.0));
        assert!((w50 - (w15 + (w85 - w15) * (35.0 / 70.0))).abs() < 1e-9, "{w15} {w50} {w85}");
        assert_eq!(at(10.0), w15, "below the calibration: the nearest");
        assert_eq!(at(200.0), w85);
    }

    #[test]
    fn pincushion_corrections_zoom_to_fill_the_frame() {
        let lens = Lens {
            model: "Tele".into(),
            crop: 1.0,
            aspect: 1.5,
            distortion: vec![Distortion { focal: 200.0, model: DistortionModel::Poly3 { k1: 0.02 } }],
            ..Default::default()
        };
        let w = lens.correction(&shot(200.0, None, 1.0)).unwrap().warp.unwrap();
        // every point of the border samples inside the picture, and the tightest one sits right on it
        let edge = 3456.0 / 5184.0f64.hypot(3456.0);
        let reach: Vec<f64> = (0..=40).map(|i| plane_ratio(&w, 1, edge + (1.0 - edge) * i as f64 / 40.0)).collect();
        let max = reach.iter().copied().fold(0.0, f64::max);
        assert!(max <= 1.0 + 1e-9 && max > 1.0 - 1e-6, "{max}");
    }

    #[test]
    fn a_crop_of_the_sensor_has_a_larger_crop_factor() {
        let mut s = shot(10.0, None, 2.73);
        s.sensor_aspect = Some(1.5);
        assert_eq!(s.picture_crop(), 2.73, "3:2 from a 3:2 sensor");
        s.width = 4864.0;
        s.height = 3648.0;
        // 4:3 keeps the height of a 3:2 sensor: diagonal sqrt(1.333² + 1) vs sqrt(1.5² + 1)
        assert!((s.picture_crop() - 2.73 * (3.25f64).sqrt() / (16.0f64 / 9.0 + 1.0).sqrt()).abs() < 1e-9);
        s.width = 5472.0;
        s.height = 3078.0;
        assert!(s.picture_crop() > 2.73, "16:9 keeps the width");
        s.sensor_aspect = None;
        assert_eq!(s.picture_crop(), 2.73, "unknown sensor shape: the camera's");
    }

    #[test]
    fn vignetting_follows_aperture_and_crop() {
        let db = sample();
        let lens = &db.lenses[0];
        let gain = |n: f64, crop: f64| {
            let v = lens.correction(&shot(15.0, Some(n), crop)).unwrap().vignette.unwrap();
            1.0 + v.k.iter().sum::<f64>() // at the corner
        };
        let (k1, k2, k3): (f64, f64, f64) = (-0.3, 0.1, -0.05);
        assert!((gain(3.5, 1.6) - 1.0 / (1.0 + k1 + k2 + k3)).abs() < 2e-3, "{}", gain(3.5, 1.6));
        assert!((gain(8.0, 1.6) - 1.0 / (1.0f64 - 0.1)).abs() < 2e-3);
        // half way in stops between f/3.5 and f/8: the coefficients half way
        let mid = (3.5f64.log2() + 8f64.log2()) / 2.0;
        let k = [(k1 - 0.1) / 2.0, k2 / 2.0, k3 / 2.0];
        assert!((gain(mid.exp2(), 1.6) - 1.0 / (1.0 + k.iter().sum::<f64>())).abs() < 2e-3);
        // a smaller sensor sees less of the falloff
        assert!(gain(3.5, 2.4) < gain(3.5, 1.6));
    }

    #[test]
    fn refuses_what_it_cannot_correct() {
        let db = sample();
        assert_eq!(db.lenses[3].correction(&shot(8.0, None, 1.6)), Err(Unsupported::Projection("fisheye".into())));
        assert_eq!(db.lenses[0].correction(&shot(0.0, None, 1.6)), Err(Unsupported::BadShot));
        assert_eq!(db.lenses[0].correction(&Shot { crop: f64::NAN, ..shot(15.0, None, 1.6) }), Err(Unsupported::BadShot));
        let none = Lens { model: "N".into(), crop: 1.0, aspect: 1.5, ..Default::default() };
        assert_eq!(none.correction(&shot(50.0, Some(2.8), 1.0)), Err(Unsupported::NoCalibration));
        // a model that would divide by ~zero at the corner leaves vignetting out instead
        let dark = Lens {
            model: "D".into(),
            crop: 1.0,
            aspect: 1.5,
            vignetting: vec![Vignetting { focal: 50.0, aperture: 2.0, distance: 1000.0, k: [-1.0, 0.0, 0.0] }],
            ..Default::default()
        };
        assert_eq!(dark.correction(&shot(50.0, Some(2.0), 1.0)), Err(Unsupported::NoCalibration));
    }
}
