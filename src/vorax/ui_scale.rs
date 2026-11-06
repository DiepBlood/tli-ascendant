//! The game's interface scale (HANDOFF 3c, measured 2026-11-06).
//!
//! The game draws its 2D interface at the HUD's `PageScale` (the `FightCtrl` line of UE_game.log, see
//! `crate::app::gamelog`). Every position in [`super::read`] was measured at the user's own setting, 0.667, near
//! the smallest. At another setting each block of the interface is drawn `s = PageScale / 0.667` times as large
//! about its own anchor (recommended 0.841 is s = 1.261, the largest 1.0 is s = 1.499), while the 3D scene (the
//! tanks, candles and floor) does not scale. Measured on frames of 2026-11-06 07:54 (recommended) and 07:58
//! (largest) by matching static interface text and icons of the original setting at s +- 0.06: every 2D block
//! scales by the logged s (within 0.03) about one of four anchors ([`ZONES`]); the tank labels scale about their
//! own row ([`Zone::Label`]).
//!
//! A frame at another scale is read by turning it back, zone by zone, into how the original setting draws it
//! ([`unscale`], area averaged), and reading that with the reader as it is; the OCR input is cut from the frame
//! itself through the same zone map ([`super::scale::Frame::ui`]), at the frame's own resolution. At s near 1
//! (or with no s) none of this runs and the reader is the one of [`read::read_with_frame`].

use super::read::{self, H, Kind, Reading, TANK_X, W};
use super::scale::Frame;
use crate::reader::{Rect, Rgba};

/// The HUD `PageScale` of the setting every position in read.rs was measured at (the user's own setting).
pub const REF_PAGE_SCALE: f64 = 0.667;

/// A scale this close to 1 is the original setting: the frame is read as before and this module does not run
/// (the user's own `PageScale` is logged as 0.66702182888985, s = 1.00003).
pub const SAME: f64 = 0.005;

/// The scale logged and the one measured on the frame ([`measure`]) may differ by this share before the
/// measured one is taken.
pub const DISAGREE: f64 = 0.02;

/// The interface scale relative to the original setting, from the HUD's `PageScale`.
pub fn from_page_scale(page_scale: f64) -> f64 {
    page_scale / REF_PAGE_SCALE
}

/// The interface scale of a `PageScale` in millionths (as `Cue::UiScale` carries it).
pub fn from_micro(micro: u32) -> f64 {
    from_page_scale(f64::from(micro) / 1e6)
}

/// `s` is the original setting (see [`SAME`]).
pub fn is_original(s: f64) -> bool {
    (s - 1.0).abs() < SAME
}

/// A block of the interface: the reference rect it covers (original setting) and the point it scales about.
struct UiZone {
    rect: Rect,
    anchor: (f32, f32),
}

const fn ui_zone(x: i32, y: i32, w: i32, h: i32, ax: f32, ay: f32) -> UiZone {
    UiZone { rect: Rect::new(x, y, w, h), anchor: (ax, ay) }
}

/// The interface blocks, a later one winning where two overlap; anywhere else is the 3D scene. The anchors are
/// the fitted ones of the 2026-11-06 frames, rounded:
/// - the top left (the back arrow and the page name): (0, 0), fitted (4, 3) and (1, 1);
/// - the top right (the held instrument icons, [`super::icons`] at x 1948-2453, y 30-95): (2560, 0), the first
///   icon at (2319, 96) on a largest frame where (2400, 64) x 1.5 about (2560, 0) gives (2320, 96);
/// - the left edge (the round panel, final activity, next reward, the card and monster tooltip, the endgame
///   battle panel): (0, 720), fitted (4, 718), (0, 718), (0, 720), (7, 719) and (-3, 717);
/// - the bottom (the choice title, the cards, their type text, Confirm, Reroll, x4): (1280, 1440), fitted
///   (1277-1286, 1436-1440) on every block of both scales.
const ZONES: [UiZone; 4] = [
    ui_zone(0, 0, 700, 200, 0.0, 0.0),
    ui_zone(1900, 0, 660, 220, W as f32, 0.0),
    ui_zone(0, 200, 440, 1240, 0.0, H as f32 / 2.0),
    ui_zone(440, 620, 2120, 820, W as f32 / 2.0, H as f32),
];

