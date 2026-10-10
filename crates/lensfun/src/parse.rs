//! The lensfun database XML format (`<lensdatabase>` with `<mount>`, `<camera>` and `<lens>` elements).
//!
//! Lenient: an element this reader doesn't know is skipped, a calibration entry with an unknown model or a malformed
//! number is counted in [`Database::skipped`], and only XML that can't be read at all is an error. Sizes are bounded
//! so a hostile file can't make the reader allocate without limit.

use quick_xml::events::{BytesStart, Event};

use crate::{Camera, Database, Distortion, DistortionModel, Lens, LensKind, Mount, Tca, TcaModel, Vignetting};

/// Largest database file accepted (the biggest lensfun file is under 1 MB).
pub const MAX_FILE_BYTES: usize = 16 << 20;
/// Most elements read from one file.
const MAX_ELEMENTS: usize = 2_000_000;
/// Deepest nesting accepted.
const MAX_DEPTH: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    TooLarge,
    TooComplex,
    Xml(String),
    NotADatabase,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::TooLarge => write!(f, "the lens database file is larger than {} MB", MAX_FILE_BYTES >> 20),
            ParseError::TooComplex => write!(f, "the lens database file is nested too deeply or has too many elements"),
            ParseError::Xml(e) => write!(f, "the lens database file is not valid XML: {e}"),
            ParseError::NotADatabase => write!(f, "not a lensfun database file (no <lensdatabase> element)"),
        }
    }
}

impl std::error::Error for ParseError {}

/// What is being read: the element's text (without `lang`), or a nested entry.
#[derive(Default)]
struct Pending {
    maker: Option<String>,
    model: Option<String>,
    mounts: Vec<String>,
    name: Option<String>,
    compat: Vec<String>,
    crop: Option<f64>,
    aspect: Option<f64>,
    kind: Option<String>,
    focal: Option<(f64, f64)>,
    distortion: Vec<Distortion>,
    tca: Vec<Tca>,
    vignetting: Vec<Vignetting>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Entry {
    None,
    Camera,
    Lens,
    Mount,
}

fn attr(e: &BytesStart<'_>, key: &str) -> Option<String> {
    e.attributes().with_checks(false).flatten().find(|a| a.key.as_ref() == key.as_bytes()).map(|a| unescape(&String::from_utf8_lossy(&a.value)))
}

fn num(e: &BytesStart<'_>, key: &str) -> Option<f64> {
    attr(e, key).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite())
}

