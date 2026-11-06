//! The run-theme charm: in a run with a theme, the bottom-left of the board (below the instrument
//! column, hanging from a chain) holds a round frame with the theme's icon; a run without a theme has
//! nothing there. This looks for the round frame on the laid-out reference screen (1918 x 1078, see
//! [`super::layout`]), then tells which theme it is by the normalised cross-correlation (NCC) with
//! grayscale templates.
//!
//! The templates assets/s16-theme-<id>.png are cropped from 2560 x 1440, UI scale 1 board screenshots
//! (see the test `build_templates`): for each theme, 3 patrol screens from different days, laid out
//! the way the reader does and then averaged pixel by pixel. Theme 303 (Iron Law) has no screenshot and
//! so no template: if its charm shows up, it is only read as "a charm but not recognised".
//!
//! Measured only on 2560 x 1440, UI scale 1 screens. The charm is not in any of layout.rs's edge
//! blocks, so when the scale is not 1 it is searched for scaled about the screen centre; if the game
//! actually pins it to the left or bottom edge, it will not be found when the scale is not 1 and is
//! read as no charm (never as the wrong theme).
//!
//! With a theme the charm is often invisible too, so Absent does not mean "no theme": when choosing an
//! inmate to eat, the left sidebar is fully collapsed; on the treasure screen the box of the leftmost
//! card sometimes covers the charm; while the left sidebar slides in or out the charm moves
//! sideways, and moving out of [`SEARCH`] it is not found. Only a read Theme counts.

use super::layout::{grey, luma};
use super::{Rect, Rgba};
use std::sync::OnceLock;

/// The charm centre on the reference screen: about (167, 1085) on a 2560 x 1440 screenshot.
const CENTRE: (i32, i32) = (125, 812);

/// Half the template side: the template is a square centred on the charm centre, with side 2 * HALF + 1.
const HALF: i32 = 62;

/// Where the template is cropped on the reference screen.
const TEMPLATE_AT: Rect = Rect::new(CENTRE.0 - HALF, CENTRE.1 - HALF, 2 * HALF + 1, 2 * HALF + 1);

/// Radius of the disk holding the icon (reference px): the icon itself is about 37, the inner edge of the round frame
/// about 47.
const DISK_R: f32 = 46.0;

/// The ring holding the round frame: inner and outer radius (reference px).
const RING_R: (f32, f32) = (47.0, 60.0);

/// How many px the centre may be searched away from [`CENTRE`]: when the left sidebar slides in or out
/// (for example entering or leaving the choice of inmate to eat), the charm moves sideways with it;
/// on the chain-eat screen of 2026-11-01 it was seen shifted left by about 10 reference px, so the
/// horizontal search reaches farther.
const SEARCH: (i32, i32) = (14, 6);

/// The themes that have templates, ordered by id.
pub const THEMES: [u32; 3] = [301, 302, 304];

/// The lowest disk NCC needed to accept a theme: on the tested board screens the correct theme scores
/// 0.74 or more, and screens without a theme reach 0.39 at most.
pub const MIN_SCORE: f32 = 0.6;

/// The lead needed to accept a theme: the best theme must beat the second best by at least this much.
/// The icons of themes 302 (Opulence) and 304 (Cage Fight) share the same scroll and are the closest to each
/// other: on a 304 screen 302 can reach 0.61, and the smallest lead is 0.34.
pub const MIN_LEAD: f32 = 0.2;

/// A ring NCC at this value counts as a charm being present (reported as Unknown when the theme is not
/// recognised): 0.94 or more when the charm is clear, about 0.72 when a card edge slants across it;
/// screens without a theme reach 0.54 at most.
pub const MIN_RING: f32 = 0.75;

/// What was read of the charm.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ThemeIcon {
    /// No charm seen: a run without a theme, or the charm is covered (for example while choosing an
    /// inmate to eat, when the left sidebar is gone entirely).
    #[default]
    Absent,
    /// A round frame, but the icon resembles no theme that has a template (for example Iron Law, or partly covered).
    Unknown { ring: f32 },
    /// The recognised theme id, its disk NCC, and how far it leads the second best theme.
    Theme { id: u32, score: f32, lead: f32 },
}