/// The tank labels (archetype icon, name, total, "activity x count"): rows LABEL_TOP..LABEL_END within
/// [`LABEL_REACH`] of [`TANK_X`], right of LABEL_LEFT (the round panel). Each label is drawn about its own row:
/// its centre moved right by 16-18 px on the largest frames for tanks 1 and 2 (name, total and count rows
/// alike), which is scaling about TANK_X - 37 (+18.5 at s = 1.5); its rows sit about y 549 (the name row's centre
/// from 521 to 509, the count row's from 564 to 570; 549 gives 507 and 571.5).
const LABEL_TOP: f32 = 470.0;
const LABEL_END: f32 = 600.0;
const LABEL_LEFT: f32 = 340.0;
const LABEL_REACH: f32 = 175.0;
const LABEL_DX: f32 = -37.0;
const LABEL_Y: f32 = 549.0;
/// Half the width of a tank label's box, in frame pixels at every scale: the box does not grow with the
/// interface, only the text does (the longest monster name, 5 characters, is about 170 px at s = 1.5). Scaled
/// with the interface (110 x s) the three games read 4 more names on the recommended and 2 on the large-font game
/// but 1 fewer on the largest, and 4 fewer large-font numbers; a box that grows also reaches further into the
/// 3D scene and, at the largest scale, into the round panel (see [`PANEL_RIGHT`]).
const LABEL_HALF: f32 = 110.0;
/// The lowest row of a tank label kept, reference (original setting): rows from 571 down are hidden. The
/// count row's digits end at row 570 (glyph boxes y 556-571 on the original frames). Just below, the 3D candles
/// stand in front of tanks 2 and 5; they are fixed in the scene, so at s > 1 they come up under the unscaled count
/// row (at s = 1.5 a flame tip sat at rows 571-574 under both `6` of `61x36`, which read `?1x3?`). Chosen
/// 2026-11-06 over the three games at other scales (tanks whose activity x count = total; recommended, largest,
/// largest with large font): 575 161/161, 70/96, 95/109; 573 the same; 572 161, 78, 100; 571 161, 94, 108;
/// 570 154, 90, 102 (the digits' last row cut). A knife edge on both sides: a frame at another scale whose
/// numbers fall back should look here first.
const LABEL_BOTTOM: f32 = 571.0;
/// What a tank label's pixel outside its box ([`label_hidden`]) is painted: the dark of the label's own backing
/// (about (30, 23, 25) behind the digits). Black, darker than anything around the digits, lowered the digit
/// threshold (between the region's median and its brightest) so that the dim candle tips under the count row
/// stuck to the digits.
pub(crate) const HIDDEN: [u8; 3] = [30, 23, 25];

/// The round panel's right edge (original setting, anchored at x 0): a tank label's pixel that comes from left of
/// it is the panel, not the label.
const PANEL_RIGHT: f32 = 310.0;

/// Where a reference point (original setting) is drawn at another interface scale.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Zone {
    /// The 3D scene: not scaled.
    Scene,
    /// An interface block scaled about this point.
    Ui(f32, f32),
    /// Tank i's label, scaled about its own row (see [`LABEL_DX`]) and boxed (see [`label_hidden`]).
    Label(usize),
}

impl Zone {
    /// The point the zone scales about (None for the scene).
    pub(crate) fn anchor(self) -> Option<(f32, f32)> {
        match self {
            Zone::Scene => None,
            Zone::Ui(x, y) => Some((x, y)),
            Zone::Label(i) => Some((TANK_X[i] as f32 + LABEL_DX, LABEL_Y)),
        }
    }
}

/// The zone of reference point (x, y) (original setting).
pub(crate) fn zone_at(x: f32, y: f32) -> Zone {
    zone_in(x, y, true)
}

