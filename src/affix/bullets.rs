//! Finds the diamond bullets of the map page's affix panel: one bullet left of every affix entry.
//!
//! Measured on 2560x1440 screenshots at the user's UI scale (128 map pages, every count right). Other resolutions
//! are scaled by the frame height; no sample of them exists yet.

/// Reference frame height the geometry below was measured on.
pub const REF_HEIGHT: f64 = 1440.0;

/// Column scanned for bullets (reference pixels, inclusive): the bullets sit at x 124..142, the text starts
/// near x 148.
const COLUMN: (f64, f64) = (124.0, 142.0);
/// Rows scanned: from the first possible entry to the bottom of the panel.
const ROWS: (f64, f64) = (495.0, 990.0);
/// Height of a bullet in reference pixels (a run outside this range is text, a frame line or noise).
const HEIGHT: (f64, f64) = (4.0, 20.0);

/// Colour of a bullet, as the game draws it (meaning not known for sure; not used for grading).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulletColour {
    White,
    Orange,
    Red,
}

/// One bullet in frame pixels: rows `top..bottom`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bullet {
    pub top: u32,
    pub bottom: u32,
    pub colour: BulletColour,
}

impl Bullet {
    /// Middle row: the bullet sits in the middle of its entry's first text line.
    pub fn centre(&self) -> f64 {
        (self.top + self.bottom) as f64 / 2.0
    }
}

/// A bullet pixel: bright (white bullet) or a strongly saturated warm colour (red or orange bullet).
fn is_bullet_pixel(rgb: [u8; 3]) -> bool {
    pixel_margin(rgb) > 0
}

/// How far a pixel is past the bullet test (positive: a bullet pixel; the larger, the safer), for checking how close
/// screenshots come to the thresholds: the better of `r + g + b - 330` and the smaller of `r - 150` and `r - b - 80`.
pub fn pixel_margin([r, g, b]: [u8; 3]) -> i32 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    (r + g + b - 330).max((r - 150).min(r - b - 80))
}

fn colour_of(r: f64, g: f64, b: f64) -> BulletColour {
    // means of the bullet pixels: white about (126,127,128), orange (160,133,81), red (165,73,73);
    // the same rule as the lab's eval_ocr.py
    if r - g > 60.0 {
        BulletColour::Red
    } else if r - b > 60.0 && g - b > 40.0 {
        BulletColour::Orange
    } else {
        BulletColour::White
    }
}

/// Scans the bullet column of a frame `height` pixels tall. `pixel(x, y)` returns the RGB of a frame pixel.
pub fn find(height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<Bullet> {
    let s = height as f64 / REF_HEIGHT;
    let px = |v: f64| (v * s).round() as u32;
    let (x0, x1) = (px(COLUMN.0), px(COLUMN.1));
    let (y0, y1) = (px(ROWS.0), px(ROWS.1).min(height.saturating_sub(1)));
    let (hmin, hmax) = ((HEIGHT.0 * s).floor() as u32, (HEIGHT.1 * s).ceil() as u32);
    let mut out = Vec::new();
    let mut start: Option<u32> = None;
    let (mut sr, mut sg, mut sb, mut n) = (0u64, 0u64, 0u64, 0u64);
    for y in y0..=y1 {
        let mut hit = false;
        for x in x0..=x1 {
            let c = pixel(x, y);
            if is_bullet_pixel(c) {
                hit = true;
                sr += c[0] as u64;
                sg += c[1] as u64;
                sb += c[2] as u64;
                n += 1;
            }
        }
        if hit && start.is_none() {
            start = Some(y);
        }
        if let Some(top) = start
            && (!hit || y == y1)
        {
            let h = y - top;
            if (hmin..=hmax).contains(&h) && n > 0 {
                let (r, g, b) = (sr as f64 / n as f64, sg as f64 / n as f64, sb as f64 / n as f64);
                out.push(Bullet { top, bottom: y, colour: colour_of(r, g, b) });
            }
            start = None;
            (sr, sg, sb, n) = (0, 0, 0, 0);
        }
    }
    out
}
