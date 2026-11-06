//! Captures the game: finds its window by its process, else by a part of the
//! title, and copies its client area with Windows Graphics Capture, which sees DirectX and OpenGL
//! windows. Where that isn't available it falls back to a copy from the screen
//! (BitBlt), which needs the window in view and misses flip-model swapchains.
//! [`Watcher`] keeps one capture session open for auto read.

use super::gamelog::{self, Signal};
use super::session::{Trigger, Watch, fingerprint};
use super::vorax_session::{self, Cue};
use crate::lang::{tr, trf};
use crate::reader::Rgba;
use serde_json::{Value, json};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
use windows::Graphics::Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::SizeInt32;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{CloseHandle, HMODULE, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDC, GetDIBits, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromPoint,
    MonitorFromWindow, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetClientRect, GetCursorPos, GetForegroundWindow, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, InternalGetWindowText, IsIconic, IsWindowVisible,
};
use windows::core::{BOOL, Interface, PWSTR, factory};

/// How long to wait for Windows Graphics Capture to deliver a frame.
const FRAME_TIMEOUT: Duration = Duration::from_secs(1);
/// How often the watcher samples the game; see [`super::session::SETTLE`].
/// At 100 ms a screen that stopped was read about 0.35 s later; the player
/// would rather spend some CPU on the board for advice sooner.
const SAMPLE: Duration = Duration::from_millis(60);

/// How often the watcher samples the game while it rests off the board (see
/// [`Watch::resting`]): in the hideout a sample every 100 ms and a read
/// every 1.5 to 3 s kept about 4% of the CPU and 1.5% of the GPU busy for
/// nothing; a big change brings the full pace back at once.
const REST: Duration = Duration::from_millis(500);
/// How long the watcher waits after a failure before looking for the window again.
const RETRY: Duration = Duration::from_secs(1);
/// Frames the capture pool holds.
const BUFFERS: i32 = 2;
const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
/// Console host windows: their title is the command line, which can contain the searched text.
const CONSOLE_CLASSES: [&str; 2] = ["ConsoleWindowClass", "CASCADIA_HOSTING_WINDOW_CLASS"];
/// The game's executable: its window is the one captured, whatever other windows' titles hold.
const GAME_EXE: &str = "torchlight_infinite.exe";

struct Search {
    needle: String,
    /// A window of [`GAME_EXE`] ends the search.
    game: Option<HWND>,
    /// The first window titled exactly `needle`.
    exact: Option<HWND>,
    /// The first window whose title contains `needle`.
    partial: Option<HWND>,
}

unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the &mut Search passed to EnumWindows below, alive for the call.
    let search = unsafe { &mut *(lparam.0 as *mut Search) };
    // This process's windows are left out before their title is read, with calls that send them nothing
    // (IsWindowVisible, GetWindowThreadProcessId, GetClassNameW): GetWindowTextW on a window of this process owned by
    // another thread is a SendMessage(WM_GETTEXT) with no timeout, and with the UI thread waiting in Watcher's drop
    // for this thread the two waited on each other for good (2026-11-06, auto read turned off, then closing hung).
    // SAFETY: hwnd comes from EnumWindows.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() || own_or_console(hwnd) {
        return BOOL(1);
    }
    let title = window_title(hwnd).to_lowercase();
    if title.is_empty() {
        return BOOL(1);
    }
    let pid = window_pid(hwnd);
    if process_file(pid).is_some_and(|f| f.eq_ignore_ascii_case(GAME_EXE)) {
        search.game = Some(hwnd);
        return BOOL(0);
    }
    if title == search.needle {
        search.exact.get_or_insert(hwnd);
    } else if title.contains(&search.needle) {
        search.partial.get_or_insert(hwnd);
    }
    BOOL(1)
}

/// The id of the process that owns the window.
fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: hwnd comes from EnumWindows; pid is a valid out pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// Whether the window belongs to this process or is a console host.
fn own_or_console(hwnd: HWND) -> bool {
    if ours(hwnd) {
        return true;
    }
    let mut class = [0u16; 64];
    // SAFETY: hwnd comes from EnumWindows; class is a valid buffer.
    let len = unsafe { GetClassNameW(hwnd, &mut class) };
    let class = String::from_utf16_lossy(&class[..len.max(0) as usize]);
    CONSOLE_CLASSES.contains(&class.as_str())
}

/// The file name of process `pid`'s executable, such as "notepad.exe";
/// none when the process can't be queried.
fn process_file(pid: u32) -> Option<String> {
    let path = process_path(pid)?;
    path.file_name().map(|f| f.to_string_lossy().into_owned())
}

/// The full path of process `pid`'s executable; none when the process can't be queried.
fn process_path(pid: u32) -> Option<std::path::PathBuf> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: the handle is opened here and closed before returning; buf and len
    // are a valid buffer and its length in characters.
    let path = unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let named = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        named.ok()?;
        String::from_utf16_lossy(&buf[..len as usize])
    };
    Some(std::path::PathBuf::from(path))
}

/// When process `pid` started; none when it can't be queried.
fn process_started(pid: u32) -> Option<SystemTime> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetProcessTimes;
    let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: the handle is opened here and closed before returning; the four FILETIMEs are valid out pointers.
    let read = unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let read = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        let _ = CloseHandle(process);
        read
    };
    read.ok()?;
    // FILETIME counts 100 ns ticks from 1601-01-01; the Unix epoch is 11644473600 s later.
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    let unix = (ticks / 10_000_000).checked_sub(11_644_473_600)?;
    Some(std::time::UNIX_EPOCH + Duration::from_secs(unix) + Duration::from_nanos(ticks % 10_000_000 * 100))
}

/// The running game: its exe and when it started, from the window `find` picks for `title`; none when that window
/// is not the game's ([`GAME_EXE`]) or the process can't be queried. Used to find and judge the game's log.
fn game_process(title: &str) -> Option<(std::path::PathBuf, Option<SystemTime>)> {
    let pid = window_pid(find(title)?);
    let path = process_path(pid)?;
    path.file_name().is_some_and(|f| f.to_string_lossy().eq_ignore_ascii_case(GAME_EXE)).then(|| (path, process_started(pid)))
}

/// Shows a new log status, waking the UI only when it changed (the follower re-checks every few seconds).
fn set_log_status(state: &Mutex<Shared>, notify: &dyn Fn(), status: gamelog::LogStatus) {
    let changed = {
        let mut shared = lock(state);
        let changed = shared.log != status;
        shared.log = status;
        changed
    };
    if changed {
        notify();
    }
}

/// How often a followed log's status is re-checked against the game (see [`gamelog::follow_status`]).
const LOG_CHECK: Duration = Duration::from_secs(2);

/// The visible top-level window of [`GAME_EXE`], else the first titled
/// `title` (any case), else the first whose title contains it, leaving out
/// this process's windows and console windows.
fn find(title: &str) -> Option<HWND> {
    let mut search = Search { needle: title.to_lowercase(), game: None, exact: None, partial: None };
    // SAFETY: the callback only touches `search`, which outlives the call. EnumWindows
    // reports an error when the callback stops early, which is how the game's window ends it.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
    search.game.or(search.exact).or(search.partial)
}

/// The window `find` picks for `title`, or why there is none.
fn find_window(title: &str) -> Result<HWND, String> {
    find(title).ok_or_else(|| {
        trf!("No Torchlight: Infinite window ({}), and no window with \"{}\" in its title: is the game running?", GAME_EXE, title)
    })
}

/// The window's title; empty when it has none. Read with InternalGetWindowText, which copies the title the window
/// manager keeps and sends the window no message, so a window whose thread is busy or hung cannot hold the caller up.
/// For another process's window GetWindowTextW reads that same stored title, so the windows found are unchanged.
fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: hwnd is a window handle from EnumWindows or a capture; buf is a valid buffer.
    let len = unsafe { InternalGetWindowText(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

/// The window's title and client size, as "\"Title\" (1920 x 1080)".
fn describe(hwnd: HWND) -> String {
    let title = window_title(hwnd);
    let (w, h) = client_rect(hwnd).map_or((0, 0), |r| (r.right - r.left, r.bottom - r.top));
    format!("\"{}\" ({} x {})", title.trim(), w, h)
}

/// Runs `f` with this thread aware of per-monitor DPI, so window rectangles
/// and the screen copy come in physical pixels, as the captured frame does.
pub(crate) fn dpi_aware<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: sets this thread's DPI awareness and restores it.
    unsafe {
        let old = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let result = f();
        if !old.is_invalid() {
            SetThreadDpiAwarenessContext(old);
        }
        result
    }
}

/// The client area of the window `find` picks for `title`,
/// or the reason it can't be captured.
pub fn capture(title: &str) -> Result<Rgba, String> {
    let hwnd = find_window(title)?;
    dpi_aware(|| capture_window(hwnd))
}

/// The game's client area in physical screen pixels: left, top, width, height.
pub fn game_rect(title: &str) -> Result<[i32; 4], String> {
    let hwnd = find_window(title)?;
    let r = dpi_aware(|| client_rect(hwnd))?;
    Ok([r.left, r.top, r.right - r.left, r.bottom - r.top])
}

/// Whether the game's window or one of this process's windows is in front.
pub fn game_in_front(title: &str) -> bool {
    // SAFETY: a plain Win32 call.
    let front = unsafe { GetForegroundWindow() };
    !front.is_invalid() && (ours(front) || find(title) == Some(front))
}

/// Whether `hwnd` is one of this process's windows.
pub(crate) fn ours(hwnd: HWND) -> bool {
    // SAFETY: a plain Win32 call.
    window_pid(hwnd) == unsafe { GetCurrentProcessId() }
}

/// Whether a point in physical screen pixels is on a monitor.
pub fn on_screen(x: i32, y: i32) -> bool {
    // SAFETY: a plain Win32 call on a point.
    dpi_aware(|| unsafe { !MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL).is_invalid() })
}

fn capture_window(hwnd: HWND) -> Result<Rgba, String> {
    let client = client_rect(hwnd)?;
    match first_frame(hwnd) {
        Ok(img) => Ok(img),
        Err(wgc) => screen_copy(client).map_err(|gdi| format!("Windows Graphics Capture failed ({wgc}); the screen copy failed too ({gdi}).")),
    }
}

/// The window's client area in screen coordinates, or why it can't be captured.
fn client_rect(hwnd: HWND) -> Result<RECT, String> {
    // SAFETY: plain Win32 calls on a window handle from `find`.
    let (client, origin) = unsafe {
        if IsIconic(hwnd).as_bool() {
            return Err(tr("The window is minimized.").to_string());
        }
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client).map_err(|e| format!("GetClientRect: {e}"))?;
        let mut origin = POINT::default();
        let _ = ClientToScreen(hwnd, &mut origin);
        (client, origin)
    };
    let (w, h) = (client.right - client.left, client.bottom - client.top);
    if w <= 0 || h <= 0 {
        return Err(tr("The window has no client area.").to_string());
    }
    Ok(RECT { left: origin.x, top: origin.y, right: origin.x + w, bottom: origin.y + h })
}

/// The first frame of a new Windows Graphics Capture session on the window.
fn first_frame(hwnd: HWND) -> Result<Rgba, String> {
    // Its own evidence decides fix 1 on the first frame (see Choice); a frame that cannot tell takes the old rule.
    Stream::open(hwnd)?.next_frame()
}

