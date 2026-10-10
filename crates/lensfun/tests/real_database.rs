//! Against a downloaded lensfun database (`LIGHTCRAFT_LENSFUN_DB=<folder of its .xml files>`); skipped without it.
//! The database is never part of the repository: `lightcraft-cli run lensProfiles.download` fetches the pinned copy.

use lightcraft_lensfun::{Database, Query, Shot};

fn load() -> Option<Database> {
    let dir = std::env::var_os("LIGHTCRAFT_LENSFUN_DB")?;
    let mut db = Database::default();
    let mut files = 0;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "xml") {
            let text = std::fs::read_to_string(&path).unwrap();
            db.add_xml(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            files += 1;
        }
    }
    assert!(files > 40, "{files} database files");
    Some(db)
}

#[test]
fn reads_the_whole_database_and_matches_common_bodies_and_lenses() {
    let Some(db) = load() else {
        eprintln!("LIGHTCRAFT_LENSFUN_DB not set: skipped");
        return;
    };
    assert!(db.cameras.len() > 900 && db.lenses.len() > 1400, "{} cameras, {} lenses", db.cameras.len(), db.lenses.len());
    let cases = [
        // (camera as LightCraft stores it, lens name as the photo records it, focal, expected database lens)
        ("Canon Canon EOS REBEL T2i", "EF-S15-85mm f/3.5-5.6 IS USM", 24.0, "Canon EF-S 15-85mm f/3.5-5.6 IS USM"),
        ("Canon Canon EOS Rebel T6i", "EF-S24mm f/2.8 STM", 24.0, "Canon EF-S 24mm f/2.8 STM"),
        ("Canon Canon EOS Rebel T6i", "EF-S18-55mm f/3.5-5.6 IS STM", 35.0, "Canon EF-S 18-55mm f/3.5-5.6 IS STM"),
        ("Canon Canon EOS DIGITAL REBEL XSi", "EF-S18-55mm f/3.5-5.6 IS", 18.0, "Canon EF-S 18-55mm f/3.5-5.6 IS"),
        ("Canon Canon EOS REBEL T3", "EF-S18-55mm f/3.5-5.6 III", 55.0, "Canon EF-S 18-55mm f/3.5-5.6 III"),
        ("Canon Canon EOS Rebel T6i", "EF50mm f/1.8 II", 50.0, "Canon EF 50mm f/1.8 MkII"),
    ];
    for (camera, lens, focal, want) in cases {
        let m = db.find_lens(&Query { camera, lens, focal: Some(focal), crop: None }).unwrap_or_else(|| panic!("no match for {lens}"));
        assert_eq!(m.lens.model, want, "{camera} / {lens}");
        assert!(m.camera.is_some(), "{camera} not found");
        assert!((m.lens.crop / m.crop - 1.0).abs() < 0.1, "{want}: calibrated at crop {} for a crop {} body", m.lens.crop, m.crop);
        let c =
            m.lens.correction(&Shot { focal, aperture: Some(5.6), distance: None, crop: m.crop, sensor_aspect: None, width: 6000.0, height: 4000.0 });
        assert!(c.is_ok(), "{want}: {c:?}");
    }
    // a fixed-lens camera without a lens name
    let m = db.find_lens(&Query { camera: "RICOH IMAGING COMPANY, LTD. RICOH GR IV", lens: "", focal: Some(18.3), crop: None }).unwrap();
    assert!(m.fixed_lens, "{}", m.lens.model);
}
