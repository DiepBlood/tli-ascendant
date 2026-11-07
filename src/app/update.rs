//! Updates from the GitHub releases of DiepBlood/tli-ascendant: a release is offered only once it has been verified,
//! and installed only when the player presses "Update", which verifies it all again before anything is replaced.
//!
//! The check ([`check`]) reads the latest-release API and, for a newer release, downloads SHA256SUMS.txt and its
//! signature and verifies them; only a release that passes is shown as "New version". A release passes when every
//! one of these holds:
//!
//! - the tag is `vX.Y.Z` and newer than the running version (same or older is a downgrade and refused);
//! - SHA256SUMS.txt carries a valid Ed25519 signature (SHA256SUMS.txt.sig, checked with `verify_strict`) made by the
//!   release signing key whose public half is built in ([`RELEASE_PUBLIC_KEY`]); the private half never leaves the
//!   developer's computer, so a stolen GitHub account cannot publish an update that installs;
//! - the signed SHA256SUMS.txt lists `Ascendant-X.Y.Z-source.zip` for the tag's version (binding the unsigned tag
//!   to the signed content, so an old signed release cannot be replayed under a new tag);
//! - GitHub's own SHA-256 digest of Ascendant.exe equals the signed one, and at download the bytes match both, in
//!   size too;
//! - the release notes shown are the release's RELEASE-NOTES.txt ([`NOTES`]) and nothing else: the signed
//!   SHA256SUMS.txt must list it, its downloaded bytes must match the signed SHA-256, it must be UTF-8 of at most
//!   [`SMALL_LIMIT`] bytes, and it is shown through [`plain_notes`] (at most [`NOTES_LIMIT`] characters, no control
//!   or hidden format characters). The GitHub release body is never read: anyone with the GitHub account could edit it,
//!   so it could tell players something the release key never signed. A release without a valid signed notes file
//!   is refused like any other verification failure;
//! - every request is HTTPS to an allowed GitHub host ([`ALLOWED_HOSTS`]), redirects included, every body is under
//!   its size limit, and one request's time limit counts across all its redirects.
//!
//! Text that came from the network (a tag, a URL, a line of a list) is put in an error message only through
//! [`shown`]: control and hidden format characters removed, at most [`SHOWN_LIMIT`] characters.
//!
//! Only then is the exe written beside the running one as Ascendant.new.exe and swapped in ([`swap`]); the old exe
//! stays as Ascendant.old.exe until the next start deletes it.
//!
//! Keys are never replaced by asking players to download anything by hand: a new key comes in a transition release
//! signed with the old key that carries the new public key, through this same update.
//!
//! While "Check for updates" is on, the check runs at start and then every [`CHECK_EVERY`] ([`next_check`]), with the
//! API's ETag ([`check_tagged`]); a failure or a rate limit waits for the next turn, never retrying sooner. Before an
//! update swaps the exe in, the release's verified notes are kept in the app's folder ([`save_notes`]), so the new
//! version can say what changed without the network ([`load_notes_in`]).

use super::http::{self, Fault, Fetch, Got};
use crate::lang::{tr, trf};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

/// The public repo the releases come from.
pub const REPO: &str = "DiepBlood/tli-ascendant";

/// The latest release, as the GitHub API gives it.
const LATEST: &str = "https://api.github.com/repos/DiepBlood/tli-ascendant/releases/latest";

/// The release's exe asset (the name the release script uploads).
const UPSTREAM_ASSET: &str = "Ascendant.exe";

/// The signed list of SHA-256 sums, and its detached Ed25519 signature (64 raw bytes).
pub const SUMS: &str = "SHA256SUMS.txt";
pub const SIG: &str = "SHA256SUMS.txt.sig";

/// The release notes asset, listed in the signed SHA256SUMS.txt; the only source of the notes shown.
pub const NOTES: &str = "RELEASE-NOTES.txt";

/// Hosts a request may go to. api.github.com answers the release query; github.com serves the asset links, which
/// redirect to GitHub's download hosts: objects.githubusercontent.com, and release-assets.githubusercontent.com,
/// which GitHub has used for release assets since 2025. A redirect anywhere else is refused.
pub const ALLOWED_HOSTS: [&str; 4] = ["api.github.com", "github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"];

/// Size limits: the API's JSON, the exe, and SHA256SUMS.txt, its signature and RELEASE-NOTES.txt.
const API_LIMIT: usize = 1 << 20;
pub const EXE_LIMIT: usize = 50 << 20;
pub const SMALL_LIMIT: usize = 64 << 10;

/// At most this much of the release notes is kept.
const NOTES_LIMIT: usize = 8000;

/// The value [`RELEASE_PUBLIC_KEY`] holds until a real key is put in: all zeros, a small-order point that
/// `verify_strict` never accepts, and that [`has_release_key`] and the release script refuse outright.
pub const PLACEHOLDER_PUBLIC_KEY: [u8; 32] = [0; 32];

/// The release signing key's public half (Ed25519, 32 bytes as 64 lowercase hex digits).
///
/// The real release key, made with `ascendant-cli keygen` on 2026-11-06; its fingerprint
/// (924d 51f6 7734 fbb4 dad1 1091 77ee 6bd5) is published in SECURITY.md and README.public.md. To change keys, follow
/// "換金鑰" in README.md: a transition release signed with the previous key carries the new line.
pub const RELEASE_PUBLIC_KEY: [u8; 32] = key_from_hex("077c85c76ad4f0ea9998086e5e9ea71b9069e6f7f1f74b949e5adc9fcd64dfed");

/// 64 lowercase hex digits as 32 bytes, at compile time (a malformed key does not build).
pub const fn key_from_hex(hex: &str) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("the release public key must be lowercase hex"),
        }
    }
    let b = hex.as_bytes();
    assert!(b.len() == 64, "the release public key must be 64 hex digits");
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nibble(b[2 * i]) << 4) | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

/// Whether `key` can verify anything: not the placeholder, a valid point, and not of small order.
pub fn usable_key(key: &[u8; 32]) -> bool {
    *key != PLACEHOLDER_PUBLIC_KEY && VerifyingKey::from_bytes(key).is_ok_and(|k| !k.is_weak())
}

/// Whether this build has a real release key (and so can update at all).
pub fn has_release_key() -> bool {
    usable_key(&RELEASE_PUBLIC_KEY)
}

/// The key's fingerprint for people to compare: the first 32 hex digits (128 bits) of the SHA-256 of the 32 key
/// bytes, lowercase, in eight groups of four separated by single spaces. 128 bits, so that finding another key
/// with the same fingerprint is out of reach (64 bits would be within reach of a well-funded attacker).
pub fn key_fingerprint(key: &[u8; 32]) -> String {
    let digest = hex(&Sha256::digest(key));
    digest.as_bytes()[..32].chunks(4).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect::<Vec<_>>().join(" ")
}

/// Restricts where this process loads DLLs from by name to System32, before anything has loaded one. Call it first
/// thing in `main` (both Ascendant and ascendant-cli do).
///
/// Players often start Ascendant.exe from their Downloads folder, where anyone's files may sit beside it. Windows'
/// default search for `LoadLibrary("x.dll")` looks in the exe's folder first, so a planted d3dcompiler_47.dll or
/// dxcompiler.dll (which wgpu's DX12 backend asks for by name, dxcompiler.dll first, falling back to FXC when it is
/// not found) would run inside Ascendant. The application directory is therefore NOT in the search: Ascendant
/// ships as one exe with no DLLs of its own, and everything it loads by name (d3d12, dxgi, dcomp,
/// d3dcompiler_47, the WinRT OCR and capture components) is a Windows DLL in System32; COM and WinRT load their
/// servers by full path, which this does not affect. Without the app folder, wgpu does not find a dxcompiler.dll
/// there and uses FXC from System32. SetDllDirectoryW("") also takes the current directory out of the legacy
/// search, in case SetDefaultDllDirectories is refused.
///
/// This governs only the loads by name made after `main` starts; what is resolved before `main` is kept to System32
/// by the build instead:
/// - The C runtime is linked in (`+crt-static`, .cargo/config.toml), so there is no VCRUNTIME140.dll,
///   VCRUNTIME140_1.dll or api-ms-win-crt-* to plant.
/// - `/DEPENDENTLOADFLAG:0x800` (build.rs) makes the loader take the exe's own imports (dwmapi, uxtheme, dxgi, d3d11,
///   bcryptprimitives... not KnownDLLs) from System32 only.
/// - bcrypt.dll, which mimalloc loads by name while the C runtime initialises, is one of those imports
///   ([`BCRYPT_IMPORT`] below puts it in both exes' import tables): it is loaded from System32 before the C runtime
///   starts, and mimalloc's load by name then returns that module. This no longer rests on which Windows DLLs happen
///   to import bcrypt.dll first.
///
/// Measured 2026-11-05 on Windows 10 22H2, with copies of 50 System32 DLLs (every one these processes load that is
/// not a KnownDLL) planted beside the exes: Ascendant.exe loaded none of them from its folder (before the change:
/// VCRUNTIME140, VCRUNTIME140_1, d3d11, dxgi, dwmapi, uxtheme). One gap remains: the flag covers only the exe's own
/// imports, and the imports of those DLLs follow each DLL's own flags. d3d11.dll (flags 0) imports dxgi.dll by name,
/// so ascendant-cli.exe, which imports d3d11 but not dxgi, still loaded a dxgi.dll planted beside it; it is run only
/// from its build folder (README.md, release signing key). The rest depends on Windows' own import graph, which can
/// change, so "Verify downloaded files..." still warns about other .dll and .exe files in the folder
/// ([`Verified::warnings`]).
///
/// Err when SetDefaultDllDirectories is refused (the search then is not restricted); the callers report it and go
/// on, since the program still works without the restriction.
pub fn restrict_dll_search() -> Result<(), String> {
    use windows::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories, SetDllDirectoryW};
    use windows::core::w;
    // Only takes the function's address (never calls it), so the linker must keep bcrypt.dll in the import table.
    std::hint::black_box(BCRYPT_IMPORT);
    // SAFETY: both calls only change this process's DLL search settings and take no pointers that outlive them.
    let (legacy, default) = unsafe { (SetDllDirectoryW(w!("")), SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)) };
    let Err(e) = default else { return Ok(()) };
    let mut why = e.message();
    if let Err(l) = legacy {
        why = format!("{why}; {}", l.message());
    }
    Err(trf!("Could not restrict the DLL search to System32: {}", why))
}

#[link(name = "bcrypt", kind = "raw-dylib")]
unsafe extern "system" {
    fn BCryptGenRandom(algorithm: *mut std::ffi::c_void, buffer: *mut u8, len: u32, flags: u32) -> i32;
}

/// BCryptGenRandom's address, never called: it makes bcrypt.dll a static import of every exe that calls
/// [`restrict_dll_search`] (both mains do), so the loader takes it from System32 under the exe's DependentLoadFlags
/// (0x800, build.rs) before mimalloc asks for "bcrypt.dll" by name while the C runtime initialises.
const BCRYPT_IMPORT: unsafe extern "system" fn(*mut std::ffi::c_void, *mut u8, u32, u32) -> i32 = BCryptGenRandom;

/// Exe name after installation (it was warden.exe before the rename).
pub(crate) const EXE: &str = "Ascendant.exe";

/// The name a downloaded exe takes beside the running one until it is swapped in.
pub(crate) const NEW: &str = "Ascendant.new.exe";

/// The name the replaced exe takes until the next start deletes it.
const OLD: &str = "Ascendant.old.exe";

/// Old exe name left behind when a pre-rename version updates; also cleaned up at startup.
const LEGACY_OLD: &str = "warden.old.exe";

/// Auto-update switch: on since the public releases of DiepBlood/tli-ascendant are signed (2026-11-05). A build
/// with the placeholder key still refuses every update, see [`has_release_key`].
pub const ENABLED: bool = true;

/// Why a release was not offered or not installed.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    /// This build has the placeholder key and cannot verify anything.
    NoKey,
    /// GitHub could not be reached or answered with an error: try again later.
    Network(String),
    /// The release broke a rule (signature, hash, version, host, size...): it must not be installed.
    Unverified(String),
    /// Files to verify by hand are missing or unreadable.
    Files(String),
}

impl Problem {
    /// The line shown: for a check (`checking`) or a download.
    pub fn headline(&self, checking: bool) -> String {
        match self {
            Problem::NoKey => tr("This build has no release signing key, so it cannot verify or install updates.").to_string(),
            Problem::Network(_) if checking => tr("Can't check right now. Try again later.").to_string(),
            Problem::Network(_) => tr("Can't download right now. Try again later.").to_string(),
            Problem::Unverified(_) => tr("This version failed verification. Do not install it.").to_string(),
            Problem::Files(e) => e.clone(),
        }
    }