// ---------------------------------------------------------------------------------------------------------------
// Where the captured texture sits on screen (fix 1), the border check on its edges (fix 2), and what both measured
// for the run log.
//
// Which screen rectangle a Windows Graphics Capture window frame covers is documented nowhere by Microsoft, and the
// sources disagree:
// - OBS (libobs-winrt winrt-capture.cpp, get_client_box) takes the texture's origin to be the window's visible frame,
//   DWMWA_EXTENDED_FRAME_BOUNDS, and crops ClientToScreen minus that; this file did the same until 2026-11-06.
// - Measured by the diagnostic build on the player's Windows 10 19045 (run log 2026-11-06_02-59-14, log.jsonl lines
//   28, 30 and 35): a game window with WS_CAPTION | WS_THICKFRAME (style 0x14CF0000) whose client fills a 1920 x 1080
//   monitor had GetWindowRect (-1928, 332)-(8, 1451), 1936 x 1119, frame bounds (-1921, 332)-(1, 1444), 1922 x 1112
//   (invisible resize borders 7 px left, right and bottom, 0 on top; a 1 px visible border; a 31 px caption off
//   screen above), client (-1920, 363)-(0, 1443). The texture (desc) was 1922 x 1112, the frame bounds' size, yet
//   cropped from the frame bounds' corner (x0 = 1) the border check found 7 px on the left (6 black columns of the
//   invisible border and the 1 px visible border): the picture is laid out from GetWindowRect's corner and cut to
//   the frame bounds' size, so the client starts at x 8 and the texture holds only 1914 of its 1920 columns; the
//   game's last 6 columns are never captured. Unlike OBS's assumption.
// - On the main monitor (line 4 of the same log) the game is WS_POPUP (0x94000000) and all three rectangles are the
//   same, so every rule agrees there.
// - Windows 11: NOT VERIFIED (some say its texture covers the frame bounds from their own corner; no source found).
// So the origin is taken from the rectangles, the texture size (D3D11 desc, as OBS uses; ContentSize is called
// unreliable there) and, where they leave two candidates, the texture's own pixels:
// - texture of the frame bounds' size, GetWindowRect's corner different (invisible borders on the left or top): two
//   candidates, the frame bounds' corner (the old rule, OBS) and GetWindowRect's (as measured here). The texture's
//   own pixels choose ([`origin_evidence`]): black lines of the invisible border then the visible border line at its
//   edge mean GetWindowRect's corner, the visible border line at the very edge means the frame bounds'. The choice
//   starts on the old rule and changes only after [`TRIM_FRAMES`] frames in a row agree ([`Choice`]); dark frames
//   keep it; a geometry change starts it over. From GetWindowRect's corner, what the texture lacks of the client is
//   filled black ([`Crop::pad`]);
// - texture of the frame bounds' size, the corners the same (a popup, or borders only right and bottom): one
//   candidate, the old rule;
// - texture of the window rectangle's size: its corner (not seen yet; kept in case another Windows version does it);
// - neither size (a pool not yet recreated after a resize, or a behaviour not seen yet): the old rule, logged as a
//   warning. A popup is so cropped exactly as before in every case; the border check below is the safety net.
// NOT VERIFIED: Windows 11 and other builds; that the invisible border always captures black there.
// ---------------------------------------------------------------------------------------------------------------

/// A screen rectangle in physical pixels: left, top, right, bottom.
pub type Rect = [i32; 4];

fn rect_size(r: Rect) -> (i32, i32) {
    (r[2] - r[0], r[3] - r[1])
}

/// Which screen rectangle the captured texture's origin was taken to be, see [`texture_crop`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// DWMWA_EXTENDED_FRAME_BOUNDS: the texture had their size and the window rectangle has it too.
    Frame,
    /// GetWindowRect's corner, the texture cut to the frame bounds' size: what Windows 10 did on 2026-11-06 for a
    /// window with invisible resize borders.
    WindowCut,
    /// GetWindowRect, invisible resize borders included: the texture had its size and not the frame's.
    Window,
    /// Neither size matched: the frame bounds as the origin, the old rule (logged as a warning).
    Unmatched,
}

impl Origin {
    fn name(self) -> &'static str {
        match self {
            Origin::Frame => "frame",
            Origin::WindowCut => "window",
            Origin::Window => "window_full",
            Origin::Unmatched => "unmatched",
        }
    }
}

/// The client area inside the captured texture, see [`texture_crop`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub origin: Origin,
    /// The texture's top-left corner on screen.
    pub at: [i32; 2],
    /// The client area in texture pixels, clipped to the texture: x0, y0, x1, y1.
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    /// From each texture edge to the client's edge, unclipped: left, top, right, bottom. A negative one means the
    /// client reaches past the texture (the box is not inside it, as OBS checks).
    pub margins: [i32; 4],
    /// Black pixels added on each side (left, top, right, bottom) so the picture has the client's size where the
    /// texture lacks part of it: only for [`Origin::WindowCut`] (6 on the right on 2026-11-06); elsewhere the crop is
    /// clipped to the texture as before.
    pub pad: [i32; 4],
}

impl Crop {
    /// Whether the whole client area lies inside the texture.
    pub fn inside(&self) -> bool {
        self.margins.iter().all(|&m| m >= 0)
    }

    /// The picture's size: the clipped client area and the padding.
    pub fn size(&self) -> (usize, usize) {
        ((self.x1 - self.x0 + self.pad[0] + self.pad[2]) as usize, (self.y1 - self.y0 + self.pad[1] + self.pad[3]) as usize)
    }

    /// Where in the texture the picture's top-left pixel comes from (outside it when padded on the left or top).
    pub fn start(&self) -> (i32, i32) {
        (self.x0 - self.pad[0], self.y0 - self.pad[1])
    }
}

/// Fix 1: where the client area lies in a captured texture of size `texture` (D3D11 desc width and height), given
/// the window's GetWindowRect `window`, its DWMWA_EXTENDED_FRAME_BOUNDS `frame` and its client area `client` (from
/// ClientToScreen), all in physical screen pixels; see the comment above [`Rect`] for which corner is the texture's
/// origin. A texture of the frame bounds' size is taken as laid out from GetWindowRect's corner only when `cut` (see
/// [`origin_evidence`] and [`Cropper`]), else from the frame bounds' (the old rule); when the two corners are the same
/// `cut` changes nothing. The client area is clipped to the texture, and for [`Origin::WindowCut`] padded back to its
/// full size.
pub fn texture_crop(texture: (i32, i32), window: Rect, frame: Rect, client: Rect, cut: bool) -> Crop {
    let (tw, th) = texture;
    let (origin, at) = if cut && texture == rect_size(frame) && corners_differ(window, frame) {
        (Origin::WindowCut, [window[0], window[1]])
    } else if texture == rect_size(frame) {
        (Origin::Frame, [frame[0], frame[1]])
    } else if texture == rect_size(window) {
        (Origin::Window, [window[0], window[1]])
    } else {
        (Origin::Unmatched, [frame[0], frame[1]])
    };
    let x0 = (client[0] - at[0]).clamp(0, tw);
    let y0 = (client[1] - at[1]).clamp(0, th);
    let x1 = (client[2] - at[0]).clamp(x0, tw);
    let y1 = (client[3] - at[1]).clamp(y0, th);
    let margins = [client[0] - at[0], client[1] - at[1], at[0] + tw - client[2], at[1] + th - client[3]];
    let pad = if origin == Origin::WindowCut && x1 > x0 && y1 > y0 { margins.map(|m| (-m).max(0)) } else { [0; 4] };
    Crop { origin, at, x0, y0, x1, y1, margins, pad }
}

/// Whether GetWindowRect's corner differs from the frame bounds' (invisible borders on the left or top): only then
/// do the two candidate origins of a frame-sized texture differ.
fn corners_differ(window: Rect, frame: Rect) -> bool {
    (window[0], window[1]) != (frame[0], frame[1])
}

/// What a frame-sized texture's own pixels say about its origin, when the window has invisible borders on the left
/// or top (else None: both candidates are the same). Some(true), laid out from GetWindowRect's corner (as measured
/// on Windows 10): on each such side the invisible border's lines are black ([`is_black`]) and the visible frame's
/// lines up to the client are a border line ([`border_line`]). Some(false), laid out from the frame bounds' corner
/// (OBS's assumption): the outermost line is not black, the visible frame's lines are border lines, and the client's
/// first line is not black. None when neither holds on every such side, or the picture is too dark to judge
/// ([`DARK_MEAN`]). `px` is the whole texture (`tw` x `th` 4-byte pixels, `pitch` bytes a row).
pub fn origin_evidence(px: &[u8], pitch: usize, (tw, th): (i32, i32), window: Rect, frame: Rect, client: Rect) -> Option<bool> {
    if !corners_differ(window, frame) || (tw, th) != rect_size(frame) {
        return None;
    }
    let (w, h) = (tw as usize, th as usize);
    if w < 8 * TRIM_MAX || h < 8 * TRIM_MAX || too_dark(px, pitch, w, h) {
        return None;
    }
    let line = |side: usize, k: usize| edge_line(px, pitch, w, h, side, k);
    let mut verdict = None;
    for side in [0, 1] {
        // Invisible border (window to frame), and the visible frame (frame to client), along this axis.
        let (hidden, visible) = (frame[side] - window[side], client[side] - frame[side]);
        if hidden == 0 {
            continue;
        }
        // NOT VERIFIED: at 150% DPI or more the invisible border and the visible frame together reach TRIM_MAX
        // (about 11 + 2 px at 150%), so no evidence is ever found and fix 1 keeps the old rule; fix 2 then stops at
        // the same limit (side_border), so both fall back to the old offset there.
        if hidden < 0 || visible < 0 || (hidden + visible) as usize >= TRIM_MAX {
            return None;
        }
        let (hidden, visible) = (hidden as usize, visible as usize);
        let from_window = (0..hidden).all(|k| is_black(&line(side, k))) && (hidden..hidden + visible).all(|k| border_line(&line(side, k)).is_some());
        let from_frame = !is_black(&line(side, 0))
            && (0..visible).all(|k| border_line(&line(side, k)).is_some())
            && !is_black(&line(side, visible));
        let here = match (from_window, from_frame) {
            (true, false) => true,
            (false, true) => false,
            _ => return None,
        };
        if verdict.is_some_and(|v| v != here) {
            return None;
        }
        verdict = Some(here);
    }
    verdict
}

/// Fix 1's choice between the two origins of a frame-sized texture over one capture session: the frame bounds'
/// corner (the old rule) until a frame's [`origin_evidence`] first tells, which is taken at once (so a session is
/// aligned from its first readable frame, a single-frame capture included); after that the choice changes only when
/// [`TRIM_FRAMES`] frames in a row tell the other way, and a frame that tells neither way breaks the run and keeps
/// it. Nothing is carried from one session to the next: two sessions open on the same window at the same size were
/// seen laid out differently (2026-11-06, run log 2026-11-06_03-05-30), so each judges by its own pixels.
#[derive(Clone, Debug, Default)]
pub struct Choice {
    cut: bool,
    /// Whether any frame has told yet.
    decided: bool,
    pending: Option<(bool, u8)>,
}

impl Choice {
    /// Whether the texture is taken as laid out from GetWindowRect's corner.
    pub fn cut(&self) -> bool {
        self.cut
    }

