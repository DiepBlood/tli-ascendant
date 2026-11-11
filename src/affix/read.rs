//! Reads the affix entries of the map page's affix panel from a game frame and grades them.
//!
//! Steps (measured in the lab on 2560x1440 screenshots; the lab's report has the numbers):
//! 1. bullets: one diamond left of every entry gives the entry count, its rows and the game's colour;
//! 2. OCR of the panel upscaled 3 times; for Chinese the blue underline of keyword links is painted over first,
//!    or the underlined keywords (致殘, 燒灼, ...) drop out of the text;
//! 3. every OCR line goes to the last bullet at or above its middle;
//! 4. each entry's text is looked up in the grade table.

use super::bullets::{self, Bullet, BulletColour, REF_HEIGHT};
use super::dict::{Dict, Grade, Lookup};
use crate::lang::{self, Lang};
use crate::reader::ocr;
use crate::reader::{Rect, Rgba};

/// Affix panel text area in reference pixels (2560x1440): x 100..620, y 380..990.
const PANEL: Rect = Rect::new(100, 380, 520, 610);
/// Upscale before OCR: small game text reads better at 3x.
const SCALE: usize = 3;
/// Blue runs at least this long (upscaled pixels) are underlines; 60 lost the fewest keywords in the lab.
const UNDERLINE_RUN: usize = 60;
/// Half a text line plus a margin (reference pixels): an entry starts this far above its bullet's middle.
const HALF_LINE: f64 = 17.0;
/// An OCR line belongs to a bullet whose top is at most this far below the line's middle.
const BULLET_SLACK: f64 = 8.0;
/// Space a further text line needs (reference pixels): a last entry ending closer than this to the panel
/// bottom may have lines below the panel, so it may match the start of a sentence.
const LINE_PITCH: i32 = 28;
/// Characters OCR reads for the bullet itself.
const BULLET_CHARS: &[char] = &['•', '·', '◆', '◇', '*', '♦', ' '];

/// One affix entry of the panel.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Rows of the entry in frame pixels (top inclusive, bottom exclusive): where its frame goes.
    pub top: i32,
    pub bottom: i32,
    pub colour: BulletColour,
    /// Text as read (lines joined; Chinese without spaces).
    pub text: String,
    /// The end of the entry may be below the panel (last entry reaching the bottom).
    pub cut: bool,
    pub lookup: Lookup,
    /// Not found, but near a sentence the user graded ([`Dict::near_user_grade`]): that grade, which frames it in place
    /// of the game's colour (a misread of an affix the user set to not framed stays not framed).
    pub user: Option<Grade>,
}

/// The entries of the panel, top to bottom; empty when no bullet was found.
#[derive(Clone, Debug, Default)]
pub struct Panel {
    pub entries: Vec<Entry>,
    /// The panel's title bar shows ([`super::panel::title_shows`]): known to be the affix panel, not another screen.
    pub title: bool,
}

/// Frame columns the frames cover (frame pixels): left of the bullets to the end of the text column.
pub fn frame_columns(frame_h: usize) -> (i32, i32) {
    let k = frame_h as f64 / REF_HEIGHT;
    ((118.0 * k).round() as i32, (600.0 * k).round() as i32)
}

/// How the panel crop is prepared for OCR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prep {
    /// Paint over blue runs at least this long (upscaled pixels): the underline of keyword links.
    pub underline_run: Option<usize>,
    /// Turn blue keyword text grey at its own brightness, so it reads like the white text around it.
    pub recolor_blue: bool,
}

impl Prep {
    /// The preparation chosen for a language (see `preparations_compared` in tests/affix_read.rs): Chinese has its
    /// keywords' underlines painted out; English has its blue keywords turned grey (2026-11-09: one more of the 307
    /// English entries with a log id, Tenacity Blessing read whole, and none worse; painting English underlines out
    /// read worse, as did turning Chinese keywords grey).
    pub fn for_lang(zh: bool) -> Self {
        if zh {
            Prep { underline_run: Some(UNDERLINE_RUN), recolor_blue: false }
        } else {
            Prep { underline_run: None, recolor_blue: true }
        }
    }
}

/// Reads and grades the affix panel of `frame` in the current OCR language. The preparation (the zh-TW client's
/// keyword underlines painted out) follows the language the OCR engine really reads in, asked at each read, as Vorax's
/// templates since 1.1.0 (`ocr::client_language`: the window language, unless Windows has no OCR of it and reads in a
/// profile language); the window language when that is neither. The sentences match in both languages either way.
pub fn read(frame: &Rgba, dict: &Dict) -> Panel {
    read_scaled(frame, dict).0
}

/// A placement this close to the measured scale is read as the measured one (its frames within half a pixel a row).
const SCALE_SAME: f64 = 0.01;

/// [`read`], and whether the panel is of the interface scale the reader was measured at ([`BULLET_HEIGHT`]; with no
/// bullet, false when the panel's title bar shows drawn at another scale, `super::panel::other_scale`, else None). A
/// panel of another scale is not read (no entries).
pub fn read_scaled(frame: &Rgba, dict: &Dict) -> (Panel, Option<bool>) {
    let (panel, fits, _) = read_placed(frame, dict);
    (panel, fits)
}

/// [`read_scaled`], and where the panel was drawn when at another interface scale than measured
/// ([`super::panel::Placement`]; its entries' rows are the frame's, its frames go through it, see
/// [`super::frames::boxes_at`]).
pub fn read_placed(frame: &Rgba, dict: &Dict) -> (Panel, Option<bool>, Option<super::panel::Placement>) {
    let client = ocr::client_language().unwrap_or_else(lang::lang);
    read_at(frame, dict, Prep::for_lang(client == Lang::ZhTw))
}

/// [`read`] with a given OCR preparation (for comparing preparations).
pub fn read_with(frame: &Rgba, dict: &Dict, prep: Prep) -> Panel {
    read_at(frame, dict, prep).0
}

