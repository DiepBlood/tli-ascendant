//! Frames of another size than the 2560x1440 reference.
//!
//! [`super::read::to_reference`] scales a frame to 2560x1440 by height. That is all the colour, icon and digit
//! reads need, but the OCR of small text (monster names, card names, titles) was tuned on 2560x1440 frames only:
//! on a 1920x1080 frame the name strip went through two bilinear upscales (1080 -> 1440 -> 3x for OCR) and OCR
//! dropped the first or last character of most names (`邪眼翼獸` read as `邪翼`). So when the frame is not 1440
//! high, the OCR input is cut from the frame itself, resampled once (to [`MAG`] times the size OCR is given at
//! 1440) and sharpened ([`Frame::text`]); the reference image itself is never sharpened (the rarity is read
//! from the colour of the same name strip).
//!
//! A frame taller than 1440 (4K) is shrunk to the reference by area averaging ([`shrink`]): the two-tap
//! bilinear resize skips source pixels when shrinking by 1.5 and the thin strokes alias.

use crate::reader::{Rect, Rgba};

/// Reference height (see [`super::read::H`]).
const H: usize = super::read::H;
const W: usize = super::read::W;

// How the OCR input is made from a frame of another size: the unsharp strength, the size relative to the
// 1440 read, and the resampling kernel. Chosen 2026-11-06 by `scaled_frames_read_like_1440`
// (tests/vorax_read.rs) over the 146 frames of trainingdata/vorax scaled with System.Drawing HighQualityBicubic
// to 1920x1080 and to 3840x2160 (a 4K stand-in), counting the monster names read the same as at 1440 (491
// tanks whose 1440 name is in the table; before this change 1080 kept about 260 and 4K about 411):
//   amount / size / kernel   1080   4K
//   0    1.0  cubic          384    472
//   1.0  1.0  cubic          466     -
//   1.5  1.0  cubic          462    468
//   2.5  1.0  cubic          447     -
//   1.5  1.0  bilinear       461     -
//   1.0  1.0  bilinear       449     -
//   1.5  1.25 cubic          450     -
//   1.0  0.6  cubic          403     -
//   1.5  0.7  cubic          475    481
//   1.0  0.75 cubic          475     -
//   1.5  0.75 cubic          476     -
//   2.0  0.75 cubic          459     -
//   1.25 0.8  cubic          479    480
//   1.5  0.8  cubic          478    487   <- chosen (best sum; card names also differed least, 1 frame at 1080)
//   0    0.8  cubic           -     415
//   1.0  0.85 cubic          471     -
// Unsharpened, the 4K stand-in reads worse than sharpened at 0.8 (415): it is itself an enlargement of 1440 and
// as soft as one, so a natively rendered 4K frame may need less; the 4K stand-in is a check, not a promise.

/// Unsharp mask strength (`v + AMOUNT * (v - mean of the 3x3 box)`, at the frame's own resolution, before the
/// resample).
const AMOUNT: f32 = 1.5;

/// The OCR input is this many times the size the 1440 read gives OCR (its 2x / 3x upscale of the reference).
pub(crate) const MAG: f32 = 0.8;

/// The OCR input size and unsharp strength for a frame at another interface scale ([`Frame::ui`]): its text is
/// drawn larger than at the original setting, so it is resampled down (by 1/s at 1440) to the size the 1440 read
/// gives OCR, unsharpened. Tried 2026-11-06 on the three games at other scales (monster names in the table,
/// recommended / largest / large font): 1.0 unsharpened 150/161, 92/96, 107/109; 1.2 151, 91, 106 (and fewer
/// cards found); the 1080 setting 0.8 with 1.5 unsharp 129, 58, 102.
const UI_MAG: f32 = 1.0;
const UI_AMOUNT: f32 = 0.0;

/// A frame of another size and where reference coordinates land in it, the same mapping as
/// [`super::read::to_reference`] (scale by height, centre horizontally).
pub(crate) struct Frame<'a> {
    img: &'a Rgba,
    /// Frame pixels per reference pixel, across and down.
    sx: f32,
    sy: f32,
    /// Reference x plus `shift` is x in the height-scaled frame (half the width cropped or padded).
    shift: f32,
    /// The game's interface scale when it is not the original setting ([`Frame::ui`]): a reference rect is then
    /// drawn scaled about its zone's anchor ([`super::ui_scale::zone_of`]).
    ui: Option<f32>,
    /// The OCR input size relative to the 1440 read, and the unsharp strength of plain text crops.
    mag: f32,
    amount: f32,
}

