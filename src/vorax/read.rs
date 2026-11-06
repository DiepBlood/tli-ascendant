//! Reading the Vorax screen: turns one screenshot into a [`Reading`].
//!
//! All coordinates are based on 2560x1440 (measured in section 5 of the first live-play notes (2026-11-03, private), then
//! calibrated against the 55 frames in `trainingdata/vorax/frames`); other sizes are first scaled by height
//! and centered horizontally.
//! - Text (choice title, card names, the type text at the bottom of a card, effect text, monster names, result
//!   screen text) is read with Windows OCR (Traditional Chinese) and then matched against the name tables
//!   ([`crate::reader::panels::similarity`]).
//! - Numbers (a tank's total activity and "activity x count", round, final activity, next reward tier, reroll
//!   count) use glyph templates ([`super::digits`]), because OCR often drops these small digits.
//! - Rarity comes from the colour of the monster name, archetype from the colour of the small icon above the name,
//!   and held instruments from icon templates in the top right.
//!
//! Anything that cannot be read or matched is marked "unknown" rather than forced: a card name must match the
//! name table almost character for character, or the visible part must match only one card and be backed by
//! occlusion or a tooltip; for a card with an archetype suffix (for example `細肢藥粉-蠱蟲`, "Slender-Limb Powder - Pest"),
//! when the suffix was not read, the effect text must print the same archetype.

use super::digits::{self, Glyph, Ink, Template};
use super::icons;
use super::names::{CardName, Category, archetype_zh, names};
use super::scale::{self, Frame};
use super::templates;
use crate::reader::ocr::{Line, ocr_all};
use crate::reader::panels::{similarity, squash};
use crate::reader::{Rect, Rgba};
use serde::Serialize;
use vorax::data::PotionRarity;
use vorax::{ARCHETYPES, Archetype, Rarity};

/// Reference screen size: all coordinates are based on 2560x1440.
pub const W: usize = 2560;
pub const H: usize = 1440;

/// Center x of the six tank labels (left to right; the tanks are in 3D perspective, so the spacing is uneven).
pub const TANK_X: [i32; 6] = [524, 881, 1232, 1579, 1941, 2312];

/// Screen kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// Choose a potion (the choice after opening a potion kit looks the same and cannot be told apart from a
    /// single screenshot, so it counts as this kind too).
    Potion,
    /// Choose an instrument.
    Instrument,
    /// Choose a protocol.
    Protocol,
    /// Tank screen that cannot be operated: resolve animations such as a potion taking effect or an instrument
    /// triggering.
    Resolving,
    /// Result screen (final activity, start surgery).
    Result,
    /// Not a Vorax board: the intro page, the endgame battle, other screens.
    Other,
}

impl Kind {
    /// A choice screen that can be operated.
    pub fn is_choice(self) -> bool {
        matches!(self, Kind::Potion | Kind::Instrument | Kind::Protocol)
    }

    pub fn category(self) -> Option<Category> {
        match self {
            Kind::Potion => Some(Category::Potion),
            Kind::Instrument => Some(Category::Instrument),
            Kind::Protocol => Some(Category::Protocol),
            _ => None,
        }
    }
}

/// One tank.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tank {
    pub empty: bool,
    /// Monster name: the Traditional Chinese name from the name table when it matches, the raw OCR text otherwise.
    pub name: Option<String>,
    pub archetype: Option<Archetype>,
    pub rarity: Option<Rarity>,
    pub activity: Option<i64>,
    pub quantity: Option<i64>,
    pub total: Option<i64>,
    /// Raw OCR text of the name.
    pub raw_name: String,
    /// The characters read from the two rows, total activity and "activity x count" (`?` is an unrecognized glyph).
    pub raw_total: String,
    pub raw_values: String,
    /// The name's representative colour and what the rarity decision was based on (for the log, to trace later why the rarity is sometimes read and
    /// sometimes not).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colour: Option<ColourRead>,
    /// The values were repaired by back-deriving from "activity × count = total activity": which field was
    /// fixed, the original text and the result (see [`tank_numbers`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repaired: Option<String>,
}

/// How a name's representative colour was judged to be a rarity: RGB, which rule fired (`close` the original
/// rule, `vivid` the relaxed rule, `none` neither passed), the nearest and second nearest reference colours and
/// their distances.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColourRead {
    pub rgb: [u8; 3],
    pub rule: &'static str,
    pub nearest: Rarity,
    pub distance: f32,
    pub second: Rarity,
    pub second_distance: f32,
}

impl Tank {
    /// Activity × count = total activity.
    pub fn consistent(&self) -> bool {
        matches!((self.activity, self.quantity, self.total), (Some(a), Some(q), Some(t)) if a * q == t)
    }

    /// Everything the solver needs was read: an empty tank, or archetype, rarity, activity and count all present
    /// and self-consistent.
    pub fn known(&self) -> bool {
        self.empty || (self.archetype.is_some() && self.rarity.is_some() && self.consistent())
    }
}

/// How a card name was matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum How {
    /// The card name matches almost character for character.
    Name,
    /// The card name is covered and the visible part matches only this one card (backed by an occluded
    /// position, a tooltip or the character count).
    Partial,
    /// The card name could not be read; the card name from the left tooltip.
    Tooltip,
    /// The card name could not be read; the card name at the start of the effect text (the effect text's area
    /// reaches up to the card-name line).
    Effect,
    /// The card name could not be read on this frame; the live session took it from the last steady read of the
    /// same hand with every name read (`carry_names` in `crate::app::vorax_session`). Never set by a read alone.
    Carried,
}

/// One choice card.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    pub category: Category,
    /// Position in the name table ([`names`]`().cards`); None means unknown.
    #[serde(skip)]
    pub index: Option<usize>,
    /// The solver's id (potions, instruments).
    pub id: Option<u8>,
    pub en: Option<String>,
    pub zh: Option<String>,
    pub how: Option<How>,
    /// Raw OCR text of the card name, the type text and the effect text.
    pub raw_name: String,
    pub raw_type: String,
    pub raw_effect: String,
    /// Why it could not be recognized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// Center x of the card (reference coordinates; the position of the type text or the default position).
    pub x: i32,
    /// How many pixels higher (negative) or lower the type text is than on a card that is not raised: a raised
    /// card is about -40.
    pub dy: i32,
    /// The card is raised (the player clicked it and has not confirmed yet).
    pub raised: bool,
}

impl Card {
    pub fn name(&self) -> Option<&'static CardName> {
        self.index.map(|i| &names().cards[i])
    }
}

/// One slot of a held instrument icon.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    /// The solver's id; None is an unrecognized icon.
    pub id: Option<u8>,
    pub en: Option<String>,
    pub zh: Option<String>,
    /// An instrument revealed on the intro page (for example `蒸骨坩堝`): it only changes the
    /// initial board, has no effect once the game starts, and is not passed to the solver.
    pub opening: bool,
    /// Correlation coefficient with the closest template (1 is identical).
    pub score: f32,
}

/// What was read from one screenshot.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading {
    pub kind: Kind,
    /// The left panel's round N (N/13).
    pub round: Option<i32>,
    /// The left panel's final activity (None when covered by a tooltip); on the result screen, the final
    /// activity at the top of the screen.
    pub ultimate: Option<i64>,
    /// The green "+N" at the upper right of the final activity is still there (the value has not updated yet).
    pub ultimate_pending: bool,
    /// There is a tooltip on the left (the mouse is on a card or monster) covering the lower half of the panel.
    pub panel_covered: bool,
    pub next_reward: Option<i64>,
    /// Remaining count on the reroll button (potion and instrument screens; the protocol screen has no reroll
    /// button).
    pub rerolls: Option<i32>,
    pub tanks: Vec<Tank>,
    /// Held instruments at the top right, left to right (order of acquisition).
    pub held: Vec<Held>,
    pub cards: Vec<Card>,
    /// Sum of all tanks' total activity (when every slot was read).
    pub tank_sum: Option<i64>,
    /// Places that were read but do not add up, or could not be recognized.
    pub problems: Vec<String>,
    /// Raw OCR text of the left tooltip's card name (mostly empty when there is no tooltip).
    pub tooltip: String,
}

/// Convert to the reference size: scale by height; when the width differs, center and crop or pad with black
/// bars. A frame taller than 1440 is shrunk by area averaging, a shorter one enlarged bilinearly; a 2560x1440
/// frame comes back as it is. Read the result with [`read_with_frame`] and the frame, so the OCR input of a
/// frame not 1440 high is cut from the frame itself (see [`super::scale`]).
pub fn to_reference(img: &Rgba) -> Rgba {
    if img.w == W && img.h == H {
        return img.clone();
    }
    rescale(img)
}

/// The scaling of [`to_reference`].
fn rescale(img: &Rgba) -> Rgba {
    let w = scale::scaled_width(img);
    let scaled = if img.h > H { scale::shrink(img, w, H) } else { img.resize(w, H) };
    if w == W {
        return scaled;
    }
    let mut data = vec![0u8; W * H * 4];
    let shift = (w as i64 - W as i64) / 2;
    for (y, row) in data.as_chunks_mut::<{ W * 4 }>().0.iter_mut().enumerate() {
        for x in 0..W {
            let sx = x as i64 + shift;
            if (0..w as i64).contains(&sx) {
                let i = (y * w + sx as usize) * 4;
                row[x * 4..x * 4 + 4].copy_from_slice(&scaled.data[i..i + 4]);
            }
        }
    }
    Rgba { w: W, h: H, data }
}

/// The inverse of [`to_reference`]: converts a point and half width/height in reference coordinates (2560x1440)
/// to pixels in a `w`x`h` frame. Scales by height `k = h / 1440`; when the width differs, centers horizontally
/// (`shift` is half of the width cropped or padded, in reference coordinates).
pub fn to_frame(w: usize, h: usize, (x, y, half_w, half_h): (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
    let k = h as f32 / H as f32;
    // Like to_reference, round the scaled width to an integer first.
    let scaled_w = (w as f32 * H as f32 / h as f32).round();
    let shift = ((scaled_w - W as f32) / 2.0).trunc();
    ((x + shift) * k, y * k, half_w * k, half_h * k)
}

/// Hand several cropped images (each with its screen position) to OCR together; line positions are converted
/// back to screen coordinates.
fn ocr_images(crops: &[(Rect, Rgba, f32)]) -> Vec<Vec<Line>> {
    const N: usize = 12;
    let empty = Rgba { w: 0, h: 0, data: vec![] };
    let mut out = Vec::with_capacity(crops.len());
    for chunk in crops.chunks(N) {
        let imgs: [&Rgba; N] = std::array::from_fn(|i| chunk.get(i).map_or(&empty, |c| &c.1));
        let regions: [(Rect, f32); N] = std::array::from_fn(|i| chunk.get(i).map_or((Rect::default(), 1.0), |c| (Rect::new(0, 0, c.1.w as i32, c.1.h as i32), c.2)));
        for (lines, (r, _, _)) in ocr_all(imgs, &regions).into_iter().zip(chunk) {
            out.push(lines.into_iter().map(|l| Line { rect: Rect::new(l.rect.x + r.x, l.rect.y + r.y, l.rect.w, l.rect.h), text: l.text }).collect());
        }
    }
    out
}

/// How a card or panel crop is prepared for OCR: as it is, or turned black on white at a percentile (see
/// [`inked`]).
#[derive(Clone, Copy)]
enum Prep {
    Plain,
    Inked(usize),
}

/// Where the OCR reads text: the reference image, and for a frame not 1440 high the frame itself (see
/// [`super::scale`]).
struct Text<'a> {
    img: &'a Rgba,
    frame: Option<Frame<'a>>,
}

impl Text<'_> {
    /// The frame is drawn at another interface scale (see [`super::ui_scale`]).
    fn ui(&self) -> bool {
        self.frame.as_ref().is_some_and(Frame::is_ui)
    }

    /// [`ocr_batch`] of the reference; from the frame when there is one.
    fn batch(&self, regions: &[(Rect, f32)]) -> Vec<Vec<Line>> {
        match &self.frame {
            None => ocr_batch(self.img, regions),
            Some(f) => ocr_frame(f, &regions.iter().map(|&(r, s)| (r, Prep::Plain, s)).collect::<Vec<_>>()),
        }
    }

    /// [`digits::text_glyphs`] of the reference; for a frame lower than 1440 (1080p), of the frame resampled
    /// once at the reference size ([`Frame::digits`]), the glyph boxes in reference coordinates as well. A
    /// frame taller than 1440 keeps the reference: shrinking it is area averaged ([`to_reference`]), while the
    /// one-pass resample at reference size would shrink by 1.5 with no prefilter and alias. A frame at another
    /// interface scale keeps the reference as well (unscaled, with the tank labels boxed, see
    /// [`super::ui_scale::unscale`]).
    fn glyphs(&self, r: Rect, min_h: i32, kind: Ink) -> Vec<Glyph> {
        let Some(f) = self.frame.as_ref().filter(|f| f.enlarges() && !f.is_ui()) else { return digits::text_glyphs(self.img, r, min_h, kind) };
        let crop = f.digits(r);
        let mut gs = digits::text_glyphs(&crop, Rect::new(0, 0, crop.w as i32, crop.h as i32), min_h, kind);
        for g in &mut gs {
            (g.x0, g.x1, g.y0, g.y1) = (g.x0 + r.x, g.x1 + r.x, g.y0 + r.y, g.y1 + r.y);
        }
        gs
    }

    /// [`ocr_images`] of crops of the reference; from the frame when there is one.
    fn crops(&self, crops: &[(Rect, Prep, f32)]) -> Vec<Vec<Line>> {
        match &self.frame {
            None => ocr_images(
                &crops
                    .iter()
                    .map(|&(r, p, s)| {
                        let c = match p {
                            Prep::Plain => self.img.crop(r),
                            Prep::Inked(q) => inked(self.img, r, q),
                        };
                        (r, c, s)
                    })
                    .collect::<Vec<_>>(),
            ),
            Some(f) => ocr_frame(f, crops),
        }
    }
}

