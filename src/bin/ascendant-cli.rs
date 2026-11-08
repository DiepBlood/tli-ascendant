//! The command line: `ascendant-cli` is the console build, so it prints to the
//! terminal it runs from and the shell waits for its exit code. `Ascendant` is
//! the window.
//!
//!   ascendant-cli solve <board.json | frame.png> [--rollouts N] [--seed S] [--tools id,id]
//!   ascendant-cli read <frame.png | frame.jpg> [--tools id,id]
//!   ascendant-cli vorax-read <frame.png> [--json] [--time] [--page-scale x]
//!   ascendant-cli vorax-read --list <list.txt | -> [--page-scale x]
//!   ascendant-cli vorax-solve <frame.png> [--rollouts N] [--instruments id,id@round]
//!   ascendant-cli vorax-learn [vorax folder] [--out file]
//!   ascendant-cli capture [--frames N] <title text> <out.png>
//!   ascendant-cli bench [--games N] [--rollouts R] [--seed S] [--threads T]
//!   ascendant-cli bench --strategy-only [--set name=value,...] [--hold id,id] [--rerolls N] [--save file] [--against file] [--games N] [--seed S] [--threads T]
//!   ascendant-cli rates [runs folder] [--each]
//!   ascendant-cli audit [run folder ...] [--rollouts N] [--each]
//!   ascendant-cli keygen <private key file>
//!   ascendant-cli sign <private key file> <file> [--force]
//!   ascendant-cli verify <SHA256SUMS.txt> [--public-key <64 hex digits>]
//!   ascendant-cli release-key
//!   ascendant-cli check-key <private key file>
//!   ascendant-cli fingerprint <64 hex digits>
//!   ascendant-cli --version
//!
//! keygen, sign, verify and release-key are the release signing tools (see src/app/update.rs): keygen makes the
//! Ed25519 key pair, writes only the private key to the file, encrypted with a password typed twice at the console
//! (Argon2id and XChaCha20-Poly1305) and readable by the current Windows user alone, and prints only the public key;
//! sign asks for the password and writes `<file>.sig` (an existing one only with --force); verify checks
//! SHA256SUMS.txt's signature with the built-in public key (or `--public-key`, to test a key that is not built in)
//! and then every file it lists, which must include RELEASE-NOTES.txt; release-key prints the built-in public key and
//! exits with 2 while it is still the placeholder; check-key asks for the private key's password, signs 64 random
//! bytes and checks the signature with the built-in public key, printing `fingerprint <fp>` of the built-in key and
//! exiting with 0 only when the private key is the built-in key's (1 when not, 2 for the placeholder), which the
//! release script uses to prove a new key pair before a key-change release; fingerprint prints `fingerprint <fp>`
//! of any public key. Passwords are never read from arguments or the environment, and are typed at the console
//! one key at a time, without echo and without entering the console's command history.
//!
//! ASCENDANT_LANG=zh-TW (or the WARDEN_LANG of before the rename) reads the zh-TW game client and writes in Traditional
//! Chinese; English otherwise. vorax-read and vorax-solve read Vorax
//! screenshots of the zh-TW client only (see src/vorax).

use std::path::Path;
use std::process::ExitCode;
use ascendant::board::{Model, build_state};
use ascendant::engine::data::Tool;
use ascendant::reader::{read_model, save_png};
use ascendant::solver::bench::{LOW, Solo, bench, is_low, mean_se, spread, strategy_bench, summarize};
use ascendant::solver::{Params, TUNED};
use ascendant::solver::solve::{instant, solve};

/// println! that ignores a closed pipe (`| head`) instead of aborting.
macro_rules! out {
    () => { out!("") };
    ($($t:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), $($t)*);
    }};
}

const USAGE: &str = "usage: ascendant-cli solve <board.json | frame.png> [--rollouts N] [--seed S] [--tools id,id] | ascendant-cli read <frame.png> [--tools id,id] | ascendant-cli vorax-read <frame.png> [--json] [--time] [--page-scale x] | ascendant-cli vorax-read --list <list.txt | -> [--page-scale x] | ascendant-cli vorax-solve <frame.png> [--rollouts N] [--instruments id,id@round] | ascendant-cli vorax-learn [vorax folder] [--out file] | ascendant-cli capture [--frames N] <title text> <out.png> | ascendant-cli bench [--games N] [--rollouts R] [--seed S] [--threads T] | ascendant-cli bench --strategy-only [--set name=value,...] [--hold id,id] [--rerolls N] [--save file] [--against file] [--games N] [--seed S] [--threads T] | ascendant-cli latency [run folder] [--game-log UE_game.log] [--each] | ascendant-cli rates [runs folder] [--each] | ascendant-cli audit [run folder ...] [--rollouts N] [--each] | ascendant-cli keygen <private key file> | ascendant-cli sign <private key file> <file> [--force] | ascendant-cli verify <SHA256SUMS.txt> [--public-key <64 hex digits>] | ascendant-cli release-key | ascendant-cli check-key <private key file> | ascendant-cli fingerprint <64 hex digits> | ascendant-cli --version";

