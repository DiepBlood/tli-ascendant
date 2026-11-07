//! A global hotkey: `RegisterHotKey` on a thread with its own message loop,
//! which calls back on every press until the `Hotkey` is dropped. With no
//! window the key belongs to that thread, so each `Hotkey` can use id 1.

use std::sync::mpsc::channel;
use std::thread::JoinHandle;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_HOTKEY, WM_QUIT};

/// Parses "F8", "Ctrl+Shift+R" or "Alt+5" into Win32 modifier bits and a
/// virtual key code: modifiers Ctrl, Alt, Shift, Win; keys F1-F24, A-Z, 0-9.
pub fn parse(text: &str) -> Result<(u32, u32), String> {
    let bad = || crate::lang::trf!("unknown hotkey {}: use a key such as F8, Ctrl+Shift+R or Alt+5", format!("{text:?}"));
    let parts: Vec<String> = text.split('+').map(|p| p.trim().to_uppercase()).collect();
    let (key, mods) = parts.split_last().ok_or_else(bad)?;
    let mut bits = 0;
    for m in mods {
        bits |= match m.as_str() {
            "CTRL" | "CONTROL" => MOD_CONTROL.0,
            "ALT" => MOD_ALT.0,
            "SHIFT" => MOD_SHIFT.0,
            "WIN" => MOD_WIN.0,
            _ => return Err(bad()),
        };
    }
    let vk = match key.as_bytes() {
        [c] if c.is_ascii_uppercase() || c.is_ascii_digit() => u32::from(*c),
        [b'F', rest @ ..] => match std::str::from_utf8(rest).ok().and_then(|n| n.parse::<u32>().ok()) {
            Some(n @ 1..=24) => 0x70 + n - 1,
            _ => return Err(bad()),
        },
        _ => return Err(bad()),
    };
    Ok((bits, vk))
}

/// What a key pressed while a hotkey is being recorded on the settings page gives (see [`record`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// Esc: recording stops and the hotkey stays as it was.
    Cancel,
    /// A hotkey [`parse`] reads back, such as "F8", "Alt+5" or "Ctrl+Shift+R".
    Key(String),
    /// A key no hotkey can be: not F1-F24, a letter or a digit.
    Unusable,
    /// A letter or a digit without Ctrl or Alt (Shift alone included), named: as a hotkey it would take that key
    /// from typing in every program.
    Typing(String),
    /// Copy, cut or paste (Ctrl+C, X or V, and Windows' Ctrl+Insert, Shift+Insert, Shift+Delete): as a hotkey it would
    /// take them from every program.
    Clipboard,
}

/// The first key pressed in `events` while a hotkey is being recorded; None when there is none yet. A modifier
/// pressed alone is no key (egui sends no key event for it), nor is a key released or held down (repeats):
/// recording waits for an ordinary key. Copy, cut and paste are refused: egui gets them as Copy, Cut and Paste, from
/// Ctrl+C, X and V but also from Ctrl+Insert, Shift+Insert and Ctrl+Shift+Delete, so which key it was is not known.
/// A key whose logical key is no letter, digit or F key (Ctrl+Shift+1 gives "!") is taken by its place on the
/// keyboard. The text is written Ctrl, Alt, Shift, key, as [`parse`] reads it.
pub fn record(events: &[eframe::egui::Event]) -> Option<Recorded> {
    use eframe::egui::{Event, Key};
    let usable = |key: Key| {
        let name = key.name();
        let plain = matches!(name.as_bytes(), [c] if c.is_ascii_uppercase() || c.is_ascii_digit());
        let function = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| (1..=24).contains(&n));
        (plain || function).then_some((name, plain))
    };
    events.iter().find_map(|event| {
        let (key, physical, mods) = match event {
            Event::Key { key, physical_key, pressed: true, repeat: false, modifiers } => (*key, *physical_key, *modifiers),
            Event::Copy | Event::Cut | Event::Paste(_) => return Some(Recorded::Clipboard),
            _ => return None,
        };
        if key == Key::Escape {
            return Some(Recorded::Cancel);
        }
        let Some((name, plain)) = usable(key).or_else(|| physical.and_then(usable)) else { return Some(Recorded::Unusable) };
        if mods.ctrl && !mods.alt && matches!(name, "C" | "X" | "V") {
            return Some(Recorded::Clipboard);
        }
        let mut text = String::new();
        for (on, part) in [(mods.ctrl, "Ctrl+"), (mods.alt, "Alt+"), (mods.shift, "Shift+")] {
            if on {
                text.push_str(part);
            }
        }
        text.push_str(name);
        Some(if plain && !mods.ctrl && !mods.alt { Recorded::Typing(text) } else { Recorded::Key(text) })
    })
}

