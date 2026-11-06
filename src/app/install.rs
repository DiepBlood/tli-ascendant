//! A per-user install with no installer program and no admin rights: the
//! downloaded Ascendant.exe copies itself to %LOCALAPPDATA%\Programs\Ascendant
//! (or the app folder of the home %APPDATA%\Ascendant\home.txt names), adds a
//! Start menu shortcut, a desktop shortcut and an entry in Windows'
//! installed apps, whose Uninstall runs `Ascendant.exe uninstall`.
//!
//! The rename migration (2026-11-05, warden.exe -> Ascendant.exe) lives in [`register`]: every launch from the
//! install folder rewrites the uninstall entry to the current exe, and [`move_shortcuts`] repoints shortcuts that
//! target the old warden.exe in the same folder. The old exe is kept, not deleted (needed when restoring an older
//! build).

use super::update;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};
use windows::core::{HSTRING, w};

/// The installed apps entry, under HKEY_CURRENT_USER. The key name stays Warden:
/// the rename only changes the values inside the key (DisplayName, UninstallString...), not the key itself: when an
/// older build from before the rename is restored, it writes this key again on launch, and a new name would show two
/// entries in the installed apps list.
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Warden";

/// The program name: shortcut file name, display name in the installed apps list, and the uninstall dialog.
const NAME: &str = "Ascendant";
/// The shortcut file name from before the rename (Warden.lnk): replaced by [`NAME`] on launch of the new build (see
/// [`move_shortcuts`]).
const OLD_NAME: &str = "Warden";
/// The exe name from before the rename: a shortcut that targets it in the same folder is repointed to the new exe
/// (see [`move_shortcuts`]).
const OLD_EXE: &str = "warden.exe";

/// The app folder in the app's [`home`](super::settings::home) when one is
/// named, else %LOCALAPPDATA%\Programs\Ascendant (or the Programs\Warden of
/// an install from before the rename, while there is no Programs\Ascendant);
/// None when LOCALAPPDATA is not set.
pub fn install_dir() -> Option<PathBuf> {
    super::settings::home().map(|home| home.join("app")).or_else(|| {
        let programs = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Programs");
        let (new, old) = (programs.join(NAME), programs.join(OLD_NAME));
        Some(if !new.exists() && old.is_dir() { old } else { new })
    })
}

/// The Start menu shortcut called `name`; None when APPDATA is not set.
fn shortcut_named(name: &str) -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join(r"Microsoft\Windows\Start Menu\Programs").join(format!("{name}.lnk")))
}

/// The Start menu shortcut; None when APPDATA is not set.
fn shortcut_path() -> Option<PathBuf> {
    shortcut_named(NAME)
}

/// The desktop shortcut called `name`, in the desktop folder Windows reports (OneDrive may move it out of
/// %USERPROFILE%).
fn desktop_named(name: &str) -> Result<PathBuf, String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    // SAFETY: the returned string is copied out, then freed with CoTaskMemFree as the API requires.
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).map_err(|e| format!("the desktop folder: {}", e.message()))?;
        let dir = path.to_hstring();
        CoTaskMemFree(Some(path.0 as *const _));
        Ok(PathBuf::from(dir.to_os_string()).join(format!("{name}.lnk")))
    }
}

/// The desktop shortcut.
fn desktop_path() -> Result<PathBuf, String> {
    desktop_named(NAME)
}

/// First launch after the rename: the old Warden.lnk in the Start menu and on the desktop is replaced by an
/// Ascendant.lnk that points to `exe`. Only shortcuts the user already had are replaced (deleted ones are not
/// recreated); when the new one already exists, only the old one is deleted.
/// After the exe rename (warden.exe -> Ascendant.exe): when Ascendant.lnk still points to the old warden.exe in the
/// same folder as `exe`, it is repointed to `exe`. Shortcuts that point elsewhere are left alone.
/// The shortcut pinned to the taskbar (User Pinned\TaskBar\Warden.lnk) is deliberately left alone: the taskbar matches
/// windows by exe path, so changing only the shortcut target could end up as two icons; after the rename the user
/// has to pin it again once by hand. Until it is pinned again, the taskbar opens the old warden.exe that was kept.
fn move_shortcuts(exe: &Path) -> Result<(), String> {
    let pairs = [(shortcut_named(OLD_NAME), shortcut_path()), (desktop_named(OLD_NAME).ok(), desktop_path().ok())];
    for (old, new) in pairs {
        let (Some(old), Some(new)) = (old, new) else { continue };
        if !old.exists() {
            continue;
        }
        if !new.exists() {
            shortcut(exe, &new).map_err(|e| format!("{}: {e}", new.display()))?;
        }
        std::fs::remove_file(&old).map_err(|e| format!("{}: {e}", old.display()))?;
    }
    for link in [shortcut_path(), desktop_path().ok()].into_iter().flatten() {
        if link_target(&link).is_some_and(|target| is_old_exe(&target, exe)) {
            shortcut(exe, &link).map_err(|e| format!("{}: {e}", link.display()))?;
        }
    }
    Ok(())
}

