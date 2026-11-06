//! The screenshot reader for the S16 prison screen (docs/s16-screen.md).
//! The board scene comes from pixels (`scene`), the text panels from the
//! Windows OCR engine (`ocr`, `panels`), and `model` joins them into a board
//! model.

pub mod click;
pub mod instruments;
pub mod layout;
pub mod model;
pub mod ocr;
pub mod panels;
pub mod scene;
pub mod theme;

use std::path::Path;

/// Width and height of the reference frame every coordinate refers to.
pub const REF_W: usize = 1918;
pub const REF_H: usize = 1078;

/// A pixel rectangle: left, top, width, height.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn centre(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// An 8-bit RGBA image, rows top to bottom.
#[derive(Clone, Debug)]
pub struct Rgba {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl Rgba {
    /// The RGB of pixel (x, y), clamped to the image.
    pub fn rgb(&self, x: i32, y: i32) -> [u8; 3] {
        let x = x.clamp(0, self.w as i32 - 1) as usize;
        let y = y.clamp(0, self.h as i32 - 1) as usize;
        let i = (y * self.w + x) * 4;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }

    /// The part of the image inside `r`, clipped to the image.
    pub fn crop(&self, r: Rect) -> Rgba {
        let x0 = r.x.clamp(0, self.w as i32) as usize;
        let y0 = r.y.clamp(0, self.h as i32) as usize;
        let x1 = (r.x + r.w).clamp(x0 as i32, self.w as i32) as usize;
        let y1 = (r.y + r.h).clamp(y0 as i32, self.h as i32) as usize;
        let mut data = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
        for y in y0..y1 {
            data.extend_from_slice(&self.data[(y * self.w + x0) * 4..(y * self.w + x1) * 4]);
        }
        Rgba { w: x1 - x0, h: y1 - y0, data }
    }

    /// Bilinear resize to `w` x `h`.
    pub fn resize(&self, w: usize, h: usize) -> Rgba {
        if w == self.w && h == self.h {
            return self.clone();
        }
        let mut data = vec![0u8; w * h * 4];
        let sx = self.w as f32 / w as f32;
        let sy = self.h as f32 / h as f32;
        // Each output column's two source byte offsets in a row and its weight, worked out once.
        let cols: Vec<(usize, usize, f32)> = (0..w)
            .map(|x| {
                let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, (self.w - 1) as f32);
                let x0 = fx as usize;
                (x0 * 4, (x0 + 1).min(self.w - 1) * 4, fx - x0 as f32)
            })
            .collect();
        let row_len = self.w * 4;
        let paint_rows = |first: usize, rows: &mut [u8]| {
            for (k, out) in rows.chunks_exact_mut(w * 4).enumerate() {
                let fy = (((first + k) as f32 + 0.5) * sy - 0.5).clamp(0.0, (self.h - 1) as f32);
                let y0 = fy as usize;
                let y1 = (y0 + 1).min(self.h - 1);
                let ty = fy - y0 as f32;
                let (top_row, bot_row) = (&self.data[y0 * row_len..][..row_len], &self.data[y1 * row_len..][..row_len]);
                for (px, &(x0, x1, tx)) in out.as_chunks_mut::<4>().0.iter_mut().zip(&cols) {
                    for c in 0..4 {
                        let top = top_row[x0 + c] as f32 * (1.0 - tx) + top_row[x1 + c] as f32 * tx;
                        let bot = bot_row[x0 + c] as f32 * (1.0 - tx) + bot_row[x1 + c] as f32 * tx;
                        px[c] = (top * (1.0 - ty) + bot * ty).round() as u8;
                    }
                }
            }
        };
        // A game frame (about 2 million output pixels) in bands of rows on
        // several threads: the same pixels as one pass, in about a tenth of
        // its 100 ms. Small images stay on this thread.
        let threads = if w * h >= 1 << 18 { std::thread::available_parallelism().map_or(1, |n| n.get()).min(16) } else { 1 };
        if threads <= 1 {
            paint_rows(0, &mut data);
        } else {
            let band = h.div_ceil(threads);
            std::thread::scope(|s| {
                for (i, rows) in data.chunks_mut(band * w * 4).enumerate() {
                    let paint_rows = &paint_rows;
                    s.spawn(move || paint_rows(i * band, rows));
                }
            });
        }
        Rgba { w, h, data }
    }
}

