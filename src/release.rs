//! Updates come only from the public, stable release of this app. The API's two SHA-256 digests,
//! not a downloaded checksum file or an asset server, are the evidence for the bytes returned.

use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::github::TokenProvider;
use crate::interval::{add_months, format_utc, now_secs};
use crate::secret::Secret;
use crate::win::http::{HttpError, Response, Url};
use crate::win::{proc, release_hash};

const LATEST: &str = "https://api.github.com/repos/AngelosP/tiny-skill-sync/releases/latest";
const BY_TAG: &str = "https://api.github.com/repos/AngelosP/tiny-skill-sync/releases/tags";
const DOWNLOAD: &str = "https://github.com/AngelosP/tiny-skill-sync/releases/download";
const NAMES: [&str; 2] = ["tiny-skill-sync.exe", "tss.exe"];
const MAX_METADATA: usize = 256 * 1024;
const MAX_EXE: usize = 8 * 1024 * 1024;
const MAX_REDIRECTS: usize = 2;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(90);
// Allow for process setup and pipe cleanup as well as gh's wait.
const TOKEN_LOOKUP_BUDGET: Duration = proc::GH_TIMEOUT.saturating_add(Duration::from_secs(2));
const END_OF_YEAR_9999: i64 = 253_402_300_799;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Version(u64, u64, u64);

/// The installed/printed form is `X.Y.Z`; the release tag must additionally start with a literal `v`.
impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let invalid = || "Expected a version like 1.2.3.".to_string();
        let mut parts = s.split('.');
        let mut number = || {
            let s = parts.next().ok_or_else(invalid)?;
            if s.is_empty() || (s.len() > 1 && s.starts_with('0')) || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            s.parse::<u64>().map_err(|_| invalid())
        };
        let version = Self(number()?, number()?, number()?);
        if parts.next().is_some() { Err(invalid()) } else { Ok(version) }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Asset {
    size: usize,
    digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Offer {
    pub version: Version,
    // Keep the digests bound to the tag that supplied them even if a caller changes the public display field.
    source_version: Version,
    assets: [Asset; 2],
    // This is an in-memory continuation of the metadata request, not a new network operation.
    deadline: Instant,
    // Installed-version evidence must never double as an offer to download older executables.
    update: bool,
}

impl Offer {
    /// Index 0 is the tray, 1 the CLI.
    pub(crate) fn expected(&self, index: usize) -> (usize, [u8; 32]) {
        let a = self.assets[index];
        (a.size, a.digest)
    }
}
type VerifiedFile = (Vec<u8>, [u8; 32]);

/// Constructed only when BOTH downloads match the same release metadata.
pub struct VerifiedPair {
    version: Version,
    files: [VerifiedFile; 2],
}

impl VerifiedPair {
    pub fn version(&self) -> Version {
        self.version
    }

    /// Index 0 is the tray, 1 the CLI.
    pub fn file(&self, index: usize) -> (&[u8], [u8; 32]) {
        let (bytes, digest) = &self.files[index];
        (bytes, *digest)
    }

    /// Compare a pending, immutable pair with newly checked metadata without rehashing or downloading it.
    pub(crate) fn matches(&self, offer: &Offer) -> bool {
        self.version == offer.version
            && self.version == offer.source_version
            && self.files.iter().enumerate().all(|(i, (bytes, digest))| (bytes.len(), *digest) == offer.expected(i))
    }

    #[cfg(test)]
    pub(crate) fn fixture(version: Version, tray: &[u8], cli: &[u8]) -> Self {
        Self { version, files: [tray, cli].map(|b| (b.to_vec(), sha256(b))) }
    }
}

impl fmt::Debug for VerifiedPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedPair").field("version", &self.version).finish_non_exhaustive()
    }
}

#[cfg(test)]
impl Offer {
    pub(crate) fn fixture(version: Version) -> Self {
        Self::fixture_files(version, b"x", b"x")
    }

    pub(crate) fn fixture_files(version: Version, tray: &[u8], cli: &[u8]) -> Self {
        Self {
            version,
            source_version: version,
            deadline: Instant::now() + OPERATION_TIMEOUT,
            update: true,
            assets: [tray, cli].map(|bytes| Asset { size: bytes.len(), digest: sha256(bytes) }),
        }
    }
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    release_hash::sha256(bytes)
}

pub(crate) fn sha256_reader(reader: impl Read) -> io::Result<[u8; 32]> {
    release_hash::sha256_reader(reader)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseError {
    pub message: String,
    /// First time the server says we may retry, in UTC seconds since the Unix epoch.
    pub retry_at: Option<i64>,
}

fn error(message: &str) -> ReleaseError {
    ReleaseError { message: message.to_string(), retry_at: None }
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ReleaseError {}

pub trait Feed {
    fn check(&self, installed: Version, now: i64, cancel: &AtomicBool) -> Result<Option<Offer>, ReleaseError>;
    /// Fetch metadata, never old binaries. The newer offer carries the original check's monotonic deadline.
    fn originals(&self, _offer: &Offer, _installed: Version, _now: i64, _cancel: &AtomicBool) -> Result<Offer, ReleaseError> {
        Err(error("The installed release could not be verified against GitHub."))
    }
    fn fetch(&self, offer: &Offer, now: i64, cancel: &AtomicBool) -> Result<VerifiedPair, ReleaseError>;
}

type Reply = Result<(Response, Vec<u8>), HttpError>;
// Only this module can construct destinations. A download URL cannot be used with a bearer.
pub(crate) struct MetadataUrl(Url);
pub(crate) struct DownloadUrl(Url);

#[cfg(test)]
impl MetadataUrl {
    pub(crate) fn fixture(address: &str) -> Self {
        Self(Url::parse(address).unwrap())
    }
}

#[cfg(test)]
impl DownloadUrl {
    pub(crate) fn fixture(address: &str) -> Self {
        Self(Url::parse(address).unwrap())
    }
}

/// No configurable base URL, proxy, referrer or arbitrary headers.
pub(crate) enum ReleaseRequest<'a> {
    Metadata { url: &'a MetadataUrl, bearer: Option<&'a Secret> },
    Download(&'a DownloadUrl),
}

impl ReleaseRequest<'_> {
    pub(crate) fn url(&self) -> &Url {
        match self {
            Self::Metadata { url, .. } => &url.0,
            Self::Download(url) => &url.0,
        }
    }

    pub(crate) fn bearer(&self) -> Option<&Secret> {
        match self {
            Self::Metadata { bearer, .. } => *bearer,
            Self::Download(_) => None,
        }
    }
}

type Get<'a> = dyn Fn(ReleaseRequest<'_>, usize, &Attempt<'_>) -> Reply + 'a;

pub(crate) struct Attempt<'a> {
    deadline: Instant,
    cancel: &'a AtomicBool,
    clock: &'a dyn Fn() -> Instant,
    receipt_time: &'a dyn Fn() -> i64,
}