fn main() -> ExitCode {
    // First, before the reader's OCR or capture loads a DLL by name: System32 only, never the current or the exe's
    // folder (see update::restrict_dll_search).
    if let Err(e) = ascendant::app::update::restrict_dll_search() {
        // Reported, not fatal: everything still works, only without the restriction.
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "{e}");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Ok(language) = std::env::var("ASCENDANT_LANG").or_else(|_| std::env::var("WARDEN_LANG")) {
        ascendant::lang::set(ascendant::lang::Lang::from_key(&language));
    }
    match args.first().map(String::as_str) {
        Some("--version") if args.len() == 1 => {
            out!("ascendant {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(cmd @ ("solve" | "read" | "vorax-read" | "vorax-solve" | "vorax-learn" | "capture" | "bench" | "latency" | "rates" | "audit" | "keygen" | "sign" | "verify" | "release-key" | "check-key" | "fingerprint")) => {
            let run = match cmd {
                "solve" => run_solve(&args[1..]),
                "read" => run_read(&args[1..]),
                "vorax-read" => run_vorax_read(&args[1..]),
                "vorax-solve" => run_vorax_solve(&args[1..]),
                "vorax-learn" => run_vorax_learn(&args[1..]),
                "bench" => run_bench(&args[1..]),
                "latency" => run_latency(&args[1..]),
                "rates" => run_rates(&args[1..]),
                "audit" => run_audit(&args[1..]),
                "keygen" => signing::keygen(&args[1..], &mut signing::Console),
                "sign" => signing::sign(&args[1..], &mut signing::Console),
                "verify" => signing::verify(&args[1..]),
                "release-key" => signing::release_key(&args[1..]),
                "check-key" => signing::check_key(&args[1..], &mut signing::Console),
                "fingerprint" => signing::fingerprint(&args[1..]),
                _ => run_capture(&args[1..]),
            };
            run.unwrap_or_else(|message| {
                eprintln!("{message}");
                ExitCode::from(1)
            })
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(1)
        }
    }
}

/// The release signing tools. The private key exists in plain form only in memory, wiped after use; on disk it is
/// encrypted with a password ([`keyfile`]) that is only ever typed at the console ([`Console`]), never taken from
/// arguments or the environment. Neither the key nor the password is printed, logged or put in an error message.
mod signing {
    use super::USAGE;
    use ascendant::app::update::{self, RELEASE_PUBLIC_KEY, has_release_key, hex, key_fingerprint, public_key_line, verify_folder};
    use ed25519_dalek::SigningKey;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use zeroize::Zeroizing;

    /// Shortest password keygen accepts, in characters.
    pub const MIN_PASSWORD: usize = 12;

    /// How many tries keygen gives to type a good password twice.
    const PASSWORD_TRIES: usize = 3;

    /// Where passwords come from: the console in use, a script of answers in tests.
    pub trait Passwords {
        fn read(&mut self, prompt: &str) -> Result<Zeroizing<String>, String>;
    }

    /// Most UTF-16 units a typed password may have. [`PasswordLine`] reserves exactly this much before the first key,
    /// so its buffer never grows (growing would copy the password and leave the old buffer unwiped).
    pub const MAX_PASSWORD_UNITS: usize = 1024;

    /// What a key did to a [`PasswordLine`].
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Key {
        /// Keep reading.
        More,
        /// Enter: the password is complete.
        Done,
        /// Ctrl+C: give up.
        Cancel,
    }

    /// A password being typed one key (UTF-16 unit) at a time, as [`Console`] reads it with line input off: Enter
    /// ends it, Backspace removes the last character (both halves of a surrogate pair), Ctrl+C cancels, other
    /// control keys are ignored. Keys past [`MAX_PASSWORD_UNITS`] are dropped and make the result an error, so the
    /// rest of a too long password is still read here and never reaches the shell. Wiped when dropped.
    pub struct PasswordLine {
        units: Zeroizing<Vec<u16>>,
        overflow: bool,
    }

    impl Default for PasswordLine {
        fn default() -> Self {
            PasswordLine { units: Zeroizing::new(Vec::with_capacity(MAX_PASSWORD_UNITS)), overflow: false }
        }
    }

    impl PasswordLine {
        /// Takes one key.
        pub fn key(&mut self, unit: u16) -> Key {
            match unit {
                13 | 10 => Key::Done,
                3 => Key::Cancel,
                8 | 127 => {
                    if let Some(last) = self.units.pop()
                        && (0xDC00..0xE000).contains(&last)
                        && self.units.last().is_some_and(|u| (0xD800..0xDC00).contains(u))
                    {
                        self.units.pop();
                    }
                    Key::More
                }
                0..0x20 => Key::More,
                _ if self.units.len() >= MAX_PASSWORD_UNITS => {
                    self.overflow = true;
                    Key::More
                }
                _ => {
                    self.units.push(unit);
                    Key::More
                }
            }
        }

        /// The password typed, in a string allocated once at its exact size (no copy left behind by growing).
        pub fn finish(self) -> Result<Zeroizing<String>, String> {
            if self.overflow {
                return Err(format!("the password is longer than {MAX_PASSWORD_UNITS} characters"));
            }
            let invalid = || "the password is not valid text".to_string();
            let len = char::decode_utf16(self.units.iter().copied()).try_fold(0usize, |n, c| c.map(|c| n + c.len_utf8())).map_err(|_| invalid())?;
            let mut text = Zeroizing::new(String::with_capacity(len));
            for c in char::decode_utf16(self.units.iter().copied()) {
                text.push(c.map_err(|_| invalid())?);
            }
            Ok(text)
        }
    }

    /// Passwords typed at the console, read one key at a time with line input, echo, Ctrl+C processing and virtual
    /// terminal input all off: nothing is shown, conhost keeps no line for its command history (F7, the up arrow),
    /// and Ctrl+C arrives as a key that cancels. Keys typed ahead before the prompt and left after Enter are
    /// discarded, so none of the password reaches the shell. The console's mode is put back by a guard's Drop when
    /// the read returns, on success or error. A panic would skip it (the release profile sets `panic = "abort"` in
    /// Cargo.toml, so Drop does not run), which is why the read writes to stderr without the panicking `eprint!`;
    /// Ctrl+Break skips it too, ending the process at once. In both cases the shell resets the mode for its own
    /// next line.
    /// Input that is not a console (a pipe or a file) is refused, so a password can only be typed by hand.
    pub struct Console;

    impl Passwords for Console {
        fn read(&mut self, prompt: &str) -> Result<Zeroizing<String>, String> {
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::System::Console::{CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, FlushConsoleInputBuffer, GetConsoleMode, GetStdHandle, ReadConsoleW, STD_INPUT_HANDLE, SetConsoleMode};

            /// Puts the console's input mode back, and drops any keys left, when the read ends in any way.
            struct Restore(HANDLE, CONSOLE_MODE);
            impl Drop for Restore {
                fn drop(&mut self) {
                    // SAFETY: the handle is this process's console input, which outlives the guard.
                    unsafe {
                        let _ = FlushConsoleInputBuffer(self.0);
                        let _ = SetConsoleMode(self.0, self.1);
                    }
                }
            }

            // SAFETY: plain console calls on this process's own input handle; the one-unit buffer and the count
            // outlive each call, and ReadConsoleW writes at most one unit into it.
            unsafe {
                let input = GetStdHandle(STD_INPUT_HANDLE).map_err(|e| format!("no console input: {}", e.message()))?;
                let mut mode = CONSOLE_MODE::default();
                if GetConsoleMode(input, &mut mode).is_err() {
                    return Err("the password must be typed in a console window (input is not a console)".to_string());
                }
                let raw = mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_VIRTUAL_TERMINAL_INPUT);
                let _ = FlushConsoleInputBuffer(input);
                SetConsoleMode(input, raw).map_err(|e| format!("cannot turn echo and line input off: {}", e.message()))?;
                let restore = Restore(input, mode);
                // write!, not eprint!: eprint! panics when stderr cannot be written, and a panic aborts without
                // running the guard.
                let _ = write!(std::io::stderr(), "{prompt}");
                let _ = std::io::stderr().flush();
                let mut line = PasswordLine::default();
                let mut unit = Zeroizing::new([0u16; 1]);
                let ended = loop {
                    let mut n = 0u32;
                    if let Err(e) = ReadConsoleW(input, unit.as_mut_ptr().cast(), 1, &mut n, None) {
                        break Err(format!("cannot read the password: {}", e.message()));
                    }
                    if n == 0 {
                        break Err("the console input ended before Enter".to_string());
                    }
                    match line.key(unit[0]) {
                        Key::More => {}
                        Key::Done => break Ok(()),
                        Key::Cancel => break Err("cancelled (Ctrl+C)".to_string()),
                    }
                };
                unit[0] = 0;
                drop(restore);
                let _ = writeln!(std::io::stderr());
                ended?;
                line.finish()
            }
        }
    }

    /// The encrypted private key file: a header (magic, format version, KDF id, Argon2id memory in KiB, passes and
    /// lanes, salt, nonce), then the Ed25519 seed sealed with XChaCha20-Poly1305 under a key derived from the
    /// password by Argon2id. The whole header is the AEAD's associated data, so no byte of it can change unnoticed,
    /// and parameters below [`MIN`] are refused before any work, so a file cannot be weakened.
    pub mod keyfile {
        use argon2::{Algorithm, Argon2, Block, Params, Version};
        use zeroize::Zeroize;
        use chacha20poly1305::aead::{Aead, Payload};
        use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
        use zeroize::Zeroizing;

        const MAGIC: &[u8; 6] = b"ASCKEY";
        const VERSION: u8 = 1;
        const KDF_ARGON2ID: u8 = 1;
        const HEADER: usize = 6 + 1 + 1 + 12 + 16 + 24;
        /// The sealed seed: 32 bytes and the 16-byte tag.
        const SEALED: usize = 32 + 16;
        pub const LEN: usize = HEADER + SEALED;

        /// Argon2id's cost: memory in KiB, passes, lanes.
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct Kdf {
            pub m: u32,
            pub t: u32,
            pub p: u32,
        }

        /// What keygen uses: 64 MiB, 3 passes, 1 lane (above OWASP's minimum for Argon2id, 19 MiB and 2 passes).
        pub const DEFAULT: Kdf = Kdf { m: 64 * 1024, t: 3, p: 1 };
        /// The least a file may ask for.
        pub const MIN: Kdf = DEFAULT;
        /// The most a file may ask for (so a damaged file cannot ask for gigabytes).
        pub const MAX: Kdf = Kdf { m: 1024 * 1024, t: 64, p: 16 };

        /// The one message for a wrong password and a damaged file alike.
        pub const WRONG: &str = "wrong password or damaged key file (密碼錯誤或檔案損毀)";

        fn header(kdf: Kdf, salt: &[u8; 16], nonce: &[u8; 24]) -> Vec<u8> {
            let mut h = Vec::with_capacity(HEADER);
            h.extend_from_slice(MAGIC);
            h.extend_from_slice(&[VERSION, KDF_ARGON2ID]);
            for n in [kdf.m, kdf.t, kdf.p] {
                h.extend_from_slice(&n.to_le_bytes());
            }
            h.extend_from_slice(salt);
            h.extend_from_slice(nonce);
            h
        }

        /// The header's KDF parameters, salt and nonce; errors for anything but this format within its limits.
        pub fn parse(file: &[u8]) -> Result<(Kdf, [u8; 16], [u8; 24]), String> {
            if !file.starts_with(MAGIC) {
                let text = String::from_utf8_lossy(&file[..file.len().min(80)]).into_owned();
                return Err(if text.starts_with('#') || text.trim().bytes().all(|b| b.is_ascii_hexdigit()) {
                    "this is an old unencrypted key file, which is no longer accepted: make a new key with `ascendant-cli keygen`".to_string()
                } else {
                    "not an Ascendant release key file".to_string()
                });
            }
            if file.len() != LEN || file[6] != VERSION || file[7] != KDF_ARGON2ID {
                return Err("unknown key file version or a damaged key file".to_string());
            }
            let n = |i: usize| u32::from_le_bytes([file[i], file[i + 1], file[i + 2], file[i + 3]]);
            let kdf = Kdf { m: n(8), t: n(12), p: n(16) };
            if kdf.m < MIN.m || kdf.t < MIN.t || kdf.p < MIN.p || kdf.m > MAX.m || kdf.t > MAX.t || kdf.p > MAX.p {
                return Err(format!("the key file's Argon2id parameters (m={} KiB, t={}, p={}) are outside the allowed range", kdf.m, kdf.t, kdf.p));
            }
            let salt: [u8; 16] = file[20..36].try_into().unwrap_or_default();
            let nonce: [u8; 24] = file[36..60].try_into().unwrap_or_default();
            Ok((kdf, salt, nonce))
        }

        /// The AEAD key from `password`. Argon2id works in a block buffer allocated here and wiped here, whatever the
        /// outcome: `hash_password_into` would allocate its own and free it unwiped, leaving memory derived from the
        /// password behind.
        pub fn derive(password: &str, kdf: Kdf, salt: &[u8; 16]) -> Result<Zeroizing<[u8; 32]>, String> {
            let params = Params::new(kdf.m, kdf.t, kdf.p, Some(32)).map_err(|e| format!("Argon2id: {e}"))?;
            let mut blocks = vec![Block::default(); params.block_count()];
            let mut key = Zeroizing::new([0u8; 32]);
            let done = Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into_with_memory(password.as_bytes(), salt, key.as_mut(), &mut blocks);
            for block in &mut blocks {
                block.zeroize();
            }
            done.map_err(|e| format!("Argon2id: {e}"))?;
            Ok(key)
        }

        /// The file holding `seed` under `password`.
        pub fn seal(seed: &[u8; 32], password: &str, kdf: Kdf, salt: [u8; 16], nonce: [u8; 24]) -> Result<Vec<u8>, String> {
            let key = derive(password, kdf, &salt)?;
            let mut file = header(kdf, &salt, &nonce);
            let sealed = XChaCha20Poly1305::new(key.as_ref().into()).encrypt(XNonce::from_slice(&nonce), Payload { msg: seed, aad: &file }).map_err(|_| "encryption failed".to_string())?;
            file.extend_from_slice(&sealed);
            Ok(file)
        }

        /// The seed in `file` with an already derived `key` (the AEAD step of [`open`]).
        pub fn open_with(key: &[u8; 32], file: &[u8]) -> Result<Zeroizing<[u8; 32]>, String> {
            let (_, _, nonce) = parse(file)?;
            let plain = Zeroizing::new(XChaCha20Poly1305::new(key.into()).decrypt(XNonce::from_slice(&nonce), Payload { msg: &file[HEADER..], aad: &file[..HEADER] }).map_err(|_| WRONG.to_string())?);
            let mut seed = Zeroizing::new([0u8; 32]);
            if plain.len() != 32 {
                return Err(WRONG.to_string());
            }
            seed.copy_from_slice(&plain);
            Ok(seed)
        }

        /// The seed in `file`, opened with `password`.
        pub fn open(file: &[u8], password: &str) -> Result<Zeroizing<[u8; 32]>, String> {
            let (kdf, salt, _) = parse(file)?;
            let key = derive(password, kdf, &salt)?;
            open_with(&key, file)
        }
    }

    /// Random bytes from the operating system.
    fn random<const N: usize>() -> Result<[u8; N], String> {
        let mut bytes = [0u8; N];
        getrandom::fill(&mut bytes).map_err(|e| format!("no system randomness: {e}"))?;
        Ok(bytes)
    }

    /// The current Windows user's SID as text (S-1-5-...), from this process's token.
    pub fn current_user_sid() -> Result<String, String> {
        use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        use windows::core::PWSTR;
        // SAFETY: the token is closed and the SID string freed; TOKEN_USER is read from a buffer of the size the
        // system asked for, aligned for it (u64).
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(|e| format!("OpenProcessToken: {}", e.message()))?;
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            let got = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), (buf.len() * 8) as u32, &mut len);
            let _ = CloseHandle(token);
            got.map_err(|e| format!("GetTokenInformation: {}", e.message()))?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut text = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(|e| format!("ConvertSidToStringSidW: {}", e.message()))?;
            let sid = text.to_string().map_err(|_| "the SID is not text".to_string());
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            sid
        }
    }

    /// Creates `path`, which must not exist, with `bytes`, its permissions set at creation (no moment without
    /// them): a protected DACL with one entry, full control for the current user's SID.
    pub fn create_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        use windows::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_NONE};
        use windows::core::HSTRING;
        let sddl = format!("D:P(A;;FA;;;{})", current_user_sid()?);
        // SAFETY: the descriptor lives until CreateFileW returns and is then freed; the handle is handed to a File,
        // which closes it.
        let handle = unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(sddl), SDDL_REVISION_1, &mut sd, None).map_err(|e| format!("security descriptor: {}", e.message()))?;
            let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
            let made = CreateFileW(&HSTRING::from(path.as_os_str()), FILE_GENERIC_WRITE.0, FILE_SHARE_NONE, Some(&sa), CREATE_NEW, FILE_ATTRIBUTE_NORMAL, None);
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            made.map_err(|e| format!("{}: {}", path.display(), e.message()))?
        };
        // SAFETY: a fresh handle from CreateFileW that nothing else owns.
        let mut file = unsafe { std::fs::File::from_raw_handle(handle.0) };
        file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// `path`'s DACL in SDDL (for example `D:P(A;;FA;;;S-1-5-21-...)`).
    pub fn dacl_sddl(path: &Path) -> Result<String, String> {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::{ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT};
        use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
        use windows::core::{HSTRING, PWSTR};
        // SAFETY: both returned buffers are freed with LocalFree once read.
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let err = GetNamedSecurityInfoW(&HSTRING::from(path.as_os_str()), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, None, None, None, None, &mut sd);
            if err.is_err() {
                return Err(format!("{}: cannot read its permissions ({})", path.display(), err.0));
            }
            let mut text = PWSTR::null();
            let done = ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, SDDL_REVISION_1, DACL_SECURITY_INFORMATION, &mut text, None);
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            done.map_err(|e| format!("{}: {}", path.display(), e.message()))?;
            let sddl = text.to_string().map_err(|_| "the permissions are not text".to_string());
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            sddl
        }
    }

    /// Ok when `sddl` is a protected DACL whose only entry allows `sid` full control.
    pub fn only_owner(sddl: &str, sid: &str) -> Result<(), String> {
        let dacl = sddl.strip_prefix("D:").ok_or_else(|| format!("unexpected permissions {sddl}"))?;
        let (flags, aces) = dacl.split_at(dacl.find('(').unwrap_or(dacl.len()));
        let entries: Vec<&str> = aces.split(['(', ')']).filter(|a| !a.is_empty()).collect();
        if !flags.contains('P') {
            return Err(format!("the permissions still inherit from the folder: {sddl}"));
        }
        if entries != [format!("A;;FA;;;{sid}")] {
            return Err(format!("the permissions are not this user's alone: {sddl}"));
        }
        Ok(())
    }

    /// Asks for a new password twice until both match and it is long enough, at most [`PASSWORD_TRIES`] times.
    fn new_password(pw: &mut dyn Passwords) -> Result<Zeroizing<String>, String> {
        for _ in 0..PASSWORD_TRIES {
            let first = pw.read(&format!("New password for the release key (at least {MIN_PASSWORD} characters): "))?;
            if first.chars().count() < MIN_PASSWORD {
                eprintln!("Too short: use at least {MIN_PASSWORD} characters.");
                continue;
            }
            let second = pw.read("Type the password again: ")?;
            if *first != *second {
                eprintln!("The two passwords differ; try again.");
                continue;
            }
            return Ok(first);
        }
        Err("no matching password of the required length was typed".to_string())
    }

    /// Makes the key pair: the encrypted private key written to `path` (which must not exist) with owner-only
    /// permissions, checked by reading it back. Returns the public key.
    pub fn keygen_to(path: &Path, pw: &mut dyn Passwords) -> Result<[u8; 32], String> {
        if path.exists() {
            return Err(format!("{} already exists: refusing to overwrite a key file", path.display()));
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let password = new_password(pw)?;
        let seed = Zeroizing::new(random::<32>()?);
        let public = update::public_of(&seed);
        eprintln!("Encrypting the key (Argon2id, {} MiB)...", keyfile::DEFAULT.m / 1024);
        let sealed = keyfile::seal(&seed, &password, keyfile::DEFAULT, random()?, random()?)?;
        create_private_file(path, &sealed)?;
        let checked = (|| {
            only_owner(&dacl_sddl(path)?, &current_user_sid()?)?;
            let back = keyfile::open(&std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?, &password)?;
            if update::public_of(&back) != public {
                return Err(format!("{}: the key read back differs from the one written", path.display()));
            }
            Ok(())
        })();
        if let Err(e) = checked {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        Ok(public)
    }

    /// `keygen <private key file>`.
    pub fn keygen(args: &[String], pw: &mut dyn Passwords) -> Result<ExitCode, String> {
        let [path] = args else { return Err(format!("keygen needs one file; {USAGE}")) };
        let path = Path::new(path);
        let public = keygen_to(path, pw)?;
        let sid = current_user_sid()?;
        out!("Private key written to {}, encrypted with your password (key not shown).", path.display());
        out!("Permissions: only this Windows user ({sid}), full control; nothing inherited.");
        out!("Keep the password in a password manager: without it the key cannot be used, and forgetting it is losing the key.");
        out!("Public key: {}", hex(&public));
        out!("Fingerprint: {}", key_fingerprint(&public));
        out!("Replace the RELEASE_PUBLIC_KEY line in src/app/update.rs with:");
        out!("{}", public_key_line(&public));
        Ok(ExitCode::SUCCESS)
    }

    /// Signs `file` with the key in `key_path` (password from `pw`) into `<file>.sig`, which must not exist unless
    /// `force`. Returns the signature's path and the key's public half.
    pub fn sign_file(key_path: &Path, file: &Path, force: bool, pw: &mut dyn Passwords) -> Result<(PathBuf, [u8; 32]), String> {
        let mut sig_path = file.as_os_str().to_owned();
        sig_path.push(".sig");
        let sig_path = PathBuf::from(sig_path);
        if sig_path.exists() && !force {
            return Err(format!("{} already exists: refusing to overwrite it (add --force to replace it)", sig_path.display()));
        }
        let sealed = std::fs::read(key_path).map_err(|e| format!("{}: {e}", key_path.display()))?;
        keyfile::parse(&sealed).map_err(|e| format!("{}: {e}", key_path.display()))?;
        let data = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let password = pw.read("Password for the release key: ")?;
        let seed = keyfile::open(&sealed, &password).map_err(|e| format!("{}: {e}", key_path.display()))?;
        drop(password);
        let key = SigningKey::from_bytes(&seed);
        let public = key.verifying_key().to_bytes();
        let sig = update::sign(&key, &data);
        update::verify_sums(&data, &sig, &public)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        if force { options.create(true).truncate(true) } else { options.create_new(true) };
        options.open(&sig_path).and_then(|mut f| f.write_all(&sig)).map_err(|e| format!("{}: {e}", sig_path.display()))?;
        Ok((sig_path, public))
    }

    /// `sign <private key file> <file> [--force]`: writes `<file>.sig`, the 64-byte Ed25519 signature of the file.
    pub fn sign(args: &[String], pw: &mut dyn Passwords) -> Result<ExitCode, String> {
        let force = args.iter().any(|a| a == "--force");
        let rest: Vec<&String> = args.iter().filter(|a| *a != "--force").collect();
        let [key_path, file] = rest[..] else { return Err(format!("sign needs a key file and a file; {USAGE}")) };
        let (sig_path, public) = sign_file(Path::new(key_path), Path::new(file), force, pw)?;
        out!("Signed {file} -> {} (public key {}, fingerprint {})", sig_path.display(), hex(&public), key_fingerprint(&public));
        if public != RELEASE_PUBLIC_KEY {
            eprintln!("note: this key is not the public key built into this program ({}); `ascendant-cli verify` without --public-key will refuse it", hex(&RELEASE_PUBLIC_KEY));
        }
        Ok(ExitCode::SUCCESS)
    }

    /// `verify <SHA256SUMS.txt> [--public-key <hex>]`: the signature `<SHA256SUMS.txt>.sig`, then each listed file
    /// in the same folder. Exit code 1 when anything does not match. A developer's tool: players verify with the
    /// installed Ascendant's "Verify downloaded files...".
    pub fn verify(args: &[String]) -> Result<ExitCode, String> {
        let (mut sums, mut given) = (None, None);
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--public-key" => given = Some(it.next().ok_or_else(|| format!("--public-key needs 64 hex digits; {USAGE}"))?),
                _ if sums.is_none() && !arg.starts_with("--") => sums = Some(arg),
                _ => return Err(format!("unexpected argument {arg}; {USAGE}")),
            }
        }
        let sums = sums.ok_or_else(|| format!("verify needs SHA256SUMS.txt; {USAGE}"))?;
        let key = match given {
            Some(text) => {
                let key = update::hex32(text).ok_or("--public-key needs 64 hex digits")?;
                eprintln!("warning: checking with the public key given on the command line ({}), not the one built into this program", hex(&key));
                key
            }
            None if has_release_key() => RELEASE_PUBLIC_KEY,
            None => return Err("this program still has the placeholder public key, so it cannot verify releases; give --public-key or use an official build".to_string()),
        };
        match verify_folder(Path::new(sums), &key) {
            Ok(lines) => {
                for line in lines {
                    out!("{line}");
                }
                out!("All files match the signed {}.", Path::new(sums).file_name().map_or_else(|| sums.to_string(), |n| n.to_string_lossy().into_owned()));
                Ok(ExitCode::SUCCESS)
            }
            Err(e) => {
                eprintln!("{e}");
                eprintln!("VERIFY FAILED: do not use these files.");
                Ok(ExitCode::from(1))
            }
        }
    }

    /// `release-key`: the built-in public key and its fingerprint; exit code 2 while it is the placeholder (the
    /// release script checks).
    pub fn release_key(args: &[String]) -> Result<ExitCode, String> {
        if !args.is_empty() {
            return Err(format!("release-key takes no arguments; {USAGE}"));
        }
        out!("{}", hex(&RELEASE_PUBLIC_KEY));
        out!("fingerprint {}", key_fingerprint(&RELEASE_PUBLIC_KEY));
        if has_release_key() {
            Ok(ExitCode::SUCCESS)
        } else {
            eprintln!("PLACEHOLDER: this build has no release public key; run `ascendant-cli keygen` and put its line into src/app/update.rs");
            Ok(ExitCode::from(2))
        }
    }

    /// Whether the private key in `key_path` (password from `pw`) is `public`'s other half: it signs 64 fresh random
    /// bytes, and the signature must verify with `public` (by `verify_strict`). An error, before any password is
    /// asked, when `public` cannot verify anything (the placeholder) or the file is not a key file; an error for a
    /// wrong password.
    pub fn check_key_with(key_path: &Path, public: &[u8; 32], pw: &mut dyn Passwords) -> Result<bool, String> {
        if !update::usable_key(public) {
            return Err("the public key to check against is the placeholder or not a usable key".to_string());
        }
        let sealed = std::fs::read(key_path).map_err(|e| format!("{}: {e}", key_path.display()))?;
        keyfile::parse(&sealed).map_err(|e| format!("{}: {e}", key_path.display()))?;
        let password = pw.read("Password for the release key: ")?;
        let seed = keyfile::open(&sealed, &password).map_err(|e| format!("{}: {e}", key_path.display()))?;
        drop(password);
        let key = SigningKey::from_bytes(&seed);
        let challenge = random::<64>()?;
        let sig = update::sign(&key, &challenge);
        Ok(update::verify_sums(&challenge, &sig, public).is_ok())
    }

    /// `check-key <private key file>`: prints `fingerprint <fp>` of the built-in public key on stdout (prompts and
    /// messages go to stderr), then exits with 0 only when the private key matches it; 1 when it does not (or on
    /// any error, such as a wrong password), 2 while the built-in key is the placeholder (no password is asked).
    /// The release script runs it to prove that a new private key belongs to the public key built into the new
    /// version before a key-change release.
    pub fn check_key(args: &[String], pw: &mut dyn Passwords) -> Result<ExitCode, String> {
        let [path] = args else { return Err(format!("check-key needs one key file; {USAGE}")) };
        out!("fingerprint {}", key_fingerprint(&RELEASE_PUBLIC_KEY));
        if !has_release_key() {
            eprintln!("PLACEHOLDER: this build has no release public key to check the private key against");
            return Ok(ExitCode::from(2));
        }
        if check_key_with(Path::new(path), &RELEASE_PUBLIC_KEY, pw)? {
            eprintln!("OK: {path} is the private half of the public key built into this program.");
            Ok(ExitCode::SUCCESS)
        } else {
            eprintln!("MISMATCH: {path} is NOT the private half of the public key built into this program ({}).", hex(&RELEASE_PUBLIC_KEY));
            Ok(ExitCode::from(1))
        }
    }

    /// `fingerprint <64 hex digits>`: prints `fingerprint <fp>` of that public key; exits with 2 (after printing)
    /// when the key cannot verify anything (the placeholder, or not a valid point of large order).
    pub fn fingerprint(args: &[String]) -> Result<ExitCode, String> {
        let [text] = args else { return Err(format!("fingerprint needs a public key; {USAGE}")) };
        let key = update::hex32(text.trim()).ok_or_else(|| format!("fingerprint needs 64 hex digits; {USAGE}"))?;
        out!("fingerprint {}", key_fingerprint(&key));
        if update::usable_key(&key) {
            Ok(ExitCode::SUCCESS)
        } else {
            eprintln!("warning: this is not a usable public key (the placeholder, or not a valid Ed25519 key)");
            Ok(ExitCode::from(2))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Passwords answered from a list, in order.
        struct Scripted(Vec<&'static str>);

        impl Passwords for Scripted {
            fn read(&mut self, _prompt: &str) -> Result<Zeroizing<String>, String> {
                if self.0.is_empty() {
                    return Err("no more passwords".to_string());
                }
                Ok(Zeroizing::new(self.0.remove(0).to_string()))
            }
        }

        const PASSWORD: &str = "correct horse battery";

        fn temp(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("ascendant-cli-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn sealed_key_round_trip_and_tampering() {
            let seed = [5u8; 32];
            let file = keyfile::seal(&seed, PASSWORD, keyfile::DEFAULT, [1; 16], [2; 24]).unwrap();
            assert_eq!(file.len(), keyfile::LEN);
            assert_eq!(*keyfile::open(&file, PASSWORD).unwrap(), seed);
            // A wrong password says no more than a damaged file would.
            assert_eq!(keyfile::open(&file, "correct horse battery!").unwrap_err(), keyfile::WRONG);
            // Every single byte changed, header (magic, version, parameters, salt, nonce) or ciphertext, is refused:
            // with the key derived from the original salt the AEAD (whose associated data is the header) fails,
            // and a header that no longer parses fails before that.
            let key = keyfile::derive(PASSWORD, keyfile::DEFAULT, &[1; 16]).unwrap();
            assert_eq!(*keyfile::open_with(&key, &file).unwrap(), seed);
            for i in 0..file.len() {
                let mut bad = file.clone();
                bad[i] ^= 0x01;
                assert!(keyfile::open_with(&key, &bad).is_err(), "byte {i}");
            }
            // And through the whole path (the key derived again from the changed salt), for the salt and the
            // ciphertext.
            for i in [25, file.len() - 1] {
                let mut bad = file.clone();
                bad[i] ^= 0x80;
                assert_eq!(keyfile::open(&bad, PASSWORD).unwrap_err(), keyfile::WRONG, "byte {i}");
            }
        }

        #[test]
        fn weak_or_old_key_files_are_refused() {
            // Parameters below the minimum: refused before any key derivation.
            let weak = keyfile::Kdf { m: 8 * 1024, t: 1, p: 1 };
            let file = keyfile::seal(&[5; 32], PASSWORD, weak, [1; 16], [2; 24]).unwrap();
            assert!(keyfile::open(&file, PASSWORD).unwrap_err().contains("outside the allowed range"));
            for (at, value) in [(8usize, keyfile::MIN.m - 1), (12, keyfile::MIN.t - 1), (8, keyfile::MAX.m + 1), (16, keyfile::MAX.p + 1)] {
                let mut bad = keyfile::seal(&[5; 32], PASSWORD, weak, [1; 16], [2; 24]).unwrap();
                bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
                assert!(keyfile::parse(&bad).unwrap_err().contains("outside the allowed range"), "{at}={value}");
            }
            // The old plain hex format.
            let old = format!("# Ascendant release signing key\n{}\n", "ab".repeat(32));
            assert!(keyfile::parse(old.as_bytes()).unwrap_err().contains("old unencrypted key file"));
            assert!(keyfile::parse("ab".repeat(32).as_bytes()).unwrap_err().contains("old unencrypted key file"));
            assert!(keyfile::parse(b"something else").unwrap_err().contains("not an Ascendant"));
        }

        #[test]
        fn keygen_and_sign() {
            let dir = temp("keygen");
            let key = dir.join("release.key");
            // Too short, then two different ones, then a good pair.
            let mut pw = Scripted(vec!["short", "a long password one", "a long password two", PASSWORD, PASSWORD]);
            let public = keygen_to(&key, &mut pw).unwrap();
            assert!(pw.0.is_empty());
            // Owner-only from the start, as read back from the file system.
            only_owner(&dacl_sddl(&key).unwrap(), &current_user_sid().unwrap()).unwrap();
            let bytes = std::fs::read(&key).unwrap();
            assert!(bytes.starts_with(b"ASCKEY"));
            // Never overwritten.
            assert!(keygen_to(&key, &mut Scripted(vec![PASSWORD, PASSWORD])).unwrap_err().contains("already exists"));
            // Signing writes the .sig, which then verifies with the public key.
            let sums = dir.join("SHA256SUMS.txt");
            std::fs::write(&sums, "data").unwrap();
            let (sig, signer) = sign_file(&key, &sums, false, &mut Scripted(vec![PASSWORD])).unwrap();
            assert_eq!(signer, public);
            update::verify_sums(b"data", &std::fs::read(&sig).unwrap(), &public).unwrap();
            // An existing .sig is not overwritten (and no password is asked), unless --force.
            std::fs::write(&sig, "keep").unwrap();
            let mut none = Scripted(vec![]);
            assert!(sign_file(&key, &sums, false, &mut none).unwrap_err().contains("refusing to overwrite"));
            assert_eq!(std::fs::read_to_string(&sig).unwrap(), "keep");
            // A wrong password: the generic message only, and nothing written.
            assert!(sign_file(&key, &sums, true, &mut Scripted(vec!["wrong password!!"])).unwrap_err().ends_with(keyfile::WRONG));
            assert_eq!(std::fs::read_to_string(&sig).unwrap(), "keep");
            sign_file(&key, &sums, true, &mut Scripted(vec![PASSWORD])).unwrap();
            update::verify_sums(b"data", &std::fs::read(&sig).unwrap(), &public).unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn permission_check() {
            let sid = "S-1-5-21-1-2-3-1001";
            assert!(only_owner(&format!("D:P(A;;FA;;;{sid})"), sid).is_ok());
            assert!(only_owner(&format!("D:(A;;FA;;;{sid})"), sid).is_err());
            assert!(only_owner(&format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)"), sid).is_err());
            assert!(only_owner("D:P(A;;FA;;;S-1-5-21-9-9-9-1001)", sid).is_err());
            assert!(only_owner(&format!("D:P(A;;FR;;;{sid})"), sid).is_err());
        }

        /// Types `keys` (as the console would deliver them) into a fresh line; the last key's outcome and the line.
        fn typed(keys: &str) -> (Key, PasswordLine) {
            let mut line = PasswordLine::default();
            let mut last = Key::More;
            for unit in keys.encode_utf16() {
                last = line.key(unit);
                if last != Key::More {
                    break;
                }
            }
            (last, line)
        }

        #[test]
        fn password_keys() {
            // Enter ends it (CR as the console sends it, or LF); keys after Enter are not taken.
            let (key, line) = typed("secret\rmore");
            assert_eq!(key, Key::Done);
            assert_eq!(*line.finish().unwrap(), "secret");
            assert_eq!(*typed("abc\n").1.finish().unwrap(), "abc");
            // Backspace (BS or DEL) removes the last character; on an empty line it does nothing.
            assert_eq!(*typed("\u{8}ab\u{8}c\u{7f}d\r").1.finish().unwrap(), "ad");
            // A character outside the BMP (a surrogate pair) goes in one Backspace.
            assert_eq!(*typed("x\u{1F511}\u{8}y\r").1.finish().unwrap(), "xy");
            assert_eq!(*typed("密碼\u{1F511}\r").1.finish().unwrap(), "密碼\u{1F511}");
            // Ctrl+C cancels.
            assert_eq!(typed("abc\u{3}def").0, Key::Cancel);
            // Other control keys (Tab, Esc, Ctrl+Z) are ignored.
            assert_eq!(*typed("a\tb\u{1b}c\u{1a}\r").1.finish().unwrap(), "abc");
            // A lone surrogate is not text.
            let mut line = PasswordLine::default();
            line.key(0xD800);
            assert!(line.finish().is_err());
        }

        #[test]
        fn password_buffer_never_grows() {
            let mut line = PasswordLine::default();
            let (start, capacity) = (line.units.as_ptr(), line.units.capacity());
            assert_eq!(capacity, MAX_PASSWORD_UNITS);
            for _ in 0..MAX_PASSWORD_UNITS + 50 {
                assert_eq!(line.key(u16::from(b'a')), Key::More);
            }
            // Same buffer, same capacity: never reallocated, so no copy was left behind.
            assert_eq!((line.units.as_ptr(), line.units.capacity(), line.units.len()), (start, capacity, MAX_PASSWORD_UNITS));
            assert_eq!(line.key(13), Key::Done);
            assert!(line.finish().unwrap_err().contains("longer than"));
            // Exactly the limit is fine, and the string is allocated at its exact size.
            let mut line = PasswordLine::default();
            for _ in 0..MAX_PASSWORD_UNITS {
                line.key(u16::from(b'b'));
            }
            assert_eq!(line.finish().unwrap().len(), MAX_PASSWORD_UNITS);
            let text = typed("密碼 password\r").1.finish().unwrap();
            assert_eq!(text.capacity(), text.len());
        }

        #[test]
        fn check_key_matches_only_its_public_key() {
            let dir = temp("check-key");
            let key = dir.join("release.key");
            let public = keygen_to(&key, &mut Scripted(vec![PASSWORD, PASSWORD])).unwrap();
            // The matching public key.
            let mut pw = Scripted(vec![PASSWORD]);
            assert!(check_key_with(&key, &public, &mut pw).unwrap());
            assert!(pw.0.is_empty());
            // Another key pair's public key.
            assert!(!check_key_with(&key, &update::public_of(&[9; 32]), &mut Scripted(vec![PASSWORD])).unwrap());
            // A wrong password is an error, not a mismatch.
            assert!(check_key_with(&key, &public, &mut Scripted(vec!["wrong password!!"])).unwrap_err().ends_with(keyfile::WRONG));
            // The placeholder: refused before any password is asked.
            let mut none = Scripted(vec![]);
            assert!(check_key_with(&key, &update::PLACEHOLDER_PUBLIC_KEY, &mut none).unwrap_err().contains("placeholder"));
            // Not a key file.
            std::fs::write(dir.join("other"), b"something else").unwrap();
            assert!(check_key_with(&dir.join("other"), &public, &mut Scripted(vec![])).unwrap_err().contains("not an Ascendant"));
            // This source tree's built-in key is the placeholder: the command exits 2 without asking.
            if !has_release_key() {
                assert_eq!(check_key(&[key.display().to_string()], &mut Scripted(vec![])).unwrap(), ExitCode::from(2));
            }
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn fingerprint_command() {
            let public = update::public_of(&[42; 32]);
            assert_eq!(fingerprint(&[hex(&public)]).unwrap(), ExitCode::SUCCESS);
            assert_eq!(fingerprint(&[hex(&public).to_uppercase()]).unwrap(), ExitCode::SUCCESS);
            assert_eq!(fingerprint(&["0".repeat(64)]).unwrap(), ExitCode::from(2));
            assert!(fingerprint(&["abc".to_string()]).is_err());
            assert!(fingerprint(&[]).is_err());
            let fp = key_fingerprint(&public);
            assert_eq!(fp.split(' ').count(), 8);
            assert!(fp.split(' ').all(|g| g.len() == 4 && g.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))));
        }
    }
}

fn number<T: std::str::FromStr>(flag: &str, value: Option<&String>) -> Result<T, String> {
    value.and_then(|v| v.parse().ok()).ok_or_else(|| format!("{flag} needs a number; {USAGE}"))
}

/// `--tools id,id`: instrument keys such as coin or chains.
fn tools(value: Option<&String>) -> Result<Vec<Tool>, String> {
    let value = value.ok_or_else(|| format!("--tools needs instrument ids; {USAGE}"))?;
    value
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| Tool::from_key(s.trim()).ok_or_else(|| format!("unknown instrument id {s}")))
        .collect()
}

/// Whether `file` is a screenshot (PNG, or a run folder's JPEG frame) rather than a board JSON.
fn is_png(file: &str) -> bool {
    Path::new(file).extension().is_some_and(|e| ["png", "jpg", "jpeg"].iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// `rates [runs folder] [--each]`: rate reconciliation. Scans every run's log.jsonl and lists the measured
/// proportions of the random events, the 95% confidence intervals and the engine's assumptions (see
/// `ascendant::rates`); the default folder is %APPDATA%\Ascendant\runs, and `--each` appends every sample for manual
/// checking.
fn run_rates(args: &[String]) -> Result<ExitCode, String> {
    let (mut root, mut each) = (None, false);
    for arg in args {
        match arg.as_str() {
            "--each" => each = true,
            _ if root.is_none() && !arg.starts_with("--") => root = Some(Path::new(arg).to_path_buf()),
            _ => return Err(format!("unexpected argument {arg}; {USAGE}")),
        }
    }
    let root = match root {
        Some(path) => path,
        None => ascendant::app::settings::app_dir().ok_or("APPDATA is not set")?.join("runs"),
    };
    let clock = std::time::Instant::now();
    let samples = ascendant::rates::scan(&root)?;
    out!("{}", root.display());
    print!("{}", ascendant::rates::report(&samples, each));
    out!("{:.1} s", clock.elapsed().as_secs_f64());
    Ok(ExitCode::SUCCESS)
}

/// `audit [run folder ...] [--rollouts N] [--each]`: per-run decision scoring. Re-solves every option on the board
/// read at the time of each advice (1600 rounds per option by default), infers which move the player actually made,
/// and splits the run's whole gap into decision loss and luck (see `ascendant::audit`); the default is the newest
/// folder with a run_end under %APPDATA%\Ascendant\runs, and `--each` prints one line per decision.
fn run_audit(args: &[String]) -> Result<ExitCode, String> {
    let (mut folders, mut rollouts, mut each) = (Vec::new(), 1600usize, false);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--rollouts" => rollouts = number("--rollouts", it.next())?,
            "--each" => each = true,
            _ if !arg.starts_with("--") => folders.push(Path::new(arg).to_path_buf()),
            _ => return Err(format!("unexpected argument {arg}; {USAGE}")),
        }
    }
    if rollouts == 0 {
        return Err(format!("--rollouts must be at least 1; {USAGE}"));
    }
    if folders.is_empty() {
        let root = ascendant::app::settings::app_dir().ok_or("APPDATA is not set")?.join("runs");
        let mut dirs: Vec<_> = std::fs::read_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.join("log.jsonl").is_file()).collect();
        dirs.sort();
        // The newest finished run.
        let newest = dirs.into_iter().rev().find(|d| std::fs::read_to_string(d.join("log.jsonl")).is_ok_and(|t| t.contains("\"kind\":\"run_end\""))).ok_or("no run log with a run_end")?;
        folders.push(newest);
    }
    let clock = std::time::Instant::now();
    let mut games = Vec::new();
    for folder in &folders {
        let file = if folder.is_dir() { folder.join("log.jsonl") } else { folder.clone() };
        let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let dir = if folder.is_dir() { folder.as_path() } else { folder.parent().unwrap_or(folder) };
        let name = dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
        games.extend(ascendant::audit::games(&name, &text));
    }
    ascendant::audit::score(&mut games, rollouts);
    print!("{}", ascendant::audit::report(&games, rollouts, each));
    out!("{:.1} s", clock.elapsed().as_secs_f64());
    Ok(ExitCode::SUCCESS)
}

/// `latency [run folder | log.jsonl] [--game-log UE_game.log] [--each]`:
/// the auto read timings of a run log (by default the newest under
/// %APPDATA%\Ascendant\runs) and, with the game's log, how long each click in
/// the game waited for the next board read and its advice; `--each` lists
/// every click. The game log is in the game's folder, under
/// `UE_game\TorchLight\Saved\Logs`.
fn run_latency(args: &[String]) -> Result<ExitCode, String> {
    use ascendant::app::latency::{game_events, game_lines, report, run_span};
    let (mut game_log, mut each, mut run) = (None, false, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--game-log" => game_log = Some(it.next().ok_or_else(|| format!("--game-log needs a file; {USAGE}"))?),
            "--each" => each = true,
            _ => run = Some(Path::new(arg).to_path_buf()),
        }
    }
    let run = match run {
        Some(path) => path,
        None => {
            let root = ascendant::app::settings::app_dir().ok_or("APPDATA is not set")?.join("runs");
            let mut dirs: Vec<_> = std::fs::read_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.join("log.jsonl").is_file()).collect();
            dirs.sort();
            // The newest run with a board read; the folder of a session's last minutes may have none.
            dirs.into_iter().rev().find(|d| std::fs::read_to_string(d.join("log.jsonl")).is_ok_and(|t| t.contains("\"kind\":\"read\""))).ok_or("no run log with a read")?
        }
    };
    let file = if run.is_dir() { run.join("log.jsonl") } else { run };
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let game = match game_log {
        Some(path) => {
            let (from, to) = run_span(&text).ok_or("the run log has no reads")?;
            let lines = game_lines(Path::new(path), from - 5_000, to + 5_000).map_err(|e| format!("{path}: {e}"))?;
            Some(game_events(lines.iter().map(String::as_str)))
        }
        None => None,
    };
    out!("{}", file.display());
    print!("{}", report(&text, game.as_ref(), each));
    Ok(ExitCode::SUCCESS)
}

