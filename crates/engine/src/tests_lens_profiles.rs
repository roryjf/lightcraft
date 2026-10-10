//! Lens profiles end to end in a session: matching, storing, rendering, choosing by hand, undo.

use lightcraft_catalog::{Op, Photo, PhotoId, Source};
use lightcraft_develop::{EmbeddedLens, EmbeddedVignette};
use lightcraft_geom::Point;
use serde_json::json;

use crate::Session;

const DB: &str = r#"<lensdatabase version="2">
    <camera><maker>Testmaker</maker><model>Testmaker Body</model><mount>Test DX</mount><cropfactor>1.6</cropfactor></camera>
    <lens><maker>Testmaker</maker><model>Testmaker DX 15-85mm f/3.5-5.6</model><mount>Test DX</mount><cropfactor>1.6</cropfactor>
        <calibration><distortion model="ptlens" focal="15" a="0.02" b="-0.03" c="-0.04"/>
        <vignetting model="pa" focal="15" aperture="4" distance="10" k1="-0.3" k2="0" k3="0"/></calibration></lens>
    <lens><maker>Testmaker</maker><model>Testmaker DX 50mm f/1.8</model><mount>Test DX</mount><cropfactor>1.6</cropfactor>
        <calibration><distortion model="poly3" focal="50" k1="-0.01"/></calibration></lens>
    </lensdatabase>"#;

fn session() -> (Session, [PhotoId; 3]) {
    let mut s = Session::new().with_fs();
    let mut db = lightcraft_lensfun::Database::default();
    db.add_xml(DB).unwrap();
    s.lens_profiles.set_database(db);
    let add = |s: &mut Session, lens: &str, embedded: bool| {
        let id = s.catalog.alloc_photo_id();
        let mut p = Photo::new(id, Source::File { path: format!("/nowhere/{}.jpg", id.0) }, "x.jpg", "JPEG", 6000, 4000, "2026-01-01");
        p.meta.camera = "Testmaker Testmaker Body".into();
        p.meta.lens = lens.into();
        p.meta.focal_mm = Some(15.0);
        p.meta.aperture = Some(4.0);
        if embedded {
            p.embedded_lens = Some(EmbeddedLens {
                warp: None,
                vignette: Some(EmbeddedVignette { k: [0.1, 0.0, 0.0, 0.0, 0.0], center: Point::new(0.5, 0.5), radius: 0.6 }),
            });
        }
        s.commit("add", Op::AddPhoto { photo: Box::new(p) }).unwrap();
        id
    };
    let a = add(&mut s, "DX15-85mm f/3.5-5.6", false);
    let b = add(&mut s, "Unknown 12mm", false);
    let c = add(&mut s, "DX15-85mm f/3.5-5.6", true);
    (s, [a, b, c])
}

#[test]
fn apply_matches_stores_renders_and_undoes() {
    let (mut s, [a, b, c]) = session();
    let before_key = s.media_mut_render_key(a);
    let r = s.execute("lensProfiles.apply", &json!({"all": true, "dryRun": true})).unwrap();
    assert_eq!((&r["matched"], &r["changed"]), (&json!(1), &json!(1)));
    assert!(s.catalog.photo(a).unwrap().profile_lens.is_none(), "a dry run changes nothing");

    let r = s.execute("lensProfiles.apply", &json!({"all": true, "enable": true})).unwrap();
    assert_eq!(r["matched"], json!(1));
    assert_eq!(r["skipped"]["ownCorrection"], json!(1), "the file's own correction wins");
    assert_eq!(r["notMatched"]["noProfile"], json!(1));
    assert_eq!(r["unmatched"][0]["lens"], json!("Unknown 12mm"));
    assert_eq!(r["enabled"], json!(1));
    let pa = s.catalog.photo(a).unwrap();
    let profile = pa.profile_lens.clone().unwrap();
    assert_eq!(profile.lens, "Testmaker DX 15-85mm f/3.5-5.6");
    assert!(pa.develop.optics.lens_profile, "enable turned profile corrections on");
    assert!(s.catalog.photo(b).unwrap().profile_lens.is_none() && s.catalog.photo(c).unwrap().profile_lens.is_none());
    // a JPEG gets the profile in its render, and renders/thumbnails key on it
    assert_eq!(crate::media::source_info(pa).lens, Some(profile.correction));
    assert_ne!(s.media_mut_render_key(a), before_key);

    // again: nothing new to store
    let r = s.execute("lensProfiles.apply", &json!({"all": true})).unwrap();
    assert_eq!((&r["matched"], &r["changed"], &r["unchanged"]), (&json!(1), &json!(0), &json!(1)));

    // one undo step reverts the profile and the switch
    s.execute("edit.undo", &json!({})).unwrap();
    let pa = s.catalog.photo(a).unwrap();
    assert!(pa.profile_lens.is_none() && !pa.develop.optics.lens_profile);
}

#[test]
fn a_profile_chosen_by_hand_is_kept_until_replaced() {
    let (mut s, [a, b, _]) = session();
    s.execute("photo.setLensProfile", &json!({"ids": [b.0], "lens": "Testmaker DX 50mm f/1.8"})).unwrap();
    let pb = s.catalog.photo(b).unwrap().profile_lens.clone().unwrap();
    assert!(pb.manual && pb.lens == "Testmaker DX 50mm f/1.8");
    let r = s.execute("lensProfiles.apply", &json!({"ids": [a.0, b.0]})).unwrap();
    assert_eq!(r["skipped"]["chosenByHand"], json!(1));
    assert_eq!(s.catalog.photo(b).unwrap().profile_lens.as_ref(), Some(&pb));
    // finding names to choose from
    let r = s.execute("lensProfiles.lenses", &json!({"query": "dx 50"})).unwrap();
    assert_eq!(r["lenses"].as_array().unwrap().len(), 1);
    // a wrong name is an error; null removes
    assert!(s.execute("photo.setLensProfile", &json!({"ids": [b.0], "lens": "No Such Lens"})).is_err());
    s.execute("photo.setLensProfile", &json!({"ids": [b.0], "lens": null})).unwrap();
    assert!(s.catalog.photo(b).unwrap().profile_lens.is_none());
}

