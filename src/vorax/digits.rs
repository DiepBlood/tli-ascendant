//! Digit glyph matching: the tank's total activity and "activity x count", the left panel's round and
//! final activity, and the reroll count. Windows OCR (Traditional Chinese) often drops or misreads these
//! small digits ("85 x 24" read as "85 × 4", the large round digits dropped entirely), so, following the
//! English vorax-scan, this uses glyph templates instead: bright, unsaturated pixels are strokes, they are
//! cut into glyphs by column, scaled to a fixed grid, and matched to the nearest template.

use crate::reader::{Rect, Rgba};

/// Glyph grid width and height: a glyph is scaled proportionally to height GH and centered in width GW.
pub const GW: usize = 12;
pub const GH: usize = 16;

/// One glyph: its extent on screen and its grid features (0..255).
#[derive(Clone, Debug)]
pub struct Glyph {
    pub x0: i32,
    pub x1: i32,
    pub y0: i32,
    pub y1: i32,
    pub feat: Vec<u8>,
}

impl Glyph {
    pub fn h(&self) -> i32 {
        self.y1 - self.y0
    }
}

/// Which kind of digits: the tank label's digits sit right next to a red heart, so reddish pixels are not
/// strokes; the panel's digits are pinkish in their upper half themselves, so they cannot be excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ink {
    Label,
    Plain,
}

/// Stroke intensity: bright and unsaturated (excludes the red heart icon, the green "+N" and the golden glow).
fn ink(p: [u8; 3], kind: Ink) -> f32 {
    let (mx, mn) = (p.iter().copied().max().unwrap_or(0), p.iter().copied().min().unwrap_or(0));
    // The heart's edge is dark pink: low saturation, but clearly more red than green.
    let reddish = kind == Ink::Label && i32::from(p[0]) - i32::from(p[1]) > 28;
    if mx - mn > 75 || reddish {
        return 0.0;
    }
    (u32::from(p[0]) * 77 + u32::from(p[1]) * 150 + u32::from(p[2]) * 29) as f32 / 256.0
}