impl Attempt<'_> {
    pub(crate) fn remaining(&self) -> Result<Duration, HttpError> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(HttpError::Cancelled);
        }
        self.deadline.checked_duration_since((self.clock)()).filter(|d| !d.is_zero()).ok_or(HttpError::Timeout)
    }

    fn running(&self) -> Result<(), ReleaseError> {
        self.remaining().map(|_| ()).map_err(http_error)
    }
}

#[cfg(test)]
impl<'a> Attempt<'a> {
    pub(crate) fn with_timeout(cancel: &'a AtomicBool, timeout: Duration) -> Self {
        Self { deadline: Instant::now() + timeout, cancel, clock: &Instant::now, receipt_time: &now_secs }
    }
}

pub struct Official<T: TokenProvider> {
    tokens: T,
}

impl<T: TokenProvider> Official<T> {
    pub fn new(tokens: T) -> Self {
        Self { tokens }
    }
}

impl<T: TokenProvider> Feed for Official<T> {
    fn check(&self, installed: Version, _now: i64, cancel: &AtomicBool) -> Result<Option<Offer>, ReleaseError> {
        check_at(&release_hash::get, &self.tokens, installed, cancel, &Instant::now, &now_secs)
    }

    fn originals(&self, offer: &Offer, installed: Version, _now: i64, cancel: &AtomicBool) -> Result<Offer, ReleaseError> {
        originals_at(&release_hash::get, &self.tokens, offer, installed, cancel, &Instant::now, &now_secs)
    }

    fn fetch(&self, offer: &Offer, _now: i64, cancel: &AtomicBool) -> Result<VerifiedPair, ReleaseError> {
        fetch_at(&release_hash::get, offer, cancel, &Instant::now, &now_secs)
    }
}

fn http_error(e: HttpError) -> ReleaseError {
    // BadUrl can contain a signed Location. Never include a URL, response body or server-supplied text in an error.
    error(match e {
        HttpError::Cancelled | HttpError::Aborted => "The update check was cancelled.",
        HttpError::TooLarge => "The update server sent more data than allowed.",
        HttpError::Incomplete => "The update download ended early.",
        HttpError::Timeout => "The update server did not answer in time.",
        HttpError::BadUrl(_) | HttpError::InsecureToken => "The update server supplied an invalid address.",
        HttpError::Os { .. } => "The update server could not be reached securely.",
    })
}

fn one_header<'a>(r: &'a Response, name: &str) -> Result<Option<&'a str>, ReleaseError> {
    let mut values = r.headers.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let first = values.next();
    if values.next().is_some() { Err(error("The update server sent conflicting response headers.")) } else { Ok(first) }
}

fn decimal(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) { None } else { s.parse().ok() }
}

/// HTTP dates include two older wire formats. Check the entire value; Windows' date parser accepts trailing junk.
fn http_date(s: &str, now: i64) -> Option<i64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let number = |s: &str, width: usize| if s.len() == width { decimal(s) } else { None };
    if s.len() > 40 || !s.is_ascii() {
        return None;
    }
    let words: Vec<_> = s.split(' ').collect();
    let (weekday, long_weekday, day, month, year, clock) = match words.as_slice() {
        [w, d, m, y, t, "GMT"] => (w.strip_suffix(',')?, false, number(d, 2)?, *m, number(y, 4)?, *t),
        [w, date, t, "GMT"] => {
            let (d, rest) = date.split_once('-')?;
            let (m, y) = rest.split_once('-')?;
            // An obsolete two-digit year cannot mean more than fifty years in the future (RFC 9110).
            let current = format_utc(now.clamp(-11_644_473_600, END_OF_YEAR_9999));
            let latest = decimal(&current[..4])? + 50;
            (w.strip_suffix(',')?, true, number(d, 2)?, m, latest - (latest - number(y, 2)?).rem_euclid(100), *t)
        }
        [w, m, "", d, t, y] => (*w, false, number(d, 1)?, *m, number(y, 4)?, *t),
        [w, m, d, t, y] if !d.starts_with('0') => (*w, false, number(d, 2)?, *m, number(y, 4)?, *t),
        _ => return None,
    };
    let (h, mmss) = clock.split_once(':')?;
    let (m, sec) = mmss.split_once(':')?;
    let (h, m, sec) = (number(h, 2)?, number(m, 2)?, number(sec, 2)?);
    let months = (year - 1970) * 12 + MONTHS.iter().position(|&s| s == month)? as i64;
    let start = add_months(0, months);
    if !(1..=(add_months(start, 1) - start) / 86_400).contains(&day) || h > 23 || m > 59 || sec > 59 {
        return None;
    }
    let at = start + (day - 1) * 86_400 + h * 3600 + m * 60 + sec;
    let name = DAYS[(at.div_euclid(86_400) + 4).rem_euclid(7) as usize];
    (weekday == if long_weekday { name } else { &name[..3] }).then_some(at)
}

struct ResponseFailure {
    error: ReleaseError,
    limited: bool,
}

fn response_error(r: &Response, now: i64) -> ResponseFailure {
    let header = |name| one_header(r, name).ok().flatten();
    let retry = header("retry-after").and_then(|v| if let Some(delta) = decimal(v) { now.checked_add(delta) } else { http_date(v, now) });
    let reset = header("x-ratelimit-reset").and_then(decimal);
    // Keep only representable, non-stale times. A malformed Retry-After must not hide a valid primary limit reset.
    let usable = |at: &i64| *at >= now && *at <= END_OF_YEAR_9999;
    let remaining = header("x-ratelimit-remaining");
    let limited = r.status == 429 || (r.status == 403 && (remaining == Some("0") || retry.is_some() || (remaining.is_none() && reset.is_some())));
    let error = if limited {
        ReleaseError { message: "GitHub has limited update requests. Try again later.".into(), retry_at: retry.filter(usable).or_else(|| reset.filter(usable)) }
    } else {
        ReleaseError { message: format!("The update server returned HTTP {}.", r.status), retry_at: if r.status == 503 { retry.filter(usable) } else { None } }
    };
    ResponseFailure { error, limited }
}

fn read(get: &Get<'_>, request: ReleaseRequest<'_>, limit: usize, work: &Attempt<'_>) -> Result<(Response, Vec<u8>), ReleaseError> {
    work.running()?;
    let reply = get(request, limit, work);
    work.running()?;
    let (r, bytes) = reply.map_err(http_error)?;
    if r.status == 200 {
        if bytes.len() > limit {
            return Err(http_error(HttpError::TooLarge));
        }
        if let Some(value) = one_header(&r, "content-length")? {
            let n = decimal(value).ok_or_else(|| error("The update server sent an invalid file size."))?;
            if n != bytes.len() as i64 {
                return Err(http_error(HttpError::Incomplete));
            }
        }
        if one_header(&r, "content-range")?.is_some() || one_header(&r, "content-encoding")?.is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
            return Err(error("The update server did not send the original, complete file."));
        }
    }
    Ok((r, bytes))
}