    /// Takes one frame's evidence; whether the choice changed.
    pub fn feed(&mut self, seen: Option<bool>) -> bool {
        if !self.decided
            && let Some(seen) = seen
        {
            let changed = seen != self.cut;
            (self.cut, self.decided, self.pending) = (seen, true, None);
            return changed;
        }
        let Some(seen) = seen.filter(|&s| s != self.cut) else {
            self.pending = None;
            return false;
        };
        let n = match self.pending {
            Some((s, n)) if s == seen => n + 1,
            _ => 1,
        };
        if n >= TRIM_FRAMES {
            (self.cut, self.pending) = (seen, None);
            true
        } else {
            self.pending = Some((seen, n));
            false
        }
    }
}

/// One frame's picture from [`Cropper::picture`].
#[derive(Clone, Debug)]
pub struct Picture {
    /// RGBA, `w` x `h` (the client's size, padded where the texture lacks it).
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
    pub crop: Crop,
    /// This frame's [`origin_evidence`], and whether fix 1's choice changed on it.
    pub evidence: Option<bool>,
    pub origin_changed: bool,
    /// The trim fix 2 started or stopped cutting on this frame.
    pub trim_changed: Option<Edges>,
}

/// What a crop's choice and trim depend on: the window's GetWindowRect, frame bounds and client area, and the
/// texture's size.
pub type Key = (Rect, Rect, Rect, (i32, i32));

/// Fix 1 and fix 2 over one capture session's frames; it starts over (the old rule, no trim) whenever the geometry
/// ([`Key`]) changes. Nothing is shared between sessions (see [`Choice`]).
#[derive(Clone, Debug, Default)]
pub struct Cropper {
    choice: Choice,
    trim: Trim,
    evidence: Option<bool>,
    /// The geometry the choice and trim are for.
    key: Option<Key>,
}

impl Cropper {
    /// Starts over (the window's geometry changed).
    pub fn reset(&mut self) {
        *self = Cropper::default();
    }

    /// Whether fix 1 takes a frame-sized texture as laid out from GetWindowRect's corner.
    pub fn cut(&self) -> bool {
        self.choice.cut()
    }

    /// The border fix 2 cuts now.
    pub fn trim(&self) -> Edges {
        self.trim.applied()
    }

    /// The last frame's [`origin_evidence`].
    pub fn evidence(&self) -> Option<bool> {
        self.evidence
    }

    /// The client's picture out of a BGRA texture (`tw` x `th`, `pitch` bytes a row): fix 1's origin, chosen by the
    /// texture's own pixels ([`origin_evidence`], [`Choice`]), then fix 2's border check on the part of the texture
    /// the crop holds. A change of origin drops any trim, so the two never move the picture twice; on the frame the
    /// origin changes, fix 2 starts counting afresh. An error when the texture holds none of the client.
    pub fn picture(&mut self, src: &[u8], pitch: usize, texture: (i32, i32), window: Rect, frame: Rect, client: Rect) -> Result<Picture, String> {
        let key = (window, frame, client, texture);
        if self.key != Some(key) {
            *self = Cropper { key: Some(key), ..Cropper::default() };
        }
        self.evidence = origin_evidence(src, pitch, texture, window, frame, client);
        let origin_changed = self.choice.feed(self.evidence);
        let mut trim_changed = None;
        if origin_changed && self.trim.applied() != Edges::default() {
            trim_changed = Some(Edges::default());
        }
        if origin_changed {
            self.trim.reset();
        }
        let crop = texture_crop(texture, window, frame, client, self.choice.cut());
        let (w, h) = ((crop.x1 - crop.x0) as usize, (crop.y1 - crop.y0) as usize);
        if w == 0 || h == 0 {
            return Err("the frame doesn't cover the client area".to_string());
        }
        let corner = crop.y0 as usize * pitch + crop.x0 as usize * 4;
        if !origin_changed && let Some(trim) = self.trim.feed(border_edges(&src[corner..], pitch, w, h)) {
            trim_changed = Some(trim);
        }
        let (pw, ph) = crop.size();
        let data = copy_client(src, pitch, texture, crop.start(), (pw, ph), self.trim.applied().shift());
        Ok(Picture { w: pw, h: ph, data, crop, evidence: self.evidence, origin_changed, trim_changed })
    }
}

/// What the watcher's capture session last told the run log of its choice and trim, so `capture_geometry` rows
/// carry what the session actually did: a row on the first frame of each geometry (after the choice is made, not
/// before) and on every new origin; a trim alone only updates what window rows carry ([`with_geometry`]).
#[derive(Clone, Debug, Default)]
pub struct Told {
    said: Option<(bool, Edges)>,
}

/// What a frame asks of the run log, see [`Told::after`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tell {
    Nothing,
    /// Update what window rows carry, no row.
    Quietly,
    /// A `capture_geometry` row.
    Row,
}

impl Told {
    /// The geometry changed: the next frame tells again.
    pub fn reset(&mut self) {
        self.said = None;
    }

    /// After a frame cropped with `cut` and `trim`: what to tell.
    pub fn after(&mut self, cut: bool, trim: Edges) -> Tell {
        let tell = match self.said {
            None => Tell::Row,
            Some((c, _)) if c != cut => Tell::Row,
            Some((_, t)) if t != trim => Tell::Quietly,
            Some(_) => Tell::Nothing,
        };
        self.said = Some((cut, trim));
        tell
    }
}

/// What the capture measured of the game's window, for the run log ([`Geometry::json`]): numbers only, never a title
/// or a path.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Geometry {
    /// GetWindowLong(GWL_STYLE) and (GWL_EXSTYLE).
    pub style: u32,
    pub exstyle: u32,
    /// GetWindowRect.
    pub window: Rect,
    /// DWMWA_EXTENDED_FRAME_BOUNDS.
    pub frame: Rect,
    /// The client area's top-left and bottom-right corners through ClientToScreen.
    pub client: Rect,
    /// The captured texture's D3D11 desc width and height; none before the session's first frame.
    pub texture: Option<(i32, i32)>,
    /// The frame's ContentSize, for comparison only (not used to crop).
    pub content: Option<(i32, i32)>,
    /// MonitorFromWindow (nearest) as a number, and its MONITORINFO: rcMonitor, rcWork, dwFlags (1 = primary).
    pub hmonitor: i64,
    pub monitor: Rect,
    pub work: Rect,
    pub monitor_flags: u32,
    /// GetDpiForWindow.
    pub dpi: u32,
}

impl Geometry {
    /// The fields of a `capture_geometry` row, and of the `capture` field of a `window` row. `crop` is the client
    /// area in the texture as [x0, y0, width, height] ([`texture_crop`]), `crop_frame` what the old rule (the frame
    /// bounds as the origin) gave, `origin` the corner taken ("frame", "window" for a frame-sized texture laid out
    /// from GetWindowRect's corner, "window_full", "unmatched"), `origin_evidence` what the texture's pixels last
    /// said ([`origin_evidence`]: "window", "frame" or null), `margins` the texture's edges to the client's, `pad` the
    /// black added on each side ([`Crop::pad`]), `trim` the border cut by fix 2.
    pub fn json(&self, cropper: &Cropper) -> Value {
        let crop = self.texture.map(|t| texture_crop(t, self.window, self.frame, self.client, cropper.cut()));
        let evidence = cropper.evidence().map(|cut| if cut { "window" } else { "frame" });
        let pair = |p: Option<(i32, i32)>| p.map(|(a, b)| json!([a, b]));
        json!({
            "style": self.style,
            "exstyle": self.exstyle,
            "window_rect": self.window,
            "frame_rect": self.frame,
            "client_tl": [self.client[0], self.client[1]],
            "client_br": [self.client[2], self.client[3]],
            "texture": pair(self.texture),
            "content": pair(self.content),
            "hmonitor": self.hmonitor,
            "monitor": self.monitor,
            "work": self.work,
            "monitor_flags": self.monitor_flags,
            "dpi": self.dpi,
            "origin": crop.map(|c| c.origin.name()),
            "crop": crop.map(|c| json!([c.x0, c.y0, c.x1 - c.x0, c.y1 - c.y0])),
            "crop_frame": [self.client[0] - self.frame[0], self.client[1] - self.frame[1]],
            "margins": crop.map(|c| c.margins),
            "pad": crop.map(|c| c.pad),
            "inside": crop.map(|c| c.inside()),
            "origin_evidence": evidence,
            "trim": cropper.trim().array(),
        })
    }
}

/// Fix 2 only looks this far in from each edge: the invisible border plus the visible line (8 px at 100% on
/// Windows 10) with room for 200%.
pub const TRIM_MAX: usize = 16;
/// A border column or row is black: at most this R+G+B in a pixel (a raw capture gives 0; JPEG saved frames up to
/// 35 on 2026-11-05)...
const BLACK_PIXEL: u32 = 48;
/// ...in at least this share of its samples (per mille)...
const BLACK_SHARE: usize = 990;
/// ...with a mean R+G+B at most this.
const BLACK_MEAN: u32 = 12;
/// The visible border line after the black: a mean R+G+B at least this (75 and 129 in the saved 1080 frames of
/// 2026-11-05, R = G = B), so a merely dark flat column of the game's (under 15 a channel) is not taken for it...
const LINE_MEAN: u32 = 45;
/// ...and at least this share (per mille) of its samples within [`LINE_SPREAD`] of its median: one flat colour.
const LINE_SHARE: usize = 950;
const LINE_SPREAD: u32 = 30;
/// A picture darker than this mean R+G+B (a loading or black screen) is not judged.
const DARK_MEAN: u32 = 45;
/// Samples taken along an edge line, about one every 4 rows of a 1080 picture.
const LINE_SAMPLES: usize = 270;
/// Frames in a row that must show the same border before the trim changes.
pub const TRIM_FRAMES: u8 = 3;

/// Pixels cut from each side of the picture by fix 2: left, top, right, bottom.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges {
    pub left: usize,
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
}

impl Edges {
    pub fn array(self) -> [usize; 4] {
        [self.left, self.top, self.right, self.bottom]
    }

    /// How far the source is moved to put the client's content back at its place: right by a left border, left by a
    /// right one; likewise down and up.
    pub fn shift(self) -> (i32, i32) {
        (self.left as i32 - self.right as i32, self.top as i32 - self.bottom as i32)
    }
}

/// R+G+B of the 4-byte pixel at `i` (BGRA or RGBA alike).
fn sum_at(px: &[u8], i: usize) -> u32 {
    u32::from(px[i]) + u32::from(px[i + 1]) + u32::from(px[i + 2])
}

/// R+G+B samples along line `k` in from `side` (0 left, 1 top, 2 right, 3 bottom) of a `w` x `h` picture of 4-byte
/// pixels, `pitch` bytes a row.
fn edge_line(px: &[u8], pitch: usize, w: usize, h: usize, side: usize, k: usize) -> Vec<u32> {
    let along = if side.is_multiple_of(2) { h } else { w };
    let step = along.div_ceil(LINE_SAMPLES).max(1);
    (0..along)
        .step_by(step)
        .map(|t| {
            let (x, y) = match side {
                0 => (k, t),
                1 => (t, k),
                2 => (w - 1 - k, t),
                _ => (t, h - 1 - k),
            };
            sum_at(px, y * pitch + x * 4)
        })
        .collect()
}

fn is_black(line: &[u32]) -> bool {
    let n = line.len().max(1);
    let dark = line.iter().filter(|&&s| s <= BLACK_PIXEL).count();
    dark * 1000 >= n * BLACK_SHARE && line.iter().sum::<u32>() <= BLACK_MEAN * n as u32
}