/// [`zone_at`], without the tank labels when `labels` is false (the endgame battle screen has none, and its panel
/// reaches into the rows tank 1's label has on the board).
fn zone_in(x: f32, y: f32, labels: bool) -> Zone {
    if labels
        && (LABEL_TOP..LABEL_END).contains(&y)
        && x >= LABEL_LEFT
        && let Some(i) = (0..TANK_X.len()).rev().find(|&i| (x - TANK_X[i] as f32).abs() <= LABEL_REACH)
    {
        return Zone::Label(i);
    }
    let inside = |r: &Rect| x >= r.x as f32 && x < (r.x + r.w) as f32 && y >= r.y as f32 && y < (r.y + r.h) as f32;
    ZONES.iter().rev().find(|z| inside(&z.rect)).map_or(Zone::Scene, |z| Zone::Ui(z.anchor.0, z.anchor.1))
}

/// The zone a reference rect reads from: the zone of its centre.
pub(crate) fn zone_of(r: Rect) -> Zone {
    zone_at(r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0)
}

/// Where a box on the reference screen at the original setting (centre x, y and half width, half height, as
/// [`super::Target`]) is drawn at interface scale `s` (the one the frame was read at, None for the original): a box
/// on an interface block (the cards, the reroll button) is scaled about its zone's anchor, one on the 3D scene (the
/// tank glass) is not moved. Unchanged, to the bit, at the original setting.
pub fn ui_box((x, y, half_w, half_h): (f32, f32, f32, f32), s: Option<f64>) -> (f32, f32, f32, f32) {
    let Some(s) = s.filter(|&s| !is_original(s)) else { return (x, y, half_w, half_h) };
    match zone_at(x, y).anchor() {
        Some((ax, ay)) => {
            let s = s as f32;
            (scaled(ax, s, x), scaled(ay, s, y), half_w * s, half_h * s)
        }
        None => (x, y, half_w, half_h),
    }
}

/// Where reference coordinate `v` (original setting) lands at scale `s` about anchor coordinate `a`.
pub(crate) fn scaled(a: f32, s: f32, v: f32) -> f32 {
    a + (v - a) * s
}

/// A pixel at (x, y) of the frame scaled to the reference (not unscaled) is outside tank i's label box at scale
/// `s`: beyond [`LABEL_HALF`] of the label's centre, left of the round panel's right edge, or below the label's
/// lowest row.
pub(crate) fn label_hidden(i: usize, s: f32, x: f32, y: f32) -> bool {
    let (ax, ay) = Zone::Label(i).anchor().unwrap_or_default();
    let centre = scaled(ax, s, TANK_X[i] as f32);
    (x - centre).abs() > LABEL_HALF || x < PANEL_RIGHT * s || y > scaled(ay, s, LABEL_BOTTOM)
}

/// The share of each source pixel along an axis of `n` pixels that output interval [lo, hi) covers (summing to 1
/// over the interval; a part outside the axis drops out, so it counts as black).
fn cover(lo: f32, hi: f32, n: usize) -> Vec<(u32, f32)> {
    let len = hi - lo;
    if len <= 0.0 {
        return vec![];
    }
    let mut out = Vec::with_capacity(4);
    let mut p = lo.floor().max(0.0) as usize;
    while (p as f32) < hi && p < n {
        let c = (hi.min(p as f32 + 1.0) - lo.max(p as f32)).max(0.0);
        if c > 0.0 {
            out.push((p as u32, c / len));
        }
        p += 1;
    }
    out
}

/// For each of `len` output pixels along an axis, the source pixels it averages at scale `s` about anchor `a`.
fn axis(a: f32, s: f32, len: usize, n: usize) -> Vec<Vec<(u32, f32)>> {
    (0..len).map(|o| cover(scaled(a, s, o as f32), scaled(a, s, o as f32 + 1.0), n)).collect()
}