#[derive(Deserialize)]
struct WireRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<WireAsset>,
}

#[derive(Deserialize)]
struct WireAsset {
    name: String,
    state: String,
    size: u64,
    digest: Option<String>,
    browser_download_url: String,
}

fn download_url(version: Version, index: usize) -> String {
    format!("{DOWNLOAD}/v{version}/{}", NAMES[index])
}

fn parse_digest(value: &str) -> Option<[u8; 32]> {
    let hex = value.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0; 32];
    for (i, b) in out.iter_mut().enumerate() {
        let digits = &hex[i * 2..i * 2 + 2];
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        *b = u8::from_str_radix(digits, 16).ok()?;
    }
    Some(out)
}

fn metadata_at<T>(
    get: &Get<'_>,
    tokens: &dyn TokenProvider,
    url: &str,
    work: &Attempt<'_>,
    validate: impl FnOnce(WireRelease) -> Result<T, ReleaseError>,
) -> Result<T, ReleaseError> {
    let validate_body = |body: &[u8]| {
        let listing = serde_json::from_slice(body).map_err(|_| error("GitHub sent incomplete or invalid release details."))?;
        work.running()?;
        validate(listing)
    };
    let url = MetadataUrl(Url::parse(url).expect("fixed HTTPS API and numeric version"));
    let (r, body) = read(get, ReleaseRequest::Metadata { url: &url, bearer: None }, MAX_METADATA, work)?;
    if r.status == 200 {
        return validate_body(&body);
    }
    // Metadata redirects are never followed, not even to another API endpoint.
    let ResponseFailure { error: original, limited } = response_error(&r, (work.receipt_time)());
    if !limited || !work.remaining().is_ok_and(|remaining| remaining > TOKEN_LOOKUP_BUDGET) {
        return Err(original);
    }
    let retry = {
        let Ok(bearer) = tokens.token() else { return Err(original) };
        // read checks the same attempt again before sending, including cancellation during lookup.
        read(get, ReleaseRequest::Metadata { url: &url, bearer: Some(&bearer) }, MAX_METADATA, work)
    };
    match retry {
        Ok((r, body)) if r.status == 200 => validate_body(&body).map_err(|_| original),
        _ => Err(original),
    }
}

fn check_at(
    get: &Get<'_>,
    tokens: &dyn TokenProvider,
    installed: Version,
    cancel: &AtomicBool,
    clock: &dyn Fn() -> Instant,
    receipt_time: &dyn Fn() -> i64,
) -> Result<Option<Offer>, ReleaseError> {
    let work = Attempt { deadline: clock() + OPERATION_TIMEOUT, cancel, clock, receipt_time };
    metadata_at(get, tokens, LATEST, &work, |listing| {
        if listing.draft || listing.prerelease {
            return Ok(None);
        }
        let version = listing
            .tag_name
            .strip_prefix('v')
            .and_then(|v| v.parse::<Version>().ok())
            .ok_or_else(|| error("The release tag is not a stable vX.Y.Z version."))?;
        if version <= installed {
            return Ok(None);
        }
        checked_assets(listing, version, true, &work).map(Some)
    })
}

fn originals_at(
    get: &Get<'_>,
    tokens: &dyn TokenProvider,
    offer: &Offer,
    installed: Version,
    cancel: &AtomicBool,
    clock: &dyn Fn() -> Instant,
    receipt_time: &dyn Fn() -> i64,
) -> Result<Offer, ReleaseError> {
    if !offer.update || offer.version != offer.source_version || installed >= offer.version {
        return Err(error("A newer update offer is needed to verify the installed release."));
    }
    let work = Attempt { deadline: offer.deadline, cancel, clock, receipt_time };
    let tag = format!("v{installed}");
    metadata_at(get, tokens, &format!("{BY_TAG}/{tag}"), &work, |listing| {
        if listing.tag_name != tag || listing.prerelease || listing.draft {
            return Err(error("GitHub did not return the exact installed stable release."));
        }
        checked_assets(listing, installed, false, &work)
    })
}

fn checked_assets(listing: WireRelease, version: Version, update: bool, work: &Attempt<'_>) -> Result<Offer, ReleaseError> {
    let mut found = [None; 2];
    for a in listing.assets {
        let Some(i) = NAMES.iter().position(|name| name.eq_ignore_ascii_case(&a.name)) else { continue };
        if a.name != NAMES[i] || found[i].is_some() {
            return Err(error("The release has ambiguous executable names."));
        }
        if a.state != "uploaded" || a.size == 0 || a.size > MAX_EXE as u64 || a.browser_download_url != download_url(version, i) {
            return Err(error("An executable in this release has the wrong address, size or upload state."));
        }
        let digest = a.digest.as_deref().and_then(parse_digest).ok_or_else(|| error("GitHub did not supply a valid SHA-256 digest for both executables."))?;
        found[i] = Some(Asset { size: a.size as usize, digest });
    }
    let [Some(tray), Some(cli)] = found else { return Err(error("The release is missing one or both standalone executables.")) };
    work.running()?;
    Ok(Offer { version, source_version: version, assets: [tray, cli], deadline: work.deadline, update })
}

fn cdn_location(location: &str) -> Result<DownloadUrl, ReleaseError> {
    let bad = || error("The release download was redirected to an unapproved address.");
    if location.len() > 8192 || !location.bytes().all(|b| (b'!'..=b'~').contains(&b)) || location.contains(['\\', '#']) {
        return Err(bad());
    }
    // Check the literal authority before URL parsing can remove or normalize anything. No ports, userinfo,
    // relative URLs, hostname suffix matching or schemeless URLs. Only the path and query may vary.
    let (host, tail) = location.strip_prefix("https://").and_then(|s| s.split_once('/')).ok_or_else(bad)?;
    if !matches!(host, "release-assets.githubusercontent.com" | "objects.githubusercontent.com") || tail.is_empty() || tail.starts_with(['/', '?']) {
        return Err(bad());
    }
    let mut bytes = tail.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let mut nibble = || bytes.next().and_then(|b| (b as char).to_digit(16)).ok_or_else(bad);
            let decoded = (nibble()? << 4 | nibble()?) as u8;
            if decoded.is_ascii_control() || decoded == b'\\' {
                return Err(bad());
            }
        } else if !(b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/?".contains(&b)) {
            return Err(bad());
        }
    }
    Url::parse(location).map(DownloadUrl).map_err(|_| bad())
}