/// Whether at least [`LINE_SHARE`] of `line`'s samples lie within [`LINE_SPREAD`] of `colour`.
fn mostly_near(line: &[u32], colour: u32) -> bool {
    let near = line.iter().filter(|&&s| s.abs_diff(colour) <= LINE_SPREAD).count();
    near * 1000 >= line.len().max(1) * LINE_SHARE
}

/// The median R+G+B of `line` when it is a border line: bright enough ([`LINE_MEAN`]) and one flat colour.
fn border_line(line: &[u32]) -> Option<u32> {
    let mut sorted = line.to_vec();
    sorted.sort_unstable();
    let median = *sorted.get(sorted.len() / 2)?;
    (line.iter().sum::<u32>() >= LINE_MEAN * line.len() as u32 && mostly_near(line, median)).then_some(median)
}

/// The border on one side: black lines from the edge, then one flat line exactly 1 px wide, then a line that is
/// neither black nor the line's flat colour again: Some(its width). Some(0) when the edge line is not black (no
/// border); None when black lines are there but the rest is not as above (the game's own black edge, a flat band of
/// the game's own after them, or a border whose next line the game happens to paint the line's colour: 16 of the 562
/// saved 1080 frames on 2026-11-06), which can tell neither way. At least one black line is needed: a flat line alone
/// is too easily the game's own, as is a flat band. NOT distinguishable by design: black lines followed by a 1 px
/// flat line of the game's own at least [`LINE_MEAN`] bright; only [`TRIM_FRAMES`] and the reset on a geometry change
/// guard against that. NOT VERIFIED: at a high DPI (for example 200%) the visible border may be 2 px wide; this rule
/// then answers None on that side for every frame, so fix 2 never trims there and only fix 1 aligns the picture.
fn side_border(px: &[u8], pitch: usize, w: usize, h: usize, side: usize) -> Option<usize> {
    let line = |k| edge_line(px, pitch, w, h, side, k);
    let black = (0..TRIM_MAX).take_while(|&k| is_black(&line(k))).count();
    if black == 0 {
        return Some(0);
    }
    if black + 1 > TRIM_MAX {
        return None;
    }
    let colour = border_line(&line(black))?;
    let after = line(black + 1);
    (!is_black(&after) && !mostly_near(&after, colour)).then_some(black + 1)
}

/// Whether a `w` x `h` picture (4-byte pixels, `pitch` bytes a row) is too dark to judge: a mean R+G+B under
/// [`DARK_MEAN`] over a 64 x 36 grid of samples (a loading or black screen).
fn too_dark(px: &[u8], pitch: usize, w: usize, h: usize) -> bool {
    let (sx, sy) = (w.div_ceil(64), h.div_ceil(36));
    let (mut total, mut n) = (0u64, 0u64);
    for y in (0..h).step_by(sy) {
        for x in (0..w).step_by(sx) {
            total += u64::from(sum_at(px, y * pitch + x * 4));
            n += 1;
        }
    }
    total < u64::from(DARK_MEAN) * n
}

/// What fix 2 saw on each side of one picture (left, top, right, bottom): Some(width) of a border, Some(0) for no
/// border, None when that side can tell neither way (see [`side_border`]).
pub type Sides = [Option<usize>; 4];

/// Fix 2's check of one picture (`w` x `h` 4-byte pixels, `pitch` bytes a row, BGRA or RGBA): what each side shows,
/// see [`side_border`]; none at all when the picture is too dark to judge ([`DARK_MEAN`]) or too small. A border on
/// two opposite sides is not a shifted capture: both are taken as telling neither way.
pub fn border_edges(px: &[u8], pitch: usize, w: usize, h: usize) -> Option<Sides> {
    if w < 8 * TRIM_MAX || h < 8 * TRIM_MAX || too_dark(px, pitch, w, h) {
        return None;
    }
    let mut sides = [0, 1, 2, 3].map(|side| side_border(px, pitch, w, h, side));
    for (a, b) in [(0, 2), (1, 3)] {
        if sides[a].is_some_and(|n| n > 0) && sides[b].is_some_and(|n| n > 0) {
            (sides[a], sides[b]) = (None, None);
        }
    }
    Some(sides)
}

/// Fix 2's trim over time: a border is cut only after [`TRIM_FRAMES`] frames in a row show the same one, and
/// dropped the same way (frames showing no border). A side that tells neither way counts as the trim already cut
/// there, so it neither starts nor drops a trim on that side; a picture that cannot be judged at all breaks the run.
/// [`Trim::reset`] when the window's geometry changes.
#[derive(Clone, Debug, Default)]
pub struct Trim {
    applied: Edges,
    pending: Option<(Edges, u8)>,
}

impl Trim {
    /// The border cut now.
    pub fn applied(&self) -> Edges {
        self.applied
    }

    /// Takes one frame's check ([`border_edges`]); the new trim when it changes.
    pub fn feed(&mut self, seen: Option<Sides>) -> Option<Edges> {
        let Some(sides) = seen else {
            self.pending = None;
            return None;
        };
        let applied = self.applied.array();
        let [left, top, right, bottom] = [0, 1, 2, 3].map(|i| sides[i].unwrap_or(applied[i]));
        let seen = Edges { left, top, right, bottom };
        if seen == self.applied {
            self.pending = None;
            return None;
        }
        let n = match self.pending {
            Some((e, n)) if e == seen => n + 1,
            _ => 1,
        };
        if n >= TRIM_FRAMES {
            self.applied = seen;
            self.pending = None;
            Some(seen)
        } else {
            self.pending = Some((seen, n));
            None
        }
    }

    pub fn reset(&mut self) {
        *self = Trim::default();
    }
}

/// The `w` x `h` client picture as RGBA from a BGRA texture (`pitch` bytes a row, `tw` x `th` pixels) whose client
/// area starts at (`x0`, `y0`), its source moved by `shift` (fix 2, see [`Edges::shift`]); pixels the moved source
/// does not cover are black. With no shift this is the plain copy of the client area.
pub fn copy_client(src: &[u8], pitch: usize, (tw, th): (i32, i32), (x0, y0): (i32, i32), (w, h): (usize, usize), shift: (i32, i32)) -> Vec<u8> {
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    let mut data = Vec::with_capacity(w * h * 4);
    let (sx0, sy0) = (x0 + shift.0, y0 + shift.1);
    // The columns of the output the texture covers.
    let from = (-sx0).clamp(0, w as i32) as usize;
    let to = (tw - sx0).clamp(from as i32, w as i32) as usize;
    for y in 0..h as i32 {
        let sy = sy0 + y;
        let (from, to) = if sy < 0 || sy >= th { (w, w) } else { (from, to) };
        for _ in 0..from {
            data.extend_from_slice(&BLACK);
        }
        if from < to {
            let start = sy as usize * pitch + (sx0 + from as i32) as usize * 4;
            for px in src[start..start + (to - from) * 4].as_chunks::<4>().0 {
                data.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        for _ in to..w {
            data.extend_from_slice(&BLACK);
        }
    }
    data
}

/// What the watcher's session last measured and chose, as [`Geometry::json`]; added to `window` rows ([`with_geometry`]).
static MEASURED: Mutex<Option<Value>> = Mutex::new(None);
/// Rows the capture asks the run log for (when asked, kind, fields), taken by [`take_notes`]; at most [`NOTES_MAX`].
static NOTES: Mutex<Vec<(SystemTime, &'static str, Value)>> = Mutex::new(Vec::new());
const NOTES_MAX: usize = 64;
/// Whether [`NOTES`] may hold rows, so the run log's every line costs no lock.
static NOTED: AtomicBool = AtomicBool::new(false);
/// The last trim noted by the watcher's session, so each change is noted once.
static LAST_TRIM: Mutex<Option<Edges>> = Mutex::new(None);
/// The generation of the watcher whose session may publish ([`publish`], [`publish_trim`]); 0 for none. Set by
/// [`Watcher::start`] and cleared by its drop, so a watcher thread left running after its drop timed out (see
/// [`join_by`]) writes nothing global when it wakes up, and a newer watcher's rows are never mixed with an older
/// one's. Held while publishing, so once a newer watcher has started no older session's write can still land.
static TELLER: Mutex<u64> = Mutex::new(0);
/// The last watcher generation handed out; generations start at 1.
static GENERATIONS: AtomicU64 = AtomicU64::new(0);

fn teller() -> MutexGuard<'static, u64> {
    TELLER.lock().unwrap_or_else(PoisonError::into_inner)
}

fn note(kind: &'static str, fields: Value) {
    let mut notes = NOTES.lock().unwrap_or_else(PoisonError::into_inner);
    if notes.len() < NOTES_MAX {
        notes.push((SystemTime::now(), kind, fields));
        NOTED.store(true, Ordering::Release);
    }
}

/// Publishes what the watcher's session measured and chose (only it calls this, see [`Told`]): kept for `window`
/// rows, and with `row` noted as a `capture_geometry` row ([`Told`] says when); a texture of neither rectangle's
/// size is noted as a `capture_warning` row too. Only while `generation` is the [`TELLER`].
fn publish(geometry: &Geometry, cropper: &Cropper, row: bool, generation: u64) {
    let teller = teller();
    if *teller != generation {
        return;
    }
    let value = geometry.json(cropper);
    *MEASURED.lock().unwrap_or_else(PoisonError::into_inner) = Some(value.clone());
    if !row {
        return;
    }
    if value["origin"] == "unmatched" {
        note(
            "capture_warning",
            json!({ "why": "texture size matches neither GetWindowRect nor DWMWA_EXTENDED_FRAME_BOUNDS; frame bounds taken as the origin", "texture": value["texture"], "window_rect": value["window_rect"], "frame_rect": value["frame_rect"] }),
        );
    }
    note("capture_geometry", value);
}

/// Notes a trim fix 2 started or stopped cutting as a `capture_trim` row, once per change. Only while `generation`
/// is the [`TELLER`].
fn publish_trim(trim: Edges, crop: Option<&Crop>, generation: u64) {
    let teller = teller();
    if *teller != generation {
        return;
    }
    let mut last = LAST_TRIM.lock().unwrap_or_else(PoisonError::into_inner);
    if *last == Some(trim) || (last.is_none() && trim == Edges::default()) {
        return;
    }
    *last = Some(trim);
    drop(last);
    note(
        "capture_trim",
        json!({
            "trim": trim.array(),
            "origin": crop.map(|c| c.origin.name()),
            "crop": crop.map(|c| [c.x0, c.y0, c.x1 - c.x0, c.y1 - c.y0]),
        }),
    );
}

/// `fields` (a `window` row's) with the last geometry the capture measured as `capture`, when there is one and the
/// row has none yet.
pub fn with_geometry(mut fields: Value) -> Value {
    if let (Value::Object(map), Some(measured)) = (&mut fields, MEASURED.lock().unwrap_or_else(PoisonError::into_inner).clone()) {
        map.entry("capture").or_insert(measured);
    }
    fields
}

/// The rows the capture asked for since the last call (when asked, kind, fields), oldest first.
pub fn take_notes() -> Vec<(SystemTime, &'static str, Value)> {
    if !NOTED.swap(false, Ordering::AcqRel) {
        return Vec::new();
    }
    std::mem::take(&mut *NOTES.lock().unwrap_or_else(PoisonError::into_inner))
}

/// The window's GetWindowRect and DWMWA_EXTENDED_FRAME_BOUNDS.
fn window_rects(hwnd: HWND) -> Result<(Rect, Rect), String> {
    let (mut window, mut frame) = (RECT::default(), RECT::default());
    // SAFETY: plain Win32 calls on a window handle from `find`, with valid out pointers.
    unsafe {
        GetWindowRect(hwnd, &mut window).map_err(fail("GetWindowRect"))?;
        DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, (&mut frame as *mut RECT).cast(), size_of::<RECT>() as u32)
            .map_err(fail("DwmGetWindowAttribute"))?;
    }
    let r = |r: RECT| [r.left, r.top, r.right, r.bottom];
    Ok((r(window), r(frame)))
}

/// Everything [`Geometry`] holds besides the rectangles and sizes: styles, monitor and DPI. Failures leave zeros.
fn window_facts(hwnd: HWND, geometry: &mut Geometry) {
    // SAFETY: plain Win32 calls on a window handle from `find`, with valid out pointers.
    unsafe {
        geometry.style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        geometry.exstyle = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        geometry.dpi = GetDpiForWindow(hwnd);
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        geometry.hmonitor = monitor.0 as i64;
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let r = |r: RECT| [r.left, r.top, r.right, r.bottom];
            (geometry.monitor, geometry.work, geometry.monitor_flags) = (r(info.rcMonitor), r(info.rcWork), info.dwFlags);
        }
    }
}