/// A panel drawn at another interface scale (no title bar where measured, but one placed by its edges,
/// [`super::panel::placement`]) is turned back into the measured scale's ([`super::panel::unscaled`]) and read as
/// measured; framed only when that reads as the measured panel does (the title bar, bullets of the measured size,
/// [`scale_fits`]), else read as another scale (no entries; the page says so). The user's choice of 2026-11-10: any
/// scale framed when sure, else the hint. Its entries' rows go back to the frame.
fn read_at(frame: &Rgba, dict: &Dict, prep: Prep) -> (Panel, Option<bool>, Option<super::panel::Placement>) {
    if !super::panel::title_shows(frame)
        && let Some(p) = super::panel::placement(frame).filter(|p| (p.s - 1.0).abs() >= SCALE_SAME)
    {
        let flat = super::panel::unscaled(frame, p, super::panel::AREA_REF);
        let (mut got, fits) = read_prepared(&flat, dict, prep);
        if fits != Some(true) || !got.title {
            return (Panel::default(), Some(false), Some(p));
        }
        let k = frame.h as f64 / REF_HEIGHT;
        for e in &mut got.entries {
            (e.top, e.bottom) = ((p.y(f64::from(e.top)) * k).round() as i32, (p.y(f64::from(e.bottom)) * k).round() as i32);
        }
        return (got, Some(true), Some(p));
    }
    let (got, fits) = read_prepared(frame, dict, prep);
    (got, fits, None)
}

/// [`read_scaled`] with a given OCR preparation.
fn read_prepared(frame: &Rgba, dict: &Dict, prep: Prep) -> (Panel, Option<bool>) {
    let k = frame.h as f64 / REF_HEIGHT;
    let reference = to_reference(frame);
    let marks = bullets::find(reference.h as u32, |x, y| reference.rgb(x as i32, y as i32));
    if marks.is_empty() {
        // at the larger interface scales (1.26, 1.5) no bullet is where the scan looks: the title bar tells that panel
        // from none (the page then says why nothing is framed; 2026-11-10)
        let other = !super::panel::title_shows(frame) && super::panel::other_scale(frame).is_some();
        return (Panel::default(), other.then_some(false));
    }
    if !scale_fits(&marks) {
        return (Panel::default(), Some(false));
    }
    let zh = lang::lang() == Lang::ZhTw;
    let lines = panel_lines(frame, &reference, prep);
    let parts = assign(&marks, &lines);
    let last = marks.len() - 1;
    let starts: Vec<f64> = marks.iter().map(|b| b.centre() - HALF_LINE).collect();
    let panel_bottom = PANEL.y + PANEL.h;
    let entries = marks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let own = &parts[i];
            let text = join(own.iter().map(|l| l.text.as_str()), zh);
            let text = if zh { fix_keywords(&text) } else { text };
            let lines_bottom = own.iter().map(|l| l.bottom).max().unwrap_or(b.bottom as i32);
            let bottom = if i < last {
                starts[i + 1]
            } else {
                ((lines_bottom + 10) as f64).max(b.centre() + HALF_LINE)
            };
            let cut = i == last && lines_bottom + LINE_PITCH > panel_bottom;
            let lookup = match dict.lookup(&text, cut) {
                // an affix learned from the log and not graded by the user yet: the game's colour grades it
                Lookup::Found { index, exact, .. } if dict.sentences()[index].by_game_colour() => {
                    Lookup::Found { index, grade: super::frames::game_grade(b.colour), exact }
                }
                other => other,
            };
            let user = (lookup == Lookup::Unknown).then(|| dict.near_user_grade(&text)).flatten();
            Entry {
                top: (starts[i] * k).round() as i32,
                bottom: (bottom * k).round() as i32,
                colour: b.colour,
                text,
                cut,
                lookup,
                user,
            }
        })
        .collect();
    (Panel { entries, title: super::panel::title_shows(frame) }, Some(true))
}

/// The game client language a read panel's words are in, when they tell, for a panel read in `read_in` (the
/// language set in Ascendant, which picks the OCR engine; nothing tells the client's own language before a level is
/// entered). Measured on every saved panel frame read in both languages and the 3000 frames of other screens
/// (2026-11-10, the user's test of 01:52:56: an English client read with Ascendant still set to Traditional Chinese):
/// - read in Chinese, an English panel comes out as Latin letters with words run together, 300 or more a read and at
///   most one CJK character; a Chinese panel has CJK characters and no letter. [`LETTERS_ENGLISH`] or more letters
///   and ten times as many letters as CJK characters is English; [`CJK_CHINESE`] or more CJK characters is Chinese.
/// - read in English, a Chinese panel comes out as its numbers and signs only (at most 8 letters a read, none on most
///   reads); an English panel has 56 letters an entry or more. Fewer than [`LETTERS_PER_ENTRY`] letters an entry is
///   Chinese, that many or more is English.
///
/// Only a panel whose title bar shows tells: another screen of an English client read in Chinese is English too, but
/// not the affix panel (one of the 3000 other screens, an English client's screen of 2026-11-07, read so).
///
/// None when the words do not tell (no title bar, no entry, too few words).
pub fn words_language(panel: &Panel, read_in: Lang) -> Option<Lang> {
    let entries = panel.entries.len();
    if entries == 0 || !panel.title {
        return None;
    }
    let (mut letters, mut cjk) = (0usize, 0usize);
    for c in panel.entries.iter().flat_map(|e| e.text.chars()) {
        letters += usize::from(c.is_ascii_alphabetic());
        cjk += usize::from(matches!(c, '\u{3400}'..='\u{9fff}'));
    }
    match read_in {
        Lang::ZhTw if letters >= LETTERS_ENGLISH && cjk * 10 <= letters => Some(Lang::En),
        Lang::ZhTw if cjk >= CJK_CHINESE => Some(Lang::ZhTw),
        Lang::En if letters >= LETTERS_PER_ENTRY * entries => Some(Lang::En),
        Lang::En => Some(Lang::ZhTw),
        _ => None,
    }
}

/// Latin letters a panel read in Chinese must have to be English ([`words_language`]): one short entry's worth.
const LETTERS_ENGLISH: usize = 30;
/// CJK characters a panel read in Chinese must have to be Chinese ([`words_language`]).
const CJK_CHINESE: usize = 10;
/// Latin letters an entry a panel read in English has when it is English ([`words_language`]).
const LETTERS_PER_ENTRY: usize = 5;