/// `capture [--frames N] <title text> <out.png>`: saves the client area of the
/// first visible window whose title contains the text, as the reader takes it.
/// With `--frames` it grabs N frames through one capture session, as auto read
/// does, and prints the mean time per frame after the first.
fn run_capture(args: &[String]) -> Result<ExitCode, String> {
    let (mut frames, mut rest) = (None, Vec::new());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => frames = Some(number::<usize>("--frames", it.next())?),
            _ => rest.push(arg),
        }
    }
    let [title, out] = rest[..] else {
        return Err(USAGE.to_string());
    };
    let img = match frames {
        None => ascendant::app::capture::capture(title)?,
        Some(n) => {
            let (img, times) = ascendant::app::capture::time_frames(title, n)?;
            let ms = |d: &std::time::Duration| d.as_secs_f64() * 1000.0;
            let after = &times[1..];
            out!("first frame {:.1} ms (opens the session)", ms(&times[0]));
            if !after.is_empty() {
                out!("{} more frames, {:.1} ms per frame (grab and fingerprint)", after.len(), after.iter().map(ms).sum::<f64>() / after.len() as f64);
            }
            img
        }
    };
    save_png(Path::new(out), &img)?;
    out!("{}x{} saved to {out}", img.w, img.h);
    Ok(ExitCode::SUCCESS)
}

