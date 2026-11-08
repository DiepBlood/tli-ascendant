//! Text from a region of the frame through the OCR engine built into Windows
//! (Windows.Media.Ocr). The engine reads in the first installed OCR language
//! of the game's language (see [`crate::lang`]: English, or Traditional
//! Chinese), else in the user's profile languages (one of another script,
//! such as Russian, misreads the game).

use super::{Rect, Rgba};
use crate::lang::{self, Lang, tr, trf};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// The language tag the engine reads in ("en-US"); None when Windows has
/// no OCR language installed.
pub fn language() -> Option<String> {
    imp::language()
}

/// The game language an OCR language tag reads: "en", "en-US"... English; "zh-Hant-TW", "zh-TW", "zh-Hant", "zh-HK"
/// Traditional Chinese; None for any other tag.
pub fn game_lang(tag: &str) -> Option<Lang> {
    let tag = tag.to_ascii_lowercase();
    if tag.starts_with("en") {
        Some(Lang::En)
    } else if ["zh-hant", "zh-tw", "zh-hk"].iter().any(|p| tag.starts_with(p)) {
        Some(Lang::ZhTw)
    } else {
        None
    }
}

/// The game client language the engine reads now: its tag's [`game_lang`]. It follows the window language, except
/// that with no OCR language of it installed Windows falls back to a profile language, and that one is what is
/// read. None with no OCR at all or a tag of another language.
pub fn client_language() -> Option<Lang> {
    language().as_deref().and_then(game_lang)
}

/// Lines of a read image with their boxes (x, y, w, h) in its pixels.
type Lines = Vec<(String, [f32; 4])>;

/// Where a region is read: its rect, scale and crop size, and the language.
type Slot = (i32, i32, i32, i32, u32, usize, usize, u8);

