//! Where the affix panel is on a frame and whether it changed between two frames: shared by the live frames
//! (`app::affix_live`) and the samples of private builds (`app::affix_samples`).

use super::bullets::REF_HEIGHT;
use crate::reader::{Rect, Rgba};

/// The panel's surroundings in reference pixels (2560x1440 at the user's UI scale the panel is x 100..620,
/// y 380..990): wide enough for a larger interface scale. The samples keep it lossless.
const AREA: Rect = Rect::new(0, 200, 1100, 1100);
/// The panel's text area (reference pixels), compared between frames: the map around it may move.
const TEXT: Rect = Rect::new(100, 380, 520, 610);
/// Cell size of the comparison (reference pixels) and the grey change in one cell that makes two panels differ: one
/// new or reverted affix line changes many cells far more.
const CELL: i32 = 16;
const CELL_DIFF: u8 = 4;

/// Whether a `w` x `h` frame is 16:9 (within a few pixels): the panel's place is measured on those only.
fn wide_16_9(w: usize, h: usize) -> bool {
    h > 0 && (w * 9).abs_diff(h * 16) <= 16 * 4
}

/// Reference rect `r` on a frame `h` high, scaled from the left edge.
fn scaled(r: Rect, h: usize) -> Rect {
    let k = h as f64 / REF_HEIGHT;
    let s = |v: i32| (f64::from(v) * k).round() as i32;
    Rect::new(s(r.x), s(r.y), s(r.w), s(r.h))
}

/// The panel's surroundings on a `w` x `h` frame (scaled by the height from the left edge), or None when the frame is
/// not 16:9 (the panel's place there is not measured yet).
pub fn area(w: usize, h: usize) -> Option<Rect> {
    wide_16_9(w, h).then(|| scaled(AREA, h))
}

/// The panel's text area on a `w` x `h` frame (scaled by the height from the left edge: what the reader reads and the
/// prints look at), or None when the frame is not 16:9.
pub fn text_area(w: usize, h: usize) -> Option<Rect> {
    wide_16_9(w, h).then(|| scaled(TEXT, h))
}

/// The mean grey of each cell of the panel's text area; of the whole frame in 64 x 36 cells when the frame is not 16:9.
pub fn print(frame: &Rgba) -> Vec<u8> {
    let (area, cw, ch) = if wide_16_9(frame.w, frame.h) {
        let c = scaled(Rect::new(0, 0, CELL, CELL), frame.h);
        (scaled(TEXT, frame.h), c.w.max(1), c.h.max(1))
    } else {
        let whole = Rect::new(0, 0, frame.w as i32, frame.h as i32);
        (whole, (whole.w / 64).max(1), (whole.h / 36).max(1))
    };
    let mut out = Vec::new();
    let mut y = area.y;
    while y + ch <= area.y + area.h {
        let mut x = area.x;
        while x + cw <= area.x + area.w {
            let mut sum = 0u64;
            for yy in (y..y + ch).step_by(2) {
                for xx in (x..x + cw).step_by(2) {
                    let [r, g, b] = frame.rgb(xx, yy);
                    sum += u64::from(r) + u64::from(g) + u64::from(b);
                }
            }
            let n = (((ch + 1) / 2) * ((cw + 1) / 2)) as u64 * 3;
            out.push((sum / n.max(1)) as u8);
            x += cw;
        }
        y += ch;
    }
    out
}

/// A pixel this bright (its brightest channel) is text: the panel's ground reads 28 to 46, the map behind the half-clear
/// panel stays dim through it, and the text (white, red, orange, cyan) is far brighter.
const TEXT_BRIGHT: u8 = 110;
/// A cell's share of text pixels (of 255) may change this much between two prints of the same text.
const TEXT_DIFF: u8 = 20;

