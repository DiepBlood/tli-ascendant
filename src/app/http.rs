//! HTTP over Win32 WinHTTP (winhttp.dll, synchronous): the update check's and download's GETs and the run log
//! upload's POST.
//!
//! Not the WinRT HttpClient any more: it runs on WinINet, which on a computer whose proxy setting is only "Automatically
//! detect settings" (WPAD) fails every request once the detection fails (0x80072EFD, seen 2026-11-07), while WinHTTP,
//! curl and .NET on that computer connect. Each request first goes through WinHTTP's automatic proxy
//! (`WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY`: WPAD, a PAC script or the system's settings) and, when that fails before
//! any answer comes back ([`falls_back`]), once more with no proxy ([`with_fallback`]). TLS 1.2 or later only, the
//! certificate checked by Windows; WinHTTP keeps no HTTP cache, and cookies are turned off. A company proxy that
//! answers 407 is answered once with the user's Windows credentials (Negotiate or NTLM only, see [`answer`]).
//!
//! No request lets WinHTTP follow redirects: for GETs [`WinHttp`] answers one hop at a time and [`get_limited`]
//! follows the redirects itself, checking every URL (HTTPS, an allowed host) before it is requested and capping
//! the body; the POST ([`post`]) goes only to an https:// URL, follows no redirect and caps the response it reads.
//! No cookies are sent or kept, and every request has a time limit: a GET [`HEADERS_TIMEOUT`] for each response's
//! headers and [`TOTAL_TIMEOUT`] in all, counted from the first request across every redirect it follows; the
//! POST [`TOTAL_TIMEOUT`] in all. URLs from the network go into error messages only through [`shown`].
//!
//! A conditional GET ([`get_conditional`], for the update check) is the same GET with `If-None-Match` on its first
//! request: the same host, scheme, size and time rules hold, and the ETag is sent only when it is plain visible
//! ASCII ([`usable_etag`]). A 403 or 429 answer is [`Fault::Throttled`], with the wait the server asked for
//! ([`server_wait`]).

use super::update::shown;
use crate::lang::trf;
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::Win32::Networking::WinHttp::{
    ERROR_WINHTTP_AUTO_PROXY_SERVICE_ERROR, ERROR_WINHTTP_AUTODETECTION_FAILED, ERROR_WINHTTP_BAD_AUTO_PROXY_SCRIPT, ERROR_WINHTTP_CANNOT_CONNECT, ERROR_WINHTTP_CONNECTION_ERROR,
    ERROR_WINHTTP_NAME_NOT_RESOLVED, ERROR_WINHTTP_RESEND_REQUEST, ERROR_WINHTTP_TIMEOUT, ERROR_WINHTTP_UNABLE_TO_DOWNLOAD_SCRIPT, INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_AUTH_SCHEME_NEGOTIATE, WINHTTP_AUTH_SCHEME_NTLM, WINHTTP_AUTH_TARGET_PROXY, WINHTTP_AUTOLOGON_SECURITY_LEVEL_LOW, WINHTTP_DISABLE_COOKIES,
    WINHTTP_DISABLE_REDIRECTS, WINHTTP_FLAG_SECURE, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3, WINHTTP_OPTION_AUTOLOGON_POLICY, WINHTTP_OPTION_CONNECT_TIMEOUT,
    WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT, WINHTTP_OPTION_RECEIVE_TIMEOUT, WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
    WINHTTP_OPTION_RESOLVE_TIMEOUT, WINHTTP_OPTION_SECURE_PROTOCOLS, WINHTTP_OPTION_SEND_TIMEOUT, WINHTTP_QUERY_CUSTOM, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle,
    WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryAuthSchemes, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetCredentials,
    WinHttpSetOption,
};
use windows::core::{HSTRING, PCWSTR};

/// "ascendant/X.Y.Z", sent as the User-Agent.
const USER_AGENT: &str = concat!("ascendant/", env!("CARGO_PKG_VERSION"));

/// At most this many redirects are followed for one GET.
const MAX_REDIRECTS: usize = 5;

/// How long a GET may wait for the response's headers (connecting included).
pub const HEADERS_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one GET may take in all, every redirect and the body included; also the POST's limit.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Why a GET gave no body.
#[derive(Clone, Debug, PartialEq)]
pub enum Fault {
    /// The network or the server failed (no connection, a time limit, an HTTP error): worth trying again later.
    Network(String),
    /// The answer broke a rule (scheme, host, size, endless redirects): never to be trusted.
    Refused(String),
    /// The server said no for now (HTTP 403 or 429, as GitHub answers past its rate limit), with how long it asked
    /// to wait when it said (Retry-After or X-RateLimit-Reset, see [`server_wait`]): worth trying again only later.
    Throttled(String, Option<Duration>),
}

impl Fault {
    /// The wait a [`Fault::Throttled`] asked for, if any.
    pub fn wait(&self) -> Option<Duration> {
        match self {
            Fault::Throttled(_, wait) => *wait,
            _ => None,
        }
    }
}

/// The longest wait [`server_wait`] takes from a server: a later time is cut to this.
pub const LONGEST_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a server asked to wait, from its `Retry-After` (seconds; an HTTP date is not read) and its
/// `X-RateLimit-Reset` (the Unix time in seconds the limit resets, against `now_unix`): the longer of the two, at
/// most [`LONGEST_WAIT`]; None when neither gives a usable value.
pub fn server_wait(retry_after: Option<&str>, reset: Option<&str>, now_unix: u64) -> Option<Duration> {
    let seconds = |v: Option<&str>| v.map(str::trim).filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())).and_then(|v| v.parse::<u64>().ok());
    let retry = seconds(retry_after);
    let reset = seconds(reset).map(|at| at.saturating_sub(now_unix));
    retry.into_iter().chain(reset).max().map(|s| Duration::from_secs(s).min(LONGEST_WAIT))
}

/// The longest ETag sent back or kept.
const ETAG_LIMIT: usize = 200;

/// Whether `etag` may be sent back as `If-None-Match`: 1 to [`ETAG_LIMIT`] bytes of visible ASCII (no spaces, no
/// control characters, so it cannot add a header line).
pub fn usable_etag(etag: &str) -> bool {
    !etag.is_empty() && etag.len() <= ETAG_LIMIT && etag.bytes().all(|b| b.is_ascii_graphic())
}

/// One response, redirects not followed.
#[derive(Clone, Debug)]
pub enum Hop {
    Body(Vec<u8>),
    /// A 3xx answer and its Location header, as sent (it may be relative).
    Redirect(String),
    /// 304 Not Modified, the answer to a request sent with `If-None-Match`.
    NotModified,
}

/// One GET without following redirects; a body over `limit` bytes is an error (and is not read past the limit), and
/// so is still waiting at `deadline` (shared by all the hops of one [`get_limited`]). Implemented by [`WinHttp`], and
/// by fakes in tests.
pub trait Fetch {
    fn fetch(&self, url: &str, accept: &str, limit: usize, deadline: Instant) -> Result<Hop, Fault>;

    /// [`Fetch::fetch`] with `If-None-Match: etag` when `etag` is Some (and [`usable_etag`]), and the response's
    /// ETag with it. The default sends no `If-None-Match` and gives no ETag, which only costs the saving.
    fn fetch_tagged(&self, url: &str, accept: &str, etag: Option<&str>, limit: usize, deadline: Instant) -> Result<(Hop, Option<String>), Fault> {
        let _ = etag;
        self.fetch(url, accept, limit, deadline).map(|hop| (hop, None))
    }
}

/// The answer of a [`get_conditional`].
#[derive(Clone, Debug, PartialEq)]
pub enum Got {
    /// A body (within its limit) and its ETag, when the server gave a [`usable_etag`].
    Body { body: Vec<u8>, etag: Option<String> },
    /// 304: unchanged since the ETag sent.
    NotModified,
}