/// A Windows Graphics Capture session on one window, kept open between frames.
struct Stream {
    hwnd: HWND,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    /// The frame size the pool was made for.
    size: SizeInt32,
    /// The texture frames are copied to for the CPU, reused while the size holds.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// The window's client size at the last grab (physical pixels); None before the first grab.
    client: Option<(i32, i32)>,
    /// The geometry measured last, see [`Stream::measure`].
    geometry: Geometry,
    /// Fix 1's origin and fix 2's border trim, see [`Cropper`].
    cropper: Cropper,
    /// Only the watcher's session tells the run log (capture_geometry and capture_trim rows, what window rows
    /// carry): observe mode's and single-frame captures' sessions judge for themselves and say nothing. See [`Told`].
    told: Option<Told>,
    /// The watcher generation the telling session publishes as (see [`TELLER`]); 0 for the others.
    generation: u64,
}

impl Stream {
    fn open(hwnd: HWND) -> Result<Stream, String> {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err("not supported on this system".to_string());
        }
        // A minimized window gives a tiny stale texture (160 x 28 on 2026-11-06) that holds none of the client: no
        // session until it is restored (the callers retry after RETRY).
        // SAFETY: a plain Win32 call on a window handle from `find`.
        if unsafe { IsIconic(hwnd) }.as_bool() {
            return Err(tr("The window is minimized.").to_string());
        }
        // SAFETY: COM and Direct3D calls on objects created here.
        unsafe {
            let (mut device, mut context): (Option<ID3D11Device>, Option<ID3D11DeviceContext>) = (None, None);
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(fail("D3D11CreateDevice"))?;
            let (device, context) = (device.ok_or("no Direct3D device")?, context.ok_or("no Direct3D context")?);
            let dxgi: IDXGIDevice = device.cast().map_err(fail("IDXGIDevice"))?;
            let winrt_device: IDirect3DDevice =
                CreateDirect3D11DeviceFromDXGIDevice(&dxgi).and_then(|d| d.cast()).map_err(fail("CreateDirect3D11DeviceFromDXGIDevice"))?;

            let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(fail("capture interop"))?;
            let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd).map_err(fail("CreateForWindow"))?;
            let size = item.Size().map_err(fail("item size"))?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt_device, FORMAT, BUFFERS, size).map_err(fail("CreateFreeThreaded"))?;
            let session = pool.CreateCaptureSession(&item).map_err(fail("CreateCaptureSession"))?;
            // Windows 11 only; older systems keep the border and the cursor.
            let _ = session.SetIsBorderRequired(false);
            let _ = session.SetIsCursorCaptureEnabled(false);
            session.StartCapture().map_err(fail("StartCapture"))?;
            Ok(Stream {
                hwnd,
                device,
                context,
                winrt_device,
                _item: item,
                pool,
                session,
                size,
                staging: None,
                client: None,
                geometry: Geometry::default(),
                cropper: Cropper::default(),
                told: None,
                generation: 0,
            })
        }
    }

    /// This session as the watcher's, the one that tells the run log (see [`Stream::told`]), as long as `generation`
    /// is the [`TELLER`].
    fn telling(mut self, generation: u64) -> Stream {
        self.told = Some(Told::default());
        self.generation = generation;
        self
    }

    /// The newest frame since the last call, cropped to the client area;
    /// none when no frame came. Fails when the window is gone or minimized.
    fn grab(&mut self) -> Result<Option<Rgba>, String> {
        let client = client_rect(self.hwnd)?;
        self.client = Some((client.right - client.left, client.bottom - client.top));
        let client = [client.left, client.top, client.right, client.bottom];
        let rects = window_rects(self.hwnd);
        let mut newest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Some(old) = newest.replace(frame) {
                let _ = old.Close();
            }
        }
        let Some(frame) = newest else {
            // No frame yet (a new session) or nothing new: the rectangles are still measured, so a window row
            // written now carries them. A failed query here changes nothing and fails nothing.
            if let Ok((window, frame_rect)) = rects {
                let (texture, content) = (self.geometry.texture, self.geometry.content);
                self.measure(window, frame_rect, client, texture, content);
            }
            return Ok(None);
        };
        // A failed query keeps the rectangles measured last; with none measured yet it fails the grab, as the frame
        // bounds' query did before 2026-11-06.
        let (window, frame_rect) = match rects {
            Ok(rects) => rects,
            Err(_) if self.geometry != Geometry::default() => (self.geometry.window, self.geometry.frame),
            Err(e) => {
                let _ = frame.Close();
                return Err(e);
            }
        };
        let size = frame.ContentSize();
        let content = size.as_ref().ok().map(|s| (s.Width, s.Height));
        let result = self.copy(&frame, window, frame_rect, client, content);
        let _ = frame.Close();
        let size = size.map_err(fail("frame size"))?;
        if (size.Width, size.Height) != (self.size.Width, self.size.Height) {
            self.pool.Recreate(&self.winrt_device, FORMAT, BUFFERS, size).map_err(fail("Recreate"))?;
            self.size = size;
        }
        result.map(Some)
    }

    /// Records the window's geometry ([`Geometry`]) when its rectangles or the texture size changed since the last
    /// grab: the styles, monitor and DPI are read only then, and fix 1's choice and fix 2's trim start over (the old
    /// rule, no trim). The watcher's session updates what window rows carry at once; its `capture_geometry` row
    /// waits for the next frame's choice ([`Told`]). The bottom-right client corner is ClientToScreen's top-left
    /// plus GetClientRect's size, which is ClientToScreen of that corner for any window not mirrored right to left.
    fn measure(&mut self, window: Rect, frame: Rect, client: Rect, texture: Option<(i32, i32)>, content: Option<(i32, i32)>) {
        let g = &self.geometry;
        if (g.window, g.frame, g.client, g.texture) == (window, frame, client, texture) {
            return;
        }
        self.geometry = Geometry { window, frame, client, texture, content, ..Geometry::default() };
        window_facts(self.hwnd, &mut self.geometry);
        let trimmed = self.cropper.trim() != Edges::default();
        self.cropper.reset();
        if let Some(told) = &mut self.told {
            if trimmed {
                publish_trim(Edges::default(), None, self.generation);
            }
            told.reset();
            publish(&self.geometry, &self.cropper, false, self.generation);
        }
    }

    /// The frame's pixels under `client` (screen coordinates in physical pixels), given the window's GetWindowRect
    /// `window` and frame bounds `frame_rect` (see [`texture_crop`]) and the frame's ContentSize (logged only).
    fn copy(&mut self, frame: &Direct3D11CaptureFrame, window: Rect, frame_rect: Rect, client: Rect, content: Option<(i32, i32)>) -> Result<Rgba, String> {
        // SAFETY: COM and Direct3D calls on objects owned here; the mapped pointer is
        // read within the staging texture's row pitch and height, then unmapped.
        unsafe {
            let texture: ID3D11Texture2D = frame
                .Surface()
                .and_then(|s| s.cast::<IDirect3DDxgiInterfaceAccess>())
                .and_then(|a| a.GetInterface())
                .map_err(fail("frame texture"))?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            let (fw, fh) = (desc.Width as i32, desc.Height as i32);
            self.measure(window, frame_rect, client, Some((fw, fh)), content);
            let staging = match &self.staging {
                Some((t, w, h)) if (*w, *h) == (desc.Width, desc.Height) => t.clone(),
                _ => {
                    let (w, h) = (desc.Width, desc.Height);
                    desc.Usage = D3D11_USAGE_STAGING;
                    desc.BindFlags = 0;
                    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                    desc.MiscFlags = 0;
                    let mut staging = None;
                    self.device.CreateTexture2D(&desc, None, Some(&mut staging)).map_err(fail("staging texture"))?;
                    let staging = staging.ok_or("no staging texture")?;
                    self.staging = Some((staging.clone(), w, h));
                    staging
                }
            };
            self.context.CopyResource(&staging, &texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(fail("Map"))?;
            // Fix 1 (the origin, chosen by the texture's own pixels) and fix 2 (the border check on its crop), see
            // Cropper::picture. A client area not wholly inside the texture (the `inside` of the logged geometry is
            // false) is still cropped as far as it goes, as before: failing the grab would drop the session for a
            // second.
            let pitch = mapped.RowPitch as usize;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * fh as usize);
            let picture = self.cropper.picture(src, pitch, (fw, fh), window, frame_rect, client);
            self.context.Unmap(&staging, 0);
            let picture = picture?;
            if let Some(told) = &mut self.told {
                if let Some(trim) = picture.trim_changed {
                    publish_trim(trim, Some(&picture.crop), self.generation);
                }
                match told.after(self.cropper.cut(), self.cropper.trim()) {
                    Tell::Row => publish(&self.geometry, &self.cropper, true, self.generation),
                    Tell::Quietly => publish(&self.geometry, &self.cropper, false, self.generation),
                    Tell::Nothing => {}
                }
            }
            Ok(Rgba { w: picture.w, h: picture.h, data: picture.data })
        }
    }

    /// Waits for the next frame, up to [`FRAME_TIMEOUT`].
    fn next_frame(&mut self) -> Result<Rgba, String> {
        let start = Instant::now();
        loop {
            if let Some(img) = self.grab()? {
                return Ok(img);
            }
            if start.elapsed() > FRAME_TIMEOUT {
                return Err("no frame within 1 s".to_string());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Grabs `n` frames of the window through one capture session, as the
/// watcher does, waiting 50 ms between them: the last frame and how long
/// each grab and fingerprint took (the first includes opening the session).
pub fn time_frames(title: &str, n: usize) -> Result<(Rgba, Vec<Duration>), String> {
    let hwnd = find_window(title)?;
    dpi_aware(|| {
        let mut start = Instant::now();
        let mut stream = Stream::open(hwnd)?;
        let mut times = Vec::new();
        loop {
            let frame = stream.next_frame()?;
            fingerprint(&frame);
            times.push(start.elapsed());
            if times.len() >= n.max(1) {
                return Ok((frame, times));
            }
            std::thread::sleep(Duration::from_millis(50));
            start = Instant::now();
        }
    })
}

/// What the watcher reports.
pub enum Event {
    /// A settled new screen to read, and why it was read.
    Frame(Rgba, Option<Trigger>),
    /// A new screen that may settle as it is, to read ahead; see [`Watch::take_ahead`].
    Ahead(Rgba),
    /// Why the watcher can't see the game, or none once it can again.
    Status(Option<String>),
    /// The window a new capture session opened on, as its title and client size.
    Window(String),
    /// The game's log says a Warden's Treasure's gift was confirmed.
    Treasure,
    /// The game's log says day `cur` of `max` began.
    Day { cur: u32, max: u32 },
    /// Observe mode's frame of the game as captured then, shrunk and as JPEG.
    Observe(Vec<u8>, SystemTime),
    /// What else the game's log says happened in the run: an objective
    /// completed, a Consume, a bite, a run's start or end; with its line's
    /// time (UTC ms) and when it was read off the log, to see how late it
    /// comes.
    Game(Signal, Option<i64>, SystemTime),
}

/// The watcher's state the UI thread reaches too.
struct Shared {
    watch: Watch,
    /// The newest sampled frame and its fingerprint.
    latest: Option<(Rgba, Vec<u8>)>,
    /// When to read whatever the screen shows: a theme scroll the game's log
    /// says opened, read before it settles (see [`THEME_READS`]).
    reads_at: Vec<Instant>,
    /// Until when a gift confirmed leaves the board between a treasure's
    /// picks, see [`gift_reads`].
    gift_gap_until: Option<Instant>,
    /// Accumulated progress of Vorax and Tartarus ([`Signal::Charges`] from the log), see [`Watcher::charges`].
    charges: gamelog::Charges,
    /// What the log follower is doing, see [`Watcher::log_status`].
    log: gamelog::LogStatus,
}

/// How often observe mode keeps a frame: about 25 a second, as the reads'
/// own sampling gave only about 6 and a fault between them went unseen.
const OBSERVE_EVERY: Duration = Duration::from_millis(40);
/// Observe mode's frames: this wide (a 2560 x 1440 frame to 1280 x 720) at
/// this JPEG quality, about 75 KB each.
const OBSERVE_WIDTH: usize = 1280;
const OBSERVE_QUALITY: u8 = 75;

/// After the game's log says the theme scroll opened, when to read it: it
/// slides in for about 0.3 s, and a player who knows the theme confirms it
/// within a second (0.9 s on 2026-10-28).
const THEME_READS: [Duration; 3] = [Duration::from_millis(350), Duration::from_millis(600), Duration::from_millis(850)];

/// After the game's log says a gift was confirmed, when to read the board
/// between the treasure's picks: it showed from about 0.4 s after the click
/// and for as little as 0.3 s (Brass Horn of Strife went unread), and the
/// log comes 30 to 130 ms after it (2026-10-28). A read takes about 0.17 s,
/// so reads due meanwhile run back to back.
const GIFT_READS: [Duration; 6] = [
    Duration::from_millis(200),
    Duration::from_millis(300),
    Duration::from_millis(400),
    Duration::from_millis(500),
    Duration::from_millis(650),
    Duration::from_millis(800),
];

/// After the game's log says the player clicked on the board, when to read
/// it whatever it shows: an eater's change after a bite on a dark board woke
/// no read for 3.4 s, and its chain's advice stayed on the bite taken
/// (2026-10-28).
const CLICK_READS: [Duration; 4] = [Duration::from_millis(400), Duration::from_millis(700), Duration::from_millis(1000), Duration::from_millis(1500)];

/// When to read after the decision cards come up: once the board under them
/// reads right (0.1 s, see `Session::cards_shown`), once the left card's art
/// shows its colour (0.25 s), and just after the fade (see
/// [`super::session::CARDS_IN`]).
const CARDS_READS: [Duration; 3] = [Duration::from_millis(120), Duration::from_millis(260), Duration::from_millis(650)];

/// When to read after the log tells of a new day: it writes the day as the
/// banner goes and the doors light up, and the door cards read from about
/// 0.2 s on (2026-10-30: reads begun 0.10 to 0.28 s on read no screen, and
/// one at +0.05 s held the next read back; see [`super::session::DOORS_SHOW`]).
const DOOR_READS: [Duration; 2] = [super::session::DOORS_SHOW, Duration::from_millis(320)];

/// When to read after the log tells of a Consume pressed and of a bite: as
/// the target screen and the bitten board show (see
/// [`super::session::CONSUME_SHOWS`] and [`super::session::BITE_READABLE`]),
/// and once more a little after.
const CONSUME_READS: [Duration; 2] = [super::session::CONSUME_SHOWS, Duration::from_millis(320)];
const BITE_READS: [Duration; 2] = [super::session::BITE_READABLE, Duration::from_millis(490)];

/// Vorax: when to read after the log says the new hand has been dealt or the card animation has started. The tank
/// monsters keep moving, so the picture may stay unsteady and Watch never judges it steady (plan section 3.3).
const VORAX_READS: [Duration; 4] = [Duration::from_millis(120), Duration::from_millis(250), Duration::from_millis(600), Duration::from_millis(1000)];
/// Vorax observe mode: this long after a resolve cue (Tool, Gone, Changed) one read is scheduled to save the tank
/// label strip (plan section 6.3).
const VORAX_STRIP: Duration = Duration::from_millis(150);

/// When [`Watcher::read_soon`] reads: an eater's change after its bite takes
/// 1 to 3 s to show, and stayed 4 s unread on a dark board (2026-10-28).
const RECHECKS: [Duration; 5] =
    [Duration::from_millis(300), Duration::from_millis(700), Duration::from_millis(1200), Duration::from_millis(2000), Duration::from_millis(3000)];

/// Auto read: a thread that keeps a capture session on the game open, samples
/// it every [`SAMPLE`] and sends a frame when [`Watch`] says to read it.
/// Dropping it stops the thread.
pub struct Watcher {
    /// Dropped to stop the thread.
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
    /// The thread that looks for and follows the game's log (see [`gamelog`]), and its stop flag.
    log: Option<(JoinHandle<()>, Arc<AtomicBool>)>,
    events: Receiver<Event>,
    shared: Arc<Mutex<Shared>>,
    /// What observe mode needs to start (see [`Watcher::observe`]): the
    /// window's title, where events go, and the wake-up.
    title: String,
    tx: Sender<Event>,
    notify: Arc<dyn Fn() + Send + Sync>,
    observer: Mutex<Option<Observer>>,
    /// This watcher's generation, see [`TELLER`].
    generation: u64,
}

/// How long dropping a [`Watcher`] (or an [`Observer`]) waits for its threads in all before leaving them to end on
/// their own: the UI thread drops it, and must never wait on a thread that may be waiting on the UI thread.
const STOP_WAIT: Duration = Duration::from_secs(2);

/// Joins `thread` if it ends by `deadline`, else drops its handle and leaves it to end on its own (detached); whether
/// it was joined. A thread left running finds its stop signal when it wakes, and its watcher's generation retired.
fn join_by(thread: Option<JoinHandle<()>>, deadline: Instant) -> bool {
    let Some(thread) = thread else { return true };
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = thread.join();
    true
}

impl Watcher {
    /// Watches the window `find` picks for `title`; `notify` runs after each event. `game_log` is the log file
    /// chosen in the settings, None to look for it ([`gamelog::locate`]): until it is found it is looked for again
    /// every [`gamelog::LOG_RETRY`], so a game started after Ascendant is still followed.
    pub fn start(title: &str, game_log: Option<std::path::PathBuf>, notify: impl Fn() + Send + Sync + 'static) -> Watcher {
        let generation = GENERATIONS.fetch_add(1, Ordering::Relaxed) + 1;
        *teller() = generation;
        // Set by the drop: a thread still running after it wakes the UI no more.
        let done = Arc::new(AtomicBool::new(false));
        let notify: Arc<dyn Fn() + Send + Sync> = {
            let done = done.clone();
            Arc::new(move || {
                if !done.load(Ordering::Relaxed) {
                    notify();
                }
            })
        };
        let (stop, stopped) = channel();
        let (tx, events) = channel();
        let shared = Arc::new(Mutex::new(Shared {
            watch: Watch::default(),
            latest: None,
            reads_at: Vec::new(),
            gift_gap_until: None,
            charges: gamelog::Charges::default(),
            log: gamelog::LogStatus::Searching,
        }));
        let log = {
            let (state, tx, notify) = (shared.clone(), tx.clone(), notify.clone());
            let flag = done.clone();
            let (status_state, status_notify, title) = (shared.clone(), notify.clone(), title.to_string());
            let click_title = title.clone();
            let mut on = move |signal: Signal, stamp: Option<i64>| {
                on_log_signal(signal, stamp, &state, &tx, &*notify, || tank_under_cursor(&click_title));
            };
            let set_status = move |status: gamelog::LogStatus| set_log_status(&status_state, &*status_notify, status);
            let thread = std::thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    let game = game_process(&title);
                    match gamelog::locate(game_log.as_deref(), game.as_ref().map(|g| g.0.as_path())) {
                        Ok((path, source)) => {
                            let modified = || std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                            let started = game.map(|g| g.1.unwrap_or(SystemTime::UNIX_EPOCH));
                            set_status(gamelog::follow_status(&path, &source, started, Duration::ZERO, modified(), SystemTime::now()));
                            // Re-checks the game and the log's freshness every LOG_CHECK while following.
                            let mut checked = Instant::now();
                            let tick = |since_new: Duration| {
                                // Stopping: no window search (which the drop would wait for) and no status.
                                if flag.load(Ordering::Relaxed) || checked.elapsed() < LOG_CHECK {
                                    return;
                                }
                                checked = Instant::now();
                                let started = game_process(&title).map(|g| g.1.unwrap_or(SystemTime::UNIX_EPOCH));
                                set_status(gamelog::follow_status(&path, &source, started, since_new, modified(), SystemTime::now()));
                            };
                            gamelog::follow(&path, &flag, &mut on, tick);
                        }
                        Err(status) => {
                            set_status(status);
                            let until = Instant::now() + gamelog::LOG_RETRY;
                            while Instant::now() < until && !flag.load(Ordering::Relaxed) {
                                std::thread::sleep(Duration::from_millis(100));
                            }
                        }
                    }
                }
            });
            Some((thread, done))
        };
        let (observe_tx, observe_notify) = (tx.clone(), notify.clone());
        let (name, state) = (title.to_string(), shared.clone());
        let thread = std::thread::spawn(move || dpi_aware(|| watch(&name, generation, &stopped, &tx, &state, &*notify)));
        Watcher {
            stop: Some(stop),
            thread: Some(thread),
            log,
            events,
            shared,
            title: title.to_string(),
            tx: observe_tx,
            notify: observe_notify,
            observer: Mutex::new(None),
            generation,
        }
    }

    /// Accumulated progress of Vorax and Tartarus (the "37/48" under the icons in the game): the value found by
    /// looking back through the log when watching starts, then updated on every change; None if not seen yet. For
    /// the state (nearly full, full) see [`gamelog::fill`].
    pub fn charges(&self) -> gamelog::Charges {
        lock(&self.shared).charges
    }

    /// What the game log follower is doing: looking for the log, the chosen file missing, or which file it follows.
    pub fn log_status(&self) -> gamelog::LogStatus {
        lock(&self.shared).log.clone()
    }

    /// The game log being followed, if any (see [`Watcher::log_status`]).
    pub fn log_path(&self) -> Option<std::path::PathBuf> {
        match lock(&self.shared).log {
            gamelog::LogStatus::Following { ref path, .. } => Some(path.clone()),
            _ => None,
        }
    }

    /// The next event, if any.
    pub fn event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Turns observe mode on or off: about 25 frames a second go to the UI
    /// as [`Event::Observe`], from a capture of their own (see [`Observer`]).
    pub fn observe(&self, on: bool) {
        let mut observer = self.observer.lock().unwrap_or_else(PoisonError::into_inner);
        if on && observer.is_none() {
            *observer = Some(Observer::start(self.title.clone(), self.tx.clone(), self.notify.clone(), self.shared.clone()));
        } else if !on {
            observer.take();
        }
    }

    /// The newest sampled frame, counted as read so auto read leaves it.
    pub fn read_latest(&self) -> Option<Rgba> {
        let mut shared = lock(&self.shared);
        let (frame, print) = shared.latest.clone()?;
        shared.watch.mark_read(print);
        Some(frame)
    }

    /// Counts the next sample as read: an opened screenshot stays on show
    /// until the game's screen changes.
    pub fn adopt_next(&self) {
        lock(&self.shared).watch.adopt_next();
    }

    /// Reads the next sample whatever it shows: a held read asks for one more.
    pub fn read_next(&self) {
        lock(&self.shared).watch.read_next();
    }

    /// Reads the screen at each of [`RECHECKS`] from now whatever it shows:
    /// a change too faint to wake auto read is due (an eater still changing
    /// after its bite on a dark board). A read with nothing new asks for no
    /// more, so the reads are all set at once.
    pub fn read_soon(&self) {
        let now = Instant::now();
        let mut shared = lock(&self.shared);
        // Reads still due cover the change: a solve of each read asks again.
        if shared.reads_at.is_empty() {
            shared.reads_at = RECHECKS.iter().map(|&after| now + after).collect();
        }
    }

    /// Reads the screen once `after` from now whatever it shows: a held read
    /// waits for time, not for a change (see [`super::session::Session::wants_read_in`]).
    pub fn read_in(&self, after: Duration) {
        let mut shared = lock(&self.shared);
        shared.reads_at.push(Instant::now() + after);
        shared.reads_at.sort();
    }

    /// Tells auto read whether the last read showed anything new, and was off the board; see [`Watch::report`].
    pub fn report(&self, news: bool, off_board: bool) {
        lock(&self.shared).watch.report(news, off_board);
    }
}