impl ThemeIcon {
    /// The recognised theme id; None when there is no charm or it is not recognised.
    pub fn id(&self) -> Option<u32> {
        match *self {
            ThemeIcon::Theme { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// The scores of one screen: the ring NCC, and the disk NCC of each theme (in the order of [`THEMES`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IconScores {
    pub ring: f32,
    pub themes: [f32; 3],
}

impl IconScores {
    /// Decide from the scores: the best theme is that theme if it is high enough and leads by enough, otherwise check
    /// whether the ring is there.
    pub fn decide(&self) -> ThemeIcon {
        let mut order = [0, 1, 2];
        order.sort_by(|&a, &b| self.themes[b].total_cmp(&self.themes[a]));
        let (best, next) = (self.themes[order[0]], self.themes[order[1]]);
        if best >= MIN_SCORE && best - next >= MIN_LEAD {
            ThemeIcon::Theme { id: THEMES[order[0]], score: best, lead: best - next }
        } else if self.ring >= MIN_RING {
            ThemeIcon::Unknown { ring: self.ring }
        } else {
            ThemeIcon::Absent
        }
    }
}

fn templates() -> &'static [Vec<f32>; 3] {
    static T: OnceLock<[Vec<f32>; 3]> = OnceLock::new();
    T.get_or_init(|| {
        [
            grey(include_bytes!("../../assets/s16-theme-301.png"), TEMPLATE_AT),
            grey(include_bytes!("../../assets/s16-theme-302.png"), TEMPLATE_AT),
            grey(include_bytes!("../../assets/s16-theme-304.png"), TEMPLATE_AT),
        ]
    })
}

/// The ring template: the average of the three theme templates (the round frame is the same in every theme, only the
/// icon differs).
fn ring_template() -> &'static [f32] {
    static R: OnceLock<Vec<f32>> = OnceLock::new();
    R.get_or_init(|| {
        let t = templates();
        (0..t[0].len()).map(|i| t.iter().map(|v| v[i]).sum::<f32>() / 3.0).collect()
    })
}

/// Indices of the template pixels with radius in [r0, r1] ((dx, dy) relative to the centre, and the template index).
fn mask(r0: f32, r1: f32) -> Vec<(i32, i32, usize)> {
    let side = 2 * HALF + 1;
    (-HALF..=HALF)
        .flat_map(|y| (-HALF..=HALF).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let r = ((x * x + y * y) as f32).sqrt();
            r >= r0 && r <= r1
        })
        .map(|(x, y)| (x, y, ((y + HALF) * side + x + HALF) as usize))
        .collect()
}

fn disk_mask() -> &'static [(i32, i32, usize)] {
    static M: OnceLock<Vec<(i32, i32, usize)>> = OnceLock::new();
    M.get_or_init(|| mask(0.0, DISK_R))
}

fn ring_mask() -> &'static [(i32, i32, usize)] {
    static M: OnceLock<Vec<(i32, i32, usize)>> = OnceLock::new();
    M.get_or_init(|| mask(RING_R.0, RING_R.1))
}

/// Grayscale near the charm: a square centred on [`CENTRE`], extending HALF + SEARCH outward.
struct Patch {
    w: i32,
    grey: Vec<f32>,
}

impl Patch {
    fn of(img: &Rgba) -> Patch {
        let (hx, hy) = (HALF + SEARCH.0, HALF + SEARCH.1);
        let w = 2 * hx + 1;
        let mut grey = Vec::with_capacity((w * (2 * hy + 1)) as usize);
        for y in CENTRE.1 - hy..=CENTRE.1 + hy {
            for x in CENTRE.0 - hx..=CENTRE.0 + hx {
                let [r, g, b] = img.rgb(x, y);
                grey.push(luma(r, g, b));
            }
        }
        Patch { w, grey }
    }

    /// The NCC between template `t`, within mask `m`, and this patch when the centre is shifted by (dx, dy).
    fn ncc(&self, t: &[f32], m: &[(i32, i32, usize)], dx: i32, dy: i32) -> f32 {
        let (cx, cy) = (HALF + SEARCH.0, HALF + SEARCH.1);
        let (mut n, mut sp, mut spp, mut st, mut stt, mut spt) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for &(x, y, i) in m {
            let p = self.grey[((cy + y + dy) * self.w + cx + x + dx) as usize] as f64;
            let v = t[i] as f64;
            n += 1.0;
            sp += p;
            spp += p * p;
            st += v;
            stt += v * v;
            spt += p * v;
        }
        let (vp, vt) = (spp - sp * sp / n, stt - st * st / n);
        if vp > 1e-6 && vt > 1e-6 { ((spt - sp * st / n) / (vp * vt).sqrt()) as f32 } else { 0.0 }
    }

