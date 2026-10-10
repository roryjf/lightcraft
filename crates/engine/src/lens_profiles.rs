//! Lens profiles from the lensfun database: a pinned copy downloaded on request, loaded when needed, and matched to
//! photos ([`profile_for`]).
//!
//! The database (CC BY-SA 3.0, the lensfun project) is never part of LightCraft. `lensProfiles.download` fetches the
//! XML files of one pinned lensfun commit ([`LENSFUN_COMMIT`], file sizes and SHA-256 in [`files::FILES`]) with
//! [`lightcraft_fetch`] on a background thread into `<lens profiles folder>/<commit>/`; only when every file has
//! arrived and checked out is an `installed.json` marker written, so a half-finished download is never loaded. See
//! `docs/lens-profiles.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lightcraft_catalog::{LensProfile, Photo};
use lightcraft_lensfun::{Database, Query, Shot};

#[path = "lens_profile_files.rs"]
pub mod files;

/// The lensfun commit whose database is downloaded.
pub const LENSFUN_COMMIT: &str = "43f9e001ab66c4fcdd4f400463f13b72f94b2288";
/// Where the pinned files are fetched from (each file is `<base>/<name>`).
pub const MIRROR: &str = "https://raw.githubusercontent.com/lensfun/lensfun/43f9e001ab66c4fcdd4f400463f13b72f94b2288/data/db";
/// Credit shown wherever profiles are used (CC BY-SA 3.0 asks for attribution).
pub const ATTRIBUTION: &str = "Lens profiles: the lensfun lens database (https://lensfun.github.io), CC BY-SA 3.0";
/// The marker written once every file of the database has been downloaded and verified.
const MARKER: &str = "installed.json";
/// Largest database file read (the pinned ones are below 1 MB; the list checks exact sizes).
const MAX_FILE: u64 = 16 << 20;

/// `"lensfun 43f9e00"`: what a photo's profile says it came from.
pub fn source_label() -> String {
    format!("lensfun {}", LENSFUN_COMMIT.get(..7).unwrap_or(LENSFUN_COMMIT))
}

/// Where a download stands.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Running { bytes: u64, total: u64 },
    Done,
    Failed(String),
    Cancelled,
}

struct Job {
    state: Mutex<State>,
    cancel: AtomicBool,
}