/// OCR of regions cut from a frame not 1440 high: each region is resampled once from the frame to the size the
/// 1440 read upscales it to times [`scale::MAG`], plain crops sharpened ([`Frame::text`]), inked ones
/// turned black on white from the unsharpened resample. Line boxes come back in reference coordinates.
fn ocr_frame(f: &Frame, crops: &[(Rect, Prep, f32)]) -> Vec<Vec<Line>> {
    let mag = f.mag();
    // Read at scale 1 (each input already has its size), then map the boxes from input pixels back.
    let inputs: Vec<(Rect, Rgba, f32)> = std::thread::scope(|s| {
        let jobs: Vec<_> = crops
            .iter()
            .map(|&(r, p, k)| {
                s.spawn(move || {
                    let c = f.text(r, k * mag, matches!(p, Prep::Plain));
                    (Rect::default(), if let Prep::Inked(q) = p { ink(&c, q) } else { c }, 1.0)
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().expect("ocr_frame")).collect()
    });
    ocr_images(&inputs)
        .into_iter()
        .zip(crops)
        .map(|(lines, &(r, _, k))| {
            let m = k * mag;
            let at = |v: i32| (v as f32 / m).round() as i32;
            lines.into_iter().map(|l| Line { rect: Rect::new(r.x + at(l.rect.x), r.y + at(l.rect.y), at(l.rect.w), at(l.rect.h)), text: l.text }).collect()
        })
        .collect()
}

/// Hand several regions to OCR together; the result order is the same as `regions`.
fn ocr_batch(img: &Rgba, regions: &[(Rect, f32)]) -> Vec<Vec<Line>> {
    const N: usize = 4;
    let mut out = Vec::with_capacity(regions.len());
    for chunk in regions.chunks(N) {
        let mut arr = [(Rect::default(), 1.0f32); N];
        arr[..chunk.len()].copy_from_slice(chunk);
        out.extend(ocr_all([img; N], &arr).into_iter().take(chunk.len()));
    }
    out
}

/// Join the lines top to bottom, left to right.
fn joined(lines: &[Line]) -> String {
    let mut ls: Vec<&Line> = lines.iter().collect();
    ls.sort_by_key(|l| (l.rect.y / 12, l.rect.x));
    ls.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ")
}

/// Swap common OCR misreads back, then squash (keep only Chinese characters and alphanumerics): `蠱` is
/// really `蟲` and `甩` is really `用`.
pub fn norm(s: &str) -> String {
    // The full-width Roman numerals U+2160..U+2162 (the three Dissection cards) are folded to ASCII before squash,
    // which would otherwise drop them; no other name in the tables contains them.
    let folded: String = s.chars().flat_map(|c| match c {
        '\u{2160}' => "I".chars().collect::<Vec<_>>(),
        '\u{2161}' => "II".chars().collect(),
        '\u{2162}' => "III".chars().collect(),
        c => vec![c],
    }).collect();
    let out: String = squash(&folded).chars().map(|c| match c {
        '蠱' => '蟲',
        '甩' => '用',
        // OCR always reads `嗅` in `驚醒嗅鹽` as `臭` (2026-11-04 ranked game); no other character in the
        // name tables (card names, monster names) contains `嗅` or `臭`.
        '嗅' => '臭',
        c => c,
    })
    .collect();
    // Right after `解剖術` (the Dissection cards) the numeral `I`/`II`/`III` is often read as `l` or `1`: fold that
    // run to `i`. No other name in the tables contains `解剖術`, so nothing else changes.
    const DISSECTION: &str = "解剖術";
    match out.find(DISSECTION) {
        Some(at) => {
            let (head, tail) = out.split_at(at + DISSECTION.len());
            let run = tail.chars().take_while(|c| matches!(c, 'i' | 'l' | '1')).count();
            format!("{head}{}{}", "i".repeat(run), &tail[run..])
        }
        None => out,
    }
}

// ---------------------------------------------------------------------------- regions

/// The choice title, `選擇一種藥劑／手術用具／手術方案` ("choose a potion / instrument / protocol"): OCR is
/// very sensitive to the crop, so read once with each of two crops and merge.
const TITLE: Rect = Rect::new(1150, 898, 540, 48);
const TITLE_TIGHT: Rect = Rect::new(1200, 900, 440, 44);
/// On the result screen: the "final activity" label and value at the top, and the "end surgery" and "start
/// surgery" buttons at the bottom.
const RESULT_LABEL: Rect = Rect::new(1150, 85, 260, 45);
/// A wider crop of the label, read only when the first one missed it but both buttons were read: on the saved
/// result screen of 2026-11-06_13-46-25 (frame-0062) [`RESULT_LABEL`] read nothing and this one `一最活性`.
const RESULT_LABEL_WIDE: Rect = Rect::new(1100, 80, 360, 50);
/// The value is centred on x 1280 and grows both ways with its digits: six digits span x 1204..1356, seven
/// starting with 1 span 1195..1366 (2026-11-06, six result screens), so the crop leaves room for seven wide ones.
const RESULT_VALUE: Rect = Rect::new(1150, 128, 260, 44);
const RESULT_BUTTONS: Rect = Rect::new(950, 1320, 680, 75);
/// Left panel: round (large digits and "/13"), final activity, the "next reward tier" label and its value.
const ROUND: Rect = Rect::new(100, 495, 88, 90);
const ROUND_OF: Rect = Rect::new(186, 495, 80, 90);
const ULTIMATE: Rect = Rect::new(100, 644, 200, 28);
const ULTIMATE_DELTA: Rect = Rect::new(100, 612, 200, 30);
const REWARD: Rect = Rect::new(100, 744, 200, 28);
/// Reroll count: for potions at x≈1634, for instruments (with Shirley's portrait added on the left) at x≈1683.
const REROLL: Rect = Rect::new(1615, 1345, 80, 30);
/// The row of type text at the bottom of the cards (used to find each card's position); the selected card
/// floats up about 40 pixels, and the band covers that too.
/// The five fanned cards of a potion kit (large) run from x≈760 to 2050.
const TYPE_BAND: Rect = Rect::new(760, 1232, 1320, 80);
/// Center y of the type text on a card that is not raised.
const TYPE_Y: i32 = 1291;
/// The area to read the type text again when only four of the five fanned cards of a potion kit (large) were
/// found: wider and taller than [`TYPE_BAND`]. The cards at the two ends of the fan are slanted, and Windows
/// OCR often misses one of them when reading the whole strip; a different crop often catches it (in the
/// 5-card fan frames of the 2026-11-03 ranked game and the 22:24 game, all five were found after merging the
/// two reads).
const FAN_BAND: Rect = Rect::new(600, 1200, 1640, 140);
/// Spacing between adjacent fanned cards: 206-218 (mean 212) when the mouse is not on a card; with the mouse on
/// one card, the cards on either side of it are each pushed out about 47, so that gap becomes about 255.
const FAN_STEP: i32 = 212;
/// Center x of the middle card of the fan: 1344; it moves to 1297 or 1391 when the mouse is on a card in the
/// left half or right half.
const FAN_MID: std::ops::RangeInclusive<i32> = 1290..=1400;
/// Center x of the middle card of the fan when the mouse is not on a card (used by [`fan_closer`] to compare
/// positions).
const FAN_MID_X: i32 = 1344;
/// The card name in the left tooltip (when the mouse is on a card).
const TOOLTIP_NAME: Rect = Rect::new(40, 845, 370, 55);
/// Default center x of the cards: three side by side (when no card is enlarged), and the single card at the
/// start of a tutorial game.
pub(crate) const CARD_X: [i32; 3] = [1205, 1416, 1629];

/// The monster-name row of tank i.
fn tank_name(i: usize) -> Rect {
    Rect::new(TANK_X[i] - 110, 503, 220, 32)
}

fn tank_total(i: usize) -> Rect {
    Rect::new(TANK_X[i] - 90, 530, 200, 24)
}

fn tank_values(i: usize) -> Rect {
    Rect::new(TANK_X[i] - 90, 554, 200, 23)
}

/// Archetype icon.
fn tank_icon(i: usize) -> Rect {
    Rect::new(TANK_X[i] - 12, 485, 24, 23)
}

/// The area where the card name sits: normally at y≈1100, and for a selected, raised card at y≈1062. A crop
/// that is too big gets the card art contrast-stretched along with it and the name becomes unreadable, so a
/// narrow strip is cropped at each of the two positions (`top` is one of [`NAME_TOPS`]).
fn card_name(x: i32, top: i32, half: i32) -> Rect {
    Rect::new(x - half, top, 2 * half, 50)
}

/// Top edges for the card-name crop: normal, raised.
const NAME_TOPS: [i32; 2] = [1083, 1043];
/// For the five fanned cards of a potion kit (large), the middle one is the highest, with its name at y≈1078,
/// and both the normal and the raised crops cut through the text (so in the 6 fan frames of 2026-11-03, 5 could
/// not read the middle card); with five cards, this position is read in addition.
const FAN_NAME_TOP: i32 = 1063;

/// The leftmost of the five fanned cards: the card leans left, so its name is low on the left and high on the
/// right and sits left of the card's center, and the normal crop cuts off the first character and the lower
/// edge of the left half (in the 2026-11-04 ranked game, `細肢藥粉-骨衛兵` was read as `肢一骨兵` and `粉一骨衛兵`,
/// and `孿生激素-蠱蟲` as `主激素蠱蟲`; 8 of 64 steady reads over two days). There is no neighbouring card on
/// the left, so crop [`FAN_LEFT_EXTRA`] more to the left, and shift down by half of the sag (`dy`) measured
/// from the type text.
fn fan_left_name(x: i32, top: i32, dy: i32, half: i32) -> Rect {
    // Coordinates are in the reference size (2560x1440, see [`to_reference`]); the left edge is clamped to 0
    // or above.
    let left = (x - half - FAN_LEFT_EXTRA).max(0);
    Rect::new(left, top + dy.max(0) / 2, x + half - left, 50)
}

/// Extra width cropped to the left of [`fan_left_name`] (about one and a half characters).
const FAN_LEFT_EXTRA: i32 = 34;

/// Effect text: covers both the normal and the raised positions (`dy` is the raised height read from the type
/// text).
fn card_effect(x: i32, dy: i32, half: i32) -> Rect {
    let top = (1095 + dy.min(0)).min(1095);
    Rect::new(x - half, top, 2 * half, 1275 - top)
}

/// A card whose type text is at least this much higher than usual counts as raised: raised is about 40 pixels.
const RAISED_DY: i32 = -20;

/// A card-name character is about 22.8 pixels wide (2560x1440).
const CHAR_W: f32 = 22.8;

// ---------------------------------------------------------------------------- digits

/// Convert a run of glyphs to characters; a glyph whose distance is too large or whose margin over the second
/// best is too small is recorded as `?`.
fn chars(gs: &[Glyph], templates: &[Template]) -> String {
    chars_within(gs, templates, 42.0)
}

/// [`chars`], with maximum distance `max`.
fn chars_within(gs: &[Glyph], templates: &[Template], max: f32) -> String {
    gs.iter()
        .map(|g| match digits::classify(g, templates) {
            Some((c, d, second)) if d <= max && second - d >= 6.0 => c,
            _ => '?',
        })
        .collect()
}

/// The value when the whole string is digits.
fn number(s: &str) -> Option<i64> {
    (!s.is_empty() && s.len() <= 9 && s.chars().all(|c| c.is_ascii_digit())).then(|| s.parse().ok())?
}

/// A tank's activity, count and total activity, plus the record of back-derived repairs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Numbers {
    pub activity: Option<i64>,
    pub quantity: Option<i64>,
    pub total: Option<i64>,
    pub repaired: Option<String>,
    /// The repair added a `1` to a total activity that was read cleanly yet is not self-consistent: it only
    /// counts once the tanks' sum matches the final activity (see [`read`], after the tanks are built).
    pub needs_sum: bool,
}

/// Maximum number of `?` in one field; together with the restored `1` (at most one), the most places repaired
/// in total.
const MAX_UNKNOWN: usize = 2;
const MAX_REPAIRS: usize = 2;

/// The values of the two rows ("activity x count" and total activity). If all three were read and are
/// self-consistent, use them as read; if some are missing or inconsistent, back-derive with
/// "activity × count = total activity":
/// - fix only one field; the other two must be read cleanly (all digits, not starting with 0, activity and
///   count ≥1) - when the product field is also misread, the unique solution may be wrong too;
/// - that field may only have `?` replaced by digits (at most 2), or have a thin "1" that was dropped as a
///   speck put back (at most 1), at most 2 places in total;
/// - adopt it only when exactly one way of repairing fits across the three fields; with several solutions or
///   none, use what was read (if inconsistent, that means unknown).
pub fn tank_numbers(raw_values: &str, raw_total: &str) -> Numbers {
    let parsed = raw_values.split_once('x').and_then(|(a, q)| Some((number(a)?, number(q)?)));
    let read = Numbers { activity: parsed.map(|v| v.0), quantity: parsed.map(|v| v.1), total: number(raw_total), repaired: None, needs_sum: false };
    if matches!((read.activity, read.quantity, read.total), (Some(a), Some(q), Some(t)) if a * q == t) {
        return read;
    }
    x_from_total(raw_values, raw_total).or_else(|| repair(raw_values, raw_total)).or_else(|| from_product(raw_values, raw_total)).unwrap_or(read)
}

/// "activity x count" read with the "x" unrecognized (`2?72`, the 1080p frames): no "x" and exactly one `?`.
/// With the total activity read cleanly (all digits), the `?` taken as the "x" must split the row into a clean
/// activity and count whose product is that total; marked in `repaired`. Any other row (an "x" read, no `?`
/// or several, a total with `?`) or a product that does not match is left to [`repair`] and the problems as
/// before. At 1440 the "x" is normally read; this comes into play on the frames where it is not (62 of 1895 saved
/// 1440 frames on 2026-11-06, every one with a clean total).
fn x_from_total(raw_values: &str, raw_total: &str) -> Option<Numbers> {
    if raw_values.contains('x') || raw_values.matches('?').count() != 1 || !raw_total.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let total = number(raw_total).filter(|&t| t >= 1 && !raw_total.starts_with('0'))?;
    let (a, q) = raw_values.split_once('?')?;
    let clean = |s: &str| number(s).filter(|&v| v >= 1 && !s.starts_with('0'));
    let (a, q) = (clean(a)?, clean(q)?);
    (a.checked_mul(q)? == total).then(|| Numbers {
        activity: Some(a),
        quantity: Some(q),
        total: Some(total),
        repaired: Some(format!("活性 x 數量「{raw_values}」→{a}x{q}（由總活性 {total} 判定）")),
        needs_sum: false,
    })
}

/// The last resort of [`tank_numbers`]: the total activity read with glyphs not recognized (`?`) while
/// "activity x count" was read cleanly, so the total is taken as their product and marked in `repaired`
/// (2026-11-06 1080p game, round 3 on: `赤螳螂` read as total `20?` with `10x204`, so 2040). The solver takes
/// only activity and count, and the total is their check, so the product counts only when it holds up:
/// - the field has at least one digit read, and lines up with the product from the left, digit for digit,
///   each `?` one digit, except that one `?` may stand for two (two glyphs stuck together): `20?` fits 2040,
///   `15?` does not fit 1152;
/// - like a "1" added to a total read cleanly, it waits for the tanks' sum to match the final activity
///   (`needs_sum`, see [`read_with_frame`]): when the final activity is covered or still jumping, or the sum
///   is off, the tank falls back to the values as read and its problem is reported.
///
/// A total read cleanly (all digits) that does not match the product is left alone and stays unknown, since
/// either row may be the misread one; [`repair`] already settles the cases where one field can be fixed from
/// the other two.
fn from_product(raw_values: &str, raw_total: &str) -> Option<Numbers> {
    let (a, q) = raw_values.split_once('x')?;
    let clean = |s: &str| number(s).filter(|&v| v >= 1 && !s.starts_with('0'));
    let (a, q) = (clean(a)?, clean(q)?);
    let total = a.checked_mul(q)?;
    let want = total.to_string();
    let seen = raw_total.as_bytes();
    if !seen.iter().all(|b| b.is_ascii_digit() || *b == b'?') || !seen.contains(&b'?') || !seen.iter().any(u8::is_ascii_digit) {
        return None;
    }
    let same = |s: &[u8], w: &[u8]| s.len() == w.len() && s.iter().zip(w).all(|(s, w)| *s == b'?' || s == w);
    let w = want.as_bytes();
    let fits = same(seen, w) || (seen.len() + 1 == w.len() && (0..seen.len()).any(|p| seen[p] == b'?' && same(&seen[..p], &w[..p]) && same(&seen[p + 1..], &w[p + 2..])));
    fits.then(|| Numbers { activity: Some(a), quantity: Some(q), total: Some(total), repaired: Some(format!("總活性「{raw_total}」→{total}（由活性×數量補上）")), needs_sum: true })
}

/// The back-derivation of [`tank_numbers`]; None when there is no unique solution.
fn repair(raw_values: &str, raw_total: &str) -> Option<Numbers> {
    let (a, q) = raw_values.split_once('x')?;
    let fields = [a, q, raw_total];
    const LABELS: [&str; 3] = ["活性", "數量", "總活性"];
    // A field that was read cleanly: all digits, not starting with 0, at least 1.
    let clean = |s: &str| number(s).filter(|&v| v >= 1 && !s.starts_with('0'));
    let mut found: Option<Numbers> = None;
    for k in 0..3 {
        let others: Vec<i64> = (0..3).filter(|&j| j != k).filter_map(|j| clean(fields[j])).collect();
        let [x, y] = others[..] else { continue };
        // What this field should be (the other two fields are x and y in order).
        let want = match k {
            0 | 1 if y % x == 0 => y / x,
            2 => x * y,
            _ => continue,
        };
        let Some(how) = fits(fields[k], &want.to_string()) else { continue };
        if want < 1 {
            continue;
        }
        // This field was read cleanly (all digits) yet needs a "1" added: all three fields look read
        // correctly and just do not multiply, so adding a 1 may be forcing the field that was read right
        // (true value 39×7=273 with the total read as 973 would be forced into 139×7). Not allowed for the
        // activity and count fields; allowed for the total activity field, but it only counts once the
        // tanks' sum matches the final activity. Adding a "1" to an empty field (the activity in "x36") is
        // not restricted.
        let clean_field = !fields[k].is_empty() && fields[k].bytes().all(|b| b.is_ascii_digit());
        if how == Fix::One && clean_field && k != 2 {
            continue;
        }
        let needs_sum = how == Fix::One && clean_field;
        let v = match k {
            0 => [want, x, y],
            1 => [x, want, y],
            _ => [x, y, want],
        };
        let n = Numbers { activity: Some(v[0]), quantity: Some(v[1]), total: Some(v[2]), repaired: Some(format!("{}「{}」→{want}", LABELS[k], fields[k])), needs_sum };
        match &found {
            Some(f) if (f.activity, f.quantity, f.total) != (n.activity, n.quantity, n.total) => return None,
            Some(_) => {}
            None => found = Some(n),
        }
    }
    found
}

/// How a field is repaired.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fix {
    /// Only `?` replaced by digits (or it was already identical).
    Unknowns,
    /// Added a "1" that had been dropped.
    One,
}

/// Whether a field `seen` as read (digits and `?`) can become `want` after repair: `?` replaced by digits, or
/// a "1" added.
fn fits(seen: &str, want: &str) -> Option<Fix> {
    if want.len() > 9 || !seen.chars().all(|c| c.is_ascii_digit() || c == '?') {
        return None;
    }
    let unknown = seen.chars().filter(|&c| c == '?').count();
    let same = |s: &[u8], w: &[u8]| s.len() == w.len() && s.iter().zip(w).all(|(s, w)| *s == b'?' || s == w);
    let (s, w) = (seen.as_bytes(), want.as_bytes());
    if unknown > MAX_UNKNOWN {
        return None;
    }
    if same(s, w) {
        return (unknown <= MAX_REPAIRS).then_some(Fix::Unknowns);
    }
    // Add a "1": it matches after removing one "1" from want.
    (unknown < MAX_REPAIRS && w.len() == s.len() + 1 && (0..w.len()).any(|i| w[i] == b'1' && same(s, &[&w[..i], &w[i + 1..]].concat()))).then_some(Fix::One)
}

/// "activity x count": the glyph that is shorter than the other digits is the "x".
fn values(text: &Text, r: Rect) -> String {
    let gs = text.glyphs(r, 9, Ink::Label);
    // The digit height: take the median (there is only one "x", and it is shorter than the digits).
    let mut hs: Vec<i32> = gs.iter().map(Glyph::h).collect();
    hs.sort_unstable();
    let Some(&hdigit) = hs.get(hs.len() / 2) else { return String::new() };
    gs.iter()
        .map(|g| {
            if g.h() * 10 < hdigit * 8 {
                // A short glyph can only be the "x"; confirm once more against the template.
                match digits::classify(g, templates::LABEL) {
                    Some(('x', d, _)) if d <= 50.0 => 'x',
                    _ => '?',
                }
            } else {
                chars(std::slice::from_ref(g), templates::LABEL).chars().next().map_or('?', |c| if c == 'x' { '?' } else { c })
            }
        })
        .collect()
}

/// The left panel's round: large digits followed by a small "/13".
fn round(text: &Text) -> Option<i32> {
    // The "/13" must be there: it is the main basis for recognizing the tank screen.
    if chars(&text.glyphs(ROUND_OF, 25, Ink::Plain), templates::SLASH) != "/13" {
        return None;
    }
    let big: Vec<Glyph> = text.glyphs(ROUND, 30, Ink::Plain).into_iter().filter(|g| g.h() >= 40).collect();
    number(&chars(&big, templates::ROUND)).map(|n| n as i32).filter(|n| (1..=14).contains(n))
}

/// A panel number: the glyph height must be within `heights` (excludes the tooltip's card art). At another
/// interface scale ([`Text::ui`]) see [`panel_digits`].
fn panel_number(text: &Text, r: Rect, min_h: i32, heights: std::ops::RangeInclusive<i32>, templates: &[Template]) -> Option<i64> {
    let gs = text.glyphs(r, min_h, Ink::Plain);
    if text.ui() {
        return panel_digits(gs, heights).and_then(|gs| number(&chars(&gs, templates)));
    }
    if gs.len() < 2 && gs.iter().all(|g| g.h() < *heights.start()) || gs.iter().any(|g| !heights.contains(&g.h())) {
        return None;
    }
    number(&chars(&gs, templates))
}

/// The digits of a panel number at another interface scale: the panel is translucent, and unscaled the 3D scene
/// behind it is not the one the original setting shows (a bright vertical glint of the tank glass stood right of
/// `3786` on the largest frames, and the whole number was given up for its height). Glyphs at either end of the
/// group whose height is outside `heights` are dropped; the rest must be at least two glyphs, all within
/// `heights` and of about one height (the digits of one number), else None.
fn panel_digits(mut gs: Vec<Glyph>, heights: std::ops::RangeInclusive<i32>) -> Option<Vec<Glyph>> {
    while gs.first().is_some_and(|g| !heights.contains(&g.h())) {
        gs.remove(0);
    }
    while gs.last().is_some_and(|g| !heights.contains(&g.h())) {
        gs.pop();
    }
    let (lo, hi) = (gs.iter().map(Glyph::h).min()?, gs.iter().map(Glyph::h).max()?);
    (gs.len() >= 2 && hi - lo <= 4 && gs.iter().all(|g| heights.contains(&g.h()))).then_some(gs)
}

// ---------------------------------------------------------------------------- colour

/// The representative colour of a monster name: the mean of the brightest strokes in the name row.
fn name_colour(img: &Rgba, r: Rect) -> Option<[f32; 3]> {
    let px: Vec<[u8; 3]> = (r.y..r.y + r.h).flat_map(|y| (r.x..r.x + r.w).map(move |x| (x, y))).map(|(x, y)| img.rgb(x, y)).collect();
    // Brightness uses the largest channel: a boss's red name has a luma of only about 115.
    let lum = |p: &[u8; 3]| u32::from(*p.iter().max().unwrap_or(&0));
    let max = px.iter().map(lum).max()?;
    if max < 150 {
        return None;
    }
    let bright: Vec<&[u8; 3]> = px.iter().filter(|p| lum(p) + 35 >= max).collect();
    if bright.len() < 12 {
        return None;
    }
    let n = bright.len() as f32;
    Some(std::array::from_fn(|c| bright.iter().map(|p| f32::from(p[c])).sum::<f32>() / n))
}

/// The rarity from the name colour: white = normal, light blue = magic, golden yellow = rare, red = boss (mean
/// colours measured over the 55 frames, take the nearest; too far, or about as close to the second nearest,
/// is not accepted).
fn rarity_of(c: [f32; 3]) -> Option<Rarity> {
    let read = rarity_read(c);
    (read.rule != "none").then_some(read.nearest)
}

/// The decision process of [`rarity_of`].
fn rarity_read(c: [f32; 3]) -> ColourRead {
    const REF: [(Rarity, [f32; 3]); 4] =
        [(Rarity::Normal, [160.0, 150.0, 156.0]), (Rarity::Magic, [142.0, 166.0, 181.0]), (Rarity::Rare, [205.0, 165.0, 105.0]), (Rarity::Boss, [209.0, 73.0, 90.0])];
    // Brightness varies (glow, fade-in), so look only at hue: subtract the mean before comparing.
    let centre = |v: [f32; 3]| {
        let m = (v[0] + v[1] + v[2]) / 3.0;
        [v[0] - m, v[1] - m, v[2] - m]
    };
    let raw = c;
    let c = centre(c);
    let mut ds: Vec<(Rarity, f32)> = REF
        .iter()
        .map(|(r, v)| {
            let v = centre(*v);
            (*r, ((c[0] - v[0]).powi(2) + (c[1] - v[1]).powi(2) + (c[2] - v[2]).powi(2)).sqrt())
        })
        .collect();
    ds.sort_by(|a, b| a.1.total_cmp(&b.1));
    // Correct reads in the 55 frames are all within a distance of 22; names that are fading out or in the
    // middle of a glow land between two colours and are not accepted.
    // When a name is more saturated than the reference colour (2026-11-03 ranked game, 28k instrument screen:
    // gold (236,177,82) is 39 from rare, red (241,76,75) is 34 from boss), it is somewhat farther from the
    // nearest colour but much farther from the second nearest: a gap of 30 or more is also accepted.
    // Across all 334 frames, none of those that were recognized before changed because of this.
    // The nearest-distance limit was relaxed from 40 to 44 (in the 2026-11-03 ranked game, the gold name of the
    // monster `腐囊異獸` is 40.0-40.2 from rare, and a slight JPEG shift made it unrecognized): among 1602 name
    // colours (the full frames in trainingdata and home), 0 of those recognized before changed and 9 are newly
    // recognized (8 gold names of that monster, all rare by eye; 1 pink effect on a battle screen, which has no
    // round panel and does not read the tanks); the smallest second-nearest gap among the newly recognized is
    // 41.3. A second-nearest gap ≥30 is unchanged.
    let close = ds[0].1 <= 24.0 && ds[1].1 - ds[0].1 >= 10.0;
    let vivid = ds[0].1 <= 44.0 && ds[1].1 - ds[0].1 >= 30.0;
    let round = |v: f32| (v * 10.0).round() / 10.0;
    ColourRead {
        rgb: raw.map(|v| v.round().clamp(0.0, 255.0) as u8),
        rule: if close { "close" } else if vivid { "vivid" } else { "none" },
        nearest: ds[0].0,
        distance: round(ds[0].1),
        second: ds[1].0,
        second_distance: round(ds[1].1),
    }
}

/// The colour of the archetype icon: red eye = Agito, purple ring = Pest, blue vortex =
/// Aemberon, gold helmet = Bone Sentry.
fn archetype_of(img: &Rgba, r: Rect) -> Option<Archetype> {
    let (mut blue, mut purple, mut gold, mut red) = (0, 0, 0, 0);
    for y in r.y..r.y + r.h {
        for x in r.x..r.x + r.w {
            let [r, g, b] = img.rgb(x, y).map(i32::from);
            blue += i32::from(b > 150 && b >= r - 10 && g > 110);
            purple += i32::from(b > 110 && r > 80 && g * 10 < b * 6);
            gold += i32::from(r > 160 && g > 110 && b * 10 < g * 6);
            red += i32::from(r > 150 && g < 70 && b < 90);
        }
    }
    if purple >= 40 {
        Some(Archetype::Pest)
    } else if gold >= 40 {
        Some(Archetype::BoneSentry)
    } else if blue >= 30 && purple < 20 {
        Some(Archetype::Aemberon)
    } else if red >= 15 && blue < 10 && purple < 10 && gold < 10 {
        Some(Archetype::Agito)
    } else {
        None
    }
}

/// The Agito eye is lit: at another interface scale the unscaled icon box of an empty tank shows the red glow
/// inside the tank's top instead of the 3D frame the original setting shows there, and that glow passes the red
/// rule of [`archetype_of`] (an empty tank 2 read as an Agito monster from round 9 on, 2026-11-06 07:54). The eye
/// itself is bright: 46-54 pixels of that red over 200 in the icon's frame area at s = 1.26, the glow none.
fn eye_lit(img: &Rgba, r: Rect) -> bool {
    let mut n = 0;
    for y in r.y..r.y + r.h {
        for x in r.x..r.x + r.w {
            let [r, g, b] = img.rgb(x, y).map(i32::from);
            n += i32::from(r > 200 && g < 70 && b < 90);
        }
    }
    n >= 8
}

// ---------------------------------------------------------------------------- cards

/// The category and potion rarity given by the type text at the bottom of a card.
pub fn type_of(text: &str) -> (Option<Category>, Option<PotionRarity>) {
    let t = norm(text);
    let rarity = if t.contains("普通") {
        Some(PotionRarity::Normal)
    } else if t.contains("魔法") {
        Some(PotionRarity::Magic)
    } else if t.contains("稀有") {
        Some(PotionRarity::Rare)
    } else {
        None
    };
    // OCR often drops a character (`稀有手術方氵`, `魔法劑`), so look at single characters only.
    let category = if t.contains('方') || t.contains('案') {
        Some(Category::Protocol)
    } else if t.contains('具') || t.contains("術用") {
        Some(Category::Instrument)
    } else if t.contains('藥') || t.contains('劑') {
        Some(Category::Potion)
    } else {
        None
    };
    (category, if category == Some(Category::Potion) { rarity } else { None })
}

/// The category given by the title.
fn title_of(text: &str) -> Option<Category> {
    let t = norm(text);
    if !(t.contains('擇') || t.contains('選') || t.contains('種')) {
        return None;
    }
    if t.contains('案') || t.contains('方') {
        Some(Category::Protocol)
    } else if t.contains('具') || t.contains('用') {
        Some(Category::Instrument)
    } else if t.contains('藥') || t.contains('劑') {
        Some(Category::Potion)
    } else {
        None
    }
}

/// The characters appear in `whole` in order (characters may be skipped).
fn subsequence(part: &str, whole: &str) -> bool {
    let mut it = whole.chars();
    part.chars().all(|c| it.any(|w| w == c))
}

/// Edit distance (in characters) between a card name and a name-table entry.
fn distance(a: &str, b: &str) -> usize {
    let n = a.chars().count().max(b.chars().count());
    ((1.0 - similarity(a, b)) * n as f32).round() as usize
}

/// The cards of `category` in the name table that have a Traditional Chinese name.
fn candidates(category: Category) -> Vec<(usize, String)> {
    names().cards.iter().enumerate().filter(|(_, c)| c.category == category).filter_map(|(i, c)| Some((i, norm(c.zh.as_deref()?)))).collect()
}

/// How many characters a card name may be off: 3 characters or fewer must match exactly, 4-7 characters may be
/// off by 1, 8 or more by 2.
fn allowed(len: usize) -> usize {
    match len {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// Match a card name: exact (or off by one or two characters) with a single nearest card; otherwise the
/// visible part matches only one card and there is corroboration (signs that the card name is covered, the left
/// tooltip is also this card, most of the name was seen, or the width of the card-name line equals this card's
/// character count).
fn match_name(text: &str, category: Category, clipped: bool, tooltip: Option<usize>, est_len: Option<usize>) -> Option<(usize, How)> {
    // The offspring chain (`子一代：寄生蠱卵`, "F1 Gen: Parasite Egg") is tried first under its rebuilt generation; no
    // other name contains `代`, so text without it goes on exactly as before.
    if let Some(hit) = generation(&norm(text)).and_then(|g| match_text(&g, category, clipped, tooltip, est_len)) {
        return Some(hit);
    }
    // Card names have no Arabic digits (`二寸顱骨釘` uses a Chinese numeral): digits are the neighbouring
    // card's effect text mixed in.
    // `一` is what the hyphen `-` gets read as (no card name contains `一`).
    let t: String = norm(text).chars().filter(|c| !c.is_ascii_digit() && *c != '一').collect();
    // The `麻` in `麻藥酊劑` is often read as `數` (7 times in the 2026-11-03 ranked game); the name table has no
    // card name containing `數`, so only swap when the original characters do not match.
    match_text(&t, category, clipped, tooltip, est_len).or_else(|| t.contains('數').then(|| match_text(&t.replace('數', "麻"), category, clipped, tooltip, est_len)).flatten())
}

/// The offspring names `子一代…`, `子二代…`, `子三代…` rebuilt from normalized text: in the 2026-11-06 07:54 game OCR
/// dropped the leading `子` (`?一代爬蟲卵族`, `二代:寄生蟲`, `三代破繭蝶`), [`match_name`]'s filter would drop the
/// `一` (taken for a hyphen), and `簇` was read as `族`. The numeral right before `代` gives the generation and the
/// rest is kept (digits dropped, `族` read as `簇`). None when there is no `代` right after `一`, `二` or `三`.
fn generation(n: &str) -> Option<String> {
    let chars: Vec<char> = n.chars().collect();
    let at = chars.iter().position(|&c| c == '代')?;
    let g = *chars.get(at.checked_sub(1)?)?;
    if !matches!(g, '一' | '二' | '三') {
        return None;
    }
    let rest: String = chars[at + 1..].iter().filter(|c| !c.is_ascii_digit()).map(|&c| if c == '族' { '簇' } else { c }).collect();
    Some(format!("子{g}代{rest}"))
}

/// The body of [`match_name`]: `t` is already normalized.
fn match_text(t: &str, category: Category, clipped: bool, tooltip: Option<usize>, est_len: Option<usize>) -> Option<(usize, How)> {
    let t = t.to_string();
    if t.chars().count() < 2 {
        return None;
    }
    let cands = candidates(category);
    let mut scored: Vec<(usize, usize, usize)> = cands.iter().map(|(i, z)| (*i, distance(&t, z), z.chars().count())).collect();
    scored.sort_by_key(|&(_, d, _)| d);
    if let Some(&(i, d, len)) = scored.first() {
        // When characters are missing, the second nearest must be at least two characters farther: `膽汁溶液`
        // is 1 character from `青膽汁溶液` and 2 from `黏稠膽汁溶液`, and the front may simply be covered, so
        // that does not count. For a second nearest that is only one character off, it blocks only when the
        // characters read are a subsequence of it (it may be the covered one): `清疸油膏` is 1 character from
        // `清疽油膏` and 2 from `化骨油膏`, a same-length misread rather than a covered name (after
        // `化骨油膏` was added on 2026-11-03, ranked game t028.40 was read as unknown because of this).
        let zh = |i: usize| cands.iter().find(|(j, _)| *j == i).map_or("", |(_, z)| z.as_str());
        let unique = scored.iter().skip(1).all(|s| s.1 > d && (d == 0 || s.1 > d + 1 || !subsequence(&t, zh(s.0))));
        if unique && d <= allowed(len) {
            return Some((i, How::Name));
        }
    }
    let fits: Vec<&(usize, String)> = cands.iter().filter(|(_, z)| subsequence(&t, z)).collect();
    if let [(i, z)] = fits[..] {
        let most = t.chars().count() * 4 >= z.chars().count() * 3;
        // The character count equals this card's (within one character), and OCR just dropped a character in
        // the middle. The count is of what is printed on screen (including brackets; a hyphen takes only half a
        // cell), not of the squashed text.
        let shown = names().cards[*i].zh.as_deref().map_or(0.0, |s| s.chars().map(|c| if c == '-' { 0.5 } else { 1.0 }).sum::<f32>());
        let same_len = est_len.is_some_and(|n| (n as f32 - shown).abs() <= 1.0);
        if clipped || tooltip == Some(*i) || most || same_len {
            return Some((*i, How::Partial));
        }
    }
    None
}

/// When the card name cannot be read, the start of the effect text is often the same card's full name (the
/// effect text's area reaches up to the card-name line; when a neighbouring card presses on the card name, in
/// the 2026-11-03 ranked game the name `混合活蛭溶液` was read as `選擇1組怪物、使2組同` while the effect text
/// started with the full name). If exactly one card in the name table has its full name (3 characters or more)
/// appearing within the first two characters, or all the others are parts of it, it is this card.
fn name_in_effect(effect: &str, category: Category) -> Option<usize> {
    // `麻` is also often read as `痳` (2026-11-04 11:31, round 8: the raised card `麻藥酊劑-異魔` had its name
    // read as `酊劑一異` and its effect text started with `痳藥酊劑異魔`); the name table has no card name
    // containing either of these two characters.
    let e = norm(effect).replace(['數', '痳'], "麻");
    let hits: Vec<(usize, String)> = candidates(category)
        .into_iter()
        .filter(|(_, z)| z.chars().count() >= 3 && e.find(z.as_str()).is_some_and(|at| e[..at].chars().count() <= 2))
        .collect();
    let longest = hits.iter().max_by_key(|(_, z)| z.chars().count())?;
    hits.iter().all(|(_, z)| longest.1.contains(z.as_str())).then_some(longest.0)
}

/// Of the cards `hits` that different reads of one card name matched, the one whose whole name starts the effect
/// text ([`name_in_effect`]); None when that name is none of them (or the effect names no card).
fn effect_among(hits: &[(usize, How)], effect: &str, category: Category) -> Option<usize> {
    name_in_effect(effect, category).filter(|i| hits.iter().any(|h| h.0 == *i))
}

/// Potion kits: `藥劑箱（大）`, `藥劑箱（小）` and `稀有藥劑箱（小）` differ by only one or two characters, and
/// `藥劑箱（小）` has no Traditional Chinese name in the name table. The size needs the card name to be read as
/// `大` or `小` ("large" or "small"), or the effect text to print the same number of potions (`包含 5 支`,
/// `包含 3 支`, "contains 5" or "contains 3"); whether it is rare needs the card name or the effect text
/// (`更有可能出現高稀有度藥劑`, "more likely to show high-rarity potions") to print `稀有` ("rare"), and a
/// non-rare one must have neither.
fn kit_ok(card: &CardName, name_text: &str, effect: &str) -> bool {
    let Some(zh) = card.zh.as_deref().filter(|z| z.contains('箱')) else { return true };
    let (n, e) = (norm(name_text), norm(effect));
    let size = if zh.contains('大') { n.contains('大') || (e.contains('5') && !e.contains('3')) } else { n.contains('小') || (e.contains('3') && !e.contains('5')) };
    let rare_seen = n.contains("稀") || e.contains("稀有");
    size && rare_seen == zh.starts_with("稀有")
}

/// When a card name carries an archetype suffix (`-蠱蟲`, the Pest archetype), the suffix or the effect text must print the
/// same archetype.
fn species_ok(card: &CardName, name_text: &str, effect: &str) -> bool {
    let Some((_, suffix)) = card.zh.as_deref().and_then(|z| z.split_once('-')) else { return true };
    if norm(name_text).contains(&norm(suffix)) {
        return true;
    }
    let e = norm(effect);
    let shown: Vec<&str> = ARCHETYPES.iter().map(|&a| archetype_zh(a)).filter(|s| e.contains(&norm(s))).collect();
    shown == [suffix]
}

// ---------------------------------------------------------------------------- whole screenshot

/// Read one screenshot in the reference size (2560x1440, see [`to_reference`]), all from `img`; for a frame of
/// another size use [`read_with_frame`].
pub fn read(img: &Rgba) -> Reading {
    read_with_frame(img, None)
}

/// [`read`] with the frame the reference `img` was scaled from: when the frame is not 1440 high, the OCR input
/// (monster names, card names, titles, type and effect text) is cut from the frame and resampled once instead of
/// from `img`; everything else reads `img`. None or a frame 1440 high reads `img` alone.
pub fn read_with_frame(img: &Rgba, frame: Option<&Rgba>) -> Reading {
    read_text(img, Text { img, frame: frame.and_then(Frame::new) })
}

/// [`read_with_frame`] of a game drawn at another interface scale: `img` is the reference unscaled to the
/// original setting ([`super::ui_scale::unscale`]) and `frame` maps reference rects to the frame through the zones
/// ([`Frame::ui`]). See [`super::ui_scale::read`].
pub(crate) fn read_unscaled(img: &Rgba, frame: Frame) -> Reading {
    read_text(img, Text { img, frame: Some(frame) })
}

/// [`read_with_frame`] of a game drawn at interface scale `ui` (the HUD's `PageScale` over 0.667, see
/// [`super::ui_scale`]; None when not known): at the original setting, or with no scale, the same reading as
/// [`read_with_frame`]; at another scale the frame is unscaled zone by zone first. `frame` is the frame `img` was
/// scaled from (None when `img` is the frame itself).
pub fn read_scaled(img: &Rgba, frame: Option<&Rgba>, ui: Option<f64>) -> Reading {
    super::ui_scale::read(img, frame, ui).reading
}

/// Both result screen buttons, "end surgery" and "start surgery", were read: with them the label need only be read
/// in part from [`RESULT_LABEL_WIDE`] ([`wide_result_label`]).
fn both_result_buttons(buttons: &str) -> bool {
    let b = norm(buttons);
    b.contains("開始手術") && b.contains("結束手術")
}

/// The wider label crop shows the result screen's "final activity" label: `活性` read whole (frame-0062 of
/// 2026-11-06_13-46-25 read `一最活性`, its `終` lost).
fn wide_result_label(label: &str) -> bool {
    norm(label).contains("活性")
}

/// The body of [`read_with_frame`]: `img` is the reference the colours, icons and digits are read from, `text`
/// where the OCR reads.
fn read_text(img: &Rgba, text: Text) -> Reading {
    // First OCR pass: screen kind, type text (to find the cards' positions), tooltip, monster names.
    let mut regions = vec![(TITLE, 3.0), (RESULT_LABEL, 2.0), (RESULT_BUTTONS, 2.0), (TITLE_TIGHT, 2.0), (TYPE_BAND, 2.0), (TOOLTIP_NAME, 2.0)];
    regions.extend((0..6).map(|i| (tank_name(i), 3.0)));
    regions.extend((0..6).map(|i| (tank_name(i), 2.0)));
    let read = text.batch(&regions);
    let (title, result_label, result_buttons, types, tooltip) = (format!("{} {}", joined(&read[0]), joined(&read[3])), joined(&read[1]), joined(&read[2]), &read[4], joined(&read[5]));
    let mut problems = Vec::new();

    let round = round(&text);
    let is_result = |label: &str, buttons: &str| norm(label).contains("最終活性") && ["開始手術", "結束手術"].iter().any(|b| norm(buttons).contains(b));
    // For a frame of another size, when the result screen was not recognized, its label and buttons are read
    // again from the reference as at 1440: on the 4K stand-in of the result screen the label read from the
    // frame kept only `最` (2026-11-06, trainingdata/vorax/frames t123.00 enlarged to 3840x2160).
    let result = is_result(&result_label, &result_buttons)
        || (round.is_none() && text.frame.is_some() && {
            let again = ocr_batch(img, &[(RESULT_LABEL, 2.0), (RESULT_BUTTONS, 2.0)]);
            is_result(&joined(&again[0]), &joined(&again[1]))
        })
        || (round.is_none() && both_result_buttons(&result_buttons) && {
            let again = text.batch(&[(RESULT_LABEL_WIDE, 2.0)]);
            wide_result_label(&joined(&again[0]))
        });
    let mut reading = Reading {
        kind: Kind::Other,
        round,
        ultimate: None,
        ultimate_pending: false,
        panel_covered: false,
        next_reward: None,
        rerolls: None,
        tanks: vec![],
        held: vec![],
        cards: vec![],
        tank_sum: None,
        problems: vec![],
        tooltip: tooltip.clone(),
    };
    if round.is_none() {
        if result {
            reading.kind = Kind::Result;
            // The result screen's digits are one size bigger than the left panel's, so the glyphs differ more.
            reading.ultimate = number(&chars(&text.glyphs(RESULT_VALUE, 20, Ink::Plain), templates::RESULT));
        }
        return reading;
    }

    // Left panel: when covered by a tooltip this is card art here, the glyphs do not match the digit templates,
    // and it reads as None.
    reading.ultimate = panel_number(&text, ULTIMATE, 15, 20..=28, templates::PANEL);
    reading.next_reward = panel_number(&text, REWARD, 14, 17..=24, templates::REWARD);
    // If the tooltip reads any text, the panel is covered; if it is not covered and the green "+N" is still
    // there, the value is changing (when it cannot be read, say so as it is rather than calling it covered by
    // the tooltip).
    reading.panel_covered = tooltip_shown(img);
    reading.ultimate_pending = !reading.panel_covered && has_green(img, ULTIMATE_DELTA);

    // Tanks. Those where a "1" was added to a total activity that was read cleanly are noted, to be checked
    // against the sum.
    let mut needs_sum: Vec<usize> = Vec::new();
    for i in 0..6 {
        // The monster name is read twice, at 3x and at 2x (OCR is very sensitive to the scale); use first the
        // one that matches the name table.
        let (three, two) = (joined(&read[6 + i]).replace(' ', ""), joined(&read[12 + i]).replace(' ', ""));
        // The colour looks only at the name itself: neither the archetype icon above nor the heart of the total
        // activity below may be counted in.
        let colour = name_colour(img, Rect::new(TANK_X[i] - 110, 508, 220, 22));
        let archetype_icon = archetype_of(img, tank_icon(i)).filter(|&a| !text.ui() || a != Archetype::Agito || eye_lit(img, tank_icon(i)));
        let glow = glowing(img, i);
        let total_glyphs = text.glyphs(tank_total(i), 9, Ink::Label);
        let raw_total = chars(&total_glyphs, templates::LABEL);
        let raw_values = values(&text, tank_values(i));
        // Empty tank: no name, icon or digits. Stray glyphs (unrecognized `?`) cut from the colour of the name
        // row, candle flames or the bright edge of the tank base do not count.
        let digits_in = |s: &str| s.chars().filter(|c| c.is_ascii_digit()).count();
        if norm(&three).is_empty() && norm(&two).is_empty() && archetype_icon.is_none() && digits_in(&raw_total) < 2 && !raw_values.contains('x') && digits_in(&raw_values) < 3 {
            reading.tanks.push(Tank { empty: true, ..Tank::default() });
            continue;
        }
        let monsters = &names().monsters;
        let keys: Vec<String> = monsters.iter().map(|(m, _)| norm(m)).collect();
        let lookup = |t: &str| crate::reader::panels::best_match(&norm(t), &keys, 0.6).map(|(k, _)| &monsters[k]);
        let known = lookup(&three).or_else(|| lookup(&two));
        let raw_name = if three.is_empty() || (known.is_some() && lookup(&three).is_none()) { two } else { three };
        let archetype = archetype_icon.or(known.map(|(_, a)| *a));
        if let (Some(icon), Some((m, a))) = (archetype_icon, known)
            && icon != *a
        {
            problems.push(format!("水箱 {}：種族圖示是{}，名字「{m}」卻是{}", i + 1, archetype_zh(icon), archetype_zh(*a)));
        }
        // When covered by the golden glow the digits are unreliable (the value is jumping), so no
        // back-derivation either.
        let numbers = if glow { Numbers::default() } else { tank_numbers(&raw_values, &raw_total) };
        if numbers.needs_sum {
            needs_sum.push(reading.tanks.len());
        }
        let tank = Tank {
            empty: false,
            name: known.map(|(m, _)| m.clone()).or_else(|| (!raw_name.is_empty()).then(|| raw_name.clone())),
            archetype,
            // When covered by the golden glow, neither the colour nor the digits are reliable (the value is
            // jumping).
            rarity: colour.and_then(rarity_of).filter(|_| !glow),
            colour: colour.map(rarity_read),
            activity: numbers.activity,
            quantity: numbers.quantity,
            total: numbers.total,
            repaired: numbers.repaired,
            raw_name,
            raw_total,
            raw_values,
        };
        if !tank.known() {
            problems.push(tank_problem(i, &tank));
        }
        reading.tanks.push(tank);
    }
    settle_sums(&mut reading, &needs_sum, &mut problems);
    reading.held = icons::read_held(img);

    // Screen kind: the title says which kind of choice it is and the cards can be read; otherwise it is
    // resolving.
    let mut category = title_of(&title);
    let anchors = card_anchors(types);
    if category.is_none() {
        // When the title is covered by the glow, look at the type text at the bottom of the cards.
        category = anchors.iter().filter_map(|(_, _, t)| type_of(t).0).next();
    }
    let Some(category) = category else {
        reading.kind = Kind::Resolving;
        reading.problems = problems;
        return reading;
    };
    reading.cards = read_cards(&text, category, &anchors, &tooltip);
    if reading.cards.is_empty() {
        // Only the card in use is still glowing (a potion taking effect, or after a protocol is confirmed).
        reading.kind = Kind::Resolving;
        reading.cards.clear();
        reading.problems = problems;
        return reading;
    }
    // Only one card left: only the start of a tutorial game (with no instrument yet) offers a single
    // instrument; at other times it is the card left on screen after confirming (a potion glowing, a protocol
    // sitting in the middle until the round ends), and it cannot be operated.
    if reading.cards.len() == 1 && !(category == Category::Instrument && reading.held.is_empty()) {
        reading.kind = Kind::Resolving;
        reading.cards.clear();
        reading.problems = problems;
        return reading;
    }
    reading.kind = match category {
        Category::Potion => Kind::Potion,
        Category::Instrument => Kind::Instrument,
        Category::Protocol => Kind::Protocol,
    };
    if category != Category::Protocol {
        let gs = text.glyphs(REROLL, 12, Ink::Label);
        let gs: Vec<Glyph> = gs.into_iter().filter(|g| (12..=24).contains(&g.h()) && g.x1 - g.x0 >= 3).collect();
        reading.rerolls = match &gs[..] {
            [g] => number(&chars(std::slice::from_ref(g), templates::REROLL)).map(|n| n as i32),
            _ => None,
        };
        if reading.rerolls.is_none() {
            problems.push("重擲次數讀不到".into());
        }
    }
    for (k, c) in reading.cards.iter().enumerate() {
        if c.index.is_none() {
            problems.push(format!("第 {} 張卡認不出（卡名讀到「{}」）{}", k + 1, c.raw_name.replace(' ', ""), c.why.as_deref().map(|w| format!("：{w}")).unwrap_or_default()));
        }
    }
    if let (Some(ult), Some(sum)) = (reading.ultimate, reading.tank_sum)
        && ult != sum
        && !reading.ultimate_pending
    {
        problems.push(format!("各水箱總活性加起來是 {sum}，最終活性卻是 {ult}"));
    }
    reading.problems = problems;
    reading
}

/// The tanks' sum, and the tanks whose values only count once that sum matches the final activity
/// (`needs_sum`: a "1" added to a total read cleanly, or a total taken from the product, see [`tank_numbers`]):
/// when the final activity is unread, still jumping or off, they fall back to the values as read and their
/// problems are added.
fn settle_sums(reading: &mut Reading, needs_sum: &[usize], problems: &mut Vec<String>) {
    if reading.tanks.iter().all(|t| t.empty || t.consistent()) {
        reading.tank_sum = Some(reading.tanks.iter().filter_map(|t| t.total).sum());
    }
    // The final activity already equals the tanks' sum: the value has finished jumping and only the green "+N"
    // is left on screen (after round 1's instrument reroll it stayed there, with the final activity stopped at
    // 396 = the sum, 2026-11-03 ranked game), so it does not count as still jumping. A final activity that is
    // still jumping is smaller than the sum.
    if reading.ultimate_pending && reading.ultimate.is_some() && reading.ultimate == reading.tank_sum {
        reading.ultimate_pending = false;
    }
    // Tanks where a "1" was added to a total activity that was read cleanly, or the total was taken from the
    // product: the tanks' sum must match the final activity (when it is not jumping) to count, otherwise fall
    // back to the values as read (unknown).
    if !needs_sum.is_empty() {
        let summed = !reading.ultimate_pending && reading.ultimate.is_some() && reading.ultimate == reading.tank_sum;
        if !summed {
            for &i in needs_sum {
                let t = &mut reading.tanks[i];
                let parsed = t.raw_values.split_once('x').and_then(|(a, q)| Some((number(a)?, number(q)?)));
                (t.activity, t.quantity, t.total, t.repaired) = (parsed.map(|v| v.0), parsed.map(|v| v.1), number(&t.raw_total), None);
                problems.push(tank_problem(i, t));
            }
            reading.tank_sum = None;
        }
    }
}

/// Tank i's digits are covered by the golden glow (the value is changing): in the two digit rows there are
/// many bright golden pixels (candle flames have only about 30, the glow has hundreds to over a thousand).
/// Not verified at another interface scale: there the unscaled label paints about half of these rows over (see
/// `ui_scale`), so the count of 150 may be reached later or not at all; no scaled frame of a glowing tank exists.
fn glowing(img: &Rgba, i: usize) -> bool {
    let mut n = 0;
    for y in 535..585 {
        for x in TANK_X[i] - 110..TANK_X[i] + 110 {
            let [r, g, b] = img.rgb(x, y).map(i32::from);
            n += i32::from(r > 190 && g > 130 && b < 130 && r - b > 90);
        }
    }
    n >= 150
}

/// Turn the crop of `r` into black text on white: pixels whose brightness is at or above the `q`th percentile
/// are text (the card name is the brightest thing on the card face), and the rest fade to white. When the card
/// face is golden or has card art, Windows OCR reading it directly often cannot read the text.
pub fn inked(img: &Rgba, r: Rect, q: usize) -> Rgba {
    ink(&img.crop(r), q)
}

/// [`inked`] of a whole image.
fn ink(c: &Rgba, q: usize) -> Rgba {
    let lum: Vec<f32> = c.data.as_chunks::<4>().0.iter().map(|p| f32::from(p[0]) * 0.3 + f32::from(p[1]) * 0.59 + f32::from(p[2]) * 0.11).collect();
    let mut sorted = lum.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let at = |p: usize| sorted.get((sorted.len() * p / 100).min(sorted.len().saturating_sub(1))).copied().unwrap_or(0.0);
    let (lo, hi) = (at(q), at(99).max(at(q) + 1.0));
    let data = lum
        .iter()
        .flat_map(|&v| {
            let ink = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
            let g = (255.0 * (1.0 - ink)) as u8;
            [g, g, g, 255]
        })
        .collect();
    Rgba { w: c.w, h: c.h, data }
}

/// There is a tooltip on the left: the area to the right of the panel (x 330..415) is normally flat dark ground
/// (brightness standard deviation about 6.5), while the tooltip's card face has a border and artwork (17 or more
/// across the 55 frames).
fn tooltip_shown(img: &Rgba) -> bool {
    let lum: Vec<f32> = (700..1100).flat_map(|y| (330..415).map(move |x| (x, y))).map(|(x, y)| {
        let [r, g, b] = img.rgb(x, y);
        (f32::from(r) + f32::from(g) + f32::from(b)) / 3.0
    }).collect();
    let mean = lum.iter().sum::<f32>() / lum.len() as f32;
    let var = lum.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / lum.len() as f32;
    var.sqrt() > 12.0
}

/// The green "+N": the value is still jumping.
fn has_green(img: &Rgba, r: Rect) -> bool {
    let mut n = 0;
    for y in r.y..r.y + r.h {
        for x in r.x..r.x + r.w {
            let [r, g, b] = img.rgb(x, y).map(i32::from);
            n += i32::from(g > 150 && g > r + 50 && g > b + 50);
        }
    }
    n >= 20
}

fn tank_problem(i: usize, t: &Tank) -> String {
    let mut miss = Vec::new();
    if t.archetype.is_none() {
        miss.push("種族".to_string());
    }
    if t.rarity.is_none() {
        miss.push("稀有度".to_string());
    }
    match (t.activity, t.quantity, t.total) {
        (Some(a), Some(q), Some(total)) if a * q != total => miss.push(format!("活性×數量 {a}×{q}≠總活性 {total}")),
        (Some(_), Some(_), Some(_)) => {}
        _ => miss.push(format!("數值（總活性讀到「{}」、活性 x 數量讀到「{}」）", t.raw_total, t.raw_values)),
    }
    format!("水箱 {}（{}）未知：{}", i + 1, t.name.as_deref().unwrap_or("?"), miss.join("、"))
}

/// Each card's center x, raised height and type text: the position of the type text at the bottom of the card;
/// a position where none was read does not count as a card. A line with a digit or a `+` is the last line of a
/// raised card's effect text risen into the band, never a type text (2026-11-06_16-26-30 round 4: the hovered
/// `卵殼藥粉`'s `使其+25數量` read as `具 + 25 數 量`, its `具` taken for an instrument, merged with the card's
/// `魔法藥劑`, and the card dropped as not matching the title).
fn card_anchors(types: &[Line]) -> Vec<(i32, i32, String)> {
    let effect_line = |t: &str| t.chars().any(|c| c.is_ascii_digit() || c == '+' || c == '＋');
    let mut lines: Vec<&Line> = types.iter().filter(|l| type_of(&l.text).0.is_some() && !effect_line(&l.text)).collect();
    lines.sort_by_key(|l| l.rect.x);
    let mut out: Vec<(i32, i32, String)> = Vec::new();
    for l in lines {
        let (x, dy) = (l.rect.x + l.rect.w / 2, l.rect.y + l.rect.h / 2 - TYPE_Y);
        // Merge the type text of one card when it was split into two lines.
        match out.last_mut() {
            Some((px, _, t)) if x - *px < 120 => {
                *px = (*px + x) / 2;
                t.push_str(&l.text);
            }
            _ => out.push((x, dy, l.text.clone())),
        }
    }
    out
}

/// The position of one card: center x, raised height and type text (see [`card_anchors`]).
type Slot = (i32, i32, String);

/// Center x of the four cards dealt when a chain potion was taken the round before: the next chain step is merged
/// into the hand as the leftmost card (2026-11-06 07:54 game, round 9). Measured (centre of the type text at the
/// bottom of each card, reference coordinates at the original setting) on two independent samples:
/// - frame 0044 of that game (zh-TW, `PageScale` 0.841017, s = 1.2609, read unscaled): 1073 (the raised, selected
///   chain card), 1274, 1499-1500, and 1713 for the potion kit, which prints no type text (the middle of its frame,
///   whose sides are at 1660 and 1993 on the frame at y 1200, unscaled);
/// - an upstream English 1920x1080 screenshot of the same kind of hand (Dissection II leftmost), enlarged to
///   2560x1440 with its bottom interface scaled by 1/0.958 about (1280, 1440) to line its HUD up with ours (the
///   card width, the magic-to-normal spacing and the reroll digits all give 0.958): 1075, 1274, 1499, 1710.
///
/// The two cards at the ends are tilted (their sides lean toward the middle at the bottom), so their type text
/// sits closer to the middle than one even spacing (225) out from the middle two: 1049 and 1724 extrapolated that
/// way were 24-28 and 11-14 off. The center, about 1390, lies between the three-card hand's 1416 and the fan's
/// 1344, as a fourth card between three and five would. Positions with the mouse on a card are not known.
pub(crate) const HAND4_X: [i32; 4] = [1074, 1274, 1500, 1712];
/// Tolerance of [`hand4_slots`] per slot (pixels). The middle two were within 2 on both samples, so 15 leaves room
/// for OCR box jitter while keeping out by position alone the fan with the mouse on its last card (1085, 1297,
/// 1509: its 1297 is 23 from slot 1, so it must be read 8 or more to the left to pass) and the usual three cards
/// with the mouse on the first (1205, 1463, 1676: 1205 is 69 from slot 1). The ends keep 30: both samples were
/// within 3, but a partial read of the type text moves its centre by a dozen or more pixels.
const HAND4_TOL: [i32; 4] = [30, 15, 15, 30];
/// Tolerance of the last slot when the leftmost card's type text was not read and it is inferred (see
/// [`hand4_slots`]): both samples were within 2 of 1712 (OCR 1710-1712). The fan with the mouse on its last card
/// puts its three right-hand cards at 1297, 1509 and 1768; with 20 its 1768 would have to be read 36 or more to the
/// left, besides its 1297 being read 8 or more to the left, before those three could pass for slots 1 to 3.
const HAND4_INFER_TOL: i32 = 20;

/// The four-card hand ([`HAND4_X`]): three or four type texts each within [`HAND4_TOL`] of a different slot, in
/// order, and much closer to them than to the usual three cards ([`CARD_X`]). `slots` is the first read merged with
/// the second read of [`FAN_BAND`] ([`merge_reads`]): the leftmost card's type text is often only partly read the
/// first time (`藥劑` alone, its centre 26 off) and in full the second time. Two ways in:
/// - the leftmost slot was read and says `至臻` (a chain card is always Supreme and always leftmost): the fan with
///   the mouse on its last card has three cards near slots 0 to 2 (1085, 1297, 1509; frame 0044 was once read as
///   five cards that way), and a Supreme second card there is told apart by its 1297 (see [`HAND4_TOL`]); one
///   missing slot (the potion kit prints no type text) is filled in. [`fan_ends_read`] adds nothing here now (the
///   slots with their tolerance already run from 1044 to 1742), it is kept as a guard should they widen;
/// - the leftmost slot was not read at all (its type text covered by its effect text) but the other three were:
///   slot 0 is filled in. These three (1274, 1500, 1712) are 69 to 84 from the usual three cards and no three cards
///   of any fan layout fit them; the last slot must also be within [`HAND4_INFER_TOL`].
///
/// Otherwise None and the hand is read as before. Returns the four cards and the index of the inferred one.
fn hand4_slots(slots: &[Slot]) -> Option<(Vec<Slot>, Vec<usize>)> {
    let found: Vec<&Slot> = slots.iter().filter(|s| !s.2.is_empty()).collect();
    if !(3..=4).contains(&found.len()) {
        return None;
    }
    let ks: Vec<usize> = found.iter().map(|s| (0..4).min_by_key(|&k| (s.0 - HAND4_X[k]).abs()).unwrap_or(0)).collect();
    if ks.windows(2).any(|w| w[0] >= w[1]) || found.iter().zip(&ks).any(|(s, &k)| (s.0 - HAND4_X[k]).abs() > HAND4_TOL[k]) {
        return None;
    }
    let supreme = |t: &str| norm(t).contains(['至', '臻']);
    if ks[0] == 0 {
        if !supreme(&found[0].2) {
            return None;
        }
    } else if ks[..] != [1, 2, 3] || (found[2].0 - HAND4_X[3]).abs() > HAND4_INFER_TOL {
        return None;
    }
    let err: i32 = found.iter().zip(&ks).map(|(s, &k)| (s.0 - HAND4_X[k]).abs()).sum();
    let card_err: i32 = found.iter().map(|s| CARD_X.iter().map(|x| (s.0 - x).abs()).min().unwrap_or(0)).sum();
    if err * 3 >= card_err {
        return None;
    }
    let (mut four, mut guessed) = (Vec::with_capacity(4), Vec::new());
    for (k, &x) in HAND4_X.iter().enumerate() {
        match ks.iter().position(|&j| j == k) {
            Some(i) => four.push(found[i].clone()),
            None => {
                four.push((x, 0, String::new()));
                guessed.push(k);
            }
        }
    }
    Some((four, guessed))
}

/// Type text read at either end of the fan, where the four-card hand has no card ([`HAND4_X`] with its tolerance
/// runs from 1044 to 1742, the fan from 920 to 1768, about 873 and 1815 with the mouse on a card): left of 1000 or
/// right of 1750. Checked on both reads merged: a type text out there means a fan whose two ends were missed the
/// first time, for example a potion kit (large) whose second card is Supreme with the mouse on its last card (its
/// 1085, 1297, 1509, read a little to the left, fit the four-card slots 0 to 2): read as four, its leftmost card
/// would be dropped.
fn fan_ends_read(anchors: &[Slot]) -> bool {
    anchors.iter().any(|a| a.0 < 1000 || a.0 > 1750)
}

/// The four-card hand read from both reads merged ([`merge_reads`]): [`hand4_slots`], unless a type text was read
/// at an end of the fan ([`fan_ends_read`]).
fn hand4_merged(merged: &[Slot]) -> Option<(Vec<Slot>, Vec<usize>)> {
    hand4_slots(merged).filter(|_| !fan_ends_read(merged))
}

/// Merge the type text of the first read (`found`; only positions where type text was really read) with the second
/// read of [`FAN_BAND`] (`band`, see [`fan_band_lines`]): for the same card (within 60) keep the read with more
/// characters, whose centre is more accurate; sorted by x.
fn merge_reads(found: &[Slot], band: &[Slot]) -> Vec<Slot> {
    // Merge only positions where type text was really read: when only two were read, read_cards adds one at the
    // default position of the usual three cards (its type text is empty), and mixing it in would make the fan
    // add up to six, fail the check and fall back to three cards (2026-11-04 ranked game, 11:31, round 4: the
    // middle card was picked up, only two type texts were read, 1629 was added, and the advice for three cards
    // flipped back and forth with that for five).
    let mut merged: Vec<Slot> = found.iter().filter(|s| !s.2.is_empty()).cloned().collect();
    for a in band {
        match merged.iter_mut().find(|m| (m.0 - a.0).abs() < 60) {
            Some(m) if norm(&a.2).chars().count() > norm(&m.2).chars().count() => *m = a.clone(),
            Some(_) => {}
            None => merged.push(a.clone()),
        }
    }
    merged.sort_by_key(|s| s.0);
    merged
}

/// When positions were found for only three (see [`maybe_fan`]), four (or six or more) of the five fanned
/// cards of a potion kit (large), given the first read merged with the second read of [`FAN_BAND`]
/// ([`merge_reads`]):
/// 1. If the merge gives exactly five whose spacing looks like a fan, use those five (all are positions that were
///    really read).
/// 2. If the merge gives three or four whose spacing looks like a fan: infer the missing ones from the middle
///    card (see [`fan_fill`]) and return their indices. With only three, the positions must also look more like
///    a fan than like the usual three cards ([`fan_closer`]).
///
/// None when neither works.
fn fan_slots(merged: &[Slot]) -> Option<(Vec<Slot>, Vec<usize>)> {
    if merged.len() == 5 && fan_spaced(merged) {
        return Some((merged.to_vec(), vec![]));
    }
    if !(merged.len() == 3 && !fan_closer(merged))
        && let Some(f) = fan_fill(merged)
    {
        return Some(f);
    }
    fan_grid(merged)
}

/// The type text read again with [`FAN_BAND`]: only the row of type text (including raised cards), since above it
/// is effect text, which may also contain the two characters `藥劑` ("potion").
fn fan_band_lines(text: &Text) -> Vec<Line> {
    let read = text.batch(&[(FAN_BAND, 2.0)]);
    read[0].iter().filter(|l| (-55..=35).contains(&(l.rect.y + l.rect.h / 2 - TYPE_Y))).cloned().collect()
}

/// The five positions of the fan: no mouse on a card (`hover` is None), or on card h (the cards to its left are
/// each pushed left and those to its right pushed right by [`FAN_HOVER_PUSH`], and it itself does not move).
fn fan_layout(hover: Option<usize>) -> [i32; 5] {
    std::array::from_fn(|k| {
        let x = FAN_MID_X + (k as i32 - 2) * FAN_STEP;
        match hover {
            Some(h) if k < h => x - FAN_HOVER_PUSH,
            Some(h) if k > h => x + FAN_HOVER_PUSH,
            _ => x,
        }
    })
}

/// The fan where only two to four type texts were read and they are not consecutive cards ([`fan_fill`] cannot
/// handle it): the middle card is picked up (raised, its type text unreadable), and the grey `普通藥劑` ("normal
/// potion") at the two ends was often not read either (2026-11-04 ranked game, 13:36 UTC, round 8: only 1132 and
/// 1562 were read, or, with the mouse on the middle one, 872, 1086 and 1609; the fill-in for the usual three
/// cards lands at 1416, the whole hand was taken as three cards, and the advice flipped between "→ tank 1" and
/// "→ tank 4" for about 3 seconds). Match the positions read to the fan's five slots ([`fan_layout`], including
/// the layouts with the mouse on any card): each must be within [`FAN_PAIR_TOL`], each matching one slot, in
/// order, and three times the total error must still be smaller than the total error against the usual
/// three-card positions ([`CARD_X`]) for it to count as a fan; the missing slots are filled in with the layout's
/// positions. When the usual three cards only had two read (for example 1205 and 1629, or with the mouse on one,
/// 1157 and 1370: error 43 against the fan and 94 against CARD_X, and a factor of two does not separate them)
/// they are not pushed into five. A real fan's error is always within 10, and over 130 against CARD_X.
fn fan_grid(found: &[Slot]) -> Option<(Vec<Slot>, Vec<usize>)> {
    if !(2..=4).contains(&found.len()) {
        return None;
    }
    let card_err: i32 = found.iter().map(|s| CARD_X.iter().map(|x| (s.0 - x).abs()).min().unwrap_or(0)).sum();
    let mut best: Option<(i32, [i32; 5], Vec<usize>)> = None;
    for hover in [None, Some(0), Some(1), Some(2), Some(3), Some(4)] {
        let pos = fan_layout(hover);
        let ks: Vec<usize> = found.iter().map(|s| (0..5).min_by_key(|&k| (s.0 - pos[k]).abs()).unwrap_or(0)).collect();
        if ks.windows(2).any(|w| w[0] >= w[1]) || found.iter().zip(&ks).any(|(s, &k)| (s.0 - pos[k]).abs() > FAN_PAIR_TOL) {
            continue;
        }
        let err: i32 = found.iter().zip(&ks).map(|(s, &k)| (s.0 - pos[k]).abs()).sum();
        if best.as_ref().is_none_or(|b| err < b.0) {
            best = Some((err, pos, ks));
        }
    }
    let (err, pos, ks) = best?;
    if err * 3 >= card_err {
        return None;
    }
    let (mut five, mut guessed) = (Vec::with_capacity(5), Vec::new());
    for (k, &x) in pos.iter().enumerate() {
        match ks.iter().position(|&j| j == k) {
            Some(i) => five.push(found[i].clone()),
            None => {
                five.push((x, 0, String::new()));
                guessed.push(k);
            }
        }
    }
    Some((five, guessed))
}

/// Tolerance of [`fan_grid`] (pixels): when only part of the type text is read, the center shifts by a dozen or
/// so pixels.
const FAN_PAIR_TOL: i32 = 30;
/// When the mouse is on one card of the fan, the distance each of its neighbours is pushed outward (see
/// [`FAN_HOVER_GAP`]).
const FAN_HOVER_PUSH: i32 = 47;

/// Three type-text positions deviate from the fixed positions of the usual three cards ([`CARD_X`]): it may be
/// the five fanned cards of a potion kit (large) with the grey `普通藥劑` ("normal potion") at both ends not read
/// (2026-11-03 ranked game, 21:27 and 4 other games: the fan was taken as three cards and the middle card was
/// read as effect text). This is only the threshold for whether to read the type text again (about 40 ms
/// more); whether it is a fan is decided by [`fan_slots`]: with the usual three cards, the card the mouse is on
/// and its neighbour are off by about 50 pixels at most (4113 reads tonight), and the 22 archived frames where a
/// fan was read as three cards are off by at least 73.
fn maybe_fan(slots: &[Slot]) -> bool {
    slots.len() == 3 && slots.iter().zip(CARD_X).any(|(s, x)| (s.0 - x).abs() > FAN_OFF)
}

/// When the spacing between two adjacent fanned cards reaches this, the mouse is on one of them and has pushed
/// the other away (normally 206-218, pushed away about 255).
const FAN_HOVER_GAP: i32 = 240;

/// The offset threshold (pixels) of [`maybe_fan`].
const FAN_OFF: i32 = 60;

/// The three positions are closer to the fan's corresponding three slots (which three is decided by the middle
/// card) than to the fixed positions of the usual three cards: with the usual three cards and the mouse on one
/// of the left two (1157, 1370, 1629) it is not a fan, while the middle three of a fan (1085, 1343, 1608;
/// 1132, 1343, 1562) are.
fn fan_closer(slots: &[Slot]) -> bool {
    let Some((five, guessed)) = fan_fill(slots) else { return false };
    let fan: i32 = (0..5).filter(|k| !guessed.contains(k)).map(|k| (five[k].0 - (FAN_MID_X + (k as i32 - 2) * FAN_STEP)).abs()).sum();
    let card: i32 = slots.iter().zip(CARD_X).map(|(s, x)| (s.0 - x).abs()).sum();
    fan < card
}

/// The spacing between adjacent cards all looks like a fan (about 212, about 255 for the slot the mouse is on;
/// when only part of the type text is read the center shifts by a dozen or so pixels) and they are about equal
/// to each other: no card missed in the middle and no extra non-card text.
fn fan_spaced(slots: &[Slot]) -> bool {
    let gaps: Vec<i32> = slots.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let (lo, hi) = (gaps.iter().min().copied().unwrap_or(0), gaps.iter().max().copied().unwrap_or(0));
    !gaps.is_empty() && lo >= 180 && hi <= 290 && hi - lo <= 80
}

/// Infer the missing cards from three or four consecutive fanned cards: which of the cards found is the exact
/// middle one ([`FAN_MID`]) decides which slots of the fan these are; the missing positions are multiples of
/// [`FAN_STEP`] outward from the middle card. Returns the five cards and the indices of the inferred ones.
///
/// It does not use the height of the type text: although the fan is highest in the middle and gets lower toward
/// the sides, the card at an end rises when the mouse is on it, and in the four-card samples of 2026-11-03,
/// judging by which end is lower got two wrong and the other two differed in height by only 1-2 pixels.
fn fan_fill(found: &[Slot]) -> Option<(Vec<Slot>, Vec<usize>)> {
    if !(3..=4).contains(&found.len()) || !fan_spaced(found) {
        return None;
    }
    let mids: Vec<usize> = (0..found.len()).filter(|&i| FAN_MID.contains(&found[i].0)).collect();
    let [j] = mids[..] else { return None };
    // Which slot of the fan the first card found is in.
    let first = 2usize.checked_sub(j)?;
    if first + found.len() > 5 {
        return None;
    }
    let mid = found[j].0;
    let last = first + found.len() - 1;
    let hover_mid = j > 0 && j + 1 < found.len() && mid - found[j - 1].0 >= FAN_HOVER_GAP && found[j + 1].0 - mid >= FAN_HOVER_GAP;
    let (mut five, mut guessed) = (Vec::with_capacity(5), Vec::new());
    for k in 0..5 {
        if (first..=last).contains(&k) {
            five.push(found[k - first].clone());
            continue;
        }
        // Normally push outward from the exact middle card (right for all the four-card samples of
        // 2026-11-03). When the mouse is on the middle card (the gaps on both sides are about 255), the whole
        // rows on both sides are pushed out about 47, so pushing from the middle would be off by 47 and the
        // card name could not be read (14:24 learn-0084, 21:27 frame-0061); push from the adjacent card instead.
        let x = if hover_mid && k < first {
            found[0].0 - (first - k) as i32 * FAN_STEP
        } else if hover_mid {
            found[found.len() - 1].0 + (k - last) as i32 * FAN_STEP
        } else {
            mid + (k as i32 - 2) * FAN_STEP
        };
        five.push((x, 0, String::new()));
        guessed.push(k);
    }
    Some((five, guessed))
}

/// Read the choice cards: first the cards found through the type text, then cards whose type text is covered
/// are filled in at default positions.
fn read_cards(text: &Text, category: Category, anchors: &[(i32, i32, String)], tooltip: &str) -> Vec<Card> {
    let mut slots: Vec<(i32, i32, String)> = anchors.to_vec();
    if slots.len() == 2 {
        // Type text was read for two cards: the third is the slot, among the default positions, farthest from
        // them.
        let missing = CARD_X.iter().copied().max_by_key(|&x| slots.iter().map(|(s, _, _)| (s - x).abs()).min().unwrap_or(0));
        if let Some(x) = missing {
            slots.push((x, 0, String::new()));
            slots.sort_by_key(|s| s.0);
        }
    }
    if slots.len() == 1 && (slots[0].0 - CARD_X[1]).abs() > 60 {
        // Only one, and not in the exact middle: it may be the card taking effect (glow animation), or the type
        // text of the other two may not have been read. Try all three default positions, and if in the end two
        // card names cannot be read, treat it as resolving.
        let only = slots[0].clone();
        slots = CARD_X.iter().map(|&x| if (x - only.0).abs() < 100 { only.clone() } else { (x, 0, String::new()) }).collect();
    }
    if slots.is_empty() {
        return vec![];
    }
    // Four or more type texts: the five fanned cards from opening a potion kit (large). Read only when the
    // positions of all five were found, otherwise the positions would be misaligned and the advice would point
    // at the wrong card, so all five are marked unknown. For when only four were found, see [`fan_slots`].
    let mut inferred: Vec<usize> = Vec::new();
    // The band of type text is read again (about 40 ms) only when the first read is not the usual three cards
    // nor five: three positions off the usual three ([`maybe_fan`]), or four, or six or more. Every four-card hand
    // falls in there (read as three, slot 0 is at most 1104, 101 from 1205, and slots 1 to 3 are at least 1259,
    // 1485 and 1692 against 1205, 1416 and 1629), so the usual three cards and the full fan read as before.
    let three = maybe_fan(&slots);
    let merged = ((slots.len() >= 4 && slots.len() != 5) || three).then(|| merge_reads(&slots, &card_anchors(&fan_band_lines(text))));
    // The four-card hand of a chain step goes first: its positions also fit the fan with the mouse on its last card.
    let four = merged.as_deref().and_then(hand4_merged);
    if let Some((hand, guess)) = four {
        slots = hand;
        inferred = guess;
    } else if let Some(merged) = &merged {
        match fan_slots(merged) {
            Some((five, guess)) => {
                slots = five;
                inferred = guess;
            }
            // Three: not a fan, read as the usual three cards.
            None if three => {}
            None => {
                let why = format!("藥劑箱（大）的五張卡只找到 {} 張的位置", slots.len());
                // The positions are unreliable: do not draw the box on these cards (x is 0).
                return (0..5).map(|_| Card { category, index: None, id: None, en: None, zh: None, how: None, raw_name: String::new(), raw_type: String::new(), raw_effect: String::new(), why: Some(why.clone()), x: 0, dy: 0, raised: false }).collect();
            }
        }
    }
    // Half width of the card-name crop: when the cards are packed tightly (about 200 pixels per card in the
    // five-card fan) it must not reach into a neighbouring card.
    let gap = slots.windows(2).map(|w| w[1].0 - w[0].0).min().unwrap_or(500);
    let half = if slots.len() > 3 { (gap / 2 - 4).clamp(80, 125) } else { 125 };
    // Card name: the normal and the raised positions (plus one more for the five-card fan), each reading the
    // original image (2x, 3x) and the black-on-white image at two thresholds (see [`inked`]); on a golden card
    // face the original often cannot be read, and when a neighbouring card presses on it the black-on-white
    // image tends to drop characters.
    let tops: Vec<i32> = if slots.len() == 5 { vec![NAME_TOPS[0], NAME_TOPS[1], FAN_NAME_TOP] } else { NAME_TOPS.to_vec() };
    let per_card = 4 * tops.len();
    let mut crops: Vec<(Rect, Prep, f32)> = Vec::new();
    for (k, (x, dy, _)) in slots.iter().enumerate() {
        for &top in &tops {
            let r = if slots.len() == 5 && k == 0 { fan_left_name(*x, top, *dy, half) } else { card_name(*x, top, half) };
            for q in [85, 90] {
                crops.push((r, Prep::Inked(q), 2.0));
            }
            crops.push((r, Prep::Plain, 2.0));
            crops.push((r, Prep::Plain, 3.0));
        }
    }
    let names_read = text.crops(&crops);
    let effects = text.batch(&slots.iter().map(|(x, dy, _)| (card_effect(*x, *dy, half), 2.0)).collect::<Vec<_>>());
    let tip = match_name(tooltip, category, false, None, None).filter(|(_, how)| *how == How::Name).map(|(i, _)| i);
    let mut cards = Vec::new();
    for (k, (x, dy, type_text)) in slots.iter().enumerate() {
        let effect = joined(&effects[k]);
        let (type_cat, type_rarity) = type_of(type_text);
        // The card matched must not conflict with the archetype suffix, the potion kit's size and rarity, or the rarity
        // at the bottom of the card.
        let verify = |i: usize, raw_name: &str| -> Result<(), String> {
            let name = &names().cards[i];
            let zh = name.zh.as_deref().unwrap_or("");
            if !species_ok(name, raw_name, &effect) {
                return Err(format!("對到「{zh}」，但種族字尾沒讀到、效果文字也沒印同一個種族"));
            }
            if !kit_ok(name, raw_name, &effect) {
                return Err(format!("對到「{zh}」，但大小或「稀有」字樣讀不到或不符"));
            }
            if let (Some(seen), Some(r)) = (type_rarity, name.rarity)
                && seen != r
                && r != PotionRarity::Supreme
            {
                return Err(format!("對到「{zh}」，但卡底是「{type_text}」，稀有度不符"));
            }
            Ok(())
        };
        let attempt_one = |name_lines: &[&Line]| -> Result<(usize, How), String> {
            let raw_name = name_lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ");
            // The card name is off the card's center: one side is covered by a neighbouring card.
            let clipped = match (name_lines.iter().map(|l| l.rect.x).min(), name_lines.iter().map(|l| l.rect.x + l.rect.w).max()) {
                (Some(l), Some(r)) => ((l + r) / 2 - x).abs() > 15,
                _ => false,
            };
            // The character count inferred from the width of the card-name line (when OCR drops characters, the
            // original length can still be told).
            let est = (name_lines.len() == 1).then(|| (name_lines[0].rect.w as f32 / CHAR_W).round() as usize);
            let mut found = match_name(&raw_name, category, clipped, tip, est);
            if found.is_none() && norm(&raw_name).chars().count() < 2 && slots.len() == 1 {
                found = tip.map(|i| (i, How::Tooltip));
            }
            let (i, how) = found.ok_or_else(|| if norm(&raw_name).is_empty() { "卡名讀不到".to_string() } else { "名稱表裡沒有相符的繁中名".to_string() })?;
            verify(i, &raw_name).map(|_| (i, how))
        };
        // The area may contain stray text from the card art, or the raised card's effect text: merge the lines
        // of the same row into one segment and compare segment by segment, preferring an exact card name and
        // then a partial match.
        let attempt = |lines: &[Line]| -> Result<(usize, How), String> {
            let mut sorted: Vec<&Line> = lines.iter().collect();
            sorted.sort_by_key(|l| (l.rect.y, l.rect.x));
            let mut rows: Vec<Vec<&Line>> = Vec::new();
            for l in sorted {
                match rows.last_mut() {
                    Some(row) if (row[0].rect.y - l.rect.y).abs() < 12 => row.push(l),
                    _ => rows.push(vec![l]),
                }
            }
            rows.iter_mut().for_each(|r| r.sort_by_key(|l| l.rect.x));
            let tries: Vec<Result<(usize, How), String>> = rows.iter().map(|r| attempt_one(r)).collect();
            if let Some(ok) = tries.iter().find(|t| matches!(t, Ok((_, How::Name)))) {
                return ok.clone();
            }
            if let Some(ok) = tries.iter().find(|t| t.is_ok()) {
                return ok.clone();
            }
            // None matched: report the segment that looks most like a card name (one with an archetype suffix or
            // rarity problem first, then the first segment).
            tries.iter().find(|t| t.as_ref().is_err_and(|e| e.starts_with("對到"))).cloned().or_else(|| tries.into_iter().next()).unwrap_or_else(|| Err("卡名讀不到".into()))
        };
        let tries = &names_read[per_card * k..per_card * (k + 1)];
        let mut card = Card { category, index: None, id: None, en: None, zh: None, how: None, raw_name: joined(&tries[0]), raw_type: type_text.clone(), raw_effect: effect.clone(), why: None, x: *x, dy: *dy, raised: *dy <= RAISED_DY };
        if type_cat.is_some_and(|c| c != category) {
            card.why = Some(format!("類型字「{type_text}」和標題不符"));
            cards.push(card);
            continue;
        }
        // Try every way of reading: the cards matched must agree (different reads matching different cards
        // counts as unknown), an exact name is preferred; when none works, report the most informative reason.
        let results: Vec<Result<(usize, How), String>> = tries.iter().map(|t| attempt(t)).collect();
        let mut hits: Vec<(usize, How)> = results.iter().filter_map(|r| r.as_ref().ok().copied()).collect();
        hits.sort_by_key(|&(i, how)| (i, how != How::Name));
        hits.dedup_by_key(|h| h.0);
        let found = match &hits[..] {
            [hit] => Ok(*hit),
            [] => Err(results.iter().filter_map(|r| r.as_ref().err()).find(|e| e.starts_with("對到")).or_else(|| results.iter().filter_map(|r| r.as_ref().err()).find(|e| *e != "卡名讀不到")).cloned().unwrap_or_default()),
            many => Err(format!("不同讀法對到不同的卡（{}）", many.iter().map(|(i, _)| names().cards[*i].zh.clone().unwrap_or_default()).collect::<Vec<_>>().join("、"))),
        };
        // Every way of reading failed to match a card name (not matching different cards): look at the start of
        // the effect text.
        // Different reads matching different cards: the effect text starts with the card name, so the one card of
        // those whose whole name ([`name_in_effect`]) starts it settles which (2026-11-06_13-46-25: `丶皮革拘束帶`
        // matched both `生皮革拘束帶` and `鞣製皮革拘束帶`, the effect began `鞣製皮革拘束帶`; `啁膽汁溶液` matched
        // `青膽汁溶液` and `黏稠膽汁溶液`, the effect began `黏稠膽汁溶液`). A name from the effect that is none of
        // them leaves the card unknown.
        let found = match found {
            Err(why) if hits.is_empty() => name_in_effect(&effect, category).filter(|&i| verify(i, "").is_ok()).map(|i| (i, How::Effect)).ok_or(why),
            Err(why) if hits.len() > 1 => effect_among(&hits, &effect, category).filter(|&i| verify(i, "").is_ok()).map(|i| (i, How::Effect)).ok_or(why),
            other => other,
        };
        match found {
            Ok((i, how)) => card.set(i, how),
            Err(why) => {
                if let Some(t) = tries.iter().find(|t| !t.is_empty()) {
                    card.raw_name = joined(t);
                }
                card.why = Some(if why.is_empty() { "卡名讀不到".into() } else { why });
            }
        }
        cards.push(card);
    }
    // The card name read for an inferred card is the same as a neighbour's: most likely the position is off and
    // the neighbour was read, so do not guess.
    for k in inferred {
        if let Some(i) = cards[k].index
            && [k.wrapping_sub(1), k + 1].iter().any(|&j| cards.get(j).is_some_and(|c| c.index == Some(i)))
        {
            let c = &mut cards[k];
            (c.index, c.id, c.en, c.zh, c.how) = (None, None, None, None, None);
            c.why = Some("位置是推出來的，讀到的卡名和鄰卡相同".into());
        }
    }
    // Fewer than two card names were read (except the single card of a tutorial game): most likely a resolve
    // animation, not a choice screen.
    let named = cards.iter().filter(|c| c.index.is_some() || norm(&c.raw_name).chars().count() >= 2).count();
    if named == 0 || (cards.len() > 1 && named < 2) {
        return vec![];
    }
    cards
}

impl Card {
    fn set(&mut self, i: usize, how: How) {
        let n = &names().cards[i];
        self.index = Some(i);
        self.id = n.id;
        self.en = Some(n.en.clone());
        self.zh = n.zh.clone();
        self.how = Some(how);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inferring the fifth card when only four of the potion kit (large) were found: the four-card samples of
    /// 2026-11-03 (the positions where the type text was first read; the center of some partial type texts is
    /// off by a dozen or so pixels), which end is missing and the inferred position (actual 1768, 873, 1815,
    /// 872).
    #[test]
    fn fills_the_missing_end_of_the_fan() {
        let s = |xs: [i32; 4]| xs.iter().map(|&x| (x, 0, "普通藥劑".to_string())).collect::<Vec<_>>();
        let fill = |xs| fan_fill(&s(xs)).map(|(five, k)| (five[k[0]].0, k[0]));
        // The 22:24 game, round 9: frame-0082 (right end missing), 0091 (left end missing), 0095 (right end
        // missing, mouse on card 1).
        assert_eq!(fill([930, 1132, 1344, 1562]), Some((1768, 4)));
        assert_eq!(fill([1109, 1297, 1515, 1757]), Some((873, 0)));
        assert_eq!(fill([930, 1178, 1390, 1620]), Some((1814, 4)));
        // Ranked game t058.50 (left end missing, mouse on card 5).
        assert_eq!(fill([1085, 1297, 1516, 1756]), Some((873, 0)));
        // A card missing in the middle (spacing over 420), spacing too uneven, the exact middle card not in the
        // middle: none are inferred.
        assert_eq!(fill([920, 1132, 1562, 1768]), None);
        assert_eq!(fill([920, 1060, 1344, 1562]), None);
        assert_eq!(fill([1000, 1212, 1424, 1636]), None);
        // Three (the left three): infers the right two.
        let (five, k) = fan_fill(&s([920, 1132, 1344, 1562])[..3]).unwrap();
        assert_eq!((five.iter().map(|s| s.0).collect::<Vec<_>>(), k), (vec![920, 1132, 1344, 1556, 1768], vec![3, 4]));
    }

    /// A fan where only the type text of the middle three was read (2026-11-03 ranked game 21:27, 17:16 and so
    /// on) counts as a fan; the usual three cards (including the offset when the mouse is on one) do not.
    #[test]
    fn type_labels_off_the_three_card_spots_are_placed_on_the_fan() {
        let s = |xs: &[i32]| xs.iter().map(|&x| (x, 0, "普通藥劑".to_string())).collect::<Vec<_>>();
        let xs = |five: &[Slot]| five.iter().map(|f| f.0).collect::<Vec<_>>();
        // Cards 2 and 4 (normal): the middle and the two ends are filled in from the layout.
        let (five, k) = fan_grid(&s(&[1132, 1562])).unwrap();
        assert_eq!((xs(&five), k), (vec![920, 1132, 1344, 1562, 1768], vec![0, 2, 4]));
        // Mouse on the middle card: cards 2 and 4 (1085, 1609), or cards 1, 2 and 4 (872, 1086, 1609).
        let (five, k) = fan_grid(&s(&[1085, 1609])).unwrap();
        assert_eq!((xs(&five), k), (vec![873, 1085, 1344, 1609, 1815], vec![0, 2, 4]));
        let (five, k) = fan_grid(&s(&[872, 1086, 1609])).unwrap();
        assert_eq!((xs(&five), k), (vec![872, 1086, 1344, 1609, 1815], vec![2, 4]));
        // With the usual three cards only two were read (including the offset when the mouse is on a card): not
        // pushed into five.
        for pair in [[1205, 1629], [1205, 1416], [1416, 1629], [1157, 1629], [1205, 1676], [1157, 1370]] {
            assert!(fan_grid(&s(&pair)).is_none(), "{pair:?}");
        }
    }

    /// The last effect line of a hovered card risen into the type band is not taken for its type text (the
    /// 2026-11-06_16-26-30 game's frame-0011: `具 + 25 數 量` above `魔 法 藥 劑` on the third card).
    #[test]
    fn an_effect_line_risen_into_the_type_band_is_not_a_type() {
        let line = |text: &str, x: i32, y: i32, w: i32| Line { text: text.to_string(), rect: Rect::new(x, y, w, 26) };
        let types = [
            line("普 通 藥 劑", 1160, 1282, 90),
            line("普 通 藥 劑", 1372, 1280, 90),
            line("具 + 25 數 量", 1590, 1222, 92),
            line("魔 法 藥 劑", 1591, 1232, 90),
        ];
        let anchors = card_anchors(&types);
        assert_eq!(anchors.iter().map(|a| a.2.as_str()).collect::<Vec<_>>(), ["普 通 藥 劑", "普 通 藥 劑", "魔 法 藥 劑"]);
        assert_eq!(type_of(&anchors[2].2), (Some(Category::Potion), Some(PotionRarity::Magic)));
        // The merged text as read live: an instrument by its `具`.
        assert_eq!(type_of("具 + 25 數 量魔 法 藥 劑").0, Some(Category::Instrument));
    }

    /// Different reads of a card name matched different cards: the effect text's start settles it, only among
    /// them (the 2026-11-06_13-46-25 game's `丶皮革拘束帶` and `啁膽汁溶液`, with their logged effect text).
    #[test]
    fn the_effect_text_settles_a_name_read_as_two_cards() {
        let at = |zh: &str| names().cards.iter().position(|c| c.zh.as_deref() == Some(zh)).unwrap();
        let hits = |zh: &[&str]| zh.iter().map(|z| (at(z), How::Partial)).collect::<Vec<_>>();
        let leather = "鞣 製 皮 革 拘 束 帶 同 時 擁 有 普 通 、 魔 冫 勁 敵 怪 物 時 , 每 回 所 有 怪 物 + 20 活";
        assert_eq!(effect_among(&hits(&["生皮革拘束帶", "鞣製皮革拘束帶"]), leather, Category::Instrument), Some(at("鞣製皮革拘束帶")));
        let bile = "黏 稠 膽 汁 溶 液 i 選 擇 1 組 怪 物 , 將 側 怪 物 變 異 為 與 其 的 怪 物 , 兩 者 各 + 3 活 性";
        assert_eq!(effect_among(&hits(&["青膽汁溶液", "黏稠膽汁溶液"]), bile, Category::Potion), Some(at("黏稠膽汁溶液")));
        // The effect names a card that is none of the cards matched, or no card: nothing.
        assert_eq!(effect_among(&hits(&["青膽汁溶液", "生皮革拘束帶"]), bile, Category::Potion), None);
        assert_eq!(effect_among(&hits(&["青膽汁溶液", "黏稠膽汁溶液"]), "選 擇 1 組 怪 物", Category::Potion), None);
    }

    /// The result screen with its label crop misread: both buttons and `活性` in the wider crop are needed
    /// (2026-11-06_13-46-25 frame-0062 read `一 最 活 性`); one button alone, or a label without `活性`, is not.
    #[test]
    fn result_screen_from_both_buttons_and_the_wide_label() {
        assert!(both_result_buttons("結 束 手 術 開 始 手 術"));
        assert!(!both_result_buttons("開 始 手 術"));
        assert!(!both_result_buttons("結束手術"));
        assert!(wide_result_label("一 最 活 性"));
        assert!(wide_result_label("最 終 活 性"));
        assert!(!wide_result_label("一 最"));
        assert!(!wide_result_label(""));
    }

    /// Text of the endgame battle panel (the raw text of two frames from the 2026-11-03 recording and common OCR
    /// variants).
    #[test]
    fn battle_panel_text_is_parsed() {
        assert_eq!(battle_numbers("836.4K+381.9K/999.9M"), (Some(836_400.0), Some(381_900.0), Some(999_900_000.0)));
        assert_eq!(battle_numbers("568.5K +221.2K /854.0K"), (Some(568_500.0), Some(221_200.0), Some(854_000.0)));
        assert_eq!(battle_numbers("91.1K+18.2K/114.0K"), (Some(91_100.0), Some(18_200.0), Some(114_000.0)));
        assert_eq!(battle_numbers("238.8K+78.6K"), (Some(238_800.0), Some(78_600.0), None));
        assert_eq!(battle_numbers("下個獎勵"), (None, None, None));
        assert_eq!(battle_wave("3 /3"), Some((3, 3)));
        assert_eq!(battle_wave("波次 1/3"), Some((1, 3)));
        assert_eq!(battle_wave("4/3"), None);
        // "1" read as l, I or |: must not become wave 3; without a "/" it does not count.
        assert_eq!(battle_wave("l /3"), Some((1, 3)));
        assert_eq!(battle_wave("| /3"), Some((1, 3)));
        assert_eq!(battle_wave("波次 2 /3"), Some((2, 3)));
        assert_eq!(battle_wave("313"), None);
        // Common OCR readings: "+" read as ".", "/" read as "'" or "1", "/3" read as "/33" or "/9".
        // "50K" is not on the reward ladder: the next tier cannot be read (no guessing which one).
        assert_eq!(battle_numbers("563.5K.221.2K'50K"), (Some(563_500.0), Some(221_200.0), None));
        // "/" read as "1": the ordinary thresholds also drop the extra 1.
        assert_eq!(battle_numbers("568.5K+221.2K1854.0K"), (Some(568_500.0), Some(221_200.0), Some(854_000.0)));
        assert_eq!(battle_numbers("91.1K+18.2K1114.0K"), (Some(91_100.0), Some(18_200.0), Some(114_000.0)));
        // The first number lost its K: it still takes a slot, so the later ones do not shift forward (activity
        // cannot be read, this time it does not count).
        assert_eq!(battle_numbers("836.4+381.9K/999.9M"), (None, Some(381_900.0), Some(999_900_000.0)));
        // 5 or more digits with no decimal point: cannot be read, not taken as 83.6M.
        assert_eq!(battle_numbers("83641K+381.9K"), (None, Some(381_900.0), None));
        // 1.12M is displayed with one decimal place.
        assert_eq!(battle_numbers("900.0K+200.0K/1.1M"), (Some(900_000.0), Some(200_000.0), Some(1_120_000.0)));
        assert_eq!(battle_numbers("836.4K+381.9K 1999.9M"), (Some(836_400.0), Some(381_900.0), Some(999_900_000.0)));
        assert_eq!(battle_numbers("836.4K+381SK"), (Some(836_400.0), None, None));
        assert_eq!(battle_numbers("8364K438t9K"), (Some(836_400.0), None, None));
        assert_eq!(battle_numbers("836 · 4K + 381.9K"), (Some(836_400.0), Some(381_900.0), None));
        assert_eq!(battle_numbers("999 、 9M"), (None, None, None));
        // A "1" inside a number read as I, l, | or i (frame-battle-0090 of the 2026-11-06 12:23 game): 482.1K, not
        // a 482 without a unit; a letter outside a number is left alone.
        assert_eq!(battle_numbers("1. OM + 482 · IK / 999 · 9M"), (Some(1_000_000.0), Some(482_100.0), Some(999_900_000.0)));
        assert_eq!(battle_numbers("1 · OM + 482. IK / 999.9M"), (Some(1_000_000.0), Some(482_100.0), Some(999_900_000.0)));
        assert_eq!(battle_numbers("1.0M+48l.3K"), (Some(1_000_000.0), Some(481_300.0), None));
        assert_eq!(battle_numbers("5|8.5K+22i.2K"), (Some(518_500.0), Some(221_200.0), None));
        assert_eq!(battle_numbers("I 836.4K"), (Some(836_400.0), None, None));
        // The five ways of reading frame-battle-0090 as logged (vorax_battle_end, log.jsonl line 444): the vote was
        // 48.2K from the one "482 K" alone, since the four "482 · IK" had no value.
        let texts = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let b = battle_from_texts(
            &texts(&["", "", "", "", "/ 3"]),
            &texts(&["1. OM + 482 · IK / 999 · 9M", "1 · OM + 482 · IK / 999 , 9M", "1. OM + 482. IK / 999.9M", "1 · OM + 482 K / 999 一 9M", "1 · OM + 482. IK / 999.9M"]),
            &texts(&["( + 1 5 % )", "( + 15 % )", "( + 1 5 % )", "( + 1 5 % )", "( + 1 5 % )"]),
        );
        assert_eq!((b.activity, b.bonus, b.next, b.percent), (Some(1_000_000.0), Some(482_100.0), Some(999_900_000.0), Some(15)));
        assert_eq!(battle_wave("3/33"), Some((3, 3)));
        assert_eq!(battle_wave("3/9"), Some((3, 3)));
        assert_eq!(vote(&[Some(1), None, Some(2), Some(2), Some(1), Some(2)]), Some(2));
        assert_eq!(vote(&[Some(1), Some(2)]), Some(1));
        assert_eq!(vote::<i32>(&[None, None]), None);
        assert_eq!(battle_percent("(+15%)"), Some(15));
        assert_eq!(battle_percent("(+0%)"), Some(0));
        assert_eq!(battle_percent("(-5%)"), Some(-5));
        assert_eq!(battle_percent(""), None);
        // "+" read as "4".
        assert_eq!(battle_percent("(415%)"), Some(15));
        assert_eq!(battle_percent("(420%)"), Some(20));
        assert_eq!(battle_percent("(445%)"), Some(45));
        assert_eq!(battle_percent("(999%)"), None);
    }

    #[test]
    fn three_type_labels_of_the_fan_are_told_from_three_cards() {
        let s = |xs: [i32; 3]| xs.iter().map(|&x| (x, 0, "普通藥劑".to_string())).collect::<Vec<_>>();
        for fan in [[1132, 1343, 1562], [1085, 1343, 1608], [1084, 1344, 1608], [920, 1132, 1344]] {
            assert!(maybe_fan(&s(fan)) && fan_closer(&s(fan)), "{fan:?}");
            let (five, k) = fan_fill(&s(fan)).unwrap();
            assert_eq!((five.len(), k.len()), (5, 2), "{fan:?}");
        }
        // Mouse on the exact middle card (the whole rows on both sides are pushed outward): the two ends are
        // pushed from the adjacent card (actual 873, 1814).
        let (five, _) = fan_fill(&s([1086, 1344, 1608])).unwrap();
        assert_eq!((five[0].0, five[4].0), (874, 1820));
        // The same with four (the four of 14:24 learn-0084; 21:27 frame-0061 with the right end missing).
        let four = |xs: [i32; 4]| xs.iter().map(|&x| (x, 0, String::new())).collect::<Vec<_>>();
        assert_eq!(fan_fill(&four([1086, 1344, 1608, 1815])).unwrap().0[0].0, 874);
        assert_eq!(fan_fill(&four([872, 1085, 1343, 1608])).unwrap().0[4].0, 1820);
        for three in [[1205, 1416, 1629], [1157, 1370, 1629], [1157, 1416, 1629], [1156, 1418, 1676], [1210, 1460, 1670]] {
            assert!(!(maybe_fan(&s(three)) && fan_closer(&s(three))), "{three:?}");
        }
    }

    /// Gold and red names that are more saturated than the reference colours (2026-11-03 ranked game, 28k
    /// instrument screen) are still recognized; those that fall between two colours are not.
    #[test]
    fn vivid_names_keep_their_rarity() {
        assert_eq!(rarity_of([236.0, 177.0, 82.0]), Some(Rarity::Rare));
        assert_eq!(rarity_of([241.0, 76.0, 75.0]), Some(Rarity::Boss));
        assert_eq!(rarity_of([137.0, 183.0, 211.0]), Some(Rarity::Magic));
        // The gold name of the monster `腐囊異獸` (13-31-44 frame-0045, 40.1 from rare).
        assert_eq!(rarity_of([236.4, 176.8, 80.4]), Some(Rarity::Rare));
        // Between gold and red (a name that is fading).
        assert_eq!(rarity_of([222.0, 120.0, 98.0]), None);
    }

    /// Back-derivation of the tank values (raw text read in the 2026-11-03 ranked game): fix only one field, and
    /// adopt only a unique solution.
    #[test]
    fn tank_numbers_are_repaired_only_when_unique() {
        let got = |v: &str, t: &str| {
            let n = tank_numbers(v, t);
            (n.activity, n.quantity, n.total, n.repaired.is_some())
        };
        // The first character right next to the heart is unrecognized, and a thin "1" was dropped as a speck.
        assert_eq!(got("?99x198", "39402"), (Some(199), Some(198), Some(39402), true));
        assert_eq!(tank_numbers("?99x198", "39402").repaired.as_deref(), Some("活性「?99」→199"));
        assert_eq!(got("x36", "36"), (Some(1), Some(36), Some(36), true));
        assert_eq!(got("?33x163", "21679"), (Some(133), Some(163), Some(21679), true));
        assert_eq!(got("1?9x36", "7164"), (Some(199), Some(36), Some(7164), true));
        assert_eq!(got("??9x198", "39402"), (Some(199), Some(198), Some(39402), true));
        assert_eq!(got("1x?1", "81"), (Some(1), Some(81), Some(81), true));
        assert_eq!(got("55x372", "?0460"), (Some(55), Some(372), Some(20460), true));
        assert_eq!(got("4?6x1114", "507984"), (Some(456), Some(1114), Some(507984), true));
        // Complete and self-consistent reads are used as read, with no repair recorded.
        assert_eq!(got("1x36", "36"), (Some(1), Some(36), Some(36), false));
        // Several solutions: 12×2 or 2×12.
        assert_eq!(got("2x2", "24"), (Some(2), Some(2), Some(24), false));
        // No solution: as read (inconsistent or a missing field, which the solver does not use).
        assert_eq!(got("?99x198", "39403"), (None, None, Some(39403), false));
        assert_eq!(got("2x3", "7"), (Some(2), Some(3), Some(7), false));
        // Both fields need repair, more than 2 `?`, more than 2 repairs, the x unreadable along with another
        // glyph, a clean field starting with 0: none are back-derived. (The x unreadable alone, `5?204` with a
        // total of 1020, is found from the total: see `the_x_is_found_from_the_total`.)
        assert_eq!(got("?99x19?", "39402"), (None, None, Some(39402), false));
        assert_eq!(got("x36", "3?"), (None, None, None, false));
        assert_eq!(got("???x198", "39402"), (None, None, Some(39402), false));
        assert_eq!(got("??x198", "39402"), (None, None, Some(39402), false));
        assert_eq!(got("5?2?4", "1020"), (None, None, Some(1020), false));
        // All three fields were read cleanly but do not multiply: the activity and count fields may not have a "1"
        // added to force a fit (true value 39×7=273 with the total read as 973; true value 2×63=126 with the
        // count read as 6).
        assert_eq!(got("39x7", "973"), (Some(39), Some(7), Some(973), false));
        assert_eq!(got("2x6", "126"), (Some(2), Some(6), Some(126), false));
        // Adding a "1" to the total activity is allowed, but it has to wait until the tanks' sum matches the
        // final activity (needs_sum).
        let n = tank_numbers("5x32", "60");
        assert_eq!((n.activity, n.quantity, n.total, n.needs_sum), (Some(5), Some(32), Some(160), true));
        assert!(!tank_numbers("x36", "36").needs_sum && !tank_numbers("?99x198", "39402").needs_sum);
        assert_eq!(got("05x2", "1?"), (Some(5), Some(2), None, false));
        // After repair it must not start with 0, and activity and count must be at least 1.
        assert_eq!(got("?x5", "0"), (None, None, Some(0), false));
    }

    /// The total activity unreadable but "activity x count" clean: the product, marked as such (2026-11-06 1080p
    /// game, tank 3 `赤螳螂` from round 3 on, before the digits were read from the frame itself).
    #[test]
    fn tank_total_from_the_product() {
        let n = tank_numbers("10x204", "20?");
        assert_eq!((n.activity, n.quantity, n.total, n.needs_sum), (Some(10), Some(204), Some(2040), true));
        assert_eq!(n.repaired.as_deref(), Some("總活性「20?」→2040（由活性×數量補上）"));
        let got = |v: &str, t: &str| tank_numbers(v, t).total;
        // No digit read in the total: nothing checks the product (activity 10 misread as 18 would pass).
        assert_eq!(got("10x204", ""), None);
        assert_eq!(got("18x204", ""), None);
        assert_eq!(got("10x204", "?"), None);
        assert_eq!(got("10x204", "??"), None);
        // Lined up from the left, each `?` one digit, one `?` at most standing for two.
        assert_eq!(got("10x204", "2?40"), Some(2040));
        assert_eq!(got("10x204", "30?"), None);
        assert_eq!(got("10x204", "2040?"), None);
        // (`15?` for 1152 through [`repair`] is its own rule, a thin leading "1" dropped: the product alone
        // does not take it.)
        assert!(from_product("32x36", "15?").is_none());
        assert_eq!(got("361x1899", "685?"), None);
        assert_eq!(got("361x1899", "6855?"), Some(685_539));
        // A total read cleanly that does not match is not overwritten (either row may be the misread one).
        assert_eq!(got("10x204", "2050"), Some(2050));
        assert!(tank_numbers("10x204", "2050").repaired.is_none());
        // "activity x count" not clean: nothing to take the product of.
        assert_eq!(got("1?x204", "20?"), None);
    }

    /// A tank whose total came from the product (`needs_sum`) counts only when the final activity is read, not
    /// jumping, and equals the tanks' sum; otherwise its values fall back to what was read, with a problem.
    #[test]
    fn a_product_total_waits_for_the_final_activity() {
        let tank = |v: &str, t: &str| {
            let n = tank_numbers(v, t);
            Tank { activity: n.activity, quantity: n.quantity, total: n.total, repaired: n.repaired, raw_values: v.into(), raw_total: t.into(), archetype: Some(Archetype::Pest), rarity: Some(Rarity::Magic), ..Tank::default() }
        };
        let settle = |ultimate: Option<i64>, pending: bool| {
            let mut r = Reading { kind: Kind::Potion, round: Some(3), ultimate, ultimate_pending: pending, panel_covered: false, next_reward: None, rerolls: None, tanks: vec![tank("1x261", "261"), tank("10x204", "20?")], held: vec![], cards: vec![], tank_sum: None, problems: vec![], tooltip: String::new() };
            let mut problems = Vec::new();
            settle_sums(&mut r, &[1], &mut problems);
            (r.tanks[1].total, r.tank_sum, problems.len())
        };
        assert_eq!(settle(Some(2301), false), (Some(2040), Some(2301), 0));
        // Covered (unread), still jumping, or off: unknown, with its problem.
        assert_eq!(settle(None, false), (None, None, 1));
        assert_eq!(settle(Some(2000), true), (None, None, 1));
        assert_eq!(settle(Some(2302), false), (None, None, 1));
    }

    /// "activity x count" with the "x" unrecognized: the one `?` taken as the "x" when that makes the product
    /// the total read cleanly (the five such tanks of the 146 frames scaled to 1080).
    #[test]
    fn the_x_is_found_from_the_total() {
        let n = tank_numbers("2?72", "144");
        assert_eq!((n.activity, n.quantity, n.total, n.needs_sum), (Some(2), Some(72), Some(144), false));
        assert_eq!(n.repaired.as_deref(), Some("活性 x 數量「2?72」→2x72（由總活性 144 判定）"));
        let got = |v: &str, t: &str| {
            let n = tank_numbers(v, t);
            (n.activity, n.quantity)
        };
        assert_eq!(got("90?739", "66510"), (Some(90), Some(739)));
        assert_eq!(got("5?56", "280"), (Some(5), Some(56)));
        assert_eq!(got("5?204", "1020"), (Some(5), Some(204)));
        // The product does not match, the total has a `?`, the row has two `?` or an "x" too, or a side starts
        // with 0: unknown as before.
        assert_eq!(got("2?72", "145"), (None, None));
        assert_eq!(got("2?72", "14?"), (None, None));
        assert_eq!(got("3?0?1383", "484050"), (None, None));
        assert_eq!(got("2?72x1", "144"), (None, None));
        assert_eq!(got("2?072", "144"), (None, None));
        assert_eq!(got("?272", "272"), (None, None));
    }

    /// A card name with one wrong character picks the nearest; when the characters read could be another card
    /// with its name covered, do not guess.
    #[test]
    fn a_misread_name_is_not_taken_for_a_clipped_one() {
        let zh = |m: Option<(usize, How)>| m.and_then(|(i, _)| names().cards[i].zh.clone());
        assert_eq!(zh(match_name("清疸油膏", Category::Potion, false, None, None)).as_deref(), Some("清疽油膏"));
        assert_eq!(zh(match_name("化骨油膏", Category::Potion, false, None, None)).as_deref(), Some("化骨油膏"));
        // `膽汁溶液` could be `青膽汁溶液` or `黏稠膽汁溶液` with the front covered.
        assert_eq!(match_name("膽汁溶液", Category::Potion, false, None, None), None);
        // The `嗅` in `驚醒嗅鹽` read as `臭`, plus one more wrong character (raw OCR text from the 2026-11-04
        // ranked game).
        assert_eq!(zh(match_name("驚酉臭鹽", Category::Potion, false, None, None)).as_deref(), Some("驚醒嗅鹽"));
        // `麻` read as `數`, plus one more wrong character: without swapping the characters it is 2 off (only 1
        // is allowed within 7 characters), and only after the swap does it match.
        assert_eq!(zh(match_name("數不酊劑一覺醒者", Category::Potion, false, None, None)).as_deref(), Some("麻藥酊劑-覺醒者"));
    }

    /// When the card name is pressed on by a neighbouring card and cannot be read, the full name at the start of
    /// the effect text (raw OCR text from the 2026-11-03 ranked game).
    #[test]
    fn the_effect_text_names_a_hidden_card() {
        let zh = |i: Option<usize>| i.and_then(|i| names().cards[i].zh.clone());
        assert_eq!(zh(name_in_effect("混 合 活 蛭 溶 液 選 擇 1 組 怪 物 , 使 2 糹 稀 有 度 怪 物 + 31 冫 並", Category::Potion)).as_deref(), Some("混合活蛭溶液"));
        assert_eq!(zh(name_in_effect("數 藥 酊 劑 : 覺 醒 者 1 組 怪 物 , 變 異 為 覺 醒 者", Category::Potion)).as_deref(), Some("麻藥酊劑-覺醒者"));
        // The start is not a card name (effect text only) or the card name has a wrong character: no guessing.
        assert_eq!(name_in_effect("選 擇 1 組 怪 物 , 使 2 組 同 稀 有 度 怪 物 + 31 活 性", Category::Potion), None);
        assert_eq!(name_in_effect("清 疸 油 膏 選 擇 1 組 怪 物 + 20 活 性", Category::Potion), None);
    }

    /// `to_frame` is the inverse of `to_reference`: draw a white dot at the position `to_frame` computes on the
    /// frame, and after converting back to the reference size the dot's centroid lands on the original reference
    /// coordinates (within 1 pixel).
    #[test]
    fn to_frame_undoes_to_reference() {
        for (w, h) in [(2560, 1440), (1920, 1080), (3440, 1440), (1280, 1024)] {
            for (x, y) in [(1416.0, 1140.0), (566.0, 345.0), (2280.0, 345.0), (1625.0, 1362.0), (200.0, 600.0)] {
                let (fx, fy, hw, hh) = to_frame(w, h, (x, y, 10.0, 20.0));
                let k = h as f32 / H as f32;
                assert!((hw - 10.0 * k).abs() < 1e-3 && (hh - 20.0 * k).abs() < 1e-3);
                let mut img = Rgba { w, h, data: vec![0; w * h * 4] };
                // A square of radius 2 centered at (fx, fy) on the frame.
                let (cx, cy) = (fx.round() as i64, fy.round() as i64);
                for py in cy - 2..=cy + 2 {
                    for px in cx - 2..=cx + 2 {
                        if (0..w as i64).contains(&px) && (0..h as i64).contains(&py) {
                            let i = (py as usize * w + px as usize) * 4;
                            img.data[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                        }
                    }
                }
                let r = to_reference(&img);
                let (mut sx, mut sy, mut n) = (0.0, 0.0, 0.0);
                for ry in 0..H {
                    for rx in 0..W {
                        let v = f32::from(r.data[(ry * W + rx) * 4]);
                        (sx, sy, n) = (sx + v * rx as f32, sy + v * ry as f32, n + v);
                    }
                }
                if n == 0.0 {
                    // The left and right sides cropped off at width 3440: the reference screen cannot see them, so
                    // this point is not in the frame either.
                    assert!(!(0.0..w as f32).contains(&fx), "{w}x{h} ({x}, {y}) lost");
                    continue;
                }
                let (gx, gy) = (sx / n, sy / n);
                assert!((gx - x).abs() <= 1.0 && (gy - y).abs() <= 1.0, "{w}x{h}: ({x}, {y}) came back at ({gx:.2}, {gy:.2})");
            }
        }
    }

    /// Match a printed or OCR-read name against the potion table and return the English name of the card.
    fn potion(text: &str) -> Option<String> {
        match_name(text, Category::Potion, false, None, None).map(|(i, _)| names().cards[i].en.clone())
    }

    /// The unsimulated Supreme potions are found under every spelling the game or OCR may produce, never as a
    /// neighbouring card, and every one is a card the solver always takes.
    #[test]
    fn unsimulated_potions_match_their_variants() {
        let cards: [(&str, &[&str]); 12] = [
            ("Dissection I: Vermectomy", &["解剖術 I：除蠱術", "解剖術 I:除蠱術", "解剖術I除蠱術", "解剖術Ⅰ：除蠱術", "解剖術 l：除蠱術", "解剖術 1：除蠱術", "解剖術 I：除術", "解剖術I除蟲術"]),
            ("Dissection II: Daemonectomy", &["解剖術 II：驅魔術", "解剖術 II:驅魔術", "解剖術II驅魔術", "解剖術Ⅱ：驅魔術", "解剖術 ll：驅魔術", "解剖術 11：驅魔術", "解剖術 II：驅術", "解剖術 Il：驅魔術"]),
            ("Dissection III: Craniotomy", &["解剖術 III：開顱術", "解剖術 III:開顱術", "解剖術III開顱術", "解剖術Ⅲ：開顱術", "解剖術 lll：開顱術", "解剖術 111：開顱術", "解剖術 III：開術", "解剖術 Ill：開顱術"]),
            ("Dissection Infusion: Morbid Corpse", &["解剖浸液：病軀", "解剖浸液:病軀", "解剖浸液病軀", "解剖 浸液：病軀", "解剖浸液：病"]),
            ("Dissection Infusion: Intact Skeleton", &["解剖浸液：完整骨架", "解剖浸液:完整骨架", "解剖浸液完整骨架", "解剖浸液：完整骨"]),
            ("Xenopanacea: Osteotrophy", &["異種萬靈藥：壯骨", "異種萬靈藥:壯骨", "異種萬靈藥壯骨", "異種萬 靈藥：壯骨", "異種萬靈藥：壯"]),
            ("Xenopanacea: Brood", &["異種萬靈藥：育雛", "異種萬靈藥:育雛", "異種萬靈藥育雛", "異種萬靈藥：育"]),
            ("First Rite Holy Water: Confession", &["第一儀式聖水：告解", "第一儀式聖水:告解", "第一儀式聖水告解", "第一儀式聖水：告"]),
            ("Ritual Holy Water: Flagellation", &["第二儀式聖水：鞭刑", "第二儀式聖水:鞭刑", "第二儀式聖水鞭刑", "儀式聖水：鞭刑", "儀式聖水鞭刑", "第二儀式聖水：鞭"]),
            ("Third Rite Holy Water: Illumination", &["第三儀式聖水：啟蒙", "第三儀式聖水:啟蒙", "第三儀式聖水啟蒙", "第三儀式聖水：啟"]),
            ("Fourth Rite Holy Water: Eucharist", &["第四儀式聖水：聖餐", "第四儀式聖水:聖餐", "第四儀式聖水聖餐", "第四儀式聖水：聖"]),
            ("Xenopanacea", &[]),
        ];
        for (en, spellings) in cards {
            assert!(vorax::advisor::is_always_take(en), "{en} is not always taken");
            for s in spellings {
                assert_eq!(potion(s).as_deref(), Some(en), "{s}");
            }
        }
        // The Xenopanacea without a suffix has no Traditional Chinese name (no evidence yet): nothing matches it.
        assert!(names().cards.iter().any(|c| c.en == "Xenopanacea" && c.zh.is_none()));
        // No name-table entry contains a full-width Roman numeral, so folding them changes nothing else.
        assert!(names().cards.iter().filter_map(|c| c.zh.as_deref()).chain(names().monsters.iter().map(|(m, _)| m.as_str())).all(|z| z.chars().all(|c| !('\u{2160}'..='\u{217f}').contains(&c))));
    }

    /// The four-card hand of a chain step at the measured positions: frame 0044 of the 2026-11-06 07:54 game (the
    /// type texts read unscaled; the potion kit prints none) gives four cards with the kit filled in at 1712; the
    /// four type texts of the upstream sample (aligned to our HUD) give four with none inferred; the same positions
    /// without `至臻` on the left, the usual three cards and the fan with the mouse on its last card do not.
    #[test]
    fn four_card_hand_of_a_chain_step() {
        let slot = |x: i32, dy: i32, t: &str| (x, dy, t.to_string());
        let found = [slot(1073, -31, "至臻藥劑"), slot(1274, -3, "魔法藥劑"), slot(1500, -2, "普通藥劑")];
        let (four, guessed) = hand4_merged(&found).unwrap();
        assert_eq!(four.iter().map(|s| s.0).collect::<Vec<_>>(), [1073, 1274, 1500, 1712]);
        assert_eq!(guessed, [3]);
        let upstream = [slot(1077, 0, "至臻藥劑"), slot(1274, 0, "魔法藥劑"), slot(1501, 0, "普通藥劑"), slot(1711, 0, "稀有藥劑")];
        let (four, guessed) = hand4_merged(&upstream).unwrap();
        assert_eq!((four, guessed), (upstream.to_vec(), vec![]));
        // The old slots extrapolated 225 apart (1049, 1724) are 25 and 12 off and still fit; 1049 - 30 does not.
        assert!(hand4_merged(&[slot(1049, 0, "至臻藥劑"), found[1].clone(), found[2].clone(), slot(1724, 0, "稀有藥劑")]).is_some());
        assert!(hand4_merged(&[slot(1043, 0, "至臻藥劑"), found[1].clone(), found[2].clone()]).is_none());
        assert!(hand4_merged(&[slot(1073, -31, "魔法藥劑"), found[1].clone(), found[2].clone()]).is_none());
        // Two type texts only (the leftmost and the kit missed): not enough to tell.
        assert!(hand4_merged(&found[1..]).is_none());
        // Slots 1 and 2 allow 15.
        assert!(hand4_merged(&[found[0].clone(), slot(1289, 0, "魔法藥劑"), slot(1485, 0, "普通藥劑")]).is_some());
        assert!(hand4_merged(&[found[0].clone(), slot(1290, 0, "魔法藥劑"), found[2].clone()]).is_none());
        assert!(hand4_merged(&[found[0].clone(), found[1].clone(), slot(1516, 0, "普通藥劑")]).is_none());
    }

    /// The leftmost card's type text only partly read on the first read (`藥劑`, its centre 26 to the right) and in
    /// full on the second read of the band: the merge keeps the longer read and gives four cards; with the
    /// rightmost card's type text missed too, the kit slot is filled in. Read as partial on both reads: no four
    /// cards (the hand is read as before).
    #[test]
    fn four_card_hand_with_its_leftmost_type_text_half_read() {
        let slot = |x: i32, t: &str| (x, 0, t.to_string());
        let first = [slot(1100, "藥劑"), slot(1274, "魔法藥劑"), slot(1501, "普通藥劑"), slot(1712, "稀有藥劑")];
        let band = [slot(1077, "至臻藥劑"), slot(1273, "魔法藥劑"), slot(1501, "普通藥劑"), slot(1711, "稀有藥劑")];
        assert!(hand4_merged(&first).is_none(), "the first read alone has no 至臻 on the left");
        let merged = merge_reads(&first, &band);
        assert_eq!(merged.iter().map(|s| (s.0, s.2.as_str())).collect::<Vec<_>>(), [(1077, "至臻藥劑"), (1274, "魔法藥劑"), (1501, "普通藥劑"), (1712, "稀有藥劑")]);
        let (four, guessed) = hand4_merged(&merged).unwrap();
        assert_eq!((four.iter().map(|s| s.0).collect::<Vec<_>>(), guessed), (vec![1077, 1274, 1501, 1712], vec![]));
        // The rightmost type text missed on both reads (as the potion kit, which prints none).
        let (four, guessed) = hand4_merged(&merge_reads(&first[..3], &band[..3])).unwrap();
        assert_eq!((four.iter().map(|s| s.0).collect::<Vec<_>>(), guessed), (vec![1077, 1274, 1501, 1712], vec![3]));
        // The half read is never replaced by a shorter one, and an empty first-read slot (the usual three cards'
        // fill-in) is not merged.
        let kept = merge_reads(&[slot(1077, "至臻藥劑"), slot(1629, "")], &[slot(1100, "藥劑")]);
        assert_eq!(kept, [slot(1077, "至臻藥劑")]);
        assert!(hand4_merged(&merge_reads(&first, &[slot(1098, "藥劑")])).is_none());
    }

    /// The leftmost card's type text not read at all (covered by its effect text) while the other three were read
    /// at 1274, 1500 and 1712: slot 0 is inferred at 1074. The same three moved toward any fan layout or the usual
    /// three cards are not.
    #[test]
    fn four_card_hand_with_its_leftmost_type_text_missed() {
        let slot = |x: i32, t: &str| (x, 0, t.to_string());
        let three = [slot(1273, "魔法藥劑"), slot(1501, "普通藥劑"), slot(1711, "稀有藥劑")];
        let (four, guessed) = hand4_merged(&three).unwrap();
        assert_eq!((four.iter().map(|s| s.0).collect::<Vec<_>>(), guessed), (vec![1074, 1273, 1501, 1711], vec![0]));
        assert!(four[0].2.is_empty());
        // The last slot allows only HAND4_INFER_TOL here (30 when the leftmost one was read).
        assert!(hand4_merged(&[three[0].clone(), three[1].clone(), slot(1692, "稀有藥劑")]).is_some());
        assert!(hand4_merged(&[three[0].clone(), three[1].clone(), slot(1691, "稀有藥劑")]).is_none());
        assert!(hand4_merged(&[slot(1074, "至臻藥劑"), three[0].clone(), three[1].clone(), slot(1691, "稀有藥劑")]).is_some());
        // The fan with the mouse on its last card, its right three a partial read off: 1768 is beyond the slot.
        assert!(hand4_merged(&[slot(1287, "魔法藥劑"), slot(1509, "普通藥劑"), slot(1756, "普通藥劑")]).is_none());
        // An end read on the second read vetoes it.
        assert!(hand4_merged(&[slot(873, "普通藥劑"), three[0].clone(), three[1].clone(), three[2].clone()]).is_none());
    }

    /// No layout of the usual three cards (the mouse on none or on any one, each card pushed 47) nor of the fan
    /// (any three or four of its five, the mouse on none or any card), each position moved up to 7 either way
    /// as an inexact read, is read as the four-card hand, even with `至臻` on every card. Frame 0044 itself was once
    /// read as the fan with the mouse on its last card. The 7 is the grid below the narrowest margin (8, that fan's
    /// 1297 against slot 1), not a bound on real reads: partial reads were measured 11 to 26 off. What is left is
    /// that fan with a Supreme second card, both ends missed on both reads and its 1297 read only as its left half.
    #[test]
    fn other_hands_are_not_four_cards() {
        let mut layouts: Vec<Vec<i32>> = vec![CARD_X.to_vec(), vec![1205, 1463, 1676], vec![1158, 1416, 1676], vec![1158, 1370, 1629]];
        for hover in [None, Some(0), Some(1), Some(2), Some(3), Some(4)] {
            let pos = fan_layout(hover);
            // Every subset of three or four of the five.
            for mask in 0u32..32 {
                if (3..=4).contains(&mask.count_ones()) {
                    layouts.push((0..5).filter(|k| mask & (1 << k) != 0).map(|k| pos[k]).collect());
                }
            }
        }
        for xs in &layouts {
            let n = xs.len() as u32;
            for shift in 0..3usize.pow(n) {
                let moved: Vec<Slot> = xs.iter().enumerate().map(|(i, &x)| (x + (shift / 3usize.pow(i as u32) % 3) as i32 * 7 - 7, 0, "至臻藥劑".to_string())).collect();
                assert!(hand4_merged(&moved).is_none(), "{moved:?}");
            }
        }
    }

    /// A potion kit (large) fan whose second card is Supreme, the mouse on its last card (873, 1085, 1297, 1509,
    /// 1768) and both ends' type text missed on the first read: at the measured positions its middle three are out
    /// of the four-card slots (1297 is 23 from slot 1); read a little to the left (1080, 1285, 1505) they fit, and
    /// only an end found on the second read of the band keeps it from being read as four (dropping the leftmost
    /// card). Frame 0044's second read has no end.
    #[test]
    fn a_fan_with_its_ends_missed_is_not_four_cards() {
        let slot = |x: i32, t: &str| (x, 0, t.to_string());
        let fan = [slot(873, "普通藥劑"), slot(1085, "至臻藥劑"), slot(1297, "魔法藥劑"), slot(1509, "普通藥劑"), slot(1768, "普通藥劑")];
        assert!(hand4_merged(&fan[1..4]).is_none());
        let three = [slot(1080, "至臻藥劑"), slot(1285, "魔法藥劑"), slot(1505, "普通藥劑")];
        let (_, guessed) = hand4_merged(&three).unwrap();
        assert_eq!(guessed, [3], "the first read alone looks like four cards");
        assert!(fan_ends_read(&fan));
        for one_end in [&fan[..1], &fan[4..]] {
            assert!(hand4_merged(&merge_reads(&three, one_end)).is_none(), "{one_end:?}");
            assert!(fan_ends_read(one_end), "{one_end:?}");
        }
        assert!(!fan_ends_read(&[slot(1073, "至臻藥劑"), slot(1274, "魔法藥劑"), slot(1500, "普通藥劑"), slot(1712, "稀有藥劑")]));
    }

    /// The offspring chain (2026-11-06 07:54 game, rounds 5 to 9) is found under the printed names and the spellings
    /// OCR produced in that game, never as one of its neighbours. These cards are not always taken, so unlike
    /// [`unsimulated_potions_match_their_variants`] there is no `is_always_take` check here.
    #[test]
    fn offspring_potions_match_their_variants() {
        let cards: [(&str, &[&str]); 8] = [
            ("Incubation Elixir: Hollow Cocoon", &["孵化靈藥：空心繭", "孵化靈藥:空心繭", "孵化靈藥空心繭"]),
            ("F1 Gen: Parasite Egg", &["子一代：寄生蠱卵", "子一代:寄生蟲卵", "?子一代、寄生蟲卵", "子一代寄生蠱卵"]),
            ("F1 Gen: Vermin Egg Cluster", &["子一代：爬蠱卵簇", "子一代爬蟲卵簇", "?一代爬蟲卵族"]),
            ("F2 Gen: Parasite Pupae", &["子二代：寄生蠱蛹", "子二代寄生蟲蛹", "二代:寄生蟲"]),
            ("F2 Gen: Vermin Pupal Cluster", &["子二代：爬蠱蛹簇", "子二代爬蟲蛹簇", "二代爬蟲蛹族"]),
            ("F3 Gen: Parasitic Butterfly", &["子三代：寄生蝶", "子三代寄生蝶", "三代寄生蝶"]),
            ("F3 Gen: Emergent Butterfly Swarm", &["子三代：破繭蝶群", "子三代破繭蝶群", "三代破繭蝶"]),
            ("Incubation Elixir: Hollow Matriarch", &["孵化靈藥：空心蠱母", "孵化靈藥空心蟲母", "化藥·空心蟲母"]),
        ];
        for (en, spellings) in cards {
            assert!(vorax::advisor::chain_steps_after(en).is_some(), "{en} is not a chain card");
            assert!(names().cards.iter().any(|c| c.en == en && c.id.is_none() && c.zh.is_some()), "{en} is not in unsimulatedPotions");
            for s in spellings {
                assert_eq!(potion(s).as_deref(), Some(en), "{s}");
            }
        }
        // A bare generation with nothing after it says which generation but not which line: no card.
        for s in ["子一代", "二代", "三代：", "代寄生"] {
            assert_eq!(potion(s), None, "{s}");
        }
    }
}

// ---------------------------------------------------------------------------- endgame battle

/// The endgame battle's left panel (2560x1440 reference size): the wave "3 /3", the "836.4K+381.9K/999.9M" under
/// the `下個獎勵` ("next reward") label, the "(+15%)" at the lower right, and the whole panel for archiving. The
/// positions were measured from trainingdata/vorax/frames2 t160.00-battle-wave3 and t186.00-battle-end-panel of
/// the 2026-11-03 recording.
const BATTLE_WAVE: Rect = Rect::new(285, 562, 85, 44);
const BATTLE_NUMBERS: Rect = Rect::new(24, 698, 340, 40);
const BATTLE_PERCENT: Rect = Rect::new(255, 758, 115, 40);
/// The archived panel crop (about 360x330).
pub const BATTLE_PANEL: Rect = Rect::new(14, 552, 362, 330);

/// What was read from the endgame battle panel. The numbers are the panel's abbreviations converted back
/// (836.4K → 836400), so they only have thousands-level precision.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Battle {
    /// Which wave, out of how many.
    pub wave: Option<(u32, u32)>,
    /// Left of the "+": the activity already counted (accumulated upward from the board's final activity while
    /// fighting monsters).
    pub activity: Option<f64>,
    /// Right of the "+": the battle bonus (activity added according to the protocol when a monster is defeated).
    pub bonus: Option<f64>,
    /// Right of the "/": the threshold of the next reward tier; after the last tier is taken it is 999.9M.
    pub next: Option<f64>,
    /// The "(+N%)" at the lower right.
    pub percent: Option<i32>,
    /// Raw OCR text of the three rows (for the log).
    pub raw: String,
}

impl Battle {
    /// Activity and bonus were read: it is a battle panel.
    pub fn readable(&self) -> bool {
        self.activity.is_some() && self.bonus.is_some()
    }
}

/// The three numbers of one panel row: (activity, bonus, next tier).
type Numbers3 = (Option<f64>, Option<f64>, Option<f64>);

/// The thresholds of the next reward tier (the ladder of the board's next reward tier, compiled from all logs on
/// 2026-11-05): the right of the battle panel's "/" is always one of these, and after the last tier is taken it
/// is 999.9M.
const BATTLE_LADDER: [f64; 17] = [
    2e3, 8e3, 18e3, 28e3, 38e3, 57e3, 76e3, 114e3, 152e3, 190e3, 266e3, 342e3, 512e3, 607e3, 721e3, 854e3, 1.12e6,
];
/// The panel's threshold after all rewards are taken.
const BATTLE_ALL: f64 = 999_900_000.0;

/// Which rung of the ladder `v` is: the panel has only one decimal place, so 1.12M may be displayed as 1.1M (the
/// K rungs are all exact). The tolerance is not relaxed: "1114.0K" must be taken as 114.0K with an extra 1, not
/// as 1.12M.
fn on_ladder(v: f64) -> Option<f64> {
    BATTLE_LADDER.iter().copied().find(|&t| v == t || (t >= 1e6 && v == (t / 1e5).round() * 1e5))
}

/// Split "836.4K+381.9K/999.9M" into (activity, bonus, next tier). OCR very often misreads the separators ("+"
/// read as ".", "/" read as "1" or "'"), so they are ignored and the numbers are taken in order: one ending in K
/// or M is a value, and any other number with two or more digits (unit dropped) also takes a slot but has no
/// value, so the later fields do not shift forward. The next tier must be on the reward ladder
/// ([`BATTLE_LADDER`]); when "/" is read as "1" there is an extra 1 in front (1854.0K, 1999.9M), and it is used
/// if it is on the ladder after removing it, otherwise it counts as unreadable.
fn battle_numbers(text: &str) -> Numbers3 {
    // The decimal point is often read as a middle dot, a bullet, an ideographic comma or ","; "O" is a 0.
    let mut chars: Vec<char> = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            'O' | 'o' => '0',
            '·' | '•' | '、' | ',' => '.',
            c => c,
        })
        .collect();
    // A "1" inside a number is often read as I, l, | or i ("482 · IK" for 482.1K, 2026-11-06 12:23 game): it is a 1
    // when a digit or decimal point is before it and a digit, decimal point or unit after it. Left alone, the
    // number ended at the letter, lost its unit and its last digit, and the one way of reading that dropped the
    // letter entirely ("482 K") won the vote as 48.2K.
    for k in 1..chars.len().saturating_sub(1) {
        let (before, after) = (chars[k - 1], chars[k + 1]);
        if matches!(chars[k], 'I' | 'l' | '|' | 'i')
            && (before.is_ascii_digit() || before == '.')
            && (after.is_ascii_digit() || matches!(after, '.' | 'K' | 'k' | 'M' | 'm'))
        {
            chars[k] = '1';
        }
    }
    // For each number: (original text, unit multiplier); a number without a unit has multiplier None.
    let mut found: Vec<(String, Option<f64>)> = Vec::new();
    let mut k = 0;
    while k < chars.len() {
        if !chars[k].is_ascii_digit() {
            k += 1;
            continue;
        }
        let start = k;
        while k < chars.len() && (chars[k].is_ascii_digit() || chars[k] == '.') {
            k += 1;
        }
        let digits = chars[start..k].iter().collect::<String>().trim_matches('.').to_string();
        let mult = match chars.get(k) {
            Some('K' | 'k') => Some(1e3),
            Some('M' | 'm') => Some(1e6),
            _ => None,
        };
        if mult.is_some() {
            k += 1;
        } else if digits.chars().filter(char::is_ascii_digit).count() < 2 {
            // A lone digit is mostly a misread separator ("/" read as "1") and takes no slot.
            continue;
        }
        found.push((digits, mult));
    }
    let value = |digits: &str, mult: Option<f64>| -> Option<f64> {
        let mut d = digits.to_string();
        // The panel's numbers always have one decimal place (836.4K, 999.9M): a 2-4 digit number with no
        // decimal point read has it put back (8364K is 836.4K); 5 or more digits with no decimal point cannot
        // tell where the misread is, so it counts as unreadable.
        if !d.contains('.') {
            match d.len() {
                2..=4 => d.insert(d.len() - 1, '.'),
                1 => {}
                _ => return None,
            }
        }
        Some((d.parse::<f64>().ok()? * mult?).round())
    };
    let plain = |k: usize| found.get(k).and_then(|(d, m)| value(d, *m)).filter(|v| *v < 1e8);
    let next = found.get(2).and_then(|(d, m)| {
        let v = value(d, *m)?;
        if v >= 1e8 {
            return Some(BATTLE_ALL);
        }
        on_ladder(v).or_else(|| d.strip_prefix('1').and_then(|rest| value(rest, *m)).and_then(on_ladder))
    });
    (plain(0), plain(1), next)
}

/// The endgame battle always has 3 waves (the 2026-11-03 and 11-04 recordings and the observed frames all show
/// "N/3").
const BATTLE_WAVES: u32 = 3;

/// The (wave, total waves) of "3 /3": trust only the digit right before the "/" ("/3" is often read as "/33" or
/// "/9"; a leading "1" is often read as l, I or |). Without a "/" it does not count.
fn battle_wave(text: &str) -> Option<(u32, u32)> {
    let t: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    let slash = t.iter().position(|&c| c == '/')?;
    let w = match *t.get(slash.checked_sub(1)?)? {
        'l' | 'I' | '|' | 'i' => 1,
        c => c.to_digit(10)?,
    };
    (1..=BATTLE_WAVES).contains(&w).then_some((w, BATTLE_WAVES))
}

/// The 15 of "(+15%)". A protocol's bonus is at most about ±60%: a 415% where "+" was read as "4" has the
/// leading 4 removed, and anything else over 100 counts as unreadable.
fn battle_percent(text: &str) -> Option<i32> {
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if !t.contains('%') {
        return None;
    }
    let inner = t.split('%').next()?;
    let digits: String = inner.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    let sign = if inner.trim_end_matches(|c: char| c.is_ascii_digit()).ends_with('-') { -1 } else { 1 };
    let v = digits.parse::<i32>().ok()?;
    let v = match digits.strip_prefix('4') {
        Some(rest) if v > 100 && !rest.is_empty() => rest.parse::<i32>().ok()?,
        _ => v,
    };
    (v <= 100).then_some(sign * v)
}

/// The value that appears most often across several ways of reading (ties go to the one that appeared first);
/// None if there is none.
fn vote<T: PartialEq + Clone>(values: &[Option<T>]) -> Option<T> {
    let seen: Vec<&T> = values.iter().flatten().collect();
    seen.iter().max_by_key(|v| (seen.iter().filter(|w| w == v).count(), std::cmp::Reverse(seen.iter().position(|w| w == *v)))).map(|v| (*v).clone())
}

/// Read the endgame battle panel on the left of a screenshot in the reference size (see [`to_reference`]); when
/// it is not a battle panel, [`Battle::readable`] is false.
pub fn read_battle(img: &Rgba) -> Battle {
    // Read the same block once with each of several preprocessings and vote: the panel's text is small and
    // moving, and Windows OCR errs in different places with each way of reading (tried on two frames of the
    // 2026-11-03 recording: the original at 2x and 3x and black-on-white at the 70th, 80th and 85th
    // percentiles).
    let regions = [BATTLE_WAVE, BATTLE_NUMBERS, BATTLE_PERCENT];
    let mut crops: Vec<(Rect, Rgba, f32)> = Vec::new();
    for r in regions {
        crops.push((r, img.crop(r), 2.0));
        crops.push((r, img.crop(r), 3.0));
        for q in [70, 80, 85] {
            crops.push((r, inked(img, r, q), 2.0));
        }
    }
    let read = ocr_images(&crops);
    let texts: Vec<String> = read.iter().map(|l| joined(l)).collect();
    let per = texts.len() / regions.len();
    battle_from_texts(&texts[..per], &texts[per..2 * per], &texts[2 * per..])
}

/// The panel from the OCR texts of its three rows, one text per way of reading each ([`read_battle`]): each row
/// parsed and voted on.
fn battle_from_texts(waves: &[String], numbers: &[String], percents: &[String]) -> Battle {
    let parsed: Vec<Numbers3> = numbers.iter().map(|t| battle_numbers(t)).collect();
    let pick = |f: fn(&Numbers3) -> Option<f64>| vote(&parsed.iter().map(f).map(|v| v.map(|x| x as i64)).collect::<Vec<_>>()).map(|v| v as f64);
    Battle {
        wave: vote(&waves.iter().map(|t| battle_wave(t)).collect::<Vec<_>>()),
        activity: pick(|p| p.0),
        bonus: pick(|p| p.1),
        next: pick(|p| p.2),
        percent: vote(&percents.iter().map(|t| battle_percent(t)).collect::<Vec<_>>()),
        raw: format!("{} | {} | {}", waves.join(" / "), numbers.join(" / "), percents.join(" / ")),
    }
}