#[test]
fn without_the_database_the_commands_say_how_to_get_it() {
    let mut s = Session::new();
    let e = s.execute("lensProfiles.apply", &json!({"all": true})).unwrap_err().to_string();
    assert!(e.contains("lensProfiles.download"), "{e}");
    let st = s.execute("lensProfiles.status", &json!({})).unwrap();
    assert_eq!(st["installed"], json!(false));
    assert!(st["attribution"].as_str().unwrap().contains("CC BY-SA 3.0"));
    // no folder (a host without a file system): a message, not a panic
    assert!(s.execute("lensProfiles.download", &json!({})).is_err());
}

impl Session {
    /// The render key of `id`'s loupe-size render (what a cached result is found under).
    fn media_mut_render_key(&mut self, id: PhotoId) -> u64 {
        self.render_job(id, 800, 800, false, true).unwrap().key
    }
}

/// A decoded JPEG renders with its profile (the decoder knows nothing about it: the catalog's lens data must reach
/// the pipeline), and only while Enable Profile Corrections is on.
#[test]
fn a_decoded_jpeg_renders_with_its_profile() {
    let (mut s, _) = session();
    let dir = std::env::temp_dir().join(format!("lc-lensprofile-jpeg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // a grid of lines, so a radial warp moves visible edges
    let (w, h) = (360u32, 240u32);
    let data: Vec<[u8; 4]> = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            let v = if x % 30 < 3 || y % 30 < 3 { 20 } else { 230 };
            [v, v, v, 255]
        })
        .collect();
    let img = lightcraft_raster::Rgba8 { width: w as usize, height: h as usize, data };
    let jpeg = lightcraft_codecs::encode_jpeg(
        &lightcraft_codecs::EncodeImage::rgba8(&img),
        95,
        lightcraft_codecs::ChromaSubsampling::S444,
        &lightcraft_codecs::EncodeMeta::default(),
    )
    .unwrap();
    let path = dir.join("grid.jpg");
    std::fs::write(&path, jpeg).unwrap();
    let r = s.execute("library.import", &json!({"paths": [path.to_string_lossy()]})).unwrap();
    let id = PhotoId(r["imported"][0].as_u64().unwrap());
    let mut meta = s.catalog.photo(id).unwrap().meta.clone();
    meta.camera = "Testmaker Testmaker Body".into();
    meta.lens = "DX15-85mm f/3.5-5.6".into();
    meta.focal_mm = Some(15.0);
    s.commit("meta", Op::SetMeta { id, meta: Box::new(meta) }).unwrap();
    s.execute("lensProfiles.apply", &json!({"ids": [id.0]})).unwrap();
    assert!(s.catalog.photo(id).unwrap().profile_lens.is_some());
    let render = |s: &mut Session, on: bool| {
        let mut d = (*s.catalog.photo(id).unwrap().develop).clone();
        d.optics.lens_profile = on;
        s.set_develop(id, d, "test").unwrap();
        s.render_now(id, 360, 360).unwrap().image
    };
    let off = render(&mut s, false);
    let on = render(&mut s, true);
    let changed = off.data.iter().zip(&on.data).filter(|(a, b)| a != b).count();
    assert!(changed > off.data.len() / 20, "the profile changed {changed} of {} pixels", off.data.len());
    let _ = std::fs::remove_dir_all(&dir);
}

/// With the database loaded, an import matches the new photo by itself.
#[test]
fn imports_get_their_profile() {
    let (mut s, _) = session();
    let dir = std::env::temp_dir().join(format!("lc-lensprofile-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // a JPEG whose Exif names camera, lens and focal length (as a camera would)
    let img = lightcraft_raster::Rgba8 { width: 32, height: 24, data: vec![[128, 128, 128, 255]; 32 * 24] };
    let exif = lightcraft_meta::write_exif(&lightcraft_meta::Metadata {
        make: Some("Testmaker".into()),
        model: Some("Testmaker Body".into()),
        lens_model: Some("DX15-85mm f/3.5-5.6".into()),
        focal_length: Some(15.0),
        ..Default::default()
    });
    let meta = lightcraft_codecs::EncodeMeta { exif: Some(&exif), ..Default::default() };
    let jpeg =
        lightcraft_codecs::encode_jpeg(&lightcraft_codecs::EncodeImage::rgba8(&img), 90, lightcraft_codecs::ChromaSubsampling::S444, &meta).unwrap();
    let path = dir.join("shot.jpg");
    std::fs::write(&path, jpeg).unwrap();
    let r = s.execute("library.import", &json!({"paths": [path.to_string_lossy()]})).unwrap();
    let id = PhotoId(r["imported"][0].as_u64().unwrap());
    let p = s.catalog.photo(id).unwrap();
    assert_eq!(p.meta.lens, "DX15-85mm f/3.5-5.6", "{:?}", p.meta);
    assert_eq!(p.profile_lens.as_ref().map(|x| x.lens.as_str()), Some("Testmaker DX 15-85mm f/3.5-5.6"));
    let _ = std::fs::remove_dir_all(&dir);
}
