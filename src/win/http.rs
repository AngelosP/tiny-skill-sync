//! A small blocking HTTP client on WinHTTP: system proxy and OS certificate store, no redirects, hard timeouts,
//! streamed bodies. It is the only networking code in the app.
//!
//! Redirects are never followed: a redirect from GitHub (a renamed repository, a moved file) is reported as such
//! so the caller can treat it as "cannot tell", never as a result. A bearer token is sent only over https, or to
//! a loopback address (used by the tests' fake server).

use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPTION_DECOMPRESSION, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WINHTTP_QUERY_RAW_HEADERS_CRLF, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};

use crate::secret::Secret;
use crate::win::wide;

const RESOLVE_TIMEOUT_MS: i32 = 10_000;
const CONNECT_TIMEOUT_MS: i32 = 10_000;
const SEND_TIMEOUT_MS: i32 = 30_000;
const RECEIVE_TIMEOUT_MS: i32 = 30_000;
/// How much of an error response is kept, for the message shown to the user.
const ERROR_BODY_LIMIT: usize = 8 * 1024;
const DECOMPRESS_GZIP_AND_DEFLATE: u32 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpError {
    BadUrl(String),
    InsecureToken,
    Timeout,
    Cancelled,
    TooLarge,
    /// The connection ended before all the bytes the server announced had arrived.
    Incomplete,
    /// The body sink asked to stop.
    Aborted,
    Os {
        call: &'static str,
        code: u32,
    },
}

impl HttpError {
    /// A sentence for the status line.
    pub fn describe(&self) -> String {
        match self {
            HttpError::BadUrl(u) => format!("not a usable web address: {u}"),
            HttpError::InsecureToken => "refusing to send a token over an unencrypted connection".to_string(),
            HttpError::Timeout => "the server did not answer in time".to_string(),
            HttpError::Cancelled => "cancelled".to_string(),
            HttpError::TooLarge => "the response is larger than allowed".to_string(),
            HttpError::Incomplete => "the connection was lost before the whole answer arrived".to_string(),
            HttpError::Aborted => "stopped".to_string(),
            HttpError::Os { code, .. } => match code {
                12007 => "the server name could not be found (no internet connection, or a DNS problem)".to_string(),
                12029 => "could not connect to the server".to_string(),
                12030 | 12031 => "the connection was lost".to_string(),
                12038 | 12045 | 12157 | 12175 => "a secure connection could not be made (certificate or TLS problem)".to_string(),
                12152 => "the server sent an invalid reply".to_string(),
                other => format!("network error {other}"),
            },
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

impl std::error::Error for HttpError {}

fn os_error(call: &'static str) -> HttpError {
    match unsafe { GetLastError() } {
        12002 => HttpError::Timeout,
        12017 => HttpError::Cancelled,
        code => HttpError::Os { call, code },
    }
}

/// Percent-encodes everything except the unreserved characters. Encode a path segment at a time.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Encodes each `/`-separated segment and keeps the slashes.
pub fn percent_encode_path(path: &str) -> String {
    path.split('/').map(percent_encode).collect::<Vec<_>>().join("/")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// Starts with `/`; already percent-encoded.
    pub path_and_query: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Url, HttpError> {
        let bad = || HttpError::BadUrl(s.to_string());
        let (secure, rest) = if let Some(r) = s.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = s.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(bad());
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| bad())?),
            None => (authority, if secure { 443 } else { 80 }),
        };
        if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
            return Err(bad());
        }
        let tail = tail.split('#').next().unwrap_or("");
        let path_and_query = match tail {
            "" => "/".to_string(),
            t if t.starts_with('?') => format!("/{t}"),
            t => t.to_string(),
        };
        if !path_and_query.bytes().all(|b| b > b' ' && b < 0x7f) {
            return Err(bad());
        }
        Ok(Url { secure, host: host.to_ascii_lowercase(), port, path_and_query })
    }

    pub fn is_loopback(&self) -> bool {
        self.host == "localhost" || self.host.starts_with("127.")
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let default = if self.secure { 443 } else { 80 };
        let scheme = if self.secure { "https" } else { "http" };
        if self.port == default {
            write!(f, "{scheme}://{}{}", self.host, self.path_and_query)
        } else {
            write!(f, "{scheme}://{}:{}{}", self.host, self.port, self.path_and_query)
        }
    }
}

