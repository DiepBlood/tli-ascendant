//! Administrator rights. Windows does not pass keys meant for a window running as administrator to a program
//! running without (UIPI): with the game started as administrator and Ascendant not, the hotkeys do nothing while the
//! game is in front. This module tells when that is so ([`GameRights`], [`notify`]), and starts Ascendant again as
//! administrator ([`relaunch`]): once, from the status bar's notice, or at every start with the setting "Start as
//! administrator" ([`should_relaunch`]). Ascendant's manifest still asks for no rights, so it never has to run so.
//!
//! The game's rights are judged by its process token: opening it is refused (ERROR_ACCESS_DENIED) when the game runs
//! with more rights than this process, and when it opens, its integrity level is compared with this process's.
//!
//! The copy started as administrator gets [`AFTER`] and this process's id in front of the original arguments, and
//! waits for this process to end before it starts ([`take_after`], [`wait_for_exit`]): its hotkeys would be taken
//! and its settings read before this one saved them. The environment cannot carry the id (as the update's restart
//! does): a process started through "runas" does not get the caller's environment. The same argument marks the
//! copy as started again ([`mark_relaunched`]): it never starts itself again, even when it still runs without
//! administrator rights, so there is no loop of restarts. The Windows account the copy was started from goes with
//! it (its SID): a player without administrator rights can answer Windows' prompt with another account, whose copy
//! then reads that account's settings and run logs; the copy says so ([`other_account`]). It does not take over the
//! first account's folder: the files it would write there would belong to another account.

use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_CANCELLED, HANDLE};
use windows::Win32::Security::{GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_ELEVATION, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenElevation, TokenIntegrityLevel};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject};

/// The argument in front of the process id the copy started as administrator waits for; it also marks that copy as
/// started again.
pub const AFTER: &str = "--elevated-relaunch";

/// Whether this process was started again as administrator (it got [`AFTER`]).
static RELAUNCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The SID of the Windows account that started this copy again, when it was given (see [`take_after`]).
static STARTED_BY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// How starting again as administrator at start went, when "Start as administrator" tried it and this process
/// carried on (declined at Windows' prompt, or a failure): for the window to say so.
static AT_START: std::sync::OnceLock<Relaunch> = std::sync::OnceLock::new();

/// How often the game's process is looked up again (a new game process is judged once, when it is first seen).
const RECHECK: Duration = Duration::from_secs(5);

/// How long the copy started as administrator waits for the one that started it to end.
const WAIT_EXIT: Duration = Duration::from_secs(10);

/// What a process's token says of its rights.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Its token cannot be opened: it runs with more rights than this process.
    Denied,
    /// Its integrity level (0x2000 medium, 0x3000 high: administrator).
    Level(u32),
    /// Not known (the process is gone, or another refusal): taken as not higher, so nothing is shown.
    Unknown,
}

/// Whether "Start as administrator" starts this process again: only when it is on, this process runs without
/// administrator rights, and this process is not itself a copy started again (so a copy that still has no rights,
/// whatever the reason, never starts another).
pub fn should_relaunch(start_as_admin: bool, own_elevated: bool, relaunched: bool) -> bool {
    start_as_admin && !own_elevated && !relaunched
}

/// Whether the status bar shows its administrator notice: unless dismissed ("x", for as long as Ascendant stays
/// open) or this process runs as administrator, when the game runs with more rights than this process, or when
/// starting again as administrator was just declined (the notice then says so).
pub fn notify(game_higher: bool, own_elevated: bool, dismissed: bool, declined: bool) -> bool {
    !dismissed && !own_elevated && (game_higher || declined)
}

/// Marks this process as a copy started again as administrator (main found [`AFTER`]), by the account with the SID
/// `by` when it is known.
pub fn mark_relaunched(by: Option<String>) {
    RELAUNCHED.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(by) = by {
        let _ = STARTED_BY.set(by);
    }
}