/// The text of the panel's text area: each cell's share of text pixels ([`TEXT_BRIGHT`], of 255), cells as [`print`]'s.
/// Unlike [`print`] it stays the same when the background behind the panel moves, and changes when the words do.
pub fn text_print(frame: &Rgba) -> Vec<u8> {
    let (area, cw, ch) = if wide_16_9(frame.w, frame.h) {
        let c = scaled(Rect::new(0, 0, CELL, CELL), frame.h);
        (scaled(TEXT, frame.h), c.w.max(1), c.h.max(1))
    } else {
        let whole = Rect::new(0, 0, frame.w as i32, frame.h as i32);
        (whole, (whole.w / 64).max(1), (whole.h / 36).max(1))
    };
    let mut out = Vec::new();
    let mut y = area.y;
    while y + ch <= area.y + area.h {
        let mut x = area.x;
        while x + cw <= area.x + area.w {
            let (mut text, mut n) = (0u32, 0u32);
            for yy in (y..y + ch).step_by(2) {
                for xx in (x..x + cw).step_by(2) {
                    let [r, g, b] = frame.rgb(xx, yy);
                    text += u32::from(r.max(g).max(b) >= TEXT_BRIGHT);
                    n += 1;
                }
            }
            out.push((text * 255 / n.max(1)) as u8);
            x += cw;
        }
        y += ch;
    }
    out
}

/// Whether two text prints ([`text_print`]) show the same words: same size and no cell's share changed by more than
/// [`TEXT_DIFF`].
pub fn same_text(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.abs_diff(*y) <= TEXT_DIFF)
}

/// Whether two prints show the same panel: same size and no cell changed by more than [`CELL_DIFF`].
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.abs_diff(*y) <= CELL_DIFF)
}

/// The mean grey of the text area at most which the panel is on screen: its dark ground read 28 to 46 on all 132
/// screenshots and the live test's frames, the map before the panel comes in 125 to 164 (2026-11-09).
const DARK: f64 = 80.0;

/// The mean grey of a print's cells: of the panel's text area (of the whole frame when it is not 16:9).
pub fn grey(print: &[u8]) -> f64 {
    if print.is_empty() {
        return 255.0;
    }
    print.iter().map(|&v| f64::from(v)).sum::<f64>() / print.len() as f64
}

/// Whether a `w` x `h` frame's print ([`print`]) shows the panel: its text area is dark ([`DARK`]); the map before the
/// panel comes in, and an empty print, do not. A frame not 16:9 always may: its print is of the whole frame, mostly
/// the bright map whether the panel shows or not, so it would never read (the reads tried such frames before the
/// threshold, 2026-11-09, and still do).
pub fn on_screen(print: &[u8], w: usize, h: usize) -> bool {
    !wide_16_9(w, h) || grey(print) <= DARK
}

/// The panel's title bar ("Base Affix", 基礎詞綴) in reference pixels: black from x 120 to 596, y 384 to 425, its words
/// at the left (2026-11-09, the screenshots). The parts looked at stay inside it, and inside the panel's ground above
/// and below it, so 1080p frames scaled to the reference keep them whole.
const TITLE_BAR: Rect = Rect::new(300, 390, 280, 30);
const TITLE_WORDS: Rect = Rect::new(128, 392, 132, 26);
const TITLE_ABOVE: Rect = Rect::new(300, 372, 280, 8);
const TITLE_BELOW: Rect = Rect::new(300, 430, 280, 8);

/// Whether the affix panel's title bar shows on `frame` (scaled by the height from the left edge): nine in ten of the
/// bar's pixels black, the panel's ground above and below it (mean brightest channel 22 or more: 31 to 64 measured,
/// black screens and other screens' black boxes up to 20), and its words white (8% of their part or more: 16% to 25%
/// measured, other screens at most 2%). On 124 of the 132 screenshots of the panel and their 1080p copies; the other
/// eight are not panels or have Ascendant's window over the title; none of 3000 frames of other screens (2026-11-09).
/// About 3000 pixels looked at.
pub fn title_shows(frame: &Rgba) -> bool {
    title_at(frame, 1.0)
}

