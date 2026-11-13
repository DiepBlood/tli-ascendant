//! Icons of the held instruments at the top right: arranged right to left (the rightmost is the last one obtained);
//! each cell is shrunk to a 20x13 RGB grid and correlated against the templates (the mean is subtracted first, so
//! only shape and coloring count). The templates cover only the six items seen in real frames (`腫大腦垂體`, `黑山羊腸縫線`,
//! `二寸顱骨釘`, `人蛹標本`, `簇生蟲卵`, `攣縮指爪`). The instruments revealed on the ranked game intro page (`蒸骨坩堝`, `水蛭罐`) have
//! templates of their own: they only change the initial board and have no effect after the start, so when
//! recognized they are marked "affects the opening only" and are not handed to the solver. For cells these
//! templates are unsure about, the 25 icon signatures of vorax-solver-rs (DinoRaos, MIT) serve as the fallback
//! (data/vorax-icons-vsr.json, see "VSR fallback" below); only cells that also fail its threshold are reported as
//! "unknown".

use super::names::{Category, names};
use super::read::Held;
use crate::reader::{Rect, Rgba};

/// Width and height of a cell (5x5 pixels per cell).
pub const IW: usize = 20;
pub const IH: usize = 13;
const CELL: i32 = 5;

/// Center x of the icons, right to left (with 1 item it is in the rightmost cell). At most 4 cells: 3 ordinary
/// instruments, plus the 1 revealed on the ranked game intro page (from round 5 of frames2 all 4 cells are full).
const SLOT_X: [i32; 4] = [2403, 2269, 2133, 1998];
const TOP: i32 = 30;

/// The correlation must be at least this high, and exceed the second most similar template by this much, to accept:
/// the same item at different positions gives 0.95-1.00; the most similar different items are `人蛹標本` and `簇生蟲卵`
/// (both purplish) at 0.88, and the cauldron is at most 0.70.
const MIN_CORR: f32 = 0.92;
const MIN_MARGIN: f32 = 0.05;

/// Threshold for the opening reveal instruments (`OPENING`): compared over all 1946 screenshot cells, after
/// loosening none was recognized as another item (of the 1069 cells that were previously unknown, the 9 with >=
/// 0.85 against the opening templates are all real opening items, and the next best is only 0.69). New opening
/// items that are not included have no data to verify against: if a new item is ever recognized as `蒸骨坩堝` or `水蛭罐`,
/// look at these two thresholds first.
const OPENING_MIN_CORR: f32 = 0.85;
const OPENING_MIN_MARGIN: f32 = 0.15;

/// Number of icon cells.
pub const SLOTS: usize = SLOT_X.len();

/// The k-th cell (right to left).
pub fn slot(k: usize) -> Rect {
    Rect::new(SLOT_X[k] - IW as i32 * CELL / 2, TOP, IW as i32 * CELL, IH as i32 * CELL)
}

/// A cell holds an icon when at least BRIGHT_MIN of its pixels are brighter than luminance 70, or at least DIM_MIN
/// brighter than DIM_LUM. The dark `攣縮指爪` has few bright pixels: in the third cell of a frame unscaled from
/// `PageScale` 0.672568 (s = 1.0083, see `ui_scale`) it kept 13-14 above 70, the cell read as empty, the icons were
/// counted from the right up to it and the claw was never read (2026-11-11_21-08-23 from round 6: [19, 10] for
/// [6, 19, 10]; 9 games of 2026-11-10/11, 92 board frames), nor an opening instrument left of it. Under a card's
/// tooltip, which dims the board, it kept 0 above 70 and 124 above 40 (that game's frame-0039), and at the original
/// scale a claw alone in the first cell 7 and 160 (2026-11-04_12-52-39 frame-0021). Over the 1639 saved frames read as
/// a board, the empty cell left of the icons read has at most 3 pixels above 40, and every icon read at least 149.
const BRIGHT_MIN: usize = 15;
const DIM_LUM: u32 = 40;
const DIM_MIN: usize = 60;

/// RGB grid features of one cell (row-major, R, G, B per cell); None when this cell has no icon (all dark).
pub fn feature(img: &Rgba, r: Rect) -> Option<Vec<f32>> {
    let lum = |p: [u8; 3]| (u32::from(p[0]) * 77 + u32::from(p[1]) * 150 + u32::from(p[2]) * 29) >> 8;
    let lums: Vec<u32> = (r.y..r.y + r.h).flat_map(|y| (r.x..r.x + r.w).map(move |x| (x, y))).map(|(x, y)| lum(img.rgb(x, y))).collect();
    let above = |t: u32| lums.iter().filter(|&&v| v > t).count();
    if above(70) < BRIGHT_MIN && above(DIM_LUM) < DIM_MIN {
        return None;
    }
    let mut f = vec![0f32; IW * IH * 3];
    for cy in 0..IH as i32 {
        for cx in 0..IW as i32 {
            let mut s = [0u32; 3];
            for y in 0..CELL {
                for x in 0..CELL {
                    let p = img.rgb(r.x + cx * CELL + x, r.y + cy * CELL + y);
                    (0..3).for_each(|c| s[c] += u32::from(p[c]));
                }
            }
            let at = ((cy as usize) * IW + cx as usize) * 3;
            (0..3).for_each(|c| f[at + c] = s[c] as f32 / (CELL * CELL) as f32);
        }
    }
    Some(f)
}

/// Features of each cell, left to right (order of acquisition).
pub fn features(img: &Rgba) -> Vec<Vec<f32>> {
    let mut out: Vec<Vec<f32>> = (0..SLOT_X.len()).map_while(|k| feature(img, slot(k))).collect();
    out.reverse();
    out
}

/// Inner product after subtracting the mean and normalizing the length to 1 (the correlation coefficient).
fn correlation(a: &[f32], b: &[f32]) -> f32 {
    let (ma, mb) = (a.iter().sum::<f32>() / a.len() as f32, b.iter().sum::<f32>() / b.len() as f32);
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (x - ma, y - mb);
        ab += x * y;
        aa += x * x;
        bb += y * y;
    }
    if aa <= 0.0 || bb <= 0.0 { 0.0 } else { ab / (aa.sqrt() * bb.sqrt()) }
}