/// The reference frame (2560x1440, see [`read::to_reference`]) of a game drawn at interface scale `s`, turned
/// back into how the original setting draws it: every interface pixel is the area average of the frame under its
/// footprint at scale `s` about its zone's anchor ([`zone_at`]); the 3D scene is copied; a tank label's pixel
/// outside its box ([`label_hidden`]) is painted [`HIDDEN`]. Only the pixels in `region` are made (the rest are
/// black) when it is given.
pub fn unscale(reference: &Rgba, s: f64, region: Option<Rect>) -> Rgba {
    unscale_in(reference, s, region, true)
}

/// [`unscale`], without the tank labels when `labels` is false (see [`zone_in`]).
fn unscale_in(reference: &Rgba, s: f64, region: Option<Rect>, labels: bool) -> Rgba {
    let s = s as f32;
    let (sw, sh) = (reference.w, reference.h);
    // The anchors along each axis (the label anchors included), and each one's table of source spans.
    let xs: Vec<f32> = ZONES.iter().map(|z| z.anchor.0).chain((0..TANK_X.len()).filter_map(|i| Zone::Label(i).anchor().map(|a| a.0))).collect();
    let ys: Vec<f32> = ZONES.iter().map(|z| z.anchor.1).chain([LABEL_Y]).collect();
    let cols: Vec<Vec<Vec<(u32, f32)>>> = xs.iter().map(|&a| axis(a, s, W, sw)).collect();
    let rows: Vec<Vec<Vec<(u32, f32)>>> = ys.iter().map(|&a| axis(a, s, H, sh)).collect();
    let find = |list: &[f32], v: f32| list.iter().position(|&a| a == v).unwrap_or(0);
    let r = region.unwrap_or(Rect::new(0, 0, W as i32, H as i32));
    let (x0, x1) = (r.x.clamp(0, W as i32) as usize, (r.x + r.w).clamp(0, W as i32) as usize);
    let (y0, y1) = (r.y.clamp(0, H as i32) as usize, (r.y + r.h).clamp(0, H as i32) as usize);
    let mut data = vec![0u8; W * H * 4];
    let paint = |first: usize, out: &mut [u8]| {
        for (k, line) in out.as_chunks_mut::<{ W * 4 }>().0.iter_mut().enumerate() {
            let y = first + k;
            if !(y0..y1).contains(&y) {
                continue;
            }
            for x in x0..x1 {
                let px = &mut line[x * 4..x * 4 + 4];
                let zone = zone_in(x as f32 + 0.5, y as f32 + 0.5, labels);
                let Some((ax, ay)) = zone.anchor() else {
                    if x < sw && y < sh {
                        let i = (y * sw + x) * 4;
                        px.copy_from_slice(&reference.data[i..i + 4]);
                    }
                    continue;
                };
                if let Zone::Label(i) = zone
                    && label_hidden(i, s, scaled(ax, s, x as f32 + 0.5), scaled(ay, s, y as f32 + 0.5))
                {
                    px.copy_from_slice(&[HIDDEN[0], HIDDEN[1], HIDDEN[2], 255]);
                    continue;
                }
                let mut acc = [0f32; 3];
                for &(sy, wy) in &rows[find(&ys, ay)][y] {
                    let src = &reference.data[sy as usize * sw * 4..(sy as usize + 1) * sw * 4];
                    for &(sx, wx) in &cols[find(&xs, ax)][x] {
                        let p = &src[sx as usize * 4..sx as usize * 4 + 3];
                        for c in 0..3 {
                            acc[c] += wy * wx * f32::from(p[c]);
                        }
                    }
                }
                for c in 0..3 {
                    px[c] = acc[c].round().clamp(0.0, 255.0) as u8;
                }
                px[3] = 255;
            }
        }
    };
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(16);
    let band = H.div_ceil(threads);
    std::thread::scope(|scope| {
        for (i, out) in data.chunks_mut(band * W * 4).enumerate() {
            let paint = &paint;
            scope.spawn(move || paint(i * band, out));
        }
    });
    Rgba { w: W, h: H, data }
}