/// Stops the threads, waiting [`STOP_WAIT`] at most in all (the UI thread drops a watcher when auto read is turned off
/// and when Ascendant closes). First this watcher's generation is retired ([`TELLER`], unless a newer watcher already
/// took over), and every thread is told to stop; then each is joined by one deadline. A thread still running then is
/// left to end on its own: it publishes nothing global (its generation is retired), its events go nowhere (the
/// receiver is dropped with the watcher) and it wakes the UI no more (`done`). It ends at its next look at its stop
/// signal, closing its own capture session and device; a new watcher meanwhile has a session and device of its own.
impl Drop for Watcher {
    fn drop(&mut self) {
        {
            let mut teller = teller();
            if *teller == self.generation {
                *teller = 0;
            }
        }
        self.stop.take();
        if let Some((_, done)) = &self.log {
            done.store(true, Ordering::Relaxed);
        }
        let mut observer = self.observer.get_mut().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(observer) = &observer {
            observer.stop.store(true, Ordering::Relaxed);
        }
        let deadline = Instant::now() + STOP_WAIT;
        let threads = [
            ("watch", self.thread.take()),
            ("log", self.log.take().map(|(thread, _)| thread)),
            ("observe", observer.as_mut().and_then(|o| o.thread.take())),
        ];
        for (name, thread) in threads {
            if !join_by(thread, deadline) {
                note("capture_warning", json!({ "why": "a watcher thread did not stop in time; left to end on its own", "thread": name }));
            }
        }
    }
}