pub struct Request<'a> {
    pub url: Url,
    /// Extra request headers; values must not contain line breaks.
    pub headers: Vec<(&'a str, String)>,
    pub bearer: Option<&'a Secret>,
    /// The most body bytes handed to the sink; more than this is an error.
    pub max_body: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Response {
    pub status: u16,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    /// The start of the body of an error response (status other than 200 and 206).
    pub error_body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Splits the raw header block WinHTTP returns into the status code and lower-cased header pairs.
pub fn parse_raw_headers(raw: &str) -> Option<(u16, Vec<(String, String)>)> {
    let mut lines = raw.split("\r\n");
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    Some((status, headers))
}

struct Handle(*mut c_void);

// SAFETY: WinHTTP handles may be used from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// Field order matters: connections must be closed before the sessions they belong to.
struct Pool {
    connections: HashMap<(String, u16, bool), Handle>,
    proxied: Option<Handle>,
    direct: Option<Handle>,
}

pub struct Client {
    user_agent: String,
    receive_timeout_ms: i32,
    pool: Mutex<Pool>,
}

impl Client {
    pub fn new(user_agent: &str) -> Client {
        Client {
            user_agent: user_agent.to_string(),
            receive_timeout_ms: RECEIVE_TIMEOUT_MS,
            pool: Mutex::new(Pool { connections: HashMap::new(), proxied: None, direct: None }),
        }
    }

    /// How long to wait for a server that has stopped answering. Applies to connections opened after the call.
    pub fn with_receive_timeout(mut self, timeout: Duration) -> Client {
        self.receive_timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        self
    }

    fn open_session(&self, direct: bool) -> Result<Handle, HttpError> {
        let access = if direct { WINHTTP_ACCESS_TYPE_NO_PROXY } else { WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY };
        let session = Handle(unsafe { WinHttpOpen(wide(&self.user_agent).as_ptr(), access, null(), null(), 0) });
        if session.0.is_null() {
            return Err(os_error("WinHttpOpen"));
        }
        unsafe { WinHttpSetTimeouts(session.0, RESOLVE_TIMEOUT_MS, CONNECT_TIMEOUT_MS, SEND_TIMEOUT_MS, self.receive_timeout_ms) };
        Ok(session)
    }

    /// Connect handles hold no socket; they are cached so repeated requests to one host reuse the pooled connection.
    fn connection(&self, url: &Url) -> Result<*mut c_void, HttpError> {
        let direct = url.is_loopback();
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let key = (url.host.clone(), url.port, direct);
        if let Some(existing) = pool.connections.get(&key) {
            return Ok(existing.0);
        }
        let have_session = if direct { pool.direct.is_some() } else { pool.proxied.is_some() };
        if !have_session {
            let session = self.open_session(direct)?;
            if direct {
                pool.direct = Some(session);
            } else {
                pool.proxied = Some(session);
            }
        }
        let session = if direct { pool.direct.as_ref() } else { pool.proxied.as_ref() }.map_or(null_mut(), |h| h.0);
        let connect = unsafe { WinHttpConnect(session, wide(&url.host).as_ptr(), url.port, 0) };
        if connect.is_null() {
            return Err(os_error("WinHttpConnect"));
        }
        pool.connections.insert(key, Handle(connect));
        Ok(connect)
    }

    /// Sends one GET. A 200 or 206 body goes to `sink` in chunks (return `false` from it to stop); the body of any
    /// other status is kept, truncated, in `Response::error_body`.
    pub fn get(&self, req: &Request, cancel: &AtomicBool, sink: &mut dyn FnMut(&[u8]) -> bool) -> Result<Response, HttpError> {
        if req.bearer.is_some() && !(req.url.secure || req.url.is_loopback()) {
            return Err(HttpError::InsecureToken);
        }
        let mut header_text = String::new();
        for (name, value) in &req.headers {
            if value.contains(['\r', '\n']) || name.contains(['\r', '\n', ':']) {
                return Err(HttpError::BadUrl(format!("invalid header {name}")));
            }
            header_text.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(token) = req.bearer {
            if token.expose().contains(['\r', '\n']) {
                return Err(HttpError::BadUrl("invalid token".to_string()));
            }
            header_text.push_str(&format!("Authorization: Bearer {}\r\n", token.expose()));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(HttpError::Cancelled);
        }

        let connect = self.connection(&req.url)?;
        let flags = if req.url.secure { WINHTTP_FLAG_SECURE } else { 0 };
        let request =
            Handle(unsafe { WinHttpOpenRequest(connect, wide("GET").as_ptr(), wide(&req.url.path_and_query).as_ptr(), null(), null(), null(), flags) });
        if request.0.is_null() {
            return Err(os_error("WinHttpOpenRequest"));
        }
        let decompression: u32 = DECOMPRESS_GZIP_AND_DEFLATE;
        unsafe { WinHttpSetOption(request.0, WINHTTP_OPTION_DECOMPRESSION, (&decompression as *const u32).cast(), 4) };
        let never: u32 = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        if unsafe { WinHttpSetOption(request.0, WINHTTP_OPTION_REDIRECT_POLICY, (&never as *const u32).cast(), 4) } == 0 {
            return Err(os_error("WinHttpSetOption"));
        }

        let header_wide: Vec<u16> = header_text.encode_utf16().collect();
        // With a zero length WinHTTP reads a non-null pointer as a NUL-terminated string, and an empty Vec's pointer
        // is dangling, so "no headers" must be a null pointer.
        let header_ptr = if header_wide.is_empty() { null() } else { header_wide.as_ptr() };
        if unsafe { WinHttpSendRequest(request.0, header_ptr, header_wide.len() as u32, null(), 0, 0, 0) } == 0 {
            return Err(os_error("WinHttpSendRequest"));
        }
        if unsafe { WinHttpReceiveResponse(request.0, null_mut()) } == 0 {
            return Err(os_error("WinHttpReceiveResponse"));
        }

        let raw = raw_headers(request.0)?;
        let (status, headers) = parse_raw_headers(&raw).ok_or(HttpError::Os { call: "WinHttpQueryHeaders", code: 0 })?;
        let mut response = Response { status, headers, error_body: Vec::new() };
        let to_sink = status == 200 || status == 206;
        // WinHTTP ends a body that stops short of its Content-Length as if it were complete. When it decompresses it
        // drops Content-Length and Content-Encoding, and a cut compressed stream fails on its own; the encoding check
        // keeps a compressed length from ever being compared with decompressed bytes.
        let mut expected = None;
        if to_sink {
            let declared = response.header("content-length").and_then(|v| v.parse::<u64>().ok());
            if declared.is_some_and(|n| n > req.max_body as u64) {
                return Err(HttpError::TooLarge);
            }
            if response.header("content-encoding").is_none_or(|e| e.eq_ignore_ascii_case("identity")) {
                expected = declared;
            }
        }

        let mut chunk = vec![0u8; 32 * 1024];
        let mut delivered = 0usize;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(HttpError::Cancelled);
            }
            let mut read = 0u32;
            if unsafe { WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut read) } == 0 {
                return Err(os_error("WinHttpReadData"));
            }
            if read == 0 {
                break;
            }
            let data = &chunk[..read as usize];
            if to_sink {
                delivered += data.len();
                if delivered > req.max_body {
                    return Err(HttpError::TooLarge);
                }
                if !sink(data) {
                    return Err(HttpError::Aborted);
                }
            } else {
                let room = ERROR_BODY_LIMIT.saturating_sub(response.error_body.len());
                response.error_body.extend_from_slice(&data[..data.len().min(room)]);
                if response.error_body.len() >= ERROR_BODY_LIMIT {
                    break;
                }
            }
        }
        if expected.is_some_and(|n| n != delivered as u64) {
            return Err(HttpError::Incomplete);
        }
        Ok(response)
    }

    /// Convenience for small bodies: the whole 200 body in memory.
    pub fn get_bytes(&self, req: &Request, cancel: &AtomicBool) -> Result<(Response, Vec<u8>), HttpError> {
        let mut body = Vec::new();
        let response = self.get(req, cancel, &mut |data| {
            body.extend_from_slice(data);
            true
        })?;
        Ok((response, body))
    }
}

