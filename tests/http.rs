//! The real GitHub client and the real WinHTTP transport against a fake GitHub on this computer.
//!
//! The engine suites use an in-memory GitHub. These tests cover what that fake cannot: the exact requests on the
//! wire, how real status codes, headers, compressed or chunked bodies and broken connections turn into results, and
//! whole checks end to end over a socket. The end-to-end tests serve `tests\fixtures\upstream-v1`, the first version
//! of the live fixture repository.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::{FakeTokens, World, config, only, repo, sync_all};
use serde_json::json;
use tiny_skill_sync::config::RepoConfig;
use tiny_skill_sync::engine::{Engine, Outcome, SyncReport};
use tiny_skill_sync::github::{FailureKind, GitHubRemote, Remote, RemoteError, RepoRef, TreeReply, now_epoch};
use tiny_skill_sync::hash::blob_hex;
use tiny_skill_sync::secret::Secret;
use tiny_skill_sync::win::http::{Client, HttpError, Request, Url};

// ---------------------------------------------------------------------------------------------------------------------
// A tiny HTTP/1.1 server: it records every request and answers with whatever the test's handler says.
// ---------------------------------------------------------------------------------------------------------------------

/// One request as the server received it.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    target: String,
    /// Lower-case names.
    headers: Vec<(String, String)>,
    /// Which connection (1, 2, ...) it came on.
    connection: usize,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

