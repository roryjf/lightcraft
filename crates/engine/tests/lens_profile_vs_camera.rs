//! A lensfun profile against the camera's own correction: a Panasonic RW2 carries the distortion correction the
//! camera applies to its JPEGs (read as the photo's embedded lens data). For the same shot, the profile LightCraft
//! derives from the lensfun database must move pixels the same way: this checks the database units (radius
//! normalisation, crop factor and aspect ratio) end to end.
//!
//! Needs `LIGHTCRAFT_LENSFUN_DB` (a folder of the database's `.xml` files) and the corpus file
//! `corpus/rw2-panasonic-fz1000m2-4x3.rw2` (`cargo xtask corpus --download`) or `LIGHTCRAFT_TEST_RW2`; skipped
//! without them.

use lightcraft_catalog::{Photo, PhotoId, Source};
use lightcraft_geom::Point;
use lightcraft_pipeline::optics::embedded_warp;

fn inputs() -> Option<(lightcraft_lensfun::Database, Vec<u8>)> {
    let dir = std::env::var_os("LIGHTCRAFT_LENSFUN_DB")?;
    let rw2 = std::env::var_os("LIGHTCRAFT_TEST_RW2")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/rw2-panasonic-fz1000m2-4x3.rw2"));
    let bytes = std::fs::read(rw2).ok()?;
    let mut db = lightcraft_lensfun::Database::default();
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        if e.path().extension().is_some_and(|x| x == "xml") {
            db.add_xml(&std::fs::read_to_string(e.path()).unwrap()).unwrap();
        }
    }
    Some((db, bytes))
}

#[test]
fn lensfun_profile_matches_the_cameras_own_correction() {
    let Some((db, bytes)) = inputs() else {
        eprintln!("LIGHTCRAFT_LENSFUN_DB / the FZ1000 II corpus file not available: skipped");
        return;
    };
    let info = lightcraft_engine::files::probe_bytes("fz1000m2.rw2", &bytes).unwrap();
    let camera = info.embedded_lens.and_then(|l| l.warp).expect("the RW2 carries the camera's distortion correction");
    let mut p = Photo::new(PhotoId(1), Source::File { path: "fz.rw2".into() }, "fz.rw2", "RW2", info.width, info.height, "2026-01-01");
    p.meta = info.meta.clone();
    let profile = lightcraft_engine::lens_profiles::profile_for(&db, &p, None, None).expect("a lensfun profile for the FZ1000 II");
    let ours = profile.correction.warp.expect("a distortion profile");
    let (w, h) = (f64::from(info.width), f64::from(info.height));
    let half_diag = w.hypot(h) / 2.0;
    // where the green plane of each sends points along the diagonal and both axes, relative to the centre
    let (mut worst, mut scale_num, mut scale_den) = (0.0f64, 0.0f64, 0.0f64);
    let mut pairs = Vec::new();
    for dir in [(1.0, 0.0), (0.0, 1.0), (w / half_diag / 2.0, h / half_diag / 2.0)] {
        for i in 1..=20 {
            let t = i as f64 / 20.0;
            let reach = if dir.1 == 0.0 {
                w / 2.0
            } else if dir.0 == 0.0 {
                h / 2.0
            } else {
                half_diag
            };
            let q = Point::new(w / 2.0 + dir.0 * reach * t, h / 2.0 + dir.1 * reach * t);
            let a = embedded_warp(&camera, q, 1, w, h);
            let b = embedded_warp(&ours, q, 1, w, h);
            let ra = (a.x - w / 2.0).hypot(a.y - h / 2.0);
            let rb = (b.x - w / 2.0).hypot(b.y - h / 2.0);
            scale_num += ra * rb;
            scale_den += rb * rb;
            pairs.push((ra, rb, reach * t));
        }
    }
    // the camera may also zoom the picture to fill the frame: compare the shape after the best overall scale
    let scale = scale_num / scale_den;
    for (ra, rb, r) in &pairs {
        let gap = (ra - rb * scale).abs() / half_diag;
        worst = worst.max(gap);
        eprintln!("r {:.0} px: camera {:.1} px, lensfun {:.1} px (×{scale:.4})", r, ra, rb);
    }
    eprintln!("{}: worst gap {:.3} % of the half diagonal, overall scale {scale:.4}", profile.lens, worst * 100.0);
    assert!(worst < 0.004, "the profile's shape differs from the camera's by {:.2} % of the half diagonal", worst * 100.0);
    assert!((scale - 1.0).abs() < 0.03, "overall scale {scale}");
}
