//! Finding a photo's camera and lens in the database from its Exif names.
//!
//! Cameras match on the normalized maker + model (`"Canon"` + `"Canon EOS REBEL T2i"`, however the photo spells
//! the maker). Lenses match word by word: every word of the database name (apart from the maker) must appear in the
//! photo's lens name, the lens must fit the camera's mount and cover the focal length, and of the candidates the one
//! with the most words in common and the fewest extra ones wins (so "EF-S18-55mm f/3.5-5.6 IS" is the "IS" lens,
//! not "IS II" or the plain one). A camera with a built-in lens (a compact) needs no lens name: its mount is unique
//! to it, and the one calibrated lens on that mount is it.

use crate::{Camera, Database, Lens};

/// What is known about a photo.
#[derive(Clone, Debug, Default)]
pub struct Query<'a> {
    /// Camera maker and model, together (`"Canon Canon EOS REBEL T2i"`) or as separate words.
    pub camera: &'a str,
    /// The lens name from the photo's metadata (Exif `LensModel`, or a maker note's); may be empty.
    pub lens: &'a str,
    /// Focal length in mm, when recorded.
    pub focal: Option<f64>,
    /// Crop factor implied by the photo (35 mm-equivalent focal / focal), used when the camera isn't in the database.
    pub crop: Option<f64>,
}

/// The lens chosen for a photo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LensMatch<'a> {
    pub lens: &'a Lens,
    pub camera: Option<&'a Camera>,
    /// The photo's crop factor: the camera's from the database, else the query's, else the lens calibration's.
    pub crop: f64,
    /// Matched without a lens name, because the camera has its own lens.
    pub fixed_lens: bool,
}

/// Lower case, punctuation other than `. / - +` to spaces, single spaces.
pub fn normalize(s: &str) -> String {
    let mapped: String =
        s.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '/' | '-' | '+') { c.to_ascii_lowercase() } else { ' ' }).collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Words of a lens name, with a letter prefix split from the focal length it is written against
/// (`ef-s15-85mm` → `ef-s`, `15-85mm`; `f/3.5-5.6` stays whole).
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for w in normalize(s).split(' ').filter(|w| !w.is_empty()) {
        if w.starts_with("f/") || w.starts_with("1:") {
            out.push(w.to_string());
            continue;
        }
        let split = w
            .char_indices()
            .find(|&(i, c)| c.is_ascii_digit() && i > 0 && w.get(..i).is_some_and(|p| p.chars().any(|ch| ch.is_alphabetic()) && !p.ends_with('.')));
        match split {
            Some((i, _)) => {
                let (head, tail) = (w.get(..i).unwrap_or(w), w.get(i..).unwrap_or(""));
                // `dmc-zr1`-style model numbers are one word: only split before a focal length
                if tail.contains("mm") {
                    out.push(head.trim_end_matches('-').to_string());
                    out.push(tail.to_string());
                } else {
                    out.push(w.to_string());
                }
            }
            None => out.push(w.to_string()),
        }
    }
    // versions: "MkII", "Mk II", "Mark II" and "II" are the same word
    let mut words = Vec::with_capacity(out.len());
    for w in out {
        if w == "mk" || w == "mark" {
            continue;
        }
        let version = w.strip_prefix("mk").filter(|v| !v.is_empty() && v.chars().all(|c| matches!(c, 'i' | 'v' | 'x')));
        words.push(version.map(str::to_string).unwrap_or(w));
    }
    words.retain(|w| !w.is_empty());
    words
}

impl Database {
    /// The camera whose maker + model (or model alone) is `name`.
    pub fn find_camera(&self, name: &str) -> Option<&Camera> {
        let n = normalize(name);
        if n.is_empty() {
            return None;
        }
        self.cameras.iter().find(|c| {
            let model = normalize(&c.model);
            normalize(&format!("{} {}", c.maker, c.model)) == n || model == n
        })
    }