/// `target` is the pre-rename warden.exe in the same folder as `exe` (and `exe` itself is not it).
fn is_old_exe(target: &Path, exe: &Path) -> bool {
    let named_old = |p: &Path| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(OLD_EXE));
    named_old(target) && !named_old(exe) && target.parent().zip(exe.parent()).is_some_and(|(a, b)| same_path(a, b))
}

/// Writes the desktop shortcut to `exe`.
pub fn desktop_shortcut(exe: &Path) -> Result<(), String> {
    let link = desktop_path()?;
    shortcut(exe, &link).map_err(|e| format!("{}: {e}", link.display()))
}

/// Whether `current` runs from the install folder.
pub fn installed_here(current: &Path) -> bool {
    install_dir().zip(current.parent()).is_some_and(|(dir, parent)| same_path(&dir, parent))
}

/// The same path, compared canonicalised where they exist and without case.
fn same_path(a: &Path, b: &Path) -> bool {
    let plain = |p: &Path| {
        let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_lowercase();
        p.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(p)
    };
    plain(a) == plain(b)
}

/// Installs `current`: copies it to the install folder (swapping out a
/// Ascendant.exe already there, which may be running), adds the Start menu and
/// desktop shortcuts and the installed apps entry. Returns the installed exe,
/// which the caller starts.
pub fn install(current: &Path) -> Result<PathBuf, String> {
    let dir = install_dir().ok_or("LOCALAPPDATA is not set")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let exe = dir.join(update::EXE);
    if !same_path(current, &exe) {
        let copy = |to: &Path| std::fs::copy(current, to).map(drop).map_err(|e| format!("{} to {}: {e}", current.display(), to.display()));
        if exe.exists() {
            let new = dir.join(update::NEW);
            copy(&new)?;
            update::swap(&exe, &new)?;
        } else {
            copy(&exe)?;
        }
    }
    let link = shortcut_path().ok_or("APPDATA is not set")?;
    shortcut(&exe, &link).map_err(|e| format!("{}: {e}", link.display()))?;
    // The desktop is a convenience: an install goes ahead without it.
    let _ = desktop_shortcut(&exe);
    register(&exe)?;
    Ok(exe)
}

/// Runs `f` with COM set up on this thread (or already set up, in another
/// mode, which suits an in-process server too).
fn with_com<T>(f: impl FnOnce() -> windows::core::Result<T>) -> Result<T, String> {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    // SAFETY: plain COM setup, balanced below when it succeeded.
    let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if init.is_err() && init != RPC_E_CHANGED_MODE {
        return Err(init.message());
    }
    let result = f();
    if init.is_ok() {
        // SAFETY: balances the successful CoInitializeEx above, with the interfaces released.
        unsafe { CoUninitialize() };
    }
    result.map_err(|e| e.message())
}

/// Writes a shell link at `link` that starts `exe`.
fn shortcut(exe: &Path, link: &Path) -> Result<(), String> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::Interface;
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    with_com(|| {
        // SAFETY: plain COM calls on interfaces this closure owns, with strings that outlive them.
        unsafe {
            let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell_link.SetPath(&HSTRING::from(exe.as_os_str()))?;
            if let Some(dir) = exe.parent() {
                shell_link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
            }
            shell_link.SetDescription(w!("Ascendant: unofficial decision tool for Torchlight: Infinite"))?;
            shell_link.cast::<IPersistFile>()?.Save(&HSTRING::from(link.as_os_str()), true)
        }
    })
}

/// The program shortcut `link` points to (environment variables not expanded); None when there is no such shortcut or
/// it cannot be read.
fn link_target(link: &Path) -> Option<PathBuf> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, SLGP_RAWPATH, ShellLink};
    use windows::core::Interface;
    if !link.is_file() {
        return None;
    }
    let mut path = vec![0u16; 1024];
    with_com(|| {
        // SAFETY: plain COM calls on interfaces this closure owns; GetPath writes
        // at most `path.len()` wide characters, nul included, and no find data.
        unsafe {
            let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell_link.cast::<IPersistFile>()?.Load(&HSTRING::from(link.as_os_str()), STGM_READ)?;
            shell_link.GetPath(&mut path, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
        }
    })
    .ok()?;
    let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
    (end > 0).then(|| PathBuf::from(String::from_utf16_lossy(&path[..end])))
}