/// Whether this copy runs as another Windows account than the one that started it again: that account's settings
/// and run logs are in use.
pub fn other_account() -> bool {
    STARTED_BY.get().is_some_and(|by| differs(by, own_user_sid().as_deref()))
}

/// Whether the account `own` (None: not known) is another than `by`: only when both are known and differ.
fn differs(by: &str, own: Option<&str>) -> bool {
    own.is_some_and(|own| !own.eq_ignore_ascii_case(by))
}

/// This process's Windows account as a SID string ("S-1-5-21-..."); None when it cannot be read.
pub fn own_user_sid() -> Option<String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{TOKEN_USER, TokenUser};
    let mut token = HANDLE::default();
    // SAFETY: the current process's pseudo handle needs no closing; the token is closed by Owned.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    let token = Owned(token);
    // TOKEN_USER plus its SID; u64s keep the buffer aligned for the pointer inside.
    let mut buf = [0u64; 16];
    let mut len = 0u32;
    // SAFETY: buf is valid for its size in bytes; the SID read lies inside it; the string is freed with LocalFree.
    unsafe {
        GetTokenInformation(token.0, TokenUser, Some(buf.as_mut_ptr().cast()), size_of_val(&buf) as u32, &mut len).ok()?;
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut text = windows::core::PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).ok()?;
        let sid = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    }
}

/// Whether this process is a copy started again as administrator.
pub fn relaunched() -> bool {
    RELAUNCHED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Keeps how starting again at start went, when this process carried on.
pub fn set_at_start(out: Relaunch) {
    let _ = AT_START.set(out);
}

/// How starting again at start went, when this process carried on after it.
pub fn at_start() -> Option<&'static Relaunch> {
    AT_START.get()
}

/// Whether this process runs as administrator.
pub fn own_elevated() -> bool {
    own_rights().0
}

/// Whether the game (`game`) runs with more rights than this process: never when this process runs as administrator.
pub fn higher(own_elevated: bool, own_level: Option<u32>, game: Probe) -> bool {
    if own_elevated {
        return false;
    }
    match game {
        Probe::Denied => true,
        Probe::Level(level) => own_level.is_some_and(|own| level > own),
        Probe::Unknown => false,
    }
}

/// Whether the game runs with more rights than Ascendant, judged once per game process: the result stays while the
/// same game runs (and while no game is found), so the status bar's notice does not come and go mid-run.
pub struct GameRights {
    own_elevated: bool,
    own_level: Option<u32>,
    /// The game process judged, and whether it is higher.
    pid: Option<u32>,
    higher: bool,
    /// When the game may be looked up again.
    next: Option<Instant>,
}

impl GameRights {
    /// This process's own rights, read once.
    pub fn current() -> GameRights {
        let (elevated, level) = own_rights();
        GameRights::with(elevated, level)
    }

    /// With this process's rights given (tests).
    pub fn with(own_elevated: bool, own_level: Option<u32>) -> GameRights {
        GameRights { own_elevated, own_level, pid: None, higher: false, next: None }
    }

    /// Whether the game found last runs with more rights than this process.
    pub fn higher(&self) -> bool {
        self.higher
    }

    /// Whether this process runs as administrator.
    pub fn own_elevated(&self) -> bool {
        self.own_elevated
    }

    /// Looks the game up with `find` at most every [`RECHECK`] (none when this process runs as administrator); a game
    /// process not judged before is judged with `probe`. No game found keeps the result.
    pub fn poll(&mut self, now: Instant, find: impl FnOnce() -> Option<u32>, probe: impl FnOnce(u32) -> Probe) {
        if self.own_elevated || self.next.is_some_and(|next| now < next) {
            return;
        }
        self.next = Some(now + RECHECK);
        let Some(pid) = find() else { return };
        if self.pid == Some(pid) {
            return;
        }
        self.pid = Some(pid);
        self.higher = higher(self.own_elevated, self.own_level, probe(pid));
    }
}