fn raw_headers(request: *mut c_void) -> Result<String, HttpError> {
    let mut len = 0u32;
    // The first call only reports the size needed.
    unsafe { WinHttpQueryHeaders(request, WINHTTP_QUERY_RAW_HEADERS_CRLF, null(), null_mut(), &mut len, null_mut()) };
    if len == 0 {
        return Err(os_error("WinHttpQueryHeaders"));
    }
    let mut buf = vec![0u16; len as usize / 2 + 1];
    let mut len = (buf.len() * 2) as u32;
    if unsafe { WinHttpQueryHeaders(request, WINHTTP_QUERY_RAW_HEADERS_CRLF, null(), buf.as_mut_ptr().cast(), &mut len, null_mut()) } == 0 {
        return Err(os_error("WinHttpQueryHeaders"));
    }
    Ok(String::from_utf16_lossy(&buf[..len as usize / 2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_split_and_defaulted() {
        let u = Url::parse("https://API.GitHub.com/repos/a/b?x=1").unwrap();
        assert_eq!((u.secure, u.host.as_str(), u.port, u.path_and_query.as_str()), (true, "api.github.com", 443, "/repos/a/b?x=1"));
        let u = Url::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!((u.secure, u.port, u.path_and_query.as_str()), (false, 8080, "/"));
        assert!(u.is_loopback());
        assert_eq!(Url::parse("https://host?x=1").unwrap().path_and_query, "/?x=1");
        assert_eq!(Url::parse("https://host/p#frag").unwrap().path_and_query, "/p");
        assert_eq!(Url::parse("https://host:444/p").unwrap().to_string(), "https://host:444/p");
        assert_eq!(Url::parse("https://host/p").unwrap().to_string(), "https://host/p");
        assert!(!Url::parse("https://example.com/").unwrap().is_loopback());
    }

    #[test]
    fn bad_urls_are_refused() {
        for bad in [
            "ftp://x/",
            "example.com",
            "https://",
            "https:///p",
            "https://us er@host/",
            "https://host:99999/",
            "https://host/a b",
            "https://host/\u{e9}",
            "https://[::1]/",
            "https://ho st/",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn encoding_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(percent_encode("main:skills/a b"), "main%3Askills%2Fa%20b");
        assert_eq!(percent_encode("Aa0-._~"), "Aa0-._~");
        assert_eq!(percent_encode("é"), "%C3%A9");
        assert_eq!(percent_encode_path("skills/my skill/SKILL.md"), "skills/my%20skill/SKILL.md");
    }

    #[test]
    fn raw_headers_are_parsed_into_status_and_lower_case_pairs() {
        let raw = "HTTP/1.1 304 Not Modified\r\nETag: W/\"abc\"\r\nX-RateLimit-Remaining: 59\r\nRetry-After:  30 \r\n\r\n";
        let (status, headers) = parse_raw_headers(raw).unwrap();
        assert_eq!(status, 304);
        let r = Response { status, headers, error_body: vec![] };
        assert_eq!(r.header("etag"), Some("W/\"abc\""));
        assert_eq!(r.header("x-ratelimit-remaining"), Some("59"));
        assert_eq!(r.header("retry-after"), Some("30"));
        assert_eq!(r.header("location"), None);
        assert!(parse_raw_headers("garbage").is_none());
    }

    #[test]
    fn a_token_is_never_sent_over_plain_http_to_another_host() {
        let client = Client::new("tss-test");
        let token = Secret::new("t");
        let req = Request { url: Url::parse("http://example.com/").unwrap(), headers: vec![], bearer: Some(&token), max_body: 10 };
        let err = client.get(&req, &AtomicBool::new(false), &mut |_| true).unwrap_err();
        assert_eq!(err, HttpError::InsecureToken);
    }

    #[test]
    fn header_injection_is_refused() {
        let client = Client::new("tss-test");
        let req =
            Request { url: Url::parse("http://127.0.0.1:9/").unwrap(), headers: vec![("Accept", "x\r\nEvil: 1".to_string())], bearer: None, max_body: 10 };
        assert!(matches!(client.get(&req, &AtomicBool::new(false), &mut |_| true), Err(HttpError::BadUrl(_))));
    }
}
