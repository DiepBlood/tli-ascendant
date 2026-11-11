//! The frames drawn on the game around graded affix entries, as a premultiplied BGRA bitmap (the layered window's
//! format, see `app::mark`). Two styles, the user's choice on the affix page (2026-11-08, default A):
//! - A, [`FrameStyle::Outline`]: a frame around the entry, red for danger, yellow for a warning;
//! - B, [`FrameStyle::Bar`]: a thick bar left of the entry and a faint tint over it.
//!
//! Sizes are reference pixels (2560x1440 at the user's UI scale) times `unit`, the frame pixels per reference pixel.

use super::bullets::BulletColour;
use super::dict::{Grade, Lookup};
use super::read::{Panel, frame_columns};
use crate::reader::Rect;

/// How the frames look (the user picks on the affix page; A by default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FrameStyle {
    /// A: a frame around the entry.
    #[default]
    Outline,
    /// B: a bar left of the entry and a faint tint over it.
    Bar,
}

/// Frame colours (RGB): danger and warning, as the mockup the user chose from (mockups\overlay-styles.png).
pub const DANGER: [u8; 3] = [235, 60, 50];
pub const WARN: [u8; 3] = [240, 200, 40];
/// Style A's line width.
const OUTLINE: f32 = 3.0;
/// Style B's bar width and its gap to the entry (left of the frame columns), and the tint's opacity.
const BAR: f32 = 7.0;
const BAR_GAP: f32 = 3.0;
const TINT_ALPHA: f32 = 45.0 / 255.0;

/// The colour of a grade, None for affixes not framed.
fn colour(grade: Grade) -> Option<[u8; 3]> {
    match grade {
        Grade::Danger => Some(DANGER),
        Grade::Warn => Some(WARN),
        Grade::None => None,
    }
}

/// The grade the game's own colour gives an entry (its bullet's): red a danger, orange a warning, white none. Entries
/// not in the list are framed by it (the user's choice, 2026-11-09: a Chinese client meets an affix it has no text
/// for until the map is entered, see `affix::learned`).
pub fn game_grade(colour: BulletColour) -> Grade {
    match colour {
        BulletColour::Red => Grade::Danger,
        BulletColour::Orange => Grade::Warn,
        BulletColour::White => Grade::None,
    }
}

/// Whether an entry of `panel` takes the game's colour ([`game_grade`], see [`boxes`]): not in the list, not near a
/// sentence the user graded, with text read, on a panel known to be the affix panel (its title bar shows, or an entry
/// of it is found).
pub fn by_game_colour(panel: &Panel, e: &crate::affix::Entry) -> bool {
    e.lookup == Lookup::Unknown
        && e.user.is_none()
        && !e.text.trim().is_empty()
        && (panel.title || panel.entries.iter().any(|e| matches!(e.lookup, Lookup::Found { .. })))
}

/// The boxes to frame on a frame `frame_h` high, in frame pixels: every entry found as danger or warning, and every
/// entry not in the list whose game colour is ([`game_grade`]), across the frame columns ([`frame_columns`]) from its
/// top to its bottom. Ambiguous entries (close to affixes of different grades) and entries not graded get none. The
/// game's colour frames an entry not in the list only on a panel known to be the affix panel (its title bar shows, or
/// an entry of it is found) and with text read: of 3000 frames of other screens (the Vorax surgery, the hideout; 2026-11-09), three read red
/// bullets with no text or a monster's name, and nothing found.
pub fn boxes(panel: &Panel, frame_h: usize) -> Vec<(Rect, Grade)> {
    boxes_at(panel, frame_h, None)
}