    /// The details, for a tooltip.
    pub fn detail(&self) -> &str {
        match self {
            Problem::NoKey | Problem::Files(_) => "",
            Problem::Network(d) | Problem::Unverified(d) => d,
        }
    }
}

impl From<Fault> for Problem {
    fn from(f: Fault) -> Problem {
        match f {
            Fault::Network(d) | Fault::Throttled(d, _) => Problem::Network(d),
            Fault::Refused(d) => Problem::Unverified(d),
        }
    }
}

/// How an update that was started ended, when it did not end in the restart.
#[derive(Clone, Debug, PartialEq)]
pub enum Failed {
    /// Downloading or verifying failed; nothing was changed.
    Problem(Problem),
    /// Writing or swapping in the new exe failed and the running exe is where it was.
    Kept(String),
    /// The swap failed and putting the old exe back failed too: Ascendant.exe may be missing, and the previous
    /// version is at `old`.
    Lost { old: PathBuf, error: String },
    /// The new exe is in place but could not be started.
    NotRestarted(String),
}

impl Failed {
    /// The line shown.
    pub fn text(&self) -> String {
        match self {
            Failed::Problem(p) => p.headline(false),
            Failed::Kept(e) => trf!("Update failed: {}. The current version is kept.", e),
            Failed::Lost { old, error } => trf!("Update failed ({}) and Ascendant.exe may be missing. The previous version is {}: rename it to Ascendant.exe to start it.", error, old.display()),
            Failed::NotRestarted(e) => trf!("The new version is installed but did not start ({}). Start Ascendant again.", e),
        }
    }

    /// The details, for a tooltip.
    pub fn detail(&self) -> &str {
        match self {
            Failed::Problem(p) => p.detail(),
            _ => "",
        }
    }
}

/// One release asset, as the API lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub url: String,
    pub size: u64,
    /// GitHub's SHA-256 of the asset ("digest": "sha256:..."), when it gives one.
    pub digest: Option<[u8; 32]>,
}

/// A release on GitHub, with the assets an update needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: [u64; 3],
    /// The release notes as plain text, from the signed RELEASE-NOTES.txt ([`NOTES`]) through [`plain_notes`];
    /// empty until [`check_with`] has verified them (never the GitHub release body).
    pub notes: String,
    pub exe: Asset,
    pub sums: Asset,
    pub sig: Asset,
    /// The SHA-256 of Ascendant.exe in the verified, signed SHA256SUMS.txt; None until verified.
    pub signed: Option<[u8; 32]>,
}

impl Release {
    /// "X.Y.Z".
    pub fn version_text(&self) -> String {
        let [a, b, c] = self.version;
        format!("{a}.{b}.{c}")
    }
}

/// A strict release version "X.Y.Z": three decimal numbers without leading zeros, nothing else (no pre-release or
/// build suffix).
pub fn semver(text: &str) -> Option<[u64; 3]> {
    let mut parts = text.split('.');
    let mut v = [0u64; 3];
    for slot in &mut v {
        let p = parts.next()?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
            return None;
        }
        *slot = p.parse().ok()?;
    }
    parts.next().is_none().then_some(v)
}

/// The version of a release tag, which must be "vX.Y.Z".
pub fn tag_version(tag: &str) -> Option<[u64; 3]> {
    semver(tag.strip_prefix('v')?)
}

/// Whether `candidate` is newer than `running` ("X.Y.Z"); false when `running` does not parse.
pub fn is_newer(candidate: [u64; 3], running: &str) -> bool {
    semver(running).is_some_and(|r| candidate > r)
}

/// 64 hex digits as 32 bytes.
pub fn hex32(text: &str) -> Option<[u8; 32]> {
    // Hex digits only: from_str_radix alone would also take a sign such as "+f".
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(bytes)
}

/// Bytes as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Text direction controls (U+061C, U+200E/200F, U+202A-202E, U+2066-2069), which could make text read differently
/// from what it is.
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Characters that take no visible space but change how text reads or where it breaks: the direction controls of
/// [`is_bidi_control`], and the soft hyphen (U+00AD), U+180E, the zero-width characters (U+200B-200D), the line and
/// paragraph separators (U+2028, U+2029), the rest of U+202A-202E, U+2060-206F (word joiner, invisible operators,
/// isolates), the byte order mark anywhere (U+FEFF), the interlinear annotation characters (U+FFF9-FFFB), the tag
/// characters (U+E0000-E007F), and the other format characters that join, prefix or mark the text after them: the
/// Arabic number signs (U+0600-0605, U+06DD, U+08E2), the Syriac abbreviation mark (U+070F), the Kaithi number signs
/// (U+110BD, U+110CD), the shorthand format controls (U+1BCA0-1BCA3) and the musical beam and slur marks
/// (U+1D173-1D17A). scripts/export-public.ps1 refuses release notes with any of these (its `$NotesHidden` is the same
/// set, which tests/notes_hidden_set.rs checks).
pub(crate) fn is_hidden_format(c: char) -> bool {
    is_bidi_control(c)
        || matches!(
            c,
            '\u{00AD}'
                | '\u{0600}'..='\u{0605}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{08E2}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// The most characters of network text [`shown`] keeps, the ellipsis included.
pub const SHOWN_LIMIT: usize = 100;

/// Text from the network (a tag, a URL, a file name) made safe to put in a message: control characters and hidden
/// format characters ([`is_hidden_format`]) removed, and cut to [`SHOWN_LIMIT`] characters with an ellipsis when
/// longer.
pub fn shown(text: &str) -> String {
    let clean: Vec<char> = text.chars().filter(|&c| !c.is_control() && !is_hidden_format(c)).collect();
    if clean.len() <= SHOWN_LIMIT {
        return clean.into_iter().collect();
    }
    clean[..SHOWN_LIMIT - 1].iter().chain(['\u{2026}'].iter()).collect()
}

/// Release notes as plain text: CR LF made LF, hidden format characters ([`is_hidden_format`], a byte order mark
/// included) and control characters other than line feeds and tabs dropped, trimmed, at most [`NOTES_LIMIT`]
/// characters.
pub fn plain_notes(body: &str) -> String {
    body.replace("\r\n", "\n").chars().filter(|&c| !is_hidden_format(c) && (!c.is_control() || c == '\n' || c == '\t')).collect::<String>().trim().chars().take(NOTES_LIMIT).collect()
}

/// The release in the latest-release API's JSON, and its RELEASE-NOTES.txt asset, with the reason when something an
/// update needs is missing. The release's notes stay empty: they come only from the signed notes file, which
/// [`check_with`] verifies (the release body is ignored).
pub fn parse_release(json: &str) -> Result<(Release, Asset), String> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("{LATEST}: {e}"))?;
    let tag = root["tag_name"].as_str().unwrap_or_default();
    let version = tag_version(tag).ok_or_else(|| trf!("the release tag {} is not vX.Y.Z", shown(tag)))?;
    let assets = root["assets"].as_array().map(Vec::as_slice).unwrap_or_default();
    let asset = |name: &str| -> Result<Asset, String> {
        let a = assets.iter().find(|a| a["name"] == name).ok_or_else(|| trf!("the release has no {}", name))?;
        Ok(Asset {
            url: a["browser_download_url"].as_str().ok_or_else(|| trf!("the release has no {}", name))?.to_string(),
            size: a["size"].as_u64().ok_or_else(|| trf!("the release has no {}", name))?,
            digest: a["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).and_then(hex32),
        })
    };
    let exe = asset(UPSTREAM_ASSET)?;
    if exe.digest.is_none() {
        return Err(trf!("GitHub gives no SHA-256 digest for {}", UPSTREAM_ASSET));
    }
    let notes = asset(NOTES)?;
    Ok((Release { version, notes: String::new(), exe, sums: asset(SUMS)?, sig: asset(SIG)?, signed: None }, notes))
}

/// The first `lines` lines of `notes`, and whether there was more.
pub fn notes_preview(notes: &str, lines: usize) -> (String, bool) {
    let all: Vec<&str> = notes.lines().collect();
    (all.iter().take(lines).copied().collect::<Vec<_>>().join("\n"), all.len() > lines)
}

/// Ok when `sig` (64 raw bytes) is `key`'s Ed25519 signature of `sums`, by `verify_strict`.
pub fn verify_sums(sums: &[u8], sig: &[u8], key: &[u8; 32]) -> Result<(), String> {
    if !usable_key(key) {
        return Err(tr("This build has no release signing key, so it cannot verify or install updates.").to_string());
    }
    let sig: [u8; 64] = sig.try_into().map_err(|_| trf!("{} is {} bytes, not 64", SIG, sig.len()))?;
    let key = VerifyingKey::from_bytes(key).map_err(|_| tr("the built-in release key is not valid").to_string())?;
    key.verify_strict(sums, &Signature::from_bytes(&sig)).map_err(|_| signature_mismatch())
}

/// The reason given when SHA256SUMS.txt's signature does not check out.
pub fn signature_mismatch() -> String {
    trf!("the signature in {} does not match {}: the release was not signed with Ascendant's release key", SIG, SUMS)
}

/// The lines of a SHA256SUMS.txt: `<64 hex digits>  <file name>` (or ` *<file name>`); blank lines are skipped and
/// any other line is an error.
pub fn parse_sums(sums: &[u8]) -> Result<Vec<(String, [u8; 32])>, String> {
    let text = std::str::from_utf8(sums).map_err(|_| trf!("{} is not text", SUMS))?;
    let mut out = Vec::new();
    for line in text.lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.trim().is_empty()) {
        let bad = || trf!("{} has a malformed line: {}", SUMS, shown(line));
        let (hash, name) = line.split_at_checked(64).ok_or_else(bad)?;
        let name = name.strip_prefix("  ").or_else(|| name.strip_prefix(" *")).filter(|n| !n.is_empty()).ok_or_else(bad)?;
        out.push((name.to_string(), hex32(hash).ok_or_else(bad)?));
    }
    Ok(out)
}

/// The one sum listed for `name`; an error when it is missing or listed twice.
pub fn sum_of(sums: &[(String, [u8; 32])], name: &str) -> Result<[u8; 32], String> {
    let mut found = sums.iter().filter(|(n, _)| n == name);
    match (found.next(), found.next()) {
        (Some((_, sum)), None) => Ok(*sum),
        (Some(_), Some(_)) => Err(trf!("{} lists {} twice", SUMS, name)),
        (None, _) => Err(trf!("{} does not list {}", SUMS, name)),
    }
}

/// The source zip the release script lists in SHA256SUMS.txt for `version`.
fn source_zip(version: [u64; 3]) -> String {
    let [a, b, c] = version;
    format!("Ascendant-{a}.{b}.{c}-source.zip")
}

/// The version a signed list is for: the X.Y.Z of the one `Ascendant-X.Y.Z-source.zip` it lists.
fn listed_version(listed: &[(String, [u8; 32])]) -> Option<[u64; 3]> {
    let mut versions = listed.iter().filter_map(|(n, _)| n.strip_prefix("Ascendant-")?.strip_suffix("-source.zip").and_then(semver));
    match (versions.next(), versions.next()) {
        (Some(v), None) => Some(v),
        _ => None,
    }
}

/// The SHA-256 sums a verified SHA256SUMS.txt signs for the files the app uses.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Signed {
    exe: [u8; 32],
    notes: [u8; 32],
}

/// Checks a release's SHA256SUMS.txt and signature against `key` and the release (version and GitHub's digest);
/// the signed SHA-256 of Ascendant.exe and of RELEASE-NOTES.txt, which must both be listed.
fn verify_release_files(release: &Release, sums: &[u8], sig: &[u8], key: &[u8; 32]) -> Result<Signed, Problem> {
    verify_sums(sums, sig, key).map_err(Problem::Unverified)?;
    let listed = parse_sums(sums).map_err(Problem::Unverified)?;
    let signed = sum_of(&listed, UPSTREAM_ASSET).map_err(Problem::Unverified)?;
    let notes = sum_of(&listed, NOTES).map_err(Problem::Unverified)?;
    let zip = source_zip(release.version);
    if sum_of(&listed, &zip).is_err() || listed_version(&listed) != Some(release.version) {
        return Err(Problem::Unverified(trf!("the signed {} is not for version {} (it does not list {})", SUMS, release.version_text(), zip)));
    }
    if release.exe.digest != Some(signed) {
        return Err(Problem::Unverified(trf!("GitHub's SHA-256 of {} differs from the signed {}", UPSTREAM_ASSET, SUMS)));
    }
    Ok(Signed { exe: signed, notes })
}