/// Writes the installed apps entry for `exe`; called at install and again at
/// start, so an update shows its version.
pub fn register(exe: &Path) -> Result<(), String> {
    use windows::Win32::System::Registry::{HKEY, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegSetValueExW};
    // Every launch passes through here: also swap the pre-rename shortcuts for the new name. Shortcuts are a
    // convenience, so the app starts as usual even when the swap fails.
    let _ = move_shortcuts(exe);
    let dir = exe.parent().ok_or_else(|| format!("{}: no folder", exe.display()))?;
    let kb = std::fs::metadata(exe).map_err(|e| format!("{}: {e}", exe.display()))?.len().div_ceil(1024);
    let exe_text = exe.display().to_string();
    let texts = [
        ("DisplayName", NAME.to_string()),
        ("DisplayVersion", env!("CARGO_PKG_VERSION").to_string()),
        // This is a derived work: the Publisher is not given as the original author; the credit stays in URLInfoAbout
        // and the program's About page.
        ("Publisher", "Ascendant (based on arcasti/tli-warden)".to_string()),
        ("DisplayIcon", format!("{exe_text},0")),
        ("InstallLocation", dir.display().to_string()),
        ("UninstallString", format!("\"{exe_text}\" uninstall")),
        ("URLInfoAbout", "https://github.com/arcasti/tli-warden".to_string()),
    ];
    let numbers = [("NoModify", 1), ("NoRepair", 1), ("EstimatedSize", u32::try_from(kb).unwrap_or(u32::MAX))];
    let failed = |step: &str, e: windows::core::Error| format!(r"HKEY_CURRENT_USER\{UNINSTALL_KEY} {step}: {}", e.message());
    let mut key = HKEY::default();
    // SAFETY: `key` is written by the call and closed below; the strings outlive it.
    unsafe {
        RegCreateKeyExW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY), None, None, REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None)
            .ok()
            .map_err(|e| failed("create", e))?;
    }
    let mut result = Ok(());
    for (name, text) in &texts {
        let wide: Vec<u8> = text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        // SAFETY: `key` is open; the data is a nul-terminated UTF-16 string.
        result = result.and_then(|()| unsafe { RegSetValueExW(key, &HSTRING::from(*name), None, REG_SZ, Some(&wide)) }.ok().map_err(|e| failed(name, e)));
    }
    for (name, number) in numbers {
        // SAFETY: `key` is open; the data is a DWORD.
        result = result.and_then(|()| unsafe { RegSetValueExW(key, &HSTRING::from(name), None, REG_DWORD, Some(&number.to_le_bytes())) }.ok().map_err(|e| failed(name, e)));
    }
    // SAFETY: `key` was opened above and is not used after.
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

/// Asks, then removes the Start menu and desktop shortcuts, the installed
/// apps entry and the install folder; the caller exits next. Settings and
/// run logs stay.
pub fn uninstall() -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONQUESTION, MB_YESNO, MessageBoxW};
    // Name the actual folder: after the rename it may be %APPDATA%\Ascendant, the old %APPDATA%\warden, or wherever
    // home.txt points.
    let kept = super::settings::app_dir().map_or_else(|| r"%APPDATA%\Ascendant".to_string(), |d| d.display().to_string());
    let question = HSTRING::from(format!("Uninstall Ascendant? Your settings and run logs in {kept} are kept."));
    // SAFETY: both strings outlive the call and no owner window is given.
    let answer = unsafe { MessageBoxW(None, &question, w!("Ascendant"), MB_YESNO | MB_ICONQUESTION) };
    if answer != IDYES {
        return Ok(());
    }
    // Delete the shortcuts of both the new and the old name: a Warden.lnk made by a pre-rename build may still exist.
    for link in [shortcut_path(), desktop_path().ok(), shortcut_named(OLD_NAME), desktop_named(OLD_NAME).ok()].into_iter().flatten() {
        match std::fs::remove_file(&link) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("{}: {e}", link.display())),
            _ => {}
        }
    }
    // SAFETY: the key name outlives the call.
    let deleted = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY)) };
    if deleted != ERROR_FILE_NOT_FOUND {
        deleted.ok().map_err(|e| format!(r"HKEY_CURRENT_USER\{UNINSTALL_KEY}: {}", e.message()))?;
    }
    let Some(dir) = install_dir().filter(|d| d.exists()) else { return Ok(()) };
    let current = std::env::current_exe().map_err(|e| format!("this program's path: {e}"))?;
    remove_dir(&dir, &current, &std::env::temp_dir())
}

