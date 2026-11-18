//! Talking to GitHub: list a folder's tree, pin a commit, download a file.
//!
//! Nothing in here decides what to install or delete. Every failure becomes a `RemoteError`, and the engine treats
//! any `RemoteError` as "the answer is unknown": a missing network, a bad token, a rate limit, a 404 or a redirect
//! is never read as "the skill was removed upstream".

use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::RepoConfig;
use crate::secret::Secret;
use crate::tree::{EntryKind, Snapshot, parse_snapshot};
use crate::win::cred;
use crate::win::http::{Client, HttpError, Request, Response, Url, percent_encode, percent_encode_path};
use crate::win::proc;

pub const DEFAULT_API_BASE: &str = "https://api.github.com";
pub const DEFAULT_RAW_BASE: &str = "https://raw.githubusercontent.com";
/// Largest tree listing accepted. GitHub itself stops at about 7 MB and marks the result truncated.
const MAX_TREE_BYTES: usize = 16 * 1024 * 1024;
const MAX_COMMIT_BYTES: usize = 4 * 1024;
/// Room for transfer overhead on top of a file's declared size; the exact size is verified by the caller.
const FILE_SLACK_BYTES: usize = 64 * 1024;
const DEFAULT_RATE_LIMIT_WAIT_SECS: i64 = 60;

#[derive(Clone, Copy, Debug)]
pub struct RepoRef<'a> {
    pub owner: &'a str,
    pub name: &'a str,
    /// Empty means the repository's default branch.
    pub branch: &'a str,
    /// Folder inside the repository; empty for the root. No leading or trailing slash.
    pub path: &'a str,
}

