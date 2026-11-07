//! The mark drawn on the game on what to click for the advised move: corner
//! brackets around the target and an arrow above it pointing down, on a
//! layered window over the game that never takes focus and lets clicks
//! through, drawn from a premultiplied BGRA bitmap.

use super::capture::{dpi_aware, ours};
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC,
    CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER, DT_NOPREFIX, DT_WORDBREAK, DeleteDC,
    DeleteObject, DrawTextW, FW_BOLD, GdiFlush, HDC, OUT_DEFAULT_PRECIS, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow, ULW_ALPHA,
    UpdateLayeredWindow, WNDCLASSW, WindowFromPoint, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

/// Sizes below are reference px (1080p): the box's line width, edges
/// included, and each dark edge's.
const STROKE: f32 = 6.0;
const EDGE: f32 = 1.5;
/// How far outside the target's box the box sits.
const PAD: f32 = 8.0;
/// The arrow's width and height, and the gap from its tip down to the
/// box's top edge.
const ARROW_W: f32 = 46.0;
const ARROW_H: f32 = 34.0;
const ARROW_GAP: f32 = 10.0;
/// The bright core, premultiplied alpha 255: a green the game paints
/// nowhere. Gold corners blended into the board's gold and orange rarity
/// glows and the player had to look for them; the player picked a green box
/// with a glow (2026-11-01).
const GREEN: [u8; 3] = [60, 255, 90];
/// The glow's reach on each side of the lines, and its opacity at them.
const GLOW: f32 = 16.0;
const GLOW_ALPHA: f32 = 0.45;
/// The gold of the notice's text and rim.
const GOLD: [u8; 3] = [255, 205, 110];
/// A target pick's mark: its lines and arrow larger. The pick's jaws paint
/// every target red and gold (2026-10-30).
const LOUD_STROKE: f32 = 9.0;
const LOUD_ARROW: f32 = 1.5;
const PLAIN_ARROW: f32 = 1.3;
/// A new mark grows in from this many times its size over [`GROW_MS`],
/// easing out: the eye goes to what moves.
const GROW_FROM: f32 = 1.8;
const GROW_MS: f32 = 220.0;
/// A mark hidden this long or less and shown again on the same target does
/// not grow in again: a read held for a moment hides it.
const REGROW_AFTER: std::time::Duration = std::time::Duration::from_millis(300);
/// How far, as a share of the target's half size, a target read again may
/// sit from the one marked and still be it. Each read places a card or
/// button from its text, a few px apart from read to read (a middle door at
/// x 939, 940 and 942), and 2 px on screen grew the mark in again at every
/// read (2026-11-01); the nearest other target is a card or cell away.
const SAME_SHARE: f32 = 0.25;
/// Up and down, more: a decision button is placed from its text, and the
/// button under the cursor floats up; the buttons stand side by side, and
/// cells and cards sit at fixed heights.
const SAME_SHARE_Y: f32 = 0.5;
/// How far outside the target, in reference px, a click still counts as one on its mark (see [`hit_rect`]): the loud
/// box's line, PAD out and LOUD_STROKE wide, ends PAD + LOUD_STROKE / 2 = 12.5 out; the other 4.5 are slack for the
/// cursor moving on between the click and the next look, 50 ms later. Tighter than the glow: a cell is 87 screen px
/// half high at 1440p and the next row's centre 187 px down (the 2026-11-07 run log's mark_at), so 17 reaches 10 px
/// into the cell below; the decision buttons stand 460 px apart (Consume 590, Replace 1050, Hunt 1510, End 1970 on
/// that run's manage frame) and the doors 440 wide, clear of each other.
const HIT_PAD: f32 = PAD + LOUD_STROKE;

/// How a mark is drawn: its lines thicker on a target pick.
#[derive(Clone, Copy, PartialEq)]
pub enum Style {
    Plain,
    Loud,
}

impl Style {
    /// Line width and arrow scale.
    fn terms(self) -> (f32, f32) {
        match self {
            Style::Plain => (STROKE, PLAIN_ARROW),
            Style::Loud => (LOUD_STROKE, LOUD_ARROW),
        }
    }
}
/// The dark edge's opacity.
const EDGE_ALPHA: f32 = 180.0 / 255.0;

/// The padded box plus a `stroke` wide line and its glow on every side, and 2 px slack, in screen px at `unit`.
fn side(stroke: f32, unit: f32) -> f32 {
    (PAD + stroke / 2.0 + GLOW) * unit + 2.0
}

/// How far the mark of a target `half_h` high (half its height, screen px) reaches above its centre (the box, the
/// arrow and their glow) and below it (the box and its glow), in screen px at `unit` screen px per reference px.
pub fn reach(half_h: f32, unit: f32, style: Style) -> (f32, f32) {
    let (stroke, arrow) = style.terms();
    let below = half_h + side(stroke, unit);
    (below + (ARROW_GAP + ARROW_H * arrow) * unit, below)
}

/// The text height of the expected value on the game (see [`Notice::value`]) against the notice's: small, so it
/// covers little of the game.
const VALUE_SCALE: f32 = 0.7;