/// The interface scales [`other_scale`] tries, in hundredths of the one the reader was measured at: the game's
/// settings run from about 0.667 (the user's, 1.0 here) to 1.0 (1.5 here, the largest); the recommended 0.841 is
/// 1.26. Below 0.95 and above 1.05 only: near 1 [`title_shows`] already sees the bar. A step of 0.01 moves the
/// parts looked at by 3 reference pixels at the most, inside the bar's margins.
const OTHER_SCALES: [std::ops::RangeInclusive<u32>; 2] = [85..=95, 105..=160];

/// The interface scale the affix panel is drawn at when it is not the one the reader was measured at: its title bar
/// ([`title_shows`]) found drawn `s` times as large about the left edge's middle (0, 720), the anchor the game's
/// left-hand interface scales about (`crate::vorax::ui_scale`; the bar measured on the largest setting's frames of
/// 2026-11-10 at x 177 to 890, y 217 to 280, 1.5 times x 120 to 596, y 384 to 425 about that point). None when no
/// scale of [`OTHER_SCALES`] shows it. At the larger scales (1.26, 1.5) the reader finds no bullet at all; this tells
/// such a panel from no panel, so the page can say why nothing is framed (2026-11-10, the user's test N1).
pub fn other_scale(frame: &Rgba) -> Option<f64> {
    OTHER_SCALES.into_iter().flatten().map(|c| f64::from(c) / 100.0).find(|&s| title_at(frame, s))
}

/// The title bar's black box at the measured interface scale (reference pixels, edges exclusive at the right and the
/// bottom): x 120 to 597, y 384 to 426 on the screenshots of 2026-11-09.
const BAR_LEFT: f64 = 120.0;
const BAR_TOP: f64 = 384.0;
const BAR_W: f64 = 477.0;
const BAR_H: f64 = 42.0;

/// Where and how large the affix panel is drawn on a frame (reference pixels of the frame scaled to 1440 rows): the
/// title bar's left and top edges and the scale against the measured interface scale. A point of the panel at the
/// measured scale, (x, y), is at ([`Placement::x`], [`Placement::y`]): the panel scales about its own title bar, which
/// needs no anchor (the left edge's middle, (0, 720), puts the bar within 3 pixels of its place at 1.5; its edges put
/// it on them).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub s: f64,
    pub left: f64,
    pub top: f64,
}

impl Placement {
    /// The panel where the reader was measured.
    pub const MEASURED: Placement = Placement { s: 1.0, left: BAR_LEFT, top: BAR_TOP };

    /// Where reference column `x` of the panel at the measured scale is drawn.
    pub fn x(&self, x: f64) -> f64 {
        self.left + (x - BAR_LEFT) * self.s
    }

    /// Where reference row `y` of the panel at the measured scale is drawn.
    pub fn y(&self, y: f64) -> f64 {
        self.top + (y - BAR_TOP) * self.s
    }
}