/// Observe mode's capture: a session of its own on the game's window, so the
/// watcher's sampling, on which auto read's pace rests, stays as it is. Its
/// frames are shrunk and encoded here, off the window and run log threads.
struct Observer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Observer {
    fn start(title: String, tx: Sender<Event>, notify: Arc<dyn Fn() + Send + Sync>, shared: Arc<Mutex<Shared>>) -> Observer {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || dpi_aware(|| observe(&title, &flag, &tx, &*notify, &shared)));
        Observer { stop, thread: Some(thread) }
    }
}

/// Stops the thread, waiting [`STOP_WAIT`] at most (observe mode is turned off on the UI thread); one still running
/// then ends on its own at its next look at `stop`, its frames going nowhere once its watcher is dropped.
impl Drop for Observer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if !join_by(self.thread.take(), Instant::now() + STOP_WAIT) {
            note("capture_warning", json!({ "why": "a watcher thread did not stop in time; left to end on its own", "thread": "observe" }));
        }
    }
}

/// Observe mode's loop: a frame every [`OBSERVE_EVERY`] while the game's
/// window shows the prison, until `stop`. Off the board (the hideout, a
/// map) it keeps none, see [`Watch::observing`]: an evening's farming after
/// a run took 933 MB (2026-10-30).
fn observe(title: &str, stop: &AtomicBool, tx: &Sender<Event>, notify: &dyn Fn(), shared: &Mutex<Shared>) {
    let mut stream: Option<Stream> = None;
    let mut last: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        let Some(s) = &mut stream else {
            stream = find_window(title).and_then(Stream::open).ok();
            if stream.is_none() {
                std::thread::sleep(RETRY);
            }
            continue;
        };
        if !lock(shared).watch.observing() {
            std::thread::sleep(OBSERVE_EVERY);
            continue;
        }
        if last.is_none_or(|t| t.elapsed() >= OBSERVE_EVERY) {
            match s.grab() {
                Ok(Some(frame)) => {
                    last = Some(Instant::now());
                    let at = SystemTime::now();
                    if let Some(jpeg) = observe_jpeg(&frame) {
                        if tx.send(Event::Observe(jpeg, at)).is_err() {
                            return;
                        }
                        notify();
                    }
                }
                Ok(None) => {}
                // Waits before opening again, so a window that keeps failing (minimized, gone) is not reopened in a
                // tight loop.
                Err(_) => {
                    stream = None;
                    std::thread::sleep(RETRY);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `frame` shrunk to [`OBSERVE_WIDTH`] as a JPEG.
fn observe_jpeg(frame: &Rgba) -> Option<Vec<u8>> {
    let small = if frame.w > OBSERVE_WIDTH { frame.resize(OBSERVE_WIDTH, frame.h * OBSERVE_WIDTH / frame.w) } else { frame.clone() };
    let mut jpeg = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut jpeg, OBSERVE_QUALITY);
    encoder.encode(&small.data, u16::try_from(small.w).ok()?, u16::try_from(small.h).ok()?, jpeg_encoder::ColorType::Rgba).ok()?;
    Some(jpeg)
}

/// A screen the game's log says opened: auto read leaves its rest, and reads
/// a theme scroll at once, and the decision cards, a day's doors, a Consume's
/// targets and a bite's board as they show.
fn on_signal(shared: &Mutex<Shared>, signal: Signal) {
    let mut shared = lock(shared);
    shared.watch.wake();
    let now = Instant::now();
    match signal {
        Signal::Theme => shared.reads_at = THEME_READS.iter().map(|&after| now + after).collect(),
        Signal::Click | Signal::Decision | Signal::CellPicked => {
            shared.reads_at.extend(CLICK_READS.iter().map(|&after| now + after));
            shared.reads_at.sort();
        }
        // Reads as soon as the board under the cards is in, and the cards
        // themselves, by when they came up: the log line comes late.
        Signal::Consume | Signal::Bite => {
            let reads = if signal == Signal::Consume { CONSUME_READS } else { BITE_READS };
            shared.reads_at.extend(reads.iter().map(|&after| now + after));
            shared.reads_at.sort();
        }
        // A run abandoned writes the day as 0 of 0.
        Signal::Day { max, .. } if max > 0 => {
            shared.reads_at.extend(DOOR_READS.iter().map(|&after| now + after));
            shared.reads_at.sort();
        }
        Signal::CardsUp(_, at) => {
            let up = now.checked_sub(gamelog::age(at)).unwrap_or(now);
            shared.reads_at.extend(CARDS_READS.iter().map(|&after| (up + after).max(now)));
            shared.reads_at.sort();
        }
        // Vorax: schedule a read when the new hand has been dealt (the tank animation may keep the picture unsteady).
        Signal::Vorax(cue) if cue.deals() => {
            shared.reads_at.extend(VORAX_READS.iter().map(|&after| now + after));
            shared.reads_at.sort();
        }
        Signal::Vorax(cue) if cue.resolves() => {
            shared.reads_at.push(now + VORAX_STRIP);
            shared.reads_at.sort();
        }
        _ => {}
    }
}

/// A Vorax tank click from the game's log, with the tank under the mouse cursor filled in (see
/// [`vorax_session::tank_at`]); every other signal as it is. Only the cursor's position is read: nothing is ever
/// sent to the game. Done here, on the log follower's thread as soon as the line is parsed, which is the earliest
/// point the click is known: the cursor is read up to one poll ([`gamelog`]'s 50 ms) plus however late the game
/// writes the line after the click, so a cursor already moved off the tank gives an unknown tank (or, rarely, the
/// wrong one). Lines found by looking back at program start (`stamp` None) are left unknown. The tank under the
/// cursor is read by `tank` ([`tank_under_cursor`] live), called only for a live tank click.
fn with_tank_from(signal: Signal, stamp: Option<i64>, tank: impl FnOnce() -> Option<u8>) -> Signal {
    match signal {
        Signal::Vorax(Cue::TankClick { .. }) if stamp.is_some() => Signal::Vorax(Cue::TankClick { tank: tank() }),
        other => other,
    }
}

/// What the log follower does with one signal of the game's log (see [`Watcher::start`]): the signal with its tank
/// filled in ([`with_tank_from`], the cursor read by `tank`) is what is recorded and sent on. Until 2026-11-06 the
/// filled-in signal was matched on but the raw one sent, so every live tank click went out with no tank.
fn on_log_signal(signal: Signal, stamp: Option<i64>, state: &Mutex<Shared>, tx: &Sender<Event>, notify: &dyn Fn(), tank: impl FnOnce() -> Option<u8>) {
    let signal = with_tank_from(signal, stamp, tank);
    match signal {
        // The accumulated progress is recorded for the UI (Watcher::charges): it does not wake a read and is
        // not handed to Tartarus (Vorax mode treats non-Vorax log signals as returning to Tartarus). The Vorax
        // charges going down is a new Vorax game paid for: that alone goes to Vorax, as Cue::NewGame (live
        // lines only; values found by looking back at start just set the first value).
        Signal::Charges { coin, value } => {
            let spent = {
                let mut shared = lock(state);
                let before = if coin == gamelog::VORAX_COIN { shared.charges.vorax } else { None };
                shared.charges.set(coin, value);
                coin == gamelog::VORAX_COIN && vorax_session::spent(before, value)
            };
            notify();
            if spent && stamp.is_some() && tx.send(Event::Game(Signal::Vorax(Cue::NewGame), stamp, SystemTime::now())).is_ok() {
                notify();
            }
        }
        // The UI scale goes to Vorax only, as a cue: it wakes no read, and as a Tartarus signal it would end a
        // Vorax mode (the deadlock guard) or drop a paused Vorax game.
        Signal::UiScale { micro } => {
            if tx.send(Event::Game(Signal::Vorax(Cue::UiScale { micro }), stamp, SystemTime::now())).is_ok() {
                notify();
            }
        }
        Signal::Treasure => {
            gift_reads(state);
            if tx.send(Event::Treasure).is_ok() {
                notify();
            }
        }
        Signal::Day { cur, max } => {
            on_signal(state, signal);
            if tx.send(Event::Day { cur, max }).is_ok() {
                notify();
            }
        }
        Signal::ObjectiveDone
        | Signal::Consume
        | Signal::Bite
        | Signal::ChainEnd
        | Signal::RunStart
        | Signal::RunEnd
        | Signal::TreasureOpen
        | Signal::Decision
        | Signal::CellPicked
        | Signal::CardsUp(..)
        | Signal::Incident(_)
        | Signal::Moved
        | Signal::Reroll
        | Signal::Instrument
        | Signal::Vaulted
        | Signal::VaultLit
        | Signal::Vorax(_) => {
            on_signal(state, signal);
            if tx.send(Event::Game(signal, stamp, SystemTime::now())).is_ok() {
                notify();
            }
        }
        _ => on_signal(state, signal),
    }
}

/// The tank under the mouse cursor, or None when the game's window or the cursor can't be read, or the cursor is
/// not over a tank. The cursor and the game's client area are both read with per-monitor DPI awareness
/// ([`dpi_aware`]), so both are physical screen pixels, as the captured frame is.
fn tank_under_cursor(title: &str) -> Option<u8> {
    let rect = game_rect(title).ok()?;
    let mut p = POINT::default();
    // SAFETY: p is a valid out pointer; GetCursorPos only reads the cursor position.
    dpi_aware(|| unsafe { GetCursorPos(&mut p) }).ok()?;
    vorax_session::tank_at(rect, (p.x, p.y))
}

/// A gift was confirmed: the pick screen leaves the board on show while the
/// gift plays out, before the treasure's next pick covers it again. Rooms
/// the Three-Headed Thumb Ring opened kept the board moving the whole time,
/// so no read waited for it to settle (2026-10-28); see [`GIFT_READS`].
fn gift_reads(shared: &Mutex<Shared>) {
    let now = Instant::now();
    let mut shared = lock(shared);
    shared.watch.wake();
    shared.reads_at = GIFT_READS.iter().map(|&after| now + after).collect();
    shared.gift_gap_until = Some(now + GIFT_GAP);
}

/// How long after the game's log says a gift was confirmed every new frame is
/// read, see [`sample`]: the board between picks showed within 0.6 s of the
/// log line (2026-10-28).
const GIFT_GAP: Duration = Duration::from_millis(1200);

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The watcher thread: samples until `stopped` disconnects; after a failure
/// drops the session and looks for the window again every [`RETRY`].
fn watch(title: &str, generation: u64, stopped: &Receiver<()>, tx: &Sender<Event>, shared: &Mutex<Shared>, notify: &dyn Fn()) {
    let send = |event| {
        if tx.send(event).is_ok() {
            notify();
        }
    };
    let (mut stream, mut status) = (None, None);
    // The client size the last Window event told of.
    let mut reported: Option<(i32, i32)> = None;
    loop {
        let opening = stream.is_none();
        let (wait, now) = match sample(title, generation, &mut stream, shared) {
            Ok(read) => {
                if let Some(s) = &stream
                    && window_due(opening, &mut reported, s.client)
                {
                    send(Event::Window(describe(s.hwnd)));
                }
                if let Some(event) = read {
                    send(event);
                }
                (if lock(shared).watch.resting() { REST } else { SAMPLE }, None)
            }
            Err(e) => {
                stream = None;
                lock(shared).latest = None;
                (RETRY, Some(e))
            }
        };
        if now != status {
            status = now;
            send(Event::Status(status.clone()));
        }
        if !matches!(stopped.recv_timeout(wait), Err(RecvTimeoutError::Timeout)) {
            return;
        }
    }
}

/// Whether the watcher tells of the window again ([`Event::Window`]): always for a new capture session, as before,
/// and while one runs whenever the client size it captures (`size`, from the last grab) differs from the size last
/// told of (`reported`, updated here), so a window resized or switched to another resolution mid-session gets a new
/// `window` row in the run logs. The same size is never told twice in a row by a running session.
fn window_due(opening: bool, reported: &mut Option<(i32, i32)>, size: Option<(i32, i32)>) -> bool {
    let due = opening || (size.is_some() && size != *reported);
    if due {
        *reported = size;
    }
    due
}

/// One sample: opens the session when there is none, takes the newest frame
/// (or the last one again when the game showed nothing new) and feeds its
/// print to the [`Watch`]; the frame when it should be read or read ahead.
fn sample(title: &str, generation: u64, stream: &mut Option<Stream>, shared: &Mutex<Shared>) -> Result<Option<Event>, String> {
    let s = match stream {
        Some(s) => s,
        None => stream.insert(Stream::open(find_window(title)?)?.telling(generation)),
    };
    let fresh = s.grab()?.map(|frame| {
        let print = fingerprint(&frame);
        let lit = crate::reader::scene::confirm_lit_raw(&frame);
        (frame, print, lit)
    });
    let mut shared = lock(shared);
    let new_frame = fresh.is_some();
    let Some((frame, print, lit)) = fresh.or_else(|| shared.latest.take().map(|(f, p)| (f, p, false))) else { return Ok(None) };
    shared.watch.note_lit(lit);
    let now = Instant::now();
    // Between a treasure's picks every new frame is read, back to back as
    // the reads take about 0.17 s: the board shows for 0.2 to 0.3 s, which
    // reads due at set times missed after a Brass Horn of Strife, and the
    // treasure's print tells only a first pick's screen (2026-10-28).
    if new_frame && shared.gift_gap_until.is_some_and(|until| now < until) {
        shared.watch.read_next();
    }
    if shared.reads_at.first().is_some_and(|&at| at <= now) {
        shared.reads_at.retain(|&at| at > now);
        shared.watch.read_next();
    }
    let read = if shared.watch.sample(print.clone()) {
        Some(Event::Frame(frame.clone(), shared.watch.take_trigger()))
    } else {
        shared.watch.take_ahead().then(|| Event::Ahead(frame.clone()))
    };
    shared.latest = Some((frame, print));
    Ok(read)
}

/// The screen pixels under `client` (screen coordinates); the window has to be in view.
fn screen_copy(client: RECT) -> Result<Rgba, String> {
    let (w, h) = (client.right - client.left, client.bottom - client.top);
    // SAFETY: GDI objects created here and released before returning.
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(dc, bitmap.into());
        let blit = BitBlt(dc, 0, 0, w, h, Some(screen), client.left, client.top, SRCCOPY);
        SelectObject(dc, old);

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut data = vec![0u8; w as usize * h as usize * 4];
        let lines = GetDIBits(dc, bitmap, 0, h as u32, Some(data.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);

        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        blit.map_err(|e| format!("BitBlt: {e}"))?;
        if lines != h {
            return Err("GetDIBits failed".to_string());
        }
        for px in data.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
            px[3] = 255;
        }
        Ok(Rgba { w: w as usize, h: h as usize, data })
    }
}

/// Maps a Windows error to "what: message".
fn fail(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {}", e.message())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new session always tells of its window; a running one tells again only when the captured client size changes,
    /// once per change.
    #[test]
    fn the_window_is_told_again_when_its_size_changes() {
        let mut reported = None;
        assert!(window_due(true, &mut reported, Some((2560, 1440))), "a new session");
        assert!(!window_due(false, &mut reported, Some((2560, 1440))), "same size");
        assert!(!window_due(false, &mut reported, None), "no size read");
        assert!(window_due(false, &mut reported, Some((1920, 1080))), "resized");
        assert!(!window_due(false, &mut reported, Some((1920, 1080))), "told once");
        assert!(window_due(true, &mut reported, Some((1920, 1080))), "a new session tells again as before");
        assert_eq!(reported, Some((1920, 1080)));
    }

    /// Only a live tank click gets the cursor's tank; a looked-back one and every other signal pass unchanged, and
    /// the cursor is not read for them. The cursor is a stand-in here (the mapping itself is tested in
    /// vorax_session).
    #[test]
    fn only_live_tank_clicks_read_the_cursor() {
        let click = Signal::Vorax(Cue::TankClick { tank: None });
        let unread = || -> Option<u8> { panic!("the cursor is read only for a live tank click") };
        assert_eq!(with_tank_from(click, None, unread), click);
        assert_eq!(with_tank_from(Signal::Vorax(Cue::Confirm), Some(1), unread), Signal::Vorax(Cue::Confirm));
        assert_eq!(with_tank_from(click, Some(1), || Some(4)), Signal::Vorax(Cue::TankClick { tank: Some(4) }));
    }

    fn shared() -> Mutex<Shared> {
        Mutex::new(Shared {
            watch: Watch::default(),
            latest: None,
            reads_at: Vec::new(),
            gift_gap_until: None,
            charges: gamelog::Charges::default(),
            log: gamelog::LogStatus::Searching,
        })
    }

    /// The tank filled in for a live click is the one sent on to the session (it was dropped until 2026-11-06, and
    /// all 12 clicks of a game logged none); a looked-back click goes out without one, and other signals as they
    /// came.
    #[test]
    fn the_tank_filled_in_is_the_one_sent() {
        let (state, (tx, rx)) = (shared(), channel());
        let notified = std::cell::Cell::new(0);
        let notify = || notified.set(notified.get() + 1);
        let click = Signal::Vorax(Cue::TankClick { tank: None });
        on_log_signal(click, Some(1_000), &state, &tx, &notify, || Some(3));
        match rx.try_recv() {
            Ok(Event::Game(signal, stamp, _)) => {
                assert_eq!(signal, Signal::Vorax(Cue::TankClick { tank: Some(3) }));
                assert_eq!(stamp, Some(1_000));
            }
            _ => panic!("a Game event"),
        }
        on_log_signal(click, None, &state, &tx, &notify, || panic!("not read for a looked-back line"));
        assert!(matches!(rx.try_recv(), Ok(Event::Game(Signal::Vorax(Cue::TankClick { tank: None }), None, _))));
        on_log_signal(Signal::Day { cur: 3, max: 12 }, Some(2), &state, &tx, &notify, || panic!("not a click"));
        assert!(matches!(rx.try_recv(), Ok(Event::Day { cur: 3, max: 12 })));
        assert_eq!(notified.get(), 3);
        assert!(rx.try_recv().is_err());
    }

    /// The UI scale goes on as a Vorax cue only (looked back or live), and wakes no read.
    #[test]
    fn the_ui_scale_goes_to_vorax() {
        let (state, (tx, rx)) = (shared(), channel());
        let notify = || {};
        for stamp in [Some(5), None] {
            on_log_signal(Signal::UiScale { micro: 842_409 }, stamp, &state, &tx, &notify, || panic!("not a click"));
            match rx.try_recv() {
                Ok(Event::Game(Signal::Vorax(Cue::UiScale { micro: 842_409 }), s, _)) => assert_eq!(s, stamp),
                _ => panic!("a Vorax cue"),
            }
        }
        assert!(rx.try_recv().is_err());
        assert!(lock(&state).reads_at.is_empty());
    }
}