/// The host of an `https://` URL, lowercased; an error for any other scheme, a user-info part (`user@host`) or
/// an explicit port.
pub fn https_host(url: &str) -> Result<String, String> {
    let rest = url.get(..8).filter(|s| s.eq_ignore_ascii_case("https://")).map(|_| &url[8..]).ok_or_else(|| trf!("{} is not an https:// address", shown(url)))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains(['@', ':', '\\']) || !authority.is_ascii() || authority.chars().any(|c| c.is_ascii_control()) {
        return Err(trf!("{} is not an allowed address", shown(url)));
    }
    Ok(authority.to_ascii_lowercase())
}

/// `location` (a redirect's target) against `from`: absolute https URLs as they are, `/path` on `from`'s host.
fn resolve(from: &str, location: &str) -> Result<String, String> {
    if location.starts_with('/') && !location.starts_with("//") {
        return Ok(format!("https://{}{location}", https_host(from)?));
    }
    Ok(location.to_string())
}

/// The body of a GET of `url` through `fetch`, following at most [`MAX_REDIRECTS`] redirects, all within
/// [`TOTAL_TIMEOUT`] from now. Every URL, the first and each redirect's target, must be https:// on one of `allowed`
/// exactly, or nothing is requested from it; the body must be at most `limit` bytes.
pub fn get_limited(fetch: &dyn Fetch, url: &str, accept: &str, limit: usize, allowed: &[&str]) -> Result<Vec<u8>, Fault> {
    get_until(fetch, url, accept, limit, allowed, Instant::now() + TOTAL_TIMEOUT)
}

/// [`get_limited`] with the time limit of the whole GET, redirects included, given as `deadline`.
fn get_until(fetch: &dyn Fetch, url: &str, accept: &str, limit: usize, allowed: &[&str], deadline: Instant) -> Result<Vec<u8>, Fault> {
    match get_tagged_until(fetch, url, accept, limit, allowed, None, deadline)? {
        Got::Body { body, .. } => Ok(body),
        // Never asked for (no If-None-Match was sent); get_tagged_until refuses it already.
        Got::NotModified => Err(Fault::Network(format!("{}: HTTP 304", shown(url)))),
    }
}

/// [`get_limited`] sent with `If-None-Match: etag` (on the first request only, and only a [`usable_etag`]): the body
/// and its ETag, or [`Got::NotModified`]. Every rule of [`get_limited`] holds the same: https:// on one of `allowed`
/// for every URL, at most `limit` bytes, [`TOTAL_TIMEOUT`] for the whole GET.
pub fn get_conditional(fetch: &dyn Fetch, url: &str, accept: &str, limit: usize, allowed: &[&str], etag: Option<&str>) -> Result<Got, Fault> {
    get_tagged_until(fetch, url, accept, limit, allowed, etag, Instant::now() + TOTAL_TIMEOUT)
}

/// The GET of [`get_limited`] and [`get_conditional`], with its deadline given.
fn get_tagged_until(fetch: &dyn Fetch, url: &str, accept: &str, limit: usize, allowed: &[&str], etag: Option<&str>, deadline: Instant) -> Result<Got, Fault> {
    let first = url;
    let mut url = url.to_string();
    // The ETag belongs to the first URL: a redirect's target is asked without it.
    let mut etag = etag.filter(|e| usable_etag(e));
    for _ in 0..=MAX_REDIRECTS {
        let host = https_host(&url).map_err(Fault::Refused)?;
        if !allowed.contains(&host.as_str()) {
            return Err(Fault::Refused(trf!("refused to connect to {}: only GitHub's hosts are allowed", shown(&host))));
        }
        if Instant::now() >= deadline {
            return Err(Fault::Network(trf!("{}: timed out", shown(&url))));
        }
        let sent = etag.take();
        match fetch.fetch_tagged(&url, accept, sent, limit, deadline)? {
            (Hop::Body(body), _) if body.len() > limit => return Err(Fault::Refused(trf!("{}: the file is over the size limit of {} bytes", shown(&url), limit))),
            // Only the first URL's ETag is kept: it is the one sent next time.
            (Hop::Body(body), tag) => return Ok(Got::Body { body, etag: tag.filter(|t| url == first && usable_etag(t)) }),
            (Hop::NotModified, _) if sent.is_some() => return Ok(Got::NotModified),
            (Hop::NotModified, _) => return Err(Fault::Network(format!("{}: HTTP 304", shown(&url)))),
            (Hop::Redirect(location), _) => url = resolve(&url, &location).map_err(Fault::Refused)?,
        }
    }
    Err(Fault::Refused(trf!("{}: too many redirects", shown(&url))))
}

/// The Win32 error code in `e` (GetLastError's value, which the windows crate wraps as HRESULT_FROM_WIN32), or the
/// HRESULT itself when it is not a Win32 error.
fn code_of(e: &windows::core::Error) -> u32 {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 { hr & 0xFFFF } else { hr }
}

/// The calling thread's last error, for the WinHTTP calls that answer a null handle.
fn last_code() -> u32 {
    code_of(&windows::core::Error::from_thread())
}

/// "WinHTTP <code> <Windows' text for it>": the text from winhttp.dll's message table (its 12xxx codes are not in
/// the system's), else the system's, in the user's language; the code alone when neither has one.
fn describe(code: u32) -> String {
    use windows::Win32::System::Diagnostics::Debug::{FORMAT_MESSAGE_FROM_HMODULE, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::core::{PWSTR, w};
    // SAFETY: GetModuleHandleW only looks up an already loaded module (winhttp.dll is an import of this exe), and
    // FormatMessageW writes at most the buffer's length into the buffer, which outlives the call.
    let text = unsafe {
        let module = GetModuleHandleW(w!("winhttp.dll")).ok();
        let mut flags = FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS;
        if module.is_some() {
            flags |= FORMAT_MESSAGE_FROM_HMODULE;
        }
        let mut buf = [0u16; 512];
        let n = FormatMessageW(flags, module.map(|m| m.0 as *const c_void), code, 0, PWSTR(buf.as_mut_ptr()), buf.len() as u32, None) as usize;
        String::from_utf16_lossy(&buf[..n.min(buf.len())])
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() { format!("WinHTTP {code}") } else { format!("WinHTTP {code} {text}") }
}

/// Which proxy setting one attempt of a request uses.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    /// `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY` (Windows 8.1 and later): WPAD, a PAC script or the system's settings.
    Automatic,
    /// `WINHTTP_ACCESS_TYPE_NO_PROXY`: straight to the server.
    Direct,
}

/// Whether a request that failed with `code` through the automatic proxy is sent once more with no proxy: the
/// failures of finding a proxy (WPAD, the PAC script) or of reaching the server or a proxy, before any HTTP answer
/// came back. A request that is not `repeatable` (a POST) is sent again only when the failure was on the way to the
/// server (no name, no connection, no proxy), never after a time out or a broken connection, when the body may have
/// arrived. Anything else (a certificate refused, an invalid argument) fails the same either way.
fn falls_back(code: u32, repeatable: bool) -> bool {
    match code {
        ERROR_WINHTTP_NAME_NOT_RESOLVED
        | ERROR_WINHTTP_CANNOT_CONNECT
        | ERROR_WINHTTP_AUTO_PROXY_SERVICE_ERROR
        | ERROR_WINHTTP_AUTODETECTION_FAILED
        | ERROR_WINHTTP_BAD_AUTO_PROXY_SCRIPT
        | ERROR_WINHTTP_UNABLE_TO_DOWNLOAD_SCRIPT => true,
        ERROR_WINHTTP_TIMEOUT | ERROR_WINHTTP_CONNECTION_ERROR => repeatable,
        _ => false,
    }
}

/// A request that got no answer: the WinHTTP error of the attempt through the automatic proxy, and of the one with
/// no proxy when it was made.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Failed {
    automatic: u32,
    direct: Option<u32>,
}

impl std::fmt::Display for Failed {
    /// "WinHTTP 12180 <text>", then " -> WinHTTP 12029 <text>" for the attempt with no proxy when its error differs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&describe(self.automatic))?;
        match self.direct {
            Some(direct) if direct != self.automatic => write!(f, " -> {}", describe(direct)),
            _ => Ok(()),
        }
    }
}

