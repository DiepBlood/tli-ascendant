//! The window beside the game, opened bare or on a saved screenshot. It is a
//! windows-subsystem program (no console window); the command line is the
//! console build, ascendant-cli.
//!
//!   Ascendant [open <frame.png> | uninstall]

#![windows_subsystem = "windows"]

use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: Ascendant [open <frame.png> | uninstall]; the command line is ascendant-cli";

fn main() -> ExitCode {
    // First, before wgpu, OCR or anything else loads a DLL by name: System32 only, never the folder the exe was
    // started from (see update::restrict_dll_search).
    // A refusal is reported on stderr (seen only when it is redirected, since the window has no console) and is not
    // fatal: everything still works, only without the restriction.
    #[cfg(windows)]
    if let Err(e) = ascendant::app::update::restrict_dll_search() {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "{e}");
    }
    note_panics();
    #[cfg(windows)]
    unthrottle();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [] => gui(None),
        [cmd, frame] if cmd == "open" => gui(Some(Path::new(frame))),
        #[cfg(windows)]
        [cmd] if cmd == "uninstall" => ascendant::app::install::uninstall(),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            show_error(&message);
            ExitCode::from(1)
        }
    }
}

/// Writes a panic's message and place to crash.log in the app's folder
/// (`settings::app_dir`) before
/// the process ends: the window has no console, and a release build aborts
/// on a panic, so a crash left only Windows' exception code (2026-10-30).
/// When renamed (2026-11-05) this went from the fixed %APPDATA%\warden to app_dir: the run check (health)
/// already looked for crash.log in app_dir, so with a home.txt the two no longer disagree.
fn note_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = ascendant::app::settings::app_dir() {
            let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
            let line = format!("{at} {} thread '{thread}': {info}
{}
", env!("CARGO_PKG_VERSION"), std::backtrace::Backtrace::force_capture());
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
                use std::io::Write;
                let _ = f.write_all(line.as_bytes());
            }
        }
        default(info);
    }));
}

/// Asks Windows not to slow this process for power: the window stands
/// behind the game, and a throttled process ran the playouts at half speed
/// (EcoQoS: a Cage Fight set took 20.7 s for 10.6 s, 2026-11-01). The
/// playouts keep their below-normal priority, so the game still comes first.
#[cfg(windows)]
fn unthrottle() {
    use windows::Win32::System::Threading::{GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation};
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    // SAFETY: the state lives through the call, which copies it.
    let _ = unsafe { SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling, (&raw const state).cast(), size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32) };
}

#[cfg(windows)]
fn gui(preload: Option<&Path>) -> Result<(), String> {
    ascendant::app::run(preload)
}

#[cfg(not(windows))]
fn gui(_: Option<&Path>) -> Result<(), String> {
    Err("the window runs on Windows only; use ascendant-cli".to_string())
}

/// There is no console to print to, so errors go to a message box.
#[cfg(windows)]
fn show_error(message: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::{HSTRING, w};
    // SAFETY: both strings outlive the call and no owner window is given.
    unsafe { MessageBoxW(None, &HSTRING::from(message), w!("Ascendant"), MB_OK | MB_ICONERROR) };
}

#[cfg(not(windows))]
fn show_error(message: &str) {
    eprintln!("{message}");
}