    /// The lens for a photo, if the database has a calibrated, rectilinear one that fits.
    pub fn find_lens(&self, q: &Query<'_>) -> Option<LensMatch<'_>> {
        let camera = self.find_camera(q.camera);
        let crop_of = |lens: &Lens| camera.map(|c| c.crop).or(q.crop).filter(|c| c.is_finite() && *c > 0.0).unwrap_or(lens.crop);
        let fits = |lens: &Lens| {
            lens.has_calibration()
                && camera.is_none_or(|c| lens.mounts.iter().any(|m| self.mount_fits(&c.mount, m)))
                && q.focal.zip(lens.focal).is_none_or(|(f, (lo, hi))| f >= lo * 0.97 - 0.5 && f <= hi * 1.03 + 0.5)
        };
        let lens_words = words(q.lens);
        if !lens_words.is_empty() {
            let mut skip: Vec<String> = Vec::new();
            for maker in camera.map(|c| c.maker.as_str()).into_iter().chain(q.camera.split_whitespace().take(1)) {
                skip.extend(words(maker));
            }
            let mut best: Option<(i64, f64, &Lens)> = None;
            for lens in self.lenses.iter().filter(|l| fits(l)) {
                let mut own_skip = skip.clone();
                own_skip.extend(words(&lens.maker));
                let wanted: Vec<String> = words(&lens.model).into_iter().filter(|w| !own_skip.contains(w)).collect();
                if wanted.is_empty() || !wanted.iter().all(|w| lens_words.contains(w)) {
                    continue;
                }
                let extra = lens_words.iter().filter(|w| !wanted.contains(w) && !own_skip.contains(w)).count() as i64;
                let score = wanted.len() as i64 * 10 - extra;
                // the same lens is often calibrated on several sensor sizes: prefer the one nearest this camera's
                let crop_gap = (lens.crop / crop_of(lens)).ln().abs();
                if best.is_none_or(|(s, gap, _)| score > s || (score == s && crop_gap < gap)) {
                    best = Some((score, crop_gap, lens));
                }
            }
            if let Some((_, _, lens)) = best {
                return Some(LensMatch { lens, camera, crop: crop_of(lens), fixed_lens: false });
            }
        }
        // a camera with a built-in lens: the one calibrated lens made for its own mount
        let camera_ref = camera?;
        let mut own = self.lenses.iter().filter(|l| fits(l) && l.mounts.iter().any(|m| m.eq_ignore_ascii_case(&camera_ref.mount)));
        let lens = own.next()?;
        let only_one = own.next().is_none();
        let interchangeable = self.lenses.iter().filter(|l| l.mounts.iter().any(|m| m.eq_ignore_ascii_case(&camera_ref.mount))).count() > 1;
        (only_one && !interchangeable).then(|| LensMatch { lens, camera, crop: crop_of(lens), fixed_lens: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::tests::sample;

    #[test]
    fn splits_lens_names_like_cameras_write_them() {
        assert_eq!(words("EF-S15-85mm f/3.5-5.6 IS USM"), ["ef-s", "15-85mm", "f/3.5-5.6", "is", "usm"]);
        assert_eq!(words("Canon EF-S 15-85mm f/3.5-5.6 IS USM"), ["canon", "ef-s", "15-85mm", "f/3.5-5.6", "is", "usm"]);
        assert_eq!(words("EF50mm f/1.8 II"), ["ef", "50mm", "f/1.8", "ii"]);
        assert_eq!(words("DMC-ZR1"), ["dmc-zr1"]);
        assert_eq!(words("Canon EF 50mm f/1.8 MkII"), words("Canon EF 50mm f/1.8 Mark II"));
        assert_eq!(words("EF50mm f/1.8 II"), ["ef", "50mm", "f/1.8", "ii"]);
        assert_eq!(words("EF 24-70mm f/2.8L II USM"), ["ef", "24-70mm", "f/2.8l", "ii", "usm"]);
        assert_eq!(normalize("RICOH IMAGING COMPANY, LTD. RICOH GR IV"), "ricoh imaging company ltd. ricoh gr iv");
    }

    #[test]
    fn finds_cameras_by_maker_and_model() {
        let db = sample();
        assert_eq!(db.find_camera("Testmaker Testmaker Body 1").map(|c| c.crop), Some(1.6));
        assert_eq!(db.find_camera("TESTMAKER BODY 1").map(|c| c.model.as_str()), Some("Testmaker Body 1"));
        assert!(db.find_camera("Other Body").is_none() && db.find_camera("").is_none());
    }

    #[test]
    fn picks_the_lens_with_every_word_and_the_fewest_extra() {
        let db = sample();
        let q = Query { camera: "Testmaker Testmaker Body 1", lens: "DX15-85mm f/3.5-5.6 IS", focal: Some(24.0), crop: None };
        let m = db.find_lens(&q).unwrap();
        assert_eq!((m.lens.model.as_str(), m.crop, m.fixed_lens), ("Testmaker DX 15-85mm f/3.5-5.6 IS", 1.6, false));
        // a compatible mount (FX lens on the DX body)
        let q = Query { lens: "FX50mm f/1.8 II", focal: Some(50.0), ..q };
        assert_eq!(db.find_lens(&q).unwrap().lens.model, "Testmaker FX 50mm f/1.8 II");
        // a focal length the lens doesn't have, or a word the photo's lens lacks
        assert!(db.find_lens(&Query { focal: Some(200.0), ..q.clone() }).is_none());
        assert!(db.find_lens(&Query { lens: "FX50mm f/1.8", ..q.clone() }).is_none(), "the database lens is the II");
        // no camera in the database: no mount check, the query's crop
        let q = Query { camera: "Unknown Body", lens: "DX15-85mm f/3.5-5.6 IS", focal: Some(15.0), crop: Some(1.5) };
        assert_eq!(db.find_lens(&q).unwrap().crop, 1.5);
        // fisheye lenses never match
        assert!(db.find_lens(&Query { camera: "Testmaker Body 1", lens: "Fish 8mm", focal: Some(8.0), crop: None }).is_none());
    }

    #[test]
    fn a_compact_needs_no_lens_name() {
        let db = sample();
        let m = db.find_lens(&Query { camera: "Testmaker Testmaker Pocket", lens: "", focal: Some(6.0), crop: None }).unwrap();
        assert!(m.fixed_lens);
        assert_eq!((m.lens.model.as_str(), m.crop), ("Testmaker Pocket & compatibles", 2.5));
        // an interchangeable-lens body without a lens name: no guess
        assert!(db.find_lens(&Query { camera: "Testmaker Body 1", lens: "", focal: Some(30.0), crop: None }).is_none());
    }
}