/// `attempt` through the automatic proxy, and once more with no proxy when it failed in a way [`falls_back`] takes.
/// `attempt` gives Ok as soon as an HTTP answer came back, whatever its status, so an HTTP error is never sent again.
fn with_fallback<T>(repeatable: bool, mut attempt: impl FnMut(Route) -> Result<T, u32>) -> Result<T, Failed> {
    match attempt(Route::Automatic) {
        Ok(answer) => Ok(answer),
        Err(code) if falls_back(code, repeatable) => attempt(Route::Direct).map_err(|direct| Failed { automatic: code, direct: Some(direct) }),
        Err(code) => Err(Failed { automatic: code, direct: None }),
    }
}

/// The host and the path (with its query, without a fragment; "/" when empty) of an `https://` URL that
/// [`https_host`] takes; its error otherwise.
fn split_url(url: &str) -> Result<(String, String), String> {
    let host = https_host(url)?;
    let rest = &url[8..];
    let path = &rest[rest.find(['/', '?', '#']).unwrap_or(rest.len())..];
    let path = path.split('#').next().unwrap_or_default();
    Ok((host, if path.starts_with('/') { path.to_string() } else { format!("/{path}") }))
}

/// The milliseconds from now to `at`, at least 1 (WinHTTP takes 0 as "no time limit").
fn ms_until(at: Instant) -> u32 {
    at.saturating_duration_since(Instant::now()).as_millis().clamp(1, i32::MAX as u128) as u32
}

/// A WinHTTP handle, closed when dropped.
struct Handle(*mut c_void);

impl Handle {
    /// `raw`, or the thread's last error when it is null.
    fn new(raw: *mut c_void) -> Result<Handle, u32> {
        if raw.is_null() { Err(last_code()) } else { Ok(Handle(raw)) }
    }

    /// Sets the DWORD option `option` to `value`.
    fn set(&self, option: u32, value: u32) -> Result<(), u32> {
        // SAFETY: the handle is open; the option's buffer is a DWORD that outlives the call.
        unsafe { WinHttpSetOption(Some(self.0), option, Some(&value.to_ne_bytes())) }.map_err(|e| code_of(&e))
    }

    /// Sets every time limit of this request handle to the time left until `by`, just before a call that waits on
    /// the network; a timeout error, and nothing set, when that time is already up.
    ///
    /// Known limit: WinHTTP applies each of these limits to its own phase, not to the call as a whole. One
    /// WinHttpSendRequest resolves the name, connects, does the TLS handshake and sends, each phase allowed the full
    /// time left, and the proxy discovery of `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY` (WPAD, the PAC script) runs under
    /// WinHTTP's own limits, so one call can end after `by`. The deadline is checked again before every following
    /// call, so no call starts after it; cutting a call short would need closing its handle from another thread,
    /// which WinHTTP documents only for asynchronous requests.
    fn limit_to(&self, by: Instant) -> Result<(), u32> {
        if Instant::now() >= by {
            return Err(ERROR_WINHTTP_TIMEOUT);
        }
        let ms = ms_until(by);
        for option in [WINHTTP_OPTION_RESOLVE_TIMEOUT, WINHTTP_OPTION_CONNECT_TIMEOUT, WINHTTP_OPTION_SEND_TIMEOUT, WINHTTP_OPTION_RECEIVE_TIMEOUT, WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT] {
            self.set(option, ms)?;
        }
        Ok(())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from WinHTTP and is closed once, here.
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// The longest response header [`Request::header`] reads, in bytes (UTF-16).
const HEADER_LIMIT: u32 = 32 << 10;

/// The scheme to answer a proxy's 407 with the current user's default credentials: Negotiate (Kerberos, else NTLM),
/// then NTLM, the two schemes default credentials work with (the automatic logon policy "only applies to the NTLM
/// and Negotiate authentication schemes", Microsoft Learn, "Authentication in WinHTTP"). None for any other offer
/// (Basic, Digest), which would need a name and password this app never asks for.
fn proxy_scheme(supported: u32) -> Option<u32> {
    [WINHTTP_AUTH_SCHEME_NEGOTIATE.0, WINHTTP_AUTH_SCHEME_NTLM.0].into_iter().find(|s| supported & s != 0)
}

/// The steps of one request on an open request handle that [`answer`] puts together, behind a trait so that the
/// proxy authentication is tested without a proxy.
trait Exchange {
    /// Sends the request and waits for its response's headers, until `by`.
    fn round_trip(&self, by: Instant) -> Result<(), u32>;
    /// The response's status code.
    fn status(&self) -> Result<u16, u32>;
    /// Sets the current user's default credentials for the proxy that answered 407, for the next round trip; false,
    /// and nothing set, when the 407 is not a proxy's or offers no scheme default credentials work with
    /// ([`proxy_scheme`]).
    fn proxy_credentials(&self) -> Result<bool, u32>;
}

/// The status of a request sent through `x`: when a proxy answers 407 (as an NTLM or Kerberos company proxy does
/// before it knows the user) and takes the default credentials, the request is sent once more with them, as WinINet
/// did on its own; that second answer is final, a second 407 included. A server's 401 is never answered.
fn answer(x: &impl Exchange, by: Instant) -> Result<u16, u32> {
    x.round_trip(by)?;
    let status = x.status()?;
    if status != 407 || !x.proxy_credentials()? {
        return Ok(status);
    }
    x.round_trip(by)?;
    x.status()
}

/// A request being sent: its handle, header lines and body.
struct Sending<'a> {
    request: &'a Handle,
    lines: &'a [u16],
    body: &'a [u8],
}

impl Exchange for Sending<'_> {
    fn round_trip(&self, by: Instant) -> Result<(), u32> {
        let length = u32::try_from(self.body.len()).map_err(|_| windows::Win32::Foundation::ERROR_INVALID_PARAMETER.0)?;
        // ERROR_WINHTTP_RESEND_REQUEST asks for the same request again (a multi-step authentication on a new
        // connection, per the WinHTTP authentication docs): honoured at most twice, so this never loops.
        let mut resent = 0;
        loop {
            self.request.limit_to(by)?;
            // SAFETY: the header text and the body outlive the call, which reads `length` bytes of the body.
            let sent = unsafe {
                WinHttpSendRequest(
                    self.request.0,
                    (!self.lines.is_empty()).then_some(self.lines),
                    (!self.body.is_empty()).then_some(self.body.as_ptr().cast()),
                    length,
                    length,
                    0,
                )
            }
            .map_err(|e| code_of(&e));
            let done = sent.and_then(|()| {
                self.request.limit_to(by)?;
                // SAFETY: the request was sent; the reserved argument must be null.
                unsafe { WinHttpReceiveResponse(self.request.0, std::ptr::null_mut()) }.map_err(|e| code_of(&e))
            });
            match done {
                Err(ERROR_WINHTTP_RESEND_REQUEST) if resent < 2 => resent += 1,
                done => return done,
            }
        }
    }

    fn status(&self) -> Result<u16, u32> {
        let (mut status, mut size) = (0u32, 4u32);
        // SAFETY: the buffer is the DWORD `status`, `size` bytes long; both outlive the call.
        unsafe { WinHttpQueryHeaders(self.request.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some((&raw mut status).cast()), &mut size, std::ptr::null_mut()) }
            .map_err(|e| code_of(&e))?;
        u16::try_from(status).map_err(|_| windows::Win32::Foundation::ERROR_INVALID_DATA.0)
    }

    fn proxy_credentials(&self) -> Result<bool, u32> {
        let (mut supported, mut first, mut target) = (0u32, 0u32, 0u32);
        // SAFETY: the three DWORDs outlive the call, which writes only them.
        unsafe { WinHttpQueryAuthSchemes(self.request.0, &mut supported, &mut first, &mut target) }.map_err(|e| code_of(&e))?;
        let Some(scheme) = proxy_scheme(supported).filter(|_| target == WINHTTP_AUTH_TARGET_PROXY) else { return Ok(false) };
        // The automatic logon policy decides where default credentials may go. Its default, MEDIUM, sends them
        // "only for requests on the local Intranet"; LOW "for all requests" (Microsoft Learn, "Option flags",
        // WINHTTP_OPTION_AUTOLOGON_POLICY, which "applies only to the request handle"). A company proxy is often not
        // in the intranet zone, so this request alone is set to LOW, and only after a proxy (the one Windows' own
        // proxy settings chose) asked: as WinINet answered such a proxy. Credentials are set for the proxy target
        // only; a server is never given any.
        self.request.set(WINHTTP_OPTION_AUTOLOGON_POLICY, WINHTTP_AUTOLOGON_SECURITY_LEVEL_LOW)?;
        // SAFETY: null user name and password select the current user's default credentials; the reserved
        // argument must be null.
        unsafe { WinHttpSetCredentials(self.request.0, WINHTTP_AUTH_TARGET_PROXY, scheme, PCWSTR::null(), PCWSTR::null(), std::ptr::null_mut()) }.map_err(|e| code_of(&e))?;
        Ok(true)
    }
}