/// A crop shrunk to the mean colour of each [`BLOCK`] x [`BLOCK`] block:
/// what the cache compares.
struct Thumb {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

/// The side of a thumb's block in crop pixels.
const BLOCK: usize = 4;

/// The most a thumb block's colour channel may differ from a cached one's
/// for the crop to read the same: the background's flicker kept 91% of crops
/// from matching pixel for pixel (2026-10-28 runs). Each channel counts, as
/// the red Consume target banner's text barely differs from its red ground
/// in gray.
const ALIKE_DIFF: u8 = 24;

/// Crops kept per slot, the last read first.
const ENTRIES: usize = 8;

/// `crop`'s thumb after the contrast stretch the engine reads it with: under
/// a dialog's dark veil the days counter's "19" and "20" differ by less than
/// [`ALIKE_DIFF`], and so does a target banner fading in from none.
fn thumb(crop: &Rgba) -> Thumb {
    let crop = &stretch(crop);
    let (w, h) = (crop.w.div_ceil(BLOCK), crop.h.div_ceil(BLOCK));
    let mut sum = vec![0u32; w * h * 3];
    let mut count = vec![0u32; w * h];
    for (y, row) in crop.data.chunks_exact(crop.w * 4).enumerate() {
        for (x, p) in row.as_chunks::<4>().0.iter().enumerate() {
            let k = (y / BLOCK) * w + x / BLOCK;
            for c in 0..3 {
                sum[k * 3 + c] += u32::from(p[c]);
            }
            count[k] += 1;
        }
    }
    Thumb { w, h, rgb: sum.iter().enumerate().map(|(i, &s)| (s / count[i / 3].max(1)) as u8).collect() }
}

fn alike(a: &Thumb, b: &Thumb) -> bool {
    (a.w, a.h) == (b.w, b.h) && a.rgb.iter().zip(&b.rgb).all(|(x, y)| x.abs_diff(*y) <= ALIKE_DIFF)
}

/// The lines of regions read before, by slot, with the thumbs they were read
/// from: the engine reads one region at a time whatever the threads (14
/// regions took 130 ms at once, 180 ms one by one), and most regions show the
/// same text from one step of a run to the next. A crop like a cached one
/// gives its lines back, skipping its upscale and contrast stretch too.
fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

type Cache = HashMap<Slot, Vec<(Thumb, Lines)>>;

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Empties the cache: the next reads read every region (for checks of the cache).
pub fn clear_cache() {
    cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Regions read so far that a read before gave back (their text unchanged),
/// and regions the engine read.
pub fn cache_counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// Why the game can't be read: Windows has no OCR language for the game's
/// language. None when it has one.
pub fn problem() -> Option<String> {
    problem_with(language())
}

/// [`problem`] for an engine reading in `language` (None: no OCR language
/// installed); the preview uses it to show the missing-OCR line without
/// touching the system's OCR.
pub fn problem_with(language: Option<String>) -> Option<String> {
    // One note line at 296 wide (each note is one line): the install button sits under it, and the
    // manual command shows in its hover and after a failed install.
    match language {
        Some(tag) if fits(&tag, lang::lang()) => None,
        Some(tag) => Some(trf!("OCR reads only {}, which misreads the game: install below.", tag)),
        None => Some(tr("No OCR for the game's language: install it below.").to_string()),
    }
}

/// Whether an OCR language tag reads the game in `lang`: "en", "en-US",
/// "en-GB"... for English, "zh-Hant-TW", "zh-TW", "zh-Hant"... for
/// Traditional Chinese.
fn fits(tag: &str, lang: Lang) -> bool {
    let tag = tag.to_ascii_lowercase();
    match lang {
        Lang::En => tag.starts_with("en"),
        Lang::ZhTw => tag.starts_with("zh-hant") || tag.starts_with("zh-tw") || tag.starts_with("zh-hk"),
    }
}

/// One line of text and its box in frame pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub text: String,
    pub rect: Rect,
}

/// The lines of text inside `rect`, read from the crop upscaled by `scale`
/// (small game text reads better at 2-3x). Empty when the region is empty
/// or OCR is not available.
pub fn ocr(img: &Rgba, rect: Rect, scale: f32) -> Vec<Line> {
    let [lines] = ocr_all([img], &[(rect, scale)]);
    lines
}

/// The lines of each `(rect, scale)` region as `ocr` reads them in the image
/// of the same index, in order, with the regions read at the same time.
pub fn ocr_all<const N: usize>(imgs: [&Rgba; N], regions: &[(Rect, f32); N]) -> [Vec<Line>; N] {
    let crops: Vec<Rgba> = imgs.iter().zip(regions).map(|(img, &(rect, _))| img.crop(rect)).collect();
    let slots: Vec<Option<Slot>> = crops
        .iter()
        .zip(regions)
        .map(|(c, &(r, scale))| (c.w > 0 && c.h > 0).then_some((r.x, r.y, r.w, r.h, scale.to_bits(), c.w, c.h, lang::lang() as u8)))
        .collect();
    let mut thumbs: Vec<Option<Thumb>> = crops.iter().zip(&slots).map(|(c, s)| s.map(|_| thumb(c))).collect();
    let mut lines: Vec<Option<Lines>> = {
        let cached = cache().lock().unwrap_or_else(|e| e.into_inner());
        slots
            .iter()
            .zip(&thumbs)
            .map(|(s, t)| {
                let (s, t) = (s.as_ref()?, t.as_ref()?);
                cached.get(s)?.iter().find(|(known, _)| alike(known, t)).map(|(_, l)| l.clone())
            })
            .collect()
    };
    // Each region's upscale and contrast stretch took 5 ms or more, 67 ms in
    // all on a board frame, one after another: they run side by side.
    let inputs: Vec<Option<Rgba>> = std::thread::scope(|scope| {
        let jobs: Vec<_> = crops
            .iter()
            .zip(regions)
            .zip(&slots)
            .zip(&lines)
            .map(|(((crop, &(_, scale)), s), known)| (s.is_some() && known.is_none()).then(|| scope.spawn(move || prepare(crop, scale))))
            .collect();
        jobs.into_iter().map(|job| job.and_then(|j| j.join().ok().flatten())).collect()
    });
    let asked = inputs.iter().filter(|i| i.is_some()).count() as u64;
    HITS.fetch_add(slots.iter().flatten().count() as u64 - asked, Ordering::Relaxed);
    MISSES.fetch_add(asked, Ordering::Relaxed);
    let read = imp::recognize_all(inputs);
    {
        let mut cached = cache().lock().unwrap_or_else(|e| e.into_inner());
        for (((slot, fresh), s), t) in lines.iter_mut().zip(read).zip(&slots).zip(thumbs.iter_mut()) {
            // A failed read is not cached.
            if let (Some(fresh), Some(s), Some(t)) = (fresh, s, t.take()) {
                let kept = cached.entry(*s).or_default();
                kept.insert(0, (t, fresh.clone()));
                kept.truncate(ENTRIES);
                *slot = Some(fresh);
            }
        }
    }
    let mut read = lines.into_iter();
    regions.map(|(rect, scale)| {
        read.next()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .map(|(text, [x, y, lw, lh])| Line {
                text,
                rect: Rect::new(
                    rect.x + (x / scale).round() as i32,
                    rect.y + (y / scale).round() as i32,
                    (lw / scale).round() as i32,
                    (lh / scale).round() as i32,
                ),
            })
            .collect()
    })
}

/// The lines of each region read from its crop repeated `n` times side by
/// side, all regions of `img` at once: Windows OCR drops a lone CJK
/// character (`吃`, `換`) and the zh-TW counter's thin "1"s, but reads them
/// as a word of repeats (`吃`, `吃`; "1 9 1 9"). Boxes are in the tiled
/// image's pixels at scale 1, so only the text is of use.
pub fn ocr_tiled<const N: usize>(img: &Rgba, regions: &[(Rect, f32); N], n: usize) -> [Vec<Line>; N] {
    let tiles: [Rgba; N] = regions.map(|(rect, _)| tiled(img, rect, n));
    let whole: [(Rect, f32); N] = std::array::from_fn(|i| (Rect::new(0, 0, tiles[i].w as i32, tiles[i].h as i32), regions[i].1));
    ocr_all(std::array::from_fn(|i| &tiles[i]), &whole)
}

/// `img`'s `rect` repeated `n` times side by side, 12 px apart, the gaps
/// filled with each row's left edge colour.
fn tiled(img: &Rgba, rect: Rect, n: usize) -> Rgba {
    const GAP: usize = 12;
    let c = img.crop(rect);
    if c.w == 0 || n == 0 {
        return c;
    }
    let w = c.w * n + GAP * (n - 1);
    let mut data = Vec::with_capacity(w * c.h * 4);
    for row in c.data.chunks_exact(c.w * 4) {
        for k in 0..n {
            if k > 0 {
                (0..GAP).for_each(|_| data.extend_from_slice(&row[..4]));
            }
            data.extend_from_slice(row);
        }
    }
    Rgba { w, h: c.h, data }
}

/// The OCR input for `rect`: the crop upscaled by `scale` with its contrast
/// stretched. None when the region is empty.
fn prepare(crop: &Rgba, scale: f32) -> Option<Rgba> {
    if crop.w == 0 || crop.h == 0 {
        return None;
    }
    let (w, h) = ((crop.w as f32 * scale).round() as usize, (crop.h as f32 * scale).round() as usize);
    Some(stretch(&crop.resize(w.max(1), h.max(1))))
}

/// Stretches the contrast so the 1st to 99th percentile of luma spans 0-255:
/// faded-in buttons and text under the red target-pick tint read better.
fn stretch(img: &Rgba) -> Rgba {
    let mut hist = [0usize; 256];
    for p in img.data.as_chunks::<4>().0 {
        hist[luma(p) as usize] += 1;
    }
    let n = img.w * img.h;
    let percentile = |q: usize| {
        let mut acc = 0;
        hist.iter().position(|&c| {
            acc += c;
            acc * 100 >= n * q
        })
    };
    let (lo, hi) = (percentile(1).unwrap_or(0) as f32, percentile(99).unwrap_or(255) as f32);
    if hi - lo < 8.0 {
        return img.clone();
    }
    let k = 255.0 / (hi - lo);
    let table: [u8; 256] = std::array::from_fn(|v| ((v as f32 - lo) * k).clamp(0.0, 255.0) as u8);
    let mut data = img.data.clone();
    for p in data.as_chunks_mut::<4>().0 {
        for v in &mut p[..3] {
            *v = table[*v as usize];
        }
    }
    Rgba { w: img.w, h: img.h, data }
}

fn luma(p: &[u8; 4]) -> u8 {
    ((u32::from(p[0]) * 77 + u32::from(p[1]) * 150 + u32::from(p[2]) * 29) >> 8) as u8
}

#[cfg(windows)]
mod imp {
    use super::Rgba;
    use crate::lang::{self, Lang};
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex, OnceLock, mpsc};
    use std::time::{Duration, Instant};
    use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::{OcrEngine, OcrResult};
    use windows::Storage::Streams::DataWriter;
    use windows::core::Result;