pub struct Hotkey {
    thread: u32,
    join: Option<JoinHandle<()>>,
}

impl Hotkey {
    /// Registers `text` (see [`parse`]); `pressed` runs on the hotkey thread.
    pub fn register(text: &str, pressed: impl Fn() + Send + 'static) -> Result<Hotkey, String> {
        let (mods, vk) = parse(text)?;
        let (tx, rx) = channel();
        let name = text.to_string();
        let join = std::thread::spawn(move || {
            // SAFETY: Win32 calls on this thread's own message queue and hotkey id 1.
            unsafe {
                let mut msg = MSG::default();
                // Creates the thread's message queue before anyone posts to it.
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                if let Err(e) = RegisterHotKey(None, 1, HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT, vk) {
                    let _ = tx.send(Err(crate::lang::trf!("Hotkey {} is taken by another program ({}).", name, e)));
                    return;
                }
                let _ = tx.send(Ok(GetCurrentThreadId()));
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_HOTKEY {
                        pressed();
                    }
                }
                let _ = UnregisterHotKey(None, 1);
            }
        });
        let thread = rx.recv().map_err(|e| e.to_string())??;
        Ok(Hotkey { thread, join: Some(join) })
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        // SAFETY: posts WM_QUIT to the hotkey thread's queue, created before registration.
        if unsafe { PostThreadMessageW(self.thread, WM_QUIT, WPARAM(0), LPARAM(0)) }.is_ok()
            && let Some(join) = self.join.take()
        {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn hotkeys() {
        assert_eq!(parse("F8"), Ok((0, 0x77)));
        assert_eq!(parse("ctrl+shift+r"), Ok((2 | 4, u32::from(b'R'))));
        assert_eq!(parse("Alt + 5"), Ok((1, u32::from(b'5'))));
        assert!(parse("F25").is_err());
        assert!(parse("Hyper+F1").is_err());
        assert!(parse("").is_err());
    }

    /// Recording a hotkey: the key pressed with its modifiers becomes the text `parse` reads; a modifier alone, a
    /// release or a repeat is no key yet; Esc cancels; a letter or digit without Ctrl or Alt is refused for typing,
    /// and a key no hotkey can be is unusable.
    #[test]
    fn recorded_keys() {
        use super::{Recorded, record};
        use eframe::egui::{Event, Key, Modifiers};
        let press = |key: Key, modifiers: Modifiers| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        let ctrl_shift = Modifiers { ctrl: true, shift: true, command: true, ..Modifiers::NONE };
        let got = |events: &[Event]| record(events);
        assert_eq!(got(&[press(Key::F8, Modifiers::NONE)]), Some(Recorded::Key("F8".into())));
        let r = got(&[press(Key::R, ctrl_shift)]);
        assert_eq!(r, Some(Recorded::Key("Ctrl+Shift+R".into())));
        assert_eq!(parse("Ctrl+Shift+R"), parse("ctrl+shift+r"));
        assert_eq!(got(&[press(Key::Num5, Modifiers::ALT)]), Some(Recorded::Key("Alt+5".into())));
        assert_eq!(got(&[press(Key::F12, Modifiers::SHIFT)]), Some(Recorded::Key("Shift+F12".into())));
        // Every key recorded reads back.
        for text in ["F8", "Ctrl+Shift+R", "Alt+5", "Shift+F12", "Ctrl+Alt+Shift+F24"] {
            assert!(parse(text).is_ok(), "{text}");
        }
        // Ctrl (and Shift) held with nothing else: no key event comes, nor from a release or a repeat.
        assert_eq!(record(&[]), None);
        let release = Event::Key { key: Key::R, physical_key: None, pressed: false, repeat: false, modifiers: ctrl_shift };
        let repeat = Event::Key { key: Key::R, physical_key: None, pressed: true, repeat: true, modifiers: ctrl_shift };
        assert_eq!(record(&[release, repeat, Event::PointerGone]), None);
        // The first key pressed counts.
        assert_eq!(got(&[Event::PointerGone, press(Key::F2, Modifiers::NONE), press(Key::F3, Modifiers::NONE)]), Some(Recorded::Key("F2".into())));
        assert_eq!(got(&[press(Key::Escape, Modifiers::NONE)]), Some(Recorded::Cancel));
        assert_eq!(got(&[press(Key::Escape, Modifiers::CTRL)]), Some(Recorded::Cancel));
        assert_eq!(got(&[press(Key::A, Modifiers::NONE)]), Some(Recorded::Typing("A".into())));
        assert_eq!(got(&[press(Key::A, Modifiers::SHIFT)]), Some(Recorded::Typing("Shift+A".into())));
        assert_eq!(got(&[press(Key::Num7, Modifiers::NONE)]), Some(Recorded::Typing("7".into())));
        assert_eq!(got(&[press(Key::ArrowUp, Modifiers::CTRL)]), Some(Recorded::Unusable));
        assert_eq!(got(&[press(Key::F25, Modifiers::NONE)]), Some(Recorded::Unusable));
        assert_eq!(got(&[press(Key::Space, Modifiers::ALT)]), Some(Recorded::Unusable));
        // Ctrl+C reaches egui as Copy; Shift+Insert as Paste, no usable key.
        // Copy, cut and paste (from Ctrl+C, Ctrl+Insert, Ctrl+Shift+Delete and so on) are refused, and so are Ctrl+C,
        // X and V as keys: as hotkeys they would take copy and paste from every program.
        for event in [Event::Copy, Event::Cut, Event::Paste(String::new())] {
            assert_eq!(got(&[event]), Some(Recorded::Clipboard));
        }
        assert_eq!(got(&[press(Key::V, Modifiers::CTRL)]), Some(Recorded::Clipboard));
        assert_eq!(got(&[press(Key::C, ctrl_shift)]), Some(Recorded::Clipboard));
        assert_eq!(got(&[press(Key::C, Modifiers { alt: true, ctrl: true, command: true, ..Modifiers::NONE })]), Some(Recorded::Key("Ctrl+Alt+C".into())));
        // Ctrl+Shift+1: the logical key is "!", the key pressed the 1 key.
        let bang = Event::Key { key: Key::Exclamationmark, physical_key: Some(Key::Num1), pressed: true, repeat: false, modifiers: ctrl_shift };
        assert_eq!(got(&[bang]), Some(Recorded::Key("Ctrl+Shift+1".into())));
        let bang = Event::Key { key: Key::Exclamationmark, physical_key: None, pressed: true, repeat: false, modifiers: ctrl_shift };
        assert_eq!(got(&[bang]), Some(Recorded::Unusable));
        // A letter on a keyboard layout that gives no Latin letter still has its place.
        let other = Event::Key { key: Key::Semicolon, physical_key: Some(Key::Q), pressed: true, repeat: false, modifiers: Modifiers::ALT };
        assert_eq!(got(&[other]), Some(Recorded::Key("Alt+Q".into())));
    }

    /// Two keys held at once, as the app holds its read and auto read keys.
    #[cfg(windows)]
    #[test]
    fn two_at_once() {
        let a = super::Hotkey::register("Ctrl+Alt+Shift+F11", || {}).unwrap();
        let b = super::Hotkey::register("Ctrl+Alt+Shift+F12", || {}).unwrap();
        drop(a);
        drop(b);
    }
}