/// The fingerprint grid: 64 x 36 blocks, 30 x 30 pixels each on a 1920 x 1080 frame.
pub const PRINT: (usize, usize) = (64, 36);

/// A frame's mean gray per block of a 64 x 36 grid: small enough to compare
/// every second, and a block average sees a change to a digit that a
/// resize, which samples a few pixels per block, can miss.
pub fn fingerprint(frame: &Rgba) -> Vec<u8> {
    let (gw, gh) = PRINT;
    if frame.w == 0 || frame.h == 0 {
        return vec![0; gw * gh];
    }
    // Each column's block, and the columns per block, worked out once: a
    // sample of the game runs this over every pixel 10 times a second.
    let cols: Vec<usize> = (0..frame.w).map(|x| x * gw / frame.w).collect();
    let mut per_block = vec![0u64; gw];
    cols.iter().for_each(|&c| per_block[c] += 1);
    let (mut sum, mut count) = (vec![0u64; gw * gh], vec![0u64; gw * gh]);
    let mut row_sum = vec![0u64; gw];
    for (y, row) in frame.data.chunks_exact(frame.w * 4).enumerate() {
        row_sum.fill(0);
        for (p, &c) in row.as_chunks::<4>().0.iter().zip(&cols) {
            row_sum[c] += p[0] as u64 * 299 + p[1] as u64 * 587 + p[2] as u64 * 114;
        }
        let base = y * gh / frame.h * gw;
        for b in 0..gw {
            sum[base + b] += row_sum[b];
            count[base + b] += per_block[b];
        }
    }
    sum.iter().zip(&count).map(|(&s, &n)| s.checked_div(n * 1000).unwrap_or(0) as u8).collect()
}

/// The print of a zh-TW Warden's Treasure pick and, per block, whether the
/// block keeps it: the median of 22 treasure pick frames of four 2026-10-28
/// runs, and the 1,978 blocks their prints stay within 4 grey levels of it
/// in three quarters of them (the sparkles and the board under the pick
/// move the rest).
static TREASURE_PRINT: &[u8; 2 * PRINT.0 * PRINT.1] = include_bytes!("../../assets/s16-treasure-print.bin");

/// The most a print's kept blocks may differ from [`TREASURE_PRINT`], on
/// average in grey levels, to show a treasure pick: those frames 0.3 to 2.2,
/// every other screen of those runs 15 or more (an incident pop-up the nearest).
const TREASURE_PRINT_MAX: f32 = 6.0;

/// Whether `print` (see [`fingerprint`]) shows a Warden's Treasure pick with
/// its cards in place: a treasure's four gifts never change, so the pick is
/// known before its sparkles settle or its title reads.
pub fn treasure_shown(print: &[u8]) -> bool {
    let n = PRINT.0 * PRINT.1;
    if print.len() != n {
        return false;
    }
    let (tpl, keep) = TREASURE_PRINT.split_at(n);
    let (sum, count) = (0..n).filter(|&b| keep[b] != 0).fold((0u32, 0u32), |(s, c), b| (s + u32::from(print[b].abs_diff(tpl[b])), c + 1));
    count > 0 && sum as f32 / count as f32 <= TREASURE_PRINT_MAX
}

/// Decodes a PNG into RGBA at its own size.
pub(crate) fn decode_png_raw(bytes: &[u8]) -> Result<Rgba, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let size = reader.output_buffer_size().ok_or("PNG too large")?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width as usize, info.height as usize);
    let src = &buf[..info.buffer_size()];
    let data = match info.color_type {
        png::ColorType::Rgba => src.to_vec(),
        png::ColorType::Rgb => src.as_chunks::<3>().0.iter().flat_map(|&[r, g, b]| [r, g, b, 255]).collect(),
        png::ColorType::GrayscaleAlpha => src.as_chunks::<2>().0.iter().flat_map(|&[g, a]| [g, g, g, a]).collect(),
        png::ColorType::Grayscale => src.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("indexed PNG left unexpanded".into()),
    };
    Ok(Rgba { w, h, data })
}