/// The round panel's heart (left of `最終活性`, "final activity") at the original setting: bounding box centre
/// and size (measured over the original frames of 2026-11-06 01:00-04:43: (147.0, 602.5), 21x18 on every one).
const HEART: (f32, f32) = (147.0, 602.5);
const HEART_SIZE: (f32, f32) = (21.0, 18.0);
/// Where the heart is looked for (reference coordinates): it lies on the line from the panel's anchor (0, 720)
/// through [`HEART`], at s 0.85..1.7 within this box.
const HEART_SEARCH: Rect = Rect::new(100, 500, 180, 140);

/// The heart's red: the same test as the strip measurements of 2026-11-06 (the card art of a tooltip and the 3D
/// scene's dark red do not pass it).
fn heart_red([r, g, b]: [u8; 3]) -> bool {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    r >= 190 && g <= 100 && b <= 110 && r - g >= 110
}

/// The interface scale measured on a reference frame from the round panel's heart: its position about the panel's
/// anchor (0, 720) gives one s across and one down, and its size must agree with them. None when no red blob in
/// [`HEART_SEARCH`] fits (no round panel, covered, a glow). Measured on the frames of 2026-11-06 06:11 and 06:15:
/// (219, 544) and (185.5, 572.5), s 1.494 and 1.259 against the logged 1.499 and 1.261.
pub fn measure(reference: &Rgba) -> Option<f64> {
    let r = HEART_SEARCH;
    let (w, h) = (r.w as usize, r.h as usize);
    let red: Vec<bool> = (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| heart_red(reference.rgb(r.x + x as i32, r.y + y as i32))).collect();
    let mut seen = vec![false; w * h];
    let mut best: Option<(usize, f64)> = None;
    for start in 0..w * h {
        if !red[start] || seen[start] {
            continue;
        }
        // One 8-connected blob: its pixel count and bounding box.
        let (mut stack, mut n) = (vec![start], 0usize);
        let (mut bx0, mut by0, mut bx1, mut by1) = (usize::MAX, usize::MAX, 0, 0);
        seen[start] = true;
        while let Some(p) = stack.pop() {
            let (x, y) = (p % w, p / w);
            n += 1;
            (bx0, by0, bx1, by1) = (bx0.min(x), by0.min(y), bx1.max(x), by1.max(y));
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                    if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                        let q = ny as usize * w + nx as usize;
                        if red[q] && !seen[q] {
                            seen[q] = true;
                            stack.push(q);
                        }
                    }
                }
            }
        }
        if n < 25 {
            continue;
        }
        let cx = r.x as f32 + (bx0 + bx1) as f32 / 2.0;
        let cy = r.y as f32 + (by0 + by1) as f32 / 2.0;
        let (bw, bh) = ((bx1 - bx0 + 1) as f32, (by1 - by0 + 1) as f32);
        let across = cx / HEART.0;
        let down = (H as f32 / 2.0 - cy) / (H as f32 / 2.0 - HEART.1);
        let s = (across + down) / 2.0;
        let fits = (across - down).abs() <= 0.03 * s
            && (0.8..=1.8).contains(&s)
            && (bw / HEART_SIZE.0 / s - 1.0).abs() <= 0.2
            && (bh / HEART_SIZE.1 / s - 1.0).abs() <= 0.2;
        if fits && best.is_none_or(|b| n > b.0) {
            best = Some((n, f64::from(s)));
        }
    }
    best.map(|b| b.1)
}

/// The scale a frame was read at and what was read.
pub struct Scaled {
    pub reading: Reading,
    /// The interface scale the frame was read at: None for the original setting (read as
    /// [`read::read_with_frame`] reads it).
    pub s: Option<f64>,
    /// The reference frame unscaled to the original setting ([`unscale`]), when `s` is Some.
    pub unscaled: Option<Rgba>,
}