/// Closes a handle when dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The integrity level in an open token, if it can be read.
fn integrity(token: HANDLE) -> Option<u32> {
    // TOKEN_MANDATORY_LABEL plus its SID; 64 bytes of u64s keep the buffer aligned for the pointer inside.
    let mut buf = [0u64; 8];
    let mut len = 0u32;
    // SAFETY: buf is valid for its size in bytes; the SID read lies inside it, as GetTokenInformation wrote it.
    unsafe {
        GetTokenInformation(token, TokenIntegrityLevel, Some(buf.as_mut_ptr().cast()), size_of_val(&buf) as u32, &mut len).ok()?;
        let label = &*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let sid = label.Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        let last = u32::from(count.checked_sub(1)?);
        Some(*GetSidSubAuthority(sid, last))
    }
}

/// Whether this process runs as administrator, and its integrity level.
fn own_rights() -> (bool, Option<u32>) {
    let mut token = HANDLE::default();
    // SAFETY: the current process's pseudo handle needs no closing; the token is closed by Owned.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return (false, None);
    }
    let token = Owned(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: elevation is a valid out buffer of its size.
    let elevated = unsafe { GetTokenInformation(token.0, TokenElevation, Some((&raw mut elevation).cast()), size_of::<TOKEN_ELEVATION>() as u32, &mut len) }.is_ok() && elevation.TokenIsElevated != 0;
    (elevated, integrity(token.0))
}

/// What process `pid`'s token says of its rights (see [`Probe`]).
pub fn probe(pid: u32) -> Probe {
    // SAFETY: the handles are opened here and closed by Owned.
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return Probe::Unknown };
        let process = Owned(process);
        let mut token = HANDLE::default();
        match OpenProcessToken(process.0, TOKEN_QUERY, &mut token) {
            Ok(()) => {
                let token = Owned(token);
                integrity(token.0).map_or(Probe::Unknown, Probe::Level)
            }
            Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => Probe::Denied,
            Err(_) => Probe::Unknown,
        }
    }
}

/// One argument quoted for a Windows command line (the C runtime's rules): left as it is when it needs no quotes.
pub fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut slashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        // Backslashes count only before a quote: doubled there, and the quote escaped.
        let n = if c == '"' { slashes * 2 + 1 } else { slashes };
        out.extend(std::iter::repeat_n('\\', n));
        out.push(c);
        slashes = 0;
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// The parameters of the copy started as administrator: [`AFTER`], this process's id `pid` and its account's SID `sid`
/// ("-" when not known), then this process's own arguments `args` (without an [`AFTER`] of its own).
pub fn parameters(pid: u32, sid: Option<&str>, args: &[String]) -> String {
    let mut args = args.to_vec();
    take_after(&mut args);
    let mut out = format!("{AFTER} {pid} {}", quote(sid.unwrap_or("-")));
    for a in &args {
        out.push(' ');
        out.push_str(&quote(a));
    }
    out
}

/// Takes a leading [`AFTER`] with its process id and account SID out of the arguments: the id of the process to wait
/// for, and the SID of the account that started this copy (None for "-").
pub fn take_after(args: &mut Vec<String>) -> Option<(u32, Option<String>)> {
    let (pid, sid) = match args.as_slice() {
        [flag, pid, sid, ..] if flag == AFTER => (pid.parse::<u32>().ok()?, (sid != "-").then(|| sid.clone())),
        _ => return None,
    };
    args.drain(..3);
    Some((pid, sid))
}

/// Waits up to [`WAIT_EXIT`] for process `pid` to end (at once when it is gone).
pub fn wait_for_exit(pid: u32) {
    // SAFETY: the handle is only waited on and then closed.
    unsafe {
        if let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let process = Owned(process);
            WaitForSingleObject(process.0, WAIT_EXIT.as_millis() as u32);
        }
    }
}

/// How starting again as administrator went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Relaunch {
    /// The copy as administrator started: this one saves and closes.
    Started,
    /// The player answered No to Windows' administrator prompt (UAC): this one keeps running.
    Cancelled,
    Failed(String),
}