/// The glyphs in `r`, left to right. Specks no taller than `min_h` do not count.
pub fn glyphs(img: &Rgba, r: Rect, min_h: i32, kind: Ink) -> Vec<Glyph> {
    let (w, h) = (r.w.max(0) as usize, r.h.max(0) as usize);
    if w == 0 || h == 0 {
        return vec![];
    }
    let lum: Vec<f32> = (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| ink(img.rgb(r.x + x as i32, r.y + y as i32), kind)).collect();
    // Pixels within two pixels of the heart (its bright red part) do not count either: the heart's pale pink
    // edge pixels would stick to the bottom left of the first digit.
    let lum: Vec<f32> = if kind == Ink::Label {
        let red: Vec<bool> = (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| {
            let [pr, pg, _] = img.rgb(r.x + x as i32, r.y + y as i32).map(i32::from);
            pr > 170 && pr - pg > 60
        }).collect();
        (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as i32, (i / w) as i32);
                let near = (-2..=2).any(|dy| (-2..=2).any(|dx| {
                    let (nx, ny) = (x + dx, y + dy);
                    nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h && red[ny as usize * w + nx as usize]
                }));
                if near { 0.0 } else { lum[i] }
            })
            .collect()
    } else {
        lum
    };
    // Threshold: between the brightest 1% in the region and the background.
    let mut sorted = lum.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let top = sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)];
    let base = sorted[sorted.len() / 2];
    if top < 60.0 || top - base < 30.0 {
        return vec![];
    }
    let thr = base + (top - base) * 0.45;
    let on = |x: usize, y: usize| lum[y * w + x] >= thr;
    // Column segmentation: a run of consecutive columns with strokes is one glyph.
    let cols: Vec<bool> = (0..w).map(|x| (0..h).any(|y| on(x, y))).collect();
    let mut out = Vec::new();
    let mut x = 0;
    while x < w {
        if !cols[x] {
            x += 1;
            continue;
        }
        let start = x;
        while x < w && cols[x] {
            x += 1;
        }
        // Specks separated above or below (candle flames, light spots) do not count: take the run of
        // consecutive rows with the most stroke pixels (at most one empty row inside).
        let rows: Vec<(usize, usize)> = (0..h).map(|y| (y, (start..x).filter(|&xx| on(xx, y)).count())).filter(|r| r.1 > 0).collect();
        let mut runs: Vec<(usize, usize, usize)> = Vec::new();
        for &(y, n) in &rows {
            match runs.last_mut() {
                Some(run) if y <= run.1 + 2 => {
                    run.1 = y;
                    run.2 += n;
                }
                _ => runs.push((y, y, n)),
            }
        }
        let Some(&(y0, y1, _)) = runs.iter().max_by_key(|r| r.2) else { continue };
        let y1 = y1 + 1;
        if ((y1 - y0) as i32) < min_h {
            continue;
        }
        out.push((start, x, y0, y1));
    }
    // Glyphs stuck to the heart or a candle flame and so taller than the other digits are cropped back to the
    // top and bottom edges the rest share (needs at least three glyphs to compute).
    let mut hs: Vec<usize> = out.iter().filter(|g| g.1 - g.0 >= 3).map(|g| g.3 - g.2).collect();
    hs.sort_unstable();
    if hs.len() >= 3 {
        let hmed = hs[hs.len() / 2];
        let tall: Vec<&(usize, usize, usize, usize)> = out.iter().filter(|g| g.1 - g.0 >= 3 && (g.3 - g.2) * 10 >= hmed * 8).collect();
        let mut tops: Vec<usize> = tall.iter().map(|g| g.2).collect();
        let mut bottoms: Vec<usize> = tall.iter().map(|g| g.3).collect();
        tops.sort_unstable();
        bottoms.sort_unstable();
        let (top_y, bottom_y) = (tops[tops.len() / 2], bottoms[bottoms.len() / 2]);
        for g in &mut out {
            if g.2 + 1 < top_y {
                g.2 = top_y;
            }
            if g.3 > bottom_y + 1 {
                g.3 = bottom_y;
            }
        }
    }
    // The label's first digit: left-hand columns with strokes only in the bottom 30% are the drip from the
    // heart and are cropped off (the crossbar of a "4" in its leftmost column is at 60-70% height, so it is kept).
    if kind == Ink::Label
        && let Some(g) = out.first_mut()
    {
        let upper = |x: usize, g: &(usize, usize, usize, usize)| (g.2..g.2 + (g.3 - g.2) * 7 / 10).any(|y| on(x, y));
        while g.1 - g.0 > 3 && !upper(g.0, g) {
            g.0 += 1;
        }
    }
    // Two digits stuck together ("44"): if the width exceeds 1.1 times the glyph height, cut at the column
    // with the fewest strokes in the middle section.
    let mut split = Vec::with_capacity(out.len());
    for g in out {
        let (gw, gh) = (g.1 - g.0, g.3 - g.2);
        if gh >= 8 && gw * 10 > gh * 11 {
            let ink = |x: usize| (g.2..g.3).filter(|&y| on(x, y)).count();
            let cut = (g.0 + gw * 3 / 10..g.0 + gw * 7 / 10).min_by_key(|&x| ink(x)).unwrap_or(g.0 + gw / 2);
            split.push((g.0, cut, g.2, g.3));
            split.push((cut + 1, g.1, g.2, g.3));
        } else {
            split.push(g);
        }
    }
    split
        .into_iter()
        .filter(|&(x0, x1, y0, y1)| x1 > x0 && y1 > y0 && (y1 - y0) as i32 >= min_h)
        .map(|(x0, x1, y0, y1)| Glyph { x0: r.x + x0 as i32, x1: r.x + x1 as i32, y0: r.y + y0 as i32, y1: r.y + y1 as i32, feat: feature(&lum, w, x0, x1, y0, y1, thr, top) })
        .collect()
}