/// Texts read exactly as one sentence that sentences of another grade are one character from ([`Dict::exact_only`]),
/// with how many reads of the page showed each: from the second such read the entry is that sentence (one read could
/// be a misread; the same text twice, with no known OCR swap between the sentences, is the game's). On the saved reads
/// of 2026-11-08 to 11-10: 41 of the 43 entries left unsure, the 28 the game log could check all right; the second read
/// came 0.14 to 1.1 s after the first. One page visit's: [`ExactSeen::clear`] when another starts or the table changes.
#[derive(Debug, Default)]
pub struct ExactSeen {
    reads: std::collections::HashMap<String, u32>,
}

impl ExactSeen {
    /// Forgets the texts counted.
    pub fn clear(&mut self) {
        self.reads.clear();
    }

    /// Counts this read's ambiguous entries that equal one sentence so, and takes those read so before for it (its
    /// grade, or the game's colour for a sentence framed by it). Returns how many were taken.
    pub fn confirm(&mut self, panel: &mut Panel, dict: &Dict) -> usize {
        let mut counted = std::collections::HashSet::new();
        let mut taken = 0;
        for e in &mut panel.entries {
            if e.lookup != Lookup::Ambiguous {
                continue;
            }
            let Some(i) = dict.exact_only(&e.text) else { continue };
            let k = super::dict::key(&e.text);
            if counted.insert(k.clone()) {
                *self.reads.entry(k.clone()).or_default() += 1;
            }
            if self.reads.get(&k).is_some_and(|&n| n >= 2) {
                let s = &dict.sentences()[i];
                let grade = if s.by_game_colour() { super::frames::game_grade(e.colour) } else { s.grade };
                e.lookup = Lookup::Found { index: i, grade, exact: true };
                taken += 1;
            }
        }
        taken
    }
}

/// The bullets' height (reference pixels) at the interface scale the reader was measured at: 11 or 12 on the 1440p
/// screenshots, 10 or 11 on 1080p frames made from them, 13 or 14 with the panel 70 greys brighter (the scan also
/// finds bullets cut short, 7 to 9, and things of 15 to 17 that are not panels; 2026-11-09). Another interface scale
/// draws the panel another size, and its entries' rows, frames and places are not the measured ones: at 0.8 the
/// bullets are about 9 high (and text the scan takes for bullets of 12 to 14 stands about 26 apart); at 1.2 and 1.5 no
/// bullet is found where the scan looks.
const BULLET_HEIGHT: (u32, u32) = (10, 14);

/// The least distance between two bullets of [`BULLET_HEIGHT`] (reference pixels) at the measured scale: one-line
/// entries are 32 to 34 apart, 28 at the least with the panel brighter; at 0.8 they are 24 to 28 apart, and at 0.9
/// 26 to 30 (on the 132 screenshots, 2026-11-09).
const LEAST_GAP: f64 = 27.0;

/// Whether the bullets (reference pixels) are of the interface scale the reader was measured at: at least half of
/// them of [`BULLET_HEIGHT`] (an entry cut short at the top and one whole make a panel; a panel of the screenshots has
/// two short ones), and none of those closer to the next than [`LEAST_GAP`]. No bullet fits (nothing is read).
fn scale_fits(marks: &[Bullet]) -> bool {
    let measured: Vec<&Bullet> = marks.iter().filter(|b| (BULLET_HEIGHT.0..=BULLET_HEIGHT.1).contains(&(b.bottom - b.top))).collect();
    !measured.is_empty() && measured.len() * 2 >= marks.len() && measured.windows(2).all(|w| w[1].centre() - w[0].centre() >= LEAST_GAP)
}

/// Where each entry of the panel starts (frame pixels, top to bottom), from the bullets alone, no OCR: the tops
/// [`read_with`] gives its entries. Empty when no bullet shows.
pub fn entry_tops(frame: &Rgba) -> Vec<i32> {
    let k = frame.h as f64 / REF_HEIGHT;
    let strip;
    let at = if frame.h == REF_HEIGHT as usize {
        frame
    } else {
        // the bullets' column only: the frame's left strip to the reference height (a whole frame's resize took
        // 5.5 ms a 1080p frame, on the window's thread)
        let w = ((BULLET_STRIP * frame.h as f64 / REF_HEIGHT).ceil() as usize).min(frame.w);
        strip = to_reference(&frame.crop(Rect::new(0, 0, w as i32, frame.h as i32)));
        &strip
    };
    let marks = bullets::find(at.h as u32, |x, y| at.rgb(x as i32, y as i32));
    marks.iter().map(|b| ((b.centre() - HALF_LINE) * k).round() as i32).collect()
}

/// [`entry_tops`] of a panel drawn at placement `p` (the last read's, [`read_placed`]): the bullets' column turned back
/// into the measured scale's, the tops back to the frame. [`entry_tops`] itself for none.
pub fn entry_tops_at(frame: &Rgba, place: Option<super::panel::Placement>) -> Vec<i32> {
    let Some(p) = place else { return entry_tops(frame) };
    let area = super::panel::AREA_REF;
    let flat = super::panel::unscaled(frame, p, Rect::new(0, area.y, BULLET_STRIP as i32, area.h));
    let marks = bullets::find(flat.h as u32, |x, y| flat.rgb(x as i32, y as i32));
    let k = frame.h as f64 / REF_HEIGHT;
    marks.iter().map(|b| (p.y(b.centre() - HALF_LINE) * k).round() as i32).collect()
}

/// Reference columns [`entry_tops`] resizes: the bullets' column (to 142) and a margin.
const BULLET_STRIP: f64 = 200.0;

/// The frame at reference height (1440), keeping its aspect: the panel is anchored to the left edge.
fn to_reference(frame: &Rgba) -> Rgba {
    if frame.h == REF_HEIGHT as usize {
        return frame.clone();
    }
    let w = (frame.w as f64 * REF_HEIGHT / frame.h as f64).round() as usize;
    frame.resize(w, REF_HEIGHT as usize)
}

/// The upscale of a frame `h` high that gives the reference's upscaled size straight from the frame (1080: 4 times,
/// as 1440 three times; 2160: 2), when there is a whole one other than the reference's own: the panel then goes
/// through one resampling, not the frame's bilinear resize to 1440 and the upscale after it, which blurred keyword
/// strokes away (2026-11-09: 1080p frames made from the 1440p screenshots).
fn native_scale(h: usize) -> Option<usize> {
    let target = SCALE * REF_HEIGHT as usize;
    (h != REF_HEIGHT as usize && h > 0 && target.is_multiple_of(h)).then(|| target / h).filter(|&f| (1..=6).contains(&f))
}