    /// How long an engine that does not read the set language (the profile
    /// languages' fallback, or none) is kept before the fitting one is looked
    /// for again.
    const RETRY: Duration = Duration::from_secs(2);

    /// The longest wait between two looks for the fitting engine: with no OCR of the set language installed at all,
    /// every thread (the window's too) would otherwise make an engine again every [`RETRY`]; the wait doubles from
    /// [`RETRY`] up to this.
    const RETRY_MAX: Duration = Duration::from_secs(60);

    /// A thread's engine: the language it was made for, the engine, and for an engine that does not read that
    /// language when it was made and how long to wait before looking again (see [`ENGINE`]).
    type Slot = (Lang, Option<OcrEngine>, Option<(Instant, Duration)>);

    thread_local! {
        /// This thread's engine, the language it was made for, and for an
        /// engine that does not read that language when it was made: made
        /// again when the set language changes, and an unfitting one again
        /// after [`RETRY`], then after twice as long up to [`RETRY_MAX`] (a listing of the installed languages that failed
        /// once must not leave a thread reading in the profile language for
        /// good: on 2026-11-08 the 1.1.0 test build read a whole Vorax round
        /// of the English client as Traditional Chinese).
        static ENGINE: RefCell<Option<Slot>> = const { RefCell::new(None) };
    }