/// The glyphs of a digit string: drop specks that are too narrow (the heart's edge), then group by spacing
/// and keep the group with the most glyphs (the gap between digits is smaller than the glyph height).
pub fn text_glyphs(img: &Rgba, r: Rect, min_h: i32, kind: Ink) -> Vec<Glyph> {
    let gs: Vec<Glyph> = glyphs(img, r, min_h, kind).into_iter().filter(|g| g.x1 - g.x0 >= 3).collect();
    let Some(hmax) = gs.iter().map(Glyph::h).max() else { return gs };
    let gap = (hmax * 4 / 5).max(8);
    let mut groups: Vec<Vec<Glyph>> = Vec::new();
    for g in gs {
        match groups.last_mut() {
            Some(last) if g.x0 - last.last().map_or(i32::MIN / 2, |p| p.x1) <= gap => last.push(g),
            _ => groups.push(vec![g]),
        }
    }
    groups.into_iter().max_by_key(|g| (g.len(), g.iter().map(|x| x.x1 - x.x0).sum::<i32>())).unwrap_or_default()
}

/// A glyph's grid features: scaled proportionally to height GH and centered; each cell is the area average
/// of the stroke intensity.
#[allow(clippy::too_many_arguments)]
fn feature(lum: &[f32], w: usize, x0: usize, x1: usize, y0: usize, y1: usize, thr: f32, top: f32) -> Vec<u8> {
    let (gw, gh) = ((x1 - x0) as f32, (y1 - y0) as f32);
    let ky = GH as f32 / gh;
    // A glyph that is too wide (such as "x") is scaled separately horizontally so its sides are not cropped.
    let kx = ky.min(GW as f32 / gw);
    let left = (GW as f32 - gw * kx) / 2.0;
    let mut f = vec![0u8; GW * GH];
    for cy in 0..GH {
        for cx in 0..GW {
            // The range within the glyph that this cell maps to.
            let (fx0, fx1) = ((cx as f32 - left) / kx, (cx as f32 + 1.0 - left) / kx);
            let (fy0, fy1) = (cy as f32 / ky, (cy as f32 + 1.0) / ky);
            if fx1 <= 0.0 || fx0 >= gw {
                continue;
            }
            let (mut sum, mut n) = (0.0, 0.0);
            let samples = 3;
            for sy in 0..samples {
                for sx in 0..samples {
                    let px = fx0 + (fx1 - fx0) * (sx as f32 + 0.5) / samples as f32;
                    let py = fy0 + (fy1 - fy0) * (sy as f32 + 0.5) / samples as f32;
                    if px < 0.0 || px >= gw || py < 0.0 || py >= gh {
                        n += 1.0;
                        continue;
                    }
                    let v = lum[(y0 + py as usize) * w + x0 + px as usize];
                    sum += ((v - thr * 0.7) / (top - thr * 0.7)).clamp(0.0, 1.0);
                    n += 1.0;
                }
            }
            f[cy * GW + cx] = (sum / n * 255.0).round() as u8;
        }
    }
    f
}

/// Template: a character and its features.
pub struct Template {
    pub ch: char,
    pub feat: &'static [u8; GW * GH],
}

/// The character a glyph most resembles and its distance (mean per-cell difference, 0..255), plus the
/// distance of the second best.
pub fn classify(g: &Glyph, templates: &[Template]) -> Option<(char, f32, f32)> {
    let mut best: Option<(char, f32)> = None;
    let mut second = f32::MAX;
    for t in templates {
        let d = g.feat.iter().zip(t.feat.iter()).map(|(&a, &b)| (f32::from(a) - f32::from(b)).abs()).sum::<f32>() / (GW * GH) as f32;
        match best {
            Some((c, bd)) if d < bd => {
                if c != t.ch {
                    second = second.min(bd);
                }
                best = Some((t.ch, d));
            }
            Some((c, _)) if c != t.ch => second = second.min(d),
            None => best = Some((t.ch, d)),
            _ => {}
        }
    }
    best.map(|(c, d)| (c, d, second))
}