/// Where the expected value's box goes, its top in screen px: just above the mark (`above`, the mark's top), or just
/// below it (`below`, the mark's bottom) when a box `height` high above it would leave the game (whose top is
/// `game_top`).
pub fn value_top(above: f32, below: f32, height: f32, game_top: f32) -> f32 {
    if above - height >= game_top { above - height } else { below }
}

/// The mark for target `[x, y, half_w, half_h, unit]` (see [`Mark::show`]): its pixels (see [`paint`]) and left,
/// top, width and height in screen px.
pub fn mark_bitmap([x, y, half_w, half_h, unit]: [f32; 5], style: Style) -> (Vec<u8>, [i32; 4]) {
    let (stroke, _) = style.terms();
    let right = half_w + side(stroke, unit);
    let (above, below) = reach(half_h, unit, style);
    let (width, height) = ((2.0 * right).ceil() as i32, (above + below).ceil() as i32);
    let (left, top) = ((x - right).round() as i32, (y - above).round() as i32);
    let pixels = paint((width as usize, height as usize), (x - left as f32, y - top as f32), (half_w, half_h), unit, style);
    (pixels, [left, top, width, height])
}

/// The mark window; hidden until [`Mark::show`], destroyed on drop.
pub struct Mark {
    hwnd: HWND,
    /// What the window shows: centre, half width, half height and unit in
    /// screen px, the style, and the scale it grew in at.
    drawn: Option<([f32; 5], Style, f32)>,
    /// The target being marked, where first read, and since when, for the
    /// grow-in.
    target: Option<([f32; 5], Style, std::time::Instant)>,
    /// When the mark was hidden, its target kept for [`REGROW_AFTER`].
    hidden: Option<std::time::Instant>,
    /// The game's left and top in screen px when last shown, see [`Mark::at`].
    game: (i32, i32),
}

/// Whether target `b` (centre, half width, half height, unit) is target `a`
/// read again: the same size and unit, its centre within [`SAME_SHARE`] of
/// the half width and [`SAME_SHARE_Y`] of the half height.
fn same_target(a: &[f32; 5], b: &[f32; 5]) -> bool {
    let near = |p: f32, q: f32, share: f32| (p - q).abs() <= share * p.abs().max(q.abs());
    near(a[4], b[4], 0.02)
        && near(a[2], b[2], 0.2)
        && near(a[3], b[3], 0.2)
        && (a[0] - b[0]).abs() <= SAME_SHARE * a[2]
        && (a[1] - b[1]).abs() <= SAME_SHARE_Y * a[3]
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: the arguments are the ones Windows passed.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A hidden layered window over the game that never takes focus and lets
/// clicks through; None when Windows refuses one.
fn overlay_window() -> Option<HWND> {
    // SAFETY: plain Win32 calls; the class name is a static string.
    dpi_aware(|| unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance.into(), lpszClassName: w!("AscendantMark"), ..Default::default() };
        // A second window finds the class registered already.
        RegisterClassW(&class);
        let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        CreateWindowExW(ex, w!("AscendantMark"), w!("Ascendant mark"), WS_POPUP, 0, 0, 1, 1, None, None, Some(instance.into()), None).ok()
    })
}

impl Mark {
    /// A hidden mark window; None when Windows refuses one.
    pub fn new() -> Option<Mark> {
        Some(Mark { hwnd: overlay_window()?, drawn: None, target: None, hidden: None, game: (0, 0) })
    }