/// Recognize the features of one cell with our own templates (TEMPLATES and OPENING); when unsure the result is
/// unknown (id and zh are both None).
fn match_ours(f: &[f32]) -> Held {
    let corr = |t: &[u8]| correlation(f, &t.iter().map(|&v| f32::from(v)).collect::<Vec<_>>());
    // Ordinary instruments (solver id) and the opening reveal ones (Traditional Chinese name) are compared
    // together, and the most similar wins.
    let mut cs: Vec<(Result<u8, &'static str>, f32)> = TEMPLATES.iter().map(|(id, t)| (Ok(*id), corr(t))).chain(OPENING.iter().map(|(zh, t)| (Err(*zh), corr(t)))).collect();
    cs.sort_by(|a, b| b.1.total_cmp(&a.1));
    // The second most similar is a different item (the same item may have two templates, for example `水蛭罐`).
    let best = cs[0];
    let second = cs.iter().skip(1).find(|c| c.0 != best.0).map_or(0.0, |c| c.1);
    match best.0 {
        // The opening reveal instruments are at most 0.69 against the other items: the threshold is loosened, and
        // the required lead is large. The `蒸骨坩堝` icon has a steam animation, so its correlation jumps between 0.90
        // and 0.98, and with the ordinary threshold it was never read for a whole game (2026-11-03 ranked game).
        Err(zh) if best.1 >= OPENING_MIN_CORR && best.1 - second >= OPENING_MIN_MARGIN => Held { id: None, en: None, zh: Some(zh.to_string()), opening: true, score: best.1 },
        _ if best.1 < MIN_CORR || best.1 - second < MIN_MARGIN => unknown(best.1),
        Ok(id) => instrument(id, best.1).unwrap_or_else(|| unknown(best.1)),
        Err(zh) => Held { id: None, en: None, zh: Some(zh.to_string()), opening: true, score: best.1 },
    }
}

fn unknown(score: f32) -> Held {
    Held { id: None, en: None, zh: None, opening: false, score }
}

/// Ordinary instrument by solver id (None if the name table has no such id).
fn instrument(id: u8, score: f32) -> Option<Held> {
    let n = names().cards.iter().find(|c| c.category == Category::Instrument && c.id == Some(id))?;
    Some(Held { id: Some(id), en: Some(n.en.clone()), zh: n.zh.clone(), opening: false, score })
}

/// Confident with our own templates (recognized as an ordinary instrument or an opening reveal instrument).
fn sure(h: &Held) -> bool {
    h.id.is_some() || h.opening
}

/// Number of icon cells (counted right to left up to the first empty cell).
fn slot_count(img: &Rgba) -> usize {
    (0..SLOTS).take_while(|&k| feature(img, slot(k)).is_some()).count()
}

/// The held instruments, left to right. Cells our own templates are unsure about are compared against the 25 VSR
/// signatures (fallback).
pub fn read_held(img: &Rgba) -> Vec<Held> {
    (0..slot_count(img))
        .rev()
        .map(|k| {
            let ours = feature(img, slot(k)).map_or_else(|| unknown(0.0), |f| match_ours(&f));
            decide(ours, || vsr_picks(img, k))
        })
        .collect()
}

/// If our own templates are confident, use that; otherwise see whether the VSR fallback is confident, and if not it
/// is our own (unknown) result.
fn decide(ours: Held, picks: impl FnOnce() -> Vec<VsrPick>) -> Held {
    if sure(&ours) { ours } else { vsr_held(&picks()).unwrap_or(ours) }
}

/// For evaluation: the result of our own templates for one cell, and the VSR fallback match details.
#[derive(Clone, Debug)]
pub struct IconEval {
    /// Our own templates only (the result before adding the VSR fallback).
    pub ours: Held,
    /// VSR signatures only (whether or not our own templates are confident); failing the threshold means unknown.
    pub vsr: Held,
    /// The result after adding the fallback (same logic as [`read_held`]).
    pub held: Held,
    /// Each box of the VSR match ([`VSR_NUDGE`] order, the first is VSR's own box).
    pub picks: Vec<VsrPick>,
}

/// For evaluation: each cell left to right, the results of our own templates and of the VSR signatures separately.
pub fn evaluate(img: &Rgba) -> Vec<IconEval> {
    (0..slot_count(img))
        .rev()
        .map(|k| {
            let picks = vsr_picks(img, k);
            let ours = feature(img, slot(k)).map_or_else(|| unknown(0.0), |f| match_ours(&f));
            let vsr = vsr_held(&picks).unwrap_or_else(|| unknown(picks[0].score as f32));
            let held = decide(ours.clone(), || picks.clone());
            IconEval { ours, vsr, held, picks }
        })
        .collect()
}

// ---- VSR fallback: the 25 instrument icon signatures of vorax-solver-rs (DinoRaos, MIT) ----
//
// Ported from crates/vorax-scan/src/instruments.rs of vorax-solver-rs (holds_an_icon, icon_similarity, ICON_MIN,
// ICON_GAP, STARTING_ITEM) and pixels.rs (crop, resize, gray, signature, js_round, clamp_u8, js_hypot); the numeric
// operations are copied exactly (including f32 grayscale and JavaScript rounding) so that they match their
// signatures. The data file data/vorax-icons-vsr.json is the instrumentIcons of their data/scan/data.json merged
// with learnedIcons.json in the original order (dropping "none", which does not take part in the ranking), with
// values multiplied by 1000 and stored as integers (the original has 3 decimals, so nothing is lost).

/// Side length of the signature (24x24 grayscale).
const VSR_N: usize = 24;
/// Range of the shift search (signature cells, +-2).
const VSR_REACH: i32 = 2;
/// The best score must be at least this high, and exceed the second best by this much, to accept (VSR's ICON_MIN,
/// ICON_GAP).
const VSR_MIN: f64 = 0.85;
const VSR_GAP: f64 = 0.08;
/// VSR's "opening instrument" category name.
const VSR_STARTING: &str = "(starting item)";
/// Judgment of whether the cell holds an icon (VSR's holds_an_icon): the fraction of bright enough pixels and the
/// fraction of near-black background.
const VSR_BRIGHT: u8 = 60;
const VSR_DARK: u8 = 25;
const VSR_FILLED_MIN: f64 = 0.008;
const VSR_BACKDROP_MIN: f64 = 0.3;
/// VSR's icon box (instrument_icon in layout.rs, measured at 3440x1440, scaled by height, aligned to the right
/// edge): center distance from the right edge 162 + 135k, center y 66, side 74. The reference frame is 2560x1440,
/// so the scale factor is exactly 1.
const VSR_RIGHT: i32 = 162;
const VSR_PITCH: i32 = 135;
const VSR_Y: i32 = 66;
const VSR_SIZE: i32 = 74;
/// Besides VSR's own box, boxes shifted 2 pixels left and right are also compared (the first is VSR's box). In the
/// user's 2560x1440 frames, the icon centers measure about 2399, 2266, 2133, 1998 (spacing about 134), while VSR's
/// box centers are 2398, 2263, 2128, 1993 (spacing 135), drifting more the further left; thin icons (`腐敗刮刀`,
/// `黑山羊腸縫線`) shrunk to 24x24 are very sensitive to these few pixels: `腐敗刮刀` in cell 2 scores only 0.83 (VSR's box)
/// and 0.95 when shifted 2 pixels right. Accept only when at least one of the three boxes passes the threshold and
/// all the boxes that pass recognize the same item.
const VSR_NUDGE: [i32; 3] = [0, -2, 2];

/// VSR icon box, k-th cell (right to left), reference coordinates.
pub fn vsr_slot(k: usize) -> Rect {
    let cx = super::read::W as i32 - (VSR_RIGHT + VSR_PITCH * k as i32);
    Rect::new(cx - VSR_SIZE / 2, VSR_Y - VSR_SIZE / 2, VSR_SIZE, VSR_SIZE)
}

/// Result of the VSR match for one cell.
#[derive(Clone, Debug)]
pub struct VsrPick {
    /// VSR's holds_an_icon: the box looks like it holds an icon (something bright on a near-black background).
    pub holds: bool,
    /// The most similar category (English name or "(starting item)") with its score, and the second most similar
    /// category with its score.
    pub best: String,
    pub score: f64,
    pub second_name: String,
    pub second: f64,
}

impl VsrPick {
    /// Passed VSR's threshold (the box holds an icon, and score and lead are both sufficient).
    pub fn sure(&self) -> bool {
        self.holds && self.score >= VSR_MIN && self.score - self.second >= VSR_GAP
    }
}

/// Turn a confident VSR result into Held: it counts only if all boxes that pass the threshold recognize the same
/// item (taking the highest score among them); an ordinary instrument is converted to a solver id through the name
/// table; an opening instrument is marked as affecting the opening only.
fn vsr_held(picks: &[VsrPick]) -> Option<Held> {
    let sure: Vec<&VsrPick> = picks.iter().filter(|p| p.sure()).collect();
    let p = *sure.iter().max_by(|a, b| a.score.total_cmp(&b.score))?;
    if sure.iter().any(|q| q.best != p.best) {
        return None;
    }
    let score = p.score as f32;
    if p.best == VSR_STARTING {
        return Some(Held { id: None, en: None, zh: Some("開局用具".to_string()), opening: true, score });
    }
    let id = names().cards.iter().find(|c| c.category == Category::Instrument && c.en == p.best)?.id?;
    instrument(id, score)
}

/// VSR signature data: category names and the samples of each (576 values).
fn vsr_icons() -> &'static [(String, Vec<Vec<f64>>)] {
    static ICONS: std::sync::OnceLock<Vec<(String, Vec<Vec<f64>>)>> = std::sync::OnceLock::new();
    ICONS.get_or_init(|| {
        #[derive(serde::Deserialize)]
        struct File {
            scale: f64,
            icons: Vec<(String, Vec<Vec<i32>>)>,
        }
        let f: File = serde_json::from_str(include_str!("../../data/vorax-icons-vsr.json")).expect("vorax-icons-vsr.json");
        f.icons.into_iter().map(|(n, vs)| (n, vs.into_iter().map(|v| v.into_iter().map(|x| f64::from(x) / f.scale).collect()).collect())).collect()
    })
}

/// Compare the k-th cell (right to left) against the signatures using the VSR box (and the boxes shifted 2 pixels
/// left and right).
pub fn vsr_picks(img: &Rgba, k: usize) -> Vec<VsrPick> {
    let r = vsr_slot(k);
    VSR_NUDGE.iter().map(|&dx| vsr_pick_at(img, Rect::new(r.x + dx, r.y, r.w, r.h))).collect()
}