/// `bench`: Chromatics and loot per run for the strategy alone and following the
/// advice, on the same seeds; see `ascendant::solver::bench`. With
/// `--strategy-only`, the strategy alone; see `run_strategy_bench`.
fn run_bench(args: &[String]) -> Result<ExitCode, String> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let (mut games, mut rollouts, mut seed, mut threads) = (None, 400usize, 1u32, threads);
    let (mut strategy_only, mut changed, mut rerolls) = (false, None::<Params>, 0u8);
    let (mut held, mut save, mut against) = (Vec::new(), None::<&String>, None::<&String>);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--games" => games = Some(number("--games", it.next())?),
            "--rollouts" => rollouts = number("--rollouts", it.next())?,
            "--seed" => seed = number("--seed", it.next())?,
            "--threads" => threads = number("--threads", it.next())?,
            "--strategy-only" => strategy_only = true,
            "--set" => set_weights(changed.get_or_insert_with(|| TUNED.clone()), it.next())?,
            "--hold" => held = tools(it.next())?,
            "--rerolls" => rerolls = number("--rerolls", it.next())?,
            "--save" => save = Some(it.next().ok_or_else(|| format!("--save needs a file; {USAGE}"))?),
            "--against" => against = Some(it.next().ok_or_else(|| format!("--against needs a file; {USAGE}"))?),
            _ => return Err(format!("unexpected argument {arg}; {USAGE}")),
        }
    }
    if !strategy_only && (changed.is_some() || !held.is_empty() || rerolls > 0 || save.is_some() || against.is_some()) {
        return Err(format!("--set, --hold, --rerolls, --save and --against need --strategy-only; {USAGE}"));
    }
    let games = games.unwrap_or(if strategy_only { 2000 } else { 100 });
    if games == 0 || rollouts == 0 || threads == 0 {
        return Err(format!("--games, --rollouts and --threads must be at least 1; {USAGE}"));
    }
    if strategy_only {
        return run_strategy_bench(games, seed, threads, (&held, rerolls), changed.as_ref(), save, against);
    }
    let clock = std::time::Instant::now();
    let s = summarize(&bench(games, rollouts, seed, threads));
    let secs = clock.elapsed().as_secs_f64();
    out!(
        "strategy  {:.3} +- {:.3} Chromatics, {:.3} +- {:.3} loot per run",
        s.strategy.0,
        s.strategy.1,
        s.strategy_loot.0,
        s.strategy_loot.1
    );
    out!("advice    {:.3} +- {:.3}, {:.3} +- {:.3}", s.advice.0, s.advice.1, s.advice_loot.0, s.advice_loot.1);
    out!("advice - strategy  {:+.3} +- {:.3} loot (paired)", s.gain.0, s.gain.1);
    out!("advice overruled the strategy at {} of {} decisions with 2+ options", s.overruled, s.decisions);
    out!("{games} games, {rollouts} playouts per option, seed {seed}, {threads} threads, {secs:.1} s");
    Ok(ExitCode::SUCCESS)
}