enum Body {
    /// Framed by `Content-Length`.
    Sized(Vec<u8>),
    /// Framed by chunked transfer encoding; `finished: false` leaves out the closing empty chunk.
    Chunked { data: Vec<u8>, finished: bool },
    /// Promises `declared` bytes in `Content-Length` but sends only `data`.
    Short { declared: usize, data: Vec<u8> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum After {
    /// Wait for the next request on the same connection.
    KeepOpen,
    Close,
    /// Send nothing more, and keep the connection open until the server is dropped.
    Stall,
}

struct Reply {
    /// `None`: say nothing at all.
    status: Option<u16>,
    headers: Vec<(String, String)>,
    body: Body,
    after: After,
}

fn answer(status: u16, body: impl Into<Vec<u8>>) -> Reply {
    Reply { status: Some(status), headers: Vec::new(), body: Body::Sized(body.into()), after: After::KeepOpen }
}

fn silence() -> Reply {
    Reply { status: None, headers: Vec::new(), body: Body::Sized(Vec::new()), after: After::Stall }
}

impl Reply {
    fn header(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn take_sized(&mut self) -> Vec<u8> {
        match std::mem::replace(&mut self.body, Body::Sized(Vec::new())) {
            Body::Sized(data) => data,
            _ => panic!("only a sized body can be framed another way"),
        }
    }

    fn chunked(mut self, finished: bool) -> Reply {
        let data = self.take_sized();
        self.body = Body::Chunked { data, finished };
        self
    }

    /// Promises the whole body but sends only its first `sent` bytes.
    fn cut_to(mut self, sent: usize) -> Reply {
        let data = self.take_sized();
        self.body = Body::Short { declared: data.len(), data: data[..sent].to_vec() };
        self
    }

    fn then(mut self, after: After) -> Reply {
        self.after = after;
        self
    }
}

fn wire(reply: &Reply) -> Vec<u8> {
    let Some(status) = reply.status else { return Vec::new() };
    let mut head = format!("HTTP/1.1 {status} Fake\r\n");
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let mut body = Vec::new();
    match &reply.body {
        Body::Sized(data) => {
            head.push_str(&format!("Content-Length: {}\r\n", data.len()));
            body.extend_from_slice(data);
        }
        Body::Short { declared, data } => {
            head.push_str(&format!("Content-Length: {declared}\r\n"));
            body.extend_from_slice(data);
        }
        Body::Chunked { data, finished } => {
            head.push_str("Transfer-Encoding: chunked\r\n");
            for piece in data.chunks(1000) {
                body.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
                body.extend_from_slice(piece);
                body.extend_from_slice(b"\r\n");
            }
            if *finished {
                body.extend_from_slice(b"0\r\n\r\n");
            }
        }
    }
    if reply.after == After::Close {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(&body);
    out
}

type Handler = dyn Fn(&Seen) -> Reply + Send + Sync;

struct FakeServer {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    connections: Arc<AtomicUsize>,
    /// Every connection accepted, to cut them all when the server is dropped.
    open: Arc<Mutex<Vec<TcpStream>>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl FakeServer {
    fn start(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port on this computer");
        let port = listener.local_addr().unwrap().port();
        let handler: Arc<Handler> = Arc::new(handler);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let open = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen_by_server, counter, open_by_server, stop_seen_by_server) =
            (Arc::clone(&seen), Arc::clone(&connections), Arc::clone(&open), Arc::clone(&stop));
        let accept = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_seen_by_server.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                if let Ok(copy) = stream.try_clone() {
                    open_by_server.lock().unwrap().push(copy);
                }
                let number = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let (handler, seen, stop) = (Arc::clone(&handler), Arc::clone(&seen_by_server), Arc::clone(&stop_seen_by_server));
                thread::spawn(move || serve(stream, number, &*handler, &seen, &stop));
            }
        });
        FakeServer { port, seen, connections, open, stop, accept: Some(accept) }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The real client, with the API under `/api` and the raw downloads under `/raw`.
    fn remote(&self) -> GitHubRemote {
        GitHubRemote::with_bases(&format!("{}/api", self.base()), &format!("{}/raw", self.base())).expect("plain http to this computer is allowed")
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop so it sees the flag; the listener closes when the thread ends.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        // A client may keep idle connections for reuse; a server that is gone answers on none of them.
        for stream in self.open.lock().unwrap().drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn serve(stream: TcpStream, connection: usize, handler: &Handler, seen: &Mutex<Vec<Seen>>, stop: &AtomicBool) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut out = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut words = line.split_whitespace();
        let method = words.next().unwrap_or_default().to_string();
        let target = words.next().unwrap_or_default().to_string();
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }
        let request = Seen { method, target, headers, connection };
        seen.lock().unwrap().push(request.clone());
        let reply = handler(&request);
        if out.write_all(&wire(&reply)).and_then(|()| out.flush()).is_err() {
            return;
        }
        match reply.after {
            After::KeepOpen => {}
            After::Close => {
                let _ = out.shutdown(Shutdown::Both);
                return;
            }
            After::Stall => {
                while !stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(20));
                }
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Helpers for the wire-level tests
// ---------------------------------------------------------------------------------------------------------------------

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn historical_pages_are_pinned_and_invalid_pages_are_not_exhaustion() {
    let server = FakeServer::start(|req| {
        if req.target.ends_with("page=2") { answer(200, json!([{"sha": SHA}]).to_string()) } else { answer(200, r#"[{"sha":"not a commit"}]"#) }
    });
    let remote = server.remote();
    assert_eq!(remote.history(&at("other", "my skills"), SHA, "alpha", 2, None, &go()).unwrap(), [SHA]);
    assert_eq!(server.seen()[0].target, format!("/api/repos/o/n/commits?sha={SHA}&path=my%20skills%2Falpha&per_page=100&page=2"));
    assert_eq!(remote.history(&at("", ""), SHA, "alpha", 1, None, &go()).unwrap_err().kind, FailureKind::Protocol);
}

#[test]
fn historical_absence_needs_a_complete_parent_and_404_is_never_absence() {
    for (status, body, absent) in [
        (200, json!({"sha":SHA,"tree":[],"truncated":false}).to_string(), true),
        (404, "{}".into(), false),
        (200, json!({"sha":SHA,"tree":[],"truncated":true}).to_string(), false),
        (200, json!({"sha":SHA}).to_string(), false),
        (200, json!({"sha":SHA,"tree":[{"path":"../escape","mode":"040000","type":"tree","sha":SHA}],"truncated":false}).to_string(), false),
    ] {
        let server = FakeServer::start(move |_| answer(status, body.clone()));
        let got = server.remote().historical_tree(&at("", "skills"), SHA, "alpha", None, &go());
        if absent {
            assert!(got.unwrap().is_none());
        } else {
            assert!(got.is_err(), "{got:?}");
        }
    }
}

/// A small listing in GitHub's format: one skill folder with its `SKILL.md`.
const LISTING: &str = r#"{"sha":"abc","url":"u","tree":[
    {"path":"alpha","mode":"040000","type":"tree","sha":"11","url":"u"},
    {"path":"alpha/SKILL.md","mode":"100644","type":"blob","sha":"22","size":5,"url":"u"}],"truncated":false}"#;

fn at<'a>(branch: &'a str, path: &'a str) -> RepoRef<'a> {
    RepoRef { owner: "o", name: "n", branch, path }
}

fn go() -> AtomicBool {
    AtomicBool::new(false)
}

fn tree_error(result: Result<TreeReply, RemoteError>) -> RemoteError {
    match result {
        Ok(_) => panic!("expected the listing to fail"),
        Err(e) => e,
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A gzip stream that stores `data` without compressing it (one deflate block of type 0): enough to show that the
/// client asks for, and unpacks, compressed answers.
fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let len = u16::try_from(data.len()).expect("one stored block holds at most 64 KiB");
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff, 1];
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

// ---------------------------------------------------------------------------------------------------------------------
// What goes on the wire
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_listing_is_requested_as_github_documents_it_and_its_etag_is_sent_back_verbatim() {
    let etag = "\"5f1c0e7a\"";
    let server = FakeServer::start(move |req| match req.header("if-none-match") {
        Some(sent) if sent == etag => answer(304, "").header("ETag", etag),
        _ => answer(200, LISTING).header("ETag", etag).header("Content-Type", "application/json; charset=utf-8"),
    });
    let remote = server.remote();

    let Ok(TreeReply::Snapshot { etag: got, snapshot }) = remote.tree(&at("", "skills"), None, None, &go()) else { panic!("expected a listing") };
    assert_eq!(got.as_deref(), Some(etag), "the ETag is kept with its quotes");
    assert_eq!((snapshot.sha.as_str(), snapshot.entries.len()), ("abc", 2));
    let Ok(TreeReply::NotModified) = remote.tree(&at("", "skills"), Some(etag), None, &go()) else { panic!("expected 'not modified'") };
    assert!(remote.tree(&at("feature/x", "my skills/set"), None, None, &go()).is_ok());

    let seen = server.seen();
    let targets: Vec<&str> = seen.iter().map(|s| s.target.as_str()).collect();
    assert_eq!(
        targets,
        [
            "/api/repos/o/n/git/trees/HEAD%3Askills?recursive=1",
            "/api/repos/o/n/git/trees/HEAD%3Askills?recursive=1",
            "/api/repos/o/n/git/trees/feature%2Fx%3Amy%20skills%2Fset?recursive=1",
        ]
    );
    for s in &seen {
        assert_eq!(s.method, "GET");
        assert_eq!(s.header("accept"), Some("application/vnd.github+json"));
        assert_eq!(s.header("x-github-api-version"), Some("2022-11-28"));
        assert!(s.header("user-agent").is_some_and(|a| a.starts_with("tiny-skill-sync/")), "{s:?}");
        assert_eq!(s.header("authorization"), None, "no token was given");
    }
    assert_eq!(seen[0].header("if-none-match"), None);
    assert_eq!(seen[1].header("if-none-match"), Some(etag));
    assert!(seen.iter().all(|s| s.connection == 1) && server.connections() == 1, "the requests share one kept-alive connection");
}

#[test]
fn a_token_goes_in_a_bearer_header_on_every_kind_of_request_and_only_when_given() {
    let server = FakeServer::start(|req| {
        if req.target.contains("/commits/") {
            answer(200, SHA)
        } else if req.target.starts_with("/raw/") {
            answer(200, "hello")
        } else {
            answer(200, LISTING)
        }
    });
    let remote = server.remote();
    let repo = at("", "skills");
    let token = Secret::new("tss-test-token".to_string());
    for given in [Some(&token), None] {
        assert!(remote.tree(&repo, None, given, &go()).is_ok());
        assert_eq!(remote.commit(&repo, given, &go()).unwrap(), SHA);
        remote.file(&repo, SHA, "alpha/SKILL.md", 5, given, &go(), &mut |_| true).unwrap();
    }
    let seen = server.seen();
    let sent: Vec<Option<&str>> = seen.iter().map(|s| s.header("authorization")).collect();
    let with = Some("Bearer tss-test-token");
    assert_eq!(sent, [with, with, with, None, None, None]);
}

#[test]
fn the_commit_pin_is_read_from_the_bare_answer_and_anything_else_is_refused() {
    let server = FakeServer::start(|req| match req.target.as_str() {
        "/api/repos/o/n/commits/HEAD" => answer(200, format!("{}\n", SHA.to_ascii_uppercase())),
        "/api/repos/o/n/commits/feature%2Fx" => answer(200, SHA),
        _ => answer(200, "<html>not a commit</html>"),
    });
    let remote = server.remote();
    assert_eq!(remote.commit(&at("", "skills"), None, &go()).unwrap(), SHA);
    assert_eq!(remote.commit(&at("feature/x", "skills"), None, &go()).unwrap(), SHA);
    assert_eq!(remote.commit(&at("other", "skills"), None, &go()).unwrap_err().kind, FailureKind::Protocol);
    for s in server.seen() {
        assert_eq!(s.header("accept"), Some("application/vnd.github.sha"));
        assert_eq!(s.header("x-github-api-version"), Some("2022-11-28"));
    }
}

#[test]
fn a_file_is_fetched_at_the_pinned_commit_with_every_path_segment_encoded_and_streamed_whole() {
    let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let served = content.clone();
    let server = FakeServer::start(move |_| answer(200, served.clone()));
    let remote = server.remote();

    let (mut got, mut pieces) = (Vec::new(), 0);
    let mut sink = |data: &[u8]| {
        got.extend_from_slice(data);
        pieces += 1;
        true
    };
    remote.file(&at("", "skills"), SHA, "my skill/notes #1 & 100% caf\u{e9}.md", content.len() as u64, None, &go(), &mut sink).unwrap();
    assert_eq!(got, content);
    assert!(pieces > 1, "the body arrives in pieces, not all at once");
    remote.file(&at("", ""), SHA, "SKILL.md", content.len() as u64, None, &go(), &mut |_| true).unwrap();

    let unpinned = remote.file(&at("", ""), "main", "SKILL.md", 1, None, &go(), &mut |_| true).unwrap_err();
    assert_eq!(unpinned.kind, FailureKind::Protocol, "a download is never made without a pinned commit");

    let seen = server.seen();
    let targets: Vec<&str> = seen.iter().map(|s| s.target.as_str()).collect();
    let nested = format!("/raw/o/n/{SHA}/skills/my%20skill/notes%20%231%20%26%20100%25%20caf%C3%A9.md");
    assert_eq!(targets, [nested, format!("/raw/o/n/{SHA}/SKILL.md")]);
}

#[test]
fn compressed_and_chunked_answers_read_the_same_as_plain_ones() {
    let server = FakeServer::start(|req| {
        let gzip_welcome = req.header("accept-encoding").is_some_and(|v| v.contains("gzip"));
        match (req.target.contains("HEAD%3Agzip"), gzip_welcome) {
            (true, true) => answer(200, gzip_stored(LISTING.as_bytes())).header("Content-Encoding", "gzip"),
            (true, false) => answer(406, r#"{"message":"the client did not ask for gzip"}"#),
            (false, _) => answer(200, LISTING).chunked(true),
        }
    });
    let remote = server.remote();
    for folder in ["gzip", "chunked"] {
        let Ok(TreeReply::Snapshot { snapshot, .. }) = remote.tree(&at("", folder), None, None, &go()) else { panic!("{folder}: expected a listing") };
        assert_eq!((snapshot.sha.as_str(), snapshot.entries.len()), ("abc", 2), "{folder}");
    }
}

#[test]
fn a_redirect_is_reported_and_never_followed() {
    for status in [301, 302, 307, 308] {
        let server = FakeServer::start(move |req| {
            if req.target.starts_with("/api/moved") {
                answer(200, LISTING)
            } else {
                answer(status, "").header("Location", "/api/moved/repos/o/n2/git/trees/HEAD%3A?recursive=1")
            }
        });
        let e = tree_error(server.remote().tree(&at("", ""), None, None, &go()));
        assert_eq!(e.kind, FailureKind::Redirect, "{status}: {e}");
        assert_eq!(server.seen().len(), 1, "{status}: the redirect was followed");
    }
}

/// Status, error body, whether a token was sent, the failure, and words its message must have.
const ERROR_ANSWERS: &[(u16, &str, bool, FailureKind, &str)] = &[
    (401, r#"{"message":"Bad credentials"}"#, true, FailureKind::Auth, "GitHub said: Bad credentials"),
    (403, r#"{"message":"Resource not accessible by personal access token"}"#, true, FailureKind::Auth, "Resource not accessible"),
    (404, r#"{"message":"Not Found"}"#, false, FailureKind::NotFound, "or it is private"),
    (404, r#"{"message":"Not Found"}"#, true, FailureKind::NotFound, "your GitHub sign-in cannot see it"),
    (500, "", false, FailureKind::Server, "HTTP 500"),
    (503, "<html>unavailable</html>", false, FailureKind::Server, "HTTP 503"),
    (418, r#"{"message":"I'm a teapot"}"#, false, FailureKind::Protocol, "unexpected status (418) (GitHub said: I'm a teapot)"),
    (304, "", false, FailureKind::Protocol, "no ETag"),
];

#[test]
fn real_error_answers_become_the_failures_the_engine_expects() {
    let token = Secret::new("t".to_string());
    for &(status, body, with_token, kind, says) in ERROR_ANSWERS {
        let server = FakeServer::start(move |_| answer(status, body));
        let e = tree_error(server.remote().tree(&at("", ""), None, with_token.then_some(&token), &go()));
        assert_eq!(e.kind, kind, "{status}: {e}");
        assert!(e.message.contains(says), "{status}: {e}");
    }
}

#[test]
fn rate_limits_over_the_wire_carry_githubs_waiting_time() {
    let now = now_epoch();
    let reset = (now + 600).to_string();
    let server = FakeServer::start(move |req| {
        if req.target.contains("HEAD%3Aprimary") {
            answer(403, r#"{"message":"API rate limit exceeded for 192.0.2.1."}"#).header("X-RateLimit-Remaining", "0").header("X-RateLimit-Reset", &reset)
        } else {
            answer(429, "").header("Retry-After", "7")
        }
    });
    let remote = server.remote();
    let e = tree_error(remote.tree(&at("", "primary"), None, None, &go()));
    assert_eq!((e.kind, e.retry_at), (FailureKind::RateLimited, Some(now + 600)), "{e}");
    assert!(e.message.contains("gh auth login"), "without a token the message says how to raise the limit: {e}");
    let e = tree_error(remote.tree(&at("", "secondary"), None, None, &go()));
    assert_eq!(e.kind, FailureKind::RateLimited, "{e}");
    let wait = e.retry_at.expect("a time to wait until") - now;
    assert!((7..=9).contains(&wait), "waits as long as Retry-After says, not {wait}s");
}

#[test]
fn answers_larger_than_allowed_are_refused_however_they_are_framed() {
    // A commit answer may be 4 KiB; a download may be its listed size plus 64 KiB.
    let server = FakeServer::start(|req| match req.target.rsplit('/').next().unwrap_or_default() {
        "sized" => answer(200, "a".repeat(5000)),
        "chunked" => answer(200, "a".repeat(5000)).chunked(true),
        "announced" => answer(200, "a".repeat(5000)).cut_to(0).then(After::Close),
        "fits" => answer(200, vec![b'z'; 10 + 65_536]),
        "too-big" => answer(200, vec![b'z'; 10 + 65_537]),
        _ => answer(502, format!("<html>{}</html>", "x".repeat(2_000_000))),
    });
    let remote = server.remote();
    for framing in ["sized", "chunked", "announced"] {
        assert_eq!(remote.commit(&at(framing, ""), None, &go()).unwrap_err().kind, FailureKind::TooLarge, "{framing}");
    }
    assert!(remote.file(&at("", ""), SHA, "fits", 10, None, &go(), &mut |_| true).is_ok(), "the size itself is checked by the caller");
    assert_eq!(remote.file(&at("", ""), SHA, "too-big", 10, None, &go(), &mut |_| true).unwrap_err().kind, FailureKind::TooLarge);
    let e = tree_error(remote.tree(&at("", "error-page"), None, None, &go()));
    assert_eq!(e.kind, FailureKind::Server, "a long error page is read only in part: {e}");
}

#[test]
fn a_connection_that_breaks_in_the_middle_of_an_answer_is_a_network_failure() {
    let server = FakeServer::start(|req| {
        let body: Vec<u8> = if req.target.contains("/git/trees/") {
            LISTING.into()
        } else if req.target.contains("/commits/") {
            SHA.into()
        } else {
            vec![b'z'; 100]
        };
        if req.target.contains("unterminated") {
            answer(200, body).chunked(false).then(After::Close)
        } else if req.target.contains("gzipped") {
            answer(200, gzip_stored(&body)).header("Content-Encoding", "gzip").cut_to(20).then(After::Close)
        } else {
            answer(200, body).cut_to(20).then(After::Close)
        }
    });
    let remote = server.remote();
    for name in ["short", "unterminated", "gzipped"] {
        let mut got = 0;
        let e = remote
            .file(&at("", ""), SHA, name, 100, None, &go(), &mut |data| {
                got += data.len();
                true
            })
            .unwrap_err();
        assert_eq!(e.kind, FailureKind::Network, "{name} download: {e}");
        assert!(got < 100, "{name}: {got} bytes");
        let e = tree_error(remote.tree(&at("", name), None, None, &go()));
        assert_eq!(e.kind, FailureKind::Network, "{name} listing: a lost connection, not a malformed answer: {e}");
        let e = remote.commit(&at(name, ""), None, &go()).unwrap_err();
        assert_eq!(e.kind, FailureKind::Network, "{name} commit: {e}");
    }
}

#[test]
fn nobody_listening_is_a_network_failure() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let remote = GitHubRemote::with_bases(&format!("http://127.0.0.1:{port}/api"), &format!("http://127.0.0.1:{port}/raw")).unwrap();
    let started = Instant::now();
    let e = tree_error(remote.tree(&at("", ""), None, None, &go()));
    assert_eq!(e.kind, FailureKind::Network, "{e}");
    assert!(started.elapsed() < Duration::from_secs(20), "a refused connection is not waited on for long ({:?})", started.elapsed());
}

#[test]
fn a_server_that_stops_answering_is_given_up_on_after_the_receive_timeout() {
    // WinHTTP checks its timers every few seconds: a 300 ms timeout fires after about 4 s on this host.
    let server = FakeServer::start(|req| if req.target == "/silent" { silence() } else { answer(200, vec![b'z'; 100]).cut_to(10).then(After::Stall) });
    let client = Client::new("tss-test").with_receive_timeout(Duration::from_millis(300));
    for target in ["/silent", "/half"] {
        let request = Request { url: Url::parse(&format!("{}{target}", server.base())).unwrap(), headers: Vec::new(), bearer: None, max_body: 1000 };
        let started = Instant::now();
        let result = client.get_bytes(&request, &go());
        assert_eq!(result.unwrap_err(), HttpError::Timeout, "{target}");
        assert!(started.elapsed() < Duration::from_secs(20), "{target}: the 30 s default was used ({:?})", started.elapsed());
    }
}

#[test]
fn a_cancel_is_honoured_before_sending_and_between_reads() {
    let server = FakeServer::start(|_| answer(200, vec![b'z'; 300_000]));
    let remote = server.remote();
    assert_eq!(tree_error(remote.tree(&at("", ""), None, None, &AtomicBool::new(true))).kind, FailureKind::Cancelled);
    assert!(server.seen().is_empty(), "nothing is sent once a cancel has been asked for");

    let cancel = AtomicBool::new(false);
    let mut got = 0;
    let e = remote
        .file(&at("", ""), SHA, "big", 300_000, None, &cancel, &mut |data| {
            got += data.len();
            cancel.store(true, Ordering::Relaxed);
            true
        })
        .unwrap_err();
    assert_eq!(e.kind, FailureKind::Cancelled);
    assert!(got < 300_000, "the download stopped early, after {got} bytes");

    let e = remote.file(&at("", ""), SHA, "big", 300_000, None, &go(), &mut |_| false).unwrap_err();
    assert_eq!(e.kind, FailureKind::Cancelled, "the sink can stop a download too");
}

// ---------------------------------------------------------------------------------------------------------------------
// Whole checks over a socket: a fake GitHub that serves one repository, `o/fixtures`, from a map of files
// ---------------------------------------------------------------------------------------------------------------------

const TREE: &str = "/api/repos/o/fixtures/git/trees/HEAD%3Askills?recursive=1";
const COMMIT: &str = "/api/repos/o/fixtures/commits/HEAD";

struct Upstream {
    /// Repository path to content.
    files: BTreeMap<String, Vec<u8>>,
    /// When set, the repository is private: no token answers 404 and a wrong one 401, as GitHub does.
    token: Option<String>,
}

/// `tests\fixtures\upstream-v1`, the first version of the live fixture repository.
fn fixture_files() -> BTreeMap<String, Vec<u8>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("upstream-v1");
    let mut files = BTreeMap::new();
    let mut folders = vec![root.clone()];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else {
                let rel = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
                files.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// The listing of `skills/` in GitHub's format, its ETag and the commit it belongs to.
fn listing(files: &BTreeMap<String, Vec<u8>>) -> (String, String, String) {
    let mut entries = Vec::new();
    let mut folders = BTreeSet::new();
    for (path, content) in files {
        let Some(rel) = path.strip_prefix("skills/") else { continue };
        for (i, _) in rel.match_indices('/') {
            folders.insert(&rel[..i]);
        }
        entries.push(json!({"path": rel, "mode": "100644", "type": "blob", "sha": blob_hex(content), "size": content.len()}));
    }
    for folder in folders {
        entries.push(json!({"path": folder, "mode": "040000", "type": "tree", "sha": blob_hex(folder.as_bytes())}));
    }
    let tree_sha = blob_hex(serde_json::to_string(&entries).unwrap().as_bytes());
    let body = json!({"sha": tree_sha, "url": "u", "tree": entries, "truncated": false}).to_string();
    let etag = format!("\"{}\"", &tree_sha[..20]);
    let commit = blob_hex(format!("commit {tree_sha}").as_bytes());
    (body, etag, commit)
}

fn decode(s: &str) -> String {
    let mut out = Vec::new();
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' && tail.len() >= 2 {
            out.push(u8::from_str_radix(std::str::from_utf8(&tail[..2]).unwrap(), 16).unwrap());
            rest = &tail[2..];
        } else {
            out.push(b);
            rest = tail;
        }
    }
    String::from_utf8(out).unwrap()
}

fn fake_github(upstream: Arc<Mutex<Upstream>>) -> impl Fn(&Seen) -> Reply + Send + Sync + 'static {
    move |req| {
        let up = upstream.lock().unwrap();
        if let Some(token) = &up.token {
            match req.header("authorization") {
                None => return answer(404, r#"{"message":"Not Found"}"#),
                Some(sent) if sent != format!("Bearer {token}") => return answer(401, r#"{"message":"Bad credentials"}"#),
                Some(_) => {}
            }
        }
        let (body, etag, commit) = listing(&up.files);
        if req.target == TREE {
            return if req.header("if-none-match") == Some(etag.as_str()) {
                answer(304, "").header("ETag", &etag)
            } else {
                answer(200, body).header("ETag", &etag).header("Content-Type", "application/json; charset=utf-8")
            };
        }
        if req.target == COMMIT {
            return answer(200, commit);
        }
        let file = req.target.strip_prefix(&format!("/raw/o/fixtures/{commit}/")).map(decode).and_then(|path| up.files.get(&path));
        match file {
            Some(content) => answer(200, content.clone()),
            None => answer(404, "404: Not Found"),
        }
    }
}

fn engine(w: &World, remote: GitHubRemote, tokens: &FakeTokens) -> Engine {
    Engine::new(w.paths(), Box::new(remote), tokens.provider())
}

/// Listings, commit lookups and downloads among `seen`.
fn counts(seen: &[Seen]) -> (usize, usize, usize) {
    let of = |f: &dyn Fn(&Seen) -> bool| seen.iter().filter(|s| f(s)).count();
    (of(&|s| s.target == TREE), of(&|s| s.target == COMMIT), of(&|s| s.target.starts_with("/raw/")))
}

/// Every file of the named upstream skills is installed, byte for byte, and no file of any other upstream folder is.
fn assert_installed(w: &World, files: &BTreeMap<String, Vec<u8>>, skills: &[&str]) {
    for (path, content) in files {
        let Some(rel) = path.strip_prefix("skills/") else { continue };
        let local = w.skills.join(rel.replace('/', "\\"));
        if skills.contains(&rel.split('/').next().unwrap()) {
            assert_eq!(fs::read(&local).ok().as_ref(), Some(content), "{rel}");
        } else {
            assert!(!local.exists(), "{rel} must not be installed");
        }
    }
}

#[track_caller]
fn failure_of(report: &SyncReport) -> FailureKind {
    match &only(report).outcome {
        Outcome::Failed(f) => f.kind,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn whole_checks_over_http_add_update_and_delete_skills_and_an_unchanged_one_costs_one_request() {
    let upstream = Arc::new(Mutex::new(Upstream { files: fixture_files(), token: None }));
    let server = FakeServer::start(fake_github(Arc::clone(&upstream)));
    let w = World::new("http-e2e");
    let cfg = config(vec![repo("fx", "o/fixtures", "skills")]);
    let mut engine = engine(&w, server.remote(), &FakeTokens::none());

    let report = sync_all(&mut engine, &cfg);
    assert!(matches!(only(&report).outcome, Outcome::Synced), "{report:?}");
    assert_installed(&w, &upstream.lock().unwrap().files, &["alpha", "beta", "gamma"]);
    assert_eq!(counts(&server.seen()), (1, 1, 5), "one listing, one commit lookup and the five skill files");

    let before = server.seen().len();
    let report = sync_all(&mut engine, &cfg);
    assert!(matches!(only(&report).outcome, Outcome::Unchanged), "{report:?}");
    let seen = server.seen();
    let (_, etag, _) = listing(&upstream.lock().unwrap().files);
    assert_eq!(seen.len() - before, 1, "an unchanged repository costs one request");
    assert_eq!(seen[before].header("if-none-match"), Some(etag.as_str()));

    {
        let mut up = upstream.lock().unwrap();
        up.files.insert("skills/beta/SKILL.md".into(), b"---\nname: beta\ndescription: second skill\n---\n# Beta v2\n".to_vec());
        up.files.insert("skills/delta/SKILL.md".into(), b"---\nname: delta\ndescription: fourth skill\n---\n".to_vec());
        up.files.insert("skills/delta/notes & ideas #1 (100%).md".into(), b"an awkward name\n".to_vec());
        up.files.insert("skills/delta/r\u{e9}f\u{e9}rences/caf\u{e9}.md".into(), b"accents\n".to_vec());
        up.files.retain(|path, _| !path.starts_with("skills/gamma/"));
    }
    let before = server.seen().len();
    let report = sync_all(&mut engine, &cfg);
    assert!(matches!(only(&report).outcome, Outcome::Synced), "{report:?}");
    assert_eq!(counts(&server.seen()[before..]), (1, 1, 4), "only the changed and the new files are downloaded");
    assert_installed(&w, &upstream.lock().unwrap().files, &["alpha", "beta", "delta"]);
    assert!(!w.skills.join("gamma").exists(), "a skill removed upstream is removed here");
}

#[test]
fn a_private_repository_gets_the_token_on_every_request_and_losing_access_removes_nothing() {
    let upstream = Arc::new(Mutex::new(Upstream { files: fixture_files(), token: Some("tss-private-token".into()) }));
    let expected = upstream.lock().unwrap().files.clone();
    let server = FakeServer::start(fake_github(Arc::clone(&upstream)));
    let w = World::new("http-private");
    let private = RepoConfig { use_token: true, ..repo("fx", "o/fixtures", "skills") };
    let cfg = config(vec![private.clone()]);
    let all = ["alpha", "beta", "gamma"];

    let report = sync_all(&mut engine(&w, server.remote(), &FakeTokens::some("tss-private-token")), &cfg);
    assert!(matches!(only(&report).outcome, Outcome::Synced), "{report:?}");
    assert!(server.seen().iter().all(|s| s.header("authorization") == Some("Bearer tss-private-token")));
    assert_installed(&w, &expected, &all);

    let report = sync_all(&mut engine(&w, server.remote(), &FakeTokens::some("revoked")), &cfg);
    assert_eq!(failure_of(&report), FailureKind::Auth, "a revoked token");
    assert_installed(&w, &expected, &all);

    let public = config(vec![RepoConfig { use_token: false, ..private }]);
    let report = sync_all(&mut engine(&w, server.remote(), &FakeTokens::none()), &public);
    assert_eq!(failure_of(&report), FailureKind::NotFound, "'use token' turned off");
    assert_installed(&w, &expected, &all);

    let remote = server.remote();
    drop(server);
    let report = sync_all(&mut engine(&w, remote, &FakeTokens::some("tss-private-token")), &cfg);
    assert_eq!(failure_of(&report), FailureKind::Network, "GitHub cannot be reached");
    assert_installed(&w, &expected, &all);
}