/// The affix panel's [`Placement`] when it is drawn at another interface scale: the scale [`other_scale`] finds the
/// title bar at, then the bar's four edges measured there (the darkest channel 18 or less is the bar: it is 12 or less,
/// the panel's ground around it 22 or more). The scale is the bar's width over its measured width (477 pixels: one
/// pixel off is 0.002); its height must agree (42 pixels: within 0.06) and so must the scale found. None when the bar
/// is not found or its edges do not agree. 2026-11-10, the user's samples of several interface scales.
pub fn placement(frame: &Rgba) -> Option<Placement> {
    if frame.h == 0 {
        return None;
    }
    let found: Vec<f64> = OTHER_SCALES.into_iter().flatten().map(|c| f64::from(c) / 100.0).filter(|&s| title_at(frame, s)).collect();
    let s0 = *found.get(found.len() / 2)?;
    let k = frame.h as f64 / 1440.0;
    let dark = |x: f64, y: f64| {
        let [r, g, b] = frame.rgb((x * k).round() as i32, (y * k).round() as i32);
        r.max(g).max(b) <= 18
    };
    let at = |v: f64| 720.0 + (v - 720.0) * s0;
    // each edge on three lines across the bar, the median taken (the words sit at the bar's left end, inside it)
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let rows = [at(392.0), at(405.0), at(418.0)];
    let cols = [s0 * 330.0, s0 * 440.0, s0 * 550.0];
    let left = median(rows.iter().map(|&y| (0..40).map(|i| s0 * BAR_LEFT - 20.0 + f64::from(i)).find(|&x| dark(x, y) && dark(x + 1.0, y)).unwrap_or(f64::NAN)).collect());
    let right = median(rows.iter().map(|&y| (0..40).map(|i| s0 * (BAR_LEFT + BAR_W) + 20.0 - f64::from(i)).find(|&x| dark(x - 1.0, y) && dark(x - 2.0, y)).unwrap_or(f64::NAN)).collect());
    let top = median(cols.iter().map(|&x| (0..40).map(|i| at(BAR_TOP) - 20.0 + f64::from(i)).find(|&y| dark(x, y) && dark(x, y + 1.0)).unwrap_or(f64::NAN)).collect());
    let bottom = median(cols.iter().map(|&x| (0..40).map(|i| at(BAR_TOP + BAR_H) + 20.0 - f64::from(i)).find(|&y| dark(x, y - 1.0) && dark(x, y - 2.0)).unwrap_or(f64::NAN)).collect());
    let (s_w, s_h) = ((right - left) / BAR_W, (bottom - top) / BAR_H);
    (s_w.is_finite() && s_h.is_finite() && (s_w - s_h).abs() <= 0.06 && (s_w - s0).abs() <= 0.03).then_some(Placement { s: s_w, left, top })
}

/// The reference frame (2560 x 1440) of `frame`, whose panel is drawn at placement `p`, with the panel as the measured
/// interface scale draws it: each reference pixel in `region` the area average of the frame under its footprint at
/// `p` (the rest black). The reader then reads it as it is (its rows, bullets, underlines and OCR scale were measured
/// there); the entries' rows go back to the frame through `p`.
pub fn unscaled(frame: &Rgba, p: Placement, region: Rect) -> Rgba {
    let (w, h) = (2560usize, 1440usize);
    let k = frame.h as f64 / 1440.0;
    // the frame pixels an output interval [lo, hi) (frame pixels) covers, with their shares
    let cover = |lo: f64, hi: f64, n: usize| -> Vec<(usize, f64)> {
        let len = hi - lo;
        let mut out = Vec::with_capacity(4);
        if len <= 0.0 {
            return out;
        }
        let mut q = lo.floor().max(0.0) as usize;
        while (q as f64) < hi && q < n {
            let c = (hi.min(q as f64 + 1.0) - lo.max(q as f64)).max(0.0);
            if c > 0.0 {
                out.push((q, c / len));
            }
            q += 1;
        }
        out
    };
    let (x0, x1) = (region.x.clamp(0, w as i32) as usize, (region.x + region.w).clamp(0, w as i32) as usize);
    let (y0, y1) = (region.y.clamp(0, h as i32) as usize, (region.y + region.h).clamp(0, h as i32) as usize);
    let cols: Vec<Vec<(usize, f64)>> = (x0..x1).map(|x| cover(p.x(x as f64) * k, p.x(x as f64 + 1.0) * k, frame.w)).collect();
    let mut data = vec![0u8; w * h * 4];
    for y in y0..y1 {
        let rows = cover(p.y(y as f64) * k, p.y(y as f64 + 1.0) * k, frame.h);
        for (i, col) in cols.iter().enumerate() {
            let mut sum = [0f64; 3];
            for &(sy, wy) in &rows {
                for &(sx, wx) in col {
                    let at = (sy * frame.w + sx) * 4;
                    for (c, v) in sum.iter_mut().enumerate() {
                        *v += f64::from(frame.data[at + c]) * wy * wx;
                    }
                }
            }
            let at = (y * w + x0 + i) * 4;
            for (c, v) in sum.iter().enumerate() {
                data[at + c] = v.round().clamp(0.0, 255.0) as u8;
            }
            data[at + 3] = 255;
        }
    }
    Rgba { w, h, data }
}

