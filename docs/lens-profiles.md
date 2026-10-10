# Lens profiles (lensfun database)

LightCraft corrects lens distortion, lateral chromatic aberration (colour fringes towards the corners) and vignetting
with profiles from the [lensfun](https://lensfun.github.io/) lens database, matched to each photo's camera, lens and
focal length. The correction is used under **Enable Profile Corrections** (Edit → Optics) with the profile
Distortion / Vignetting amounts, exactly like the corrections some files carry themselves.

## Using it

1. **Settings → Lens Profiles → Download Lens Profiles** (about 5 MB). Or `lightcraft-cli run lensProfiles.download wait=true`.
2. **Apply to Library** matches every photo already in the library (new imports are matched as they arrive) (`lensProfiles.apply all=true`); tick *Also turn on Enable Profile
   Corrections* to switch the corrections on for the photos that got a profile (`enable=true`). One undo step.
3. Edit → Optics names the profile a photo uses, or says why it has none.

A lens the photo doesn't name (old lenses, adapted manual lenses) can be chosen by hand:
`lensProfiles.lenses query="50mm 1.8"` lists names, `photo.setLensProfile lens="Canon EF 50mm f/1.8 MkII"` applies
one to the selected photos (interpolated for each photo's own focal length and aperture); a later automatic
`lensProfiles.apply` keeps it unless `replace=true`. `lens=null` removes profiles.

Files that carry their camera's own correction (DNG `WarpRectilinear` / `FixVignetteRadial`, Panasonic / Leica RW2
distortion) keep using it: a photo's own data wins over a profile (`Photo::lens_correction`).

## Matching

- **Camera:** Exif maker + model against the database's maker + model, normalised (case, punctuation), so
  `RICOH IMAGING COMPANY, LTD.` + `RICOH GR IV` finds `Ricoh Imaging Company, Ltd.` + `Ricoh GR IV`.
- **Lens:** every word of the database name (apart from the maker) must appear in the photo's lens name; the
  candidate with the most words in common and the fewest extra wins. A letter prefix written against the focal
  length is split (`EF-S15-85mm` → `EF-S`, `15-85mm`) and `MkII` / `Mark II` / `II` count as one word. The lens must
  fit the camera's mount (or a mount listed as compatible: EF lenses on EF-S bodies) and cover the focal length.
  When the same lens is calibrated on several sensor sizes, the calibration nearest the camera's crop factor wins.
- **Lens names in maker notes:** Canon EOS bodies from before Exif 2.3 (the 450D, 550D…) name the lens only in their
  maker note (tag `0x0095`). LightCraft reads it at import, and `lensProfiles.apply` reads it from the files of photos
  imported before (the first 96 KiB of each, in parallel).
- **Built-in lenses:** a compact's mount is its own (`ricohGRIV`); the one calibrated lens on it is matched without
  a lens name.

Photos that get no profile are listed in the `lensProfiles.apply` result (`unmatched`: camera, lens, count).

## Models and units

Implemented from lensfun's published formulas (its documentation and the PanoTools lens model description); no
lensfun code is used. Radii are measured from the image centre.

| Correction | Models | Unit (`r = 1`) |
| --- | --- | --- |
| Distortion | `ptlens` `r_d = r·(a·r³ + b·r² + c·r + 1 − a − b − c)`, `poly3` `r_d = r·(1 − k1 + k1·r²)`, `poly5` `r_d = r·(1 + k1·r² + k2·r⁴)` | half the shorter side of the calibration frame |
| Lateral CA | `linear` `r_d = r·k`, `poly3` `r_d = r·(b·r² + c·r + v)`, red and blue, applied to the distorted radius | as distortion |
| Vignetting | `pa` `C_d = C_s / (1 + k1·r² + k2·r⁴ + k3·r⁶)` | half the diagonal of the calibration frame |

- As in lensfun, the PTLens/poly3 centre scale `d = 1 − a − b − c` is divided out (the centre keeps its size).
- A photo's corner lies at `r = c_cal·√(1 + aspect²)/c` (distortion, TCA) and `r = c_cal/c` (vignetting), with `c_cal`
  and `aspect` the calibration camera's crop factor and frame shape and `c` the photo's crop factor. When the profile
  was calibrated on the same kind of camera (crop factors within 10 %), a photo of another shape is taken as a crop
  of the sensor (4:3 from 3:2 keeps the height, 16:9 keeps the width) and its crop factor raised accordingly.
- Calibration is interpolated linearly between the nearest calibrated focal lengths (distortion, TCA) and between the
  nearest focal lengths, apertures (in stops) and focus distances (in dioptres) for vignetting; the nearest entry
  outside the range. Without a recorded aperture no vignetting correction is made; an unknown focus distance counts
  as far.
- The result is stored on the photo as an [`EmbeddedLens`](../crates/develop/src/settings.rs): each colour plane's
  radial curve fitted with `1, r, …, r⁶` (exact for `ptlens` and `poly3`/`poly5` distortion; the odd powers are
  `EmbeddedWarp::odd`, catalog format 4), the vignetting gain with `1 + Σ kᵢ·r²ⁱ`. A correction that would sample
  beyond the frame (pincushion) is scaled to fill it.
- Fisheye, equisolid, stereographic and panoramic lenses, and Adobe's `acm` model, are not corrected.

**Checked against a camera:** a Panasonic FZ1000 II RW2 (CC0 corpus, `rw2-panasonic-fz1000m2-4x3.rw2`) records the
distortion correction the camera applies to its own JPEGs. The lensfun profile LightCraft derives for the same shot
moves pixels along the same curve within 0.21 % of the half diagonal, after a 2.7 % overall scale (the camera crops
a little tighter): `crates/engine/tests/lens_profile_vs_camera.rs` (needs `LIGHTCRAFT_LENSFUN_DB` and the corpus file).
`crates/lensfun/tests/real_database.rs` checks that the whole database parses and that common Canon bodies and
lenses match.

## The database

- **Licence:** CC BY-SA 3.0, © the lensfun contributors. It is never part of LightCraft's repository or packages:
  the user downloads it, and the app credits it (Settings → Lens Profiles, `lensProfiles.status` → `attribution`).
- **Version:** one pinned lensfun commit (`crates/engine/src/lens_profiles.rs` → `LENSFUN_COMMIT`), 56 XML files with
  their sizes and SHA-256 (`lens_profile_files.rs`), fetched from `raw.githubusercontent.com` with
  `lightcraft-fetch` (resumable, every file checked). An `installed.json` marker is written only when all have
  arrived; nothing half-downloaded is ever loaded.
- **Where:** `$LIGHTCRAFT_LENS_PROFILES`, else `<config>/lens-profiles` (Windows `%APPDATA%\LightCraft\lens-profiles`).
- **Updating:** download the newer commit's `data/db/*.xml`, regenerate the file list, change `LENSFUN_COMMIT`.

## Limits

- Lens names are matched by words, not by lensfun's own matcher: an unusual spelling may match nothing (choose the
  profile by hand) and two lenses whose names differ only in a word the photo lacks can't be told apart.
- Many in-camera JPEGs already have vignetting (and some distortion) corrected by the camera; lower the profile
  amounts for those.
- No lens-profile picker in the Optics panel yet (the commands exist).
- Once the database is downloaded, imports match new photos as they arrive; photos already in the library need
  Apply to Library once.