/// Read a reference frame (`img`, see [`read::to_reference`]; `frame` the frame it was scaled from, as for
/// [`read::read_with_frame`]) of a game drawn at interface scale `ui` (the logged one, None when unknown):
/// - `ui` None or the original setting ([`is_original`]): read as [`read::read_with_frame`]; only when that finds
///   no round panel (`Other`) is the scale measured on the frame ([`measure`]), and when it is off the original
///   by more than [`DISAGREE`] the frame is read again at the measured scale (with a problem saying so);
/// - otherwise the measured scale checks the logged one: off by more than [`DISAGREE`], the measured one is taken
///   (with a problem); not measurable, the logged one.
pub fn read(img: &Rgba, frame: Option<&Rgba>, ui: Option<f64>) -> Scaled {
    let logged = ui.filter(|&s| !is_original(s));
    let Some(s) = logged else {
        let reading = read::read_with_frame(img, frame);
        if reading.kind != Kind::Other || reading.round.is_some() {
            return Scaled { reading, s: None, unscaled: None };
        }
        return match measure(img).filter(|m| (m - 1.0).abs() > DISAGREE) {
            Some(m) => {
                let mut out = read_at(img, frame, m);
                let why = if ui.is_none() { "遊戲日誌沒有介面縮放值" } else { "遊戲日誌的介面縮放是原設定" };
                out.reading.problems.push(format!("{why}，影格量到縮放 {m:.3}，照量到的讀"));
                out
            }
            None => Scaled { reading, s: None, unscaled: None },
        };
    };
    let measured = measure(img).filter(|m| (m / s - 1.0).abs() > DISAGREE);
    let use_s = measured.unwrap_or(s);
    // A measured scale within DISAGREE of the original is the original (the measure is not finer than that); the
    // logged one keeps the narrower SAME.
    let original = match measured {
        Some(m) => (m - 1.0).abs() <= DISAGREE,
        None => is_original(s),
    };
    let mut out = if original { Scaled { reading: read::read_with_frame(img, frame), s: None, unscaled: None } } else { read_at(img, frame, use_s) };
    if let Some(m) = measured {
        out.reading.problems.push(format!("給定的介面縮放是 {s:.3}（遊戲日誌或本局前一張影格量到），影格量到 {m:.3}，照量到的讀"));
    }
    out
}

/// Read a reference frame at interface scale `s` (not the original setting): unscaled ([`unscale`]) for the
/// colours, icons and digits, the OCR input cut from the frame through the zone map.
fn read_at(img: &Rgba, frame: Option<&Rgba>, s: f64) -> Scaled {
    let Some(at) = Frame::ui(frame.unwrap_or(img), s as f32) else {
        // An empty frame: nothing to cut the OCR input from, read as at the original setting.
        return Scaled { reading: read::read_with_frame(img, frame), s: None, unscaled: None };
    };
    let unscaled = unscale(img, s, None);
    let reading = read::read_unscaled(&unscaled, at);
    Scaled { reading, s: Some(s), unscaled: Some(unscaled) }
}

/// The endgame battle panel's rect ([`read::BATTLE_PANEL`]) at interface scale `s`: it scales about the left
/// edge's anchor (0, 720) like the round panel (the `下個獎勵` label, about (32, 690) at the original setting, is
/// at (40, 682) recommended and (46-48, 675) largest), clipped to the reference.
pub fn battle_panel(s: f64) -> Rect {
    let r = read::BATTLE_PANEL;
    if is_original(s) {
        return r;
    }
    let (s, a) = (s as f32, H as f32 / 2.0);
    let (x0, y0) = ((r.x as f32 * s).round() as i32, scaled(a, s, r.y as f32).round() as i32);
    let (x1, y1) = (((r.x + r.w) as f32 * s).round() as i32, scaled(a, s, (r.y + r.h) as f32).round() as i32);
    let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(W as i32), y1.min(H as i32));
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

/// Read the endgame battle panel of a reference frame drawn at interface scale `s` ([`read::read_battle`] of the
/// panel unscaled; as it is at the original setting or with no scale), and the panel's crop for saving (the frame's
/// own pixels under [`battle_panel`]).
pub fn read_battle(reference: &Rgba, s: Option<f64>) -> (read::Battle, Rgba) {
    match s.filter(|&s| !is_original(s)) {
        None => (read::read_battle(reference), reference.crop(read::BATTLE_PANEL)),
        Some(s) => (read::read_battle(&unscale_in(reference, s, Some(read::BATTLE_PANEL), false)), reference.crop(battle_panel(s))),
    }
}