/// Compare against the VSR signatures using box `r` (reference coordinates).
pub fn vsr_pick_at(img: &Rgba, r: Rect) -> VsrPick {
    let p = vsr_signature(img, r);
    let mut cache = Vec::new();
    let mut ranked: Vec<(&str, f64)> = vsr_icons().iter().map(|(n, vs)| (n.as_str(), vs.iter().map(|v| vsr_similarity(v, &p, &mut cache)).fold(f64::NEG_INFINITY, f64::max))).collect();
    // Same as VSR: stable sort, keeping the data order when scores are equal.
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let (best, second) = (ranked[0], ranked.get(1).copied().unwrap_or(("", 0.0)));
    VsrPick { holds: vsr_holds_an_icon(img, r), best: best.0.to_string(), score: best.1, second_name: second.0.to_string(), second: second.1 }
}

/// VSR's holds_an_icon.
fn vsr_holds_an_icon(img: &Rgba, r: Rect) -> bool {
    let (mut n, mut dark) = (0usize, 0usize);
    for y in r.y.max(0)..(r.y + r.h).min(img.h as i32) {
        for x in r.x.max(0)..(r.x + r.w).min(img.w as i32) {
            let m = img.rgb(x, y).into_iter().max().unwrap_or(0);
            n += usize::from(m >= VSR_BRIGHT);
            dark += usize::from(m < VSR_DARK);
        }
    }
    let area = f64::from(r.w * r.h);
    n as f64 / area >= VSR_FILLED_MIN && dark as f64 / area >= VSR_BACKDROP_MIN
}

/// JavaScript's Math.round (.5 rounds up).
fn js_round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

/// The value stored into a Uint8ClampedArray: clamped to 0..255, .5 rounds to even.
fn clamp_u8(x: f64) -> u8 {
    if x.is_nan() || x <= 0.0 {
        return 0;
    }
    if x >= 255.0 {
        return 255;
    }
    let f = x.floor();
    let d = x - f;
    let r = if d < 0.5 {
        f
    } else if d > 0.5 || f % 2.0 != 0.0 {
        f + 1.0
    } else {
        f
    };
    r as u8
}

/// V8's Math.hypot: scaled by the maximum, with Kahan compensation for the sum of squares.
fn js_hypot(v: &[f64]) -> f64 {
    let max = v.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    if max == 0.0 {
        return 0.0;
    }
    let (mut sum, mut comp) = (0.0f64, 0.0f64);
    for &x in v {
        let n = x.abs() / max;
        let summand = n * n - comp;
        let pre = sum + summand;
        comp = (pre - sum) - summand;
        sum = pre;
    }
    sum.sqrt() * max
}

/// VSR's signature: crop, then bilinear shrink to 24x24 (RGB each rounded to u8), then f32 grayscale, subtract the
/// mean, divide by the length.
fn vsr_signature(img: &Rgba, r: Rect) -> Vec<f64> {
    // crop: the top-left corner is clamped inside the frame, and the width and height do not exceed the frame.
    let x0 = r.x.max(0) as usize;
    let y0 = r.y.max(0) as usize;
    let cw = (img.w.saturating_sub(x0)).min(r.w as usize).max(1);
    let ch = (img.h.saturating_sub(y0)).min(r.h as usize).max(1);
    let px = |x: usize, y: usize, c: usize| -> f64 {
        let (x, y) = (x0 + x, y0 + y);
        if x < img.w && y < img.h { f64::from(img.data[(y * img.w + x) * 4 + c]) } else { 0.0 }
    };
    let (sx, sy) = (cw as f64 / VSR_N as f64, ch as f64 / VSR_N as f64);
    let mut g = Vec::with_capacity(VSR_N * VSR_N);
    for y in 0..VSR_N {
        let fy = ((y as f64 + 0.5) * sy - 0.5).max(0.0).min(ch as f64 - 1.0);
        let ya = fy.floor() as usize;
        let yb = (ch - 1).min(ya + 1);
        let ty = fy - ya as f64;
        for x in 0..VSR_N {
            let fx = ((x as f64 + 0.5) * sx - 0.5).max(0.0).min(cw as f64 - 1.0);
            let xa = fx.floor() as usize;
            let xb = (cw - 1).min(xa + 1);
            let tx = fx - xa as f64;
            let c: [f64; 3] = std::array::from_fn(|c| {
                let a = px(xa, ya, c) * (1.0 - tx) + px(xb, ya, c) * tx;
                let b = px(xa, yb, c) * (1.0 - tx) + px(xb, yb, c) * tx;
                f64::from(clamp_u8(a * (1.0 - ty) + b * ty))
            });
            g.push((0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]) as f32);
        }
    }
    let mean = g.iter().fold(0.0f64, |s, &v| s + f64::from(v)) / g.len() as f64;
    let v: Vec<f64> = g.iter().map(|&x| f64::from(x) - mean).collect();
    let n = js_hypot(&v);
    let norm = if n != 0.0 { n } else { 1.0 };
    v.iter().map(|x| x / norm).collect()
}

/// Centroid of the signature (the minimum is subtracted to use as weights).
fn vsr_centre(p: &[f64]) -> (f64, f64) {
    let lo = p.iter().fold(f64::INFINITY, |m, &v| if v < m { v } else { m });
    let (mut s, mut x, mut y) = (0.0, 0.0, 0.0);
    for (i, &v) in p.iter().enumerate() {
        let w = v - lo;
        s += w;
        x += w * (i % VSR_N) as f64;
        y += w * (i / VSR_N) as f64;
    }
    if s != 0.0 { (x / s, y / s) } else { ((VSR_N - 1) as f64 / 2.0, (VSR_N - 1) as f64 / 2.0) }
}

/// Shift the signature by (dx, dy), fill the vacated area with the minimum, then subtract the mean and divide by
/// the length.
fn vsr_moved(p: &[f64], dx: i32, dy: i32) -> Vec<f64> {
    let bg = p.iter().fold(f64::INFINITY, |m, &v| if v < m { v } else { m });
    let mut q = vec![0.0; p.len()];
    let mut sum = 0.0;
    for (i, v) in q.iter_mut().enumerate() {
        let x = (i % VSR_N) as i32 - dx;
        let y = (i / VSR_N) as i32 - dy;
        *v = if x >= 0 && (x as usize) < VSR_N && y >= 0 && (y as usize) < VSR_N { p[y as usize * VSR_N + x as usize] } else { bg };
        sum += *v;
    }
    let mean = sum / q.len() as f64;
    let mut norm = 0.0;
    for v in q.iter_mut() {
        *v -= mean;
        norm += *v * *v;
    }
    let norm = if norm.sqrt() != 0.0 { norm.sqrt() } else { 1.0 };
    for v in q.iter_mut() {
        *v /= norm;
    }
    q
}

/// VSR's icon_similarity: after aligning the centroids, shift within +-2 cells and take the largest inner product.
/// `cache` stores the signatures of this cell at each offset.
fn vsr_similarity(rf: &[f64], p: &[f64], cache: &mut Vec<((i32, i32), Vec<f64>)>) -> f64 {
    if p.len() != VSR_N * VSR_N || rf.len() != VSR_N * VSR_N {
        return rf.iter().enumerate().fold(0.0, |s, (i, x)| s + x * p.get(i).copied().unwrap_or(0.0));
    }
    let (rx, ry) = vsr_centre(rf);
    let (px, py) = vsr_centre(p);
    let (bx, by) = (js_round(rx - px) as i32, js_round(ry - py) as i32);
    let mut best = -1.0f64;
    for dy in by - VSR_REACH..=by + VSR_REACH {
        for dx in bx - VSR_REACH..=bx + VSR_REACH {
            let at = match cache.iter().position(|(d, _)| *d == (dx, dy)) {
                Some(i) => i,
                None => {
                    cache.push(((dx, dy), if dx == 0 && dy == 0 { p.to_vec() } else { vsr_moved(p, dx, dy) }));
                    cache.len() - 1
                }
            };
            let q = &cache[at].1;
            let s = rf.iter().zip(q).fold(0.0, |s, (a, b)| s + a * b);
            if s > best {
                best = s;
            }
        }
    }
    best
}