/// A response whose headers have arrived: [`Request`] from WinHTTP, fakes in tests.
trait Response {
    /// The status code.
    fn status(&self) -> u16;
    /// The first `name` header.
    fn header(&self, name: &str) -> Option<String>;
    /// Reads the next bytes of the body into `buf`, waiting at most until `by`; 0 at its end.
    fn read(&self, buf: &mut [u8], by: Instant) -> Result<usize, u32>;
}

/// Sends a request and waits for its response's headers: [`System`] over WinHTTP, fakes in tests, so that the GET
/// and the POST are tested to go through [`with_fallback`].
trait Open {
    type Answer: Response;
    /// `verb` for `path` on `host` by `route`, with `headers` and `body`; the headers by `respond_by`.
    #[allow(clippy::too_many_arguments)]
    fn open(&self, route: Route, host: &str, path: &str, verb: &str, headers: &[(&str, &str)], body: &[u8], respond_by: Instant) -> Result<Self::Answer, u32>;
}

/// [`Open`] over Windows' WinHTTP ([`Request::send`]).
struct System;

impl Open for System {
    type Answer = Request;

    fn open(&self, route: Route, host: &str, path: &str, verb: &str, headers: &[(&str, &str)], body: &[u8], respond_by: Instant) -> Result<Request, u32> {
        Request::send(route, host, path, verb, headers, body, respond_by)
    }
}

/// A request whose response's headers have arrived. The handles drop in order: the request, its connection, then
/// its session.
struct Request {
    status: u16,
    request: Handle,
    _connect: Handle,
    _session: Handle,
}

impl Request {
    /// Sends `verb` for `path` on `host` (HTTPS, port 443) by `route`, with the User-Agent, `headers` and `body`,
    /// and waits for the response's headers until `respond_by` (answering a proxy's 407 once, see [`answer`]). A new
    /// session per request, so nothing (no connection, no proxy answer) is shared between them. Redirects and cookies
    /// are turned off, TLS 1.2 or later is required (1.3 where Windows' WinHTTP offers it). The WinHTTP error when
    /// no answer came.
    fn send(route: Route, host: &str, path: &str, verb: &str, headers: &[(&str, &str)], body: &[u8], respond_by: Instant) -> Result<Request, u32> {
        if Instant::now() >= respond_by {
            return Err(ERROR_WINHTTP_TIMEOUT);
        }
        // A header with a line break would add a header line; none is expected, so none is sent.
        if headers.iter().any(|(n, v)| n.chars().chain(v.chars()).any(|c| c.is_control())) {
            return Err(windows::Win32::Foundation::ERROR_INVALID_PARAMETER.0);
        }
        let access = match route {
            Route::Automatic => WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            Route::Direct => WINHTTP_ACCESS_TYPE_NO_PROXY,
        };
        let (agent, host, verb, path) = (HSTRING::from(USER_AGENT), HSTRING::from(host), HSTRING::from(verb), HSTRING::from(path));
        // SAFETY: every string passed is an HSTRING that outlives the call; the null pointers are the documented
        // "none" values (no proxy name or bypass list, the default HTTP version, no referrer, no accept types).
        let session = Handle::new(unsafe { WinHttpOpen(PCWSTR(agent.as_ptr()), access, PCWSTR::null(), PCWSTR::null(), 0) })?;
        if session.set(WINHTTP_OPTION_SECURE_PROTOCOLS, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3).is_err() {
            // Windows 10's WinHTTP refuses the TLS 1.3 flag.
            session.set(WINHTTP_OPTION_SECURE_PROTOCOLS, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2)?;
        }
        session.set(WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER)?;
        // SAFETY: as for WinHttpOpen above. WinHttpConnect and WinHttpOpenRequest do not touch the network; the time
        // limits are set on the request before each call that does (Handle::limit_to).
        let connect = Handle::new(unsafe { WinHttpConnect(session.0, PCWSTR(host.as_ptr()), INTERNET_DEFAULT_HTTPS_PORT, 0) })?;
        let request = Handle::new(unsafe { WinHttpOpenRequest(connect.0, PCWSTR(verb.as_ptr()), PCWSTR(path.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), WINHTTP_FLAG_SECURE) })?;
        request.set(WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_REDIRECTS)?;
        let lines: Vec<u16> = headers.iter().map(|(n, v)| format!("{n}: {v}\r\n")).collect::<String>().encode_utf16().collect();
        let status = answer(&Sending { request: &request, lines: &lines, body }, respond_by)?;
        Ok(Request { status, request, _connect: connect, _session: session })
    }
}

impl Response for Request {
    fn status(&self) -> u16 {
        self.status
    }

    /// None when it is missing or longer than [`HEADER_LIMIT`].
    fn header(&self, name: &str) -> Option<String> {
        let name = HSTRING::from(name);
        let mut size = 0u32;
        // SAFETY: with no buffer, WinHTTP only writes the size it needs (in bytes) into `size`.
        let sized = unsafe { WinHttpQueryHeaders(self.request.0, WINHTTP_QUERY_CUSTOM, PCWSTR(name.as_ptr()), None, &mut size, std::ptr::null_mut()) };
        if sized.is_ok() || size == 0 || size > HEADER_LIMIT {
            return None;
        }
        let mut buf = vec![0u16; size.div_ceil(2) as usize];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: the buffer is `size` bytes long and outlives the call, which writes at most that.
        unsafe { WinHttpQueryHeaders(self.request.0, WINHTTP_QUERY_CUSTOM, PCWSTR(name.as_ptr()), Some(buf.as_mut_ptr().cast()), &mut size, std::ptr::null_mut()) }.ok()?;
        Some(String::from_utf16_lossy(&buf[..(size as usize / 2).min(buf.len())]))
    }

    fn read(&self, buf: &mut [u8], by: Instant) -> Result<usize, u32> {
        if Instant::now() >= by {
            return Err(ERROR_WINHTTP_TIMEOUT);
        }
        self.request.set(WINHTTP_OPTION_RECEIVE_TIMEOUT, ms_until(by))?;
        let mut n = 0u32;
        let want = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: WinHTTP writes at most `want` bytes into `buf`, which outlives the call.
        unsafe { WinHttpReadData(self.request.0, buf.as_mut_ptr().cast(), want, &mut n) }.map_err(|e| code_of(&e))?;
        Ok(n as usize)
    }
}

/// The body of `response`, as [`read_body`] reads it.
fn body_of(response: &impl Response, limit: usize, deadline: Instant) -> Result<Result<Vec<u8>, Cut>, u32> {
    read_body(response.header("Content-Length").as_deref(), limit, deadline, |buf, by| response.read(buf, by))
}

/// Why [`read_body`] did not give a whole body.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cut {
    /// Over the limit, by its Content-Length or as it arrived.
    TooBig,
    /// Still arriving at the deadline.
    TimedOut,
    /// The body ended before the length its Content-Length gave (the connection closed early).
    Short { got: usize, expected: u64 },
}