/// The panel's surroundings in reference pixels ([`area`] on a 2560 x 1440 frame): what [`unscaled`] makes to read.
pub const AREA_REF: Rect = AREA;

/// [`title_shows`] for the panel drawn `s` times as large about the left edge's middle (reference pixels).
fn title_at(frame: &Rgba, s: f64) -> bool {
    if frame.h == 0 {
        return false;
    }
    let k = frame.h as f64 / 1440.0;
    let bright = |x: i32, y: i32| {
        let (x, y) = (f64::from(x) * s, 720.0 + (f64::from(y) - 720.0) * s);
        let [r, g, b] = frame.rgb((x * k).round() as i32, (y * k).round() as i32);
        r.max(g).max(b)
    };
    let share = |area: Rect, step: usize, hit: &dyn Fn(u8) -> bool| {
        let (mut n, mut hits) = (0u32, 0u32);
        for y in (area.y..area.y + area.h).step_by(2) {
            for x in (area.x..area.x + area.w).step_by(step) {
                n += 1;
                hits += u32::from(hit(bright(x, y)));
            }
        }
        f64::from(hits) / f64::from(n.max(1))
    };
    let mean = |area: Rect| {
        let (mut n, mut sum) = (0u32, 0u32);
        for y in area.y..area.y + area.h {
            for x in (area.x..area.x + area.w).step_by(4) {
                n += 1;
                sum += u32::from(bright(x, y));
            }
        }
        f64::from(sum) / f64::from(n.max(1))
    };
    share(TITLE_BAR, 4, &|v| v <= 12) >= 0.9
        && mean(TITLE_ABOVE) >= 22.0
        && mean(TITLE_BELOW) >= 22.0
        && share(TITLE_WORDS, 2, &|v| v >= 150) >= 0.08
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The title bar shows: black with white words, on the panel's ground; a black screen, a bar with no words, or no
    /// bar do not. At 1440 and at 1080 rows.
    #[test]
    fn the_title_bar_tells_the_affix_panel() {
        let paint = |f: &mut Rgba, r: Rect, v: u8, step: usize| {
            for y in r.y..r.y + r.h {
                for x in (r.x..r.x + r.w).step_by(step) {
                    let i = (y as usize * f.w + x as usize) * 4;
                    f.data[i..i + 3].copy_from_slice(&[v, v, v]);
                }
            }
        };
        let panel = |words: bool| {
            let mut f = Rgba { w: 2560, h: 1440, data: vec![34; 2560 * 1440 * 4] };
            paint(&mut f, Rect::new(120, 384, 476, 42), 0, 1);
            if words {
                paint(&mut f, Rect::new(132, 394, 90, 20), 240, 4);
            }
            f
        };
        assert!(title_shows(&panel(true)));
        assert!(title_shows(&panel(true).resize(1920, 1080)), "1080p");
        assert!(!title_shows(&panel(false)), "no words");
        assert!(!title_shows(&Rgba { w: 2560, h: 1440, data: vec![0; 2560 * 1440 * 4] }), "a black screen");
        assert!(!title_shows(&Rgba { w: 2560, h: 1440, data: vec![34; 2560 * 1440 * 4] }), "no bar");
        assert!(!title_shows(&Rgba { w: 0, h: 0, data: Vec::new() }));
    }

    /// The panel drawn at another interface scale (about its own title bar): its placement measured to 0.005 and a
    /// pixel, at 1440 and 1080 rows, and turned back into the measured scale's panel by [`unscaled`]; none for a panel at
    /// the measured scale or no panel.
    #[test]
    fn the_title_bar_places_the_panel() {
        let drawn = |s: f64, left: f64, top: f64| {
            let mut f = Rgba { w: 2560, h: 1440, data: vec![34; 2560 * 1440 * 4] };
            let p = Placement { s, left, top };
            let mut paint = |r: Rect, v: u8| {
                let (x0, x1) = (p.x(f64::from(r.x)).round() as usize, p.x(f64::from(r.x + r.w)).round() as usize);
                let (y0, y1) = (p.y(f64::from(r.y)).round() as usize, p.y(f64::from(r.y + r.h)).round() as usize);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let i = (y * 2560 + x) * 4;
                        f.data[i..i + 3].copy_from_slice(&[v, v, v]);
                    }
                }
            };
            paint(Rect::new(120, 384, 477, 42), 0);
            paint(Rect::new(132, 394, 90, 20), 240);
            // a white block in the text area, to find again unscaled
            paint(Rect::new(200, 600, 40, 20), 250);
            f
        };
        for (s, left, top) in [(1.5, 177.0, 217.0), (1.258, 148.0, 297.0), (0.958, 113.0, 398.0)] {
            let f = drawn(s, left, top);
            for frame in [f.clone(), f.resize(1920, 1080)] {
                let p = placement(&frame).unwrap_or_else(|| panic!("{s} at {} rows: placed", frame.h));
                assert!((p.s - s).abs() <= 0.005 && (p.left - left).abs() <= 1.5 && (p.top - top).abs() <= 1.5, "{s} at {} rows: {p:?}", frame.h);
                let flat = unscaled(&frame, p, AREA_REF);
                assert!(title_shows(&flat), "{s}: the title bar where it is at the measured scale");
                assert!(flat.rgb(220, 610)[0] >= 200 && flat.rgb(260, 610)[0] <= 60, "{s}: the block where it was drawn");
            }
        }
        assert_eq!(placement(&drawn(1.0, 120.0, 384.0)).map(|p| (p.s * 100.0).round()), None, "the measured scale: no other placement");
        assert_eq!(placement(&Rgba { w: 2560, h: 1440, data: vec![34; 2560 * 1440 * 4] }), None, "no panel");
    }

    /// The title bar drawn at another interface scale about the left edge's middle is found at that scale, and only
    /// there: the largest setting (1.5) and the recommended one (1.26), at 1440 and 1080 rows; the measured scale and
    /// screens with no bar give none.
    #[test]
    fn the_title_bar_tells_another_interface_scale() {
        let panel = |s: f64| {
            let mut f = Rgba { w: 2560, h: 1440, data: vec![34; 2560 * 1440 * 4] };
            let at = |x: i32, y: i32| ((f64::from(x) * s).round() as i32, (720.0 + (f64::from(y) - 720.0) * s).round() as i32);
            let mut paint = |r: Rect, v: u8, step: usize| {
                let ((x0, y0), (x1, y1)) = (at(r.x, r.y), at(r.x + r.w, r.y + r.h));
                for y in y0..y1 {
                    for x in (x0..x1).step_by(step) {
                        let i = (y as usize * 2560 + x as usize) * 4;
                        f.data[i..i + 3].copy_from_slice(&[v, v, v]);
                    }
                }
            };
            paint(Rect::new(120, 384, 476, 42), 0, 1);
            // solid words: thin strokes a step apart blur under the 1080p resize at another scale
            paint(Rect::new(132, 394, 90, 20), 240, 1);
            f
        };
        for s in [1.5, 1.26] {
            let f = panel(s);
            assert!(!title_shows(&f), "{s}: not at the measured scale");
            let got = other_scale(&f).expect("found");
            assert!((got - s).abs() <= 0.03, "{s}: found at {got}");
            let got = other_scale(&f.resize(1920, 1080)).expect("found at 1080p");
            assert!((got - s).abs() <= 0.03, "{s} 1080p: found at {got}");
        }
        assert!(title_shows(&panel(1.0)));
        assert_eq!(other_scale(&Rgba { w: 2560, h: 1440, data: vec![0; 2560 * 1440 * 4] }), None, "a black screen");
        assert_eq!(other_scale(&Rgba { w: 2560, h: 1440, data: vec![34; 2560 * 1440 * 4] }), None, "no bar");
    }

    /// The crop: the panel's surroundings on 16:9 frames, scaled by the height; none on other shapes.
    #[test]
    fn area_on_16_9_frames_only() {
        assert_eq!(area(2560, 1440), Some(Rect::new(0, 200, 1100, 1100)));
        assert_eq!(area(1920, 1080), Some(Rect::new(0, 150, 825, 825)));
        assert_eq!(area(3440, 1440), None);
    }

    /// A one-line change in the panel's text area changes the print; the same frame gives the same print; a cell a
    /// few grey levels off is the same panel.
    #[test]
    fn print_sees_a_line_change() {
        let mut frame = Rgba { w: 2560, h: 1440, data: vec![20; 2560 * 1440 * 4] };
        let before = print(&frame);
        assert!(same(&before, &print(&frame)));
        let mut near = before.clone();
        near[3] += CELL_DIFF;
        assert!(same(&before, &near));
        for y in 600..620 {
            for x in 150..500 {
                frame.data[(y * 2560 + x) * 4..(y * 2560 + x) * 4 + 3].copy_from_slice(&[230, 230, 230]);
            }
        }
        assert!(!same(&before, &print(&frame)));
    }

    /// The panel is on screen when its text area is dark (its ground), not when it is bright (the map before the
    /// panel comes in); an empty print is not a panel.
    #[test]
    fn the_panel_is_dark() {
        let shade = |g: u8| print(&Rgba { w: 2560, h: 1440, data: vec![g; 2560 * 1440 * 4] });
        let on = |p: &[u8]| on_screen(p, 2560, 1440);
        assert!(on(&shade(30)) && on(&shade(46)));
        assert!(!on(&shade(125)) && !on(&shade(164)));
        assert!(!on(&[]));
        assert!((grey(&shade(40)) - 40.0).abs() < 1.0);
    }

    /// The text print stays when the dim background behind the panel changes, and changes when a line of text does.
    #[test]
    fn the_text_print_sees_words_not_the_background() {
        let mut frame = Rgba { w: 2560, h: 1440, data: vec![30; 2560 * 1440 * 4] };
        let line = |frame: &mut Rgba, y0: usize, x0: usize, x1: usize, v: u8| {
            for y in y0..y0 + 18 {
                for x in (x0..x1).step_by(3) {
                    frame.data[(y * 2560 + x) * 4..(y * 2560 + x) * 4 + 3].copy_from_slice(&[v, v, v]);
                }
            }
        };
        line(&mut frame, 600, 150, 500, 220);
        let before = text_print(&frame);
        // the background moves: dim pixels change all over
        let mut moved = frame.clone();
        for (i, p) in moved.data.chunks_mut(4).enumerate() {
            if p[0] == 30 {
                let v = 30 + (i % 50) as u8;
                p[..3].copy_from_slice(&[v, v, v]);
            }
        }
        assert!(!same(&print(&frame), &print(&moved)), "the grey print sees the background");
        assert!(same_text(&before, &text_print(&moved)), "the text print does not");
        // another line of words
        let mut other = frame.clone();
        line(&mut other, 660, 150, 420, 220);
        assert!(!same_text(&before, &text_print(&other)));
    }

    /// A frame not 16:9 (16:10, 21:9) is never kept from a read by its grey: its print is of the whole frame, bright
    /// with the map though the panel shows. A 1920x1080 frame is 16:9 and is.
    #[test]
    fn frames_not_16_9_are_not_kept_by_their_grey() {
        for (w, h) in [(2560, 1600), (3440, 1440)] {
            let bright = print(&Rgba { w, h, data: vec![150; w * h * 4] });
            assert!(grey(&bright) > DARK);
            assert!(on_screen(&bright, w, h), "{w}x{h}");
        }
        let bright = print(&Rgba { w: 1920, h: 1080, data: vec![150; 1920 * 1080 * 4] });
        assert!(!on_screen(&bright, 1920, 1080));
    }
}