/// Removes the install folder `dir`. A running exe can't be deleted but can
/// be moved, so `current`, when it runs from `dir`, first goes to `temp`
/// (as ascendant-uninstalled-<pid>.exe), which Windows' temporary file cleanup
/// empties.
fn remove_dir(dir: &Path, current: &Path, temp: &Path) -> Result<(), String> {
    if current.parent().is_some_and(|p| same_path(p, dir)) {
        let parked = temp.join(format!("ascendant-uninstalled-{}.exe", std::process::id()));
        std::fs::rename(current, &parked).map_err(|e| format!("{} to {}: {e}", current.display(), parked.display()))?;
    }
    std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

// ───────────── Installing an OCR language (2026-11-05) ─────────────
//
// With no Windows OCR for the game's language, the buttons on the Board and Settings tabs start
// powershell.exe through ShellExecuteExW's "runas" verb: the player answers Windows' own
// administrator prompt (UAC), and Add-WindowsCapability runs. The command is fixed here
// ([`ocr_parameters`]) and takes no outside input. ShellExecuteExW rather than ShellExecuteW: the
// process handle lets it wait for the end and read the exit code, telling declined, failed and done apart.

/// The OCR language to install: for the zh-TW client (the main one, the default) or the English one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OcrLang {
    ZhTw,
    EnUs,
}

impl OcrLang {
    /// The OCR language for the game's language.
    pub fn of(lang: crate::lang::Lang) -> OcrLang {
        match lang {
            crate::lang::Lang::En => OcrLang::EnUs,
            crate::lang::Lang::ZhTw => OcrLang::ZhTw,
        }
    }

    /// The Windows capability's name.
    pub fn capability(self) -> &'static str {
        match self {
            OcrLang::ZhTw => "Language.OCR~~~zh-TW~0.0.1.0",
            OcrLang::EnUs => "Language.OCR~~~en-US~0.0.1.0",
        }
    }
}

/// The command for the player to run in an administrator PowerShell themselves.
pub fn ocr_manual_command(lang: OcrLang) -> String {
    format!("Add-WindowsCapability -Online -Name {}", lang.capability())
}

/// powershell.exe's parameters: exit code 0 once installed, 3010 when Windows wants a restart, and on a
/// failure the error's HRESULT (1 when it has none). Nothing is written to disk. Single quotes only, the whole
/// script inside -Command's double quotes; the window is hidden by ShellExecuteExW's SW_HIDE, the execution
/// policy is left as it is (-Command runs no script file).
pub fn ocr_parameters(lang: OcrLang) -> String {
    let script = format!(
        "$ErrorActionPreference='Stop'; try {{ $r=Add-WindowsCapability -Online -Name '{}'; if ($r.RestartNeeded) {{ exit 3010 }} else {{ exit 0 }} }} \
         catch {{ $h=$_.Exception.HResult; if ($h -eq 0) {{ $h=1 }}; exit $h }}",
        lang.capability()
    );
    format!("-NoProfile -NonInteractive -Command \"{script}\"")
}

/// How an install ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OcrInstall {
    /// Installed; `restart` when Windows asks for a restart.
    Done { restart: bool },
    /// The player answered No to the administrator prompt.
    Declined,
    /// Failed: the HRESULT or exit code, "0x800F0954", and what it means when known.
    Failed(String),
}

/// What a failure code means, for the common ones (in the window's language).
pub fn ocr_code_meaning(code: u32) -> Option<&'static str> {
    ocr_code_meaning_in(crate::lang::lang(), code)
}

/// [`ocr_code_meaning`] in `lang`.
pub fn ocr_code_meaning_in(lang: crate::lang::Lang, code: u32) -> Option<&'static str> {
    let tr = |en: &'static str| crate::lang::tr_in(lang, en);
    Some(match code {
        0x8007_04C7 => tr("the install was cancelled"),
        0x8007_0005 => tr("access denied"),
        0x8007_0422 => tr("the Windows Update service is turned off"),
        0x8007_0070 => tr("not enough disk space"),
        0x800F_0954 | 0x800F_0907 => tr("Windows could not reach Windows Update (often a WSUS or group policy setting)"),
        0x800F_0950 | 0x800F_081F | 0x800F_0906 => tr("the install files could not be found or downloaded"),
        0x8007_2F8F => tr("the secure connection failed (check the system clock)"),
        0x8024_402C | 0x8024_401C | 0x8024_0438 | 0x8007_2EE7 | 0x8007_2EFD => tr("no network connection to Windows Update"),
        1 => tr("PowerShell failed without an error code"),
        _ => return None,
    })
}