/// `--set name=value,...`: changes strategy weights, by `Params` field name.
fn set_weights(p: &mut Params, value: Option<&String>) -> Result<(), String> {
    let value = value.ok_or_else(|| format!("--set needs name=value pairs; {USAGE}"))?;
    for pair in value.split(',').filter(|s| !s.trim().is_empty()) {
        let (name, x) = pair.split_once('=').ok_or_else(|| format!("--set {pair}: expected name=value"))?;
        let x: f64 = x.trim().parse().map_err(|_| format!("--set {pair}: {x} is not a number"))?;
        *p.weight_mut(name.trim()).ok_or_else(|| format!("--set {pair}: no strategy weight {name}"))? = x;
    }
    Ok(())
}

/// `bench --strategy-only`: Chromatics, loot and moves per run for the strategy on
/// the tuned weights and, given `changed`, on those too, with the paired
/// difference by game. Every game starts holding `held` with `rerolls`
/// instrument rerolls (`start`). `save` writes the tuned Chromatics per game,
/// one per line, and `against` reads such a file from another build and
/// prints the paired difference to it.
fn run_strategy_bench(
    games: u32,
    seed: u32,
    threads: usize,
    start: (&[Tool], u8),
    changed: Option<&Params>,
    save: Option<&String>,
    against: Option<&String>,
) -> Result<ExitCode, String> {
    let (held, rerolls) = start;
    let saved = match against {
        None => None,
        Some(file) => {
            let text = std::fs::read_to_string(file).map_err(|e| format!("--against {file}: {e}"))?;
            let runs = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.trim().parse::<usize>().map_err(|_| format!("--against {file}: {l} is not a Chromatics count")))
                .collect::<Result<Vec<_>, _>>()?;
            if runs.len() != games as usize {
                return Err(format!("--against {file} has {} games, this run {games}", runs.len()));
            }
            Some(runs)
        }
    };
    let clock = std::time::Instant::now();
    let tuned = strategy_bench(games, seed, threads, held, rerolls, &TUNED);
    let row = |name: &str, runs: &[Solo]| {
        let (mean, se) = mean_se(&runs.iter().map(|r| r.chromatics as f64).collect::<Vec<_>>());
        let (loot, loot_se) = mean_se(&runs.iter().map(|r| r.loot).collect::<Vec<_>>());
        let per_run = |f: fn(&Solo) -> u32| runs.iter().map(|r| f64::from(f(r))).sum::<f64>() / runs.len() as f64;
        out!(
            "{name:<8}  {mean:.3} +- {se:.3} Chromatics, {loot:.3} +- {loot_se:.3} loot per run; per run {:.2} Consumes, {:.2} chain bites, {:.2} Replaces, {:.2} doors opened",
            per_run(|r| r.stats.devours),
            per_run(|r| r.stats.chains),
            per_run(|r| r.stats.swaps),
            per_run(|r| r.stats.opened)
        );
        let s = spread(&runs.iter().map(|r| r.chromatics).collect::<Vec<_>>());
        let [p10, p25, p50, p75, p90] = s.percentiles;
        let done = 100.0 * runs.iter().filter(|r| r.objective).count() as f64 / runs.len().max(1) as f64;
        out!(
            "{:<8}  {:.2}% runs with <= {LOW} Chromatics; p10/p25/p50/p75/p90 {p10}/{p25}/{p50}/{p75}/{p90}, max {}; objective done in {done:.2}%",
            "",
            s.low,
            s.max
        );
    };
    row("tuned", &tuned);
    if let Some(file) = save {
        let text: String = tuned.iter().map(|r| format!("{}\n", r.chromatics)).collect();
        std::fs::write(file, text).map_err(|e| format!("--save {file}: {e}"))?;
    }
    if let Some(saved) = &saved {
        let gain = mean_se(&tuned.iter().zip(saved).map(|(t, &s)| t.chromatics as f64 - s as f64).collect::<Vec<_>>());
        out!("tuned - saved  {:+.3} +- {:.3} (paired)", gain.0, gain.1);
    }
    if let Some(p) = changed {
        let runs = strategy_bench(games, seed, threads, held, rerolls, p);
        row("changed", &runs);
        let gain = mean_se(&runs.iter().zip(&tuned).map(|(c, t)| c.chromatics as f64 - t.chromatics as f64).collect::<Vec<_>>());
        out!("changed - tuned  {:+.3} +- {:.3} (paired)", gain.0, gain.1);
        let low = |r: &Solo| if is_low(r.chromatics) { 100.0 } else { 0.0 };
        let fewer = mean_se(&runs.iter().zip(&tuned).map(|(c, t)| low(c) - low(t)).collect::<Vec<_>>());
        out!("changed - tuned  {:+.2} +- {:.2} points of runs with <= {LOW} Chromatics (paired)", fewer.0, fewer.1);
        out!("changed weights: {p:?}");
    }
    let holding = if held.is_empty() {
        String::new()
    } else {
        format!(", holding {}", held.iter().map(|t| t.key()).collect::<Vec<_>>().join(","))
    };
    out!("{games} games, seed {seed}, {threads} threads{holding}, {rerolls} rerolls, {:.1} s", clock.elapsed().as_secs_f64());
    Ok(ExitCode::SUCCESS)
}