    /// Marks a target `[x, y, half_w, half_h, unit]`: half size `half_w` x
    /// `half_h` centred on (`x`, `y`) in physical screen px, `unit` screen px
    /// per reference px; drawn again
    /// only when one of them changes. A new target grows in (see
    /// [`GROW_FROM`]): true while it does, for the caller to come back soon.
    /// The same target read a few px off (see [`same_target`]) stays where
    /// it was first marked. Nothing is drawn outside `clip` (the game's
    /// left, top, width and height): a mark growing in past the game's edge
    /// flashed on the player's other screen (2026-11-01).
    pub fn show(&mut self, read: [f32; 5], style: Style, clip: [i32; 4]) -> bool {
        self.game = (clip[0], clip[1]);
        let back = self.hidden.take().is_none_or(|at| at.elapsed() <= REGROW_AFTER);
        let (spot, since) = match self.target {
            Some((at, was, since)) if back && was == style && same_target(&at, &read) => (at, since),
            _ => (read, std::time::Instant::now()),
        };
        self.target = Some((spot, style, since));
        let [x, y, half_w, half_h, unit] = spot;
        let t = (since.elapsed().as_secs_f32() * 1000.0 / GROW_MS).min(1.0);
        let scale = 1.0 + (GROW_FROM - 1.0) * (1.0 - t).powi(3);
        let want = (spot, style, scale);
        if self.drawn == Some(want) {
            return false;
        }
        let (pixels, [left, top, width, height]) = mark_bitmap([x, y, half_w * scale, half_h * scale, unit], style);
        let Some((pixels, [left, top, width, height])) = clipped(&pixels, [left, top, width, height], clip) else {
            return t < 1.0;
        };
        if dpi_aware(|| update(self.hwnd, &pixels, width, height, left, top)) {
            self.drawn = Some(want);
            // SAFETY: a window this mark made.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
        t < 1.0
    }

    pub fn shown(&self) -> bool {
        self.drawn.is_some()
    }

    /// The target the mark holds while on show, `[x, y, half_w, half_h, unit]` in screen px: where it was first
    /// marked when read again a few px off (see [`same_target`]), so what goes with it does not move either.
    pub fn spot(&self) -> Option<[f32; 5]> {
        self.drawn.and(self.target).map(|(spot, _, _)| spot)
    }

    /// Where the mark sits on the game, in the game frame's px: its centre,
    /// half width and half height, rounded; None while hidden. The run log
    /// has it, to tell a mark moved from one under the same advice: the
    /// player saw a patrol's mark go between doors 10 and 11 that the log
    /// could not show (2026-11-02, day 2).
    pub fn at(&self) -> Option<[i32; 4]> {
        let ([x, y, half_w, half_h, _], _, _) = self.drawn?;
        let (left, top) = self.game;
        Some([(x - left as f32).round() as i32, (y - top as f32).round() as i32, half_w.round() as i32, half_h.round() as i32])
    }

    pub fn hide(&mut self) {
        if self.drawn.take().is_some() {
            self.hidden = Some(std::time::Instant::now());
            // SAFETY: a window this mark made.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
}

/// Puts the `width` x `height` premultiplied BGRA `pixels` on the window
/// at (`left`, `top`); false when Windows refuses.
/// The part of a BGRA bitmap placed at `rect` (left, top, width, height)
/// inside `clip`, and where that part sits; None when none of it is.
fn clipped(pixels: &[u8], [left, top, width, height]: [i32; 4], [cl, ct, cw, ch]: [i32; 4]) -> Option<(Vec<u8>, [i32; 4])> {
    let (x0, y0) = (left.max(cl), top.max(ct));
    let (x1, y1) = ((left + width).min(cl + cw), (top + height).min(ct + ch));
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let rect = [x0, y0, x1 - x0, y1 - y0];
    if rect == [left, top, width, height] {
        return Some((pixels.to_vec(), rect));
    }
    let row = (x1 - x0) as usize * 4;
    let mut out = Vec::with_capacity(row * (y1 - y0) as usize);
    for y in y0..y1 {
        let from = (((y - top) * width + (x0 - left)) * 4) as usize;
        out.extend_from_slice(&pixels[from..from + row]);
    }
    Some((out, rect))
}

fn update(hwnd: HWND, pixels: &[u8], width: i32, height: i32, left: i32, top: i32) -> bool {
    let info = dib_info(width, height);
    // SAFETY: the DC and bitmap are made, used and freed here; the bitmap
    // holds width * height * 4 bytes, the length of `pixels`.
    unsafe {
        let dc = CreateCompatibleDC(None);
        let mut bits = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return false;
        };
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len());
        let old = SelectObject(dc, bitmap.into());
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let (at, size, from) = (POINT { x: left, y: top }, SIZE { cx: width, cy: height }, POINT::default());
        let done = UpdateLayeredWindow(hwnd, None, Some(&at), Some(&size), Some(dc), Some(&from), COLORREF(0), Some(&blend), ULW_ALPHA);
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        done.is_ok()
    }
}

impl Drop for Mark {
    fn drop(&mut self) {
        // SAFETY: a window this mark made, destroyed once.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// A line the player should not miss, on the game above its board: gold
/// text on a dark box with a gold rim, on a window like the mark's; hidden
/// until [`Notice::show`], destroyed on drop.
pub struct Notice {
    hwnd: HWND,
    /// What the window shows: the text, and where (see [`Notice::place`]).
    drawn: Option<(String, [i32; 5])>,
    /// The text's and rim's colour.
    colour: [u8; 3],
}

/// The notice's text height, its box's padding and its rim, in reference px.
const NOTICE_TEXT: f32 = 28.0;
const NOTICE_PAD: f32 = 12.0;
const NOTICE_RIM: f32 = 2.0;
/// The notice box's opacity.
const NOTICE_ALPHA: f32 = 215.0 / 255.0;

impl Notice {
    /// A hidden notice window, gold; None when Windows refuses one.
    pub fn new() -> Option<Notice> {
        Some(Notice { hwnd: overlay_window()?, drawn: None, colour: GOLD })
    }

    /// A hidden chain badge window, green as the mark: the player asked for
    /// the Cage Fight's numbers in the mark's colour (2026-11-01).
    pub fn badge() -> Option<Notice> {
        Some(Notice { hwnd: overlay_window()?, drawn: None, colour: GREEN })
    }

    /// A hidden window for the advised move's expected value on the game, green as the mark (see
    /// [`Notice::show_value`]).
    pub fn value() -> Option<Notice> {
        Notice::badge()
    }

    /// Shows `text` centred on `x` with its box's top at `top`, in physical
    /// screen px, `unit` screen px per reference px, wrapped to lines at
    /// most `max_width` px wide; drawn again only when one of them changes.
    pub fn show(&mut self, text: &str, x: f32, top: f32, unit: f32, max_width: f32) {
        let key = [x.round() as i32, top.round() as i32, 0, (unit * 1000.0).round() as i32, max_width.round() as i32];
        let colour = self.colour;
        self.place(text, key, || notice_bitmap(text, unit, max_width, colour, x, |_| top));
    }

    /// Shows the expected value `text` (see [`value_bitmap`]) centred on `x`, just above the mark whose top and bottom
    /// are `reach` (see [`reach`]), or below it when there is no room above in the game (whose top is `game_top`);
    /// screen px, `unit` screen px per reference px. Drawn again only when one of them changes.
    pub fn show_value(&mut self, text: &str, x: f32, reach: (f32, f32), game_top: f32, unit: f32) {
        let key = [x.round() as i32, reach.0.round() as i32, reach.1.round() as i32, (unit * 1000.0).round() as i32, game_top.round() as i32];
        self.place(text, key, || value_bitmap(text, x, reach, game_top, unit));
    }

    /// Puts `bitmap()` (premultiplied BGRA pixels and their left, top, width and height in screen px) on the window,
    /// unless `text` is already on show placed as `key`.
    fn place(&mut self, text: &str, key: [i32; 5], bitmap: impl FnOnce() -> Option<(Vec<u8>, [i32; 4])>) {
        if self.drawn.as_ref().is_some_and(|(t, k)| t == text && *k == key) {
            return;
        }
        let Some((pixels, [left, top, width, height])) = bitmap() else { return };
        if dpi_aware(|| update(self.hwnd, &pixels, width, height, left, top)) {
            self.drawn = Some((text.to_string(), key));
            // SAFETY: a window this notice made.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
    }

    /// Whether the notice is on show.
    pub fn shown(&self) -> bool {
        self.drawn.is_some()
    }

    pub fn hide(&mut self) {
        if self.drawn.take().is_some() {
            // SAFETY: a window this notice made.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
}

impl Drop for Notice {
    fn drop(&mut self) {
        // SAFETY: a window this notice made, destroyed once.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// A 32-bit DIB's header, rows top down (a negative height).
fn dib_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A notice's bitmap (see [`paint_notice`]) centred on `x` with its top at `top(height)`: its pixels and left, top,
/// width and height in screen px.
fn notice_bitmap(text: &str, unit: f32, max_width: f32, colour: [u8; 3], x: f32, top: impl Fn(f32) -> f32) -> Option<(Vec<u8>, [i32; 4])> {
    let (pixels, width, height) = dpi_aware(|| paint_notice(text, unit, max_width.round() as i32, colour))?;
    Some((pixels, [(x - width as f32 / 2.0).round() as i32, top(height as f32).round() as i32, width, height]))
}

/// The expected value's bitmap (see [`Notice::show_value`]): green as the mark, its text [`VALUE_SCALE`] of the
/// notice's, centred on `x` and placed by [`value_top`].
pub fn value_bitmap(text: &str, x: f32, (above, below): (f32, f32), game_top: f32, unit: f32) -> Option<(Vec<u8>, [i32; 4])> {
    let unit = unit * VALUE_SCALE;
    notice_bitmap(text, unit, 600.0 * unit, GREEN, x, |height| value_top(above, below, height, game_top))
}

/// The notice for `text` at `unit` screen px per reference px, its box at
/// most `max_width` px wide (the text wraps), as premultiplied BGRA pixels,
/// width and height. Windows draws the text white on black in grey
/// anti-aliasing, whose level is the text's coverage.
fn paint_notice(text: &str, unit: f32, max_width: i32, colour: [u8; 3]) -> Option<(Vec<u8>, i32, i32)> {
    let px = (NOTICE_TEXT * unit).round().max(10.0) as i32;
    let (pad, rim) = ((NOTICE_PAD * unit).round() as i32, (NOTICE_RIM * unit).round().max(1.0) as i32);
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: the DC, font and bitmap are made, used and freed here; the
    // bitmap holds width * height * 4 bytes, read after GdiFlush.
    unsafe {
        let dc: HDC = CreateCompatibleDC(None);
        let font = CreateFontW(-px, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY, 0, w!("Microsoft JhengHei UI"));
        let old_font = SelectObject(dc, font.into());
        let format = DT_WORDBREAK | DT_CENTER | DT_NOPREFIX;
        let mut size = RECT { right: (max_width - 2 * pad).max(px), ..Default::default() };
        DrawTextW(dc, &mut wide, &mut size, DT_CALCRECT | format);
        let (width, height) = (size.right + 2 * pad, size.bottom + pad);
        let mut bits = std::ptr::null_mut();
        let made = CreateDIBSection(Some(dc), &dib_info(width, height), DIB_RGB_COLORS, &mut bits, None, 0);
        let result = made.ok().map(|bitmap| {
            let old = SelectObject(dc, bitmap.into());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
            let mut inner = RECT { left: pad, top: pad / 2, right: width - pad, bottom: height };
            DrawTextW(dc, &mut wide, &mut inner, format);
            let _ = GdiFlush();
            let drawn = std::slice::from_raw_parts(bits.cast::<u8>(), (width * height * 4) as usize).to_vec();
            SelectObject(dc, old);
            let _ = DeleteObject(bitmap.into());
            (compose_notice(&drawn, width, height, rim, colour), width, height)
        });
        SelectObject(dc, old_font);
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(dc);
        result
    }
}

/// The notice's pixels from the text drawn white on black (`drawn`, BGRA):
/// text in `colour` over a dark box with a rim `rim` px wide in it,
/// premultiplied.
fn compose_notice(drawn: &[u8], width: i32, height: i32, rim: i32, colour: [u8; 3]) -> Vec<u8> {
    let mut out = vec![0u8; drawn.len()];
    for (i, (px, d)) in out.as_chunks_mut::<4>().0.iter_mut().zip(drawn.as_chunks::<4>().0).enumerate() {
        let (x, y) = (i as i32 % width, i as i32 / width);
        let on_rim = x < rim || y < rim || x >= width - rim || y >= height - rim;
        // The text's coverage, or the rim's.
        let t = if on_rim { 1.0 } else { d[0].max(d[1]).max(d[2]) as f32 / 255.0 };
        let alpha = t + NOTICE_ALPHA * (1.0 - t);
        let [r, g, b] = colour.map(|c| (c as f32 * t).round() as u8);
        px.copy_from_slice(&[b, g, r, (alpha * 255.0).round() as u8]);
    }
    out
}

/// A `width` x `height` premultiplied BGRA mark for a target of half size
/// `half_w` x `half_h` centred on (`cx`, `cy`), `unit` px per reference px:
/// a box all round and an arrow, a bright core in a dark edge over a soft
/// glow, anti-aliased.
fn paint((width, height): (usize, usize), (cx, cy): (f32, f32), (half_w, half_h): (f32, f32), unit: f32, style: Style) -> Vec<u8> {
    let (stroke, arrow) = style.terms();
    let (stroke, edge, glow) = (stroke * unit, EDGE * unit, GLOW * unit);
    // The padded box's corner.
    let (bx, by) = (half_w + PAD * unit, half_h + PAD * unit);
    // The arrow's tip, below the centre of its top side.
    let (tip, arrow_w, arrow_h) = (-by - stroke / 2.0 - ARROW_GAP * unit, ARROW_W * arrow * unit, ARROW_H * arrow * unit);
    let side_len = (arrow_h * arrow_h + arrow_w * arrow_w / 4.0).sqrt();
    let mut out = vec![0u8; width * height * 4];
    for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = ((i % width) as f32 + 0.5 - cx, (i / width) as f32 + 0.5 - cy);
        // The four sides mirror each other.
        let (ax, ay) = (x.abs(), y.abs());
        let d = segment(ax, ay, (0.0, by), (bx, by)).min(segment(ax, ay, (bx, 0.0), (bx, by)));
        // Signed distance to the triangle: the largest of its sides'.
        let q = y - tip;
        let s = (-q - arrow_h).max((ax * arrow_h + q * arrow_w / 2.0) / side_len);
        let core = (stroke / 2.0 - edge - d + 0.5).clamp(0.0, 1.0).max((-s - edge + 0.5).clamp(0.0, 1.0));
        let rim = (stroke / 2.0 - d + 0.5).clamp(0.0, 1.0).max((-s + 0.5).clamp(0.0, 1.0));
        let top = core + rim * EDGE_ALPHA * (1.0 - core);
        // The glow fades out from the lines' edges, under them.
        let away = (d - stroke / 2.0).min(s).max(0.0);
        let halo = GLOW_ALPHA * (1.0 - away / glow).clamp(0.0, 1.0).powi(2);
        let under = halo * (1.0 - top);
        let alpha = top + under;
        let [r, g, b] = GREEN.map(|c| (c as f32 * (core + under)).round() as u8);
        px.copy_from_slice(&[b, g, r, (alpha * 255.0).round() as u8]);
    }
    out
}

/// The distance from (`x`, `y`) to the segment from `a` to `b`.
fn segment(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy).max(f32::EPSILON)).clamp(0.0, 1.0);
    let (ex, ey) = (x - a.0 - t * dx, y - a.1 - t * dy);
    (ex * ex + ey * ey).sqrt()
}

/// A mouse button, as [`button_down`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// Each button's "pressed since the last call" bit (by [`Button`] in order), kept for [`Clicks`]: Windows clears it
/// for the whole process on every `GetAsyncKeyState` call, and click-through's thread reads the buttons too (see
/// [`button_down`]), so a quick click between two looks went to whichever read first.
static PRESSED: [AtomicBool; 3] = [const { AtomicBool::new(false) }; 3];

/// Whether `button` is down now. Every read of the mouse buttons in this app goes through here, so the bit "pressed
/// since the last call" that the read clears is kept in [`PRESSED`] for [`Clicks`] whoever read it.
pub fn button_down(button: Button) -> bool {
    let vk = match button {
        Button::Left => VK_LBUTTON,
        Button::Right => VK_RBUTTON,
        Button::Middle => VK_MBUTTON,
    };
    // SAFETY: a plain Win32 call.
    let state = unsafe { GetAsyncKeyState(vk.0 as i32) } as u16;
    if state & 1 != 0 {
        PRESSED[button as usize].store(true, Ordering::SeqCst);
    }
    state & 0x8000 != 0
}

/// Whether `button` was pressed since the last look (any read in this app counts, see [`button_down`]); clears it.
fn take_pressed(button: Button) -> bool {
    PRESSED[button as usize].swap(false, Ordering::SeqCst)
}

/// One look at the mouse for [`Clicks`]: whether the left button is down, whether it was pressed since the last look,
/// whether the right or middle button is down or was pressed since (`other`), and where the cursor is in physical
/// screen px (None when it is on this app's own window over the game, or unknown).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub down: bool,
    pub pressed: bool,
    pub other: bool,
    pub at: Option<(i32, i32)>,
}

/// Looks at the buttons and the cursor now, see [`Look`]. It takes the "pressed since" bits: one call per look.
pub fn look() -> Look {
    // SAFETY: plain Win32 calls; the point is a valid out pointer.
    let (down, other, cursor) = dpi_aware(|| {
        let down = button_down(Button::Left);
        // Both read, not one or the other: each read keeps its own "pressed since" bit.
        let (right, middle) = (button_down(Button::Right), button_down(Button::Middle));
        let mut p = POINT::default();
        (down, right || middle, unsafe { GetCursorPos(&mut p).ok().map(|_| (p, WindowFromPoint(p))) })
    });
    let pressed = take_pressed(Button::Left);
    let other = other | take_pressed(Button::Right) | take_pressed(Button::Middle);
    Look { down, pressed, other, at: cursor.filter(|&(_, under)| !ours(under)).map(|(p, _)| (p.x, p.y)) }
}

/// One click as [`Clicks`] saw it, in physical screen px: where the button went down, where it came up, and the box
/// around every cursor position seen while it was held (`min`, `max`, the two ends included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Click {
    pub down: (i32, i32),
    pub up: (i32, i32),
    pub min: (i32, i32),
    pub max: (i32, i32),
}

impl Click {
    /// A click down and up at `p` without moving.
    pub fn at(p: (i32, i32)) -> Click {
        Click { down: p, up: p, min: p, max: p }
    }

    /// The click with the cursor seen at `p` too, which is where it is now.
    fn through(self, p: (i32, i32)) -> Click {
        Click { up: p, min: (self.min.0.min(p.0), self.min.1.min(p.1)), max: (self.max.0.max(p.0), self.max.1.max(p.1)), ..self }
    }
}

/// The left button's clicks as a game button takes them: pressed on it, held without leaving it, and released on it
/// (2026-11-08, the user: pressed on the box and let go elsewhere, and pressed on it, moved off and back and let go
/// on it: the game played neither and the box went). From looks 50 ms apart, every cursor position seen while held
/// goes into the click's box ([`Click::min`], [`Click::max`]); a move off and back between two looks is not seen. The
/// right or middle button pressed while the left is held cancels it too (2026-11-08, the user: the game dropped the
/// pick); one pressed by the look the left first shows down, or comes up, counts as during it, the order unknown. A
/// click quicker than the looks, down and up between two, shows only as "pressed since": the cursor then is taken
/// for the whole click.
#[derive(Default)]
pub struct Clicks {
    /// The button is down since a look: the click so far, None when it does not count (it went down on this app's
    /// window or was held over it, or was down already when the clicks were forgotten).
    held: Option<Option<Click>>,
}

impl Clicks {
    /// Forgets the clicks so far (no mark was on show): a press under way does not count when it comes up.
    pub fn forget(&mut self) {
        let down = button_down(Button::Left);
        for button in [Button::Left, Button::Right, Button::Middle] {
            button_down(button);
            take_pressed(button);
        }
        self.held = down.then_some(None);
    }

    /// The click that ended by `look`, every position seen on the game; None while the button is down, when there
    /// was no click, when the cursor was on this app's window (or unknown) at a look while it was held, or when the
    /// right or middle button was pressed meanwhile.
    pub fn next(&mut self, look: Look) -> Option<Click> {
        let at = look.at.filter(|_| !look.other);
        if look.down {
            let held = match self.held {
                None => at.map(Click::at),
                Some(click) => click.zip(at).map(|(click, p)| click.through(p)),
            };
            self.held = Some(held);
            return None;
        }
        match self.held.take() {
            Some(click) => click.zip(at).map(|(click, p)| click.through(p)),
            None if look.pressed => at.map(Click::at),
            None => None,
        }
    }
}

/// Whether the point `(x, y)` lies in `rect` (left, top, width, height, all in the same px).
pub fn inside([left, top, width, height]: [i32; 4], (x, y): (i32, i32)) -> bool {
    (left..left + width).contains(&x) && (top..top + height).contains(&y)
}

/// Where a click counts as one on the mark of target `[x, y, half_w, half_h, unit]` (screen px, see [`Mark::show`]):
/// the target and [`HIT_PAD`] reference px around it, as left, top, width and height in screen px.
pub fn hit_rect([x, y, half_w, half_h, unit]: [f32; 5]) -> [i32; 4] {
    let (w, h) = (half_w + HIT_PAD * unit, half_h + HIT_PAD * unit);
    [(x - w).floor() as i32, (y - h).floor() as i32, (2.0 * w).ceil() as i32, (2.0 * h).ceil() as i32]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At 1080p, a 200 x 200 target centred on (200.5, 250.5): its padded
    /// corner sits on the centre of pixel (308, 358), its sides' middles on
    /// x 308 and y 358, the arrow's tip at y 129.
    /// At 1440p (4/3 screen px a reference px): the middle door read at x
    /// 939 and 942 is one target; the doors beside it and the next cell are
    /// others.
    #[test]
    fn a_target_read_a_few_px_off_is_the_same() {
        let u = 4.0 / 3.0;
        let door = |x: f32| [x * u, 835.0 * u, 165.0 * u, 85.0 * u, u];
        let cell = |x: f32| [x * u, 400.0 * u, 65.0 * u, 65.0 * u, u];
        assert!(same_target(&door(939.0), &door(942.0)));
        assert!(same_target(&door(942.0), &door(939.0)));
        assert!(!same_target(&door(939.0), &door(614.0)));
        assert!(!same_target(&door(959.0), &door(1304.0)));
        assert!(!same_target(&cell(500.0), &cell(675.0)));
        let button = |y: f32| [1500.0 * u, y * u, 70.0 * u, 70.0 * u, u];
        assert!(same_target(&button(800.0), &button(770.0)), "a button floated up under the cursor");
        assert!(!same_target(&cell(500.0), &[500.0 * u, 575.0 * u, 65.0 * u, 65.0 * u, u]), "the cell below");
        assert!(!same_target(&door(939.0), &[939.0 * u, 835.0 * u, 70.0 * u, 70.0 * u, u]), "another size");
    }

    /// At 1440p a hunt target in the top row (the 2026-11-07 run's cell 06, mark_at 1862, 187, 87 x 87): a click on
    /// it or on its box counts; one a little past the box, on the next cell along or on the row below does not.
    #[test]
    fn a_click_counts_on_the_target_and_its_box() {
        let u = 4.0 / 3.0;
        let hit = hit_rect([1862.0, 187.0, 87.0, 87.0, u]);
        assert_eq!(hit, [1752, 77, 220, 220]);
        assert!(inside(hit, (1862, 187)), "the target's centre");
        assert!(inside(hit, (1862 + 87 + 16, 187)), "on the loud box's line (12.5 reference px out)");
        assert!(!inside(hit, (1862 + 111, 187)), "past the box and its slack");
        assert!(!inside(hit, (1629, 187)), "the next cell along");
        assert!(!inside(hit, (1862, 374)), "the cell below");
        assert!(!inside(hit, (300, 700)), "elsewhere on the game");
    }

    /// A 4 x 3 bitmap at (10, 20), each pixel's blue its index: clipped at
    /// x 12 and y 22 it keeps the right half of its last row.
    #[test]
    fn a_mark_is_cut_at_the_game_edge() {
        let pixels: Vec<u8> = (0..12u8).flat_map(|i| [i, 0, 0, 255]).collect();
        let (out, rect) = clipped(&pixels, [10, 20, 4, 3], [0, 0, 12 + 2, 22 + 1]).unwrap();
        assert_eq!(rect, [10, 20, 4, 3]);
        assert_eq!(out, pixels);
        let (out, rect) = clipped(&pixels, [10, 20, 4, 3], [12, 22, 100, 100]).unwrap();
        assert_eq!(rect, [12, 22, 2, 1]);
        assert_eq!(out, vec![10, 0, 0, 255, 11, 0, 0, 255]);
        assert!(clipped(&pixels, [10, 20, 4, 3], [14, 0, 100, 100]).is_none());
    }

    #[test]
    fn box_and_arrow_are_green_and_the_target_clear() {
        let (w, h) = (420, 420);
        let out = paint((w, h), (200.5, 250.5), (100.0, 100.0), 1.0, Style::Plain);
        let at = |x: usize, y: usize| <[u8; 4]>::try_from(&out[(y * w + x) * 4..][..4]).unwrap();
        let green = [GREEN[2], GREEN[1], GREEN[0], 255];
        assert_eq!(at(200, 100), green, "the arrow's middle");
        assert_eq!(at(308, 358), green, "a corner");
        assert_eq!(at(92, 142), green, "the opposite corner");
        assert_eq!(at(308, 250), green, "a side's middle");
        assert_eq!(at(200, 250), [0; 4], "the target's centre");
        let glow = at(318, 250);
        assert!(glow[3] > 0 && glow[3] < 255 && glow[1] > glow[2], "the glow outside a side: {glow:?}");
        assert_eq!(at(340, 250), [0; 4], "past the glow");
    }

    /// The value sits just above the mark (its arrow included), and below it when that would leave the game's top;
    /// [`reach`] is where the mark's own bitmap ends.
    #[test]
    fn the_value_sits_above_the_mark_or_below() {
        let (u, half_h) = (4.0 / 3.0, 85.0 * 4.0 / 3.0);
        for style in [Style::Plain, Style::Loud] {
            let (above, below) = reach(half_h, u, style);
            let (_, [_, top, _, height]) = mark_bitmap([1000.0, 800.0, 100.0, half_h, u], style);
            assert!((800.0 - above - top as f32).abs() <= 0.5, "the mark's top");
            assert!((top + height) as f32 >= 800.0 + below - 0.5 && (top + height) as f32 <= 800.0 + below + 1.0, "the mark's bottom");
            assert!(above > below, "the arrow is above");
        }
        // Room above: the value's box ends where the mark begins.
        assert_eq!(value_top(500.0, 900.0, 30.0, 0.0), 470.0);
        assert_eq!(value_top(500.0, 900.0, 30.0, 470.0), 470.0);
        // None above (a target at the game's top): under the mark.
        assert_eq!(value_top(500.0, 900.0, 30.0, 480.0), 900.0);
    }

    /// Not a check: the picture of the mark and the expected value as the overlay windows put them over the game,
    /// for a look without the game (the windows themselves are never in a capture). Run by hand with `--ignored` and
    /// ASCENDANT_OVERLAY_SHOT=`frame|x,y,half_w,half_h|text|plain or loud|out.png`: a saved frame (a run folder's
    /// frame-*.jpg), the target in its px (the run log's `mark_at`, or a Vorax `marks` box), the value as the window
    /// writes it. The same bitmaps and placement as on screen ([`mark_bitmap`], [`value_bitmap`], [`reach`]), cut
    /// around the target.
    #[test]
    #[ignore]
    fn overlay_shot() {
        let Ok(spec) = std::env::var("ASCENDANT_OVERLAY_SHOT") else { return };
        let parts: Vec<&str> = spec.split('|').collect();
        let [frame, at, text, style, out] = parts[..] else { panic!("frame|x,y,half_w,half_h|text|style|out.png") };
        let mut img = crate::reader::load_image_raw(std::path::Path::new(frame)).unwrap();
        let at: Vec<f32> = at.split(',').map(|v| v.trim().parse().unwrap()).collect();
        let style = if style == "loud" { Style::Loud } else { Style::Plain };
        let unit = img.h as f32 / 1080.0;
        let over = |img: &mut crate::reader::Rgba, pixels: &[u8], [left, top, width, _]: [i32; 4]| {
            for (i, px) in pixels.as_chunks::<4>().0.iter().enumerate() {
                let (x, y) = (left + i as i32 % width, top + i as i32 / width);
                if x < 0 || y < 0 || x as usize >= img.w || y as usize >= img.h {
                    continue;
                }
                let k = (y as usize * img.w + x as usize) * 4;
                let keep = 1.0 - f32::from(px[3]) / 255.0;
                for (c, src) in [px[2], px[1], px[0]].into_iter().enumerate() {
                    img.data[k + c] = (f32::from(src) + f32::from(img.data[k + c]) * keep).round().min(255.0) as u8;
                }
            }
        };
        let (pixels, rect) = mark_bitmap([at[0], at[1], at[2], at[3], unit], style);
        over(&mut img, &pixels, rect);
        let (above, below) = reach(at[3], unit, style);
        let (value, [vl, vt, vw, vh]) = value_bitmap(text, at[0], (at[1] - above, at[1] + below), 0.0, unit).unwrap();
        over(&mut img, &value, [vl, vt, vw, vh]);
        // Cut around the mark and the value, with some of the game about them.
        let margin = (120.0 * unit) as i32;
        let x0 = (rect[0].min(vl) - margin).max(0) as usize;
        let y0 = (rect[1].min(vt) - margin).max(0) as usize;
        let x1 = ((rect[0] + rect[2]).max(vl + vw) + margin).min(img.w as i32) as usize;
        let y1 = ((rect[1] + rect[3]).max(vt + vh) + margin).min(img.h as i32) as usize;
        let mut cut = crate::reader::Rgba { w: x1 - x0, h: y1 - y0, data: Vec::with_capacity((x1 - x0) * (y1 - y0) * 4) };
        for y in y0..y1 {
            cut.data.extend_from_slice(&img.data[(y * img.w + x0) * 4..(y * img.w + x1) * 4]);
        }
        crate::reader::save_png(std::path::Path::new(out), &cut).unwrap();
        println!("{out}: {}x{}, value box at ({vl}, {vt}) {vw}x{vh}, mark at {rect:?}", cut.w, cut.h);
    }

    /// The notice's rim is opaque gold, its box dark and see-through, and
    /// fully covered text gold.
    #[test]
    fn notice_has_gold_text_on_a_dark_box() {
        let (w, h) = (10, 6);
        let mut drawn = vec![0u8; (w * h * 4) as usize];
        drawn[((3 * w + 5) * 4) as usize..][..3].fill(255);
        let out = compose_notice(&drawn, w, h, 1, GOLD);
        let at = |x: i32, y: i32| <[u8; 4]>::try_from(&out[((y * w + x) * 4) as usize..][..4]).unwrap();
        let gold = [GOLD[2], GOLD[1], GOLD[0], 255];
        assert_eq!(at(0, 0), gold, "the rim");
        assert_eq!(at(5, 3), gold, "the text");
        assert_eq!(at(3, 2), [0, 0, 0, (NOTICE_ALPHA * 255.0).round() as u8], "the box");
    }
}

