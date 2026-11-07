//! Click-through for the app window: clicks on its widgets stay in it, and
//! clicks anywhere else in it pass to the window under it (the game).
//!
//! A window takes no mouse input at all while it has `WS_EX_TRANSPARENT`, so
//! it cannot see the cursor come over a widget: a thread here follows the
//! cursor instead, and sets that style while the cursor is over the client
//! area but on none of the widgets' rects.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetClientRect, GetCursorPos, GetWindowLongPtrW, SetCursorPos, SetWindowLongPtrW, WS_EX_TRANSPARENT,
};

/// How often the thread looks at the cursor.
const POLL: Duration = Duration::from_millis(16);

/// The thread that sets `WS_EX_TRANSPARENT` by where the cursor is; stops
/// and clears the style on drop.
pub struct ClickThrough {
    /// The widgets' rects, left, top, right and bottom in client pixels.
    rects: Arc<Mutex<Vec<[i32; 4]>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    hwnd: isize,
}

impl ClickThrough {
    /// Starts following the cursor over `hwnd`, this app's window.
    pub fn start(hwnd: HWND) -> ClickThrough {
        let rects = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let raw = hwnd.0 as isize;
        let thread = {
            let (rects, stop) = (rects.clone(), stop.clone());
            std::thread::spawn(move || {
                let hwnd = HWND(raw as *mut _);
                while !stop.load(Ordering::SeqCst) {
                    let cursor = over_nothing(hwnd, &rects.lock().unwrap_or_else(|e| e.into_inner()));
                    if let Some(through) = cursor.filter(|_| !pressed())
                        && set_transparent(hwnd, through)
                        && !through
                    {
                        // Windows sends a click to the window it found under the
                        // cursor at its last move, the game's while this one took
                        // no input: a still cursor's click would go there. Setting
                        // the cursor where it is moves it, and finds this one.
                        let mut at = POINT::default();
                        // SAFETY: plain Win32 calls, with an out pointer to a local.
                        unsafe {
                            if GetCursorPos(&mut at).is_ok() {
                                let _ = SetCursorPos(at.x, at.y);
                            }
                        }
                    }
                    std::thread::sleep(POLL);
                }
            })
        };
        ClickThrough { rects, stop, thread: Some(thread), hwnd: raw }
    }

    /// The widgets' rects as of the last frame, in client pixels.
    pub fn set_rects(&self, rects: Vec<[i32; 4]>) {
        *self.rects.lock().unwrap_or_else(|e| e.into_inner()) = rects;
    }
}

impl Drop for ClickThrough {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = set_transparent(HWND(self.hwnd as *mut _), false);
    }
}

/// Whether the cursor is in `hwnd`'s client area on none of `rects`; None
/// when Windows can't tell. The title bar and the borders take clicks.
fn over_nothing(hwnd: HWND, rects: &[[i32; 4]]) -> Option<bool> {
    let mut at = POINT::default();
    let mut client = RECT::default();
    // SAFETY: plain Win32 calls on this app's own window, with out pointers to locals.
    unsafe {
        GetCursorPos(&mut at).ok()?;
        ScreenToClient(hwnd, &mut at).ok().ok()?;
        GetClientRect(hwnd, &mut client).ok()?;
    }
    let inside = |[l, t, r, b]: [i32; 4]| at.x >= l && at.x < r && at.y >= t && at.y < b;
    Some(inside([client.left, client.top, client.right, client.bottom]) && !rects.iter().any(|&r| inside(r)))
}

/// A mouse button is down: the style stays as it is until it comes up, so
/// a drag that leaves a widget (a slider's) keeps going. The buttons are read through [`super::mark::button_down`],
/// which keeps the "pressed since" bits this read clears for the mark's clicks (this thread looks every 16 ms and
/// took a quick click's bit before the mark's look 50 ms apart could see it).
fn pressed() -> bool {
    use super::mark::{Button, button_down};
    [Button::Left, Button::Right, Button::Middle].into_iter().any(button_down)
}

/// Sets or clears `WS_EX_TRANSPARENT` on `hwnd`; returns whether it differed.
fn set_transparent(hwnd: HWND, on: bool) -> bool {
    let bit = WS_EX_TRANSPARENT.0 as isize;
    // SAFETY: plain Win32 calls on this app's own window.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let want = if on { style | bit } else { style & !bit };
        if want != style {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
        }
        want != style
    }
}