/// For tests: an original-setting reference frame drawn as the game would draw it at interface scale `s` (the
/// inverse of [`unscale`], bilinear): every interface zone scaled about its anchor over the scene. The zones are
/// rects of the original that also hold 3D scene, while the game draws only the interface itself on top, so the
/// order here is what lies on top where scaled zones overlap: the round panel and the tooltip (the left edge's
/// zone left of the panel's right edge, or from the tooltip's top down; all of it without labels), the tank labels, the top corners, the
/// bottom, then the rest of the left edge's zone. The scene behind a scaled zone is the scaled original's, unlike
/// the game's.
/// `labels` false draws a screen without tank labels (the endgame battle, see [`zone_in`]).
#[doc(hidden)]
pub fn enlarge(original: &Rgba, s: f64, labels: bool) -> Rgba {
    let s = s as f32;
    let left = Zone::Ui(ZONES[2].anchor.0, ZONES[2].anchor.1);
    // Without labels (the battle), the whole left edge's zone is the panel.
    let panel = |x: f32, y: f32| !labels || x < PANEL_RIGHT || y >= 640.0;
    let mut zones: Vec<(Zone, bool)> = vec![(left, true)];
    zones.extend((0..TANK_X.len()).filter(|_| labels).map(|i| (Zone::Label(i), false)));
    zones.extend([1, 0, 3, 2].map(|k| (Zone::Ui(ZONES[k].anchor.0, ZONES[k].anchor.1), false)));
    let sample = |x: f32, y: f32| -> [u8; 4] {
        let (x, y) = ((x - 0.5).clamp(0.0, (W - 1) as f32), (y - 0.5).clamp(0.0, (H - 1) as f32));
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = ((x0 + 1).min(W - 1), (y0 + 1).min(H - 1));
        let (tx, ty) = (x - x0 as f32, y - y0 as f32);
        let at = |x: usize, y: usize, c: usize| f32::from(original.data[(y * original.w + x) * 4 + c]);
        let mut px = [255u8; 4];
        for (c, v) in px.iter_mut().enumerate().take(3) {
            let top = at(x0, y0, c) * (1.0 - tx) + at(x1, y0, c) * tx;
            let bottom = at(x0, y1, c) * (1.0 - tx) + at(x1, y1, c) * tx;
            *v = (top * (1.0 - ty) + bottom * ty).round() as u8;
        }
        px
    };
    let mut data = vec![0u8; W * H * 4];
    for (y, row) in data.as_chunks_mut::<{ W * 4 }>().0.iter_mut().enumerate() {
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (qx, qy) = (x as f32 + 0.5, y as f32 + 0.5);
            let from = zones.iter().find_map(|&(z, only_panel)| {
                let (ax, ay) = z.anchor()?;
                let (px, py) = (ax + (qx - ax) / s, ay + (qy - ay) / s);
                (zone_in(px, py, labels) == z && (!only_panel || panel(px, py))).then_some((px, py))
            });
            let (px_, py_) = from.unwrap_or((qx, qy));
            *px = sample(px_, py_);
        }
    }
    Rgba { w: W, h: H, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The advice boxes at another scale: the potion kit of frame 0044 (round 9, four cards, s = 1.26), seen at
    /// x 1843 on the frame (its frame's sides at 1660 and 1993, middle 1827), is inside its box scaled about the
    /// bottom's anchor (the fixed box missed it); the reroll box scales too, the tank glass (3D) does not, and at
    /// the original setting nothing moves.
    #[test]
    fn boxes_follow_the_interface_scale() {
        let s = Some(from_page_scale(0.841017));
        let kit = (crate::vorax::read::HAND4_X[3] as f32, 1140.0, 110.0, 185.0);
        let (x, y, hw, hh) = ui_box(kit, s);
        assert!((x - hw..=x + hw).contains(&1843.0) && (x - 1827.0).abs() < 3.0, "{x} ± {hw}");
        assert!((y - (1440.0 - 300.0 * 1.2609)).abs() < 0.5 && (hh / 185.0 - 1.2609).abs() < 0.001);
        let reroll = ui_box((1625.0, 1362.0, 75.0, 28.0), s);
        assert!(reroll.0 > 1625.0 && reroll.2 > 75.0, "{reroll:?}");
        let tank = (566.0, 345.0, 150.0, 245.0);
        assert_eq!(ui_box(tank, s), tank);
        for none in [None, Some(1.0), Some(from_page_scale(0.66702182888985))] {
            assert_eq!(ui_box(kit, none), kit);
        }
    }

    /// The zones of the reader's regions: the round panel, the title, the cards, the held icons, a tank label and
    /// the 3D scene.
    #[test]
    fn regions_fall_in_their_zones() {
        assert_eq!(zone_of(Rect::new(186, 495, 80, 90)), Zone::Ui(0.0, 720.0));
        assert_eq!(zone_of(Rect::new(40, 845, 370, 55)), Zone::Ui(0.0, 720.0));
        assert_eq!(zone_of(Rect::new(1150, 898, 540, 48)), Zone::Ui(1280.0, 1440.0));
        assert_eq!(zone_of(Rect::new(1615, 1345, 80, 30)), Zone::Ui(1280.0, 1440.0));
        assert_eq!(zone_of(Rect::new(2353, 30, 100, 65)), Zone::Ui(2560.0, 0.0));
        assert_eq!(zone_of(Rect::new(524 - 110, 503, 220, 32)), Zone::Label(0));
        assert_eq!(zone_of(Rect::new(2312 - 90, 554, 200, 23)), Zone::Label(5));
        assert_eq!(zone_at(1300.0, 300.0), Zone::Scene);
    }

    /// `cover` shares sum to 1 inside the axis and drop the part outside.
    #[test]
    fn cover_shares() {
        let c = cover(2.25, 3.75, 10);
        assert_eq!(c.len(), 2);
        assert!((c.iter().map(|p| p.1).sum::<f32>() - 1.0).abs() < 1e-6);
        assert!((c[0].1 - 0.5).abs() < 1e-6);
        let edge = cover(9.5, 10.5, 10);
        assert!((edge.iter().map(|p| p.1).sum::<f32>() - 0.5).abs() < 1e-6);
    }

    /// The panel heart at the original position and size measures 1; drawn 1.26 and 1.5 times about (0, 720), it
    /// measures that.
    #[test]
    fn the_heart_measures_the_scale() {
        for s in [1.0f32, 1.2609, 1.4993] {
            let mut img = Rgba { w: W, h: H, data: [30u8, 10, 12, 255].repeat(W * H) };
            let (cx, cy) = (HEART.0 * s, scaled(720.0, s, HEART.1));
            let (hw, hh) = (HEART_SIZE.0 * s / 2.0, HEART_SIZE.1 * s / 2.0);
            for y in 0..H {
                for x in 0..W {
                    let (dx, dy) = ((x as f32 + 0.5 - cx) / hw, (y as f32 + 0.5 - cy) / hh);
                    if dx.abs() <= 1.0 && dy.abs() <= 1.0 && dx * dx + dy * dy <= 1.6 {
                        let i = (y * W + x) * 4;
                        img.data[i..i + 3].copy_from_slice(&[230, 30, 40]);
                    }
                }
            }
            let m = measure(&img).unwrap();
            assert!((m - f64::from(s)).abs() < 0.01, "{s}: {m}");
        }
    }

    /// The battle panel grows about (0, 720) and keeps its original rect at the original setting.
    #[test]
    fn the_battle_panel_scales_about_the_left_edge() {
        assert_eq!(battle_panel(1.0), read::BATTLE_PANEL);
        let r = battle_panel(1.5);
        assert_eq!((r.x, r.y, r.w, r.h), (21, 468, 543, 495));
    }
}