/// Templates: the solver's instrument id and the features (per-cell average, rounded to 0..255).
static TEMPLATES: &[(u8, [u8; IW * IH * 3])] = &[
    // `人蛹標本` (ranked game frames2)
    (1, [6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,3,6,2,3,6,2,3,6,1,3,6,2,4,6,1,3,6,1,3,6,1,3,7,2,4,6,2,3,6,2,3,6,2,3,6,2,3,6,2,4,6,2,4,6,2,4,6,2,3,6,2,4,6,2,3,6,1,3,6,1,3,6,1,2,6,1,3,6,1,3,6,2,4,7,1,4,6,1,3,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,7,2,3,10,6,10,39,31,49,67,51,83,65,49,78,33,24,37,6,1,4,6,1,2,6,1,3,6,1,3,6,1,3,6,1,3,6,2,4,6,1,3,6,2,4,6,2,4,6,2,4,6,2,4,6,2,3,6,2,4,11,5,9,56,43,67,113,93,129,118,94,133,130,106,145,88,69,106,34,24,41,6,1,3,6,1,3,6,2,3,6,1,3,6,2,3,6,2,4,6,1,3,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,7,2,4,24,17,26,96,78,113,134,106,156,80,57,93,96,72,111,129,103,152,63,49,77,11,6,11,6,1,3,6,2,3,6,2,3,6,2,4,6,2,4,6,2,3,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,9,4,7,38,29,45,93,72,112,122,96,144,63,43,76,82,60,96,129,102,153,69,51,87,23,16,26,5,1,3,6,2,4,6,2,3,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,9,4,7,46,36,54,98,77,119,106,80,127,70,48,83,95,72,109,116,89,138,80,62,98,26,20,30,5,1,4,6,2,4,6,2,3,6,2,4,6,2,4,7,2,4,6,2,4,7,2,4,7,2,4,6,2,4,6,2,4,10,5,9,44,35,53,88,68,110,119,90,141,67,43,82,82,58,99,112,84,136,75,59,94,23,18,29,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,7,2,4,6,2,4,7,2,4,7,2,4,6,2,4,6,2,4,8,4,7,33,25,40,79,61,99,118,90,143,92,66,115,108,81,130,102,75,126,63,47,81,21,16,25,6,2,3,6,2,3,6,2,4,6,2,4,6,2,4,7,2,4,6,2,4,7,2,4,7,2,4,6,2,4,6,2,4,6,2,4,21,13,25,67,49,87,97,71,120,91,65,112,101,74,123,88,62,111,46,32,64,11,7,12,6,2,3,6,2,3,6,2,4,6,2,4,6,2,4,7,2,4,6,2,4,7,2,3,7,2,3,7,2,3,7,2,4,6,2,4,11,7,11,44,32,55,87,66,110,94,68,117,94,68,119,74,55,95,28,21,37,5,3,5,6,2,4,6,2,3,6,2,4,7,2,4,7,2,4,7,2,4,6,2,4,7,2,3,7,2,3,7,2,3,7,2,4,6,2,4,7,2,5,20,13,24,61,47,79,78,56,101,82,59,105,44,31,60,9,5,12,5,2,3,6,2,4,6,2,3,7,2,4,7,2,4,7,2,4,7,2,4,6,2,4,7,2,4,7,2,4,7,2,4,7,2,4,6,2,3,6,2,3,6,2,3,14,8,15,46,35,61,33,24,45,6,2,6,6,2,3,6,2,4,7,2,4,7,2,4,7,2,4,7,2,4,7,2,4,7,2,4]),
    // `腫大腦垂體` (tutorial game frames)
    (4, [11,1,5,9,1,4,8,1,4,7,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,2,4,6,1,3,6,2,3,6,2,3,6,1,3,10,1,5,9,1,4,8,2,4,6,2,4,6,2,4,6,2,4,6,2,3,6,2,4,7,2,4,7,2,3,7,2,3,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,6,1,3,6,1,3,6,1,3,6,1,3,10,1,4,8,2,4,7,2,4,6,2,4,6,2,4,6,2,4,6,2,3,7,2,4,7,2,3,7,2,3,7,2,3,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,6,1,3,6,2,3,6,1,3,6,1,3,10,1,4,8,2,4,7,1,4,6,2,4,6,2,4,7,2,3,8,3,4,12,8,8,26,22,21,27,23,22,31,26,26,12,8,8,7,3,4,6,2,4,6,2,4,6,2,4,6,1,3,6,2,3,6,2,3,6,1,3,9,1,4,8,2,4,7,1,4,6,2,4,7,3,4,16,11,11,58,53,51,109,105,100,129,125,120,134,130,124,130,126,120,110,105,100,55,50,47,13,8,8,6,2,4,6,2,4,6,2,3,6,2,4,6,2,4,6,1,3,9,1,4,7,2,4,7,2,4,7,3,4,17,13,13,82,77,75,131,127,121,128,125,117,130,126,119,121,117,111,144,140,134,132,128,121,97,92,86,67,62,59,15,11,11,6,2,3,6,2,3,6,2,4,6,2,3,6,2,3,9,1,4,7,2,4,7,2,4,8,3,4,37,33,33,92,88,85,115,111,104,124,120,113,131,128,120,129,126,118,111,108,100,111,107,99,97,93,88,86,82,79,46,42,41,8,3,4,6,2,3,6,2,4,6,2,4,6,2,4,9,1,4,7,2,4,7,2,4,10,5,7,59,56,56,99,97,95,91,89,84,93,90,84,98,96,89,70,67,60,79,77,68,86,83,75,77,73,66,67,62,57,48,44,40,19,15,14,6,2,3,6,2,4,6,2,3,6,2,3,9,1,4,8,2,4,7,2,4,8,3,5,25,22,22,53,50,49,63,61,58,57,55,51,61,59,53,101,99,91,103,100,92,69,66,58,76,72,65,50,46,40,51,46,42,19,15,15,6,2,3,6,2,4,6,2,3,6,2,3,9,1,4,8,2,4,8,2,4,18,14,16,27,26,26,21,20,20,33,31,31,55,54,51,65,63,59,48,45,39,58,55,49,48,45,39,51,47,42,44,40,35,45,42,38,32,29,28,7,3,5,6,2,4,6,2,3,6,2,3,10,1,4,9,2,5,19,14,16,65,63,63,59,60,60,56,59,59,53,56,56,52,55,54,56,58,57,61,63,62,58,60,58,52,52,51,49,48,46,47,46,44,44,43,42,52,50,50,35,33,34,7,2,4,7,1,4,7,1,4,10,1,5,9,2,5,9,3,6,17,12,14,39,36,37,48,47,48,49,48,49,49,47,48,50,48,49,47,45,46,45,44,45,47,47,48,45,45,47,44,42,43,38,35,36,34,29,31,14,9,11,7,2,4,7,1,3,7,1,3,11,1,5,9,1,4,8,2,4,8,2,5,7,3,4,7,3,4,8,2,4,8,3,4,8,3,4,8,3,4,7,3,5,7,3,5,7,3,5,6,3,4,6,3,4,7,3,4,7,2,4,7,2,4,7,1,4,7,1,3]),
    // `簇生蟲卵` (ranked game frames2)
    (9, [4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,5,2,3,5,2,3,5,1,3,5,1,3,5,1,3,5,2,3,5,2,3,5,2,3,6,1,3,6,1,3,7,0,3,6,0,2,4,1,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,5,2,3,5,2,3,5,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,6,1,3,6,1,3,6,0,3,6,0,2,4,1,2,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,22,14,22,84,72,88,39,29,43,5,1,5,5,2,4,4,2,3,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,6,0,3,5,1,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,5,2,4,26,19,28,134,103,150,113,95,122,61,48,67,70,58,72,14,8,13,4,2,3,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,7,0,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,37,30,38,148,130,148,93,67,99,139,117,142,153,135,153,88,71,88,28,19,27,4,2,3,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,7,0,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,20,18,19,104,90,111,150,126,152,118,91,120,126,101,131,137,111,144,93,73,95,59,48,58,4,2,3,4,2,3,4,2,3,4,2,3,5,2,3,6,1,3,6,1,3,4,2,3,5,2,3,5,1,3,4,2,3,4,2,3,23,21,22,90,71,93,131,102,132,94,67,95,140,111,148,107,77,116,115,87,118,19,11,19,4,2,3,4,2,3,4,2,3,4,2,3,5,2,3,6,1,3,6,1,3,4,2,3,5,1,3,6,1,3,4,2,3,4,2,3,23,21,22,66,55,68,102,79,108,82,61,89,98,74,106,70,46,76,95,71,99,35,28,34,4,2,3,4,2,3,4,2,3,4,2,3,5,2,3,6,1,3,6,1,3,4,2,3,5,1,3,6,1,3,4,2,3,4,2,3,7,5,6,23,18,21,121,109,123,157,137,168,88,65,101,106,86,110,58,43,60,9,5,8,4,2,3,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,6,0,3,4,2,3,5,1,3,6,1,3,4,2,3,4,2,3,4,2,3,5,1,4,53,45,53,112,94,119,97,75,111,82,64,86,90,74,91,5,2,4,4,2,3,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,7,0,3,4,2,3,5,1,3,6,1,3,6,1,3,5,1,3,4,2,3,4,2,3,12,8,11,37,27,42,64,45,71,60,45,68,21,16,20,5,1,4,4,2,3,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,7,0,3,4,2,3,5,1,3,6,1,3,6,1,3,5,1,3,6,1,3,6,1,3,6,1,3,9,4,7,55,42,57,56,41,60,6,1,4,6,1,3,6,1,3,5,2,3,5,2,3,5,1,3,6,1,3,6,1,3,7,0,3,4,2,3,5,1,3,6,1,3,6,1,3,4,2,3,6,1,3,6,1,3,6,1,3,6,1,3,9,4,7,10,5,9,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,0,3,6,0,2]),
    // `黑山羊腸縫線` (tutorial game frames and ranked game frames2)
    (14, [6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,2,3,6,2,3,6,2,3,6,2,3,7,2,3,7,2,3,7,2,3,6,2,3,6,2,3,7,2,3,7,2,3,6,1,3,5,2,3,5,2,3,5,2,3,6,1,3,6,1,3,6,1,3,6,1,3,6,2,3,6,2,3,7,2,3,7,2,3,7,2,3,7,2,3,10,5,7,7,2,3,7,2,3,7,2,3,7,2,3,7,2,3,7,1,3,5,1,3,5,1,3,5,1,3,6,1,3,6,1,3,6,1,3,6,2,3,6,2,3,7,2,3,7,2,3,7,3,3,6,3,3,7,4,5,55,53,53,17,15,15,7,2,3,8,3,3,7,2,3,7,2,3,6,2,3,5,2,3,4,2,3,4,2,3,6,2,3,6,1,3,6,2,3,6,2,3,7,2,3,7,2,3,7,2,3,19,16,16,19,17,17,28,25,25,45,42,43,20,18,18,8,3,3,8,3,3,7,2,3,7,2,3,6,2,3,5,2,3,5,2,3,5,2,3,6,2,3,6,1,3,6,2,3,6,2,3,7,2,3,7,2,3,12,8,8,17,15,15,6,3,3,44,41,41,46,44,44,11,9,8,23,20,19,8,3,3,7,2,3,6,2,3,6,2,3,5,2,3,5,2,3,5,2,3,6,2,3,6,2,3,6,2,3,7,2,3,7,2,3,7,2,3,19,15,15,13,10,10,7,4,4,39,36,36,13,10,10,8,4,4,21,19,18,6,3,3,7,2,3,6,2,3,6,2,3,5,2,3,5,2,3,4,2,3,6,2,3,6,1,3,7,2,3,7,2,3,7,2,3,7,2,3,10,5,6,26,23,23,7,4,4,33,30,30,7,4,4,8,4,3,23,21,20,6,3,3,7,2,3,6,2,3,6,2,3,5,2,3,5,2,3,4,2,3,6,2,3,7,2,3,7,2,3,7,2,3,7,2,3,7,2,3,8,3,3,15,12,13,21,19,20,27,26,26,7,5,4,7,4,3,19,16,16,6,3,3,7,2,3,7,2,3,6,2,2,6,2,3,5,2,3,5,2,3,6,2,3,7,2,3,7,2,3,7,2,3,7,2,3,7,2,3,7,2,3,7,3,3,22,21,21,20,18,18,22,20,19,17,14,14,10,7,7,7,3,3,7,2,3,7,2,1,6,2,1,6,2,3,5,2,3,5,2,3,6,2,3,6,1,3,7,1,3,7,2,3,7,2,3,7,2,3,10,5,6,12,8,7,29,28,29,9,8,8,13,12,12,19,18,18,6,3,3,7,2,3,7,2,2,6,2,1,6,2,2,6,1,3,5,2,3,5,2,3,6,2,3,6,1,1,7,2,1,7,2,1,7,2,3,10,5,6,10,6,6,13,10,10,23,21,23,8,6,6,6,3,4,24,21,22,7,2,3,7,2,3,7,2,2,6,2,1,6,2,2,6,1,3,6,1,3,6,1,3,6,2,3,6,1,1,7,1,1,7,2,1,7,2,2,8,3,3,15,10,10,8,5,5,18,15,17,16,12,12,17,13,13,6,2,3,7,2,3,7,2,3,7,2,2,6,2,1,6,1,2,6,1,3,6,1,3,6,1,3,6,2,3,6,1,2,6,1,2,7,1,2,7,2,1,8,2,1,7,2,1,10,6,7,6,3,4,7,2,2,7,2,2,7,2,2,7,2,3,7,2,3,7,2,3,7,2,2,7,2,3,6,1,3,6,1,3,6,1,3]),
    // `二寸顱骨釘` (tutorial game frames)
    (16, [4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,5,2,3,6,1,3,6,1,3,6,1,3,6,0,2,4,1,2,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,6,1,3,6,1,3,6,1,3,6,0,2,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,3,1,2,4,2,3,4,2,3,13,11,12,16,14,15,15,13,14,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,6,0,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,6,4,5,41,39,40,53,51,52,87,86,86,32,30,31,4,2,3,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,6,0,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,23,22,23,37,37,37,51,51,51,95,94,95,20,18,20,4,2,4,4,2,4,4,2,3,5,1,3,6,1,3,7,0,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,5,3,4,44,43,44,65,65,65,51,51,51,32,32,31,21,20,22,4,2,3,4,2,3,4,2,3,5,2,3,6,1,3,6,1,3,4,2,3,5,1,3,6,1,3,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,30,28,29,66,65,66,70,70,71,31,31,31,34,34,35,29,28,32,4,2,3,4,2,3,4,2,3,5,2,3,6,1,3,6,1,3,4,2,3,5,1,3,6,1,3,4,2,3,4,2,3,4,2,3,4,2,3,12,10,11,58,56,57,62,62,63,22,24,25,1,3,4,2,3,4,3,3,3,4,2,3,4,2,3,4,2,3,5,2,3,6,1,3,6,1,3,4,2,3,5,1,3,6,1,3,5,1,3,5,2,3,4,2,3,6,5,5,37,36,36,48,48,48,23,22,23,3,2,3,3,2,3,3,2,3,4,2,2,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,6,0,3,4,2,3,5,1,3,6,1,3,6,1,3,5,2,3,4,2,3,25,25,25,48,48,48,26,26,26,4,3,3,4,2,3,4,2,4,4,2,3,4,2,3,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,7,0,3,4,2,3,5,1,3,6,1,3,6,1,3,5,2,3,9,7,8,38,37,38,20,20,22,3,3,4,5,2,3,6,1,3,6,1,3,6,1,3,5,1,3,4,2,3,4,2,3,4,2,3,5,1,3,6,1,3,7,0,3,4,2,3,5,1,3,6,1,3,6,1,3,5,1,3,25,22,24,13,12,14,5,2,2,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,5,1,3,5,2,3,5,1,3,6,1,3,6,0,3,7,0,3,4,2,3,5,1,3,6,1,3,6,1,3,5,1,3,5,2,4,5,2,3,5,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,1,3,6,0,3,6,0,2]),
    // `攣縮指爪` (ranked game of 2026-11-03 20:20, observe mode pairing: the icon newly added after this card was chosen)
    (6, [6,2,3,6,2,3,6,3,3,6,3,3,6,2,3,6,2,3,6,2,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,6,2,3,7,2,3,7,2,2,8,2,3,8,1,2,6,2,2,6,2,2,6,2,2,6,2,3,6,2,2,6,2,2,6,2,2,6,2,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,8,1,2,6,2,3,6,2,3,6,3,3,6,3,3,6,2,3,6,2,3,6,2,3,12,8,7,17,10,6,37,26,16,7,4,4,17,12,9,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,2,8,2,2,6,2,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,12,9,9,37,23,7,26,19,12,36,23,10,28,17,7,29,19,9,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,2,8,2,3,6,2,3,6,3,3,6,3,3,6,3,3,6,2,3,10,5,4,43,27,12,41,25,6,31,19,8,54,34,12,52,33,9,16,11,9,7,4,4,7,5,5,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,6,2,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,35,21,6,64,42,16,83,53,18,73,45,10,64,39,8,6,3,3,20,12,6,11,8,8,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,6,2,3,6,3,3,6,3,3,6,3,3,6,2,3,22,15,10,30,21,13,74,50,24,66,39,5,59,36,6,56,35,6,32,22,12,43,26,9,21,13,6,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,6,2,3,6,3,3,6,3,3,6,3,3,6,2,3,10,5,3,36,22,7,54,33,9,75,47,12,62,36,6,72,46,12,78,51,18,48,31,12,7,4,4,6,3,3,6,3,3,6,2,3,6,2,3,7,2,2,7,2,2,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,5,2,2,5,2,2,23,14,7,75,48,16,70,43,10,56,33,5,41,27,11,13,8,6,5,2,3,6,2,3,6,2,3,6,2,3,6,2,3,7,2,3,7,2,2,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,6,3,2,12,6,3,45,26,5,60,36,8,41,25,8,4,2,2,4,2,2,5,2,2,6,2,3,6,2,3,6,2,3,6,2,3,7,2,3,8,2,3,6,2,3,7,2,3,7,2,3,6,2,3,6,2,3,6,2,3,8,4,2,10,6,2,47,26,3,52,30,4,54,36,16,6,4,2,6,3,3,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,7,2,2,8,2,3,6,2,3,7,2,3,7,2,3,7,2,3,6,2,3,6,2,3,7,3,2,13,7,3,46,27,2,53,30,1,49,27,1,10,7,5,5,3,3,6,3,3,6,2,3,6,2,3,6,2,3,7,2,3,7,1,2,8,2,3,6,2,3,7,2,3,7,2,3,7,2,3,6,2,3,6,2,3,6,2,3,5,2,3,18,11,4,21,13,4,14,8,3,6,2,3,6,2,3,6,2,3,7,2,3,7,2,3,6,2,3,7,2,3,8,2,2,8,1,2]),
    // `疫區聖母像` (average of 35 frame cells from the ranked games of 2026-11-03 15:23 and 16:02, mutually >= 0.994):
    // the VSR fallback recognized it as `獸筋絞肉索` (0.97), so a whole game that chose `疫區聖母像` applied the wrong
    // effect.
    (19, [7,3,2,6,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,6,3,2,16,15,10,55,61,42,5,4,2,5,4,2,5,4,2,6,3,2,7,3,2,7,3,2,7,3,2,6,2,2,6,2,3,6,2,2,7,3,2,7,3,3,7,3,3,7,3,4,7,3,2,7,3,2,7,3,2,6,3,2,39,41,26,105,105,97,14,11,11,6,3,1,6,2,2,6,3,2,6,3,2,6,3,2,6,2,2,6,2,1,6,2,1,6,2,1,7,3,2,6,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,70,74,52,95,96,90,35,33,29,6,3,2,6,3,2,6,3,2,6,3,2,7,3,2,7,3,2,6,2,3,6,2,2,6,2,2,7,3,2,6,3,2,7,3,2,7,3,2,7,3,3,7,3,3,7,3,2,18,14,13,144,143,129,116,117,110,84,86,79,6,3,2,6,4,2,6,4,2,6,3,2,7,3,2,7,3,2,6,2,3,6,2,3,6,2,3,7,3,2,7,3,2,6,3,2,7,3,2,7,3,2,7,3,2,7,3,2,44,42,37,94,95,82,89,90,81,77,81,69,14,12,11,5,4,2,5,4,2,6,3,2,6,3,2,7,3,2,6,2,3,6,2,3,6,2,3,7,3,2,7,3,2,6,3,2,7,3,2,7,3,2,7,3,2,7,3,2,65,64,58,81,82,74,91,93,85,65,68,59,37,37,33,5,4,2,5,4,2,6,3,2,6,3,2,7,3,2,6,2,3,6,2,3,6,2,3,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,58,57,49,104,105,96,96,97,89,67,70,59,38,38,34,6,4,2,6,3,2,6,3,2,7,3,2,7,3,2,6,2,3,6,2,3,6,2,3,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,61,62,47,110,111,99,127,127,120,60,64,48,45,46,38,6,3,2,6,3,2,7,3,2,7,3,2,7,3,2,6,2,3,6,2,3,6,2,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,51,51,40,88,89,77,115,118,96,61,64,49,41,41,35,7,3,3,7,3,3,7,3,2,7,3,2,7,3,3,6,2,3,6,2,3,6,2,3,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,14,12,9,70,75,56,70,74,59,56,58,49,6,3,3,7,3,2,7,3,2,7,3,2,7,3,2,7,3,3,6,2,3,6,2,3,6,2,3,7,3,2,7,3,2,7,3,3,7,3,3,7,3,3,7,3,3,7,3,3,7,4,3,64,67,49,72,76,61,48,48,40,7,3,3,7,3,3,7,3,3,7,3,2,7,3,2,7,3,3,7,3,4,6,2,3,7,2,3,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,2,7,3,3,54,55,42,75,78,63,44,44,36,7,4,4,6,2,3,7,3,3,7,3,2,7,3,2,7,3,3,7,3,4,6,2,3,7,2,3,7,3,2,7,3,2,7,3,3,7,3,4,7,3,4,7,3,4,7,3,4,8,5,4,87,90,76,132,132,124,76,77,68,7,3,4,6,2,3,7,3,3,7,3,3,7,3,3,7,3,4,7,3,3,7,3,3,7,2,3]),
    // `獸筋絞肉索` (average of 10 frame cells from the ranked game of 2026-11-03 20:35): 0.05 against `疫區聖母像`.
    (11, [6,2,3,5,2,2,5,3,2,6,3,3,6,2,3,6,2,3,6,2,3,5,3,2,5,4,2,5,3,2,6,3,3,6,3,3,5,3,3,5,3,2,6,2,3,6,2,3,7,3,3,7,2,2,8,2,2,7,1,1,6,2,2,5,2,1,6,3,2,6,3,2,6,2,2,6,2,3,6,2,3,5,4,2,5,4,2,5,4,2,5,4,2,5,4,2,5,4,2,5,4,2,5,4,2,6,2,3,6,2,3,7,2,2,8,2,2,7,1,1,6,2,2,5,3,2,5,4,2,5,3,2,6,2,2,6,2,3,6,2,3,5,4,2,5,3,2,6,3,2,5,3,2,5,4,2,5,4,2,5,4,2,5,4,2,6,3,3,6,2,3,6,2,3,7,2,2,7,2,3,6,2,3,5,3,2,5,4,2,6,3,2,6,2,2,6,3,3,15,10,10,19,17,15,16,10,5,22,15,9,22,14,8,16,12,6,6,4,3,5,4,3,5,4,2,5,3,3,6,2,3,6,2,3,7,2,2,7,2,3,6,2,3,5,3,2,5,4,2,5,3,2,25,19,18,51,44,43,51,41,36,44,31,22,35,24,15,26,19,12,29,21,14,40,28,18,48,34,19,22,16,13,5,4,2,5,4,2,6,2,3,6,2,3,7,2,2,8,2,2,6,2,3,5,3,2,5,4,2,6,3,2,10,6,5,25,17,11,33,24,15,6,4,3,5,4,2,5,4,3,6,4,2,5,4,2,18,13,9,45,31,20,20,15,12,6,4,2,5,3,3,5,2,3,6,2,2,7,2,1,6,2,3,5,3,2,5,4,2,6,3,2,6,3,3,41,30,19,4,3,3,5,4,2,5,4,2,5,4,2,5,4,2,5,4,2,5,4,3,47,33,24,31,25,18,5,4,2,5,3,3,5,2,3,6,2,2,7,2,1,6,2,3,5,3,2,5,4,2,6,3,3,6,3,4,44,30,20,4,3,3,5,3,3,5,4,2,5,4,2,5,4,2,5,4,2,5,4,2,57,42,30,19,14,13,5,3,3,6,2,3,5,2,3,6,2,2,7,1,1,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,20,13,9,42,30,17,24,16,10,5,4,3,5,3,4,5,4,3,13,8,5,32,25,20,39,35,38,5,2,4,6,2,3,6,2,3,6,3,4,7,2,2,7,2,1,6,2,3,6,2,3,6,2,3,6,2,3,6,2,4,5,2,3,7,4,5,31,21,13,44,29,17,41,28,16,37,25,13,42,28,19,57,52,52,17,16,18,5,2,3,6,2,3,6,2,3,6,2,3,7,2,2,8,2,2,6,2,3,7,3,4,7,3,4,6,2,3,6,2,3,6,2,3,6,2,3,5,4,2,5,4,2,5,4,3,5,3,3,36,32,32,66,66,70,4,3,3,6,2,3,6,2,3,6,2,3,6,2,3,7,2,2,8,2,2,6,2,3,7,3,4,7,3,4,7,3,4,6,2,3,6,2,3,6,2,3,6,3,2,7,3,2,7,3,2,7,4,3,41,33,27,22,18,21,6,3,3,6,2,3,6,2,3,6,2,3,7,3,3,7,2,2,8,2,2,6,2,3,7,3,4,7,3,4,7,3,4,6,2,3,6,2,3,6,2,3,7,3,3,7,3,2,7,3,3,7,3,3,9,5,4,7,3,3,6,2,3,6,2,3,7,3,4,6,2,3,6,2,2,8,2,2,9,1,2]),
];