/// PowerShell's exit code as an outcome (in the window's language).
pub fn ocr_outcome(code: u32) -> OcrInstall {
    ocr_outcome_in(crate::lang::lang(), code)
}

/// [`ocr_outcome`] in `lang`.
pub fn ocr_outcome_in(lang: crate::lang::Lang, code: u32) -> OcrInstall {
    match code {
        0 => OcrInstall::Done { restart: false },
        3010 => OcrInstall::Done { restart: true },
        _ => OcrInstall::Failed(match ocr_code_meaning_in(lang, code) {
            Some(why) => format!("0x{code:08X} ({why})"),
            None => format!("0x{code:08X}"),
        }),
    }
}

/// powershell.exe by its full path: the system folder (GetSystemDirectoryW) and
/// WindowsPowerShell\v1.0\powershell.exe, so no other powershell.exe on the PATH or in the working folder runs.
pub fn powershell_path() -> Result<String, String> {
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: the buffer outlives the call, which writes at most its length.
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    if n == 0 || n >= buf.len() {
        return Err("GetSystemDirectoryW failed".to_string());
    }
    Ok(format!("{}\\WindowsPowerShell\\v1.0\\powershell.exe", String::from_utf16_lossy(&buf[..n])))
}

/// Installs `lang`'s OCR as administrator and waits for the end (blocks: call it on a background thread).
pub fn install_ocr(lang: OcrLang) -> OcrInstall {
    use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let exe = match powershell_path() {
        Ok(p) => HSTRING::from(p),
        Err(e) => return OcrInstall::Failed(e),
    };
    let params = HSTRING::from(ocr_parameters(lang));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: windows::core::PCWSTR(exe.as_ptr()),
        lpParameters: windows::core::PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: info and the strings it points to live through the calls; the process handle is closed after use.
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            return if e.code() == ERROR_CANCELLED.to_hresult() { OcrInstall::Declined } else { OcrInstall::Failed(format!("0x{:08X} {}", e.code().0 as u32, e.message())) };
        }
        if info.hProcess.is_invalid() {
            return OcrInstall::Failed("no process".to_string());
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let got = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        if let Err(e) = got {
            return OcrInstall::Failed(e.message());
        }
        ocr_outcome(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_here_paths() {
        let dir = install_dir().unwrap();
        assert!(installed_here(&dir.join(update::EXE)));
        assert!(installed_here(Path::new(&dir.join(update::EXE).to_string_lossy().to_uppercase())));
        assert!(!installed_here(&std::env::temp_dir().join(update::EXE)));
        assert!(!installed_here(&dir.join("sub").join(update::EXE)));
        // The pre-rename warden.exe in the same folder also counts as installed here.
        assert!(installed_here(&dir.join(OLD_EXE)));
    }

    #[test]
    fn old_exe_beside_the_new_one() {
        let dir = std::env::temp_dir().join("ascendant-old-exe");
        let exe = dir.join(update::EXE);
        assert!(is_old_exe(&dir.join("warden.exe"), &exe));
        assert!(is_old_exe(&dir.join("WARDEN.EXE"), &exe));
        assert!(!is_old_exe(&exe, &exe));
        assert!(!is_old_exe(&dir.join("other").join("warden.exe"), &exe));
        // When an older build is restored (itself warden.exe), the shortcuts are left alone.
        assert!(!is_old_exe(&dir.join("warden.exe"), &dir.join("warden.exe")));
    }

    #[test]
    fn link_target_reads_a_written_shortcut() {
        let dir = std::env::temp_dir().join(format!("ascendant-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (link, exe) = (dir.join("test.lnk"), dir.join(OLD_EXE));
        assert_eq!(link_target(&link), None);
        shortcut(&exe, &link).unwrap();
        assert!(link_target(&link).is_some_and(|t| same_path(&t, &exe)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn desktop_path_is_in_an_existing_folder() {
        let link = desktop_path().unwrap();
        assert_eq!(link.file_name().unwrap(), "Ascendant.lnk");
        assert!(link.parent().unwrap().is_dir());
        assert_eq!(desktop_named(OLD_NAME).unwrap().file_name().unwrap(), "Warden.lnk");
    }

    #[test]
    fn shortcuts_are_named_ascendant() {
        assert!(shortcut_path().unwrap().ends_with(r"Start Menu\Programs\Ascendant.lnk"));
        assert!(shortcut_named(OLD_NAME).unwrap().ends_with(r"Start Menu\Programs\Warden.lnk"));
    }

    #[test]
    fn remove_dir_moves_the_running_exe_out_first() {
        let base = std::env::temp_dir().join(format!("ascendant-remove-dir-{}", std::process::id()));
        let (dir, temp) = (base.join("Ascendant"), base.join("temp"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        // ping.exe stands in for the running Ascendant.exe: copied in and run
        // from there, it holds the folder as a running image does.
        let exe = dir.join(update::EXE);
        std::fs::copy(r"C:\Windows\System32\PING.EXE", &exe).unwrap();
        std::fs::write(dir.join("Ascendant.old.exe"), b"old").unwrap();
        let mut child = std::process::Command::new(&exe).args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        assert!(std::fs::remove_file(&exe).is_err(), "a running exe can't be deleted");
        let parked = temp.join(format!("ascendant-uninstalled-{}.exe", std::process::id()));
        let result = remove_dir(&dir, &exe, &temp);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(result, Ok(()));
        assert!(!dir.exists() && parked.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
    #[test]
    fn ocr_command_is_fixed() {
        // No global language change: the outcome is asked for in English explicitly.
        use crate::lang::Lang::En;
        assert_eq!(OcrLang::of(crate::lang::Lang::ZhTw).capability(), "Language.OCR~~~zh-TW~0.0.1.0");
        assert_eq!(OcrLang::of(crate::lang::Lang::En).capability(), "Language.OCR~~~en-US~0.0.1.0");
        assert_eq!(ocr_manual_command(OcrLang::ZhTw), "Add-WindowsCapability -Online -Name Language.OCR~~~zh-TW~0.0.1.0");
        let p = ocr_parameters(OcrLang::ZhTw);
        assert_eq!(
            p,
            "-NoProfile -NonInteractive -Command \"$ErrorActionPreference='Stop'; try { $r=Add-WindowsCapability -Online -Name 'Language.OCR~~~zh-TW~0.0.1.0'; if ($r.RestartNeeded) { exit 3010 } else { exit 0 } } catch { $h=$_.Exception.HResult; if ($h -eq 0) { $h=1 }; exit $h }\""
        );
        // No execution policy or window style switches, no file in %TEMP%; -Command's two double quotes only.
        assert!(!p.contains("ExecutionPolicy") && !p.contains("WindowStyle") && !p.contains("TEMP"));
        assert_eq!(p.matches('"').count(), 2);
        assert_eq!(ocr_parameters(OcrLang::EnUs), p.replace("zh-TW", "en-US"));
        assert_eq!(ocr_outcome_in(En, 0), OcrInstall::Done { restart: false });
        assert_eq!(ocr_outcome_in(En, 3010), OcrInstall::Done { restart: true });
        assert_eq!(ocr_outcome_in(En, 0x800F_0954), OcrInstall::Failed("0x800F0954 (Windows could not reach Windows Update (often a WSUS or group policy setting))".into()));
        assert_eq!(ocr_outcome_in(En, 0x8007_2EE7), OcrInstall::Failed("0x80072EE7 (no network connection to Windows Update)".into()));
        assert_eq!(ocr_outcome_in(En, 0x8000_4005), OcrInstall::Failed("0x80004005".into()));
        assert_eq!(ocr_code_meaning_in(En, 0x800F_0907), ocr_code_meaning_in(En, 0x800F_0954));
        assert_eq!(ocr_outcome_in(En, 0x8007_2F8F), OcrInstall::Failed("0x80072F8F (the secure connection failed (check the system clock))".into()));
        assert_eq!(ocr_outcome_in(crate::lang::Lang::ZhTw, 0x8007_2EE7), OcrInstall::Failed("0x80072EE7 (沒有網路，連不上 Windows Update)".into()));
        // powershell.exe from the system folder, by its full path.
        let exe = powershell_path().unwrap();
        assert!(exe.to_ascii_lowercase().ends_with("\\system32\\windowspowershell\\v1.0\\powershell.exe"), "{exe}");
        assert!(std::path::Path::new(&exe).is_file(), "{exe}");
    }
}