impl<'a> Frame<'a> {
    /// None for a frame 1440 high: the reference is the frame itself (at most cropped or padded), so the OCR
    /// reads it as before.
    pub fn new(img: &'a Rgba) -> Option<Frame<'a>> {
        if img.h == H || img.w == 0 || img.h == 0 {
            return None;
        }
        let w = scaled_width(img);
        Some(Frame { img, sx: img.w as f32 / w as f32, sy: img.h as f32 / H as f32, shift: ((w as i64 - W as i64) / 2) as f32, ui: None, mag: MAG, amount: AMOUNT })
    }

    /// A frame (of any size, 1440 high included) of a game drawn at interface scale `s` (not the original
    /// setting, see [`super::ui_scale`]): a reference rect maps to the frame through its zone, and the OCR input is
    /// cut at [`UI_MAG`] times the 1440 read's size, unsharpened ([`UI_AMOUNT`]). None for an empty frame (0 wide
    /// or high), as [`Frame::new`]: [`Frame::sample`]'s clamps need at least one pixel.
    pub fn ui(img: &'a Rgba, s: f32) -> Option<Frame<'a>> {
        if img.w == 0 || img.h == 0 {
            return None;
        }
        let w = scaled_width(img);
        Some(Frame { img, sx: img.w as f32 / w as f32, sy: img.h as f32 / H as f32, shift: ((w as i64 - W as i64) / 2) as f32, ui: Some(s), mag: UI_MAG, amount: UI_AMOUNT })
    }

    /// Drawn at another interface scale ([`Frame::ui`]).
    pub fn is_ui(&self) -> bool {
        self.ui.is_some()
    }

    /// The OCR input size relative to the 1440 read ([`MAG`], or [`UI_MAG`]).
    pub fn mag(&self) -> f32 {
        self.mag
    }

    /// The frame is lower than the reference (enlarged to it).
    pub fn enlarges(&self) -> bool {
        self.img.h < H
    }

    /// The text in reference rect `r` as OCR input: the frame under `r` resampled once to `r` times `m`
    /// (Catmull-Rom), after an unsharp mask at the frame's resolution when `sharpen`.
    pub fn text(&self, r: Rect, m: f32, sharpen: bool) -> Rgba {
        self.sample(r, m, if sharpen { self.amount } else { 0.0 })
    }

    /// The digits in reference rect `r` for the glyph templates: the frame under `r` resampled once at the
    /// reference size (the glyph cutting counts reference pixels), not sharpened. Tried 2026-11-06 on the 1080p
    /// game of 03:05 (frames 0001-0026) and the 146 frames scaled to 1080: an unsharp mask misread glyphs the
    /// templates read right unsharpened (at 0.5 a total `2360` read as `7360`; at 1.0 and 1.5 most totals got
    /// `?` and round-1 tanks became unknown); 0.25 read the same as none on the 146 frames.
    pub fn digits(&self, r: Rect) -> Rgba {
        self.sample(r, 1.0, 0.0)
    }

    /// The frame under reference rect `r` resampled once to `r` times `m` (Catmull-Rom), after an unsharp mask
    /// of strength `amount` at the frame's resolution (none when 0).
    fn sample(&self, r: Rect, m: f32, amount: f32) -> Rgba {
        let (ow, oh) = (((r.w as f32 * m).round() as usize).max(1), ((r.h as f32 * m).round() as usize).max(1));
        // At another interface scale, the rect is drawn scaled about its zone's anchor.
        let zone = self.ui.map(|s| (s, super::ui_scale::zone_of(r)));
        let (ax, ay, k) = match zone.and_then(|(s, z)| z.anchor().map(|a| (a, s))) {
            Some(((ax, ay), s)) => (ax, ay, s),
            None => (0.0, 0.0, 1.0),
        };
        let ux = |x: f32| if k == 1.0 { x } else { super::ui_scale::scaled(ax, k, x) };
        let uy = |y: f32| if k == 1.0 { y } else { super::ui_scale::scaled(ay, k, y) };
        // Frame coordinates (pixel centres at +0.5) of each output pixel's centre.
        let fx = |u: usize| (ux(r.x as f32 + (u as f32 + 0.5) * r.w as f32 / ow as f32) + self.shift) * self.sx - 0.5;
        let fy = |v: usize| uy(r.y as f32 + (v as f32 + 0.5) * r.h as f32 / oh as f32) * self.sy - 0.5;
        // The frame window the taps reach, with a margin for the kernel and the 3x3 box.
        let (fw, fh) = (self.img.w as i64, self.img.h as i64);
        let x0 = (fx(0).floor() as i64 - 3).clamp(0, fw - 1);
        let x1 = (fx(ow - 1).ceil() as i64 + 4).clamp(x0 + 1, fw);
        let y0 = (fy(0).floor() as i64 - 3).clamp(0, fh - 1);
        let y1 = (fy(oh - 1).ceil() as i64 + 4).clamp(y0 + 1, fh);
        let mut win = self.img.crop(Rect::new(x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32));
        // A tank label's pixels outside its box are painted over, as in the unscaled reference.
        if let Some((s, super::ui_scale::Zone::Label(i))) = zone {
            for wy in 0..win.h {
                for wx in 0..win.w {
                    let x = (x0 as f32 + wx as f32 + 0.5) / self.sx - self.shift;
                    let y = (y0 as f32 + wy as f32 + 0.5) / self.sy;
                    if super::ui_scale::label_hidden(i, s, x, y) {
                        let o = (wy * win.w + wx) * 4;
                        win.data[o..o + 3].copy_from_slice(&super::ui_scale::HIDDEN);
                    }
                }
            }
        }
        if amount > 0.0 {
            win = unsharp(&win, amount);
        }
        let taps_x: Vec<Taps> = (0..ow).map(|u| taps(fx(u) - x0 as f32, win.w)).collect();
        let taps_y: Vec<Taps> = (0..oh).map(|v| taps(fy(v) - y0 as f32, win.h)).collect();
        let mut data = vec![0u8; ow * oh * 4];
        let mut row = vec![0f32; win.w * 4];
        for (v, ty) in taps_y.iter().enumerate() {
            // The vertical pass for this output row, then the horizontal one.
            row.iter_mut().for_each(|s| *s = 0.0);
            for &(sy, wy) in &ty[..] {
                let src = &win.data[sy * win.w * 4..(sy + 1) * win.w * 4];
                for (acc, &p) in row.iter_mut().zip(src) {
                    *acc += wy * f32::from(p);
                }
            }
            for (u, tx) in taps_x.iter().enumerate() {
                let mut px = [0f32; 4];
                for &(sx, wx) in &tx[..] {
                    for c in 0..4 {
                        px[c] += wx * row[sx * 4 + c];
                    }
                }
                let o = (v * ow + u) * 4;
                for c in 0..4 {
                    data[o + c] = px[c].round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        Rgba { w: ow, h: oh, data }
    }
}

/// The four (index, weight) taps of one output coordinate; the weights sum to 1.
type Taps = [(usize, f32); 4];

/// The Catmull-Rom taps at source coordinate `f` (pixel index space) along a line of `n` pixels, clamped to
/// the line.
fn taps(f: f32, n: usize) -> Taps {
    let last = n as i64 - 1;
    let i = f.floor() as i64;
    let t = f - i as f32;
    let at = |k: i64| (k.clamp(0, last)) as usize;
    // a = -0.5.
    let w = |d: f32| {
        let d = d.abs();
        if d < 1.0 {
            1.5 * d * d * d - 2.5 * d * d + 1.0
        } else if d < 2.0 {
            -0.5 * d * d * d + 2.5 * d * d - 4.0 * d + 2.0
        } else {
            0.0
        }
    };
    [(at(i - 1), w(1.0 + t)), (at(i), w(t)), (at(i + 1), w(1.0 - t)), (at(i + 2), w(2.0 - t))]
}

/// Unsharp mask: each colour channel `v + amount * (v - mean of its 3x3 neighbourhood)`, the edges clamped.
fn unsharp(img: &Rgba, amount: f32) -> Rgba {
    let (w, h) = (img.w as i64, img.h as i64);
    let mut data = img.data.clone();
    for y in 0..h {
        for x in 0..w {
            let o = ((y * w + x) * 4) as usize;
            for c in 0..3 {
                let mut sum = 0u32;
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let (sx, sy) = ((x + dx).clamp(0, w - 1), (y + dy).clamp(0, h - 1));
                        sum += u32::from(img.data[((sy * w + sx) * 4) as usize + c]);
                    }
                }
                let v = f32::from(img.data[o + c]);
                data[o + c] = (v + amount * (v - sum as f32 / 9.0)).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Rgba { w: img.w, h: img.h, data }
}

/// The width of a frame scaled to 1440 high, rounded (as [`super::read::to_reference`] scales it).
pub(crate) fn scaled_width(img: &Rgba) -> usize {
    ((img.w as f32 * H as f32 / img.h as f32).round() as usize).max(1)
}

/// Area-average resize to `w` x `h` (for shrinking: every source pixel counts by the share of it each output
/// pixel covers).
pub fn shrink(img: &Rgba, w: usize, h: usize) -> Rgba {
    if (w, h) == (img.w, img.h) || img.w == 0 || img.h == 0 || w == 0 || h == 0 {
        return img.resize(w, h);
    }
    let cols = spans(img.w, w);
    let rows = spans(img.h, h);
    let mut data = vec![0u8; w * h * 4];
    let src_row = img.w * 4;
    let paint = |first: usize, out: &mut [u8]| {
        let mut acc = vec![0f32; src_row];
        for (k, line) in out.chunks_exact_mut(w * 4).enumerate() {
            acc.iter_mut().for_each(|a| *a = 0.0);
            for &(sy, wy) in &rows[first + k] {
                for (a, &p) in acc.iter_mut().zip(&img.data[sy * src_row..(sy + 1) * src_row]) {
                    *a += wy * f32::from(p);
                }
            }
            for (px, span) in line.as_chunks_mut::<4>().0.iter_mut().zip(&cols) {
                let mut s = [0f32; 4];
                for &(sx, wx) in span {
                    for c in 0..4 {
                        s[c] += wx * acc[sx * 4 + c];
                    }
                }
                for c in 0..4 {
                    px[c] = s[c].round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    };
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(16);
    let band = h.div_ceil(threads);
    std::thread::scope(|s| {
        for (i, out) in data.chunks_mut(band * w * 4).enumerate() {
            let paint = &paint;
            s.spawn(move || paint(i * band, out));
        }
    });
    Rgba { w, h, data }
}

/// For each of `n` output pixels along a line of `from` source pixels: the source pixels it covers and the
/// share of each (summing to 1). When enlarging, the nearest source pixel.
fn spans(from: usize, n: usize) -> Vec<Vec<(usize, f32)>> {
    let step = from as f64 / n as f64;
    (0..n)
        .map(|i| {
            let (a, b) = (i as f64 * step, ((i + 1) as f64 * step).min(from as f64));
            let mut out = Vec::new();
            let mut s = a.floor() as usize;
            while (s as f64) < b && s < from {
                let cover = (b.min(s as f64 + 1.0) - a.max(s as f64)).max(0.0);
                if cover > 0.0 {
                    out.push((s, (cover / (b - a)) as f32));
                }
                s += 1;
            }
            if out.is_empty() {
                out.push((a.floor().min(from as f64 - 1.0) as usize, 1.0));
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: usize, h: usize) -> Rgba {
        Rgba { w, h, data: (0..w * h).flat_map(|i| [((i % w) * 255 / w) as u8, ((i / w) * 255 / h) as u8, 7, 255]).collect() }
    }

    /// Area averaging keeps a flat colour flat and the mean of the image.
    #[test]
    fn shrink_averages() {
        let img = Rgba { w: 6, h: 3, data: [[10u8, 20, 30, 255], [50, 60, 70, 255]].iter().cycle().take(18).flatten().copied().collect() };
        let s = shrink(&img, 4, 2);
        assert_eq!((s.w, s.h), (4, 2));
        // Every output pixel covers 1.5 source columns: (10*1 + 50*0.5) / 1.5 and so on.
        assert_eq!(&s.data[..4], &[23, 33, 43, 255]);
        let flat = Rgba { w: 9, h: 9, data: [9u8, 99, 199, 255].repeat(81) };
        assert!(shrink(&flat, 6, 6).data.as_chunks::<4>().0.iter().all(|p| *p == [9, 99, 199, 255]));
    }

    /// The frame crop lands where to_reference puts the same pixels: a 1920x1080 gradient read through
    /// `Frame::text` matches the reference crop within a level or two.
    #[test]
    fn frame_text_matches_the_reference() {
        let img = gradient(1920, 1080);
        let reference = super::super::read::to_reference(&img);
        let f = Frame::new(&img).unwrap();
        let r = Rect::new(414, 503, 220, 32);
        let t = f.text(r, 1.0, false);
        let c = reference.crop(r);
        assert_eq!((t.w, t.h), (c.w, c.h));
        let worst = t.data.iter().zip(&c.data).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
        assert!(worst <= 2, "{worst}");
        // A frame 1440 high reads the reference as before.
        assert!(Frame::new(&gradient(3440, 1440)).is_none());
    }
}