/// A body of at most `limit` bytes through `read` (which fills the buffer it is given, waiting at most until the
/// instant it is given, and gives 0 at the end), read by `deadline`: refused at once when its `content_length` is
/// over the limit, never read more than one byte past the limit when it does not give one, and [`Cut::Short`] when
/// it ends before its `content_length`. A WinHTTP time out is [`Cut::TimedOut`]; any other WinHTTP error is given
/// back.
fn read_body(content_length: Option<&str>, limit: usize, deadline: Instant, mut read: impl FnMut(&mut [u8], Instant) -> Result<usize, u32>) -> Result<Result<Vec<u8>, Cut>, u32> {
    let declared = content_length.and_then(|n| n.trim().parse::<u64>().ok());
    if declared.is_some_and(|n| n > limit as u64) {
        return Ok(Err(Cut::TooBig));
    }
    let mut bytes = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Ok(Err(Cut::TimedOut));
        }
        // One byte past the limit is enough to know the body is over it.
        let start = bytes.len();
        let room = (limit + 1 - start).min(64 << 10);
        bytes.resize(start + room, 0);
        let n = match read(&mut bytes[start..], deadline) {
            Ok(n) => n.min(room),
            Err(ERROR_WINHTTP_TIMEOUT) => return Ok(Err(Cut::TimedOut)),
            Err(code) => return Err(code),
        };
        bytes.truncate(start + n);
        if n == 0 {
            return Ok(match declared {
                Some(expected) if (bytes.len() as u64) < expected => Err(Cut::Short { got: bytes.len(), expected }),
                _ => Ok(bytes),
            });
        }
        if bytes.len() > limit {
            return Ok(Err(Cut::TooBig));
        }
    }
}

/// The message for a body cut short: what arrived against what the Content-Length promised.
fn short_body(shown_url: &str, got: usize, expected: u64) -> String {
    format!("{shown_url}: {got} / Content-Length {expected}")
}

/// [`Fetch`] over WinHTTP, with automatic redirects and cookies turned off (WinHTTP has no HTTP cache), through the
/// automatic proxy and then with none ([`with_fallback`]).
pub struct WinHttp;

impl Fetch for WinHttp {
    fn fetch(&self, url: &str, accept: &str, limit: usize, deadline: Instant) -> Result<Hop, Fault> {
        self.fetch_tagged(url, accept, None, limit, deadline).map(|(hop, _)| hop)
    }

    fn fetch_tagged(&self, url: &str, accept: &str, etag: Option<&str>, limit: usize, deadline: Instant) -> Result<(Hop, Option<String>), Fault> {
        get_hop(&System, url, accept, etag, limit, deadline)
    }
}

/// [`WinHttp`]'s GET of one hop, its requests sent through `open`.
fn get_hop(open: &impl Open, url: &str, accept: &str, etag: Option<&str>, limit: usize, deadline: Instant) -> Result<(Hop, Option<String>), Fault> {
    let shown_url = shown(url);
    let too_big = || Fault::Refused(trf!("{}: the file is over the size limit of {} bytes", shown_url, limit));
    let timed_out = || Fault::Network(trf!("{}: timed out", shown_url));
    let failed = |why: &dyn std::fmt::Display| Fault::Network(format!("{shown_url}: {why}"));
    let (host, path) = split_url(url).map_err(Fault::Refused)?;
    let etag = etag.filter(|e| usable_etag(e));
    let mut headers = vec![("Accept", accept)];
    if let Some(etag) = etag {
        headers.push(("If-None-Match", etag));
    }
    // Each attempt gets its headers within HEADERS_TIMEOUT, and everything ends by the whole GET's deadline.
    let response = with_fallback(true, |route| open.open(route, &host, &path, "GET", &headers, &[], (Instant::now() + HEADERS_TIMEOUT).min(deadline))).map_err(|f| failed(&f))?;
    let status = response.status();
    // 304 only means "unchanged" when an ETag was sent; otherwise it is a 3xx without a Location below.
    if status == 304 && etag.is_some() {
        return Ok((Hop::NotModified, None));
    }
    if (300..400).contains(&status) {
        return match response.header("Location") {
            Some(l) if !l.is_empty() => Ok((Hop::Redirect(l), None)),
            _ => Err(Fault::Network(format!("{shown_url}: HTTP {status} without a Location"))),
        };
    }
    if status == 403 || status == 429 {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let wait = server_wait(response.header("Retry-After").as_deref(), response.header("X-RateLimit-Reset").as_deref(), now);
        return Err(Fault::Throttled(format!("{shown_url}: HTTP {status}"), wait));
    }
    if !(200..300).contains(&status) {
        return Err(Fault::Network(format!("{shown_url}: HTTP {status}")));
    }
    let tag = response.header("ETag").filter(|t| usable_etag(t));
    match body_of(&response, limit, deadline) {
        Ok(Ok(bytes)) => Ok((Hop::Body(bytes), tag)),
        Ok(Err(Cut::TooBig)) => Err(too_big()),
        Ok(Err(Cut::TimedOut)) => Err(timed_out()),
        Ok(Err(Cut::Short { got, expected })) => Err(Fault::Network(short_body(&shown_url, got, expected))),
        Err(code) => Err(failed(&describe(code))),
    }
}

/// The most bytes of a POST's response that [`post`] reads.
pub const ANSWER_LIMIT: usize = 64 << 10;