/// `vorax-read <frame.png> [--json] [--time] [--page-scale x]`: reads a Vorax screenshot and prints the board
/// (`--json` prints JSON). `--time` also prints the read time: converting to the reference size and reading the
/// screen (including OCR) are timed separately, and the OCR cache is warm on the second read, so both reads are
/// printed. `--page-scale` is the game's HUD `PageScale` the frame was taken at (the `FightCtrl` line of
/// UE_game.log, 0.667 the original setting; see `ascendant::vorax::ui_scale`), in (0, 4] as the game log reader
/// takes it; without it the frame is read as the app reads it while the log has no scale: as at the original
/// setting, and only a frame that reads as no board is measured and, when off the original, read again at the
/// measured scale (`ui_scale::read`). `--list` reads many frames in one process instead ([`vorax_read_list`]).
fn run_vorax_read(args: &[String]) -> Result<ExitCode, String> {
    let (mut file, mut json, mut time, mut page, mut list) = (None, false, false, None, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--time" => time = true,
            "--list" => list = Some(it.next().ok_or_else(|| format!("--list needs a file or -; {USAGE}"))?),
            "--page-scale" => {
                let p = number::<f64>("--page-scale", it.next())?;
                if !(p.is_finite() && p > 0.0 && p <= 4.0) {
                    return Err(format!("--page-scale {p} is outside (0, 4]"));
                }
                page = Some(p);
            }
            // The game client read, `zh` (the default) or `en` (feature `en-vorax`, on by default since 1.1.0).
            #[cfg(feature = "en-vorax")]
            "--client" => match it.next().map(String::as_str) {
                Some("zh") => ascendant::vorax::read::set_english(false),
                Some("en") => ascendant::vorax::read::set_english(true),
                other => return Err(format!("--client takes zh or en, not {other:?}")),
            },
            _ if file.is_none() && !arg.starts_with("--") => file = Some(arg),
            _ => return Err(format!("unexpected argument {arg}; {USAGE}")),
        }
    }
    if let Some(list) = list {
        if file.is_some() || time {
            return Err(format!("--list takes no frame and no --time; {USAGE}"));
        }
        return vorax_read_list(list, page);
    }
    let path = Path::new(file.ok_or(USAGE)?);
    let img = ascendant::reader::load_image_raw(path)?;
    let clock = std::time::Instant::now();
    let reference = ascendant::vorax::read::to_reference(&img);
    let scale_ms = clock.elapsed().as_secs_f64() * 1000.0;
    let ui = page.map(ascendant::vorax::ui_scale::from_page_scale);
    // The same path as the app (`vorax_session::read_frame`), with or without a scale.
    let read = || ascendant::vorax::read::read_scaled(&reference, Some(&img), ui);
    let clock = std::time::Instant::now();
    let reading = read();
    let read_ms = clock.elapsed().as_secs_f64() * 1000.0;
    if time {
        let clock = std::time::Instant::now();
        let _ = read();
        let again_ms = clock.elapsed().as_secs_f64() * 1000.0;
        out!("{}x{}: to_reference {scale_ms:.1} ms, read {read_ms:.1} ms, read again {again_ms:.1} ms", img.w, img.h);
    }
    if json {
        out!("{}", serde_json::to_string_pretty(&reading).map_err(|e| e.to_string())?);
    } else {
        print!("{}", ascendant::vorax::describe(&reading));
    }
    Ok(ExitCode::SUCCESS)
}

