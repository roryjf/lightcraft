//! Lens profiles (the lensfun database, see `crate::lens_profiles` and docs/lens-profiles.md): download the pinned
//! database on request, match photos to it, or choose a profile by hand. A profile is stored on the photo and
//! applied under "Enable Profile Corrections" when the file carries no correction of its own.

use std::collections::BTreeMap;

use lightcraft_catalog::{Op, PhotoId, Source};
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, bool_or, cmd, str_param};
use crate::lens_profiles::{ATTRIBUTION, NoProfile, State, lens_names_from_files, profile_for, source_label, total_bytes};
use crate::{Result, Session};

fn download_row(state: Option<State>) -> Value {
    match state {
        None => Value::Null,
        Some(State::Running { bytes, total }) => json!({"state": "running", "bytes": bytes, "total": total}),
        Some(State::Done) => json!({"state": "done"}),
        Some(State::Failed(why)) => json!({"state": "failed", "error": why}),
        Some(State::Cancelled) => json!({"state": "cancelled"}),
    }
}

/// `lensProfiles.status` → {installed, version, folder, cameras?, lenses?, download, attribution, error?}
fn status(s: &mut Session, _: &Value) -> Result<Value> {
    let installed = s.lens_profiles.installed();
    let loaded = if installed { s.lens_profiles.database() } else { Ok(None) };
    let (cameras, lenses, error) = match &loaded {
        Ok(Some(db)) => (Some(db.cameras.len()), Some(db.lenses.iter().filter(|l| l.has_calibration()).count()), None),
        Ok(None) => (None, None, None),
        Err(e) => (None, None, Some(e.clone())),
    };
    Ok(json!({
        "installed": installed && error.is_none(),
        "version": source_label(),
        "folder": s.lens_profiles.dir.as_ref().map(|d| d.display().to_string()),
        "downloadBytes": total_bytes(),
        "cameras": cameras,
        "lenses": lenses,
        "download": download_row(s.lens_profiles.download_state()),
        "attribution": ATTRIBUTION,
        "error": error,
    }))
}

fn download(s: &mut Session, p: &Value) -> Result<Value> {
    s.lens_profiles.start_download().map_err(|e| bad("lensProfiles.download", e))?;
    if !bool_or(p, "wait", false) {
        return Ok(json!({"started": true, "bytes": total_bytes(), "version": source_label()}));
    }
    // a one-shot caller (the CLI) waits: its process would otherwise end the download with it
    let started = web_time::Instant::now();
    loop {
        match s.lens_profiles.download_state() {
            Some(State::Running { .. }) if started.elapsed() < std::time::Duration::from_secs(600) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Some(State::Done) => return Ok(json!({"installed": true, "bytes": total_bytes(), "version": source_label()})),
            Some(State::Failed(why)) => return Err(bad("lensProfiles.download", why)),
            Some(State::Cancelled) => return Err(bad("lensProfiles.download", "cancelled")),
            _ => return Err(bad("lensProfiles.download", "the download took longer than 10 minutes")),
        }
    }
}

fn cancel(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(json!({"cancelled": s.lens_profiles.cancel()}))
}

fn remove(s: &mut Session, _: &Value) -> Result<Value> {
    let removed = s.lens_profiles.remove().map_err(|e| bad("lensProfiles.remove", e))?;
    Ok(json!({"removed": removed}))
}

fn database(s: &mut Session, c: &str) -> Result<std::sync::Arc<lightcraft_lensfun::Database>> {
    match s.lens_profiles.database() {
        Ok(Some(db)) => Ok(db),
        Ok(None) => Err(bad(c, "the lens profiles aren't downloaded yet: run lensProfiles.download (Settings → Lens Profiles)")),
        Err(e) => Err(bad(c, e)),
    }
}

/// `lensProfiles.lenses {query?, limit?}`: calibrated lenses whose maker + model contain every word of `query`.
fn lenses(s: &mut Session, p: &Value) -> Result<Value> {
    let db = database(s, "lensProfiles.lenses")?;
    let words: Vec<String> =
        lightcraft_lensfun::normalize(str_param(p, "query").unwrap_or("")).split(' ').filter(|w| !w.is_empty()).map(String::from).collect();
    let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(50).clamp(1, 500) as usize;
    let mut seen = std::collections::BTreeSet::new();
    let rows: Vec<Value> = db
        .lenses
        .iter()
        .filter(|l| l.has_calibration())
        .filter(|l| {
            let name = lightcraft_lensfun::normalize(&format!("{} {}", l.maker, l.model));
            words.iter().all(|w| name.contains(w.as_str()))
        })
        .filter(|l| seen.insert(l.model.clone()))
        .take(limit)
        .map(|l| json!({"lens": l.model, "maker": l.maker, "mounts": l.mounts}))
        .collect();
    Ok(json!({"lenses": rows}))
}