/// Downloads the release's SHA256SUMS.txt and signature through `fetch` and verifies them (see
/// [`verify_release_files`]).
fn fetch_signed(fetch: &dyn Fetch, release: &Release, key: &[u8; 32]) -> Result<Signed, Problem> {
    if release.exe.size > EXE_LIMIT as u64 || release.sums.size > SMALL_LIMIT as u64 || release.sig.size > SMALL_LIMIT as u64 {
        return Err(Problem::Unverified(tr("a release file is larger than the size limit").to_string()));
    }
    let sums = http::get_limited(fetch, &release.sums.url, "application/octet-stream", SMALL_LIMIT, &ALLOWED_HOSTS)?;
    let sig = http::get_limited(fetch, &release.sig.url, "application/octet-stream", SMALL_LIMIT, &ALLOWED_HOSTS)?;
    verify_release_files(release, &sums, &sig, key)
}

/// The notes in a RELEASE-NOTES.txt whose SHA-256 must be `signed`: UTF-8, through [`plain_notes`].
fn signed_notes(bytes: &[u8], signed: &[u8; 32]) -> Result<String, Problem> {
    if Sha256::digest(bytes)[..] != signed[..] {
        return Err(Problem::Unverified(trf!("the downloaded {} does not match the signed SHA-256", NOTES)));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Problem::Unverified(trf!("{} is not UTF-8 text", NOTES)))?;
    Ok(plain_notes(text))
}

/// Downloads the release's RELEASE-NOTES.txt (`asset`) through `fetch` and checks it against its `signed` SHA-256.
fn fetch_notes(fetch: &dyn Fetch, asset: &Asset, signed: &[u8; 32]) -> Result<String, Problem> {
    if asset.size > SMALL_LIMIT as u64 {
        return Err(Problem::Unverified(tr("a release file is larger than the size limit").to_string()));
    }
    let bytes = http::get_limited(fetch, &asset.url, "application/octet-stream", SMALL_LIMIT, &ALLOWED_HOSTS)?;
    signed_notes(&bytes, signed)
}

/// The latest release through `fetch` when it is newer than `running` and its signed files verify against `key`,
/// with the notes of its signed RELEASE-NOTES.txt. None when there is no newer release; an error, and nothing to
/// offer, when the newer one does not verify.
pub fn check_with(fetch: &dyn Fetch, running: &str, key: &[u8; 32]) -> Result<Option<Release>, Problem> {
    check_tagged(fetch, running, key, None).result
}

/// The release in the latest-release API's `json` when it is newer than `running`, verified as [`check_with`] says.
fn verify_latest(fetch: &dyn Fetch, json: &[u8], running: &str, key: &[u8; 32]) -> Result<Option<Release>, Problem> {
    let (mut release, notes) = parse_release(&String::from_utf8_lossy(json)).map_err(Problem::Unverified)?;
    if !is_newer(release.version, running) {
        return Ok(None);
    }
    let signed = fetch_signed(fetch, &release, key)?;
    release.notes = fetch_notes(fetch, &notes, &signed.notes)?;
    release.signed = Some(signed.exe);
    Ok(Some(release))
}

/// The last check that read the latest-release API and verified its answer: the API's ETag and what the check made
/// of it, so the next check can ask with `If-None-Match` and take this outcome again on a 304 (which GitHub does not
/// count against the unauthenticated rate limit). Kept only in memory.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    pub etag: String,
    pub release: Option<Release>,
}

/// One update check: its outcome, the [`Seen`] to send with the next check, and how long GitHub asked to wait
/// (403 or 429 with Retry-After or X-RateLimit-Reset).
#[derive(Clone, Debug, PartialEq)]
pub struct Checked {
    pub result: Result<Option<Release>, Problem>,
    pub seen: Option<Seen>,
    pub wait: Option<std::time::Duration>,
}

/// [`check_with`] as a conditional request: with `seen`, the API is asked with `If-None-Match`, and a 304 gives
/// `seen`'s outcome again without parsing or fetching anything more. A failed check keeps `seen` (it still pairs that
/// ETag with that outcome); a verified answer replaces it (or drops it, when the API gave no ETag).
pub fn check_tagged(fetch: &dyn Fetch, running: &str, key: &[u8; 32], seen: Option<Seen>) -> Checked {
    let failed = |problem: Problem, seen: Option<Seen>, wait| Checked { result: Err(problem), seen, wait };
    if !usable_key(key) {
        return failed(Problem::NoKey, seen, None);
    }
    let asked = seen.as_ref().map(|s| s.etag.as_str());
    let (json, etag) = match http::get_conditional(fetch, LATEST, "application/vnd.github+json", API_LIMIT, &ALLOWED_HOSTS, asked) {
        Ok(Got::NotModified) => {
            return match seen {
                Some(s) => Checked { result: Ok(s.release.clone()), seen: Some(s), wait: None },
                None => failed(Problem::Network(format!("{LATEST}: HTTP 304")), None, None),
            };
        }
        Ok(Got::Body { body, etag }) => (body, etag),
        Err(fault) => {
            let wait = fault.wait();
            return failed(fault.into(), seen, wait);
        }
    };
    match verify_latest(fetch, &json, running, key) {
        Ok(release) => Checked { seen: etag.map(|etag| Seen { etag, release: release.clone() }), result: Ok(release), wait: None },
        Err(problem) => failed(problem, seen, None),
    }
}

/// The latest release when it is newer than the running app and verified, asked with `seen`'s ETag (see
/// [`check_tagged`]). Does not touch the network in a build without a release key.
pub fn check(seen: Option<Seen>) -> Checked {
    if !ENABLED {
        return Checked { result: Ok(None), seen: None, wait: None };
    }
    check_tagged(&http::WinHttp, env!("CARGO_PKG_VERSION"), &RELEASE_PUBLIC_KEY, seen)
}

/// How often a running Ascendant asks GitHub again while "Check for updates" is on (after the check at start).
pub const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// When to check next after a check that ended at `now`, however it ended: [`CHECK_EVERY`] later, or later still
/// when GitHub asked to wait longer (`wait`, at most [`http::LONGEST_WAIT`]). Nothing is retried sooner, a failure
/// included.
pub fn next_check(now: std::time::Instant, wait: Option<std::time::Duration>) -> std::time::Instant {
    now + wait.map_or(CHECK_EVERY, |w| w.min(http::LONGEST_WAIT).max(CHECK_EVERY))
}

/// Whether the periodic check is due at `now`: checking is `on`, a time was set (`due`) and has come, and no
/// check or update is running (`busy`).
pub fn check_due(on: bool, now: std::time::Instant, due: Option<std::time::Instant>, busy: bool) -> bool {
    on && !busy && due.is_some_and(|d| now >= d)
}

/// How long until the window must wake up for the periodic check: only while it is `on`, not `busy` and `due` is
/// still ahead. Never zero: a check that is due while busy waits for the busy thread's own repaint when it ends
/// (a zero delay would repaint every frame meanwhile).
pub fn wake_in(on: bool, now: std::time::Instant, due: Option<std::time::Instant>, busy: bool) -> Option<std::time::Duration> {
    due.filter(|d| on && !busy && *d > now).map(|d| d - now)
}

/// `answer` with a release of the version a download refused as unverified (`refused`, kept in memory until
/// Ascendant restarts) taken as no newer release, and its ETag dropped, so neither a 304 nor the same metadata
/// passing the check again offers it once more.
pub fn drop_refused(answer: Checked, refused: Option<[u64; 3]>) -> Checked {
    match &answer.result {
        Ok(Some(r)) if Some(r.version) == refused => Checked { result: Ok(None), seen: None, wait: answer.wait },
        _ => answer,
    }
}

/// The file in the app's folder that keeps the notes of the version an update installed.
pub const SAVED_NOTES: &str = "release-notes.json";

/// The verified notes of the release an update installed, kept in [`SAVED_NOTES`] so the new version can show
/// them ("Updated to X.Y.Z") without the network.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SavedNotes {
    /// "X.Y.Z".
    pub version: String,
    /// The notes as [`plain_notes`] gave them.
    pub notes: String,
}

/// Writes `release`'s version and verified notes to [`SAVED_NOTES`] in `dir` (through a temporary file, so a
/// failure leaves no half-written file behind under that name).
pub fn save_notes_in(dir: &Path, release: &Release) -> Result<(), String> {
    let saved = SavedNotes { version: release.version_text(), notes: release.notes.clone() };
    let text = serde_json::to_string_pretty(&saved).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (path, part) = (dir.join(SAVED_NOTES), dir.join(format!("{SAVED_NOTES}.part")));
    std::fs::write(&part, text).map_err(|e| format!("{}: {e}", part.display()))?;
    std::fs::rename(&part, &path).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        format!("{}: {e}", path.display())
    })
}

/// [`save_notes_in`] the app's folder; errors are dropped: keeping the notes must never stop an update.
pub fn save_notes(release: &Release) {
    if let Some(dir) = super::settings::app_dir() {
        let _ = save_notes_in(&dir, release);
    }
}

/// The notes in [`SAVED_NOTES`] in `dir` when they are for `running` ("X.Y.Z"). Anything wrong means none: a file
/// over [`SMALL_LIMIT`] bytes, not JSON of a [`SavedNotes`], a version that is not X.Y.Z, notes over
/// [`NOTES_LIMIT`] characters (the release script's limit too) or empty once through [`plain_notes`], the same
/// filter as a release's notes.
pub fn load_notes_in(dir: &Path, running: &str) -> Option<SavedNotes> {
    let bytes = read_limited(&dir.join(SAVED_NOTES), SMALL_LIMIT).ok()?;
    let saved: SavedNotes = serde_json::from_slice(&bytes).ok()?;
    if semver(&saved.version).is_none() || saved.version != running || saved.notes.chars().count() > NOTES_LIMIT {
        return None;
    }
    let notes = plain_notes(&saved.notes);
    (!notes.is_empty()).then_some(SavedNotes { version: saved.version, notes })
}

/// Whether "Updated to X.Y.Z" shows: the saved notes are for the `running` version and that version is not the one
/// the player already dismissed (`seen`, by its "x" or by playing a game since).
pub fn show_updated(saved: Option<&SavedNotes>, running: &str, seen: &str) -> bool {
    saved.is_some_and(|s| s.version == running && s.version != seen)
}

/// Where an update is, for the window to show; stored in an [`AtomicU8`] the update's thread sets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Step {
    Signature = 0,
    Download = 1,
    Verify = 2,
    Restart = 3,
}

impl Step {
    pub fn load(at: &AtomicU8) -> Step {
        match at.load(Ordering::Relaxed) {
            0 => Step::Signature,
            1 => Step::Download,
            2 => Step::Verify,
            _ => Step::Restart,
        }
    }

    pub fn set(self, at: &AtomicU8) {
        at.store(self as u8, Ordering::Relaxed);
    }

    /// What the window says during this step (English key of the translation table).
    pub fn text(self) -> &'static str {
        match self {
            Step::Signature => "Checking the release signature...",
            Step::Download => "Downloading Ascendant.exe...",
            Step::Verify => "Verifying the download...",
            Step::Restart => "Restarting...",
        }
    }
}

/// The release's exe, through `fetch`, only once everything checks out again (see the module docs); never
/// written anywhere by this function.
pub fn download_with(fetch: &dyn Fetch, release: &Release, running: &str, key: &[u8; 32], step: &AtomicU8) -> Result<Vec<u8>, Problem> {
    if !usable_key(key) {
        return Err(Problem::NoKey);
    }
    if !is_newer(release.version, running) {
        return Err(Problem::Unverified(trf!("{} is not newer than the running {}: downgrades are refused", release.version_text(), running)));
    }
    Step::Signature.set(step);
    let signed = fetch_signed(fetch, release, key)?.exe;
    Step::Download.set(step);
    let bytes = http::get_limited(fetch, &release.exe.url, "application/octet-stream", EXE_LIMIT, &ALLOWED_HOSTS)?;
    Step::Verify.set(step);
    if bytes.len() as u64 != release.exe.size {
        return Err(Problem::Unverified(trf!("{}: {} bytes downloaded, the release has {}", UPSTREAM_ASSET, bytes.len(), release.exe.size)));
    }
    if Sha256::digest(&bytes)[..] != signed {
        return Err(Problem::Unverified(trf!("the downloaded {} does not match the signed SHA-256", UPSTREAM_ASSET)));
    }
    Ok(bytes)
}