/// OCR lines of the panel in reference pixels, in reading order: from `frame` itself when its height has a
/// [`native_scale`], else from `reference` (the frame at reference height).
fn panel_lines(frame: &Rgba, reference: &Rgba, prep: Prep) -> Vec<OcrLine> {
    let (crop, factor) = match native_scale(frame.h) {
        Some(f) => {
            let k = frame.h as f64 / REF_HEIGHT;
            let s = |v: i32| (f64::from(v) * k).round() as i32;
            (frame.crop(Rect::new(s(PANEL.x), s(PANEL.y), s(PANEL.w), s(PANEL.h))), f)
        }
        None => (reference.crop(PANEL), SCALE),
    };
    if crop.w == 0 || crop.h == 0 {
        return Vec::new();
    }
    // either way the upscaled panel is the reference's size times SCALE: the underline lengths and rows below hold
    let mut big = upscale_cubic(&crop, factor);
    if let Some(run) = prep.underline_run {
        paint_underlines(&mut big, run);
    }
    if prep.recolor_blue {
        recolor_blue(&mut big);
    }
    let s = SCALE as f64;
    let mut lines: Vec<OcrLine> = ocr::ocr(&big, Rect::new(0, 0, big.w as i32, big.h as i32), 1.0)
        .into_iter()
        .map(|l| {
            let y = PANEL.y as f64 + l.rect.y as f64 / s;
            let h = l.rect.h as f64 / s;
            OcrLine { text: l.text, x: l.rect.x, middle: y + h / 2.0, bottom: (y + h).round() as i32 }
        })
        .collect();
    reading_order(&mut lines);
    lines
}

/// One OCR line in reference pixels (`x` in upscaled pixels, only used for ordering).
#[derive(Clone, Debug)]
struct OcrLine {
    text: String,
    x: i32,
    middle: f64,
    bottom: i32,
}

/// Sorts lines top to bottom and, within one text row, left to right: OCR sometimes splits a row into pieces
/// whose middles differ by a pixel or two, and sorting by the middle alone put the right piece first.
fn reading_order(lines: &mut [OcrLine]) {
    lines.sort_by(|a, b| a.middle.total_cmp(&b.middle));
    let mut start = 0;
    while start < lines.len() {
        let mut end = start + 1;
        // pieces of one row: middles within a third of a line of the row's first piece
        while end < lines.len() && lines[end].middle - lines[start].middle < HALF_LINE * 2.0 / 3.0 {
            end += 1;
        }
        lines[start..end].sort_by_key(|l| l.x);
        start = end;
    }
}

/// Catmull-Rom taps of output index `o` along a line of `n` source pixels, upscaled `f` times.
fn cubic_taps(o: usize, n: usize, f: usize) -> [(usize, f32); 4] {
    let src = (o as f32 + 0.5) / f as f32 - 0.5;
    let i = src.floor();
    let t = src - i;
    let wts = [
        ((-t + 2.0) * t - 1.0) * t / 2.0,
        ((3.0 * t - 5.0) * t * t + 2.0) / 2.0,
        ((-3.0 * t + 4.0) * t + 1.0) * t / 2.0,
        (t - 1.0) * t * t / 2.0,
    ];
    std::array::from_fn(|k| ((i as i64 - 1 + k as i64).clamp(0, n as i64 - 1) as usize, wts[k]))
}

/// Catmull-Rom upscale by an integer factor: sharper strokes than bilinear, as the lab's bicubic upscale
/// (bilinear lost more keywords and letters on the same screenshots). Both passes (horizontal into floats, then
/// vertical) are split by rows over a few threads: each output value is the same sum, in the same order, as on one
/// thread, so the image is the same byte for byte (`parallel_upscale_matches_one_thread`); the 3x panel took 45 to 52
/// ms on one thread on the live test's frames (2026-11-09).
fn upscale_cubic(img: &Rgba, f: usize) -> Rgba {
    let (w, h) = (img.w, img.h);
    let (ow, oh) = (w * f, h * f);
    if ow == 0 || oh == 0 {
        return Rgba { w: ow, h: oh, data: Vec::new() };
    }
    let cols: Vec<[(usize, f32); 4]> = (0..ow).map(|x| cubic_taps(x, w, f)).collect();
    let rows: Vec<[(usize, f32); 4]> = (0..oh).map(|y| cubic_taps(y, h, f)).collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    let mut mid = vec![0f32; ow * h * 3];
    let per = h.div_ceil(threads);
    std::thread::scope(|scope| {
        for (k, part) in mid.chunks_mut(per * ow * 3).enumerate() {
            let cols = &cols;
            scope.spawn(move || {
                for (r, row) in part.chunks_mut(ow * 3).enumerate() {
                    let y = k * per + r;
                    for (x, tap) in cols.iter().enumerate() {
                        for c in 0..3 {
                            row[x * 3 + c] = tap.iter().map(|&(sx, wt)| img.data[(y * w + sx) * 4 + c] as f32 * wt).sum();
                        }
                    }
                }
            });
        }
    });
    let mut data = vec![255u8; ow * oh * 4];
    let per = oh.div_ceil(threads);
    std::thread::scope(|scope| {
        for (k, part) in data.chunks_mut(per * ow * 4).enumerate() {
            let (rows, mid) = (&rows, &mid);
            scope.spawn(move || {
                for (r, out) in part.chunks_mut(ow * 4).enumerate() {
                    let tap = &rows[k * per + r];
                    for x in 0..ow {
                        for c in 0..3 {
                            let v: f32 = tap.iter().map(|&(sy, wt)| mid[(sy * ow + x) * 3 + c] * wt).sum();
                            out[x * 4 + c] = v.round().clamp(0.0, 255.0) as u8;
                        }
                    }
                }
            });
        }
    });
    Rgba { w: ow, h: oh, data }
}

/// Rows below a long blue run (upscaled pixels) that must hold no blue for it to be an underline: about 4 reference
/// pixels, less than the gap to the next text line.
const UNDERLINE_CLEAR: usize = 12;
/// A row below the run is part of the same line (its thickness) when at least half its pixels across the run are blue.
const UNDERLINE_BODY: f64 = 0.5;

