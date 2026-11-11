//! The affix frames on the game: a layered window over the map page's affix panel, the mark's kind of window
//! (`mark.rs`: it never takes focus and lets clicks through), showing [`crate::affix::frames::paint`]'s bitmap.
//!
//! The boxes come in frame pixels of a capture of the game's client area; the game's client area is in physical
//! screen pixels (`capture::game_rect`, per-monitor DPI aware), so a box maps by the client's origin only, as the mark
//! does (ui.rs, `mark_spot`): a capture of another size than the client is the client cut (capture.rs, `Origin`), not
//! scaled. Nothing is drawn outside the client area, so no frame shows on another screen.

use super::capture::dpi_aware;
use super::mark::{clipped, overlay_window, update};
use crate::affix::frames::{self, FrameStyle};
use crate::affix::{Grade, bullets::REF_HEIGHT};
use crate::reader::Rect;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow};

/// What the window shows: the boxes in screen pixels, the style and the game's client area.
type Drawn = (Vec<(Rect, Grade)>, FrameStyle, [i32; 4]);

/// The frames' window; hidden until [`AffixOverlay::show`], destroyed on drop.
pub struct AffixOverlay {
    hwnd: HWND,
    drawn: Option<Drawn>,
}

/// `boxes` (frame pixels of a `frame` = (width, height) capture) in screen pixels over the client area `game` (left,
/// top, width, height): moved by the client's origin. Also the unit, frame pixels per reference pixel.
pub fn on_screen(boxes: &[(Rect, Grade)], frame: (usize, usize), game: [i32; 4]) -> (Vec<(Rect, Grade)>, f32) {
    let [left, top, ..] = game;
    let unit = frame.1 as f32 / REF_HEIGHT as f32;
    (boxes.iter().map(|&(r, g)| (Rect::new(left + r.x, top + r.y, r.w, r.h), g)).collect(), unit)
}

impl AffixOverlay {
    /// A hidden frames window; None when Windows refuses one.
    pub fn new() -> Option<AffixOverlay> {
        Some(AffixOverlay { hwnd: overlay_window()?, drawn: None })
    }

    /// Frames `boxes` (frame pixels of a `frame` capture) in `style` over the game's client area `game`; drawn again
    /// only when the boxes, the style or the game's place change. No box to frame hides the window. Returns whether
    /// frames show.
    pub fn show(&mut self, boxes: &[(Rect, Grade)], frame: (usize, usize), style: FrameStyle, game: [i32; 4]) -> bool {
        let (screen, unit) = on_screen(boxes, frame, game);
        let want = (screen, style, game);
        if self.drawn.as_ref() == Some(&want) {
            return true;
        }
        let Some((pixels, at)) = frames::paint(&want.0, style, unit) else {
            self.hide();
            return false;
        };
        let Some((pixels, [left, top, width, height])) = clipped(&pixels, [at.x, at.y, at.w, at.h], game) else {
            self.hide();
            return false;
        };
        if !dpi_aware(|| update(self.hwnd, &pixels, width, height, left, top)) {
            // the window would keep the old frames where they were
            self.hide();
            return false;
        }
        self.drawn = Some(want);
        // SAFETY: a window this overlay made.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        true
    }

    pub fn shown(&self) -> bool {
        self.drawn.is_some()
    }

    pub fn hide(&mut self) {
        if self.drawn.take().is_some() {
            // SAFETY: a window this overlay made.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
}

impl Drop for AffixOverlay {
    fn drop(&mut self) {
        // SAFETY: a window this overlay made, destroyed once.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boxes move by the client's origin (on a screen left of the main one too), whatever the client's size: a capture
    /// of another size is the client cut, not scaled. The unit follows the frame's height.
    #[test]
    fn boxes_map_onto_the_screen() {
        let boxes = [(Rect::new(118, 500, 482, 60), Grade::Danger)];
        let (same, unit) = on_screen(&boxes, (2560, 1440), [100, 50, 2560, 1440]);
        assert_eq!(same, vec![(Rect::new(218, 550, 482, 60), Grade::Danger)]);
        assert!((unit - 1.0).abs() < 1e-6);
        let (left_screen, unit) = on_screen(&boxes, (1920, 1080), [-1920, 0, 1920, 1080]);
        assert_eq!(left_screen, vec![(Rect::new(-1802, 500, 482, 60), Grade::Danger)]);
        assert!((unit - 0.75).abs() < 1e-6);
    }
}