    /// Runs `f` on this thread's engine for the set language.
    fn with_engine<T>(f: impl FnOnce(Option<&OcrEngine>) -> T) -> T {
        ENGINE.with(|cell| {
            let mut slot = cell.borrow_mut();
            let want = lang::lang();
            let stale = |made_for: &Lang, unfit: &Option<(Instant, Duration)>| *made_for != want || unfit.is_some_and(|(at, wait)| at.elapsed() >= wait);
            if slot.as_ref().is_none_or(|(made_for, _, unfit)| stale(made_for, unfit)) {
                // The wait before this look, when this thread's engine for the same language did not fit.
                let waited = slot.as_ref().and_then(|(made_for, _, unfit)| unfit.filter(|_| *made_for == want).map(|(_, wait)| wait));
                let e = engine(want);
                let fitting = e.as_ref().and_then(|e| e.RecognizerLanguage().ok()?.LanguageTag().ok()).is_some_and(|t| super::fits(&t.to_string(), want));
                if fitting && waited.is_some() {
                    // What the unfitting engine read is cached under the set language and would keep coming back for
                    // a crop that looks the same (a hit is not written again, so it is never pushed out): read again.
                    super::clear_cache();
                }
                let next = waited.map_or(RETRY, |w| (w * 2).min(RETRY_MAX));
                *slot = Some((want, e, (!fitting).then(|| (Instant::now(), next))));
            }
            f(slot.as_ref().and_then(|(_, e, _)| e.as_ref()))
        })
    }

    /// The first OCR language installed that reads `lang`, else the profile languages.
    fn engine(lang: Lang) -> Option<OcrEngine> {
        let fitting = OcrEngine::AvailableRecognizerLanguages()
            .ok()
            .and_then(|all| all.into_iter().find(|l| l.LanguageTag().is_ok_and(|t| super::fits(&t.to_string(), lang))));
        fitting.map_or_else(OcrEngine::TryCreateFromUserProfileLanguages, |l| OcrEngine::TryCreateFromLanguage(&l)).ok()
    }