/// Downloads and verifies the release's exe (see [`download_with`]) and only then writes it into `dir` as
/// Ascendant.new.exe.
pub fn download(release: &Release, dir: &Path, step: &AtomicU8) -> Result<PathBuf, Failed> {
    let bytes = download_with(&http::WinHttp, release, env!("CARGO_PKG_VERSION"), &RELEASE_PUBLIC_KEY, step).map_err(Failed::Problem)?;
    let path = dir.join(NEW);
    std::fs::write(&path, bytes).map_err(|e| Failed::Kept(format!("{}: {e}", path.display())))?;
    Ok(path)
}

/// Puts `new` in place of `current`, saying what state a failure left. Windows renames a running exe but does not
/// overwrite it, so `current` becomes Ascendant.old.exe beside it first; when `new` cannot take its place, the old
/// one goes back, and when that fails too the exe may be missing ([`Failed::Lost`]).
pub fn swap_checked(current: &Path, new: &Path) -> Result<(), Failed> {
    let old = current.with_file_name(OLD);
    if old.exists() {
        std::fs::remove_file(&old).map_err(|e| Failed::Kept(format!("{}: {e}", old.display())))?;
    }
    std::fs::rename(current, &old).map_err(|e| Failed::Kept(format!("{} to {}: {e}", current.display(), old.display())))?;
    if let Err(e) = std::fs::rename(new, current) {
        let error = format!("{} to {}: {e}", new.display(), current.display());
        return Err(match std::fs::rename(&old, current) {
            Ok(()) => Failed::Kept(error),
            Err(back) => Failed::Lost { old, error: format!("{error}; {back}") },
        });
    }
    Ok(())
}

/// [`swap_checked`] with the outcome as text (for the installer).
pub fn swap(current: &Path, new: &Path) -> Result<(), String> {
    swap_checked(current, new).map_err(|f| match f {
        Failed::Kept(e) => e,
        other => other.text(),
    })
}

/// Deletes the Ascendant.old.exe a swap left beside `current`, if any
/// (and the warden.old.exe of a version from before the rename).
pub fn clean_old(current: &Path) {
    for name in [OLD, LEGACY_OLD] {
        let _ = std::fs::remove_file(current.with_file_name(name));
    }
}

/// The environment variable [`restart`] passes this process's id in.
const WAIT_PID: &str = "ASCENDANT_WAIT_PID";

/// Variable name used by pre-rename versions: when moving from an old version to the new one, the old one passes this.
const LEGACY_WAIT_PID: &str = "WARDEN_WAIT_PID";

/// Starts `current` on its own, with no arguments; the caller then exits.
/// The new process waits for this one in [`wait_for_previous`].
pub fn restart(current: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    std::process::Command::new(current)
        .env(WAIT_PID, std::process::id().to_string())
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .map(drop)
        .map_err(|e| format!("{}: {e}", current.display()))
}

/// Waits up to 5 s for the Ascendant that started this one with [`restart`] to
/// exit, so its hotkeys and its exe file are free; returns at once on a
/// normal start.
pub fn wait_for_previous() {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    let Some(pid) = [WAIT_PID, LEGACY_WAIT_PID].into_iter().find_map(|name| std::env::var(name).ok()).and_then(|p| p.parse::<u32>().ok()) else { return };
    // SAFETY: the handle is only waited on and then closed.
    unsafe {
        if let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(process, 5000);
            let _ = CloseHandle(process);
        }
    }
}

/// Reads `path` if it is at most `limit` bytes.
fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
    if len > limit as u64 {
        return Err(trf!("{}: the file is over the size limit of {} bytes", path.display(), limit));
    }
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// What "Verify downloaded files..." found when the files verified.
#[derive(Clone, Debug, PartialEq)]
pub struct Verified {
    /// The version the signed SHA256SUMS.txt is for.
    pub version: [u64; 3],
    /// "Verified: Ascendant.exe is version X.Y.Z as signed with Ascendant's release key."
    pub line: String,
    /// The other files the signed list names that were in the folder and matched it (Ascendant.exe not included),
    /// in the list's order; files the list names but the folder lacks are not required and not here.
    pub others: Vec<String>,
    /// Things to warn about although the files verified, one line each (empty when none): other .dll or .exe files
    /// in the folder ([`stray_programs`]), and a version older than the running one.
    pub warnings: Vec<String>,
}

impl Verified {
    /// The line, a line naming the other files that matched (when any), and the warnings, one per line (what
    /// [`verify_download`] returns).
    pub fn text(&self) -> String {
        let others = (!self.others.is_empty()).then(|| trf!("Also matches the signed list: {}.", self.others.join(", ")));
        std::iter::once(self.line.as_str()).chain(others.as_deref()).chain(self.warnings.iter().map(String::as_str)).collect::<Vec<_>>().join("\n")
    }
}

/// The .dll and .exe files in `dir` other than Ascendant.exe, by name, sorted (names through [`shown`]). Windows may
/// load a DLL from the folder an exe starts in, so a downloaded Ascendant.exe should not sit beside them.
pub fn stray_programs(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            let lower = n.to_ascii_lowercase();
            (lower.ends_with(".dll") || lower.ends_with(".exe")) && !n.eq_ignore_ascii_case(UPSTREAM_ASSET)
        })
        .map(|n| shown(&n))
        .collect();
    names.sort();
    names
}

/// At most this many stray file names are listed in the warning.
const STRAY_LISTED: usize = 10;

/// [`verify_download_report`] for the running version, as text ([`Verified::text`]: the line, the other files that
/// matched, then any warnings, one per line). Only the tests use it; the settings page's "Verify downloaded
/// files..." calls [`verify_download_report`] itself, to show the warnings apart.
pub fn verify_download(dir: &Path, key: &[u8; 32]) -> Result<String, Problem> {
    verify_download_report(dir, key, env!("CARGO_PKG_VERSION")).map(|v| v.text())
}

/// Whether `name`, from a signed list, names a plain file in the list's own folder: no path separator or drive
/// (`/`, `\`, `:`), unchanged by [`shown`] (nothing unprintable, not overlong), not ending in a dot or a space
/// (Windows drops those, so the name would open another file; `.` and `..` included), and not a Windows device
/// name (CON, PRN, AUX, NUL, CONIN$, CONOUT$, COM1-COM9, LPT1-LPT9, also with the superscript digits Windows treats
/// the same), in any case and with any extension.
fn safe_listed_name(name: &str) -> bool {
    if name.is_empty() || name.contains(['/', '\\', ':']) || shown(name) != name || name.ends_with(['.', ' ']) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    let device = match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => ["COM", "LPT"].iter().any(|p| stem.strip_prefix(p).is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{B9}" | "\u{B2}" | "\u{B3}"))),
    };
    !device
}