/// Paints over the underline of keyword links (Windows OCR drops the underlined word otherwise): horizontal runs of
/// blue pixels at least `min_run` long with no blue below them for [`UNDERLINE_CLEAR`] rows past the line's own
/// thickness. A keyword's own long strokes (the top of 凍, 聚, 麻) have more of the word below them, and its lowest
/// stroke the underline: painting every long run took those strokes too, and 凍僵 read as 冫, 麻痺 not at all
/// (2026-11-09).
fn paint_underlines(img: &mut Rgba, min_run: usize) {
    let (w, h) = (img.w, img.h);
    let blue: Vec<bool> = img
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| p[2] > 140 && p[2] as i32 - p[0] as i32 > 50)
        .collect();
    let blue_in = |y: usize, x0: usize, x1: usize| blue[y * w + x0..y * w + x1].iter().filter(|&&b| b).count();
    let mut paint = Vec::new();
    for y in 0..h {
        let mut x = 0;
        while x < w {
            if !blue[y * w + x] {
                x += 1;
                continue;
            }
            let x0 = x;
            while x < w && blue[y * w + x] {
                x += 1;
            }
            let len = x - x0;
            if len < min_run {
                continue;
            }
            // past the line's own thickness, then nothing blue below it
            let mut below = y + 1;
            while below < h && blue_in(below, x0, x) as f64 >= len as f64 * UNDERLINE_BODY {
                below += 1;
            }
            let stray = (len / 20).max(3);
            if (below..h.min(below + UNDERLINE_CLEAR)).all(|r| blue_in(r, x0, x) < stray) {
                paint.push((y, x0, x));
            }
        }
    }
    for &(y, x0, x1) in &paint {
        for p in img.data[(y * w + x0) * 4..(y * w + x1) * 4].as_chunks_mut::<4>().0 {
            p[..3].copy_from_slice(&PANEL_GROUND);
        }
    }
    // The upscale spreads the 1 px line over a few more rows than pass the blue test: a faint cyan row left above it
    // touches the word's lowest stroke, and OCR reads the word and the line as one shape (凍僵 as 冫, 麻痺 dropped,
    // 聚 dropped; 2026-11-09). Faint cyan pixels in the row above the line and the two below it go too, not further
    // up, and not the bright cyan of a stroke there (both took strokes of 祝福 off: 祝褔, 堅韌福).
    for &(y, x0, x1) in &paint {
        let rows = y.saturating_sub(EDGE_ABOVE)..(y + EDGE_BELOW + 1).min(h);
        for r in rows.filter(|&r| r != y) {
            for p in img.data[(r * w + x0) * 4..(r * w + x1) * 4].as_chunks_mut::<4>().0 {
                let (red, green, blue) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
                // faint cyan only: a keyword's own strokes there are bright (the blue test above) and stay
                let bright = blue > 140 && blue - red > 50;
                if !bright && blue > 60 && green > 50 && blue - red > 30 && green - red > 25 {
                    p[..3].copy_from_slice(&PANEL_GROUND);
                }
            }
        }
    }
}

/// The panel's dark ground, painted over an underline.
const PANEL_GROUND: [u8; 3] = [22, 24, 25];
/// Rows (upscaled) above and below an underline row whose cyan edge pixels are painted too, see [`paint_underlines`].
const EDGE_ABOVE: usize = 1;
const EDGE_BELOW: usize = 2;

/// Keyword pixels (the same blue test as [`paint_underlines`]) become grey at their brightest channel.
fn recolor_blue(img: &mut Rgba) {
    for p in img.data.as_chunks_mut::<4>().0 {
        if p[2] > 140 && p[2] as i32 - p[0] as i32 > 50 {
            let v = p[0].max(p[1]).max(p[2]);
            p[..3].copy_from_slice(&[v, v, v]);
        }
    }
}

/// Lines of each bullet's entry, in reading order: a line goes to the last bullet whose top is at most
/// `BULLET_SLACK` below the line's middle; lines above the first bullet (the panel title) are dropped.
fn assign<'a>(marks: &[Bullet], lines: &'a [OcrLine]) -> Vec<Vec<&'a OcrLine>> {
    let mut parts = vec![Vec::new(); marks.len()];
    for l in lines {
        if let Some(i) = marks.iter().rposition(|b| b.top as f64 - BULLET_SLACK <= l.middle) {
            parts[i].push(l);
        }
    }
    parts
}

/// Joins an entry's lines: with spaces for English, without for Chinese; the bullet read as text is dropped.
/// The game's Chinese keywords of four characters or more (the `<e>` words of the user's logs, 2026-11-09): a
/// keyword cut by a line's end or read with a character wrong (聚 alone at a line's end read as 棸, 靈動祝,福) is put
/// back. Shorter ones are left alone: one character off fits other words (致盲 / 致殘; 迷踪值 / 迷踪效果, 迷踪 being
/// a keyword of its own).
const ZH_KEYWORDS: &[&str] = &["掉落數量", "中度症狀", "白夜模式", "全頭哥·布林", "靈頭哥·布林", "典獄秘藏", "堅韌祝福", "格擋比例", "聚能祝福", "靈動祝福"];