/// POSTs `body` as `content_type` to `url` with `headers`, without cookies (WinHTTP keeps no cache) and within
/// [`TOTAL_TIMEOUT`] in all; the status and the response's text (as UTF-8, at most [`ANSWER_LIMIT`] bytes) for any
/// status. The URL must be https:// with a plain host ([`https_host`]), or nothing is sent; a redirect is not followed
/// (its 3xx status is the answer), so the body never goes anywhere but `url`. Sent once more with no proxy only when
/// the automatic proxy failed before reaching the server ([`falls_back`]). Err when nothing was sent, no response came
/// (a time out included), the response is over the limit or ends before its Content-Length.
pub fn post(url: &str, content_type: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<(u16, String), String> {
    post_with(&System, url, content_type, headers, body)
}

/// [`post`] with its requests sent through `open`.
fn post_with(open: &impl Open, url: &str, content_type: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<(u16, String), String> {
    let (host, path) = split_url(url)?;
    let shown_url = shown(url);
    let deadline = Instant::now() + TOTAL_TIMEOUT;
    let mut all = vec![("Content-Type", content_type)];
    all.extend_from_slice(headers);
    let response = with_fallback(false, |route| open.open(route, &host, &path, "POST", &all, body, deadline)).map_err(|f| format!("{shown_url}: {f}"))?;
    let status = response.status();
    match body_of(&response, ANSWER_LIMIT, deadline) {
        Ok(Ok(bytes)) => Ok((status, String::from_utf8_lossy(&bytes).into_owned())),
        Ok(Err(Cut::TooBig)) => Err(trf!("{}: the file is over the size limit of {} bytes", shown_url, ANSWER_LIMIT)),
        Ok(Err(Cut::TimedOut)) => Err(trf!("{}: timed out", shown_url)),
        Ok(Err(Cut::Short { got, expected })) => Err(short_body(&shown_url, got, expected)),
        Err(code) => Err(format!("{shown_url}: {}", describe(code))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(https_host("https://GitHub.com/a/b").unwrap(), "github.com");
        assert_eq!(https_host("HTTPS://api.github.com?x").unwrap(), "api.github.com");
        for bad in ["http://github.com/", "ftp://github.com", "https://", "https:///x", "https://github.com:8443/x", "https://user@github.com/", "github.com/x", "https://git\\hub.com"] {
            assert!(https_host(bad).is_err(), "{bad}");
        }
        assert_eq!(resolve("https://github.com/a", "/b/c").unwrap(), "https://github.com/b/c");
        assert_eq!(resolve("https://github.com/a", "//evil.example/x").unwrap(), "//evil.example/x");
        assert!(https_host(&resolve("https://github.com/a", "//evil.example/x").unwrap()).is_err());
    }

    /// Redirects without end are cut off.
    #[test]
    fn redirect_loop() {
        struct Loop;
        impl Fetch for Loop {
            fn fetch(&self, _: &str, _: &str, _: usize, _: Instant) -> Result<Hop, Fault> {
                Ok(Hop::Redirect("/again".into()))
            }
        }
        assert!(matches!(get_limited(&Loop, "https://github.com/x", "*/*", 10, &["github.com"]), Err(Fault::Refused(e)) if e.contains("too many redirects")));
    }

    /// One deadline for the whole GET: every hop gets the same one, and a GET whose time is up asks for nothing more.
    #[test]
    fn time_limit_spans_redirects() {
        use std::cell::RefCell;
        #[derive(Default)]
        struct Slow(RefCell<Vec<Instant>>);
        impl Fetch for Slow {
            fn fetch(&self, _: &str, _: &str, _: usize, deadline: Instant) -> Result<Hop, Fault> {
                self.0.borrow_mut().push(deadline);
                std::thread::sleep(Duration::from_millis(30));
                Ok(Hop::Redirect("/next".into()))
            }
        }
        let slow = Slow::default();
        let before = Instant::now();
        let _ = get_limited(&slow, "https://github.com/x", "*/*", 10, &["github.com"]);
        let seen = slow.0.borrow();
        assert_eq!(seen.len(), MAX_REDIRECTS + 1);
        assert!(seen.iter().all(|d| *d == seen[0]));
        assert!(seen[0] >= before + TOTAL_TIMEOUT && seen[0] <= Instant::now() + TOTAL_TIMEOUT);
        // Each hop takes 30 ms; with 70 ms for the whole GET the fourth hop is never asked for.
        let slow = Slow::default();
        let r = get_until(&slow, "https://github.com/x", "*/*", 10, &["github.com"], Instant::now() + Duration::from_millis(70));
        assert!(matches!(r, Err(Fault::Network(e)) if e.contains("timed out")));
        assert!(slow.0.borrow().len() <= 3);
    }

    /// The POST checks its URL before anything is sent: these all fail without touching the network.
    #[test]
    fn post_refuses_other_than_https() {
        for (url, why) in [
            ("http://warden.arcasti.dev/api/runs", "not an https:// address"),
            ("ftp://warden.arcasti.dev/api/runs", "not an https:// address"),
            ("https://user@warden.arcasti.dev/api/runs", "not an allowed address"),
            ("https://warden.arcasti.dev:8443/api/runs", "not an allowed address"),
            ("https:///api/runs", "not an allowed address"),
        ] {
            let e = post(url, "application/gzip", &[], b"x").unwrap_err();
            assert!(e.contains(why), "{url}: {e}");
        }
    }

    /// A body is read up to its limit and refused past it, whether or not it states its length.
    #[test]
    fn body_limit() {
        use std::cell::Cell;
        /// Gives `body` in pieces of at most 300 bytes, counting the bytes handed out in `given`.
        fn reader(body: Vec<u8>, given: &Cell<usize>) -> impl FnMut(&mut [u8], Instant) -> Result<usize, u32> + '_ {
            let mut at = 0;
            move |buf, _| {
                let n = buf.len().min(300).min(body.len() - at);
                buf[..n].copy_from_slice(&body[at..at + n]);
                at += n;
                given.set(given.get() + n);
                Ok(n)
            }
        }
        let later = Instant::now() + Duration::from_secs(30);
        let body = vec![b'a'; 1000];
        let given = Cell::new(0);
        assert_eq!(read_body(Some("1000"), 1000, later, reader(body.clone(), &given)), Ok(Ok(body.clone())));
        // A Content-Length over the limit is refused before anything is read.
        given.set(0);
        assert_eq!(read_body(Some("1000"), 999, later, reader(body.clone(), &given)), Ok(Err(Cut::TooBig)));
        assert_eq!(given.get(), 0);
        // Without a Content-Length (or a lying one), the limit is kept as the body arrives, never reading more than
        // one byte past it.
        for length in [None, Some("10"), Some("x")] {
            given.set(0);
            assert_eq!(read_body(length, 999, later, reader(body.clone(), &given)), Ok(Err(Cut::TooBig)), "{length:?}");
            assert_eq!(given.get(), 1000, "{length:?}");
            assert_eq!(read_body(length, 1000, later, reader(body.clone(), &given)), Ok(Ok(body.clone())), "{length:?}");
        }
        let big = vec![b'b'; 200_000];
        assert_eq!(read_body(None, 200_000, later, reader(big.clone(), &given)), Ok(Ok(big)));
        assert_eq!(read_body(None, 10, later, reader(Vec::new(), &given)), Ok(Ok(Vec::new())));
        // A WinHTTP time out, or the deadline passed, is a time out; another error is given back.
        assert_eq!(read_body(None, 10, later, |_, _| Err(ERROR_WINHTTP_TIMEOUT)), Ok(Err(Cut::TimedOut)));
        assert_eq!(read_body(None, 10, Instant::now(), |_, _| panic!("read after the deadline")), Ok(Err(Cut::TimedOut)));
        assert_eq!(read_body(None, 10, later, |_, _| Err(ERROR_WINHTTP_CONNECTION_ERROR)), Err(ERROR_WINHTTP_CONNECTION_ERROR));
        // A body that ends before its Content-Length (the connection closed early) is not whole.
        assert_eq!(read_body(Some("1000"), 2000, later, reader(vec![b'a'; 600], &given)), Ok(Err(Cut::Short { got: 600, expected: 1000 })));
        assert_eq!(read_body(Some(" 600 "), 2000, later, reader(vec![b'a'; 600], &given)), Ok(Ok(vec![b'a'; 600])));
        assert_eq!(read_body(Some("1"), 10, later, reader(Vec::new(), &given)), Ok(Err(Cut::Short { got: 0, expected: 1 })));
    }

    /// A proxy's 407 is answered once with the default credentials, when it takes Negotiate or NTLM: the request is
    /// sent again once, and a second 407 is the answer. A server's 401 is not answered.
    #[test]
    fn proxy_407_is_answered_once() {
        use std::cell::RefCell;
        struct Fake {
            statuses: RefCell<Vec<u16>>,
            takes_default: bool,
            log: RefCell<Vec<&'static str>>,
        }
        impl Exchange for Fake {
            fn round_trip(&self, _: Instant) -> Result<(), u32> {
                self.log.borrow_mut().push("send");
                Ok(())
            }
            fn status(&self) -> Result<u16, u32> {
                Ok(self.statuses.borrow_mut().remove(0))
            }
            fn proxy_credentials(&self) -> Result<bool, u32> {
                self.log.borrow_mut().push(if self.takes_default { "credentials" } else { "no credentials" });
                Ok(self.takes_default)
            }
        }
        let run = |statuses: Vec<u16>, takes_default: bool| {
            let fake = Fake { statuses: RefCell::new(statuses), takes_default, log: RefCell::default() };
            let status = answer(&fake, Instant::now() + Duration::from_secs(5));
            (status, fake.log.into_inner())
        };
        assert_eq!(run(vec![407, 200], true), (Ok(200), vec!["send", "credentials", "send"]));
        // Still 407 with the credentials: that is the answer, never a third send.
        assert_eq!(run(vec![407, 407, 200], true), (Ok(407), vec!["send", "credentials", "send"]));
        // A proxy that takes only Basic or Digest: nothing set, nothing sent again.
        assert_eq!(run(vec![407], false), (Ok(407), vec!["send", "no credentials"]));
        for status in [200, 304, 401, 403, 503] {
            assert_eq!(run(vec![status], true), (Ok(status), vec!["send"]), "{status}");
        }
        // Negotiate first, then NTLM; never Basic, Digest or Passport.
        let (negotiate, ntlm) = (WINHTTP_AUTH_SCHEME_NEGOTIATE.0, WINHTTP_AUTH_SCHEME_NTLM.0);
        assert_eq!(proxy_scheme(negotiate | ntlm | 1), Some(negotiate));
        assert_eq!(proxy_scheme(ntlm | 1 | 8), Some(ntlm));
        assert_eq!(proxy_scheme(1 | 4 | 8), None);
    }

    /// One request a [`Script`] was asked for: route, verb, host, path and header lines.
    type Sent = (Route, String, String, String, Vec<String>);

    /// A scripted [`Open`]: each request takes the next answer, and is recorded.
    struct Script {
        answers: std::cell::RefCell<Vec<Result<Canned, u32>>>,
        sent: std::cell::RefCell<Vec<Sent>>,
    }

    /// A canned response: status, headers and body.
    struct Canned {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: std::cell::RefCell<Vec<u8>>,
    }

    impl Canned {
        fn new(status: u16, headers: &[(&'static str, &str)], body: &[u8]) -> Canned {
            Canned { status, headers: headers.iter().map(|(n, v)| (*n, v.to_string())).collect(), body: std::cell::RefCell::new(body.to_vec()) }
        }
    }

    impl Response for Canned {
        fn status(&self) -> u16 {
            self.status
        }
        fn header(&self, name: &str) -> Option<String> {
            self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
        }
        fn read(&self, buf: &mut [u8], _: Instant) -> Result<usize, u32> {
            let mut body = self.body.borrow_mut();
            let n = buf.len().min(body.len());
            buf[..n].copy_from_slice(&body[..n]);
            body.drain(..n);
            Ok(n)
        }
    }

    impl Script {
        fn new(answers: Vec<Result<Canned, u32>>) -> Script {
            Script { answers: std::cell::RefCell::new(answers), sent: Default::default() }
        }
        fn routes(&self) -> Vec<Route> {
            self.sent.borrow().iter().map(|s| s.0).collect()
        }
    }

    impl Open for Script {
        type Answer = Canned;
        fn open(&self, route: Route, host: &str, path: &str, verb: &str, headers: &[(&str, &str)], _: &[u8], _: Instant) -> Result<Canned, u32> {
            let headers = headers.iter().map(|(n, v)| format!("{n}: {v}")).collect();
            self.sent.borrow_mut().push((route, verb.into(), host.into(), path.into(), headers));
            self.answers.borrow_mut().remove(0)
        }
    }

    /// The GET and the POST go through with_fallback: a connection failure through the automatic proxy is sent again
    /// with no proxy, an HTTP answer (407 included) is not, and the error names both WinHTTP codes.
    #[test]
    fn get_and_post_go_through_the_fallback() {
        use Route::{Automatic, Direct};
        let later = Instant::now() + Duration::from_secs(30);
        let url = "https://api.github.com/repos/x?y=1#z";
        let ok = || Ok(Canned::new(200, &[("ETag", "\"e\""), ("Content-Length", "2")], b"hi"));
        let script = Script::new(vec![Err(ERROR_WINHTTP_CANNOT_CONNECT), ok()]);
        let got = get_hop(&script, url, "*/*", Some("\"a\""), 100, later);
        assert!(matches!(&got, Ok((Hop::Body(b), Some(t))) if b == b"hi" && t == "\"e\""), "{got:?}");
        assert_eq!(script.routes(), [Automatic, Direct]);
        let (_, verb, host, path, headers) = &script.sent.borrow()[1];
        assert_eq!((verb.as_str(), host.as_str(), path.as_str()), ("GET", "api.github.com", "/repos/x?y=1"));
        assert_eq!(*headers, ["Accept: */*", "If-None-Match: \"a\""]);
        // An HTTP error, a proxy's final 407 included, is the answer: asked once.
        for status in [407, 503] {
            let script = Script::new(vec![Ok(Canned::new(status, &[], b""))]);
            assert!(matches!(get_hop(&script, url, "*/*", None, 100, later), Err(Fault::Network(e)) if e.ends_with(&format!("HTTP {status}"))), "{status}");
            assert_eq!(script.routes(), [Automatic]);
        }
        // Both routes fail: both codes are named.
        let script = Script::new(vec![Err(ERROR_WINHTTP_AUTODETECTION_FAILED), Err(ERROR_WINHTTP_NAME_NOT_RESOLVED)]);
        let e = get_hop(&script, url, "*/*", None, 100, later).unwrap_err();
        assert!(matches!(&e, Fault::Network(m) if m.contains("WinHTTP 12180 ") && m.contains(" -> WinHTTP 12007 ")), "{e:?}");
        // A body cut short of its Content-Length is a network error, not a body.
        let script = Script::new(vec![Ok(Canned::new(200, &[("Content-Length", "10")], b"abcd"))]);
        assert!(matches!(get_hop(&script, url, "*/*", None, 100, later), Err(Fault::Network(e)) if e.ends_with("4 / Content-Length 10")));

        // The POST: sent again with no proxy only when it cannot have reached the server.
        let posted = "https://warden.example/api/runs";
        let script = Script::new(vec![Err(ERROR_WINHTTP_CANNOT_CONNECT), Ok(Canned::new(201, &[], b"ok"))]);
        assert_eq!(post_with(&script, posted, "application/gzip", &[("X-A", "1")], b"x"), Ok((201, "ok".into())));
        assert_eq!(script.routes(), [Automatic, Direct]);
        assert_eq!(script.sent.borrow()[1].1, "POST");
        assert_eq!(script.sent.borrow()[1].4, ["Content-Type: application/gzip", "X-A: 1"]);
        let script = Script::new(vec![Err(ERROR_WINHTTP_TIMEOUT)]);
        let e = post_with(&script, posted, "application/gzip", &[], b"x").unwrap_err();
        assert!(e.contains("WinHTTP 12002"), "{e}");
        assert_eq!(script.routes(), [Automatic]);
        let script = Script::new(vec![Ok(Canned::new(200, &[("Content-Length", "9")], b"ok"))]);
        assert!(post_with(&script, posted, "application/gzip", &[], b"x").unwrap_err().ends_with("2 / Content-Length 9"));
    }

    /// A request that fails on the way through the automatic proxy is sent once more with no proxy; an HTTP answer
    /// (any status) and other failures are not sent again; a POST is sent again only when it cannot have arrived.
    #[test]
    fn falls_back_to_no_proxy_once() {
        use std::cell::RefCell;
        // Answers each attempt from `answers` in turn, recording the route it took.
        let run = |repeatable: bool, answers: Vec<Result<u16, u32>>| {
            let routes = RefCell::new(Vec::new());
            let mut answers = answers.into_iter();
            let result = with_fallback(repeatable, |route| {
                routes.borrow_mut().push(route);
                answers.next().expect("asked once too often")
            });
            (result, routes.into_inner())
        };
        use Route::{Automatic, Direct};
        // The automatic proxy fails to connect or to find a proxy: the request goes again with no proxy.
        for code in [
            ERROR_WINHTTP_CANNOT_CONNECT,
            ERROR_WINHTTP_NAME_NOT_RESOLVED,
            ERROR_WINHTTP_AUTO_PROXY_SERVICE_ERROR,
            ERROR_WINHTTP_AUTODETECTION_FAILED,
            ERROR_WINHTTP_TIMEOUT,
            ERROR_WINHTTP_CONNECTION_ERROR,
            ERROR_WINHTTP_BAD_AUTO_PROXY_SCRIPT,
            ERROR_WINHTTP_UNABLE_TO_DOWNLOAD_SCRIPT,
        ] {
            assert_eq!(run(true, vec![Err(code), Ok(200)]), (Ok(200), vec![Automatic, Direct]), "{code}");
        }
        // Both fail: both errors are kept for the message.
        assert_eq!(
            run(true, vec![Err(ERROR_WINHTTP_AUTODETECTION_FAILED), Err(ERROR_WINHTTP_CANNOT_CONNECT)]),
            (Err(Failed { automatic: ERROR_WINHTTP_AUTODETECTION_FAILED, direct: Some(ERROR_WINHTTP_CANNOT_CONNECT) }), vec![Automatic, Direct])
        );
        // An HTTP answer is final, an error status included: asked once.
        for status in [200, 304, 404, 429, 503] {
            assert_eq!(run(true, vec![Ok(status)]), (Ok(status), vec![Automatic]), "{status}");
        }
        // A failure no proxy setting changes (a refused certificate, a bad argument) is not sent again.
        for code in [12175, 87] {
            assert_eq!(run(true, vec![Err(code)]), (Err(Failed { automatic: code, direct: None }), vec![Automatic]), "{code}");
        }
        // A POST: sent again only when it cannot have reached the server.
        assert_eq!(run(false, vec![Err(ERROR_WINHTTP_CANNOT_CONNECT), Ok(201)]), (Ok(201), vec![Automatic, Direct]));
        for code in [ERROR_WINHTTP_TIMEOUT, ERROR_WINHTTP_CONNECTION_ERROR] {
            assert_eq!(run(false, vec![Err(code)]), (Err(Failed { automatic: code, direct: None }), vec![Automatic]), "{code}");
        }
    }

    /// The error shown: the WinHTTP code with Windows' text for it, and the attempt with no proxy when it differs.
    #[test]
    fn errors_name_the_winhttp_code() {
        let one = Failed { automatic: ERROR_WINHTTP_CANNOT_CONNECT, direct: Some(ERROR_WINHTTP_CANNOT_CONNECT) }.to_string();
        assert!(one.starts_with("WinHTTP 12029 ") && one.len() > "WinHTTP 12029 ".len() && !one.contains("->"), "{one}");
        assert!(!one.contains(['\r', '\n']), "{one}");
        let two = Failed { automatic: ERROR_WINHTTP_AUTODETECTION_FAILED, direct: Some(ERROR_WINHTTP_NAME_NOT_RESOLVED) }.to_string();
        assert!(two.starts_with("WinHTTP 12180 ") && two.contains(" -> WinHTTP 12007 "), "{two}");
        assert_eq!(describe(0xDEAD_BEEF), format!("WinHTTP {}", 0xDEAD_BEEFu32));
        assert_eq!(code_of(&windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_2EFDu32 as i32))), 12029);
    }

    #[test]
    fn urls_split_into_host_and_path() {
        let split = |u: &str| split_url(u).unwrap();
        assert_eq!(split("https://api.github.com/repos/a/b?x=1#frag"), ("api.github.com".into(), "/repos/a/b?x=1".into()));
        assert_eq!(split("https://GitHub.com"), ("github.com".into(), "/".into()));
        assert_eq!(split("https://github.com?q=%2F"), ("github.com".into(), "/?q=%2F".into()));
        assert_eq!(split("https://github.com#x"), ("github.com".into(), "/".into()));
        assert!(split_url("http://github.com/").is_err() && split_url("https://github.com:444/").is_err());
    }

    /// Answers from a script of hops, recording the If-None-Match each request carried.
    struct Tagged {
        hops: std::cell::RefCell<Vec<(Hop, Option<String>)>>,
        sent: std::cell::RefCell<Vec<(String, Option<String>)>>,
    }

    impl Tagged {
        fn new(hops: Vec<(Hop, Option<&str>)>) -> Tagged {
            let hops = hops.into_iter().rev().map(|(h, t)| (h, t.map(String::from))).collect();
            Tagged { hops: std::cell::RefCell::new(hops), sent: Default::default() }
        }
    }

    impl Fetch for Tagged {
        fn fetch(&self, _: &str, _: &str, _: usize, _: Instant) -> Result<Hop, Fault> {
            panic!("a conditional GET goes through fetch_tagged");
        }

        fn fetch_tagged(&self, url: &str, _: &str, etag: Option<&str>, _: usize, _: Instant) -> Result<(Hop, Option<String>), Fault> {
            self.sent.borrow_mut().push((url.to_string(), etag.map(String::from)));
            Ok(self.hops.borrow_mut().pop().expect("no more answers"))
        }
    }

    const API: &str = "https://api.github.com/repos/x/y/releases/latest";
    const HOSTS: [&str; 2] = ["api.github.com", "github.com"];

    /// The ETag goes out as If-None-Match on the first request only, a 304 answers it, and the body's ETag is kept;
    /// an ETag that could add a header line is neither sent nor kept.
    #[test]
    fn conditional_get_sends_and_keeps_the_etag() {
        let fake = Tagged::new(vec![(Hop::NotModified, None)]);
        assert_eq!(get_conditional(&fake, API, "*/*", 100, &HOSTS, Some("\"abc\"")), Ok(Got::NotModified));
        assert_eq!(*fake.sent.borrow(), [(API.to_string(), Some("\"abc\"".to_string()))]);
        // A changed answer: the body and its new ETag.
        let fake = Tagged::new(vec![(Hop::Body(b"{}".to_vec()), Some("W/\"def\""))]);
        assert_eq!(get_conditional(&fake, API, "*/*", 100, &HOSTS, Some("\"abc\"")), Ok(Got::Body { body: b"{}".to_vec(), etag: Some("W/\"def\"".into()) }));
        // A redirect: its target is asked without the ETag, and its ETag is not the first URL's.
        let fake = Tagged::new(vec![(Hop::Redirect("https://github.com/z".into()), None), (Hop::Body(b"x".to_vec()), Some("\"z\""))]);
        assert_eq!(get_conditional(&fake, API, "*/*", 100, &HOSTS, Some("\"abc\"")), Ok(Got::Body { body: b"x".to_vec(), etag: None }));
        assert_eq!(fake.sent.borrow()[1], ("https://github.com/z".to_string(), None));
        // An ETag with a line break or a space is never sent, nor kept from a response.
        for bad in ["\"a\"\r\nX-Evil: 1", "\"a b\"", "", "\"\u{e9}\""] {
            let fake = Tagged::new(vec![(Hop::Body(b"x".to_vec()), Some(bad))]);
            assert_eq!(get_conditional(&fake, API, "*/*", 100, &HOSTS, Some(bad)), Ok(Got::Body { body: b"x".to_vec(), etag: None }), "{bad:?}");
            assert_eq!(fake.sent.borrow()[0].1, None, "{bad:?}");
        }
        assert!(usable_etag("W/\"0123456789abcdef\"") && !usable_etag(&"a".repeat(ETAG_LIMIT + 1)));
        // A 304 nobody asked for is an error, never "unchanged".
        let fake = Tagged::new(vec![(Hop::NotModified, None)]);
        assert!(matches!(get_conditional(&fake, API, "*/*", 100, &HOSTS, None), Err(Fault::Network(e)) if e.contains("304")));
        // The other rules hold the same with an ETag: size, hosts, https.
        let fake = Tagged::new(vec![(Hop::Body(vec![0; 101]), Some("\"a\""))]);
        assert!(matches!(get_conditional(&fake, API, "*/*", 100, &HOSTS, Some("\"a\"")), Err(Fault::Refused(_))));
        let fake = Tagged::new(vec![(Hop::Redirect("https://evil.example/x".into()), None)]);
        assert!(matches!(get_conditional(&fake, API, "*/*", 100, &HOSTS, Some("\"a\"")), Err(Fault::Refused(_))));
        let fake = Tagged::new(Vec::new());
        assert!(matches!(get_conditional(&fake, "http://api.github.com/x", "*/*", 100, &HOSTS, Some("\"a\"")), Err(Fault::Refused(_))));
        assert!(fake.sent.borrow().is_empty());
    }

    /// Retry-After in seconds and X-RateLimit-Reset as a Unix time: the longer wait, at most a day; nonsense is
    /// ignored.
    #[test]
    fn waits_the_server_asks_for() {
        let now = 1_800_000_000;
        assert_eq!(server_wait(Some("120"), None, now), Some(Duration::from_secs(120)));
        assert_eq!(server_wait(None, Some("1800003600"), now), Some(Duration::from_secs(3600)));
        assert_eq!(server_wait(Some("60"), Some("1800000600"), now), Some(Duration::from_secs(600)));
        // A reset already past waits nothing more.
        assert_eq!(server_wait(None, Some("1700000000"), now), Some(Duration::ZERO));
        assert_eq!(server_wait(Some("99999999"), None, now), Some(LONGEST_WAIT));
        for bad in ["", "-5", "1.5", "Wed, 21 Oct 2026 07:28:00 GMT", "12abc"] {
            assert_eq!(server_wait(Some(bad), Some(bad), now), None, "{bad}");
        }
        assert_eq!(server_wait(None, None, now), None);
        assert_eq!(Fault::Throttled("x".into(), Some(Duration::from_secs(5))).wait(), Some(Duration::from_secs(5)));
        assert_eq!(Fault::Network("x".into()).wait(), None);
    }

    #[test]
    fn urls_in_errors_are_shown_safely() {
        let hostile = format!("http://github.com/\u{202E}{}", "a".repeat(400));
        let e = https_host(&hostile).unwrap_err();
        assert!(!e.contains('\u{202E}') && e.chars().count() < 140, "{e}");
        assert!(https_host("https://git\u{7}hub.com/").is_err());
        let e = https_host("https://\u{1b}[2J/").unwrap_err();
        assert!(!e.contains('\u{1b}'), "{e}");
    }
}