/// Starts this exe again as administrator with this process's arguments, through ShellExecuteExW's "runas" verb.
/// Blocks while Windows asks the player: call it on a background thread.
pub fn relaunch() -> Relaunch {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize};
    use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};
    let exe = match std::env::current_exe() {
        Ok(exe) => HSTRING::from(exe.as_os_str()),
        Err(e) => return Relaunch::Failed(e.to_string()),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let params = HSTRING::from(parameters(std::process::id(), own_user_sid().as_deref(), &args));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // The call returns once the process is started (or refused), not while the shell still works on it.
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: COM is set up for this thread only and undone after; info and its strings live through the call.
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let out = match ShellExecuteExW(&mut info) {
            Ok(()) => Relaunch::Started,
            Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => Relaunch::Cancelled,
            Err(e) => Relaunch::Failed(format!("0x{:08X} {}", e.code().0 as u32, e.message())),
        };
        if init.is_ok() {
            CoUninitialize();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEDIUM: u32 = 0x2000;
    const HIGH: u32 = 0x3000;

    #[test]
    fn higher_rights() {
        assert!(higher(false, Some(MEDIUM), Probe::Denied), "a token refused: higher");
        assert!(higher(false, Some(MEDIUM), Probe::Level(HIGH)));
        assert!(!higher(false, Some(MEDIUM), Probe::Level(MEDIUM)), "the same level");
        assert!(!higher(false, Some(HIGH), Probe::Level(MEDIUM)));
        assert!(!higher(false, Some(MEDIUM), Probe::Unknown), "not known: nothing shown");
        assert!(!higher(false, None, Probe::Level(HIGH)), "our own level not known: nothing shown");
        assert!(!higher(true, Some(HIGH), Probe::Denied), "running as administrator: never");
    }

    /// The game is judged once per process: not again while the same game runs, nor between looks; a game not
    /// found keeps the result; a new game process is judged anew; running as administrator never looks.
    #[test]
    fn judged_once_per_game_process() {
        let t0 = Instant::now();
        let mut rights = GameRights::with(false, Some(MEDIUM));
        let mut probes = Vec::new();
        let poll = |rights: &mut GameRights, at: Duration, pid: Option<u32>, answer: Probe, probes: &mut Vec<u32>| {
            rights.poll(t0 + at, || pid, |p| {
                probes.push(p);
                answer
            });
        };
        poll(&mut rights, Duration::ZERO, Some(7), Probe::Denied, &mut probes);
        assert!(rights.higher());
        poll(&mut rights, Duration::from_secs(1), Some(9), Probe::Level(MEDIUM), &mut probes);
        assert_eq!(probes, [7], "too soon: not looked up");
        poll(&mut rights, RECHECK, Some(7), Probe::Level(MEDIUM), &mut probes);
        assert_eq!(probes, [7], "the same game: not judged again");
        assert!(rights.higher());
        poll(&mut rights, RECHECK * 2, None, Probe::Level(MEDIUM), &mut probes);
        assert!(rights.higher(), "no game found: kept");
        poll(&mut rights, RECHECK * 3, Some(9), Probe::Level(MEDIUM), &mut probes);
        assert_eq!(probes, [7, 9], "a new game process");
        assert!(!rights.higher());

        let mut admin = GameRights::with(true, Some(HIGH));
        let mut looked = false;
        admin.poll(t0, || {
            looked = true;
            Some(7)
        }, |_| Probe::Denied);
        assert!(!looked && !admin.higher());
    }

    #[test]
    fn arguments_for_the_copy_as_administrator() {
        assert_eq!(parameters(42, None, &[]), "--elevated-relaunch 42 -");
        let args = ["open".to_string(), r"C:\My Frames\a.png".to_string()];
        assert_eq!(parameters(42, Some("S-1-5-21-1-2-3-1001"), &args), r#"--elevated-relaunch 42 S-1-5-21-1-2-3-1001 open "C:\My Frames\a.png""#);
        let again = [AFTER.to_string(), "7".to_string(), "S-1-5-21-9".to_string(), "open".to_string(), "a.png".to_string()];
        assert_eq!(parameters(42, None, &again), "--elevated-relaunch 42 - open a.png", "a flag of its own is not passed on");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote(r"C:\a b\"), r#""C:\a b\\""#, "trailing backslashes doubled before the closing quote");
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(r"a\\b c"), r#""a\\b c""#, "backslashes not before a quote stay as they are");
    }

    #[test]
    fn the_after_pair_is_taken_out() {
        let mut args = vec![AFTER.to_string(), "42".to_string(), "S-1-5-21-1".to_string(), "open".to_string(), "a.png".to_string()];
        assert_eq!(take_after(&mut args), Some((42, Some("S-1-5-21-1".to_string()))));
        assert_eq!(args, ["open", "a.png"]);
        let mut unknown = vec![AFTER.to_string(), "42".to_string(), "-".to_string()];
        assert_eq!(take_after(&mut unknown), Some((42, None)));
        assert!(unknown.is_empty());
        let mut plain = vec!["open".to_string(), "a.png".to_string()];
        assert_eq!(take_after(&mut plain), None);
        assert_eq!(plain, ["open", "a.png"]);
        let mut bad = vec![AFTER.to_string(), "x".to_string(), "-".to_string()];
        assert_eq!(take_after(&mut bad), None, "not an id: left as it is (and refused as usage)");
        assert_eq!(bad.len(), 3);
    }

    /// Another account only when both SIDs are known and differ (SIDs compare without case).
    #[test]
    fn another_account_is_told_apart() {
        assert!(!differs("S-1-5-21-1-1001", Some("S-1-5-21-1-1001")));
        assert!(!differs("S-1-5-21-1-1001", Some("s-1-5-21-1-1001")));
        assert!(differs("S-1-5-21-1-1001", Some("S-1-5-21-1-500")));
        assert!(!differs("S-1-5-21-1-1001", None), "own account not known: not said");
    }

    /// "Start as administrator" starts Ascendant again only when it is on, Ascendant runs without administrator
    /// rights, and it is not itself a copy started again: no loop of restarts.
    #[test]
    fn start_as_administrator_never_loops() {
        assert!(should_relaunch(true, false, false));
        assert!(!should_relaunch(false, false, false), "off");
        assert!(!should_relaunch(true, true, false), "already administrator");
        assert!(!should_relaunch(true, false, true), "a copy started again that still has no rights: not again");
    }

    /// The status bar's notice: the game higher, not higher, Ascendant already administrator, dismissed with "x";
    /// and after a declined prompt.
    #[test]
    fn the_notice_shows_only_when_it_helps() {
        assert!(notify(true, false, false, false), "the game higher");
        assert!(!notify(false, false, false, false), "the game not higher");
        assert!(!notify(true, true, false, false), "Ascendant already administrator");
        assert!(!notify(true, false, true, false), "dismissed with x");
        assert!(notify(false, false, false, true), "declined: says so");
        assert!(!notify(false, false, true, true), "declined, then dismissed");
        assert!(!notify(false, true, false, true), "administrator after all");
    }

    /// Run with --ignored: this process judged against itself is not higher.
    #[test]
    #[ignore]
    fn this_process_against_itself() {
        let (elevated, level) = own_rights();
        let probe = probe(std::process::id());
        let higher = higher(elevated, level, probe);
        println!("own: elevated {elevated}, level {level:#x?}, sid {:?}; probe of own pid {}: {probe:?}; higher: {higher}", own_user_sid(), std::process::id());
        assert!(own_user_sid().is_some_and(|s| s.starts_with("S-1-")));
        assert!(!higher);
        assert_eq!(probe, level.map_or(Probe::Unknown, Probe::Level));
    }
}