/// Chinese text with its keywords put back ([`ZH_KEYWORDS`]): a run of characters one edit from a keyword (the
/// spaces OCR puts between characters left out) becomes it, when no keyword is there already and no other run or
/// keyword is as near (no tie).
fn fix_keywords(text: &str) -> String {
    // the game's own characters first (棸能褔 is two edits from 聚能祝福, 聚能福 one; 2026-11-10 1080p)
    let raw: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    let chars: Vec<char> = raw.iter().copied().map(super::dict::game_char).collect();
    let mut out = chars.clone();
    let mut done: Vec<(usize, usize)> = Vec::new();
    for word in ZH_KEYWORDS {
        let w: Vec<char> = word.chars().collect();
        if out.windows(w.len()).any(|s| s == w.as_slice()) {
            continue;
        }
        let ws: String = w.iter().collect();
        let mut hits = Vec::new();
        for len in [w.len() - 1, w.len(), w.len() + 1] {
            for start in 0..=out.len().saturating_sub(len) {
                if start + len > out.len() {
                    break;
                }
                let s: String = out[start..start + len].iter().collect();
                if super::dict::levenshtein(&s, &ws) == 1 {
                    hits.push((start, len));
                }
            }
        }
        // one place only (overlapping windows of one place count once: at the first start, the longest span, so a
        // stray mark inside the keyword goes with it)
        hits.sort_by_key(|&(s, l)| (s, std::cmp::Reverse(l)));
        let first = hits.first().copied();
        let one_place = first.is_some_and(|(s, l)| hits.iter().all(|&(s2, l2)| s2 + l2 > s && s2 < s + l + 1));
        let Some((start, len)) = first.filter(|_| one_place) else { continue };
        // no other keyword as near the same place
        let rival = ZH_KEYWORDS.iter().filter(|o| *o != word).any(|o| {
            let os: String = o.chars().collect();
            (start.saturating_sub(1)..=start + 1).any(|s| {
                [len.saturating_sub(1), len, len + 1].iter().any(|&l| s + l <= out.len() && l > 0 && super::dict::levenshtein(&out[s..s + l].iter().collect::<String>(), &os) <= 1)
            })
        });
        if rival || done.iter().any(|&(s, l)| start < s + l && s < start + len) {
            continue;
        }
        out.splice(start..start + len, w.iter().copied());
        // keywords put back after this one moved with the length it changed by
        for d in done.iter_mut().filter(|d| d.0 > start) {
            d.0 = (d.0 + w.len()).saturating_sub(len);
        }
        done.push((start, w.len()));
    }
    if out == raw { text.to_string() } else { out.into_iter().collect() }
}