/// `vorax-read --list <list.txt | -> [--page-scale x]`: reads every frame of the list (one path a line, `-` reads
/// the list from standard input; a tab after the path may give that frame's page scale, overriding
/// `--page-scale`) in this one process and prints one JSON line a frame, in the list's order:
/// `{"file":<path>,"reading":<the reading vorax-read --json prints>}`, or `{"file":<path>,"error":<why>}` for a
/// frame that does not load or whose read panics, and goes on with the next frame. Launching the tool once a frame
/// made Windows Defender check every start (9-16% CPU over a comparison of thousands of frames, 2026-11-08), so
/// comparisons read in batches. The OCR cache is emptied before each frame, so each reads as in a fresh process: the
/// cache gives a crop alike a cached one the cached text back, which would let one frame's text answer another's.
fn vorax_read_list(list: &str, page: Option<f64>) -> Result<ExitCode, String> {
    use std::io::{Read, Write};
    let text = if list == "-" {
        let mut t = String::new();
        std::io::stdin().read_to_string(&mut t).map_err(|e| format!("standard input: {e}"))?;
        t
    } else {
        std::fs::read_to_string(list).map_err(|e| format!("{list}: {e}"))?
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in text.lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.trim().is_empty()) {
        let (file, own_page) = list_entry(line);
        let name = serde_json::to_string(file).map_err(|e| e.to_string())?;
        // A page scale that does not parse is that line's error; the frames after it are read.
        let own_page = match own_page {
            Ok(p) => p,
            Err(why) => {
                writeln!(out, "{{\"file\":{name},\"error\":{}}}", serde_json::to_string(&why).map_err(|e| e.to_string())?).and_then(|()| out.flush()).map_err(|e| e.to_string())?;
                continue;
            }
        };
        let ui = own_page.or(page).map(ascendant::vorax::ui_scale::from_page_scale);
        ascendant::reader::ocr::clear_cache();
        let read = std::panic::catch_unwind(|| -> Result<String, String> {
            let img = ascendant::reader::load_image_raw(Path::new(file))?;
            let reference = ascendant::vorax::read::to_reference(&img);
            let reading = ascendant::vorax::read::read_scaled(&reference, Some(&img), ui);
            serde_json::to_string(&reading).map_err(|e| e.to_string())
        });
        let row = match read {
            Ok(Ok(reading)) => format!("{{\"file\":{name},\"reading\":{reading}}}"),
            Ok(Err(why)) => format!("{{\"file\":{name},\"error\":{}}}", serde_json::to_string(&why).map_err(|e| e.to_string())?),
            Err(panic) => {
                let why = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                format!("{{\"file\":{name},\"error\":{}}}", serde_json::to_string(&format!("panic: {why}")).map_err(|e| e.to_string())?)
            }
        };
        writeln!(out, "{row}").and_then(|()| out.flush()).map_err(|e| e.to_string())?;
    }
    Ok(ExitCode::SUCCESS)
}

/// One line of a `vorax-read --list`: the frame, and its own page scale when a tab and one follow it (None without),
/// or why that page scale is not one: not a number, or outside (0, 4] as `--page-scale` takes it.
fn list_entry(line: &str) -> (&str, Result<Option<f64>, String>) {
    let Some((file, p)) = line.split_once('\t') else { return (line, Ok(None)) };
    let scale = match p.trim().parse::<f64>() {
        Ok(s) if s.is_finite() && s > 0.0 && s <= 4.0 => Ok(Some(s)),
        Ok(s) => Err(format!("page scale {s} of {file} is outside (0, 4]")),
        Err(e) => Err(format!("page scale {p:?} of {file}: {e}")),
    };
    (file, scale)
}