/// `lensProfiles.apply {ids?, all?, replace?, enable?, dryRun?}`
fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "lensProfiles.apply";
    let db = database(s, C)?;
    let ids: Vec<PhotoId> =
        if bool_or(p, "all", false) { s.catalog.photos().filter(|ph| ph.in_library()).map(|ph| ph.id).collect() } else { s.targets(p) };
    let (replace, enable, dry) = (bool_or(p, "replace", false), bool_or(p, "enable", false), bool_or(p, "dryRun", false));
    let mut ops = Vec::new();
    let mut by_lens: BTreeMap<String, usize> = BTreeMap::new();
    let mut unmatched: BTreeMap<(String, String), usize> = BTreeMap::new();
    let (mut matched, mut unchanged, mut own, mut manual, mut names_read, mut enabled) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut unknown, mut no_focal, mut unsupported) = (0usize, 0usize, 0usize);
    // photos imported before LightCraft read Canon maker notes have no lens name yet: read those files first, in parallel
    let nameless: Vec<(PhotoId, String)> = ids
        .iter()
        .filter_map(|id| s.catalog.photo(*id))
        .filter(|ph| ph.embedded_lens.is_none() && ph.meta.lens.trim().is_empty() && (replace || !ph.profile_lens.as_ref().is_some_and(|x| x.manual)))
        .filter_map(|ph| match &ph.source {
            Source::File { path } => Some((ph.id, path.clone())),
            _ => None,
        })
        .collect();
    let paths: Vec<String> = nameless.iter().map(|(_, p)| p.clone()).collect();
    let read: std::collections::HashMap<PhotoId, String> =
        nameless.iter().map(|(id, _)| *id).zip(lens_names_from_files(&paths)).filter_map(|(id, n)| n.map(|n| (id, n))).collect();
    for id in ids {
        let Some(ph) = s.catalog.photo(id).map(|x| (**x).clone()) else { continue };
        if ph.embedded_lens.is_some() {
            own += 1;
            continue;
        }
        if ph.profile_lens.as_ref().is_some_and(|x| x.manual) && !replace {
            manual += 1;
            continue;
        }
        let mut file_lens = None;
        if let Some(name) = read.get(&id).cloned() {
            names_read += 1;
            let mut meta = ph.meta.clone();
            meta.lens = name.clone();
            ops.push(Op::SetMeta { id, meta: Box::new(meta) });
            file_lens = Some(name);
        }
        match profile_for(&db, &ph, file_lens.as_deref(), None) {
            Ok(profile) => {
                matched += 1;
                *by_lens.entry(profile.lens.clone()).or_default() += 1;
                if ph.profile_lens.as_ref() == Some(&profile) {
                    unchanged += 1;
                } else {
                    ops.push(Op::SetLensProfile { id, profile: Some(Box::new(profile)) });
                }
                if enable && !ph.develop.optics.lens_profile {
                    let mut d = (*ph.develop).clone();
                    d.optics.lens_profile = true;
                    if let Some(op) = s.develop_op(id, d, "Enable Profile Corrections") {
                        ops.push(op);
                        enabled += 1;
                    }
                }
            }
            Err(why) => {
                match why {
                    NoProfile::Unknown | NoProfile::NoMatch => unknown += 1,
                    NoProfile::NoFocal => no_focal += 1,
                    NoProfile::Unsupported(_) => unsupported += 1,
                }
                let lens = file_lens.unwrap_or_else(|| ph.meta.lens.clone());
                *unmatched.entry((ph.meta.camera.clone(), lens)).or_default() += 1;
            }
        }
    }
    let changed = ops.iter().filter(|o| matches!(o, Op::SetLensProfile { .. })).count();
    if !dry && !ops.is_empty() {
        s.commit("Apply Lens Profiles", Op::Batch { ops })?;
    }
    let mut unmatched: Vec<((String, String), usize)> = unmatched.into_iter().collect();
    unmatched.sort_by_key(|u| std::cmp::Reverse(u.1));
    Ok(json!({
        "matched": matched,
        "changed": changed,
        "unchanged": unchanged,
        "enabled": enabled,
        "lensNamesRead": names_read,
        "skipped": {"ownCorrection": own, "chosenByHand": manual},
        "notMatched": {"noProfile": unknown, "noFocalLength": no_focal, "unsupportedLens": unsupported},
        "byLens": by_lens,
        "unmatched": unmatched.iter().take(30).map(|((camera, lens), n)| json!({"camera": camera, "lens": lens, "photos": n})).collect::<Vec<_>>(),
        "dryRun": dry,
        "attribution": ATTRIBUTION,
    }))
}

