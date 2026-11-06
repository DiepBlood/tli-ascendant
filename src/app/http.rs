//! HTTP over the WinRT HttpClient: the update check's and download's GETs
//! and the run log upload's POST.
//!
//! No request lets HttpClient follow redirects: for GETs [`WinHttp`] answers one hop at a time and [`get_limited`]
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
use std::time::{Duration, Instant};
use windows::Web::Http::HttpClient;
use windows::core::{HSTRING, h};

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

/// Makes `client` send the User-Agent and `headers` with each request.
fn add_headers(client: &HttpClient, headers: &[(&str, &str)]) -> windows::core::Result<()> {
    let defaults = client.DefaultRequestHeaders()?;
    defaults.TryAppendWithoutValidation(h!("User-Agent"), &HSTRING::from(USER_AGENT))?;
    for (name, value) in headers {
        defaults.TryAppendWithoutValidation(&HSTRING::from(*name), &HSTRING::from(*value))?;
    }
    Ok(())
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

/// Waits for a WinRT async operation until `deadline`, cancelling it when the time is up (None then).
macro_rules! within {
    ($op:expr, $deadline:expr) => {{
        let op = $op;
        loop {
            // 0 is AsyncStatus::Started; anything else is done (completed, failed or cancelled).
            if op.Status()?.0 != 0 {
                break Some(op.GetResults()?);
            }
            if Instant::now() >= $deadline {
                let _ = op.Cancel();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }};
}

/// Why [`read_body`] did not give a whole body.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cut {
    /// Over the limit, by its Content-Length or as it arrived.
    TooBig,
    /// Still arriving at the deadline.
    TimedOut,
}

/// The body of `content`, at most `limit` bytes, read by `deadline`: refused at once when its Content-Length is
/// over the limit, and never read past the limit when it does not give one.
fn read_body(content: &windows::Web::Http::IHttpContent, limit: usize, deadline: Instant) -> windows::core::Result<Result<Vec<u8>, Cut>> {
    use windows::Storage::Streams::{DataReader, InputStreamOptions};
    if let Ok(n) = content.Headers()?.ContentLength().and_then(|n| n.Value())
        && n > limit as u64
    {
        return Ok(Err(Cut::TooBig));
    }
    let Some(stream) = within!(content.ReadAsInputStreamAsync()?, deadline) else {
        return Ok(Err(Cut::TimedOut));
    };
    let reader = DataReader::CreateDataReader(&stream)?;
    reader.SetInputStreamOptions(InputStreamOptions::Partial)?;
    let mut bytes = Vec::new();
    loop {
        let Some(n) = within!(reader.LoadAsync(64 * 1024)?, deadline) else {
            return Ok(Err(Cut::TimedOut));
        };
        let n = n as usize;
        if n == 0 {
            break;
        }
        if bytes.len() + n > limit {
            return Ok(Err(Cut::TooBig));
        }
        let start = bytes.len();
        bytes.resize(start + n, 0);
        reader.ReadBytes(&mut bytes[start..])?;
    }
    Ok(Ok(bytes))
}

/// [`Fetch`] over the WinRT HttpClient, with automatic redirects, cookies and the HTTP cache turned off.
pub struct WinHttp;

impl Fetch for WinHttp {
    fn fetch(&self, url: &str, accept: &str, limit: usize, deadline: Instant) -> Result<Hop, Fault> {
        self.fetch_tagged(url, accept, None, limit, deadline).map(|(hop, _)| hop)
    }

    fn fetch_tagged(&self, url: &str, accept: &str, etag: Option<&str>, limit: usize, deadline: Instant) -> Result<(Hop, Option<String>), Fault> {
        use windows::Foundation::Uri;
        use windows::Web::Http::HttpCompletionOption;
        let shown_url = shown(url);
        let too_big = || Fault::Refused(trf!("{}: the file is over the size limit of {} bytes", shown_url, limit));
        let timed_out = || Fault::Network(trf!("{}: timed out", shown_url));
        let etag = etag.filter(|e| usable_etag(e));
        // The headers within HEADERS_TIMEOUT of this hop, and everything within the whole GET's deadline.
        let (headers_by, done_by) = ((Instant::now() + HEADERS_TIMEOUT).min(deadline), deadline);
        let fetch = || -> windows::core::Result<Result<(Hop, Option<String>), Fault>> {
            let filter = private_filter()?;
            filter.SetAllowAutoRedirect(false)?;
            let client = HttpClient::Create(&filter)?;
            let mut headers = vec![("Accept", accept)];
            if let Some(etag) = etag {
                headers.push(("If-None-Match", etag));
            }
            add_headers(&client, &headers)?;
            let Some(response) = within!(client.GetWithOptionAsync(&Uri::CreateUri(&HSTRING::from(url))?, HttpCompletionOption::ResponseHeadersRead)?, headers_by) else {
                return Ok(Err(timed_out()));
            };
            let status = response.StatusCode()?.0;
            let header = |name: &HSTRING| response.Headers().and_then(|h| h.Lookup(name)).ok().map(|v| v.to_string());
            // 304 only means "unchanged" when an ETag was sent; otherwise it is a 3xx without a Location below.
            if status == 304 && etag.is_some() {
                return Ok(Ok((Hop::NotModified, None)));
            }
            if (300..400).contains(&status) {
                return Ok(match header(h!("Location")) {
                    Some(l) if !l.is_empty() => Ok((Hop::Redirect(l), None)),
                    _ => Err(Fault::Network(format!("{shown_url}: HTTP {status} without a Location"))),
                });
            }
            if status == 403 || status == 429 {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let wait = server_wait(header(h!("Retry-After")).as_deref(), header(h!("X-RateLimit-Reset")).as_deref(), now);
                return Ok(Err(Fault::Throttled(format!("{shown_url}: HTTP {status}"), wait)));
            }
            if !response.IsSuccessStatusCode()? {
                return Ok(Err(Fault::Network(format!("{shown_url}: HTTP {status}"))));
            }
            let tag = header(h!("ETag")).filter(|t| usable_etag(t));
            Ok(match read_body(&response.Content()?, limit, done_by)? {
                Ok(bytes) => Ok((Hop::Body(bytes), tag)),
                Err(Cut::TooBig) => Err(too_big()),
                Err(Cut::TimedOut) => Err(timed_out()),
            })
        };
        fetch().unwrap_or_else(|e| Err(Fault::Network(format!("{shown_url}: {}", e.message()))))
    }
}

/// A protocol filter that sends and keeps no cookies, reads and writes no HTTP cache, and never shows a dialog
/// (credentials, certificates).
fn private_filter() -> windows::core::Result<windows::Web::Http::Filters::HttpBaseProtocolFilter> {
    use windows::Web::Http::Filters::{HttpBaseProtocolFilter, HttpCacheReadBehavior, HttpCacheWriteBehavior, HttpCookieUsageBehavior};
    let filter = HttpBaseProtocolFilter::new()?;
    filter.SetAllowUI(false)?;
    filter.SetCookieUsageBehavior(HttpCookieUsageBehavior::NoCookies)?;
    let cache = filter.CacheControl()?;
    cache.SetReadBehavior(HttpCacheReadBehavior::NoCache)?;
    cache.SetWriteBehavior(HttpCacheWriteBehavior::NoCache)?;
    Ok(filter)
}

/// The most bytes of a POST's response that [`post`] reads.
pub const ANSWER_LIMIT: usize = 64 << 10;

/// POSTs `body` as `content_type` to `url` with `headers`, without cookies or cache (see [`private_filter`]) and
/// within [`TOTAL_TIMEOUT`] in all; the status and the response's text (as UTF-8, at most [`ANSWER_LIMIT`] bytes)
/// for any status. The URL must be https:// with a plain host ([`https_host`]), or nothing is sent; a redirect is
/// not followed (its 3xx status is the answer), so the body never goes anywhere but `url`. Err when nothing was
/// sent, no response came (a time out included) or the response is over the limit.
pub fn post(url: &str, content_type: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<(u16, String), String> {
    use windows::Foundation::Uri;
    use windows::Storage::Streams::DataWriter;
    use windows::Web::Http::HttpBufferContent;
    use windows::Web::Http::Headers::HttpMediaTypeHeaderValue;
    https_host(url)?;
    let shown_url = shown(url);
    let deadline = Instant::now() + TOTAL_TIMEOUT;
    let timed_out = || trf!("{}: timed out", shown_url);
    let send = || -> windows::core::Result<Result<(u16, String), String>> {
        let filter = private_filter()?;
        filter.SetAllowAutoRedirect(false)?;
        let client = HttpClient::Create(&filter)?;
        add_headers(&client, headers)?;
        let writer = DataWriter::new()?;
        writer.WriteBytes(body)?;
        let content = HttpBufferContent::CreateFromBuffer(&writer.DetachBuffer()?)?;
        content.Headers()?.SetContentType(&HttpMediaTypeHeaderValue::Create(&HSTRING::from(content_type))?)?;
        let Some(response) = within!(client.PostAsync(&Uri::CreateUri(&HSTRING::from(url))?, &content)?, deadline) else { return Ok(Err(timed_out())) };
        let status = response.StatusCode()?.0 as u16;
        Ok(match read_body(&response.Content()?, ANSWER_LIMIT, deadline)? {
            Ok(bytes) => Ok((status, String::from_utf8_lossy(&bytes).into_owned())),
            Err(Cut::TooBig) => Err(trf!("{}: the file is over the size limit of {} bytes", shown_url, ANSWER_LIMIT)),
            Err(Cut::TimedOut) => Err(timed_out()),
        })
    };
    send().unwrap_or_else(|e| Err(format!("{shown_url}: {}", e.message())))
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
        use windows::Storage::Streams::DataWriter;
        use windows::Web::Http::{HttpBufferContent, IHttpContent};
        let content = |bytes: &[u8]| -> IHttpContent {
            let writer = DataWriter::new().unwrap();
            writer.WriteBytes(bytes).unwrap();
            HttpBufferContent::CreateFromBuffer(&writer.DetachBuffer().unwrap()).unwrap().into()
        };
        let later = Instant::now() + Duration::from_secs(30);
        let body = vec![b'a'; 1000];
        assert_eq!(read_body(&content(&body), 1000, later).unwrap(), Ok(body.clone()));
        assert_eq!(read_body(&content(&body), 999, later).unwrap(), Err(Cut::TooBig));
        // Without a Content-Length, the limit is kept as the body arrives.
        let no_length = || {
            let c = content(&body);
            c.Headers().unwrap().SetContentLength(None::<&windows::Foundation::IReference<u64>>).unwrap();
            assert!(c.Headers().unwrap().ContentLength().and_then(|n| n.Value()).is_err());
            c
        };
        assert_eq!(read_body(&no_length(), 999, later).unwrap(), Err(Cut::TooBig));
        assert_eq!(read_body(&no_length(), 1000, later).unwrap(), Ok(body.clone()));
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