fn fetch_file(get: &Get<'_>, offer: &Offer, index: usize, work: &Attempt<'_>) -> Result<VerifiedFile, ReleaseError> {
    let asset = offer.assets[index];
    let mut url = DownloadUrl(Url::parse(&download_url(offer.source_version, index)).expect("fixed HTTPS prefix and numeric version"));
    for hops in 0..=MAX_REDIRECTS {
        let (r, bytes) = read(get, ReleaseRequest::Download(&url), asset.size, work)?;
        match r.status {
            200 => {
                if bytes.len() != asset.size {
                    return Err(error("A downloaded executable does not have the size GitHub reported."));
                }
                if sha256(&bytes) != asset.digest {
                    return Err(error("A downloaded executable does not match GitHub's SHA-256 digest."));
                }
                work.running()?;
                return Ok((bytes, asset.digest));
            }
            301 | 302 | 303 | 307 | 308 if hops < MAX_REDIRECTS => {
                let location = one_header(&r, "location")?.ok_or_else(|| error("The download redirect has no destination."))?;
                url = cdn_location(location)?;
            }
            _ => return Err(response_error(&r, (work.receipt_time)()).error),
        }
    }
    unreachable!("last response returns without following a redirect")
}

fn fetch_at(
    get: &Get<'_>,
    offer: &Offer,
    cancel: &AtomicBool,
    clock: &dyn Fn() -> Instant,
    receipt_time: &dyn Fn() -> i64,
) -> Result<VerifiedPair, ReleaseError> {
    let work = Attempt { deadline: offer.deadline, cancel, clock, receipt_time };
    if !offer.update || offer.version != offer.source_version {
        return Err(error("The update offer no longer matches the release."));
    }
    let tray = fetch_file(get, offer, 0, &work)?;
    let cli = fetch_file(get, offer, 1, &work)?;
    work.running()?;
    let pair = VerifiedPair { version: offer.source_version, files: [tray, cli] };
    debug_assert!(pair.matches(offer));
    Ok(pair)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    use serde_json::{Value, json};

    use super::*;
    use crate::hash::hex;

    const API: &str = "https://api.github.com/repos/AngelosP/tiny-skill-sync/releases/latest";
    const TAG_API: &str = "https://api.github.com/repos/AngelosP/tiny-skill-sync/releases/tags/v1.9.9";
    const FIXTURE_BEARER: &str = "fixture-bearer";
    const BASE: &str = "https://github.com/AngelosP/tiny-skill-sync/releases/download/v1.10.0";
    const CDN: &str = "https://release-assets.githubusercontent.com/download/id?signature=do-not-show";
    const TRAY: &[u8] = b"tray fixture";
    const CLI: &[u8] = b"command line fixture";
    const NOW: i64 = 10_000;

    struct NoLookup;

    impl TokenProvider for NoLookup {
        fn token(&self) -> Result<Secret, String> {
            panic!("the token should not be looked up")
        }
    }

    struct Lookup<F>(F);

    impl<F: Fn() -> Result<Secret, String> + Send + Sync> TokenProvider for Lookup<F> {
        fn token(&self) -> Result<Secret, String> {
            (self.0)()
        }
    }

    fn fixture_bearer() -> Result<Secret, String> {
        Ok(Secret::new(FIXTURE_BEARER))
    }

    fn no_token() -> Result<Secret, String> {
        Err("No fixture token was found.".into())
    }

    fn check(get: &Get<'_>, installed: Version, now: i64, cancel: &AtomicBool) -> Result<Option<Offer>, ReleaseError> {
        check_at(get, &NoLookup, installed, cancel, &Instant::now, &|| now)
    }

    fn fetch(get: &Get<'_>, offer: &Offer, now: i64, cancel: &AtomicBool) -> Result<VerifiedPair, ReleaseError> {
        fetch_at(get, offer, cancel, &Instant::now, &|| now)
    }

    fn metadata() -> Value {
        let asset = |name: &str, data: &[u8]| {
            json!({
                "state": "uploaded", "name": name, "size": data.len(), "digest": format!("sha256:{}", hex(&sha256(data))),
                "browser_download_url": format!("{BASE}/{name}")
            })
        };
        json!({
            "tag_name":"v1.10.0", "draft":false, "prerelease":false,
            "assets":[asset("tss.exe", CLI), asset("SHA256SUMS.txt", b"not evidence"), asset("manual.zip", b"not used"), asset("tiny-skill-sync.exe", TRAY)]
        })
    }

    fn old_metadata() -> Value {
        let mut old = metadata();
        old["tag_name"] = "v1.9.9".into();
        for a in old["assets"].as_array_mut().unwrap() {
            a["browser_download_url"] = a["browser_download_url"].as_str().unwrap().replace("v1.10.0", "v1.9.9").into();
        }
        old
    }

    fn response(status: u16, headers: &[(&str, &str)], bytes: &[u8]) -> Reply {
        Ok((Response { status, headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), error_body: vec![] }, bytes.to_vec()))
    }

    fn script(steps: Vec<(String, usize, Reply)>) -> impl Fn(ReleaseRequest<'_>, usize, &Attempt<'_>) -> Reply {
        script_with_bearer(steps.into_iter().map(|(url, limit, reply)| (url, limit, false, reply)).collect())
    }

    fn script_with_bearer(steps: Vec<(String, usize, bool, Reply)>) -> impl Fn(ReleaseRequest<'_>, usize, &Attempt<'_>) -> Reply {
        let steps = RefCell::new(VecDeque::from(steps));
        move |request, limit, _| {
            let (expected, budget, authenticated, result) = steps.borrow_mut().pop_front().expect("an unexpected request was made");
            assert_eq!(request.url().to_string(), expected);
            assert_eq!(limit, budget);
            assert_eq!(request.bearer().is_some(), authenticated, "wrong authorization for the request");
            assert_eq!(matches!(request, ReleaseRequest::Metadata { .. }), expected.starts_with("https://api.github.com/"));
            assert!(request.bearer().is_none_or(|token| token.expose() == FIXTURE_BEARER));
            result
        }
    }

    fn limited_reply() -> Reply {
        response(429, &[("retry-after", "30")], b"untrusted first response")
    }

    fn original_limit() -> ReleaseError {
        ReleaseError { message: "GitHub has limited update requests. Try again later.".into(), retry_at: Some(NOW + 30) }
    }

    fn metadata_retry(url: &str, first: Reply, retry: Reply) -> impl Fn(ReleaseRequest<'_>, usize, &Attempt<'_>) -> Reply {
        script_with_bearer(vec![(url.into(), MAX_METADATA, false, first), (url.into(), MAX_METADATA, true, retry)])
    }

    fn check_reply(reply: Reply, installed: &str) -> Result<Option<Offer>, ReleaseError> {
        check(&script(vec![(API.to_string(), MAX_METADATA, reply)]), installed.parse().unwrap(), NOW, &AtomicBool::new(false))
    }

    fn listing(value: &Value, installed: &str) -> Result<Option<Offer>, ReleaseError> {
        check_reply(response(200, &[], &serde_json::to_vec(value).unwrap()), installed)
    }

    fn offer() -> Offer {
        listing(&metadata(), "1.9.9").unwrap().unwrap()
    }

    #[test]
    fn latest_metadata_retries_a_rate_limit() {
        for (status, headers) in
            [(429, vec![]), (403, vec![("x-ratelimit-remaining", "0")]), (403, vec![("retry-after", "30")]), (403, vec![("x-ratelimit-reset", "12000")])]
        {
            let lookups = AtomicUsize::new(0);
            let tokens = Lookup(|| {
                lookups.fetch_add(1, Ordering::Relaxed);
                fixture_bearer()
            });
            let retry = response(200, &[], &serde_json::to_vec(&metadata()).unwrap());
            let scripted = metadata_retry(API, response(status, &headers, b""), retry);
            let get = |request: ReleaseRequest<'_>, limit, work: &Attempt<'_>| {
                assert_eq!(lookups.load(Ordering::Relaxed), usize::from(request.bearer().is_some()));
                scripted(request, limit, work)
            };
            let found = check_at(&get, &tokens, "1.9.9".parse().unwrap(), &AtomicBool::new(false), &Instant::now, &|| NOW).unwrap().unwrap();
            assert_eq!(found.version.to_string(), "1.10.0");
            assert_eq!(found.assets, offer().assets);
        }
    }

    #[test]
    fn installed_tag_retry_does_not_authorize_downloads_or_their_redirects() {
        let get = script_with_bearer(vec![
            (TAG_API.into(), MAX_METADATA, false, limited_reply()),
            (TAG_API.into(), MAX_METADATA, true, response(200, &[], &serde_json::to_vec(&old_metadata()).unwrap())),
            (format!("{BASE}/tiny-skill-sync.exe"), TRAY.len(), false, response(302, &[("location", CDN)], b"")),
            (CDN.into(), TRAY.len(), false, response(200, &[], TRAY)),
            (format!("{BASE}/tss.exe"), CLI.len(), false, response(200, &[], CLI)),
        ]);
        let installed = "1.9.9".parse().unwrap();
        let target = offer();
        let cancel = AtomicBool::new(false);
        let old = originals_at(&get, &Lookup(fixture_bearer), &target, installed, &cancel, &Instant::now, &|| NOW).unwrap();
        assert_eq!(old.version, installed);
        assert_eq!(old.assets, target.assets);
        let pair = fetch(&get, &target, NOW, &cancel).unwrap();
        assert!(pair.matches(&target));
    }

    #[test]
    fn only_a_rate_limit_prompts_token_lookup() {
        let installed = "1.9.9".parse().unwrap();
        let target = offer(); // Successful anonymous metadata uses a panicking provider, too.
        let cancel = AtomicBool::new(false);
        for (status, headers, retry_at) in [
            (404, vec![], None),
            (403, vec![], None),
            (403, vec![("x-ratelimit-remaining", "5"), ("x-ratelimit-reset", "12000")], None),
            (503, vec![("retry-after", "40")], Some(NOW + 40)),
            (302, vec![("location", TAG_API)], None),
        ] {
            let get = script(vec![(API.into(), MAX_METADATA, response(status, &headers, b""))]);
            let expected = ReleaseError { message: format!("The update server returned HTTP {status}."), retry_at };
            assert_eq!(check(&get, installed, NOW, &cancel).unwrap_err(), expected);
            let get = script(vec![(TAG_API.into(), MAX_METADATA, response(status, &headers, b""))]);
            assert_eq!(originals_at(&get, &NoLookup, &target, installed, &cancel, &Instant::now, &|| NOW).unwrap_err(), expected);
        }
    }

    #[test]
    fn missing_tokens_and_lookup_errors_keep_the_anonymous_error() {
        for detail in ["No fixture token was found.", "A provider failed with private details."] {
            let lookups = AtomicUsize::new(0);
            let tokens = Lookup(|| {
                lookups.fetch_add(1, Ordering::Relaxed);
                Err(detail.into())
            });
            let get = script(vec![(API.into(), MAX_METADATA, limited_reply())]);
            let e = check_at(&get, &tokens, "1.9.9".parse().unwrap(), &AtomicBool::new(false), &Instant::now, &|| NOW).unwrap_err();
            assert_eq!(e, original_limit());
            assert_eq!(lookups.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn a_failed_authenticated_retry_keeps_the_anonymous_error_and_never_tries_again() {
        let mut bad = metadata();
        bad["assets"][0]["digest"] = Value::Null;
        for retry in [
            response(401, &[], b"untrusted response"),
            response(403, &[("x-ratelimit-remaining", "0")], b""),
            response(404, &[], b""),
            response(429, &[("retry-after", "600")], b""),
            response(503, &[("retry-after", "600")], b""),
            response(302, &[("location", TAG_API)], b""),
            Err(HttpError::Timeout),
            Err(HttpError::InsecureToken),
            response(200, &[], b"not JSON"),
            response(200, &[], &serde_json::to_vec(&bad).unwrap()),
        ] {
            let calls = Cell::new(0);
            let scripted = metadata_retry(API, limited_reply(), retry);
            let get = |request: ReleaseRequest<'_>, limit, work: &Attempt<'_>| {
                calls.set(calls.get() + 1);
                scripted(request, limit, work)
            };
            let e = check_at(&get, &Lookup(fixture_bearer), "1.9.9".parse().unwrap(), &AtomicBool::new(false), &Instant::now, &|| NOW).unwrap_err();
            assert_eq!(e, original_limit());
            assert_eq!(calls.get(), 2);
        }
    }

    #[test]
    fn latest_retry_skips_asset_checks_when_the_release_is_not_newer() {
        let mut same = metadata();
        same["assets"] = json!([]);
        let installed = "1.10.0".parse().unwrap();
        let cancel = AtomicBool::new(false);
        let get = metadata_retry(API, limited_reply(), response(200, &[], &serde_json::to_vec(&same).unwrap()));
        assert_eq!(check_at(&get, &Lookup(fixture_bearer), installed, &cancel, &Instant::now, &|| NOW), Ok(None));
    }

    #[test]
    fn latest_retry_offers_neither_a_draft_nor_a_prerelease() {
        let installed = "1.9.9".parse().unwrap();
        let cancel = AtomicBool::new(false);
        for flag in ["draft", "prerelease"] {
            let mut unstable = metadata();
            unstable[flag] = true.into();
            let get = metadata_retry(API, limited_reply(), response(200, &[], &serde_json::to_vec(&unstable).unwrap()));
            assert_eq!(check_at(&get, &Lookup(fixture_bearer), installed, &cancel, &Instant::now, &|| NOW), Ok(None), "{flag}");
        }
    }

    #[test]
    fn no_token_lookup_starts_when_cancelled_or_with_too_little_time_for_gh() {
        let start = Instant::now();
        for cancel_first in [true, false] {
            let cancel = AtomicBool::new(false);
            let elapsed = Cell::new(Duration::ZERO);
            let clock = || start + elapsed.get();
            let receipt = || {
                if cancel_first {
                    cancel.store(true, Ordering::Relaxed);
                } else {
                    elapsed.set(OPERATION_TIMEOUT - proc::GH_TIMEOUT + Duration::from_secs(1));
                }
                NOW
            };
            let get = script(vec![(API.into(), MAX_METADATA, limited_reply())]);
            assert_eq!(check_at(&get, &NoLookup, "1.9.9".parse().unwrap(), &cancel, &clock, &receipt).unwrap_err(), original_limit());
        }
    }

    #[test]
    fn token_lookup_requires_more_than_its_budget() {
        let start = Instant::now();
        let installed = "1.9.9".parse().unwrap();
        let cancel = AtomicBool::new(false);
        for (remaining, expected_retries) in [(TOKEN_LOOKUP_BUDGET, 0), (TOKEN_LOOKUP_BUDGET + Duration::from_millis(1), 1)] {
            let elapsed = Cell::new(Duration::ZERO);
            let clock = || start + elapsed.get();
            let lookups = AtomicUsize::new(0);
            let tokens = Lookup(|| {
                lookups.fetch_add(1, Ordering::Relaxed);
                fixture_bearer()
            });
            let requests = Cell::new(0);
            let retry = response(200, &[], &serde_json::to_vec(&metadata()).unwrap());
            let scripted = metadata_retry(API, limited_reply(), retry);
            let get = |request: ReleaseRequest<'_>, limit, work: &Attempt<'_>| {
                requests.set(requests.get() + 1);
                elapsed.set(OPERATION_TIMEOUT - remaining);
                scripted(request, limit, work)
            };
            let result = check_at(&get, &tokens, installed, &cancel, &clock, &|| NOW);
            assert_eq!(lookups.load(Ordering::Relaxed), expected_retries, "remaining time: {remaining:?}");
            assert_eq!(requests.get(), 1 + expected_retries);
            if expected_retries == 0 {
                assert_eq!(result, Err(original_limit()));
            } else {
                assert!(result.unwrap().is_some());
            }
        }
    }

    #[test]
    fn no_authenticated_request_starts_if_lookup_is_cancelled_or_spends_the_budget() {
        let start = Instant::now();
        for cancel_lookup in [true, false] {
            let cancel = AtomicBool::new(false);
            let expired = AtomicBool::new(false);
            let clock = || if expired.load(Ordering::Relaxed) { start + OPERATION_TIMEOUT } else { start };
            let tokens = Lookup(|| {
                if cancel_lookup {
                    cancel.store(true, Ordering::Relaxed);
                } else {
                    expired.store(true, Ordering::Relaxed);
                }
                fixture_bearer()
            });
            let get = script(vec![(API.into(), MAX_METADATA, limited_reply())]);
            assert_eq!(check_at(&get, &tokens, "1.9.9".parse().unwrap(), &cancel, &clock, &|| NOW).unwrap_err(), original_limit());
            assert!(cancel.load(Ordering::Relaxed) || expired.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn only_a_newer_numeric_stable_tag_is_offered() {
        assert_eq!(offer().version.to_string(), "1.10.0");
        let value = metadata();
        for installed in ["1.10.0", "1.11.0", "2.0.0"] {
            assert!(listing(&value, installed).unwrap().is_none());
        }
        for flag in ["draft", "prerelease"] {
            let mut value = metadata();
            value[flag] = true.into();
            assert!(listing(&value, "0.0.1").unwrap().is_none());
            value.as_object_mut().unwrap().remove(flag);
            assert!(listing(&value, "0.0.1").is_err());
        }
        for tag in [
            "1.10.0",
            "vv1.10.0",
            "V1.10.0",
            "v1.10",
            "v1.010.0",
            "v1.+10.0",
            "v1.10.0-rc.2",
            "v1.10.0+build",
            "v1.10.0 ",
            "v1.10.0.1",
            "v1.18446744073709551616.0",
        ] {
            let mut value = metadata();
            value["tag_name"] = tag.into();
            // Even otherwise consistent download addresses cannot make a malformed tag eligible.
            for a in value["assets"].as_array_mut().unwrap() {
                a["browser_download_url"] = a["browser_download_url"].as_str().unwrap().replace("v1.10.0", tag).into();
            }
            assert!(listing(&value, "0.0.1").is_err(), "{tag}");
        }
    }

    #[test]
    fn metadata_must_name_and_pin_both_standalone_files_once() {
        for (field, bad) in [
            ("name", json!("../tss.exe")),
            ("name", json!("TSS.EXE")),
            ("state", json!("starter")),
            ("size", json!(0)),
            ("size", json!(MAX_EXE + 1)),
            ("size", json!(-1)),
            ("size", json!(1.5)),
            ("digest", Value::Null),
            ("digest", json!("sha1:0000")),
            ("digest", json!("sha256:00")),
            ("digest", json!(format!("sha256:{}+f", "00".repeat(31)))),
            ("browser_download_url", json!(format!("{BASE}/tiny-skill-sync.exe"))),
            ("browser_download_url", json!("https://github.com/AngelosP/tiny-skill-sync/releases/download/v0.1.0/tss.exe")),
            ("browser_download_url", json!("https://unrelated.example/file")),
        ] {
            let mut value = metadata();
            value["assets"][0][field] = bad;
            assert!(listing(&value, "0.0.1").is_err(), "{field}");
        }
        let mut value = metadata();
        value["assets"][0].as_object_mut().unwrap().remove("digest");
        assert!(listing(&value, "0.0.1").is_err()); // SHA256SUMS.txt is still there; never use it as evidence.
        let mut value = metadata();
        let duplicate = value["assets"][0].clone();
        value["assets"].as_array_mut().unwrap().push(duplicate);
        assert!(listing(&value, "0.0.1").is_err());
    }

    #[test]
    fn a_pair_comes_from_one_listing_in_tray_cli_order_even_if_the_api_order_differs() {
        let offer = offer();
        let steps = vec![
            (format!("{BASE}/tiny-skill-sync.exe"), TRAY.len(), response(302, &[("location", CDN)], b"")),
            (CDN.into(), TRAY.len(), response(200, &[], TRAY)),
            (format!("{BASE}/tss.exe"), CLI.len(), response(200, &[], CLI)),
        ];
        let pair = fetch(&script(steps), &offer, NOW, &AtomicBool::new(false)).unwrap();
        assert_eq!(pair.version(), offer.version);
        assert_eq!(pair.file(0), (TRAY, sha256(TRAY)));
        assert_eq!(pair.file(1), (CLI, sha256(CLI)));
    }

    #[test]
    fn pending_pair_matches_only_the_same_version_and_both_pinned_files() {
        let offer = offer();
        let pair = VerifiedPair::fixture(offer.version, TRAY, CLI);
        assert!(pair.matches(&offer));
        assert!(!pair.matches(&Offer::fixture(offer.version)));
        for index in 0..2 {
            let mut changed = offer.clone();
            changed.assets[index].size += 1;
            assert!(!pair.matches(&changed));
            let mut changed = offer.clone();
            changed.assets[index].digest[0] ^= 1;
            assert!(!pair.matches(&changed));
        }
        let mut changed = offer;
        changed.version = "1.11.0".parse().unwrap();
        assert!(!pair.matches(&changed));
        changed.source_version = changed.version;
        assert!(!pair.matches(&changed));
    }

    #[test]
    fn metadata_redirects_and_both_files_spend_one_budget() {
        let start = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let clock = || start + elapsed.get();
        let calls = Cell::new(0);
        let cancel = AtomicBool::new(false);
        let get = |_: ReleaseRequest<'_>, _: usize, work: &Attempt<'_>| {
            let call = calls.get();
            calls.set(call + 1);
            assert_eq!(work.remaining()?, OPERATION_TIMEOUT - elapsed.get());
            elapsed.set(elapsed.get() + OPERATION_TIMEOUT / 5);
            match call {
                0 => response(200, &[], &serde_json::to_vec(&metadata()).unwrap()),
                1 => response(200, &[], &serde_json::to_vec(&old_metadata()).unwrap()),
                2 => response(302, &[("location", CDN)], b""),
                3 => response(200, &[], TRAY),
                4 => response(200, &[], CLI), // complete and correct bytes, but too late to return the pair
                _ => panic!("unexpected request"),
            }
        };
        let installed = "1.9.9".parse().unwrap();
        let offer = check_at(&get, &NoLookup, installed, &cancel, &clock, &|| NOW).unwrap().unwrap();
        originals_at(&get, &NoLookup, &offer, installed, &cancel, &clock, &|| NOW).unwrap();
        assert_eq!(fetch_at(&get, &offer, &cancel, &clock, &|| NOW).unwrap_err(), http_error(HttpError::Timeout));
        assert_eq!(calls.get(), 5);
        // Retrying an expired offer cannot restart the budget or send even the first asset request.
        assert_eq!(fetch_at(&get, &offer, &cancel, &clock, &|| NOW).unwrap_err(), http_error(HttpError::Timeout));
        assert_eq!(originals_at(&get, &NoLookup, &offer, installed, &cancel, &clock, &|| NOW).unwrap_err(), http_error(HttpError::Timeout));
        assert_eq!(calls.get(), 5);
    }

    #[test]
    fn originals_need_both_pinned_assets_from_the_exact_official_installed_tag() {
        let target = offer();
        let installed = "1.9.9".parse().unwrap();
        let cancel = AtomicBool::new(false);
        let load = |value: &Value| {
            let get = script(vec![(TAG_API.into(), MAX_METADATA, response(200, &[], &serde_json::to_vec(value).unwrap()))]);
            originals_at(&get, &NoLookup, &target, installed, &cancel, &Instant::now, &|| NOW)
        };
        let originals = load(&old_metadata()).unwrap();
        assert_eq!(originals.version, installed);
        assert_eq!(originals.expected(0), (TRAY.len(), sha256(TRAY)));
        assert_eq!(originals.expected(1), (CLI.len(), sha256(CLI)));
        assert!(fetch(&script(vec![]), &originals, NOW, &cancel).is_err()); // evidence is not a downgrade offer
        let mut wrong_tag = old_metadata();
        wrong_tag["tag_name"] = "v1.9.8".into();
        assert!(load(&wrong_tag).is_err());
        for flag in ["draft", "prerelease"] {
            let mut value = old_metadata();
            value[flag] = true.into();
            assert!(load(&value).is_err());
        }
        let mut missing = old_metadata();
        missing["assets"].as_array_mut().unwrap().remove(0);
        assert!(load(&missing).is_err());
        let mut unsigned = old_metadata();
        unsigned["assets"][0].as_object_mut().unwrap().remove("digest");
        assert!(load(&unsigned).is_err());
        let latest = metadata();
        let get = metadata_retry(TAG_API, limited_reply(), response(200, &[], &serde_json::to_vec(&latest).unwrap()));
        assert_eq!(originals_at(&get, &Lookup(fixture_bearer), &target, installed, &cancel, &Instant::now, &|| NOW), Err(original_limit()));
        // A no-offer check does nothing beyond the single latest-metadata request (script rejects another).
        assert!(listing(&latest, "1.10.0").unwrap().is_none());
    }

    #[test]
    fn retry_after_starts_when_the_rate_limit_response_arrives() {
        let elapsed = Cell::new(0);
        let receipt = || NOW + elapsed.get();
        let cancel = AtomicBool::new(false);
        let delayed_rate_limit = |_: ReleaseRequest<'_>, _: usize, _: &Attempt<'_>| {
            elapsed.set(elapsed.get() + 12);
            response(429, &[("retry-after", "30")], b"")
        };
        let installed = "1.9.9".parse().unwrap();
        let target = offer();
        let tokens = Lookup(no_token);
        assert_eq!(check_at(&delayed_rate_limit, &tokens, installed, &cancel, &Instant::now, &receipt).unwrap_err().retry_at, Some(NOW + 42));
        assert_eq!(originals_at(&delayed_rate_limit, &tokens, &target, installed, &cancel, &Instant::now, &receipt).unwrap_err().retry_at, Some(NOW + 54));
        assert_eq!(fetch_at(&delayed_rate_limit, &target, &cancel, &Instant::now, &receipt).unwrap_err().retry_at, Some(NOW + 66));
    }

    #[test]
    fn no_partial_pair_from_a_changed_short_long_or_partial_second_asset() {
        for reply in [
            response(200, &[], &[b'x'; CLI.len()]),
            response(200, &[], &CLI[..CLI.len() - 1]),
            response(200, &[], &[CLI, b"!"].concat()),
            response(200, &[("content-length", "999")], CLI),
            response(200, &[("content-range", "bytes 0-19/20")], CLI),
            response(206, &[], CLI),
            Err(HttpError::Incomplete),
        ] {
            let steps = vec![(format!("{BASE}/tiny-skill-sync.exe"), TRAY.len(), response(200, &[], TRAY)), (format!("{BASE}/tss.exe"), CLI.len(), reply)];
            assert!(fetch(&script(steps), &offer(), NOW, &AtomicBool::new(false)).is_err());
        }
        let mut changed = offer();
        changed.version = "2.0.0".parse().unwrap();
        assert!(fetch(&script(vec![]), &changed, NOW, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn allowlisted_redirects_only_for_assets_bounded_and_never_reported() {
        let e = check_reply(response(302, &[("location", CDN)], b""), "1.0.0").unwrap_err();
        assert!(!e.message.contains("do-not-show"));
        let first = format!("{BASE}/tiny-skill-sync.exe");
        for location in [
            "http://release-assets.githubusercontent.com/file",
            "https://release-assets.githubusercontent.com.example/file",
            "https://github.com/other/file",
            "https://release-assets.githubusercontent.com@evil.example/file",
            "https://user@release-assets.githubusercontent.com/file",
            "https://release-assets.githubusercontent.com:444/file",
            "//objects.githubusercontent.com/file",
            "/relative",
            "https://objects.githubusercontent.com//another",
            "https://objects.githubusercontent.com/file#frag",
            "https://objects.githubusercontent.com/a\\b",
            "https://objects.githubusercontent.com/a\r\nInjected: x",
            "https://objects.githubusercontent.com/a%0d%0ab",
            "https://objects.githubusercontent.com/incomplete%",
        ] {
            let url = format!("{location}?secret=do-not-show");
            let get = script(vec![(first.clone(), TRAY.len(), response(302, &[("location", &url)], b""))]);
            let e = fetch(&get, &offer(), NOW, &AtomicBool::new(false)).unwrap_err();
            assert!(!e.message.contains("do-not-show"));
        }
        for bad in [response(302, &[("location", CDN), ("location", CDN)], b""), response(302, &[], b""), Err(HttpError::BadUrl(CDN.into()))] {
            let e = fetch(&script(vec![(first.clone(), TRAY.len(), bad)]), &offer(), NOW, &AtomicBool::new(false)).unwrap_err();
            assert!(!e.message.contains("do-not-show"));
        }
        let get = script(
            std::iter::once(first)
                .chain(std::iter::repeat_n(CDN.to_string(), MAX_REDIRECTS))
                .map(|url| (url, TRAY.len(), response(307, &[("location", CDN)], b"")))
                .collect(),
        );
        assert!(fetch(&get, &offer(), NOW, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn incomplete_or_oversized_metadata_is_an_error_not_no_update() {
        let bytes = serde_json::to_vec(&metadata()).unwrap();
        for reply in [
            response(200, &[], &bytes[..bytes.len() - 1]),
            response(200, &[("content-length", &(bytes.len() + 1).to_string())], &bytes),
            response(200, &[("content-length", "2"), ("content-length", "3")], &bytes),
            response(200, &[], &[&bytes[..], &vec![b' '; MAX_METADATA]].concat()),
            Err(HttpError::Timeout),
        ] {
            assert!(check_reply(reply, "1.9.9").is_err());
        }
    }

    #[test]
    fn observes_cancellation_before_during_and_between_files() {
        let cancel = AtomicBool::new(true);
        assert!(check(&script(vec![]), "1.0.0".parse().unwrap(), NOW, &cancel).is_err());
        let cancel = AtomicBool::new(false);
        let calls = Cell::new(0);
        let stop = |_: ReleaseRequest<'_>, _: usize, work: &Attempt<'_>| {
            calls.set(calls.get() + 1);
            work.cancel.store(true, Ordering::Relaxed);
            response(200, &[], TRAY)
        };
        assert!(fetch(&stop, &offer(), NOW, &cancel).unwrap_err().message.contains("cancelled"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn rate_limits_use_valid_retry_after_or_reset_and_do_not_echo_the_server() {
        for (status, headers, until) in [
            (429, vec![("retry-after", "45"), ("x-ratelimit-reset", "99999")], Some(NOW + 45)),
            (429, vec![("retry-after", "Sun, 06 Nov 1994 08:49:37 GMT")], Some(784_111_777)),
            (429, vec![("retry-after", "Sunday, 06-Nov-94 08:49:37 GMT")], Some(784_111_777)),
            (429, vec![("retry-after", "Sun Nov  6 08:49:37 1994")], Some(784_111_777)),
            (429, vec![("retry-after", "Sun, 06 Nov 1994 08:49:37 GMT trailing"), ("x-ratelimit-reset", "20000")], Some(20000)),
            (403, vec![("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "12000")], Some(12000)),
            (403, vec![("x-ratelimit-remaining", "11"), ("x-ratelimit-reset", "12000")], None),
            (429, vec![("retry-after", "-2"), ("x-ratelimit-reset", "20000")], Some(20000)),
            (429, vec![("retry-after", "9223372036854775807"), ("x-ratelimit-reset", "20000")], Some(20000)),
            (429, vec![("retry-after", "invalid"), ("x-ratelimit-reset", "200")], None),
        ] {
            let mut reply = response(status, &headers, b"").unwrap();
            reply.0.error_body = CDN.as_bytes().to_vec();
            let get = script(vec![(API.into(), MAX_METADATA, Ok(reply))]);
            let e = check_at(&get, &Lookup(no_token), "0.0.1".parse().unwrap(), &AtomicBool::new(false), &Instant::now, &|| NOW).unwrap_err();
            assert_eq!(e.retry_at, until);
            assert!(!e.message.contains("do-not-show"));
        }
        let get = script(vec![(format!("{BASE}/tiny-skill-sync.exe"), TRAY.len(), response(429, &[("retry-after", "30")], b""))]);
        assert_eq!(fetch(&get, &offer(), NOW, &AtomicBool::new(false)).unwrap_err().retry_at, Some(NOW + 30));
    }

    #[test]
    fn sha256_matches_known_vectors_across_padding_and_multiple_input_blocks() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(hex(&sha256(&vec![b'a'; 1_000_000])), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn reader_hash_handles_short_interrupted_reads_and_reports_failure() {
        struct Pieces<'a> {
            input: &'a [u8],
            interrupt: bool,
            fail_at_end: bool,
        }
        impl Read for Pieces<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.interrupt = !self.interrupt;
                if self.interrupt {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                if self.input.is_empty() && self.fail_at_end {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                let n = buf.len().min(73);
                self.input.read(&mut buf[..n])
            }
        }
        let input: Vec<u8> = (0..100_001).map(|i| (i % 251) as u8).collect();
        assert_eq!(sha256_reader(Pieces { input: &input, interrupt: false, fail_at_end: false }).unwrap(), sha256(&input));
        assert_eq!(sha256_reader(Pieces { input: &input, interrupt: false, fail_at_end: true }).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}