/// The SHA-256 of the file at `path`, read in pieces (a listed file may be larger than what is worth holding in
/// memory at once).
fn file_sha256(path: &Path) -> std::io::Result<[u8; 32]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Checks Ascendant.exe in `dir` against the SHA256SUMS.txt and SHA256SUMS.txt.sig beside it, with `key` (the
/// built-in key), then every other file the signed list names that is also in `dir` (the source zip,
/// RELEASE-NOTES.txt): each must match its signed SHA-256. Listed files that are not in `dir` are not required,
/// since players usually download only the exe, the list and its signature. Every listed name must be safe
/// ([`safe_listed_name`]), present or not. When it passes, also warns about other .dll or .exe files in `dir` and
/// about a version older than `running` ("X.Y.Z").
pub fn verify_download_report(dir: &Path, key: &[u8; 32], running: &str) -> Result<Verified, Problem> {
    if !usable_key(key) {
        return Err(Problem::NoKey);
    }
    let (exe, sums, sig) = (dir.join(UPSTREAM_ASSET), dir.join(SUMS), dir.join(SIG));
    for path in [&exe, &sums, &sig] {
        if !path.is_file() {
            return Err(Problem::Files(trf!("The folder has no {}: put Ascendant.exe, {} and {} in one folder.", path.file_name().unwrap_or_default().to_string_lossy(), SUMS, SIG)));
        }
    }
    let read = |p: &Path, limit| read_limited(p, limit).map_err(Problem::Unverified);
    let (sums, sig) = (read(&sums, SMALL_LIMIT)?, read(&sig, SMALL_LIMIT)?);
    verify_sums(&sums, &sig, key).map_err(Problem::Unverified)?;
    let listed = parse_sums(&sums).map_err(Problem::Unverified)?;
    if let Some((name, _)) = listed.iter().find(|(n, _)| !safe_listed_name(n)) {
        return Err(Problem::Unverified(trf!("{} lists a path outside its folder, or an unsafe, unprintable or overlong name: {}", SUMS, shown(name))));
    }
    let signed = sum_of(&listed, UPSTREAM_ASSET).map_err(Problem::Unverified)?;
    let version = listed_version(&listed).ok_or_else(|| Problem::Unverified(trf!("the signed {} names no version", SUMS)))?;
    let bytes = read(&exe, EXE_LIMIT)?;
    if Sha256::digest(&bytes)[..] != signed {
        return Err(Problem::Unverified(trf!("the downloaded {} does not match the signed SHA-256", UPSTREAM_ASSET)));
    }
    let mut others = Vec::new();
    for (name, want) in listed.iter().filter(|(n, _)| n != UPSTREAM_ASSET) {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        let got = file_sha256(&path).map_err(|e| Problem::Files(format!("{}: {e}", path.display())))?;
        if got != *want {
            return Err(Problem::Unverified(trf!("the downloaded {} does not match the signed SHA-256", name)));
        }
        others.push(name.clone());
    }
    let [a, b, c] = version;
    let line = trf!("Verified: Ascendant.exe is version {} as signed with Ascendant's release key.", format!("{a}.{b}.{c}"));
    let mut warnings = Vec::new();
    let stray = stray_programs(dir);
    if !stray.is_empty() {
        let mut list = stray.iter().take(STRAY_LISTED).cloned().collect::<Vec<_>>().join(", ");
        if stray.len() > STRAY_LISTED {
            list.push_str(&trf!(" and {} more", stray.len() - STRAY_LISTED));
        }
        warnings.push(trf!("Warning: the folder also holds {}. Windows may load a DLL from the folder an exe starts in: move Ascendant.exe to a folder of its own before starting it.", list));
    }
    if semver(running).is_some_and(|r| version < r) {
        warnings.push(trf!("Warning: this is an older version ({}) than the one running ({}).", format!("{a}.{b}.{c}"), running));
    }
    Ok(Verified { version, line, others, warnings })
}

// ---- Release signing (ascendant-cli sign / verify; the key file itself is handled by ascendant-cli) ----

/// The public key of a seed, as bytes.
pub fn public_of(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// The line to paste over [`RELEASE_PUBLIC_KEY`] in this file.
pub fn public_key_line(public: &[u8; 32]) -> String {
    format!("pub const RELEASE_PUBLIC_KEY: [u8; 32] = key_from_hex(\"{}\");", hex(public))
}

/// `key`'s detached signature of `data`, 64 raw bytes (the .sig file's content).
pub fn sign(key: &SigningKey, data: &[u8]) -> [u8; 64] {
    key.sign(data).to_bytes()
}

/// For `ascendant-cli verify`: checks `sums_path`'s signature (`<sums_path>.sig`) with `key`, that it lists
/// RELEASE-NOTES.txt, then every file it lists in the same folder, and that the notes are UTF-8 of at most
/// [`SMALL_LIMIT`] bytes (what the app accepts). Returns one line per file; Err on the first failure of the
/// signature or the list, or when any file is missing or differs (with all lines so far).
pub fn verify_folder(sums_path: &Path, key: &[u8; 32]) -> Result<Vec<String>, String> {
    let mut sig_path = sums_path.as_os_str().to_owned();
    sig_path.push(".sig");
    let sums = read_limited(sums_path, SMALL_LIMIT)?;
    let sig = read_limited(Path::new(&sig_path), SMALL_LIMIT)?;
    verify_sums(&sums, &sig, key)?;
    let mut lines = vec![format!("OK   signature of {} ({} bytes)", sums_path.display(), sums.len())];
    let dir = sums_path.parent().unwrap_or(Path::new("."));
    let listed = parse_sums(&sums)?;
    if listed.is_empty() {
        return Err(format!("{} lists no files", sums_path.display()));
    }
    // Every release signs its notes: the app refuses a release whose list lacks them.
    sum_of(&listed, NOTES)?;
    let mut failed = false;
    for (name, want) in &listed {
        if !safe_listed_name(name) {
            return Err(format!("{} lists a path outside its folder, or an unsafe, unprintable or overlong name: {}", sums_path.display(), shown(name)));
        }
        let path = dir.join(name);
        match std::fs::read(&path) {
            Ok(bytes) if Sha256::digest(&bytes)[..] == want[..] => {
                if name == NOTES {
                    if bytes.len() > SMALL_LIMIT {
                        failed = true;
                        lines.push(format!("BAD  {name}: over the size limit of {SMALL_LIMIT} bytes the app accepts"));
                        continue;
                    }
                    if std::str::from_utf8(&bytes).is_err() {
                        failed = true;
                        lines.push(format!("BAD  {name}: not UTF-8 text, which the app refuses"));
                        continue;
                    }
                }
                lines.push(format!("OK   {name}"));
            }
            Ok(_) => {
                failed = true;
                lines.push(format!("BAD  {name}: SHA-256 differs from the signed list"));
            }
            Err(e) => {
                failed = true;
                lines.push(format!("BAD  {name}: {e}"));
            }
        }
    }
    if failed { Err(lines.join("\n")) } else { Ok(lines) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::http::Hop;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A test-only signing key (a fixed seed), never used for releases.
    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn test_public() -> [u8; 32] {
        test_key().verifying_key().to_bytes()
    }

    const EXE_BYTES: &[u8] = b"pretend this is Ascendant.exe 1.2.0";
    const EXE_URL: &str = "https://github.com/DiepBlood/tli-ascendant/releases/download/v1.2.0/Ascendant.exe";
    const SUMS_URL: &str = "https://github.com/DiepBlood/tli-ascendant/releases/download/v1.2.0/SHA256SUMS.txt";
    const SIG_URL: &str = "https://github.com/DiepBlood/tli-ascendant/releases/download/v1.2.0/SHA256SUMS.txt.sig";
    const NOTES_URL: &str = "https://github.com/DiepBlood/tli-ascendant/releases/download/v1.2.0/RELEASE-NOTES.txt";
    const CDN: &str = "https://release-assets.githubusercontent.com/github-production-release-asset/1/abc";

    /// The signed notes file, with the byte order mark and CR LF a PowerShell script may write.
    const NOTES_BYTES: &[u8] = b"\xEF\xBB\xBF## What's new\r\n- one\r\n- two\r\n- three\r\n";
    /// What the window shows of [`NOTES_BYTES`].
    const NOTES_SHOWN: &str = "## What's new\n- one\n- two\n- three";

    fn sha(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    fn sums_with(exe: &[u8], version: &str, notes: Option<&[u8]>) -> Vec<u8> {
        let mut text = format!("{}  Ascendant.exe\n{}  Ascendant-{version}-source.zip\n", hex(&sha(exe)), hex(&sha(b"zip")));
        if let Some(notes) = notes {
            text.push_str(&format!("{}  RELEASE-NOTES.txt\n", hex(&sha(notes))));
        }
        text.into_bytes()
    }

    fn sums_text(exe: &[u8], version: &str) -> Vec<u8> {
        sums_with(exe, version, Some(NOTES_BYTES))
    }

    fn json(tag: &str, exe_digest: &[u8; 32], exe_size: usize) -> String {
        serde_json::json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/DiepBlood/tli-ascendant/releases/tag/{tag}"),
            "body": "UNSIGNED BODY \u{202E}never shown",
            "assets": [
                {"name": "Ascendant.exe", "size": exe_size, "digest": format!("sha256:{}", hex(exe_digest)), "browser_download_url": EXE_URL},
                {"name": "SHA256SUMS.txt", "size": 200, "browser_download_url": SUMS_URL},
                {"name": "SHA256SUMS.txt.sig", "size": 64, "browser_download_url": SIG_URL},
                {"name": "RELEASE-NOTES.txt", "size": NOTES_BYTES.len(), "browser_download_url": NOTES_URL},
            ]
        })
        .to_string()
    }

    /// Answers each URL from a table and records what was asked; panics on a URL not in the table, so a test also
    /// shows nothing else is fetched.
    #[derive(Default)]
    struct Fake {
        answers: HashMap<String, Result<Hop, Fault>>,
        asked: RefCell<Vec<String>>,
    }

    impl Fake {
        fn with(mut self, url: &str, hop: Hop) -> Fake {
            self.answers.insert(url.to_string(), Ok(hop));
            self
        }

        fn failing(mut self, url: &str, fault: Fault) -> Fake {
            self.answers.insert(url.to_string(), Err(fault));
            self
        }
    }

    impl Fetch for Fake {
        fn fetch(&self, url: &str, _accept: &str, _limit: usize, _deadline: std::time::Instant) -> Result<Hop, Fault> {
            self.asked.borrow_mut().push(url.to_string());
            self.answers.get(url).unwrap_or_else(|| panic!("unexpected request {url}")).clone()
        }
    }

    /// A release whose files all check out, served through a redirect to GitHub's download host.
    fn good() -> (Release, Fake) {
        let sums = sums_text(EXE_BYTES, "1.2.0");
        let sig = sign(&test_key(), &sums);
        let (release, _) = parse_release(&json("v1.2.0", &sha(EXE_BYTES), EXE_BYTES.len())).unwrap();
        let fake = Fake::default()
            .with(SUMS_URL, Hop::Body(sums))
            .with(SIG_URL, Hop::Body(sig.to_vec()))
            .with(NOTES_URL, Hop::Body(NOTES_BYTES.to_vec()))
            .with(EXE_URL, Hop::Redirect(CDN.to_string()))
            .with(CDN, Hop::Body(EXE_BYTES.to_vec()));
        (release, fake)
    }

    /// `good`'s files with SHA256SUMS.txt replaced by `sums`, signed with the test key.
    fn signed_sums(fake: Fake, sums: Vec<u8>) -> Fake {
        let sig = sign(&test_key(), &sums);
        fake.with(SUMS_URL, Hop::Body(sums)).with(SIG_URL, Hop::Body(sig.to_vec()))
    }

    /// The API answer for `good`'s release on top of its files.
    fn with_api(fake: Fake, tag: &str) -> Fake {
        fake.with(LATEST, Hop::Body(json(tag, &sha(EXE_BYTES), EXE_BYTES.len()).into_bytes()))
    }

    fn run(release: &Release, fake: &Fake, key: &[u8; 32]) -> Result<Vec<u8>, Problem> {
        download_with(fake, release, "1.0.0", key, &AtomicU8::new(0))
    }

    fn unverified(r: Result<Vec<u8>, Problem>) -> String {
        match r {
            Err(Problem::Unverified(d)) => d,
            other => panic!("expected an unverified release, got {other:?}"),
        }
    }

    #[test]
    fn signed_release_downloads() {
        let (release, fake) = good();
        assert_eq!(run(&release, &fake, &test_public()).unwrap(), EXE_BYTES);
        assert_eq!(*fake.asked.borrow(), [SUMS_URL, SIG_URL, EXE_URL, CDN]);
    }

    #[test]
    fn check_offers_only_a_verified_release() {
        // A good release is offered, with its signed sum and signed notes, after fetching its list, signature and
        // notes (not the exe).
        let (_, fake) = good();
        let fake = with_api(fake, "v1.2.0");
        let offered = check_with(&fake, "1.0.0", &test_public()).unwrap().unwrap();
        assert_eq!((offered.version, offered.signed), ([1, 2, 0], Some(sha(EXE_BYTES))));
        assert_eq!(offered.notes, NOTES_SHOWN);
        assert_eq!(notes_preview(&offered.notes, 2), ("## What's new\n- one".to_string(), true));
        assert!(!notes_preview(&offered.notes, 4).1);
        assert_eq!(*fake.asked.borrow(), [LATEST, SUMS_URL, SIG_URL, NOTES_URL]);
        // A bad signature: no offer, the release is reported unverified.
        let (_, fake) = good();
        let fake = with_api(fake, "v1.2.0").with(SIG_URL, Hop::Body(sign(&SigningKey::from_bytes(&[9u8; 32]), b"x").to_vec()));
        assert!(matches!(check_with(&fake, "1.0.0", &test_public()), Err(Problem::Unverified(_))));
        // A list signed for another version, and GitHub's digest differing from the signed sum.
        let (_, fake) = good();
        let old = sums_text(EXE_BYTES, "1.1.0");
        let fake = with_api(fake, "v1.2.0").with(SIG_URL, Hop::Body(sign(&test_key(), &old).to_vec())).with(SUMS_URL, Hop::Body(old));
        assert!(matches!(check_with(&fake, "1.0.0", &test_public()), Err(Problem::Unverified(_))));
        let (_, fake) = good();
        let fake = fake.with(LATEST, Hop::Body(json("v1.2.0", &sha(b"other"), EXE_BYTES.len()).into_bytes()));
        assert!(matches!(check_with(&fake, "1.0.0", &test_public()), Err(Problem::Unverified(_))));
        // A network failure is only "try again later".
        let (_, fake) = good();
        let fake = with_api(fake, "v1.2.0").failing(SIG_URL, Fault::Network("HTTP 502".into()));
        let problem = check_with(&fake, "1.0.0", &test_public()).unwrap_err();
        assert_eq!(problem, Problem::Network("HTTP 502".into()));
        assert_eq!(problem.headline(true), "Can't check right now. Try again later.");
    }

    #[test]
    fn signature_checks() {
        let sums = sums_text(EXE_BYTES, "1.2.0");
        let sig = sign(&test_key(), &sums);
        let key = test_public();
        assert!(verify_sums(&sums, &sig, &key).is_ok());
        // Any changed byte of SHA256SUMS.txt breaks the signature.
        for i in 0..sums.len() {
            let mut changed = sums.clone();
            changed[i] ^= 0x01;
            assert!(verify_sums(&changed, &sig, &key).is_err(), "byte {i}");
        }
        // A signature of the wrong length, or from another key.
        assert!(verify_sums(&sums, &sig[..63], &key).unwrap_err().contains("63"));
        assert!(verify_sums(&sums, &[sig.as_slice(), &[0]].concat(), &key).is_err());
        assert!(verify_sums(&sums, &[], &key).is_err());
        let other = sign(&SigningKey::from_bytes(&[9u8; 32]), &sums);
        assert!(verify_sums(&sums, &other, &key).is_err());
        // The placeholder key verifies nothing, not even a signature made for it.
        assert!(verify_sums(&sums, &sig, &PLACEHOLDER_PUBLIC_KEY).is_err());
        assert!(!usable_key(&PLACEHOLDER_PUBLIC_KEY) && usable_key(&key));
    }

    #[test]
    fn placeholder_key_refuses_every_update() {
        // The built-in key is still the placeholder in this source tree: nothing would install.
        assert_eq!(has_release_key(), RELEASE_PUBLIC_KEY != PLACEHOLDER_PUBLIC_KEY);
        let (release, _) = good();
        // An empty fake panics on any request: the refusal comes before the network.
        let none = Fake::default();
        assert_eq!(run(&release, &none, &PLACEHOLDER_PUBLIC_KEY), Err(Problem::NoKey));
        assert_eq!(check_with(&none, "1.0.0", &PLACEHOLDER_PUBLIC_KEY), Err(Problem::NoKey));
        assert_eq!(verify_download(Path::new("."), &PLACEHOLDER_PUBLIC_KEY), Err(Problem::NoKey));
        assert!(none.asked.borrow().is_empty());
    }

    #[test]
    fn exe_must_match_the_signed_sum() {
        let (release, fake) = good();
        // The download differs (same size), though GitHub's digest and the signed list agree.
        let fake = fake.with(CDN, Hop::Body(b"pretend this is Ascendant.exe 6.6.6".to_vec()));
        assert!(unverified(run(&release, &fake, &test_public())).contains("does not match the signed"));
        // A shorter download.
        let (release, fake) = good();
        let fake = fake.with(CDN, Hop::Body(EXE_BYTES[1..].to_vec()));
        assert!(unverified(run(&release, &fake, &test_public())).contains("bytes downloaded"));
    }

    #[test]
    fn api_digest_must_match_the_signed_sum() {
        let (mut release, fake) = good();
        release.exe.digest = Some(sha(b"something else"));
        assert!(unverified(run(&release, &fake, &test_public())).contains("differs from the signed"));
        // Nothing past the signature was fetched.
        assert_eq!(*fake.asked.borrow(), [SUMS_URL, SIG_URL]);
        // And a release without GitHub's digest is not offered at all.
        let json = json("v1.2.0", &sha(EXE_BYTES), EXE_BYTES.len()).replace("\"digest\"", "\"other\"");
        assert!(parse_release(&json).is_err());
    }

    #[test]
    fn signed_list_must_be_for_the_tag() {
        // An old signed release (1.1.0's list) republished under the tag v1.2.0.
        let (release, fake) = good();
        let sums = sums_text(EXE_BYTES, "1.1.0");
        let sig = sign(&test_key(), &sums);
        let fake = fake.with(SUMS_URL, Hop::Body(sums)).with(SIG_URL, Hop::Body(sig.to_vec()));
        assert!(unverified(run(&release, &fake, &test_public())).contains("not for version 1.2.0"));
    }

    #[test]
    fn versions() {
        assert_eq!(tag_version("v1.2.3"), Some([1, 2, 3]));
        assert_eq!(tag_version("v10.0.0"), Some([10, 0, 0]));
        for bad in ["1.2.3", "v1.2", "v1.2.3.4", "v1.2.x", "v1.2.3-beta", "v01.2.3", "v1..3", "", "v", "latest", "v1.2.3 ", "V1.2.3", "v+1.2.3"] {
            assert_eq!(tag_version(bad), None, "{bad}");
        }
        assert!(is_newer([1, 0, 1], "1.0.0"));
        assert!(is_newer([1, 10, 0], "1.9.9"));
        assert!(is_newer([2, 0, 0], "1.99.99"));
        assert!(!is_newer([1, 0, 0], "1.0.0"));
        assert!(!is_newer([0, 9, 9], "1.0.0"));
        assert!(!is_newer([9, 9, 9], "not a version"));
        // The running version parses.
        assert!(semver(env!("CARGO_PKG_VERSION")).is_some());
    }

    #[test]
    fn same_or_older_release_is_refused() {
        for (tag, version) in [("v1.0.0", "1.0.0"), ("v0.9.0", "0.9.0")] {
            let sums = sums_text(EXE_BYTES, version);
            let sig = sign(&test_key(), &sums);
            let (release, _) = parse_release(&json(tag, &sha(EXE_BYTES), EXE_BYTES.len())).unwrap();
            let fake = Fake::default().with(SUMS_URL, Hop::Body(sums)).with(SIG_URL, Hop::Body(sig.to_vec()));
            assert!(unverified(run(&release, &fake, &test_public())).contains("downgrades are refused"), "{tag}");
            assert!(fake.asked.borrow().is_empty());
            // The check does not offer it either, and fetches nothing more than the API.
            let api = Fake::default().with(LATEST, Hop::Body(json(tag, &sha(EXE_BYTES), EXE_BYTES.len()).into_bytes()));
            assert_eq!(check_with(&api, "1.0.0", &test_public()), Ok(None));
        }
        let api = Fake::default().with(LATEST, Hop::Body(json("1.2.0", &sha(EXE_BYTES), EXE_BYTES.len()).into_bytes()));
        assert!(matches!(check_with(&api, "1.0.0", &test_public()), Err(Problem::Unverified(d)) if d.contains("not vX.Y.Z")));
    }

    #[test]
    fn redirect_to_another_host_is_refused() {
        let (release, fake) = good();
        let fake = fake.with(EXE_URL, Hop::Redirect("https://evil.example.com/Ascendant.exe".into()));
        assert!(unverified(run(&release, &fake, &test_public())).contains("evil.example.com"));
        assert!(!fake.asked.borrow().iter().any(|u| u.contains("evil")));
        // Plain http, even to an allowed host.
        let (release, fake) = good();
        let fake = fake.with(EXE_URL, Hop::Redirect(CDN.replace("https:", "http:")));
        assert!(matches!(run(&release, &fake, &test_public()), Err(Problem::Unverified(_))));
        // A look-alike host and a user-info trick.
        let (release, fake) = good();
        let fake = fake.with(EXE_URL, Hop::Redirect("https://github.com.evil.example/x".into()));
        assert!(matches!(run(&release, &fake, &test_public()), Err(Problem::Unverified(_))));
        let (release, fake) = good();
        let fake = fake.with(EXE_URL, Hop::Redirect("https://github.com@evil.example/x".into()));
        assert!(matches!(run(&release, &fake, &test_public()), Err(Problem::Unverified(_))));
        // An asset link that is not on GitHub to begin with.
        let (mut release, fake) = good();
        release.sums.url = "https://evil.example.com/SHA256SUMS.txt".into();
        assert!(matches!(run(&release, &fake, &test_public()), Err(Problem::Unverified(_))));
        assert!(fake.asked.borrow().is_empty());
    }

    #[test]
    fn oversize_is_refused() {
        // Declared by the API.
        let (mut release, fake) = good();
        release.exe.size = EXE_LIMIT as u64 + 1;
        assert!(unverified(run(&release, &fake, &test_public())).contains("size limit"));
        assert!(fake.asked.borrow().is_empty());
        // Or only noticed in the body.
        let (release, fake) = good();
        let fake = fake.with(SIG_URL, Hop::Body(vec![0; SMALL_LIMIT + 1]));
        assert!(unverified(run(&release, &fake, &test_public())).contains("size limit"));
    }

    #[test]
    fn parse_release_json() {
        let (r, notes) = parse_release(&json("v1.2.0", &sha(EXE_BYTES), 35)).unwrap();
        assert_eq!((r.version, r.exe.size, r.version_text(), r.signed), ([1, 2, 0], 35, "1.2.0".to_string(), None));
        assert_eq!((r.exe.url.as_str(), r.sums.url.as_str(), r.sig.url.as_str(), notes.url.as_str()), (EXE_URL, SUMS_URL, SIG_URL, NOTES_URL));
        // The release body is never taken: notes come only from the signed file, after verification.
        assert_eq!(r.notes, "");
        let full = json("v1.2.0", &sha(EXE_BYTES), 35);
        assert!(parse_release(&full.replace("\"SHA256SUMS.txt.sig\"", "\"other.sig\"")).unwrap_err().contains(SIG));
        assert!(parse_release(&full.replace("\"Ascendant.exe\"", "\"warden.exe\"")).is_err());
        assert!(parse_release(&full.replace("\"RELEASE-NOTES.txt\"", "\"notes.md\"")).unwrap_err().contains(NOTES));
    }

    #[test]
    fn notes_lose_direction_controls() {
        let body = "\u{FEFF}safe\u{202E}exe.txt\r\n\u{2066}a\u{2069} \u{200E}b\u{200F} \u{202A}c\u{202C} \u{2067}\u{2068}d\u{202B}\u{202D}\u{0007}\ttab \u{061C}e";
        let notes = plain_notes(body);
        assert_eq!(notes, "safeexe.txt\na b c d\ttab e");
        assert!(!notes.chars().any(is_bidi_control));
        assert_eq!(plain_notes(&"x".repeat(NOTES_LIMIT + 50)).chars().count(), NOTES_LIMIT);
        // A signed notes file goes through the same filter.
        let file = "\u{202E}evil\r\nline".as_bytes();
        assert_eq!(signed_notes(file, &sha(file)).unwrap(), "evil\nline");
    }

    /// Notes come only from a RELEASE-NOTES.txt the signed list names, whose bytes match; anything else is refused
    /// like any other verification failure, and the release body is never shown.
    #[test]
    fn notes_must_be_signed() {
        let check = |fake: &Fake| check_with(fake, "1.0.0", &test_public());
        let refused = |r: Result<Option<Release>, Problem>| match r {
            Err(Problem::Unverified(d)) => d,
            other => panic!("expected an unverified release, got {other:?}"),
        };
        // The body is not what is shown.
        let (_, fake) = good();
        let offered = check(&with_api(fake, "v1.2.0")).unwrap().unwrap();
        assert!(!offered.notes.contains("BODY") && !offered.notes.contains("never shown"));
        // A signed list without the notes: refused, and the notes are not even fetched; the download refuses it too.
        let (release, fake) = good();
        let fake = signed_sums(with_api(fake, "v1.2.0"), sums_with(EXE_BYTES, "1.2.0", None));
        assert!(refused(check(&fake)).contains("does not list RELEASE-NOTES.txt"));
        assert!(!fake.asked.borrow().iter().any(|u| u == NOTES_URL));
        assert!(unverified(run(&release, &fake, &test_public())).contains("RELEASE-NOTES.txt"));
        // Notes that differ from the signed ones (an edited file on GitHub).
        let (_, fake) = good();
        let fake = with_api(fake, "v1.2.0").with(NOTES_URL, Hop::Body(b"## What's new\r\n- install this other thing".to_vec()));
        assert!(refused(check(&fake)).contains("RELEASE-NOTES.txt does not match the signed SHA-256"));
        // Signed but not UTF-8.
        let bad = b"\xff\xfe notes".to_vec();
        let (_, fake) = good();
        let fake = signed_sums(with_api(fake, "v1.2.0"), sums_with(EXE_BYTES, "1.2.0", Some(&bad))).with(NOTES_URL, Hop::Body(bad));
        assert!(refused(check(&fake)).contains("not UTF-8"));
        // Too big, as declared by the API or as sent.
        let (_, fake) = good();
        let big = json("v1.2.0", &sha(EXE_BYTES), EXE_BYTES.len()).replace(&format!("\"size\":{}", NOTES_BYTES.len()), &format!("\"size\":{}", SMALL_LIMIT + 1));
        let fake = fake.with(LATEST, Hop::Body(big.into_bytes()));
        assert!(refused(check(&fake)).contains("size limit"));
        assert!(!fake.asked.borrow().iter().any(|u| u == NOTES_URL));
        let huge = vec![b'a'; SMALL_LIMIT + 1];
        let (_, fake) = good();
        let fake = signed_sums(with_api(fake, "v1.2.0"), sums_with(EXE_BYTES, "1.2.0", Some(&huge))).with(NOTES_URL, Hop::Body(huge));
        assert!(refused(check(&fake)).contains("size limit"));
        // A release without a notes asset at all.
        let (_, fake) = good();
        let none = json("v1.2.0", &sha(EXE_BYTES), EXE_BYTES.len()).replace("\"RELEASE-NOTES.txt\"", "\"notes.md\"");
        assert!(refused(check(&fake.with(LATEST, Hop::Body(none.into_bytes())))).contains("RELEASE-NOTES.txt"));
    }

    #[test]
    fn network_text_is_shown_safely() {
        assert_eq!(shown("v1.2.3"), "v1.2.3");
        assert_eq!(shown("a\u{202E}b\u{061C}c\u{2066}d\u{0007}e\r\nf\u{001B}[31m"), "abcdef[31m");
        let long = "x".repeat(500);
        let cut = shown(&long);
        assert_eq!(cut.chars().count(), SHOWN_LIMIT);
        assert!(cut.ends_with('\u{2026}'));
        assert_eq!(shown(&"y".repeat(SHOWN_LIMIT)), "y".repeat(SHOWN_LIMIT));
        // A hostile tag in the API's answer.
        let tag = format!("v1\u{202E}{}", "z".repeat(1000));
        let e = parse_release(&json(&tag, &sha(EXE_BYTES), 35)).unwrap_err();
        assert!(!e.contains('\u{202E}') && e.chars().count() < SHOWN_LIMIT + 40, "{e}");
        // A malformed line of a list.
        let e = parse_sums(format!("\u{2067}{}\n", "q".repeat(400)).as_bytes()).unwrap_err();
        assert!(!e.contains('\u{2067}') && e.chars().count() < SHOWN_LIMIT + 40, "{e}");
    }

    #[test]
    fn sums_lines() {
        let h = "ab".repeat(32);
        let listed = parse_sums(format!("{h}  Ascendant.exe\r\n\n{h} *b.zip\n").as_bytes()).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(sum_of(&listed, "b.zip").unwrap(), [0xab; 32]);
        assert!(sum_of(&listed, "c").is_err());
        let twice = parse_sums(format!("{h}  a\n{h}  a\n").as_bytes()).unwrap();
        assert!(sum_of(&twice, "a").unwrap_err().contains("twice"));
        for bad in [format!("{h} a"), format!("{h}  "), format!("{}  a", &h[..62]), "zz".repeat(32) + "  a"] {
            assert!(parse_sums(bad.as_bytes()).is_err(), "{bad}");
        }
        let two = parse_sums(format!("{h}  Ascendant-1.0.0-source.zip\n{h}  Ascendant-1.1.0-source.zip\n").as_bytes()).unwrap();
        assert_eq!(listed_version(&two), None);
        assert_eq!(listed_version(&two[..1]), Some([1, 0, 0]));
    }

    #[test]
    fn fingerprint_and_key_line() {
        // 128 bits: 32 lowercase hex digits of the key's SHA-256, eight groups of four, single spaces.
        let fp = key_fingerprint(&[0xab; 32]);
        assert_eq!(fp.len(), 39);
        assert_eq!(fp.split(' ').count(), 8);
        assert!(fp.split(' ').all(|g| g.len() == 4 && g.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))));
        assert_eq!(fp.replace(' ', ""), hex(&sha(&[0xab; 32]))[..32]);
        // A known value: SHA-256 of 32 zero bytes is 66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925.
        assert_eq!(key_fingerprint(&[0; 32]), "6668 7aad f862 bd77 6c8f c18b 8e9f 8e20");
        assert_eq!(key_from_hex(&hex(&public_of(&[42; 32]))), public_of(&[42; 32]));
        assert!(public_key_line(&[0xab; 32]).ends_with(&format!("key_from_hex(\"{}\");", "ab".repeat(32))));
    }

    /// A fresh folder under the temp folder.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ascendant-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn verify_folder_files() {
        let dir = temp("verify");
        std::fs::write(dir.join("Ascendant.exe"), EXE_BYTES).unwrap();
        std::fs::write(dir.join("Ascendant-1.2.0-source.zip"), b"zip").unwrap();
        std::fs::write(dir.join(NOTES), NOTES_BYTES).unwrap();
        let path = dir.join(SUMS);
        let write_sums = |sums: &[u8]| {
            std::fs::write(&path, sums).unwrap();
            std::fs::write(dir.join(SIG), sign(&test_key(), sums)).unwrap();
        };
        // `ascendant-cli verify` requires the signed notes in the list, and notes the app would accept.
        write_sums(&sums_with(EXE_BYTES, "1.2.0", None));
        assert!(verify_folder(&path, &test_public()).unwrap_err().contains("does not list RELEASE-NOTES.txt"));
        let bad_notes = b"\xff not text";
        std::fs::write(dir.join(NOTES), bad_notes).unwrap();
        write_sums(&sums_with(EXE_BYTES, "1.2.0", Some(bad_notes)));
        assert!(verify_folder(&path, &test_public()).unwrap_err().contains("BAD  RELEASE-NOTES.txt: not UTF-8"));
        std::fs::write(dir.join(NOTES), NOTES_BYTES).unwrap();
        assert!(verify_folder(&path, &test_public()).unwrap_err().contains("BAD  RELEASE-NOTES.txt: SHA-256 differs"));
        write_sums(&sums_text(EXE_BYTES, "1.2.0"));
        assert_eq!(verify_folder(&path, &test_public()).unwrap().len(), 4);
        assert!(verify_folder(&path, &PLACEHOLDER_PUBLIC_KEY).is_err());
        // The window's check of a downloaded folder.
        assert!(verify_download(&dir, &test_public()).unwrap().contains("1.2.0"));
        assert!(matches!(verify_download(&dir, &public_of(&[9; 32])), Err(Problem::Unverified(_))));
        std::fs::write(dir.join("Ascendant.exe"), b"pretend this is Ascendant.exe 1.2.1").unwrap();
        assert!(verify_folder(&path, &test_public()).unwrap_err().contains("BAD  Ascendant.exe"));
        assert!(matches!(verify_download(&dir, &test_public()), Err(Problem::Unverified(d)) if d.contains("does not match")));
        std::fs::remove_file(dir.join(SIG)).unwrap();
        assert!(matches!(verify_download(&dir, &test_public()), Err(Problem::Files(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_download_warnings() {
        let dir = temp("warnings");
        std::fs::write(dir.join("Ascendant.exe"), EXE_BYTES).unwrap();
        let sums = sums_text(EXE_BYTES, "1.2.0");
        std::fs::write(dir.join(SUMS), &sums).unwrap();
        std::fs::write(dir.join(SIG), sign(&test_key(), &sums)).unwrap();
        // A clean folder, same or newer version: no warnings.
        for running in ["1.2.0", "1.1.9", "not a version"] {
            let v = verify_download_report(&dir, &test_public(), running).unwrap();
            assert_eq!((v.version, v.warnings.len()), ([1, 2, 0], 0), "{running}");
            assert_eq!(v.text(), v.line);
        }
        // Programs beside it (any case), listed; other files and folders are not.
        for name in ["version.dll", "Setup.EXE", "Ascendant.old.exe", "readme.txt", "x.dll.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        std::fs::create_dir_all(dir.join("folder.dll")).unwrap();
        assert_eq!(stray_programs(&dir), ["Ascendant.old.exe", "Setup.EXE", "version.dll"]);
        let v = verify_download_report(&dir, &test_public(), "1.2.0").unwrap();
        assert_eq!(v.warnings.len(), 1);
        assert!(v.warnings[0].contains("Ascendant.old.exe, Setup.EXE, version.dll"), "{}", v.warnings[0]);
        // An older version than the running one.
        let v = verify_download_report(&dir, &test_public(), "1.3.0").unwrap();
        assert_eq!(v.warnings.len(), 2);
        assert!(v.warnings[1].contains("older version (1.2.0)") && v.warnings[1].contains("(1.3.0)"));
        assert_eq!(v.text().lines().count(), 3);
        // Many strays: the list is cut.
        for i in 0..15 {
            std::fs::write(dir.join(format!("extra{i:02}.dll")), b"x").unwrap();
        }
        let v = verify_download_report(&dir, &test_public(), "1.2.0").unwrap();
        assert!(v.warnings[0].contains(" and 8 more"), "{}", v.warnings[0]);
        // The window's text form carries the warnings too.
        assert!(verify_download(&dir, &test_public()).unwrap().contains("Setup.EXE"));
        // No warnings when the files do not verify: the error alone.
        std::fs::write(dir.join("Ascendant.exe"), b"something else").unwrap();
        assert!(matches!(verify_download_report(&dir, &test_public(), "9.9.9"), Err(Problem::Unverified(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// "Verify downloaded files..." also checks every other listed file that is in the folder, and does not require
    /// the ones that are not.
    #[test]
    fn verify_download_checks_other_listed_files() {
        let dir = temp("others");
        std::fs::write(dir.join("Ascendant.exe"), EXE_BYTES).unwrap();
        let sums = sums_text(EXE_BYTES, "1.2.0");
        std::fs::write(dir.join(SUMS), &sums).unwrap();
        std::fs::write(dir.join(SIG), sign(&test_key(), &sums)).unwrap();
        let zip = "Ascendant-1.2.0-source.zip";
        // Only the exe, the list and its signature: the other listed files are not required.
        let v = verify_download_report(&dir, &test_public(), "1.2.0").unwrap();
        assert!(v.others.is_empty());
        assert_eq!(v.text(), v.line);
        // Listed files that match are checked and named.
        std::fs::write(dir.join(zip), b"zip").unwrap();
        let v = verify_download_report(&dir, &test_public(), "1.2.0").unwrap();
        assert_eq!(v.others, [zip]);
        std::fs::write(dir.join(NOTES), NOTES_BYTES).unwrap();
        let v = verify_download_report(&dir, &test_public(), "1.2.0").unwrap();
        assert_eq!(v.others, [zip, NOTES]);
        assert_eq!(v.text().lines().nth(1), Some("Also matches the signed list: Ascendant-1.2.0-source.zip, RELEASE-NOTES.txt."));
        // A listed file whose content was changed is refused, though the exe still matches.
        std::fs::write(dir.join(zip), b"zap").unwrap();
        let refused = verify_download_report(&dir, &test_public(), "1.2.0");
        assert!(matches!(&refused, Err(Problem::Unverified(d)) if d.contains("Ascendant-1.2.0-source.zip does not match")), "{refused:?}");
        std::fs::write(dir.join(zip), b"zip").unwrap();
        std::fs::write(dir.join(NOTES), b"## What's new\r\n- install this other thing").unwrap();
        let refused = verify_download_report(&dir, &test_public(), "1.2.0");
        assert!(matches!(&refused, Err(Problem::Unverified(d)) if d.contains("RELEASE-NOTES.txt does not match")), "{refused:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn listed_names_must_be_safe() {
        for good in ["Ascendant.exe", "RELEASE-NOTES.txt", "Ascendant-1.2.0-source.zip", "CONFIG.txt", "console.log", "COM10.txt", "LPT0", "nul_x.txt", "x.y z", "CONIN", "conout.txt", "CONIN$X", "xCONOUT$", "CONOUT$$"] {
            assert!(safe_listed_name(good), "{good}");
        }
        for bad in [
            "CON", "con.txt", "Nul.tar.gz", "aux", "PRN.md", "COM1", "com9.zip", "lpt1.dll", "LPT9", "COM\u{B9}", "aux .txt", "x.", "x ", "x. ", ".", "..", "", "a/b", "a\\b", "c:x", "a\u{200B}b",
            "a\u{202E}b", "a\tb", "CONIN$", "conin$", "CONOUT$", "Conout$", "conin$.txt", "CONOUT$.tar.gz", "conout$ .log",
        ] {
            assert!(!safe_listed_name(bad), "{bad:?}");
        }
        // A signed list naming a device or a name Windows would change is refused by both checks.
        for name in ["NUL.txt", "notes.txt."] {
            let dir = temp("names");
            std::fs::write(dir.join("Ascendant.exe"), EXE_BYTES).unwrap();
            let mut sums = sums_text(EXE_BYTES, "1.2.0");
            sums.extend_from_slice(format!("{}  {name}\n", hex(&sha(b"x"))).as_bytes());
            std::fs::write(dir.join(SUMS), &sums).unwrap();
            std::fs::write(dir.join(SIG), sign(&test_key(), &sums)).unwrap();
            let refused = verify_download_report(&dir, &test_public(), "1.2.0");
            assert!(matches!(&refused, Err(Problem::Unverified(d)) if d.contains("unsafe")), "{name}: {refused:?}");
            assert!(verify_folder(&dir.join(SUMS), &test_public()).unwrap_err().contains("unsafe"), "{name}");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// Invisible format characters are dropped from network text and notes, line feeds and tabs kept in notes.
    #[test]
    fn hidden_format_characters_are_dropped() {
        let hidden = [
            '\u{00AD}', '\u{061C}', '\u{180E}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{2028}', '\u{2029}', '\u{202A}', '\u{202E}', '\u{2060}', '\u{2064}',
            '\u{2066}', '\u{2069}', '\u{206F}', '\u{FEFF}', '\u{FFF9}', '\u{FFFB}', '\u{E0000}', '\u{E0041}', '\u{E007F}', '\u{0600}', '\u{0603}', '\u{0605}',
            '\u{06DD}', '\u{070F}', '\u{08E2}', '\u{110BD}', '\u{110CD}', '\u{1BCA0}', '\u{1BCA3}', '\u{1D173}', '\u{1D177}', '\u{1D17A}',
        ];
        for c in hidden {
            assert!(is_hidden_format(c), "U+{:04X}", c as u32);
            assert_eq!(shown(&format!("a{c}b")), "ab", "U+{:04X}", c as u32);
            assert_eq!(plain_notes(&format!("a{c}b\n\tc")), "ab\n\tc", "U+{:04X}", c as u32);
        }
        for c in [
            'a', '\u{4E2D}', ' ', '\u{200A}', '\u{2027}', '\u{2030}', '\u{2070}', '\u{FEFE}', '\u{FFFC}', '\u{DFFFF}', '\u{E0080}', '\u{05FF}', '\u{0606}', '\u{06DC}', '\u{06DE}', '\u{070E}',
            '\u{0710}', '\u{08E1}', '\u{08E3}', '\u{110BC}', '\u{110BE}', '\u{110CC}', '\u{110CE}', '\u{1BC9F}', '\u{1BCA4}', '\u{1D172}', '\u{1D17B}',
        ] {
            assert!(!is_hidden_format(c), "U+{:04X}", c as u32);
        }
        // A tag sequence spelling hidden text inside a line of notes.
        let tagged: String = "safe".chars().chain("evil".chars().map(|c| char::from_u32(0xE0000 + c as u32).unwrap())).chain("\u{2028}line".chars()).collect();
        assert_eq!(plain_notes(&tagged), "safeline");
    }

    #[test]
    fn hex32_takes_hex_digits_only() {
        assert_eq!(hex32(&"ab".repeat(32)), Some([0xab; 32]));
        assert_eq!(hex32(&"AB".repeat(32)), Some([0xab; 32]));
        for bad in [format!("+f{}", "0".repeat(62)), format!("-0{}", "0".repeat(62)), format!(" f{}", "0".repeat(62)), "g".repeat(64), "0".repeat(63)] {
            assert_eq!(hex32(&bad), None, "{bad}");
        }
    }

    #[test]
    fn swap_and_clean_old() {
        let dir = temp("swap");
        let (current, new, old, legacy) = (dir.join(EXE), dir.join(NEW), dir.join(OLD), dir.join(LEGACY_OLD));
        std::fs::write(&current, "current").unwrap();
        std::fs::write(&new, "new").unwrap();
        std::fs::write(&old, "stale").unwrap();
        std::fs::write(&legacy, "before the rename").unwrap();
        swap(&current, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&current).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "current");
        assert!(!new.exists());
        // No new exe: the running one is put back and kept.
        assert!(matches!(swap_checked(&current, &new), Err(Failed::Kept(_))));
        assert_eq!(std::fs::read_to_string(&current).unwrap(), "new");
        clean_old(&current);
        assert!(!old.exists() && !legacy.exists());
        clean_old(&current);
        let lost = Failed::Lost { old: old.clone(), error: "e".into() };
        assert!(lost.text().contains("Ascendant.old.exe") && lost.text().contains("may be missing"));
        assert!(!Failed::NotRestarted("e".into()).text().contains("kept"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The periodic check: an hour after any check, later only when GitHub asked for longer (at most a day), never
    /// sooner; due only while on, with nothing running.
    #[test]
    fn periodic_check_schedule() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        assert_eq!(CHECK_EVERY, Duration::from_secs(3600));
        assert_eq!(next_check(now, None), now + CHECK_EVERY);
        // A short Retry-After is no reason to retry sooner; a long one or a rate limit reset later is followed.
        assert_eq!(next_check(now, Some(Duration::from_secs(60))), now + CHECK_EVERY);
        assert_eq!(next_check(now, Some(Duration::ZERO)), now + CHECK_EVERY);
        assert_eq!(next_check(now, Some(Duration::from_secs(2 * 3600))), now + Duration::from_secs(2 * 3600));
        assert_eq!(next_check(now, Some(Duration::from_secs(9 * 24 * 3600))), now + http::LONGEST_WAIT);
        let due = Some(now);
        assert!(check_due(true, now, due, false));
        assert!(check_due(true, now + Duration::from_secs(1), due, false));
        assert!(!check_due(true, now, Some(now + Duration::from_secs(1)), false));
        assert!(!check_due(false, now, due, false), "off");
        assert!(!check_due(true, now, due, true), "a check or an update is running");
        assert!(!check_due(true, now, None, false), "nothing scheduled");
    }

    /// The wake-up for the periodic check is never a zero delay: none while busy (the busy thread repaints when it
    /// ends), none once due (the check starts instead), none while off.
    #[test]
    fn periodic_check_never_repaints_without_a_wait() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let later = Some(now + Duration::from_secs(90));
        assert_eq!(wake_in(true, now, later, false), Some(Duration::from_secs(90)));
        // Due already while a check or an update runs: no repaint every frame.
        assert_eq!(wake_in(true, now, Some(now), true), None);
        assert_eq!(wake_in(true, now + Duration::from_secs(5), Some(now), true), None);
        assert_eq!(wake_in(true, now, later, true), None);
        // Due and not busy: check_due starts the check; no zero-delay repaint either.
        assert_eq!(wake_in(true, now, Some(now), false), None);
        assert_eq!(wake_in(false, now, later, false), None);
        assert_eq!(wake_in(true, now, None, false), None);
        for (due, busy) in [(Some(now), true), (Some(now), false), (later, true), (later, false), (None, false)] {
            assert_ne!(wake_in(true, now, due, busy), Some(Duration::ZERO));
        }
    }

    /// A version a download refused as unverified is not offered again by a later check (304 or 200), and its ETag
    /// is not kept; other versions and outcomes pass unchanged.
    #[test]
    fn a_refused_version_is_not_offered_again() {
        let (release, _) = good();
        let seen = Some(Seen { etag: "\"v120\"".into(), release: Some(release.clone()) });
        let wait = Some(std::time::Duration::from_secs(7200));
        let answer = Checked { result: Ok(Some(release.clone())), seen: seen.clone(), wait };
        assert_eq!(drop_refused(answer.clone(), Some([1, 2, 0])), Checked { result: Ok(None), seen: None, wait });
        assert_eq!(drop_refused(answer.clone(), Some([1, 1, 0])), answer);
        assert_eq!(drop_refused(answer.clone(), None), answer);
        let none = Checked { result: Ok(None), seen: seen.clone(), wait: None };
        assert_eq!(drop_refused(none.clone(), Some([1, 2, 0])), none);
        let failed = Checked { result: Err(Problem::Network("x".into())), seen, wait: None };
        assert_eq!(drop_refused(failed.clone(), Some([1, 2, 0])), failed);
    }

    /// [`Fake`] for the API with an ETag: the API's answers carry `etag`, a request with that ETag gets a 304, and
    /// the If-None-Match of each API request is recorded.
    struct Tagged {
        fake: Fake,
        etag: &'static str,
        sent: RefCell<Vec<Option<String>>>,
    }

    impl Fetch for Tagged {
        fn fetch(&self, url: &str, accept: &str, limit: usize, deadline: std::time::Instant) -> Result<Hop, Fault> {
            self.fake.fetch(url, accept, limit, deadline)
        }

        fn fetch_tagged(&self, url: &str, accept: &str, etag: Option<&str>, limit: usize, deadline: std::time::Instant) -> Result<(Hop, Option<String>), Fault> {
            if url != LATEST {
                return self.fake.fetch(url, accept, limit, deadline).map(|hop| (hop, None));
            }
            self.sent.borrow_mut().push(etag.map(String::from));
            if etag == Some(self.etag) {
                self.fake.asked.borrow_mut().push(url.to_string());
                return Ok((Hop::NotModified, None));
            }
            self.fake.fetch(url, accept, limit, deadline).map(|hop| (hop, Some(self.etag.to_string())))
        }
    }

    /// A check keeps the API's ETag with its outcome, sends it next time, and on a 304 gives that outcome again
    /// without fetching anything else; a failure keeps the old pair, and a rate limit passes its wait on.
    #[test]
    fn check_sends_the_etag_and_reuses_a_304() {
        let tagged = |fake: Fake| Tagged { fake, etag: "\"v120\"", sent: RefCell::new(Vec::new()) };
        let (_, fake) = good();
        let api = tagged(with_api(fake, "v1.2.0"));
        let first = check_tagged(&api, "1.0.0", &test_public(), None);
        let offered = first.result.clone().unwrap().unwrap();
        assert_eq!(offered.version, [1, 2, 0]);
        let seen = first.seen.clone().unwrap();
        assert_eq!((seen.etag.as_str(), seen.release.as_ref()), ("\"v120\"", Some(&offered)));
        // The next check: If-None-Match goes out, the 304 gives the same release, and nothing else is fetched.
        api.fake.asked.borrow_mut().clear();
        let again = check_tagged(&api, "1.0.0", &test_public(), Some(seen.clone()));
        assert_eq!(again, Checked { result: Ok(Some(offered.clone())), seen: Some(seen.clone()), wait: None });
        assert_eq!(*api.sent.borrow(), [None, Some("\"v120\"".to_string())]);
        assert_eq!(*api.fake.asked.borrow(), [LATEST]);
        // "No newer release" is kept and reused the same way.
        let (_, fake) = good();
        let current = tagged(with_api(fake, "v1.0.0"));
        let none = check_tagged(&current, "1.0.0", &test_public(), None);
        assert_eq!((none.result.clone(), none.seen.clone().map(|s| s.release)), (Ok(None), Some(None)));
        assert_eq!(check_tagged(&current, "1.0.0", &test_public(), none.seen.clone()).result, Ok(None));
        // Rate limited: a network problem, the wait passed on, the old ETag kept for next time.
        let wait = std::time::Duration::from_secs(1800);
        let limited = tagged(Fake::default().failing(LATEST, Fault::Throttled("api.github.com: HTTP 429".into(), Some(wait))));
        let other = Seen { etag: "\"old\"".into(), release: None };
        let answer = check_tagged(&limited, "1.0.0", &test_public(), Some(other.clone()));
        assert_eq!(answer, Checked { result: Err(Problem::Network("api.github.com: HTTP 429".into())), seen: Some(other.clone()), wait: Some(wait) });
        assert_eq!(answer.result.unwrap_err().headline(true), "Can't check right now. Try again later.");
        // A failure after a changed answer keeps the old pair too (its ETag still matches its outcome).
        let (_, fake) = good();
        let broken = tagged(with_api(fake, "v1.2.0").failing(SIG_URL, Fault::Network("HTTP 502".into())));
        let answer = check_tagged(&broken, "1.0.0", &test_public(), Some(other.clone()));
        assert_eq!((answer.result, answer.seen, answer.wait), (Err(Problem::Network("HTTP 502".into())), Some(other), None));
        // Without the release key nothing is asked.
        let none_asked = tagged(Fake::default());
        assert_eq!(check_tagged(&none_asked, "1.0.0", &PLACEHOLDER_PUBLIC_KEY, None).result, Err(Problem::NoKey));
        assert!(none_asked.sent.borrow().is_empty());
    }

    /// The notes an update keeps: read back for that version only, through the same filter as a release's notes,
    /// and ignored when the file is too big, broken, or for another version.
    #[test]
    fn saved_notes_are_checked_when_read() {
        let dir = temp("saved-notes");
        let (mut release, _) = parse_release(&json("v1.2.0", &sha(EXE_BYTES), EXE_BYTES.len())).unwrap();
        release.notes = NOTES_SHOWN.to_string();
        assert_eq!(load_notes_in(&dir, "1.2.0"), None, "no file");
        save_notes_in(&dir, &release).unwrap();
        assert!(!dir.join(format!("{SAVED_NOTES}.part")).exists());
        let saved = SavedNotes { version: "1.2.0".into(), notes: NOTES_SHOWN.into() };
        assert_eq!(load_notes_in(&dir, "1.2.0"), Some(saved.clone()));
        assert_eq!(load_notes_in(&dir, "1.1.0"), None, "for another version");
        let write = |version: &str, notes: &str| std::fs::write(dir.join(SAVED_NOTES), serde_json::json!({"version": version, "notes": notes}).to_string()).unwrap();
        // Hidden and control characters are dropped as from a release.
        write("1.2.0", "\u{FEFF}- one\u{202E}\r\n- two\u{7}\u{200B}");
        assert_eq!(load_notes_in(&dir, "1.2.0").unwrap().notes, "- one\n- two");
        // 8000 characters at most, as the release script allows.
        write("1.2.0", &"\u{4E2D}".repeat(NOTES_LIMIT));
        assert_eq!(load_notes_in(&dir, "1.2.0").unwrap().notes.chars().count(), NOTES_LIMIT);
        write("1.2.0", &"\u{4E2D}".repeat(NOTES_LIMIT + 1));
        assert_eq!(load_notes_in(&dir, "1.2.0"), None, "over the limit");
        // Broken files: none of them shows anything, or panics.
        for bad in [&b"not json"[..], b"{\"version\":\"1.2.0\",\"notes\":\"cut", b"{\"version\":\"1.2.0\"}", b"[]", b"\xff\xfe", b""] {
            std::fs::write(dir.join(SAVED_NOTES), bad).unwrap();
            assert_eq!(load_notes_in(&dir, "1.2.0"), None, "{}", String::from_utf8_lossy(bad));
        }
        write("v1.2.0", "- one");
        assert_eq!(load_notes_in(&dir, "v1.2.0"), None, "not X.Y.Z");
        write("1.2.0", " \n\u{200B}\t ");
        assert_eq!(load_notes_in(&dir, "1.2.0"), None, "empty once filtered");
        std::fs::write(dir.join(SAVED_NOTES), vec![b' '; SMALL_LIMIT + 1]).unwrap();
        assert_eq!(load_notes_in(&dir, "1.2.0"), None, "over the file size limit");
        // A folder that cannot be written: an error, which save_notes drops.
        let file = dir.join("a-file");
        std::fs::write(&file, "x").unwrap();
        assert!(save_notes_in(&file, &release).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// "Updated to X.Y.Z" shows for the running version's saved notes until that version is dismissed.
    #[test]
    fn updated_notice_conditions() {
        let saved = SavedNotes { version: "1.2.0".into(), notes: "- one".into() };
        assert!(show_updated(Some(&saved), "1.2.0", ""));
        assert!(show_updated(Some(&saved), "1.2.0", "1.1.0"), "dismissed for an older version only");
        assert!(!show_updated(Some(&saved), "1.2.0", "1.2.0"), "dismissed");
        assert!(!show_updated(Some(&saved), "1.2.1", ""), "another version runs");
        assert!(!show_updated(None, "1.2.0", ""), "nothing saved");
    }
}