/// `vorax-learn [vorax folder] [--out file]`: collates the data saved by observe mode (plan section 6.5, see
/// `ascendant::vorax::learn`). The default folder is %APPDATA%\Ascendant\vorax (or the place home.txt points to).
/// Prints a table for humans; `--out` additionally writes JSON that can be pasted directly (names, icon template
/// candidates, success rate per resolution) into a file. It does not write the data files or icons.rs.
fn run_vorax_learn(args: &[String]) -> Result<ExitCode, String> {
    let (mut root, mut out_file) = (None, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => out_file = Some(it.next().ok_or_else(|| format!("--out needs a file; {USAGE}"))?),
            _ if root.is_none() && !arg.starts_with("--") => root = Some(Path::new(arg).to_path_buf()),
            _ => return Err(format!("unexpected argument {arg}; {USAGE}")),
        }
    }
    let root = match root {
        Some(path) => path,
        None => ascendant::app::settings::app_dir().ok_or("APPDATA is not set")?.join("vorax"),
    };
    // Re-running OCR on the card names reads screenshots of the Traditional Chinese client.
    ascendant::lang::set(ascendant::lang::Lang::ZhTw);
    let clock = std::time::Instant::now();
    let learned = ascendant::vorax::learn::scan(&root)?;
    print!("{}", ascendant::vorax::learn::report(&learned, &root));
    if let Some(file) = out_file {
        let icons: Vec<serde_json::Value> = learned.icons.iter().map(|i| serde_json::json!({ "key": i.key, "id": i.id, "card": i.card, "samples": i.samples, "feature": i.feature })).collect();
        let rates: serde_json::Map<String, serde_json::Value> =
            learned.rates.iter().map(|(size, (all, choice, ok))| (size.clone(), serde_json::json!({ "reads": all, "choiceReads": choice, "readable": ok }))).collect();
        let json = serde_json::json!({ "names": ascendant::vorax::learn::paste(&learned), "icons": icons, "rates": rates, "evidence": "(a) 類型字、(b) 效果文字；(c) 依效果重新模擬未做" });
        std::fs::write(file, serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?).map_err(|e| format!("{file}: {e}"))?;
        out!("寫入 {file}");
    }
    out!("{:.1} s", clock.elapsed().as_secs_f64());
    Ok(ExitCode::SUCCESS)
}

/// Default number of rollouts per option for vorax-solve. The advisor in crates/vorax defaults to 64 rollouts (kept
/// as is for parity with upstream, so its constant is not changed). Measured on 7 choice screens on 2026-11-03: at 64
/// and 1024 rollouts some screens change their first place (frames2 t084.00 is only steady from 2048 up); from 2048
/// up all screens are steady except one whose top two differ by 0.1%; 4096 rollouts take 0.43 s at most, 8192 take
/// 0.71 s at most.
const VORAX_ROLLOUTS: u32 = 4096;

/// `vorax-solve <frame.png> [--rollouts N] [--instruments id,id@round]`: reads the screenshot and hands it to the
/// solver in crates/vorax, printing each option's expected score (cap 840k) and the probability of reaching 840k.
/// When the read has an unknown it does not solve and prints the reason (exit code 2). `--instruments` replaces the
/// already-held instruments read from the top-right icons (the icon templates only cover the three seen in tutorial
/// games).
fn run_vorax_solve(args: &[String]) -> Result<ExitCode, String> {
    use vorax::advisor::{ScreenPhase, advise_plan};
    let zh = ascendant::lang::zh();
    let t = |zh_text: &'static str, en: &'static str| if zh { zh_text } else { en };
    let (mut file, mut rollouts, mut held) = (None, VORAX_ROLLOUTS, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--rollouts" => rollouts = number::<u32>("--rollouts", it.next())?,
            "--instruments" => held = Some(ascendant::vorax::parse_instruments(it.next().ok_or_else(|| format!("--instruments needs instrument ids; {USAGE}"))?)?),
            _ if file.is_none() && !arg.starts_with("--") => file = Some(arg),
            _ => return Err(format!("unexpected argument {arg}; {USAGE}")),
        }
    }
    if rollouts == 0 {
        return Err(format!("--rollouts must be at least 1; {USAGE}"));
    }
    // As `ascendant::vorax::read_file`, but a JPEG frame loads too.
    let raw = ascendant::reader::load_image_raw(Path::new(file.ok_or(USAGE)?))?;
    let reading = ascendant::vorax::read::read_with_frame(&ascendant::vorax::read::to_reference(&raw), Some(&raw));
    print!("{}", ascendant::vorax::describe(&reading));
    out!();
    let x = match ascendant::vorax::to_screen(&reading, held.as_ref()) {
        Ok(x) => x,
        Err(why) => {
            out!("{}", t("不給建議：讀到的盤面有未知或對不起來的地方，不用猜的狀態去解。", "No advice: the read has unknown or inconsistent parts, and a guessed board is not solved."));
            for w in &why {
                out!("  - {w}");
            }
            return Ok(ExitCode::from(2));
        }
    };
    if x.phase == ScreenPhase::Protocol {
        out!("{}", t("手術方案只影響終局戰鬥，不影響最終活性：求解器不給建議。", "Surgical Protocols change the endgame battle, not Ultimate Activity: the solver gives no advice."));
        return Ok(ExitCode::SUCCESS);
    }
    let clock = std::time::Instant::now();
    let advice = advise_plan(&x, Some(rollouts), true);
    let ms = clock.elapsed().as_millis();
    let text = ascendant::vorax::advice_rows(&x, &advice);
    if let Some(note) = &text.note {
        out!("{note}");
    }
    out!("{}", text.headline);
    let width = text.rows.iter().map(|r| r.label.chars().count()).max().unwrap_or(0).max(6);
    out!("
{:>4}  {:<width$}  {:>9}  {:>7}", t("排名", "rank"), t("選項", "option"), t("期望分數", "mean"), t("達840k", "840k"));
    for (i, r) in text.rows.iter().enumerate() {
        out!("{:>4}  {:<width$}  {:>9.0}  {:>6.1}%", i + 1, r.label, r.mean, r.reached.unwrap_or(f64::NAN) * 100.0);
    }
    out!(
        "
{} {ms} ms{}",
        text.method,
        if text.unknown.is_empty() { String::new() } else { format!("；{}: {}", t("求解器不認得", "unknown to the solver"), text.unknown.join(", ")) }
    );
    Ok(ExitCode::SUCCESS)
}

fn run_read(args: &[String]) -> Result<ExitCode, String> {
    let (mut file, mut held) = (None, Vec::new());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--tools" => held = tools(it.next())?,
            _ if file.is_none() && !arg.starts_with("--") => file = Some(arg),
            _ => return Err(format!("unexpected argument {arg}; {USAGE}")),
        }
    }
    let (model, warnings) = read_model(Path::new(file.ok_or(USAGE)?), &held)?;
    for w in &warnings {
        eprintln!("{w}");
    }
    out!("{}", serde_json::to_string_pretty(&model).map_err(|e| e.to_string())?);
    Ok(ExitCode::SUCCESS)
}

fn run_solve(args: &[String]) -> Result<ExitCode, String> {
    let (mut file, mut rollouts, mut seed, mut held) = (None, 400usize, 1u32, Vec::new());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--rollouts" => rollouts = number("--rollouts", it.next())?,
            "--seed" => seed = number("--seed", it.next())?,
            "--tools" => held = tools(it.next())?,
            _ if file.is_none() && !arg.starts_with("--") => file = Some(arg),
            _ => return Err(format!("unexpected argument {arg}; {USAGE}")),
        }
    }
    let file = file.ok_or(USAGE)?;
    if rollouts == 0 {
        return Err(format!("--rollouts must be at least 1; {USAGE}"));
    }
    let model: Model = if is_png(file) {
        let (model, warnings) = read_model(Path::new(file), &held)?;
        for w in &warnings {
            out!("{w}");
        }
        if !warnings.is_empty() {
            out!();
        }
        model
    } else {
        let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
        let mut model: Model = serde_json::from_str(&text).map_err(|e| format!("{file} is not JSON: {e}"))?;
        if !held.is_empty() {
            model.tools = held;
        }
        model
    };
    let (st, problems) = build_state(&model, seed);
    if !problems.is_empty() {
        for p in &problems {
            out!("{p}");
        }
        return Ok(ExitCode::from(2));
    }

    match instant(&st) {
        Some(pick) => out!("Pick: {}\n  {}", pick.label, pick.why),
        None => out!("No move: the run is over."),
    }
    let options = solve(&st, rollouts, seed, &TUNED);
    if options.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let width = options.iter().map(|o| o.label.chars().count()).max().unwrap_or(0).max(6);
    out!("\n{:>4}  {:<width$}  {:>6}  {:>6}  {:>5}  {:>5}", "rank", "option", "mean", "gap", "best", "n");
    for (i, o) in options.iter().enumerate() {
        let gap = if i == 0 {
            "-".to_string()
        } else if o.gap_se.is_finite() {
            format!("{:.3}", o.gap_se)
        } else {
            "inf".to_string()
        };
        let mark = if o.policy { " *" } else { "" };
        out!("{:>4}  {:<width$}  {:>6.3}  {:>6}  {:>5.2}  {:>5}{mark}", i + 1, o.label, o.mean, gap, o.best, o.n);
    }
    out!("\nUp to {rollouts} playouts per option, n of them played: a move that clearly trails stops early; gap is the paired standard error of the margin to the leader; * is the strategy's pick.");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod list_tests {
    use super::list_entry;

    /// `vorax-read --list`: a line without a tab is a frame alone; a page scale that is not a number or outside (0, 4]
    /// is that line's error (the batch goes on with the next frame), not the whole run's.
    #[test]
    fn a_list_line_gives_the_frame_and_its_own_page_scale() {
        assert_eq!(list_entry("a.jpg"), ("a.jpg", Ok(None)));
        assert_eq!(list_entry("a.jpg\t0.8"), ("a.jpg", Ok(Some(0.8))));
        assert_eq!(list_entry("a.jpg\t 1.0 "), ("a.jpg", Ok(Some(1.0))));
        for bad in ["a.jpg\tx", "a.jpg\t", "a.jpg\t0", "a.jpg\t5", "a.jpg\tNaN"] {
            let (file, scale) = list_entry(bad);
            assert_eq!(file, "a.jpg");
            assert!(scale.is_err(), "{bad:?}");
        }
    }
}