/// Decodes a PNG or, by its SOI marker, a JPEG (the frame-*.jpg a run folder
/// keeps) into RGBA at its own size. A PNG decodes as [`decode_png_raw`].
pub fn decode_image(bytes: &[u8]) -> Result<Rgba, String> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        crate::app::report::decode_jpeg(bytes)
    } else {
        decode_png_raw(bytes)
    }
}

/// Loads a PNG or JPEG file at its own size; see [`decode_image`].
pub fn load_image_raw(path: &Path) -> Result<Rgba, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    decode_image(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Loads a PNG (or JPEG) file in the board layout; see [`load_layouts`].
pub fn load_png(path: &Path) -> Result<Rgba, String> {
    load_layouts(path).map(|(board, _)| board)
}

/// Loads a PNG or JPEG file at the reference size, in the board layout and,
/// when it differs, the title layout; see [`layout::lay_out_scaled`].
fn load_layouts(path: &Path) -> Result<(Rgba, Option<Rgba>), String> {
    let img = load_image_raw(path)?;
    let (board, title, _) = layout::lay_out_scaled(&img, None);
    Ok((board, title))
}

/// Writes an image as an 8-bit RGBA PNG file.
pub fn save_png(path: &Path, img: &Rgba) -> Result<(), String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let file = std::fs::File::create(path).map_err(|e| at(&e))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), img.w as u32, img.h as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| at(&e))?;
    writer.write_image_data(&img.data).map_err(|e| at(&e))?;
    writer.finish().map_err(|e| at(&e))
}

/// Reads a screenshot into a board model and the warnings to check;
/// `held_tools` are the instruments the player holds, or when empty those the
/// instrument panel's icons name.
pub fn read_model(path: &Path, held_tools: &[crate::engine::data::Tool]) -> Result<(crate::board::Model, Vec<model::Warning>), String> {
    let (img, title) = load_layouts(path)?;
    let panels = panels::read_panels_titled(&img, title.as_ref().unwrap_or(&img));
    let held = if held_tools.is_empty() { panels.held.clone().unwrap_or_default() } else { held_tools.to_vec() };
    // Only Iron Law runs past 20 days.
    let run_days = if panels.days.is_some_and(|left| i64::from(left) > model::RUN_DAYS) { model::iron_law_days() } else { model::RUN_DAYS };
    Ok(model::to_model(&scene::read_scene(&img), &panels, &held, None, run_days))
}

/// The training frames folder `trainingdata/<rel>`, None (and a note) when
/// it is missing.
#[cfg(test)]
fn training(rel: &str) -> Option<std::path::PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("trainingdata").join(rel);
    if !dir.is_dir() {
        println!("skipped: {} is missing", dir.display());
        return None;
    }
    Some(dir)
}

#[cfg(test)]
mod print_tests {
    use super::*;

    /// The template's own print shows a treasure pick, a flat grey one doesn't.
    #[test]
    fn treasure_print_matches_itself_only() {
        let n = PRINT.0 * PRINT.1;
        assert!(treasure_shown(&TREASURE_PRINT[..n]));
        assert!(!treasure_shown(&vec![40; n]));
        assert!(!treasure_shown(&TREASURE_PRINT[..n - 1]));
    }

    /// A JPEG is told by its header, not its name, and a PNG decodes as before.
    #[test]
    fn decode_image_takes_png_and_jpeg() {
        let img = Rgba { w: 16, h: 8, data: (0..16 * 8).flat_map(|i| [(i * 2) as u8, 90, 160, 255]).collect() };
        let dir = std::env::temp_dir().join(format!("ascendant-decode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("frame.png");
        save_png(&png, &img).unwrap();
        let bytes = std::fs::read(&png).unwrap();
        assert_eq!(decode_image(&bytes).unwrap().data, decode_png_raw(&bytes).unwrap().data);
        assert_eq!(decode_image(&bytes).unwrap().data, img.data);
        let jpeg = crate::app::report::encode_jpeg(&img, 95).unwrap();
        let back = decode_image(&jpeg).unwrap();
        assert_eq!((back.w, back.h), (16, 8));
        // A JPEG named .png still loads.
        std::fs::write(&png, &jpeg).unwrap();
        assert_eq!(load_image_raw(&png).unwrap().data, back.data);
        assert!(decode_image(b"not an image").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