    pub fn language() -> Option<String> {
        with_engine(|e| Some(e?.RecognizerLanguage().ok()?.LanguageTag().ok()?.to_string()))
    }

    type Lines = super::Lines;
    type Job = (Rgba, mpsc::Sender<(usize, Option<Lines>)>, usize);

    /// Reader threads, each with its own engine: one engine given several
    /// reads at once returns some of them empty.
    const WORKERS: usize = 4;

    /// The queue the reader threads take images from, started on first use.
    fn pool() -> &'static mpsc::Sender<Job> {
        static POOL: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
        POOL.get_or_init(|| {
            let (jobs, queue) = mpsc::channel::<Job>();
            let queue = Arc::new(Mutex::new(queue));
            for _ in 0..WORKERS {
                let queue = Arc::clone(&queue);
                std::thread::spawn(move || {
                    while let Ok((img, back, i)) = queue.lock().map_err(drop).and_then(|q| q.recv().map_err(drop)) {
                        let _ = back.send((i, recognize(&img)));
                    }
                });
            }
            jobs
        })
    }

    /// The lines of each image with their boxes (x, y, w, h) in its pixels;
    /// None for a missing image or a failed read. The images are read on the
    /// reader threads at once.
    pub fn recognize_all(imgs: Vec<Option<Rgba>>) -> Vec<Option<Lines>> {
        let mut out = vec![None; imgs.len()];
        let (back, done) = mpsc::channel();
        for (i, img) in imgs.into_iter().enumerate() {
            if let Some(img) = img {
                let _ = pool().send((img, back.clone(), i));
            }
        }
        drop(back);
        for (i, lines) in done {
            out[i] = lines;
        }
        out
    }

    /// The lines of `img` read on this thread's engine; None when the read
    /// fails, which is not cached.
    fn recognize(img: &Rgba) -> Option<Lines> {
        let read = || lines(&with_engine(|e| e.map(|e| e.RecognizeAsync(&bitmap(img)?))).ok_or_else(windows::core::Error::empty)??.join()?);
        read().ok()
    }

    /// `img` as a Bgra8 bitmap, shrunk to the engine's largest size.
    fn bitmap(img: &Rgba) -> Result<SoftwareBitmap> {
        let max = OcrEngine::MaxImageDimension()? as usize;
        let img = if img.w > max || img.h > max {
            let k = max as f32 / img.w.max(img.h) as f32;
            img.resize(((img.w as f32 * k) as usize).max(1), ((img.h as f32 * k) as usize).max(1))
        } else {
            img.clone()
        };
        let bgra: Vec<u8> = img.data.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| [b, g, r, 255]).collect();
        let writer = DataWriter::new()?;
        writer.WriteBytes(&bgra)?;
        SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
            &writer.DetachBuffer()?,
            BitmapPixelFormat::Bgra8,
            img.w as i32,
            img.h as i32,
            BitmapAlphaMode::Ignore,
        )
    }

    /// The lines of a result with their boxes (x, y, w, h).
    fn lines(result: &OcrResult) -> Result<Vec<(String, [f32; 4])>> {
        let mut out = Vec::new();
        for line in result.Lines()? {
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for word in line.Words()? {
                let r = word.BoundingRect()?;
                (x0, y0) = (x0.min(r.X), y0.min(r.Y));
                (x1, y1) = (x1.max(r.X + r.Width), y1.max(r.Y + r.Height));
            }
            if x0 <= x1 {
                out.push((line.Text()?.to_string(), [x0, y0, x1 - x0, y1 - y0]));
            }
        }
        Ok(out)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Rgba;

    pub fn recognize_all(imgs: Vec<Option<Rgba>>) -> Vec<Option<super::Lines>> {
        vec![None; imgs.len()]
    }

    pub fn language() -> Option<String> {
        None
    }

}