impl Job {
    fn set(&self, s: State) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = s;
    }
    fn get(&self) -> State {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// The session's lens profiles: where they live, the loaded database and a download in progress.
#[derive(Default)]
pub struct LensProfiles {
    /// The lens profiles folder (`None`: this host keeps none, e.g. the web).
    pub dir: Option<PathBuf>,
    db: Option<Arc<Database>>,
    job: Option<Arc<Job>>,
}

impl Drop for LensProfiles {
    /// Quitting stops a running download (its verified files stay for the next try).
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn database_dir(dir: &Path) -> PathBuf {
    dir.join(LENSFUN_COMMIT.get(..12).unwrap_or(LENSFUN_COMMIT))
}

/// Total size of the pinned database.
pub fn total_bytes() -> u64 {
    files::FILES.iter().map(|(_, size, _)| size).sum()
}

impl LensProfiles {
    /// Whether the pinned database has been downloaded completely.
    pub fn installed(&self) -> bool {
        self.dir.as_deref().is_some_and(|d| database_dir(d).join(MARKER).is_file())
    }

    /// The download's state, if one was started this session.
    pub fn download_state(&self) -> Option<State> {
        self.job.as_ref().map(|j| j.get())
    }

    /// The database, loaded on first use (`Ok(None)`: not downloaded).
    pub fn database(&mut self) -> Result<Option<Arc<Database>>, String> {
        if let Some(db) = &self.db {
            return Ok(Some(db.clone()));
        }
        if !self.installed() {
            return Ok(None);
        }
        let Some(dir) = self.dir.as_deref().map(database_dir) else { return Ok(None) };
        let mut db = Database::default();
        for (name, size, _) in files::FILES {
            let path = dir.join(name);
            let len = std::fs::metadata(&path).map_err(|e| format!("lens profiles: {name}: {e} (download them again)"))?.len();
            if len != *size || len > MAX_FILE {
                return Err(format!("lens profiles: {name} is not the downloaded file (download them again)"));
            }
            let text = std::fs::read_to_string(&path).map_err(|e| format!("lens profiles: {name}: {e}"))?;
            db.add_xml(&text).map_err(|e| format!("lens profiles: {name}: {e}"))?;
        }
        let db = Arc::new(db);
        self.db = Some(db.clone());
        Ok(Some(db))
    }

    /// Use `db` as the loaded database (tests: a small synthetic one instead of the download).
    #[cfg(test)]
    pub(crate) fn set_database(&mut self, db: Database) {
        self.db = Some(Arc::new(db));
    }

    /// Start fetching the pinned database in the background (files already there and verified are kept).
    pub fn start_download(&mut self) -> Result<(), String> {
        let dir = self.dir.clone().ok_or("this build has nowhere to keep lens profiles (the desktop app and the CLI do)")?;
        if self.job.as_ref().is_some_and(|j| matches!(j.get(), State::Running { .. })) {
            return Err("the lens profiles are already downloading".into());
        }
        let target = database_dir(&dir);
        std::fs::create_dir_all(&target).map_err(|e| format!("could not create {}: {e}", target.display()))?;
        let job = Arc::new(Job { state: Mutex::new(State::Running { bytes: 0, total: total_bytes() }), cancel: AtomicBool::new(false) });
        let worker = job.clone();
        std::thread::Builder::new()
            .name("lens-profile-download".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&target, &worker)));
                if outcome.is_err() {
                    worker.set(State::Failed("the download stopped unexpectedly".into()));
                }
            })
            .map_err(|e| format!("could not start the download: {e}"))?;
        self.job = Some(job);
        self.db = None;
        Ok(())
    }

    /// Stop a running download. `true` if there was one.
    pub fn cancel(&mut self) -> bool {
        match &self.job {
            Some(j) if matches!(j.get(), State::Running { .. }) => {
                j.cancel.store(true, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Delete the downloaded database (photos keep the profiles already applied to them).
    pub fn remove(&mut self) -> Result<bool, String> {
        self.cancel();
        self.db = None;
        let Some(dir) = self.dir.as_deref().map(database_dir) else { return Ok(false) };
        if !dir.exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&dir).map_err(|e| format!("could not delete {}: {e}", dir.display()))?;
        Ok(true)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn run(target: &Path, job: &Job) {
    let specs: Vec<lightcraft_fetch::FileSpec<'_>> =
        files::FILES.iter().map(|(name, size, sha)| lightcraft_fetch::FileSpec { name, size: Some(*size), sha256: Some(sha), max: *size }).collect();
    let total = total_bytes();
    let outcome = lightcraft_fetch::download(&specs, &[MIRROR.to_string()], target, &lightcraft_fetch::Options::default(), &job.cancel, &mut |p| {
        job.set(State::Running { bytes: p.done.min(total), total });
    });
    match outcome {
        Ok(()) => {
            let marker = serde_json::json!({"commit": LENSFUN_COMMIT, "files": files::FILES.len(), "licence": "CC BY-SA 3.0", "from": MIRROR});
            match std::fs::write(target.join(MARKER), marker.to_string()) {
                Ok(()) => job.set(State::Done),
                Err(e) => job.set(State::Failed(format!("could not finish the download: {e}"))),
            }
        }
        Err(lightcraft_fetch::DownloadError::Cancelled) => job.set(State::Cancelled),
        Err(lightcraft_fetch::DownloadError::AllMirrorsFailed { errors, .. }) => {
            let detail: String = errors.join("; ").chars().take(300).collect();
            job.set(State::Failed(format!("could not download the lens profiles ({detail}): check the internet connection")));
        }
        Err(e) => job.set(State::Failed(e.to_string())),
    }
}

#[cfg(target_arch = "wasm32")]
fn run(_: &Path, job: &Job) {
    job.set(State::Failed("the web build downloads no lens profiles".into()));
}

/// Why a photo got no profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoProfile {
    /// The camera isn't in the database and the photo names no lens.
    Unknown,
    /// Camera and lens names known, but nothing in the database fits them.
    NoMatch,
    /// The photo records no focal length.
    NoFocal,
    /// The matched lens can't be corrected (fisheye…); the reason.
    Unsupported(String),
}

/// The profile for `p` from `db`, its lens named `lens` (the photo's own lens name when `None`): matched by camera
/// and lens name, or by name alone for `exact_lens` (a lens chosen by hand).
pub fn profile_for(db: &Database, p: &Photo, lens: Option<&str>, exact_lens: Option<&str>) -> Result<LensProfile, NoProfile> {
    let focal = p.meta.focal_mm.map(f64::from).filter(|f| f.is_finite() && *f > 0.0);
    let camera = db.find_camera(&p.meta.camera);
    let chosen = match exact_lens {
        Some(model) => {
            let crop = camera.map(|c| c.crop);
            db.lenses
                .iter()
                .filter(|l| l.model.eq_ignore_ascii_case(model))
                .min_by(|a, b| {
                    let gap = |c: f64| crop.map_or(0.0, |k| (c / k).ln().abs());
                    gap(a.crop).total_cmp(&gap(b.crop))
                })
                .map(|l| (l, crop.unwrap_or(l.crop)))
                .ok_or(NoProfile::NoMatch)?
        }
        None => {
            let name = lens.unwrap_or(&p.meta.lens);
            let q = Query { camera: &p.meta.camera, lens: name, focal, crop: None };
            match db.find_lens(&q) {
                Some(m) => (m.lens, m.crop),
                None if camera.is_none() && name.trim().is_empty() => return Err(NoProfile::Unknown),
                None => return Err(NoProfile::NoMatch),
            }
        }
    };
    let (found, crop) = chosen;
    // a prime's focal length is known from its name even when the photo doesn't record it
    let focal = focal.or(found.focal.filter(|(lo, hi)| lo == hi).map(|(f, _)| f)).ok_or(NoProfile::NoFocal)?;
    // calibrated on this kind of camera (same crop factor): the calibration's frame shape is the sensor's, so a
    // 4:3 or 16:9 picture from it is known to be a crop
    let sensor_aspect = camera.filter(|c| (found.crop / c.crop - 1.0).abs() < 0.1).map(|_| found.aspect);
    let shot = Shot {
        focal,
        aperture: p.meta.aperture.map(f64::from).filter(|a| a.is_finite() && *a > 0.0),
        distance: None,
        crop,
        sensor_aspect,
        width: f64::from(p.width.max(1)),
        height: f64::from(p.height.max(1)),
    };
    let correction = found.correction(&shot).map_err(|e| NoProfile::Unsupported(e.to_string()))?;
    Ok(LensProfile { lens: found.model.clone(), source: source_label(), manual: exact_lens.is_some(), correction })
}

/// The lens name recorded in the first part of a photo's file (Canon's maker note included), for photos imported
/// before LightCraft read it. Reads at most 96 KiB (a JPEG's Exif block is at most 64 KiB, near the start).
pub fn lens_name_from_file(path: &str) -> Option<String> {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(path).ok()?.take(96 << 10).read_to_end(&mut head).ok()?;
    lightcraft_meta::extract(&head).lens_model.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// [`lens_name_from_file`] for many files, read side by side (a library of JPEGs is mostly waiting on the disk).
pub fn lens_names_from_files(paths: &[String]) -> Vec<Option<String>> {
    if paths.len() < 32 {
        return paths.iter().map(|p| lens_name_from_file(p)).collect();
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8);
    let chunk = paths.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let workers: Vec<_> =
            paths.chunks(chunk).map(|part| scope.spawn(move || part.iter().map(|p| lens_name_from_file(p)).collect::<Vec<_>>())).collect();
        // a worker that panicked (it can't: file errors are `None`) leaves its files without a name
        let mut out = Vec::with_capacity(paths.len());
        for (w, part) in workers.into_iter().zip(paths.chunks(chunk)) {
            match w.join() {
                Ok(names) => out.extend(names),
                Err(_) => out.extend(part.iter().map(|_| None)),
            }
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_catalog::{PhotoId, Source};

    const DB: &str = r#"<lensdatabase version="2">
        <camera><maker>Testmaker</maker><model>Testmaker Body</model><mount>Test DX</mount><cropfactor>1.6</cropfactor></camera>
        <lens><maker>Testmaker</maker><model>Testmaker DX 15-85mm f/3.5-5.6</model><mount>Test DX</mount><cropfactor>1.6</cropfactor>
            <calibration><distortion model="ptlens" focal="15" a="0.02" b="-0.03" c="-0.04"/>
            <vignetting model="pa" focal="15" aperture="4" distance="10" k1="-0.3" k2="0" k3="0"/></calibration></lens>
        <lens><maker>Testmaker</maker><model>Testmaker DX 50mm f/1.8</model><mount>Test DX</mount><cropfactor>1.6</cropfactor>
            <calibration><distortion model="poly3" focal="50" k1="-0.01"/></calibration></lens>
        </lensdatabase>"#;

    fn db() -> Database {
        let mut d = Database::default();
        d.add_xml(DB).unwrap();
        d
    }

    fn photo(camera: &str, lens: &str, focal: Option<f32>) -> Photo {
        let mut p = Photo::new(PhotoId(1), Source::File { path: "/x.jpg".into() }, "x.jpg", "JPEG", 6000, 4000, "2026-01-01");
        p.meta.camera = camera.into();
        p.meta.lens = lens.into();
        p.meta.focal_mm = focal;
        p.meta.aperture = Some(4.0);
        p
    }

    #[test]
    fn matches_a_photo_and_says_why_not() {
        let db = db();
        let pr = profile_for(&db, &photo("Testmaker Testmaker Body", "DX15-85mm f/3.5-5.6", Some(15.0)), None, None).unwrap();
        assert_eq!((pr.lens.as_str(), pr.manual), ("Testmaker DX 15-85mm f/3.5-5.6", false));
        assert!(pr.correction.warp.is_some() && pr.correction.vignette.is_some());
        assert!(pr.source.starts_with("lensfun "));
        assert_eq!(profile_for(&db, &photo("Other Body", "", Some(15.0)), None, None), Err(NoProfile::Unknown));
        assert_eq!(profile_for(&db, &photo("Testmaker Body", "Other 35mm", Some(35.0)), None, None), Err(NoProfile::NoMatch));
        assert_eq!(profile_for(&db, &photo("Testmaker Body", "DX15-85mm f/3.5-5.6", None), None, None), Err(NoProfile::NoFocal));
        // a lens name read from the file instead of the catalog's
        assert!(profile_for(&db, &photo("Testmaker Body", "", Some(20.0)), Some("DX15-85mm f/3.5-5.6"), None).is_ok());
    }

    #[test]
    fn a_lens_chosen_by_hand() {
        let db = db();
        // no lens name and no focal length: a prime's focal length comes from its name
        let pr = profile_for(&db, &photo("Testmaker Body", "", None), None, Some("testmaker dx 50mm f/1.8")).unwrap();
        assert_eq!((pr.lens.as_str(), pr.manual), ("Testmaker DX 50mm f/1.8", true));
        assert_eq!(profile_for(&db, &photo("Testmaker Body", "", None), None, Some("No Such Lens")), Err(NoProfile::NoMatch));
    }

    #[test]
    fn nothing_is_loaded_until_the_download_completes() {
        let dir = std::env::temp_dir().join(format!("lc-lensprofiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut lp = LensProfiles::default();
        lp.dir = Some(dir.clone());
        assert!(!lp.installed());
        assert_eq!(lp.database(), Ok(None));
        // a marker without the files is reported, not loaded half-way
        let d = database_dir(&dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(MARKER), "{}").unwrap();
        assert!(lp.installed());
        assert!(lp.database().unwrap_err().contains("download them again"));
        assert_eq!(lp.remove(), Ok(true));
        assert!(!lp.installed() && !d.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_pinned_list_is_sane() {
        assert!(files::FILES.len() > 40);
        assert!(files::FILES.iter().all(|(n, s, h)| n.ends_with(".xml") && *s > 0 && *s < MAX_FILE && h.len() == 64));
        assert!(MIRROR.contains(LENSFUN_COMMIT));
    }
}