fn join<'a>(lines: impl Iterator<Item = &'a str>, zh: bool) -> String {
    let parts: Vec<&str> = lines.map(str::trim).filter(|t| !t.is_empty()).collect();
    let text = if zh { parts.concat() } else { parts.join(" ") };
    // a leading "+ " (the bullet read as plus) stays: key() tells it from a real "+15%"
    text.trim_start_matches(BULLET_CHARS).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry left unsure but equal to one sentence one safe character from others is that sentence from its second
    /// read in a page visit; one read, or after the visit's end, it stays unsure; entries found are not touched.
    #[test]
    fn an_exact_text_read_twice_is_taken() {
        let d = Dict::builtin().with_learned(&[super::super::learned::Learned { id: "5521013".into(), en: String::new(), zh: "玩家低魔時額外受到 #% 傷害".into() }]);
        let entry = |text: &str, lookup: Lookup| Entry { top: 0, bottom: 10, colour: BulletColour::White, text: text.into(), cut: false, lookup, user: None };
        let found = Lookup::Found { index: 3, grade: Grade::Danger, exact: true };
        let panel = || Panel { entries: vec![entry("玩 家 低 魔 時 額 外 受 到 30 % 傷 害", Lookup::Ambiguous), entry("anything", found)], title: true };
        let mut seen = ExactSeen::default();
        let mut first = panel();
        assert_eq!(seen.confirm(&mut first, &d), 0);
        assert_eq!(first.entries[0].lookup, Lookup::Ambiguous, "one read: unsure");
        let mut second = panel();
        assert_eq!(seen.confirm(&mut second, &d), 1);
        let Lookup::Found { index, grade, .. } = second.entries[0].lookup else { panic!("{:?}", second.entries[0].lookup) };
        assert!(d.sentences()[index].ids.contains(&"5521013".to_string()));
        assert_eq!(grade, Grade::None, "a learned sentence: the game's colour, white");
        assert_eq!(second.entries[1].lookup, found, "found entries untouched");
        seen.clear();
        let mut again = panel();
        assert_eq!(seen.confirm(&mut again, &d), 0, "another visit counts again");
    }

    /// The words of a panel tell the client language against the one it was read in: an English panel read in
    /// Chinese (the user's test of 2026-11-10, words run together), a Chinese one read in English (numbers only, the
    /// title bar on), and each read in its own language; too few words or no title bar tell nothing.
    #[test]
    fn the_words_tell_the_client_language() {
        let panel = |texts: &[&str], title: bool| Panel {
            entries: texts
                .iter()
                .map(|t| Entry { top: 0, bottom: 0, colour: BulletColour::White, text: (*t).to_string(), cut: false, lookup: Lookup::Unknown, user: None })
                .collect(),
            title,
        };
        let english_in_chinese = panel(&["Rare monsters inflict a stackofSlow effect ontargetswithin 8 m ofthem for3severy2s.", "Gets Weakened for 8 s ifyou ave movedmore than 17 m recently"], true);
        assert_eq!(words_language(&english_in_chinese, Lang::ZhTw), Some(Lang::En));
        let chinese = panel(&["關 卡 內 新 增 1 組 攻 擊 加 速 的 災 燼 觸 槍", "玩 家 額 外 -30% 物 理 傷 害"], true);
        assert_eq!(words_language(&chinese, Lang::ZhTw), Some(Lang::ZhTw));
        let chinese_in_english = panel(&["", "+23%", "", "-150/0"], true);
        assert_eq!(words_language(&chinese_in_english, Lang::En), Some(Lang::ZhTw));
        assert_eq!(words_language(&panel(&["", "+23%"], false), Lang::En), None, "no title bar: another screen");
        let english_screen = panel(&["Rare monsters inflict a stackofSlow effect ontargetswithin 8 m ofthem for3severy2s."], false);
        assert_eq!(words_language(&english_screen, Lang::ZhTw), None, "no title bar: another screen, English or not");
        let english = panel(&["When Magic monsters deal damage to a target, the target additionally -23% Life Regeneration Speed for 6 s"], true);
        assert_eq!(words_language(&english, Lang::En), Some(Lang::En));
        assert_eq!(words_language(&panel(&["+30%"], true), Lang::ZhTw), None, "too few words");
        assert_eq!(words_language(&panel(&[], true), Lang::En), None, "no entry");
    }

    const BLUE: [u8; 4] = [40, 90, 200, 255];

    fn blue_run(img: &mut Rgba, y: usize, x0: usize, x1: usize) {
        for x in x0..x1 {
            let i = (y * img.w + x) * 4;
            img.data[i..i + 4].copy_from_slice(&BLUE);
        }
    }

    fn painted(img: &Rgba, y: usize, x: usize) -> bool {
        let i = (y * img.w + x) * 4;
        img.data[i..i + 3] == PANEL_GROUND
    }

    /// A long blue run with nothing blue below is an underline and is painted over; a short run (part of a letter)
    /// is not.
    #[test]
    fn underlines_are_painted_over() {
        let mut img = Rgba { w: 80, h: 20, data: vec![0; 80 * 20 * 4] };
        blue_run(&mut img, 0, 0, 70);
        blue_run(&mut img, 1, 0, 70);
        // a letter of the next text line, further down than UNDERLINE_CLEAR
        blue_run(&mut img, 16, 0, 20);
        paint_underlines(&mut img, 60);
        assert!(painted(&img, 0, 5) && painted(&img, 1, 69), "the underline, both rows of it");
        assert!(!painted(&img, 16, 5), "a letter's short run");
    }

    /// A keyword's long strokes stay: its top stroke has the rest of the word below it, its lowest stroke the
    /// underline a few rows down; only the underline goes (凍僵 read as 冫 when its strokes were painted too).
    #[test]
    fn a_keywords_own_strokes_are_not_painted() {
        let mut img = Rgba { w: 100, h: 40, data: vec![0; 100 * 40 * 4] };
        // a word: top stroke, two upright strokes, lowest stroke; then the underline
        blue_run(&mut img, 2, 5, 85);
        blue_run(&mut img, 3, 5, 85);
        for y in 4..12 {
            blue_run(&mut img, y, 10, 14);
            blue_run(&mut img, y, 60, 64);
        }
        blue_run(&mut img, 12, 5, 85);
        blue_run(&mut img, 18, 2, 90);
        blue_run(&mut img, 19, 2, 90);
        paint_underlines(&mut img, 60);
        assert!(!painted(&img, 2, 30) && !painted(&img, 3, 30), "the top stroke");
        assert!(!painted(&img, 12, 30), "the lowest stroke");
        assert!(!painted(&img, 6, 11), "an upright stroke");
        assert!(painted(&img, 18, 30) && painted(&img, 19, 89), "the underline");
    }

    /// The faint cyan edges the upscale leaves around an underline go too: one row above it, two below; not the row
    /// two above (a stroke of 祝福 there), nor a white, red or bright cyan (a stroke's) pixel in an edge row.
    #[test]
    fn an_underlines_faint_edges_are_painted_over() {
        let mut img = Rgba { w: 100, h: 30, data: vec![0; 100 * 30 * 4] };
        let faint = [34, 99, 109, 255];
        let set = |img: &mut Rgba, y: usize, x: usize, p: [u8; 4]| {
            let i = (y * img.w + x) * 4;
            img.data[i..i + 4].copy_from_slice(&p);
        };
        for x in 5..90 {
            for y in [8, 9, 13, 14, 15] {
                set(&mut img, y, x, faint);
            }
        }
        blue_run(&mut img, 10, 5, 90);
        blue_run(&mut img, 11, 5, 90);
        blue_run(&mut img, 12, 5, 90);
        set(&mut img, 9, 40, [200, 200, 200, 255]);
        set(&mut img, 13, 41, [200, 60, 50, 255]);
        set(&mut img, 9, 60, [60, 170, 200, 255]);
        paint_underlines(&mut img, 60);
        assert!(!painted(&img, 9, 60), "a stroke's bright cyan above the line stays");
        assert!((10..=12).all(|y| painted(&img, y, 50)), "the underline");
        assert!(painted(&img, 9, 50), "the edge above");
        assert!(painted(&img, 13, 50) && painted(&img, 14, 50), "the edge below");
        assert!(!painted(&img, 8, 50), "two rows above: left");
        assert!(!painted(&img, 15, 50), "three rows below: left");
        assert!(!painted(&img, 9, 40) && !painted(&img, 13, 41), "white and red pixels stay");
    }

    #[test]
    fn pieces_of_one_row_read_left_to_right() {
        let l = |text: &str, x: i32, middle: f64| OcrLine { text: text.into(), x, middle, bottom: 0 };
        let mut lines = vec![l("next row", 0, 540.0), l("s. This effect", 900, 509.0), l("damage for 3", 0, 510.5)];
        reading_order(&mut lines);
        let order: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(order, ["damage for 3", "s. This effect", "next row"]);
    }

    #[test]
    fn cubic_upscale_keeps_flat_colour() {
        let img = Rgba { w: 4, h: 3, data: [10u8, 120, 200, 255].repeat(12) };
        let big = upscale_cubic(&img, 3);
        assert_eq!((big.w, big.h), (12, 9));
        assert!(big.data.as_chunks::<4>().0.iter().all(|p| *p == [10, 120, 200, 255]));
    }

    /// The bullets' heights and distances tell the interface scale the reader was measured at (10 to 14 reference
    /// pixels high, one-line entries 32 apart) from another one (0.8: about 9 high, 26 apart; 1.5: about 17 high).
    #[test]
    fn the_bullets_tell_the_interface_scale() {
        let marks = |hs: &[u32], gap: u32| -> Vec<Bullet> {
            hs.iter().enumerate().map(|(i, &h)| Bullet { top: 500 + i as u32 * gap, bottom: 500 + i as u32 * gap + h, colour: BulletColour::White }).collect()
        };
        assert!(scale_fits(&marks(&[11, 12, 12, 11, 12, 8], 64)), "the measured scale, one bullet cut short");
        assert!(scale_fits(&marks(&[8, 12], 64)), "two entries kept, the first cut short");
        assert!(scale_fits(&marks(&[11, 11, 12], 32)), "one-line entries");
        assert!(scale_fits(&marks(&[10, 10, 11, 10], 34)), "1080p");
        assert!(scale_fits(&marks(&[13, 14, 14], 28)), "the panel brighter");
        assert!(!scale_fits(&marks(&[9, 9, 10, 9], 52)), "0.8");
        assert!(scale_fits(&marks(&[8, 9, 12, 12, 12, 12], 64)), "two short (a screenshot's panel)");
        assert!(!scale_fits(&marks(&[9, 13, 13, 13], 26)), "0.8: text taken for bullets, closer than one line");
        assert!(!scale_fits(&marks(&[17, 16, 16], 96)), "1.5, or not a panel");
        assert!(!scale_fits(&[]), "no bullet");
    }

    /// Frames whose height times a whole upscale gives the reference's upscaled height are upscaled straight from
    /// the frame; the reference's own height, and others, go through the reference as before.
    #[test]
    fn some_heights_upscale_straight_from_the_frame() {
        assert_eq!(native_scale(1080), Some(4));
        assert_eq!(native_scale(2160), Some(2));
        assert_eq!(native_scale(1440), None);
        assert_eq!(native_scale(1200), None, "1200 x 3.6");
        assert_eq!(native_scale(0), None);
    }

    /// Chinese keywords cut by a line's end or misread by a character are put back; two-character ones, keywords
    /// already right, texts without one, and ties are left alone.
    #[test]
    fn chinese_keywords_are_put_back() {
        assert_eq!(fix_keywords("稀 有 怪 物 擊 中 時 , 在 5 秒 內 減 少 目 標 3 層 棸能 祝 福 、 靈 動 祝 福"), "稀有怪物擊中時,在5秒內減少目標3層聚能祝福、靈動祝福");
        assert_eq!(fix_keywords("和 靈 動 祝,福 和 堅 韌 祝 福"), "和靈動祝福和堅韌祝福");
        // a lookalike of the game's character in the keyword (1080p, 2026-11-10): 棸能褔 and 棸能福 are 聚能祝福
        assert_eq!(fix_keywords("玩家每獲得3靈動祝福或棸能福,失去2層該祝福"), "玩家每獲得3靈動祝福或聚能祝福,失去2層該祝福");
        assert_eq!(fix_keywords("玩家每獲得3靈動祝福或棸能褔,失去2層該祝福"), "玩家每獲得3靈動祝福或聚能祝福,失去2層該祝福");
        // right already: as it was, spaces and all
        assert_eq!(fix_keywords("+100% 掉 落 數 量"), "+100% 掉 落 數 量");
        // two characters: left (致殘 is another keyword)
        assert_eq!(fix_keywords("施 加 致 殘 , 持 續 5 秒"), "施 加 致 殘 , 持 續 5 秒");
        assert_eq!(fix_keywords("玩 家 受 到 致 目 效 果 時"), "玩 家 受 到 致 目 效 果 時");
        assert_eq!(fix_keywords("怪 物 額 外 一 40 % 受 到 的 詛 咒 效 果"), "怪 物 額 外 一 40 % 受 到 的 詛 咒 效 果");
        // two keywords put back in one text, the later one first in the list and shorter or longer than read: both
        assert_eq!(fix_keywords("靈 動 祝,福 和 棸能 祝 福"), "靈動祝福和聚能祝福");
        assert_eq!(fix_keywords("棸能 祝 福 和 靈 動 祝,福 和 堅 韌 祝 福"), "聚能祝福和靈動祝福和堅韌祝福");
        // 迷踪效果 (迷踪, a keyword of its own, then 效果) is not 迷踪值 (2026-11-09, sample-0004 of 18-03-54)
        assert_eq!(fix_keywords("玩 家 一 25 % 迷 踪 效 果"), "玩 家 一 25 % 迷 踪 效 果");
    }

    #[test]
    fn join_drops_bullet_text() {
        assert_eq!(join(["• At Full Life, you deal", "damage"].into_iter(), false), "At Full Life, you deal damage");
        assert_eq!(join(["稀有怪物擊中時 ,", "最多疊加5層"].into_iter(), true), "稀有怪物擊中時 ,最多疊加5層");
    }

    /// The upscale as it was on one thread (until 2026-11-09), kept to check the threaded one against.
    fn upscale_one_thread(img: &Rgba, f: usize) -> Rgba {
        let (w, h) = (img.w, img.h);
        let (ow, oh) = (w * f, h * f);
        let cols: Vec<[(usize, f32); 4]> = (0..ow).map(|x| cubic_taps(x, w, f)).collect();
        let rows: Vec<[(usize, f32); 4]> = (0..oh).map(|y| cubic_taps(y, h, f)).collect();
        let mut mid = vec![0f32; ow * h * 3];
        for y in 0..h {
            for (x, tap) in cols.iter().enumerate() {
                for c in 0..3 {
                    mid[(y * ow + x) * 3 + c] = tap.iter().map(|&(sx, wt)| img.data[(y * w + sx) * 4 + c] as f32 * wt).sum();
                }
            }
        }
        let mut data = vec![255u8; ow * oh * 4];
        for (y, tap) in rows.iter().enumerate() {
            for x in 0..ow {
                for c in 0..3 {
                    let v: f32 = tap.iter().map(|&(sy, wt)| mid[(sy * ow + x) * 3 + c] * wt).sum();
                    data[(y * ow + x) * 4 + c] = v.round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        Rgba { w: ow, h: oh, data }
    }

    /// The threaded upscale gives the same image, byte for byte, as on one thread: small, odd and panel-sized images
    /// (the panel's text area is 520x610), at 2x and 3x.
    #[test]
    fn parallel_upscale_matches_one_thread() {
        let mut seed = 0x2545_f491_u32;
        let mut noise = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    (seed >> 16) as u8
                })
                .collect()
        };
        for (w, h) in [(1, 1), (2, 3), (37, 23), (520, 61), (520, 610)] {
            let img = Rgba { w, h, data: noise(w * h * 4) };
            for f in [2, 3] {
                let (a, b) = (upscale_cubic(&img, f), upscale_one_thread(&img, f));
                assert_eq!((a.w, a.h), (b.w, b.h), "{w}x{h} at {f}x");
                assert!(a.data == b.data, "{w}x{h} at {f}x: the threaded upscale differs");
            }
        }
        assert!(upscale_cubic(&Rgba { w: 0, h: 5, data: Vec::new() }, 3).data.is_empty());
    }

    /// The time of the panel's 3x upscale on one thread and threaded (ignored: a measurement, not a check).
    #[test]
    #[ignore]
    fn upscale_time() {
        let img = Rgba { w: 520, h: 610, data: (0..520 * 610 * 4).map(|i| (i * 7 % 251) as u8).collect() };
        let time = |f: &dyn Fn() -> Rgba| {
            let _ = f();
            let t = std::time::Instant::now();
            for _ in 0..10 {
                let _ = f();
            }
            t.elapsed().as_secs_f64() * 100.0
        };
        let one = time(&|| upscale_one_thread(&img, 3));
        let threaded = time(&|| upscale_cubic(&img, 3));
        println!("520x610 at 3x: one thread {one:.1} ms, threaded {threaded:.1} ms");
    }
}