/// Instruments revealed on the intro page: not among the English solver's 25, they only change the initial board
/// (`蒸骨坩堝`: initial monsters become 2 groups of Bone Sentry and 2 groups of other monsters). The template is
/// taken from 75 cells of ranked game frames2 (all four positions), the same item has correlation 0.92-1.00, and at
/// most 0.69 against the other five. `接肢標本` (initial monsters become 6 groups of random monsters) is also of this
/// kind, but there is no sample yet: when it appears it is reported as unknown, and the template has to be added
/// later from real screenshots. `水蛭罐` (initial monsters become 4 groups of normal Pest): the template is the
/// average of 3 cells saved by observe mode in the 2026-11-03 ranked game, the correlation of the 3 cells with the
/// average is above 0.999, and at most 0.69 against the other six (`人蛹標本`). There is another copy for the positions
/// of cells 3 and 4 (same name; the two copies differ a lot, so the most similar and the second most similar will
/// not be the two copies of the same item).
pub static OPENING: &[(&str, [u8; IW * IH * 3])] = &[
    // `蒸骨坩堝` (ranked game frames2)
    ("蒸骨坩堝", [16,2,8,14,3,7,11,2,6,9,2,5,8,2,5,7,2,4,7,2,4,7,2,4,6,2,4,6,2,4,7,2,4,7,2,4,7,2,4,7,2,4,7,2,4,7,2,4,7,2,4,6,2,4,6,2,4,6,2,4,15,2,8,12,2,6,10,2,5,8,2,4,7,2,4,7,2,4,7,2,4,6,2,4,7,3,4,7,5,5,6,2,4,7,2,4,7,2,4,7,2,4,7,2,4,7,2,4,7,2,4,6,2,4,6,2,4,7,2,4,14,2,7,11,2,5,9,2,5,7,2,4,7,2,4,7,2,4,7,2,4,6,3,4,10,12,8,19,35,19,10,15,8,14,16,11,10,12,8,6,2,3,7,2,4,7,2,4,7,2,4,6,2,4,6,2,4,6,2,4,13,2,7,10,2,5,8,2,5,7,2,4,7,2,4,7,2,3,9,6,7,24,34,22,43,71,43,23,54,26,27,49,28,22,35,21,22,46,25,5,3,3,6,2,3,7,2,4,7,2,4,6,2,4,6,2,4,6,2,4,13,2,6,10,2,5,8,2,4,7,2,4,7,2,4,8,2,4,19,24,17,45,87,51,75,158,86,62,151,76,66,133,71,59,94,54,25,51,28,10,10,7,5,2,2,6,2,3,7,2,4,6,2,4,6,2,4,6,2,4,12,2,6,9,2,5,8,2,4,7,2,4,7,2,4,8,4,5,24,33,24,54,120,69,84,205,107,98,214,115,116,216,124,89,192,104,34,102,48,19,31,18,5,2,3,6,2,4,7,2,4,6,2,4,6,2,4,6,2,4,12,2,6,9,2,5,8,2,4,7,2,4,7,2,4,12,7,8,22,19,12,25,20,11,34,28,19,37,36,24,51,47,34,56,52,38,38,33,20,25,23,17,7,5,5,6,2,3,7,2,4,6,2,4,6,2,4,6,2,4,12,2,6,9,2,5,8,2,4,7,2,4,6,2,4,12,7,7,20,16,8,29,23,15,60,50,37,27,24,12,40,36,23,62,57,41,39,37,24,23,23,17,12,12,11,6,2,3,7,2,3,6,2,4,6,2,4,6,2,4,12,2,6,9,2,5,8,2,4,7,2,4,6,2,4,10,6,5,38,34,23,39,32,20,19,15,7,27,23,12,41,36,23,57,51,35,31,28,16,16,16,10,10,10,9,6,2,3,7,2,3,7,2,4,6,2,4,6,2,4,13,2,6,10,2,5,8,2,4,7,2,4,7,2,4,7,3,3,12,8,4,16,13,5,18,15,6,21,16,7,32,27,15,39,34,22,20,18,8,14,14,8,5,5,4,5,1,3,7,2,4,7,2,4,6,2,4,6,2,4,14,2,7,11,2,5,9,2,4,8,2,4,7,2,4,6,2,4,8,5,4,18,13,6,23,16,5,24,17,5,24,17,5,28,18,6,25,17,6,10,7,4,3,2,3,5,1,3,7,2,3,7,2,4,7,2,4,7,2,4,15,3,7,11,2,6,9,2,5,8,2,4,7,2,4,6,2,4,10,7,6,18,14,10,23,14,7,39,23,10,44,26,11,33,19,9,18,12,7,11,8,7,5,2,3,7,2,4,7,2,4,7,2,4,7,2,4,7,2,4,17,5,9,13,3,7,11,3,6,9,3,5,9,4,6,7,3,4,6,2,4,5,2,3,4,2,3,4,2,3,4,2,2,4,2,3,4,2,2,6,2,3,7,2,4,7,2,4,7,2,4,7,2,4,7,2,4,7,2,4]),
    // `水蛭罐` (2026-11-03 20:08 ranked game, average of 3 cells saved by observe mode)
    ("水蛭罐", [7,1,2,7,2,2,7,2,3,6,2,3,6,2,3,6,2,3,6,2,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,8,2,3,8,1,2,7,1,2,7,2,3,7,2,3,6,2,3,6,2,3,6,2,3,6,2,2,6,2,3,11,7,7,41,31,25,8,3,3,6,2,3,6,2,2,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,8,1,2,6,1,2,7,2,3,6,2,3,6,3,3,6,2,3,6,2,3,6,2,3,10,7,7,132,105,83,136,94,61,74,60,48,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,2,8,2,3,6,2,2,7,2,3,6,3,3,6,3,3,6,2,3,6,2,3,13,10,10,45,44,42,79,79,76,60,56,53,63,61,56,30,29,28,7,4,5,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,2,8,2,3,6,2,2,6,2,3,6,3,3,6,3,3,6,2,3,6,2,3,28,26,26,10,7,7,67,62,62,63,60,55,50,50,50,21,19,19,12,10,10,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,6,2,2,6,2,3,6,3,3,6,3,3,6,2,3,6,2,3,18,15,14,27,24,22,97,94,91,62,59,54,67,63,62,36,36,36,11,9,9,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,6,2,2,6,2,3,6,3,3,6,3,3,6,2,3,6,2,3,6,2,3,61,56,55,144,112,114,109,42,46,78,40,42,42,40,41,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,6,2,2,6,2,3,6,3,3,6,3,3,6,2,3,6,2,3,23,16,17,118,102,102,72,42,44,61,21,22,67,20,21,69,34,36,13,10,10,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,2,7,2,2,6,2,2,7,2,3,6,2,3,6,2,3,6,2,3,6,2,3,62,41,42,114,88,84,57,14,12,67,20,19,74,17,15,84,16,18,47,23,25,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,7,2,3,7,2,2,6,2,2,7,2,3,6,2,3,6,2,3,6,2,3,6,2,3,81,54,53,90,45,38,67,12,9,77,18,15,77,6,6,113,41,41,74,32,35,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,7,2,3,8,2,2,7,2,3,7,2,3,7,2,3,6,2,3,6,2,3,6,2,3,53,30,31,68,26,23,77,22,21,108,18,22,128,31,31,167,77,81,55,21,24,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,7,2,2,8,2,3,7,2,2,7,2,3,7,2,3,7,2,3,6,2,3,6,2,3,15,9,11,70,36,36,91,26,26,102,20,24,101,18,24,120,42,47,4,3,3,4,3,3,6,2,3,6,2,3,6,2,3,7,2,3,7,1,2,8,2,3,8,2,2,8,2,3,7,2,3,7,2,3,6,2,3,6,2,3,6,2,3,10,8,8,56,34,34,68,28,27,68,40,38,4,3,3,5,2,3,6,2,3,6,2,3,7,2,3,7,2,3,7,2,3,8,2,2,8,1,2]),
    // `水蛭罐`, cells 3 and 4 (2026-11-03 12:34 ranked game, 18 and 86 frames each averaged and then averaged
    // together): the icons in these two cells are 3-5 pixels to the right of SLOT_X, and the copy above (taken from
    // cells 1 and 2) scores only 0.83 / 0.70; the two copies are 0.78 against each other, with at most 0.78 / 0.82
    // in cells 1 and 2 and 0.99 in cells 3 and 4.
    ("水蛭罐", [18,5,9,16,4,8,13,4,7,10,4,6,9,4,5,9,4,5,9,4,5,9,4,5,8,4,4,8,4,5,9,4,6,8,4,5,8,4,4,8,4,4,8,3,3,8,3,3,8,3,3,8,4,4,8,4,4,8,3,3,17,5,9,14,4,7,11,4,6,9,4,5,8,4,5,8,4,5,8,4,5,8,4,5,8,4,5,22,16,14,38,29,24,7,4,5,7,3,4,8,4,4,8,4,4,8,4,4,7,3,4,8,4,4,8,4,4,8,4,4,16,4,8,13,4,7,10,4,5,9,4,5,8,4,5,8,4,5,8,3,5,8,4,5,29,25,27,157,119,86,142,99,65,52,44,38,8,4,4,7,3,3,8,4,4,8,4,4,8,4,4,8,4,4,8,4,4,8,4,4,15,4,8,12,4,7,9,4,5,9,4,5,8,4,5,8,4,5,8,4,5,27,23,24,50,49,47,87,86,82,67,62,57,61,61,57,28,25,24,7,4,4,8,4,4,8,4,4,8,4,4,8,4,4,8,4,4,8,4,4,14,4,8,11,4,6,9,4,4,9,4,5,8,4,5,8,4,5,9,4,5,36,34,33,11,7,7,83,77,76,72,70,66,40,38,39,28,26,26,11,8,9,8,4,4,8,4,4,8,4,4,8,4,4,8,4,4,8,4,4,14,4,7,11,3,5,9,4,5,9,4,5,8,4,5,8,4,5,9,4,5,34,30,29,26,23,22,113,110,107,72,69,64,54,51,49,42,41,41,10,6,7,8,4,4,8,4,4,8,4,4,8,4,5,8,4,4,8,4,4,14,4,7,11,3,5,9,4,5,9,4,5,8,4,5,9,4,6,8,4,5,15,10,10,93,85,85,154,105,107,110,41,46,82,54,55,33,31,32,8,4,4,8,3,3,8,3,4,8,3,4,8,4,5,8,4,4,8,4,4,14,4,7,11,3,5,9,4,5,9,4,5,8,4,5,8,4,5,9,4,6,43,33,34,131,112,112,71,37,37,73,29,30,72,19,21,64,37,38,10,6,6,8,3,3,8,3,3,8,3,4,8,4,5,8,4,4,8,4,4,14,4,7,11,4,6,9,4,4,9,4,5,9,4,5,8,5,5,12,7,8,94,69,70,109,77,74,65,18,17,74,21,21,84,19,17,95,23,27,32,18,20,8,3,3,8,3,3,8,3,3,8,4,5,8,3,4,8,3,3,15,4,7,11,4,6,9,4,4,9,4,5,9,4,5,8,5,5,26,19,19,87,54,52,94,42,35,69,9,8,86,19,16,90,12,12,131,50,52,50,22,25,8,3,3,8,3,3,8,3,4,8,4,5,9,3,4,8,3,3,16,5,7,13,4,7,10,4,5,9,4,4,9,4,5,8,4,5,8,5,5,76,42,43,75,28,25,92,26,25,121,22,26,144,41,42,177,81,84,31,16,17,8,4,5,8,4,4,8,3,3,9,4,4,9,4,5,8,3,3,17,5,8,14,4,7,11,4,5,9,4,4,9,4,5,9,4,4,9,4,5,31,21,22,81,36,37,106,30,31,108,19,25,123,25,33,95,36,40,5,3,4,6,3,3,8,4,5,8,3,3,9,3,4,9,3,5,8,3,4,19,7,9,15,5,7,13,4,7,10,4,5,10,5,5,10,4,4,9,4,5,8,4,5,23,18,18,64,35,35,77,36,35,57,35,34,6,4,4,7,3,4,8,3,4,9,3,4,8,3,3,9,3,3,9,3,4,9,3,5]),
    // `雜交試劑瓶` (initial monsters become 4 groups of rare monsters of different populations; 2026-11-03 21:29 ranked
    // game, the average of 2 cells saved by observe mode in cell 1, the two cells are 0.995 against each other).
    ("雜交試劑瓶", [7,1,2,7,2,2,7,2,3,6,2,3,6,2,3,6,2,3,6,2,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,8,2,3,8,1,2,6,1,2,7,2,3,7,2,3,6,2,3,6,2,3,6,2,3,6,2,2,6,2,3,6,2,3,6,2,3,6,2,2,6,2,3,6,2,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,8,1,2,6,1,2,7,2,3,6,2,3,6,3,3,6,2,3,6,2,3,6,2,2,6,2,3,6,2,3,34,35,39,28,30,32,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,6,2,3,7,2,2,8,2,2,6,2,2,7,2,3,6,2,3,6,3,3,6,2,3,6,2,3,6,2,3,11,8,9,46,48,52,52,56,64,25,26,28,6,3,3,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,2,8,2,3,6,2,2,6,2,3,6,2,3,6,3,3,6,2,3,6,2,3,6,2,3,28,28,30,22,22,22,30,30,33,12,10,10,14,11,10,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,8,2,2,6,2,2,6,2,3,6,3,3,6,3,3,6,2,3,6,2,3,6,2,3,46,45,46,6,3,3,23,23,24,16,15,16,40,40,42,15,14,14,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,3,7,2,2,6,2,2,6,2,3,6,3,3,6,3,3,6,2,3,6,2,3,6,2,3,40,40,40,6,3,3,25,40,20,16,15,16,52,53,54,11,9,9,6,3,3,6,3,3,6,3,3,6,3,3,6,2,3,7,2,3,7,2,2,6,2,2,6,2,3,6,3,3,6,3,3,6,2,3,6,2,3,37,36,38,84,88,92,6,3,3,42,46,34,8,4,4,82,88,92,24,23,24,6,3,3,6,3,3,6,3,3,6,2,3,6,2,3,7,2,2,7,2,2,6,2,2,7,2,3,6,2,3,6,2,3,6,2,3,6,2,3,20,18,20,48,52,58,6,3,3,20,19,19,12,10,10,58,61,64,29,29,30,6,2,3,6,2,3,6,2,3,6,2,3,6,2,3,7,2,3,7,2,2,6,2,2,7,2,3,6,2,3,6,2,3,6,2,3,6,2,3,50,52,57,40,42,46,12,12,12,20,20,20,70,74,80,20,20,20,39,24,26,22,20,22,6,2,3,6,2,3,6,2,3,6,2,3,7,2,3,8,2,2,7,2,3,7,2,3,7,2,3,6,2,3,6,2,3,29,31,36,78,84,96,24,30,48,40,43,50,26,28,28,107,50,56,113,32,38,118,53,60,36,24,27,6,2,3,6,2,3,6,2,3,6,2,3,7,2,2,8,2,3,7,2,2,7,2,3,7,2,3,7,2,3,5,2,3,74,112,150,99,117,166,45,61,146,68,98,170,58,62,76,83,42,41,131,39,44,130,70,72,24,15,12,6,2,3,6,2,3,6,2,3,7,2,3,7,1,2,8,2,3,8,2,2,8,2,3,7,2,3,7,2,3,5,3,3,18,22,25,35,62,73,46,75,88,34,52,50,7,6,6,4,3,3,16,9,6,24,14,10,5,3,3,6,2,3,7,2,3,6,2,3,7,2,3,8,2,3,8,1,2]),
];