    /// The template's highest NCC within the [`SEARCH`] shifts: first every 2 px, then a fine search +-1 px around
    /// the best spot.
    fn best(&self, t: &[f32], m: &[(i32, i32, usize)]) -> f32 {
        let (mut best, mut at) = (f32::MIN, (0, 0));
        for dy in (-SEARCH.1..=SEARCH.1).step_by(2) {
            for dx in (-SEARCH.0..=SEARCH.0).step_by(2) {
                let s = self.ncc(t, m, dx, dy);
                if s > best {
                    (best, at) = (s, (dx, dy));
                }
            }
        }
        for dy in (at.1 - 1).max(-SEARCH.1)..=(at.1 + 1).min(SEARCH.1) {
            for dx in (at.0 - 1).max(-SEARCH.0)..=(at.0 + 1).min(SEARCH.0) {
                best = best.max(self.ncc(t, m, dx, dy));
            }
        }
        best
    }
}

/// The scores of the charm on `img` (the board-layout reference screen).
pub fn icon_scores(img: &Rgba) -> IconScores {
    let patch = Patch::of(img);
    let t = templates();
    IconScores { ring: patch.best(ring_template(), ring_mask()), themes: std::array::from_fn(|k| patch.best(&t[k], disk_mask())) }
}

/// The theme charm on `img` (the board-layout reference screen).
pub fn read_theme_icon(img: &Rgba) -> ThemeIcon {
    icon_scores(img).decide()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::load_png;
    use std::path::{Path, PathBuf};

    fn dir() -> Option<PathBuf> {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("trainingdata").join("theme");
        if !d.is_dir() {
            println!("skipped: {} is missing", d.display());
            return None;
        }
        Some(d)
    }

    /// The screens used to build the templates: 3 patrol screens from different days per theme. These screens are not
    /// used in the tests below.
    const SOURCES: [(u32, [&str; 3]); 3] = [
        (301, ["2026-11-01_18-34-33_frame-0007", "2026-11-01_18-34-33_frame-0014", "2026-11-01_18-34-33_frame-0025"]),
        (302, ["2026-11-02_12-23-27_frame-0008", "2026-11-02_12-23-27_frame-0022", "2026-11-02_12-23-27_frame-0027"]),
        (304, ["2026-11-01_19-18-03_frame-0007", "2026-11-01_19-18-03_frame-0021", "2026-11-01_19-18-03_frame-0036"]),
    ];

    /// Rebuild assets/s16-theme-<id>.png: `cargo test --profile fast-test --offline build_templates -- --ignored`.
    #[test]
    #[ignore]
    fn build_templates() {
        let Some(d) = dir() else { return };
        let r = TEMPLATE_AT;
        for (id, names) in SOURCES {
            let mut sum = vec![0.0f32; (r.w * r.h) as usize];
            for name in names {
                let img = load_png(&d.join(format!("{name}.png"))).unwrap();
                for y in 0..r.h {
                    for x in 0..r.w {
                        let [cr, cg, cb] = img.rgb(r.x + x, r.y + y);
                        sum[(y * r.w + x) as usize] += luma(cr, cg, cb) / names.len() as f32;
                    }
                }
            }
            let out = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("assets/s16-theme-{id}.png"));
            let file = std::fs::File::create(&out).unwrap();
            let mut enc = png::Encoder::new(std::io::BufWriter::new(file), r.w as u32, r.h as u32);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            let bytes: Vec<u8> = sum.iter().map(|v| v.round().clamp(0.0, 255.0) as u8).collect();
            enc.write_header().unwrap().write_image_data(&bytes).unwrap();
            println!("wrote {}", out.display());
        }
    }

    /// Test screens: (run folder_frame, theme). The theme comes from the "theme" field of that run's
    /// log.jsonl (frame-0001 of 2026-11-02_20-24-01 is still null in the log: Ascendant had not yet
    /// read the theme scroll then, but the charm was already there). The comment gives the screen kind and the day.
    const BOARDS: &[(&str, Option<u32>)] = &[
        // Theme 301 (Tyranny): 3 runs.
        ("2026-11-01_18-34-33_frame-0039", Some(301)), // patrol d5
        ("2026-11-01_18-34-33_frame-0067", Some(301)), // pick instrument d7 (dimmed)
        ("2026-11-01_18-34-33_frame-0084", Some(301)), // manage d9
        ("2026-11-01_18-34-33_frame-0096", Some(301)), // treasure d10
        ("2026-11-01_18-34-33_frame-0198", Some(301)), // interrogation d19
        ("2026-11-01_18-34-33_frame-0212", Some(301)), // treasure d20 (card edge slants across the charm)
        ("2026-11-02_20-24-01_frame-0001", Some(301)), // pick instrument d1 (log theme is null)
        ("2026-11-02_20-24-01_frame-0018", Some(301)), // patrol d2
        ("2026-11-02_20-24-01_frame-0048", Some(301)), // manage d5
        ("2026-11-02_20-24-01_frame-0127", Some(301)), // pick instrument d12
        ("2026-11-02_20-24-01_frame-0042", Some(301)), // interrogation d4
        ("2026-11-02_21-25-51_frame-0045", Some(301)), // pick cell d5
        ("2026-11-02_21-25-51_frame-0157", Some(301)), // patrol d17
        ("2026-11-02_21-25-51_frame-0102", Some(301)), // manage d11
        ("2026-11-02_21-25-51_frame-0206", Some(301)), // interrogation d20
        // Theme 302 (Opulence): 2 runs.
        ("2026-11-02_12-23-27_frame-0043", Some(302)), // manage d5
        ("2026-11-02_12-23-27_frame-0053", Some(302)), // pick instrument d5
        ("2026-11-02_12-23-27_frame-0070", Some(302)), // treasure d6
        ("2026-11-02_12-23-27_frame-0091", Some(302)), // interrogation d8
        ("2026-11-02_12-23-27_frame-0210", Some(302)), // patrol d12
        ("2026-11-02_21-56-48_frame-0012", Some(302)), // patrol d2
        ("2026-11-02_21-56-48_frame-0028", Some(302)), // pick instrument d4
        ("2026-11-02_21-56-48_frame-0083", Some(302)), // treasure d9
        ("2026-11-02_21-56-48_frame-0219", Some(302)), // manage d14
        ("2026-11-02_21-56-48_frame-0248", Some(302)), // pick cell d17
        // Theme 304 (Cage Fight): only 1 run has frames.
        ("2026-11-01_19-18-03_frame-0010", Some(304)), // pick instrument d1
        ("2026-11-01_19-18-03_frame-0052", Some(304)), // pick cell d4
        ("2026-11-01_19-18-03_frame-0068", Some(304)), // patrol d5
        ("2026-11-01_19-18-03_frame-0091", Some(304)), // treasure d6
        ("2026-11-01_19-18-03_frame-0108", Some(304)), // treasure d7
        ("2026-11-01_19-18-03_frame-0113", Some(304)), // manage d8
        ("2026-11-01_19-18-03_frame-0175", Some(304)), // patrol d12
        ("2026-11-01_19-18-03_frame-0227", Some(304)), // pick cell d17
        ("2026-11-01_19-18-03_frame-0256", Some(304)), // manage d19
        // No theme.
        ("2026-11-02_22-50-04_frame-0002", None), // pick instrument d1
        ("2026-11-02_22-50-04_frame-0009", None), // patrol d1
        ("2026-11-02_22-50-04_frame-0025", None), // treasure d3
        ("2026-11-02_22-50-04_frame-0031", None), // chain target d3
        ("2026-11-02_22-50-04_frame-0047", None), // manage d6
        ("2026-11-02_22-50-04_frame-0129", None), // treasure d16
        ("2026-11-02_22-50-04_frame-0175", None), // patrol d19
        ("2026-11-02_17-54-45_frame-0004", None), // pick instrument d1
        ("2026-11-02_17-54-45_frame-0021", None), // chain target d2
        ("2026-11-02_17-54-45_frame-0056", None), // manage d5
        ("2026-11-02_17-54-45_frame-0076", None), // treasure d7
        ("2026-11-02_17-54-45_frame-0089", None), // chain target d9
        ("2026-11-02_17-54-45_frame-0131", None), // interrogation d14
        ("2026-11-02_17-54-45_frame-0184", None), // patrol d19
        // No theme, and among the frames with the highest theme score or ring score in a coarse sweep of the whole
        // screenshot set.
        ("2026-11-01_17-59-57_frame-0080", None), // treasure d9 (card box over the charm position)
        ("2026-11-01_17-59-57_frame-0196", None), // run end d20
        ("2026-11-01_19-23-53_frame-0165", None), // treasure d18 (card box over the charm position)
        ("2026-11-01_20-00-00_frame-0173", None), // manage d20
        ("2026-11-01_20-00-00_frame-0187", None), // run end d20
        ("2026-11-01_20-02-37_frame-0045", None), // pick instrument d3
        ("2026-11-02_11-42-01_frame-0272", None), // run end d20
    ];

    /// Screens with a theme but the charm covered or sliding: may be read as Absent, Unknown or the
    /// correct theme, but never as another theme.
    const COVERED: &[(&str, u32)] = &[
        ("2026-11-01_18-34-33_frame-0017", 301), // chain target: left sidebar fully collapsed
        ("2026-11-01_18-34-33_frame-0035", 301), // treasure: card edge slants across
        ("2026-11-01_18-34-33_frame-0082", 301), // treasure: a card covers the charm
        ("2026-11-02_20-24-01_frame-0154", 301), // treasure: the leftmost card box covers the charm
        ("2026-11-02_21-25-51_frame-0069", 301), // treasure: the leftmost card box covers the charm
        ("2026-11-01_19-18-03_frame-0016", 304), // chain target: left sidebar collapsed
        ("2026-11-01_19-18-03_frame-0044", 304), // chain target: left sidebar sliding
        ("2026-11-01_19-18-03_frame-0065", 304), // treasure: card edge slants across
        ("2026-11-01_19-18-03_frame-0129", 304), // left sidebar sliding
        // chain target: left sidebar sliding out, charm shifted left by about 19 reference px
        ("2026-11-02_12-23-27_frame-0015", 302),
    ];

    fn scores(d: &Path, name: &str) -> IconScores {
        icon_scores(&load_png(&d.join(format!("{name}.png"))).unwrap())
    }

    /// Every board screen must be recognised correctly; a screen without a theme must read as Absent
    /// (not Unknown). Also prints the error margins: each theme's own score range, the smallest lead
    /// over the second best, and the highest theme score and ring score on screens without a theme.
    #[test]
    fn theme_icons_read() {
        let Some(d) = dir() else { return };
        let mut bad = Vec::new();
        // Per theme: (frame count, lowest score, highest score, least lead).
        let mut per = [(0usize, f32::MAX, f32::MIN, f32::MAX); 3];
        let (mut none_n, mut none_theme, mut none_ring) = (0usize, f32::MIN, f32::MIN);
        for &(name, want) in BOARDS {
            let s = scores(&d, name);
            let got = s.decide();
            match want {
                Some(id) => {
                    let k = THEMES.iter().position(|&t| t == id).unwrap();
                    let own = s.themes[k];
                    let other = (0..3).filter(|&j| j != k).map(|j| s.themes[j]).fold(f32::MIN, f32::max);
                    let p = &mut per[k];
                    *p = (p.0 + 1, p.1.min(own), p.2.max(own), p.3.min(own - other));
                    if got.id() != Some(id) {
                        bad.push(format!("{name}: want {id}, got {got:?} ({s:?})"));
                    }
                }
                None => {
                    none_n += 1;
                    none_theme = none_theme.max(s.themes.iter().copied().fold(f32::MIN, f32::max));
                    none_ring = none_ring.max(s.ring);
                    if got != ThemeIcon::Absent {
                        bad.push(format!("{name}: want Absent, got {got:?} ({s:?})"));
                    }
                }
            }
        }
        for (k, p) in per.iter().enumerate() {
            println!("theme {}: {} frames, own score {:.3}..{:.3}, least lead {:.3}", THEMES[k], p.0, p.1, p.2, p.3);
        }
        println!("no theme: {none_n} frames, highest theme score {none_theme:.3}, highest ring {none_ring:.3}");
        for &(name, id) in COVERED {
            let s = scores(&d, name);
            let got = s.decide();
            println!("covered {name} ({id}): {got:?} ring {:.3} themes {:.3?}", s.ring, s.themes);
            if got.id().is_some_and(|g| g != id) {
                bad.push(format!("{name}: covered {id}, read as {got:?} ({s:?})"));
            }
        }
        for b in &bad {
            println!("miss: {b}");
        }
        assert!(per.iter().all(|p| p.0 >= 5) && none_n >= 15, "too few frames");
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// A flat grey screen has no charm.
    #[test]
    fn flat_frame_has_no_icon() {
        let img = Rgba { w: super::super::REF_W, h: super::super::REF_H, data: [40, 40, 40, 255].repeat(super::super::REF_W * super::super::REF_H) };
        assert_eq!(read_theme_icon(&img), ThemeIcon::Absent);
    }
}