/// [`boxes`] of a panel drawn at placement `place` (another interface scale, [`super::read::read_placed`]): the frame
/// columns go through it (its rows are the frame's already).
pub fn boxes_at(panel: &Panel, frame_h: usize, place: Option<super::panel::Placement>) -> Vec<(Rect, Grade)> {
    let (x0, x1) = match place {
        None => frame_columns(frame_h),
        Some(p) => {
            let k = frame_h as f64 / 1440.0;
            let (a, b) = frame_columns(1440);
            ((p.x(f64::from(a)) * k).round() as i32, (p.x(f64::from(b)) * k).round() as i32)
        }
    };
    panel
        .entries
        .iter()
        .filter_map(|e| {
            let grade = match e.lookup {
                Lookup::Found { grade, .. } => grade,
                Lookup::Unknown => e.user.unwrap_or_else(|| if by_game_colour(panel, e) { game_grade(e.colour) } else { Grade::None }),
                Lookup::Ambiguous => Grade::None,
            };
            colour(grade).is_some().then(|| (Rect::new(x0, e.top, x1 - x0, e.bottom - e.top), grade))
        })
        .collect()
}

/// The bitmap of `boxes` (frame pixels) in `style`: premultiplied BGRA rows top to bottom, and where it sits in frame
/// pixels (left, top, width, height). None when there is nothing to draw.
pub fn paint(boxes: &[(Rect, Grade)], style: FrameStyle, unit: f32) -> Option<(Vec<u8>, Rect)> {
    // the bar's width and gap, rounded as they are drawn (rounding their sum instead lost a column at some units)
    let bar = ((BAR * unit).round() as i32).max(2);
    let gap = (BAR_GAP * unit).round() as i32;
    let reach = match style {
        FrameStyle::Outline => 0,
        FrameStyle::Bar => bar + gap,
    };
    let drawn: Vec<(Rect, [u8; 3])> = boxes.iter().filter_map(|&(r, g)| colour(g).map(|c| (r, c))).filter(|(r, _)| r.w > 0 && r.h > 0).collect();
    let left = drawn.iter().map(|(r, _)| r.x - reach).min()?;
    let top = drawn.iter().map(|(r, _)| r.y).min()?;
    let right = drawn.iter().map(|(r, _)| r.x + r.w).max()?;
    let bottom = drawn.iter().map(|(r, _)| r.y + r.h).max()?;
    let (w, h) = ((right - left) as usize, (bottom - top) as usize);
    let mut px = vec![0u8; w * h * 4];
    let mut fill = |x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3], alpha: f32| {
        let a = (alpha * 255.0).round() as u8;
        let pm = |v: u8| ((f32::from(v) * alpha).round()) as u8;
        let p = [pm(c[2]), pm(c[1]), pm(c[0]), a];
        for y in y0.max(top)..y1.min(bottom) {
            for x in x0.max(left)..x1.min(right) {
                let i = ((y - top) as usize * w + (x - left) as usize) * 4;
                px[i..i + 4].copy_from_slice(&p);
            }
        }
    };
    for (r, c) in drawn {
        let (x0, y0, x1, y1) = (r.x, r.y, r.x + r.w, r.y + r.h);
        match style {
            FrameStyle::Outline => {
                let t = ((OUTLINE * unit).round() as i32).max(1);
                // the top line one pixel in, so two boxes one above the other keep a gap between their lines; the bottom
                // line at the box's edge: one pixel higher it covered the lowest row of g, y and p
                let y0 = y0 + 1;
                fill(x0, y0, x1, y0 + t, c, 1.0);
                fill(x0, y1 - t, x1, y1, c, 1.0);
                fill(x0, y0, x0 + t, y1, c, 1.0);
                fill(x1 - t, y0, x1, y1, c, 1.0);
            }
            FrameStyle::Bar => {
                fill(x0, y0 + 1, x1, y1 - 1, c, TINT_ALPHA);
                fill(x0 - gap - bar, y0 + 1, x0 - gap, y1 - 1, c, 1.0);
            }
        }
    }
    Some((px, Rect::new(left, top, w as i32, h as i32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(bitmap: &(Vec<u8>, Rect), x: i32, y: i32) -> [u8; 4] {
        let (px, r) = bitmap;
        let i = ((y - r.y) as usize * r.w as usize + (x - r.x) as usize) * 4;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    /// Style A: an opaque line along the box's edges in the grade's colour (BGRA), the inside clear.
    #[test]
    fn outline_frames_the_box() {
        let b = paint(&[(Rect::new(118, 500, 482, 60), Grade::Danger)], FrameStyle::Outline, 1.0).unwrap();
        assert_eq!(b.1, Rect::new(118, 500, 482, 60));
        assert_eq!(pixel(&b, 118, 530), [50, 60, 235, 255]);
        assert_eq!(pixel(&b, 300, 501), [50, 60, 235, 255]);
        assert_eq!(pixel(&b, 300, 530), [0, 0, 0, 0]);
        assert_eq!(pixel(&b, 300, 500), [0, 0, 0, 0]);
    }

    /// Style B: the bar left of the box, opaque; the box tinted with premultiplied colour.
    #[test]
    fn bar_and_tint() {
        let b = paint(&[(Rect::new(118, 500, 482, 60), Grade::Warn)], FrameStyle::Bar, 1.0).unwrap();
        assert_eq!(b.1.x, 108);
        assert_eq!(pixel(&b, 110, 530), [40, 200, 240, 255]);
        let tint = pixel(&b, 300, 530);
        assert_eq!(tint[3], 45);
        assert_eq!(tint[2], (240.0f32 * 45.0 / 255.0).round() as u8);
        assert_eq!(pixel(&b, 116, 530), [0, 0, 0, 0]);
    }

    /// Entries not in the list are framed by the game's colour (red a danger, orange a warning, white none); found
    /// ones by their grade whatever their colour; ambiguous ones not at all.
    #[test]
    fn entries_not_in_the_list_take_the_games_colour() {
        use crate::affix::Entry;
        let e = |colour, lookup| Entry { top: 500, bottom: 560, colour, text: "An affix as read".into(), cut: false, lookup, user: None };
        let found_none = Lookup::Found { index: 0, grade: Grade::None, exact: true };
        let panel = Panel {
            title: false,
            entries: vec![
                e(BulletColour::Red, Lookup::Unknown),
                e(BulletColour::Orange, Lookup::Unknown),
                e(BulletColour::White, Lookup::Unknown),
                e(BulletColour::Red, found_none),
                e(BulletColour::Red, Lookup::Ambiguous),
                // near a sentence the user graded: the user's grade, not the game's colour
                Entry { user: Some(Grade::None), ..e(BulletColour::Red, Lookup::Unknown) },
                Entry { user: Some(Grade::Warn), ..e(BulletColour::Red, Lookup::Unknown) },
            ],
        };
        let grades: Vec<Grade> = boxes(&panel, 1440).into_iter().map(|(_, g)| g).collect();
        assert_eq!(grades, [Grade::Danger, Grade::Warn, Grade::Warn]);
        // nothing found on it (not known to be the affix panel), or no text read: not by the game's colour
        let only_unknown = Panel { entries: vec![e(BulletColour::Red, Lookup::Unknown), Entry { user: Some(Grade::Warn), ..e(BulletColour::Red, Lookup::Unknown) }], title: false };
        let grades: Vec<Grade> = boxes(&only_unknown, 1440).into_iter().map(|(_, g)| g).collect();
        assert_eq!(grades, [Grade::Warn], "the user's grade still holds");
        // its title bar seen: the affix panel, the game's colour frames
        let titled = Panel { title: true, ..only_unknown };
        let grades: Vec<Grade> = boxes(&titled, 1440).into_iter().map(|(_, g)| g).collect();
        assert_eq!(grades, [Grade::Danger, Grade::Warn], "the title tells the affix panel");
        let found = Lookup::Found { index: 0, grade: Grade::Danger, exact: true };
        let blank = Panel { entries: vec![e(BulletColour::White, found), Entry { text: "  ".into(), ..e(BulletColour::Red, Lookup::Unknown) }], title: true };
        assert_eq!(boxes(&blank, 1440).len(), 1, "the found one only");
    }

    /// Unframed grades draw nothing; nothing at all gives no bitmap.
    #[test]
    fn nothing_to_draw() {
        assert!(paint(&[(Rect::new(118, 500, 482, 60), Grade::None)], FrameStyle::Outline, 1.0).is_none());
        assert!(paint(&[], FrameStyle::Bar, 1.0).is_none());
    }
}