/// `&amp;`-style references in attribute values (the predefined five and numeric ones).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(rest.get(..i).unwrap_or(""));
        rest = rest.get(i..).unwrap_or("");
        match rest.find(';').and_then(|j| rest.get(1..j).and_then(entity).map(|c| (c, j))) {
            Some((c, j)) => {
                out.push(c);
                rest = rest.get(j + 1..).unwrap_or("");
            }
            None => {
                out.push('&');
                rest = rest.get(1..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let n = name.strip_prefix('#')?;
            let code = match n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => n.parse::<u32>().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// `"4:3"`, `"1.5"` → long/short ratio (≥ 1).
fn aspect(s: &str) -> Option<f64> {
    let v = match s.split_once(':') {
        Some((a, b)) => a.trim().parse::<f64>().ok()? / b.trim().parse::<f64>().ok()?,
        None => s.trim().parse::<f64>().ok()?,
    };
    (v.is_finite() && v > 0.0).then(|| if v < 1.0 { 1.0 / v } else { v })
}

/// Focal range from a model name: "18-55mm", "50mm".
pub(crate) fn focal_from_name(model: &str) -> Option<(f64, f64)> {
    let lower = model.to_ascii_lowercase();
    let mm = lower.find("mm")?;
    let head = lower.get(..mm)?;
    let start = head.rfind(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).map_or(0, |i| i + 1);
    let span = head.get(start..)?;
    let mut parts = span.split('-').filter(|p| !p.is_empty()).filter_map(|p| p.parse::<f64>().ok());
    let lo = parts.next()?;
    let hi = parts.next().unwrap_or(lo);
    (lo > 0.0 && hi >= lo && hi < 10_000.0).then_some((lo, hi))
}

fn distortion(e: &BytesStart<'_>) -> Option<Distortion> {
    let focal = num(e, "focal")?;
    let model = match attr(e, "model")?.as_str() {
        "poly3" => DistortionModel::Poly3 { k1: num(e, "k1").unwrap_or(0.0) },
        "poly5" => DistortionModel::Poly5 { k1: num(e, "k1").unwrap_or(0.0), k2: num(e, "k2").unwrap_or(0.0) },
        "ptlens" => DistortionModel::PtLens { a: num(e, "a").unwrap_or(0.0), b: num(e, "b").unwrap_or(0.0), c: num(e, "c").unwrap_or(0.0) },
        _ => return None,
    };
    (focal > 0.0).then_some(Distortion { focal, model })
}

fn tca(e: &BytesStart<'_>) -> Option<Tca> {
    let focal = num(e, "focal")?;
    let model = match attr(e, "model")?.as_str() {
        "linear" => TcaModel::Linear { kr: num(e, "kr").unwrap_or(1.0), kb: num(e, "kb").unwrap_or(1.0) },
        "poly3" => TcaModel::Poly3 {
            vr: num(e, "vr").unwrap_or(1.0),
            cr: num(e, "cr").unwrap_or(0.0),
            br: num(e, "br").unwrap_or(0.0),
            vb: num(e, "vb").unwrap_or(1.0),
            cb: num(e, "cb").unwrap_or(0.0),
            bb: num(e, "bb").unwrap_or(0.0),
        },
        _ => return None,
    };
    (focal > 0.0).then_some(Tca { focal, model })
}

fn vignetting(e: &BytesStart<'_>) -> Option<Vignetting> {
    if attr(e, "model")?.as_str() != "pa" {
        return None;
    }
    let focal = num(e, "focal")?;
    let aperture = num(e, "aperture")?;
    let distance = num(e, "distance").unwrap_or(1000.0);
    let k = [num(e, "k1").unwrap_or(0.0), num(e, "k2").unwrap_or(0.0), num(e, "k3").unwrap_or(0.0)];
    (focal > 0.0 && aperture > 0.0 && distance > 0.0).then_some(Vignetting { focal, aperture, distance, k })
}

fn finish(db: &mut Database, kind: Entry, p: Pending) {
    match kind {
        Entry::Camera => {
            if let (Some(maker), Some(model), Some(mount)) = (p.maker, p.model, p.mounts.into_iter().next()) {
                db.cameras.push(Camera { maker, model, mount, crop: p.crop.filter(|c| *c > 0.0).unwrap_or(1.0) });
            }
        }
        Entry::Lens => {
            if let Some(model) = p.model {
                let focal = p.focal.or_else(|| focal_from_name(&model));
                let kind = match p.kind.as_deref().map(str::trim) {
                    None | Some("rectilinear") => LensKind::Rectilinear,
                    Some(other) => LensKind::Other(other.to_string()),
                };
                db.lenses.push(Lens {
                    maker: p.maker.unwrap_or_default(),
                    model,
                    mounts: p.mounts,
                    crop: p.crop.filter(|c| *c > 0.0).unwrap_or(1.0),
                    aspect: p.aspect.unwrap_or(1.5),
                    kind,
                    focal,
                    distortion: p.distortion,
                    tca: p.tca,
                    vignetting: p.vignetting,
                });
            }
        }
        Entry::Mount => {
            if let Some(name) = p.name {
                db.mounts.push(Mount { name, compat: p.compat });
            }
        }
        Entry::None => {}
    }
}

pub(crate) fn add(db: &mut Database, xml: &str) -> Result<(), ParseError> {
    if xml.len() > MAX_FILE_BYTES {
        return Err(ParseError::TooLarge);
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut path: Vec<String> = Vec::new();
    let mut entry = Entry::None;
    let mut pending = Pending::default();
    // the text element being read and whether it carries `lang` (translations are display-only)
    let mut text: Option<(String, String, bool)> = None;
    let mut seen_root = false;
    let mut elements = 0usize;
    loop {
        let ev = reader.read_event().map_err(|e| ParseError::Xml(e.to_string()))?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                elements += 1;
                if elements > MAX_ELEMENTS || path.len() > MAX_DEPTH {
                    return Err(ParseError::TooComplex);
                }
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let empty = matches!(ev, Event::Empty(_));
                match (name.as_str(), path.len()) {
                    ("lensdatabase", 0) => seen_root = true,
                    ("camera", 1) => (entry, pending) = (Entry::Camera, Pending::default()),
                    ("lens", 1) => (entry, pending) = (Entry::Lens, Pending::default()),
                    ("mount", 1) => (entry, pending) = (Entry::Mount, Pending::default()),
                    ("distortion", 3) if entry == Entry::Lens => match distortion(e) {
                        Some(d) => pending.distortion.push(d),
                        None => db.skipped += 1,
                    },
                    ("tca", 3) if entry == Entry::Lens => match tca(e) {
                        Some(t) => pending.tca.push(t),
                        None => db.skipped += 1,
                    },
                    ("vignetting", 3) if entry == Entry::Lens => match vignetting(e) {
                        Some(v) => pending.vignetting.push(v),
                        None => db.skipped += 1,
                    },
                    ("focal", 2) if entry == Entry::Lens => {
                        let value = num(e, "value");
                        let (lo, hi) = (num(e, "min").or(value), num(e, "max").or(value));
                        if let (Some(lo), Some(hi)) = (lo, hi)
                            && lo > 0.0
                            && hi >= lo
                        {
                            pending.focal = Some((lo, hi));
                        }
                    }
                    (_, 2) if entry != Entry::None && !empty => {
                        text = Some((name.clone(), String::new(), attr(e, "lang").is_some()));
                    }
                    _ => {}
                }
                if !empty {
                    path.push(name);
                }
            }
            Event::Text(t) => {
                if let Some((_, buf, _)) = text.as_mut() {
                    buf.push_str(&t.decode().map_err(|e| ParseError::Xml(e.to_string()))?);
                }
            }
            Event::GeneralRef(r) => {
                if let Some((_, buf, _)) = text.as_mut() {
                    match entity(&String::from_utf8_lossy(&r)) {
                        Some(c) => buf.push(c),
                        None => buf.push('&'),
                    }
                }
            }
            Event::End(_) => {
                let closed = path.pop();
                if path.len() == 2 {
                    if let Some((name, value, lang)) = text.take() {
                        let value = value.trim().to_string();
                        match (name.as_str(), lang) {
                            (_, true) => {}
                            ("maker", _) => pending.maker = Some(value),
                            ("model", _) => pending.model = Some(value),
                            ("mount", _) => pending.mounts.push(value),
                            ("name", _) => pending.name = Some(value),
                            ("compat", _) => pending.compat.push(value),
                            ("cropfactor", _) => pending.crop = value.parse::<f64>().ok().filter(|v| v.is_finite()),
                            ("aspect-ratio", _) => pending.aspect = aspect(&value),
                            ("type", _) => pending.kind = Some(value),
                            _ => {}
                        }
                    }
                } else if path.len() == 1 && matches!(closed.as_deref(), Some("camera" | "lens" | "mount")) {
                    finish(db, entry, std::mem::take(&mut pending));
                    entry = Entry::None;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if seen_root { Ok(()) } else { Err(ParseError::NotADatabase) }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A synthetic database in lensfun's format (made-up bodies and numbers, not lensfun data).
    pub(crate) const SAMPLE: &str = r#"<!DOCTYPE lensdatabase SYSTEM "lensfun-database.dtd">
<lensdatabase version="2">
    <mount>
        <name>Test DX</name>
        <compat>Test FX</compat>
    </mount>
    <camera>
        <maker>Testmaker</maker>
        <maker lang="de">Testhersteller</maker>
        <model>Testmaker Body 1</model>
        <model lang="en">Body 1</model>
        <mount>Test DX</mount>
        <cropfactor>1.6</cropfactor>
    </camera>
    <camera>
        <maker>Testmaker</maker>
        <model>Testmaker Pocket</model>
        <mount>testPocket</mount>
        <cropfactor>2.5</cropfactor>
    </camera>
    <lens>
        <maker>Testmaker</maker>
        <model>Testmaker DX 15-85mm f/3.5-5.6 IS</model>
        <mount>Test DX</mount>
        <cropfactor>1.6</cropfactor>
        <calibration>
            <distortion model="ptlens" focal="15" a="0.02" b="-0.03" c="-0.04"/>
            <distortion model="ptlens" focal="85" a="0.001" b="-0.001" c="-0.006"/>
            <distortion model="acm" focal="85" k1="0.1"/>
            <tca model="poly3" focal="15" vr="1.0004" vb="1.0002"/>
            <tca model="linear" focal="85" kr="0.9998" kb="0.9996"/>
            <vignetting model="pa" focal="15" aperture="3.5" distance="10" k1="-0.3" k2="0.1" k3="-0.05"/>
            <vignetting model="pa" focal="15" aperture="8" distance="10" k1="-0.1" k2="0.0" k3="0.0"/>
        </calibration>
    </lens>
    <lens>
        <maker>Testmaker</maker>
        <model>Testmaker FX 50mm f/1.8 II</model>
        <mount>Test FX</mount>
        <aspect-ratio>4:3</aspect-ratio>
        <calibration>
            <distortion model="poly3" focal="50" k1="-0.01"/>
        </calibration>
    </lens>
    <lens>
        <maker>Testmaker</maker>
        <model>Testmaker Pocket &amp; compatibles</model>
        <mount>testPocket</mount>
        <cropfactor>2.5</cropfactor>
        <focal min="5" max="20"/>
        <calibration>
            <distortion model="poly5" focal="5" k1="-0.05" k2="0.01"/>
        </calibration>
    </lens>
    <lens>
        <maker>Testmaker</maker>
        <model>Testmaker Fish 8mm</model>
        <mount>Test DX</mount>
        <type>fisheye</type>
        <calibration>
            <distortion model="poly3" focal="8" k1="-0.2"/>
        </calibration>
    </lens>
</lensdatabase>
"#;

    pub(crate) fn sample() -> Database {
        let mut db = Database::default();
        db.add_xml(SAMPLE).unwrap();
        db
    }

    #[test]
    fn reads_cameras_lenses_mounts_and_calibration() {
        let db = sample();
        assert_eq!(db.cameras.len(), 2);
        assert_eq!(db.cameras[0], Camera { maker: "Testmaker".into(), model: "Testmaker Body 1".into(), mount: "Test DX".into(), crop: 1.6 });
        assert_eq!(db.lenses.len(), 4);
        let l = &db.lenses[0];
        assert_eq!(l.focal, Some((15.0, 85.0)), "focal range from the name");
        assert_eq!(l.distortion.len(), 2, "acm is skipped");
        assert_eq!(db.skipped, 1);
        assert_eq!(l.distortion[0].model, DistortionModel::PtLens { a: 0.02, b: -0.03, c: -0.04 });
        assert_eq!(l.tca[0].model, TcaModel::Poly3 { vr: 1.0004, cr: 0.0, br: 0.0, vb: 1.0002, cb: 0.0, bb: 0.0 });
        assert_eq!(l.vignetting[1], Vignetting { focal: 15.0, aperture: 8.0, distance: 10.0, k: [-0.1, 0.0, 0.0] });
        assert_eq!(l.aspect, 1.5, "default 3:2");
        assert_eq!(db.lenses[1].aspect, 4.0 / 3.0);
        assert_eq!(db.lenses[1].crop, 1.0, "no cropfactor: full frame");
        assert_eq!(db.lenses[2].model, "Testmaker Pocket & compatibles", "entities in text");
        assert_eq!(db.lenses[2].focal, Some((5.0, 20.0)), "<focal min max> wins over the name");
        assert_eq!(db.lenses[3].kind, LensKind::Other("fisheye".into()));
        assert!(!db.lenses[3].has_calibration(), "fisheye calibration is not applied");
        assert!(db.mount_fits("Test DX", "Test FX") && db.mount_fits("test dx", "Test DX") && !db.mount_fits("Test FX", "Test DX"));
    }

    #[test]
    fn hostile_or_foreign_input_is_an_error_not_a_panic() {
        let mut db = Database::default();
        assert_eq!(db.add_xml("<notlensfun/>"), Err(ParseError::NotADatabase));
        assert!(matches!(db.add_xml("<lensdatabase><lens><model>x</lens"), Ok(()) | Err(ParseError::Xml(_))));
        let deep = format!("<lensdatabase>{}</lensdatabase>", "<a>".repeat(100));
        assert_eq!(db.add_xml(&deep), Err(ParseError::TooComplex));
        // malformed numbers and missing attributes are skipped, never parsed as zero focal lengths
        let odd = r#"<lensdatabase><lens><model>L</model><mount>M</mount><calibration>
            <distortion model="ptlens" focal="x" a="1"/><vignetting model="pa" focal="10"/>
            <tca model="poly3"/></calibration></lens></lensdatabase>"#;
        let mut db = Database::default();
        db.add_xml(odd).unwrap();
        assert_eq!((db.lenses[0].distortion.len(), db.lenses[0].vignetting.len(), db.lenses[0].tca.len(), db.skipped), (0, 0, 0, 3));
    }

    #[test]
    fn focal_ranges_from_names() {
        assert_eq!(focal_from_name("Canon EF-S 18-55mm f/3.5-5.6 IS"), Some((18.0, 55.0)));
        assert_eq!(focal_from_name("EF50mm f/1.8 II"), Some((50.0, 50.0)));
        assert_eq!(focal_from_name("smc PENTAX-DA 18-55mm"), Some((18.0, 55.0)));
        assert_eq!(focal_from_name("Lens without focal"), None);
    }
}