/// `photo.setLensProfile {ids?, lens: string | null}`: choose a profile by hand (or remove the profile).
fn set_profile(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "photo.setLensProfile";
    let ids = s.targets(p);
    if ids.is_empty() {
        return Err(bad(C, "no photos selected (or give `ids`)"));
    }
    let lens = match p.get("lens") {
        Some(Value::Null) => None,
        Some(Value::String(l)) if !l.trim().is_empty() => Some(l.trim().to_string()),
        _ => return Err(bad(C, "give `lens` (a name from lensProfiles.lenses) or null to remove the profile")),
    };
    let mut ops = Vec::new();
    let mut failed = Vec::new();
    match lens {
        None => {
            for id in &ids {
                if s.catalog.photo(*id).is_some_and(|ph| ph.profile_lens.is_some()) {
                    ops.push(Op::SetLensProfile { id: *id, profile: None });
                }
            }
        }
        Some(model) => {
            let db = database(s, C)?;
            for id in &ids {
                let Some(ph) = s.catalog.photo(*id).map(|x| (**x).clone()) else { continue };
                match profile_for(&db, &ph, None, Some(&model)) {
                    Ok(profile) => ops.push(Op::SetLensProfile { id: *id, profile: Some(Box::new(profile)) }),
                    Err(NoProfile::NoMatch) => return Err(bad(C, format!("no lens profile named \"{model}\" (see lensProfiles.lenses)"))),
                    Err(NoProfile::NoFocal) => failed.push(json!([id.0, "the photo records no focal length and the lens is a zoom"])),
                    Err(NoProfile::Unsupported(why)) => failed.push(json!([id.0, why])),
                    Err(NoProfile::Unknown) => failed.push(json!([id.0, "no profile"])),
                }
            }
        }
    }
    let changed = ops.len();
    if !ops.is_empty() {
        s.commit("Lens Profile", Op::Batch { ops })?;
    }
    Ok(json!({"changed": changed, "failed": failed}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "lensProfiles.status", "Lens Profiles", [], None,
            "{} — the lensfun lens database: downloaded?, its version, how many cameras and calibrated lenses, a download in progress → {installed, version, folder, downloadBytes, cameras, lenses, download: {state, bytes, total, error}, attribution}",
            always, status),
        cmd!(query "lensProfiles.download", "Download Lens Profiles", [], None,
            "{wait?: bool} — fetch the pinned lensfun database (CC BY-SA 3.0, about 5 MB from GitHub) in the background (`wait`: return when it is done); files already downloaded and verified are kept. Watch lensProfiles.status → {started | installed, bytes, version}",
            always, download),
        cmd!(query "lensProfiles.cancel", "Cancel Lens Profile Download", [], None, "{} → {cancelled}", always, cancel),
        cmd!(query "lensProfiles.remove", "Remove Lens Profiles", [], None,
            "{} — delete the downloaded database (photos keep the profiles they have) → {removed}", always, remove),
        cmd!(query "lensProfiles.lenses", "Find Lens Profiles", [], None,
            "{query?, limit?: 50} — calibrated lenses whose maker and name contain every word of `query` → {lenses: [{lens, maker, mounts}]}",
            always, lenses),
        cmd!(
            "lensProfiles.apply",
            "Apply Lens Profiles",
            [],
            None,
            "{ids?, all?: bool, replace?: bool, enable?: bool, dryRun?: bool} — match the selected photos (or every photo in the library with `all`) to the lensfun database by camera, lens name and focal length and store each one's profile; photos whose file has its own correction are skipped, profiles chosen by hand are kept unless `replace`; lens names missing from the catalog are read from the file (Canon maker notes); `enable` also turns on Enable Profile Corrections for matched photos; one undo step → {matched, changed, unchanged, enabled, lensNamesRead, skipped, notMatched, byLens, unmatched: [{camera, lens, photos}]}",
            always,
            apply
        ),
        cmd!(
            "photo.setLensProfile",
            "Choose Lens Profile",
            [],
            None,
            "{ids?, lens: string | null} — use the named lensfun lens (from lensProfiles.lenses) for the selected photos, interpolated for each photo's focal length and aperture; null removes their profiles; one undo step → {changed, failed: [[id, why]]}",
            always,
            set_profile
        ),
    ]
}