impl<'a> RepoRef<'a> {
    pub fn of(cfg: &'a RepoConfig) -> Result<RepoRef<'a>, String> {
        let (owner, name) = cfg.owner_and_name().ok_or_else(|| format!("'{}' is not an owner/name repository", cfg.repo))?;
        Ok(RepoRef { owner, name, branch: &cfg.branch, path: &cfg.path })
    }

    fn reference(&self) -> &str {
        if self.branch.is_empty() { "HEAD" } else { self.branch }
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

pub enum TreeReply {
    /// The listing is the one the stored ETag stands for.
    NotModified,
    Snapshot {
        etag: Option<String>,
        snapshot: Snapshot,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// The token is missing, rejected or lacks permission.
    Auth,
    /// GitHub says it does not know the repository, branch or folder (or the caller may not see it).
    NotFound,
    RateLimited,
    Server,
    Redirect,
    Protocol,
    Network,
    TooLarge,
    Cancelled,
    /// The remote changed between listing and download.
    Changed,
    /// This computer cannot do the work (for example a full disk or a locked folder).
    Local,
}

impl FailureKind {
    /// Whether waiting is likely to fix it by itself.
    pub fn is_transient(self) -> bool {
        matches!(self, FailureKind::Network | FailureKind::Server | FailureKind::RateLimited | FailureKind::Changed | FailureKind::Cancelled)
    }
}

#[derive(Clone, Debug)]
pub struct RemoteError {
    pub kind: FailureKind,
    /// A sentence for the user.
    pub message: String,
    /// Do not try again before this time (seconds since 1970, UTC).
    pub retry_at: Option<i64>,
}

impl RemoteError {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> RemoteError {
        RemoteError { kind, message: message.into(), retry_at: None }
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RemoteError {}

pub trait Remote: Send + Sync {
    /// Lists the folder's whole tree. `etag` is the one stored with the last listing that was fully applied.
    fn tree(&self, repo: &RepoRef, etag: Option<&str>, token: Option<&Secret>, cancel: &AtomicBool) -> Result<TreeReply, RemoteError>;

    /// The commit the branch points to now, to pin file downloads to one consistent version.
    fn commit(&self, repo: &RepoRef, token: Option<&Secret>, cancel: &AtomicBool) -> Result<String, RemoteError>;

    /// Commits touching `rel`, newest first, pinned to `pin`. An empty page proves exhaustion.
    fn history(&self, repo: &RepoRef, pin: &str, rel: &str, page: u32, token: Option<&Secret>, cancel: &AtomicBool) -> Result<Vec<String>, RemoteError>;

    /// A complete historical subtree. Absence must be proven by a complete parent listing, never by a 404.
    fn historical_tree(&self, repo: &RepoRef, commit: &str, rel: &str, token: Option<&Secret>, cancel: &AtomicBool) -> Result<Option<Snapshot>, RemoteError>;

    /// Streams one file (`rel` is relative to the repository folder) as it was at `commit`.
    #[allow(clippy::too_many_arguments)]
    fn file(
        &self,
        repo: &RepoRef,
        commit: &str,
        rel: &str,
        expected_size: u64,
        token: Option<&Secret>,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<(), RemoteError>;
}

/// Where the token comes from. Resolved once per sync, never stored anywhere.
pub trait TokenProvider: Send + Sync {
    fn token(&self) -> Result<Secret, String>;
}

/// The Windows Credential Manager entry first, then `gh auth token`.
pub struct SystemTokens;

impl TokenProvider for SystemTokens {
    fn token(&self) -> Result<Secret, String> {
        if let Some(t) = cred::read_token() {
            return Ok(t);
        }
        proc::gh_token().map_err(|why| format!("no GitHub token is available: none is saved in Settings, and the GitHub CLI gave none ({why})"))
    }
}

pub fn now_epoch() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn is_commit_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn join_repo_path(folder: &str, rel: &str) -> String {
    if folder.is_empty() { rel.to_string() } else { format!("{folder}/{rel}") }
}

fn from_http(e: HttpError) -> RemoteError {
    let kind = match &e {
        HttpError::Cancelled | HttpError::Aborted => FailureKind::Cancelled,
        HttpError::TooLarge => FailureKind::TooLarge,
        HttpError::BadUrl(_) | HttpError::InsecureToken => FailureKind::Protocol,
        HttpError::Timeout | HttpError::Incomplete | HttpError::Os { .. } => FailureKind::Network,
    };
    RemoteError::new(kind, e.describe())
}

/// Turns an unsuccessful response into an error. Pure, so every status can be tested without a network.
pub fn failure(resp: &Response, what: &str, authenticated: bool, now: i64) -> RemoteError {
    let api_message = serde_json::from_slice::<serde_json::Value>(&resp.error_body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str().map(str::to_string)))
        .unwrap_or_default();
    let lower = api_message.to_ascii_lowercase();
    let exhausted = resp.header("x-ratelimit-remaining") == Some("0");
    let retry_after = resp.header("retry-after").and_then(|v| v.trim().parse::<i64>().ok());
    let limited = resp.status == 429 || (resp.status == 403 && (exhausted || retry_after.is_some() || lower.contains("rate limit") || lower.contains("abuse")));

    if limited {
        let reset = resp.header("x-ratelimit-reset").and_then(|v| v.trim().parse::<i64>().ok());
        let until = match (retry_after, reset) {
            (Some(secs), _) => now + secs.max(1),
            (None, Some(at)) if at > now => at,
            _ => now + DEFAULT_RATE_LIMIT_WAIT_SECS,
        };
        let hint = if authenticated { "" } else { " Signing in with the GitHub CLI (gh auth login) or saving a token in Settings raises the limit." };
        return RemoteError {
            kind: FailureKind::RateLimited,
            message: format!("GitHub's request limit has been reached; waiting before trying {what} again.{hint}"),
            retry_at: Some(until),
        };
    }
    let detail = if api_message.is_empty() { String::new() } else { format!(" (GitHub said: {})", api_message.chars().take(160).collect::<String>()) };
    match resp.status {
        401 => RemoteError::new(FailureKind::Auth, format!("GitHub did not accept the token for {what}; it may have expired or been revoked{detail}")),
        403 => RemoteError::new(FailureKind::Auth, format!("GitHub refused access to {what}{detail}")),
        404 => {
            let why = if authenticated { "or your GitHub sign-in cannot see it" } else { "or it is private" };
            RemoteError::new(FailureKind::NotFound, format!("GitHub could not find {what}: it may not exist, the branch or folder may be wrong, {why}"))
        }
        300..=399 => RemoteError::new(
            FailureKind::Redirect,
            format!(
                "GitHub redirected the request for {what} (the repository may have been renamed or moved). Redirects are not followed; update the repository name in Settings"
            ),
        ),
        500..=599 => RemoteError::new(FailureKind::Server, format!("GitHub is having problems right now (HTTP {}); will try again later", resp.status)),
        other => RemoteError::new(FailureKind::Protocol, format!("GitHub answered {what} with an unexpected status ({other}){detail}")),
    }
}

pub struct GitHubRemote {
    client: Client,
    api: String,
    raw: String,
}

/// A base address is accepted only if it is https, or plain http to this computer (used by the tests' fake server).
fn clean_base(value: &str) -> Result<String, String> {
    let url = Url::parse(value).map_err(|_| format!("'{value}' is not a usable web address"))?;
    if !(url.secure || url.is_loopback()) {
        return Err(format!("'{value}' is not allowed: only https, or http to this computer, can be used"));
    }
    Ok(value.trim_end_matches('/').to_string())
}

impl GitHubRemote {
    pub fn new() -> GitHubRemote {
        GitHubRemote::with_bases(DEFAULT_API_BASE, DEFAULT_RAW_BASE).expect("the built-in addresses are valid")
    }

    pub fn with_bases(api: &str, raw: &str) -> Result<GitHubRemote, String> {
        let agent = format!("tiny-skill-sync/{}", env!("CARGO_PKG_VERSION"));
        Ok(GitHubRemote { client: Client::new(&agent), api: clean_base(api)?, raw: clean_base(raw)? })
    }

    /// Uses `TSS_API_BASE` and `TSS_RAW_BASE` when set (for tests); otherwise GitHub itself.
    pub fn from_env() -> Result<GitHubRemote, String> {
        let api = std::env::var("TSS_API_BASE").unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
        let raw = std::env::var("TSS_RAW_BASE").unwrap_or_else(|_| DEFAULT_RAW_BASE.to_string());
        GitHubRemote::with_bases(&api, &raw)
    }

    fn api_url(&self, repo: &RepoRef, tail: &str) -> Result<Url, RemoteError> {
        let text = format!("{}/repos/{}/{}/{tail}", self.api, percent_encode(repo.owner), percent_encode(repo.name));
        Url::parse(&text).map_err(from_http)
    }

    fn tree_at(&self, repo: &RepoRef, reference: &str, recursive: bool, token: Option<&Secret>, cancel: &AtomicBool) -> Result<Snapshot, RemoteError> {
        let query = if recursive { "?recursive=1" } else { "" };
        let url = self.api_url(repo, &format!("git/trees/{}{query}", percent_encode(reference)))?;
        let request = Request { url, headers: api_headers("application/vnd.github+json"), bearer: token, max_body: MAX_TREE_BYTES };
        let (resp, body) = self.client.get_bytes(&request, cancel).map_err(from_http)?;
        if resp.status != 200 {
            return Err(failure(&resp, &repo.label(), token.is_some(), now_epoch()));
        }
        let snapshot = parse_snapshot(&body).map_err(|why| RemoteError::new(FailureKind::Protocol, why))?;
        if !is_commit_sha(&snapshot.sha) || snapshot.entries.iter().any(|e| !is_commit_sha(&e.sha)) {
            return Err(RemoteError::new(FailureKind::Protocol, "GitHub returned an invalid historical tree id"));
        }
        if snapshot.truncated {
            return Err(RemoteError::new(FailureKind::TooLarge, "GitHub cut a historical listing short; nothing was changed"));
        }
        Ok(snapshot)
    }
}

impl Default for GitHubRemote {
    fn default() -> Self {
        GitHubRemote::new()
    }
}

fn api_headers(accept: &str) -> Vec<(&'static str, String)> {
    vec![("Accept", accept.to_string()), ("X-GitHub-Api-Version", "2022-11-28".to_string())]
}

impl Remote for GitHubRemote {
    fn history(&self, repo: &RepoRef, pin: &str, rel: &str, page: u32, token: Option<&Secret>, cancel: &AtomicBool) -> Result<Vec<String>, RemoteError> {
        if !is_commit_sha(pin) || page == 0 {
            return Err(RemoteError::new(FailureKind::Protocol, "history was requested without a pinned commit or page"));
        }
        let path = if rel.is_empty() { repo.path.to_string() } else { join_repo_path(repo.path, rel) };
        let url = self.api_url(repo, &format!("commits?sha={pin}&path={}&per_page=100&page={page}", percent_encode(&path)))?;
        let request = Request { url, headers: api_headers("application/vnd.github+json"), bearer: token, max_body: MAX_TREE_BYTES };
        let (resp, body) = self.client.get_bytes(&request, cancel).map_err(from_http)?;
        if resp.status != 200 {
            return Err(failure(&resp, &repo.label(), token.is_some(), now_epoch()));
        }
        #[derive(serde::Deserialize)]
        struct Commit {
            sha: String,
        }
        let commits: Vec<Commit> =
            serde_json::from_slice(&body).map_err(|_| RemoteError::new(FailureKind::Protocol, "GitHub returned an incomplete history page"))?;
        let mut seen = std::collections::HashSet::new();
        if commits.len() > 100 || commits.iter().any(|c| !is_commit_sha(&c.sha) || !seen.insert(c.sha.to_ascii_lowercase())) {
            return Err(RemoteError::new(FailureKind::Protocol, "GitHub returned an invalid history page"));
        }
        Ok(commits.into_iter().map(|c| c.sha.to_ascii_lowercase()).collect())
    }

    fn historical_tree(&self, repo: &RepoRef, commit: &str, rel: &str, token: Option<&Secret>, cancel: &AtomicBool) -> Result<Option<Snapshot>, RemoteError> {
        if !is_commit_sha(commit) {
            return Err(RemoteError::new(FailureKind::Protocol, "a historical listing was requested without a pinned commit"));
        }
        let path = if rel.is_empty() { repo.path.to_string() } else { join_repo_path(repo.path, rel) };
        let mut reference = commit.to_string();
        for component in path.split('/').filter(|p| !p.is_empty()) {
            let parent = self.tree_at(repo, &reference, false, token, cancel)?;
            let mut seen = std::collections::HashSet::new();
            for entry in &parent.entries {
                if crate::pathsafe::check_component(&entry.path).is_err() || !seen.insert(crate::pathsafe::fold(&entry.path)) || !is_commit_sha(&entry.sha) {
                    return Err(RemoteError::new(FailureKind::Protocol, "GitHub returned an unsafe historical parent listing"));
                }
            }
            let Some(entry) = parent.entries.iter().find(|e| e.path == component) else { return Ok(None) };
            if entry.kind != EntryKind::Tree || entry.mode != "040000" {
                return Err(RemoteError::new(FailureKind::Protocol, "a historical folder is not a plain tree"));
            }
            reference = entry.sha.clone();
        }
        self.tree_at(repo, &reference, true, token, cancel).map(Some)
    }

    fn tree(&self, repo: &RepoRef, etag: Option<&str>, token: Option<&Secret>, cancel: &AtomicBool) -> Result<TreeReply, RemoteError> {
        // The whole "ref:path" is one encoded segment; GitHub accepts it for any branch name and nested folder.
        let spec = format!("{}:{}", repo.reference(), repo.path);
        let url = self.api_url(repo, &format!("git/trees/{}?recursive=1", percent_encode(&spec)))?;
        let mut headers = api_headers("application/vnd.github+json");
        if let Some(etag) = etag {
            headers.push(("If-None-Match", etag.to_string()));
        }
        let request = Request { url, headers, bearer: token, max_body: MAX_TREE_BYTES };
        let (resp, body) = self.client.get_bytes(&request, cancel).map_err(from_http)?;
        match resp.status {
            304 if etag.is_some() => Ok(TreeReply::NotModified),
            304 => Err(RemoteError::new(FailureKind::Protocol, "GitHub answered 'not modified' to a request that had no ETag")),
            200 => {
                let snapshot = parse_snapshot(&body).map_err(|e| RemoteError::new(FailureKind::Protocol, e))?;
                if snapshot.truncated {
                    return Err(RemoteError::new(
                        FailureKind::TooLarge,
                        format!("the folder in {} is too large for GitHub to list in one piece; point the repository at a smaller folder", repo.label()),
                    ));
                }
                Ok(TreeReply::Snapshot { etag: resp.header("etag").map(str::to_string), snapshot })
            }
            _ => Err(failure(&resp, &repo.label(), token.is_some(), now_epoch())),
        }
    }

    fn commit(&self, repo: &RepoRef, token: Option<&Secret>, cancel: &AtomicBool) -> Result<String, RemoteError> {
        let url = self.api_url(repo, &format!("commits/{}", percent_encode(repo.reference())))?;
        let request = Request { url, headers: api_headers("application/vnd.github.sha"), bearer: token, max_body: MAX_COMMIT_BYTES };
        let (resp, body) = self.client.get_bytes(&request, cancel).map_err(from_http)?;
        if resp.status != 200 {
            return Err(failure(&resp, &repo.label(), token.is_some(), now_epoch()));
        }
        let sha = String::from_utf8_lossy(&body).trim().to_ascii_lowercase();
        if is_commit_sha(&sha) { Ok(sha) } else { Err(RemoteError::new(FailureKind::Protocol, "GitHub did not return a commit id")) }
    }

    fn file(
        &self,
        repo: &RepoRef,
        commit: &str,
        rel: &str,
        expected_size: u64,
        token: Option<&Secret>,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<(), RemoteError> {
        if !is_commit_sha(commit) {
            return Err(RemoteError::new(FailureKind::Protocol, "a download was requested without a pinned commit"));
        }
        let path = join_repo_path(repo.path, rel);
        let text = format!("{}/{}/{}/{}/{}", self.raw, percent_encode(repo.owner), percent_encode(repo.name), commit, percent_encode_path(&path));
        let url = Url::parse(&text).map_err(from_http)?;
        let max_body = usize::try_from(expected_size).unwrap_or(usize::MAX).saturating_add(FILE_SLACK_BYTES);
        let request = Request { url, headers: Vec::new(), bearer: token, max_body };
        let resp = self.client.get(&request, cancel, sink).map_err(from_http)?;
        if resp.status == 200 { Ok(()) } else { Err(failure(&resp, &format!("{}/{path}", repo.label()), token.is_some(), now_epoch())) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
        Response { status, headers: headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect(), error_body: body.as_bytes().to_vec() }
    }

    #[test]
    fn rate_limits_are_recognised_however_github_says_them() {
        let now = 1_000;
        let by_remaining = failure(&resp(403, &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1500")], "{}"), "r", false, now);
        assert_eq!((by_remaining.kind, by_remaining.retry_at), (FailureKind::RateLimited, Some(1500)));
        let by_429 = failure(&resp(429, &[("retry-after", "30")], ""), "r", true, now);
        assert_eq!((by_429.kind, by_429.retry_at), (FailureKind::RateLimited, Some(1030)));
        let by_message = failure(&resp(403, &[], r#"{"message":"You have exceeded a secondary rate limit."}"#), "r", true, now);
        assert_eq!((by_message.kind, by_message.retry_at), (FailureKind::RateLimited, Some(1060)));
        let stale_reset = failure(&resp(403, &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "900")], "{}"), "r", true, now);
        assert_eq!(stale_reset.retry_at, Some(1060), "a reset time in the past still waits a little");
        assert!(by_remaining.message.contains("gh auth login"), "anonymous users are told how to get more room");
        assert!(!by_429.message.contains("gh auth login"));
    }

    #[test]
    fn other_failures_are_classified_and_none_of_them_is_a_removal() {
        let now = 1_000;
        let cases = [
            (401, FailureKind::Auth),
            (403, FailureKind::Auth),
            (404, FailureKind::NotFound),
            (301, FailureKind::Redirect),
            (302, FailureKind::Redirect),
            (307, FailureKind::Redirect),
            (308, FailureKind::Redirect),
            (500, FailureKind::Server),
            (502, FailureKind::Server),
            (503, FailureKind::Server),
            (418, FailureKind::Protocol),
            (204, FailureKind::Protocol),
        ];
        for (status, kind) in cases {
            let e = failure(&resp(status, &[], r#"{"message":"boom"}"#), "owner/repo", false, now);
            assert_eq!(e.kind, kind, "status {status}");
            assert!(e.retry_at.is_none());
            assert!(!e.message.is_empty());
        }
        assert!(failure(&resp(404, &[], ""), "r", false, now).message.contains("private"));
        assert!(failure(&resp(404, &[], ""), "r", true, now).message.contains("sign-in cannot see it"));
        assert!(failure(&resp(403, &[], r#"{"message":"Resource not accessible"}"#), "r", true, now).message.contains("Resource not accessible"));
    }

    #[test]
    fn long_or_hostile_server_messages_are_cut_down() {
        let long = format!(r#"{{"message":"{}"}}"#, "x".repeat(5000));
        let e = failure(&resp(403, &[], &long), "r", true, 0);
        assert!(e.message.len() < 400);
        assert!(failure(&resp(403, &[], "<html>not json</html>"), "r", true, 0).kind == FailureKind::Auth);
    }

    #[test]
    fn only_transient_kinds_are_worth_waiting_for() {
        for kind in [FailureKind::Network, FailureKind::Server, FailureKind::RateLimited, FailureKind::Changed, FailureKind::Cancelled] {
            assert!(kind.is_transient());
        }
        for kind in [FailureKind::Auth, FailureKind::NotFound, FailureKind::Redirect, FailureKind::Protocol, FailureKind::TooLarge, FailureKind::Local] {
            assert!(!kind.is_transient());
        }
    }

    #[test]
    fn base_addresses_must_be_https_or_loopback() {
        assert_eq!(clean_base("https://api.github.com/").unwrap(), "https://api.github.com");
        assert_eq!(clean_base("http://127.0.0.1:8080").unwrap(), "http://127.0.0.1:8080");
        assert_eq!(clean_base("http://localhost:9").unwrap(), "http://localhost:9");
        assert!(clean_base("http://example.com").is_err());
        assert!(clean_base("http://127.attacker.example").is_err(), "a name that starts with 127. is not this computer");
        assert!(clean_base("http://127.0.0.1.evil.com").is_err());
        assert!(clean_base("ftp://x").is_err());
        assert!(clean_base("not a url").is_err());
        assert!(GitHubRemote::with_bases("http://example.com", DEFAULT_RAW_BASE).is_err());
    }

    #[test]
    fn urls_encode_the_reference_path_and_names() {
        let remote = GitHubRemote::with_bases("https://api.github.com", "https://raw.githubusercontent.com").unwrap();
        let repo = RepoRef { owner: "Owner", name: "my.repo", branch: "feature/x y", path: "skills/my skills" };
        let url = remote.api_url(&repo, &format!("git/trees/{}?recursive=1", percent_encode(&format!("{}:{}", repo.reference(), repo.path)))).unwrap();
        assert_eq!(url.host, "api.github.com");
        assert_eq!(url.path_and_query, "/repos/Owner/my.repo/git/trees/feature%2Fx%20y%3Askills%2Fmy%20skills?recursive=1");
        let root = RepoRef { owner: "o", name: "n", branch: "", path: "" };
        assert_eq!(root.reference(), "HEAD");
        assert_eq!(join_repo_path(root.path, "a/b.md"), "a/b.md");
        assert_eq!(join_repo_path("skills", "a/b.md"), "skills/a/b.md");
    }

    #[test]
    fn only_full_commit_ids_are_accepted() {
        assert!(is_commit_sha(&"a".repeat(40)));
        assert!(is_commit_sha("8c3d5be0d1e0c40c1e0ea18337ef5a910e1067cf"));
        assert!(!is_commit_sha("main"));
        assert!(!is_commit_sha(&"g".repeat(40)));
        assert!(!is_commit_sha(&"a".repeat(39)));
        assert!(!is_commit_sha("../../evil"));
    }

    #[test]
    fn a_file_request_without_a_pinned_commit_is_refused_before_any_network_use() {
        let remote = GitHubRemote::with_bases("http://127.0.0.1:9", "http://127.0.0.1:9").unwrap();
        let repo = RepoRef { owner: "o", name: "n", branch: "", path: "" };
        let e = remote.file(&repo, "main", "a.md", 1, None, &AtomicBool::new(false), &mut |_| true).unwrap_err();
        assert_eq!(e.kind, FailureKind::Protocol);
    }
}
