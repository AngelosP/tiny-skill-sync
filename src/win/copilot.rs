//! Runs only the supported Copilot marketplace commands. The only Copilot file read here
//! is settings.json, for the origin proof that the CLI's marketplace list does not include.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use super::proc::{self, CaptureOptions, Captured};
use crate::config::{RepoSpec, check_plugin_branch_chars};
use crate::plugins::native::{ManagerError, NativeMarket, NativePlugin, PluginManager, parse_market_origins, parse_markets, parse_plugins};
use crate::plugins::normal_name;
use crate::source::check_root;

const ISOLATION: &[&str] = &["TSS_HOME", "TSS_SKILLS_DIR", "TSS_TEST_ROOT", "TSS_AGENTS_DIR", "TSS_INSTRUCTIONS_DIR"];
const HOME: &str = "COPILOT_HOME";
const CACHE: &str = "COPILOT_CACHE_HOME";
const PROBE_SECS: u64 = 30;
const CHANGE_SECS: u64 = 180;
const MAX_SETTINGS: u64 = 4 * 1024 * 1024;

type Launch = fn(&[&str], u64, &AtomicBool) -> Result<Captured, ManagerError>;

pub(crate) struct Copilot(Launch);

fn env(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

fn starts_with(path: &Path, prefix: &Path) -> bool {
    let mut parts = path.components();
    prefix.components().all(|c| parts.next().is_some_and(|p| super::fs::same_win_name(&c.as_os_str().to_string_lossy(), &p.as_os_str().to_string_lossy())))
}

fn isolation_requested(env: &dyn Fn(&str) -> Option<OsString>) -> bool {
    ISOLATION.iter().any(|key| env(key).is_some())
}

fn isolated(env: &dyn Fn(&str) -> Option<OsString>) -> bool {
    if !isolation_requested(env) {
        return true;
    }
    let path = |key| env(key).map(PathBuf::from).filter(|p| check_root(p).is_ok());
    // A drive path with long components is the only spelling we can compare here. UNC shares,
    // device paths, and short aliases can name the same folders without matching their names.
    let target = |key| {
        path(key).filter(|p| {
            matches!(p.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_)))
                && !p.as_os_str().as_encoded_bytes().contains(&b'~')
        })
    };
    let (Some(profile), Some(local), Some(home), Some(cache)) = (path("USERPROFILE"), path("LOCALAPPDATA"), target(HOME), target(CACHE)) else {
        return false;
    };
    // This is a path-name check: no probing, following links or creating directories.
    let protected = [profile.join(".copilot"), local.join("copilot")];
    !starts_with(&home, &cache)
        && !starts_with(&cache, &home)
        && [home, cache].iter().all(|p| protected.iter().all(|real| !starts_with(p, real) && !starts_with(real, p)))
}

fn find_exe(env: &dyn Fn(&str) -> Option<OsString>, cwd: Option<&Path>, is_file: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let valid = |p: &PathBuf| p.is_absolute() && p.extension().is_some_and(|s| s.eq_ignore_ascii_case("exe")) && is_file(p);
    if let Some(p) = env("TSS_COPILOT_CLI") {
        let p = PathBuf::from(p);
        return valid(&p).then_some(p);
    }
    if let Some(installed) = env("LOCALAPPDATA").map(|p| PathBuf::from(p).join(r"GitHubCopilotCLI\copilot.exe")).filter(valid) {
        return Some(installed);
    }
    let cwd = cwd?;
    let path = env("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| check_root(dir).is_ok() && !(starts_with(dir, cwd) && starts_with(cwd, dir)))
        .map(|dir| dir.join("copilot.exe"))
        .find(valid)
}

fn executable() -> Result<PathBuf, ManagerError> {
    find_exe(&env, std::env::current_dir().ok().as_deref(), &Path::is_file).ok_or(ManagerError::NotFound)
}

/// Preview may check file existence, not query the native CLI or its settings.
pub(crate) fn available() -> bool {
    executable().is_ok()
}

#[inline(never)]
fn first_error(stderr: &[u8], stdout: &[u8]) -> ManagerError {
    let err = String::from_utf8_lossy(stderr);
    let out = String::from_utf8_lossy(stdout);
    let line = err.lines().chain(out.lines()).map(str::trim).find(|line| line.chars().any(|ch| !ch.is_control())).unwrap_or_default();
    // Keep the existing fail-closed handling for GitHub tokens, then mask other likely secrets before clipping.
    if ["github_pat_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_"].iter().any(|key| line.contains(key)) {
        return ManagerError::Failed("GitHub Copilot CLI returned an error containing a credential.".into());
    }
    let line = mask_encoded(&mask_labelled(line));
    ManagerError::Failed(line.chars().take(240).collect())
}

fn mask_labelled(line: &str) -> String {
    let mut out = String::new();
    let mut copied = 0;
    for (i, ch) in line.char_indices() {
        if i < copied {
            continue;
        }
        let tail = &line[i..];
        let span = if let Some(url) = tail.strip_prefix("://") {
            let authority = url.split(|ch: char| ch.is_whitespace() || ch.is_control() || "/?#\"<>".contains(ch)).next().unwrap_or_default();
            // Look for @ only in the authority, never in the path or query.
            authority.rfind('@').filter(|&at| at > 0).map(|at| (3, 3 + at))
        } else if ch.is_ascii_alphabetic() && (i == 0 || !word_byte(line.as_bytes()[i - 1])) {
            labelled_secret(tail)
        } else {
            None
        };
        if let Some((start, end)) = span {
            out.push_str(&line[copied..i + start]);
            out.push_str("***");
            copied = i + end;
        }
    }
    out.push_str(&line[copied..]);
    out
}

fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn labelled_secret(tail: &str) -> Option<(usize, usize)> {
    for label in ["authorization:", "bearer", "access_token=", "token=", "password="] {
        if !tail.as_bytes().get(..label.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(label.as_bytes())) {
            continue;
        }
        let rest = &tail[label.len()..];
        let value = rest.trim_start_matches(char::is_whitespace);
        if value.is_empty() || (label == "bearer" && value.len() == rest.len()) {
            continue;
        }
        let start = tail.len() - value.len();
        let len = if label == "authorization:" { value.len() } else { secret_value_len(value) };
        return (len != 0).then_some((start, start + len));
    }
    None
}

fn secret_value_len(value: &str) -> usize {
    if let Some(&quote @ (b'\'' | b'"')) = value.as_bytes().first() {
        let mut escaped = false;
        for (i, b) in value.bytes().enumerate().skip(1) {
            if !escaped && b == quote {
                return i + 1;
            }
            escaped = !escaped && b == b'\\';
        }
        return value.len();
    }
    value.find(|ch: char| ch.is_whitespace() || ch.is_control() || "&#;,)]}\"'".contains(ch)).unwrap_or(value.len())
}

fn mask_encoded(line: &str) -> String {
    const MIN_LEN: usize = 40;
    let encoded = |b: u8| word_byte(b) || matches!(b, b'+' | b'/' | b'-');
    let mut out = String::new();
    let mut copied = 0;
    let mut scanned = 0;
    for (i, b) in line.bytes().enumerate() {
        if i < scanned || !encoded(b) {
            continue;
        }
        let tail = &line.as_bytes()[i..];
        let len = tail.iter().take_while(|&&b| encoded(b)).count();
        let padding = tail[len..].iter().take(2).take_while(|&&b| b == b'=').count();
        scanned = i + len + padding;
        if len + padding < MIN_LEN {
            continue;
        }
        let run = &tail[..len];
        // A path may mix '/' with '-' or '_'; neither base64 alphabet does. Keep it only
        // if no part could still be a long encoded value in either alphabet.
        if padding == 0
            && run.contains(&b'/')
            && run.split(|&b| matches!(b, b'-' | b'_')).all(|part| part.len() < MIN_LEN)
            && run.split(|&b| matches!(b, b'/' | b'+')).all(|part| part.len() < MIN_LEN)
        {
            continue;
        }
        out.push_str(&line[copied..i]);
        out.push_str("***");
        copied = scanned;
    }
    out.push_str(&line[copied..]);
    out
}

fn launch(args: &[&str], secs: u64, cancel: &AtomicBool) -> Result<Captured, ManagerError> {
    launch_with(&env, executable, proc::capture, args, secs, cancel)
}

fn launch_with(
    env: &dyn Fn(&str) -> Option<OsString>,
    executable: impl FnOnce() -> Result<PathBuf, ManagerError>,
    capture: impl FnOnce(&Path, &[&str], CaptureOptions<'_>, Duration, &AtomicBool) -> io::Result<Captured>,
    args: &[&str],
    secs: u64,
    cancel: &AtomicBool,
) -> Result<Captured, ManagerError> {
    if !isolated(env) {
        return Err(ManagerError::RefusedUnderIsolation);
    }
    let exe = executable()?;
    let dir = exe.parent().ok_or(ManagerError::NotFound)?;
    // Do not let a checkout that invoked tss influence Copilot's project config or implicit exe lookup.
    let overrides = [("NO_COLOR", "1"), proc::NO_CWD_EXE_LOOKUP, ("COPILOT_AUTO_UPDATE", "false")];
    let options = CaptureOptions { dir, env: if isolation_requested(env) { &overrides } else { &overrides[..2] }, below_normal: true };
    let output = capture(&exe, args, options, Duration::from_secs(secs), cancel).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => ManagerError::NotFound,
        io::ErrorKind::Interrupted => ManagerError::Cancelled,
        io::ErrorKind::TimedOut => ManagerError::TimedOut,
        _ => ManagerError::Failed(format!("Could not run GitHub Copilot CLI: {e}")),
    })?;
    if output.code == 0 { Ok(output) } else { Err(first_error(&output.stderr, &output.stdout)) }
}

impl Copilot {
    pub(crate) fn new() -> Self {
        Self(launch)
    }

    fn listing(&self, args: &[&str], cancel: &AtomicBool) -> Result<Vec<u8>, ManagerError> {
        let output = (self.0)(args, PROBE_SECS, cancel)?;
        // A clipped prefix is not a complete list, even if it happens to parse.
        if output.stdout_cut { Err(ManagerError::BadOutput) } else { Ok(output.stdout) }
    }

    fn change(&self, verb: &str, qualified: &str, secs: u64, cancel: &AtomicBool) -> Result<(), ManagerError> {
        if !qualified.split_once('@').is_some_and(|(name, market)| normal_name(name) && normal_name(market)) {
            return Err(ManagerError::Failed("The plugin name is not in Copilot's normal form.".into()));
        }
        (self.0)(&["plugin", verb, qualified], secs, cancel).map(drop)
    }
}

impl PluginManager for Copilot {
    fn home(&self) -> Result<PathBuf, ManagerError> {
        let path = env(HOME).map(PathBuf::from).or_else(|| env("USERPROFILE").map(|p| PathBuf::from(p).join(".copilot")));
        path.filter(|p| check_root(p).is_ok()).ok_or_else(|| ManagerError::Failed("Copilot home must be a full folder path.".into()))
    }

    fn list(&self, cancel: &AtomicBool) -> Result<Vec<NativePlugin>, ManagerError> {
        parse_plugins(&self.listing(&["plugin", "list", "--json"], cancel)?)
    }

    fn markets(&self, cancel: &AtomicBool) -> Result<Vec<NativeMarket>, ManagerError> {
        let mut markets = parse_markets(&self.listing(&["plugin", "marketplace", "list", "--json"], cancel)?)?;
        if markets.iter().any(|market| !market.is_default) {
            let mut bytes = Vec::new();
            let home = self.home()?;
            if File::open(home.join("settings.json")).and_then(|f| f.take(MAX_SETTINGS + 1).read_to_end(&mut bytes)).is_ok()
                && bytes.len() as u64 <= MAX_SETTINGS
            {
                if let Ok(mut origins) = parse_market_origins(&bytes) {
                    for market in &mut markets {
                        market.origin = origins.take(&market.name);
                    }
                }
            }
        }
        Ok(markets)
    }

    fn add_market(&self, repo: &str, branch: &str, cancel: &AtomicBool) -> Result<(), ManagerError> {
        // The CLI must see a GitHub shorthand, never an option, file path, URL, or different ref.
        check_plugin_branch_chars(branch).map_err(|why| ManagerError::Failed(why.into()))?;
        if repo.starts_with('-')
            || !RepoSpec::parse(repo).is_ok_and(|s| s.full_name() == repo)
            || branch.is_empty()
            || branch == "HEAD"
            || crate::config::check_branch(branch).is_err()
        {
            return Err(ManagerError::Failed("The marketplace needs a repository and a named branch.".into()));
        }
        (self.0)(&["plugin", "marketplace", "add", &format!("{repo}#{branch}")], CHANGE_SECS, cancel).map(drop)
    }

    fn install(&self, qualified: &str, cancel: &AtomicBool) -> Result<(), ManagerError> {
        self.change("install", qualified, CHANGE_SECS, cancel)
    }

    fn update(&self, qualified: &str, cancel: &AtomicBool) -> Result<(), ManagerError> {
        self.change("update", qualified, CHANGE_SECS, cancel)
    }

    fn uninstall(&self, qualified: &str, cancel: &AtomicBool) -> Result<(), ManagerError> {
        self.change("uninstall", qualified, 60, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r"C:\Users\me";
    const LOCAL: &str = r"C:\Users\me\AppData\Local";

    fn isolated_homes(key: &str, home: Option<&str>, cache: Option<&str>) -> bool {
        isolated(&|name| match name {
            "USERPROFILE" => Some(PROFILE.into()),
            "LOCALAPPDATA" => Some(LOCAL.into()),
            HOME => home.map(Into::into),
            CACHE => cache.map(Into::into),
            name if name == key => Some("".into()),
            _ => None,
        })
    }

    #[test]
    fn copilot_isolation_refuses_either_real_home_and_requires_two_absolute_homes() {
        let safe = Some(r"D:\sandbox\home");
        let cache = Some(r"D:\sandbox\cache");
        for key in ["TSS_HOME", "TSS_SKILLS_DIR", "TSS_TEST_ROOT", "TSS_AGENTS_DIR", "TSS_INSTRUCTIONS_DIR"] {
            assert!(isolated_homes(key, safe, cache), "both isolated for {key}");
            for bad in [
                None,
                Some(""),
                Some(r"D:relative"),
                Some(r"\profile"),
                Some(PROFILE),
                Some(LOCAL),
                Some(r"c:/users/ME/./.COPILOT/skills"),
                Some(r"C:\Users\me\.Copilot"),
                Some(r"C:\Users\me\AppData\Local\Copilot\cache"),
                Some(r"\\?\C:\Users\me\.copilot"),
                Some(r"C:\Users\me\.copilot.\cache"),
                Some(r"C:\Users\me\safe\..\.copilot"),
            ] {
                assert!(!isolated_homes(key, bad, cache), "unsafe home {bad:?} for {key}");
                assert!(!isolated_homes(key, safe, bad), "unsafe cache {bad:?} for {key}");
            }
        }
        assert!(isolated_homes("no isolation", None, None));
        assert!(isolated_homes("TSS_HOME", Some(r"C:\Users\me\.copilot-tests"), cache));
        assert!(!isolated(&|k| (k != "USERPROFILE").then(|| r"D:\sandbox".into())));
    }

    #[test]
    fn copilot_isolation_rejects_unc_device_short_and_relative_paths() {
        let key = "TSS_TEST_ROOT";
        let home = Some(r"D:\sandbox\home");
        let cache = Some(r"D:\sandbox\cache");
        for bad in [
            r"\\localhost\C$\Users\me\.copilot",
            r"\\fileserver\share\cache",
            r"\\?\D:\sandbox\home",
            r"\\.\D:\sandbox\home",
            r"C:\Users\me\COPILO~1\cache",
            r"D:\LONGNA~1\sandbox\cache",
            r"D:home",
            r"\sandbox\home",
            r"sandbox\home",
        ] {
            assert!(!isolated_homes(key, Some(bad), cache), "unsafe home {bad:?}");
            assert!(!isolated_homes(key, home, Some(bad)), "unsafe cache {bad:?}");
        }
        assert!(isolated_homes(key, home, cache));
    }

    #[test]
    fn copilot_isolation_refuses_equal_and_nested_homes() {
        let home = r"D:\sandbox\home";
        let nested = r"D:\sandbox\home\cache";
        let overlapping = [(home, home), (home, r"d:/SANDBOX/./HOME/"), (home, nested), (nested, home)];
        assert_eq!(overlapping.map(|(home, cache)| isolated_homes("TSS_TEST_ROOT", Some(home), Some(cache))), [false; 4]);
        assert!(isolated_homes("TSS_TEST_ROOT", Some(home), Some(r"D:\sandbox\home-cache")));
    }

    #[test]
    fn copilot_lookup_honors_the_override_and_ignores_relative_and_current_path_entries() {
        let local = r"C:\Profile\Local";
        let installed = r"C:\Profile\Local\GitHubCopilotCLI\copilot.exe";
        let path = r".;relative;C:relative;c:\HERE\.;C:\Here\child\..;D:\first;E:\second";
        let cwd = Path::new(r"C:\Here");
        let lookup = |override_path: Option<&str>, known_exists| {
            find_exe(
                &|k| match k {
                    "TSS_COPILOT_CLI" => override_path.map(Into::into),
                    "LOCALAPPDATA" => Some(local.into()),
                    "PATH" => Some(path.into()),
                    _ => None,
                },
                Some(cwd),
                &|p| p != Path::new(installed) || known_exists,
            )
        };
        assert_eq!(lookup(None, true), Some(PathBuf::from(installed)));
        assert_eq!(lookup(None, false), Some(PathBuf::from(r"D:\first\copilot.exe")));
        assert_eq!(lookup(Some(r"F:\package\chosen.EXE"), true), Some(PathBuf::from(r"F:\package\chosen.EXE")));
        for refused in ["", "copilot.exe", r"F:relative.exe", r"F:\package\chosen.cmd"] {
            assert_eq!(lookup(Some(refused), true), None, "no fallback for a bad explicit override");
        }
        assert!(find_exe(&|k| (k == "TSS_COPILOT_CLI").then(|| r"F:\missing.exe".into()), Some(cwd), &|_| false).is_none());
    }

    #[test]
    fn copilot_launch_refuses_a_missing_isolated_home_before_lookup() {
        let result = launch_with(
            &|key| match key {
                "TSS_TEST_ROOT" => Some(r"D:\sandbox".into()),
                "USERPROFILE" => Some(r"C:\Users\me".into()),
                "LOCALAPPDATA" => Some(r"C:\Users\me\AppData\Local".into()),
                CACHE => Some(r"D:\sandbox\cache".into()),
                _ => None,
            },
            || panic!("the isolation guard must run before executable lookup"),
            |_, _, _, _, _| panic!("must not launch under isolation"),
            &["plugin", "list", "--json"],
            PROBE_SECS,
            &AtomicBool::new(false),
        );
        assert_eq!(result.err(), Some(ManagerError::RefusedUnderIsolation));
    }

    #[test]
    fn copilot_launch_sets_exe_folder_environment_and_lower_priority() {
        let exe = Path::new(r"D:\Copilot CLI\copilot.exe");
        let args = ["plugin", "list", "--json"];
        for isolation in [false, true] {
            launch_with(
                &|key| match key {
                    "TSS_TEST_ROOT" if isolation => Some(r"D:\sandbox".into()),
                    "USERPROFILE" => Some(PROFILE.into()),
                    "LOCALAPPDATA" => Some(LOCAL.into()),
                    HOME => Some(r"D:\sandbox\home".into()),
                    CACHE => Some(r"D:\sandbox\cache".into()),
                    "COPILOT_AUTO_UPDATE" => Some("true".into()),
                    _ => None,
                },
                || Ok(exe.into()),
                |program, actual_args, options, timeout, _| {
                    assert_eq!(program, exe);
                    assert_eq!(actual_args, args);
                    assert_eq!(options.dir, Path::new(r"D:\Copilot CLI"));
                    assert!(options.env.contains(&("NO_COLOR", "1")));
                    assert!(options.env.contains(&("NoDefaultCurrentDirectoryInExePath", "1")));
                    let update = options.env.iter().copied().find(|&(key, _)| key == "COPILOT_AUTO_UPDATE");
                    assert_eq!(update, isolation.then_some(("COPILOT_AUTO_UPDATE", "false")), "isolation: {isolation}");
                    assert!(options.below_normal);
                    assert_eq!(timeout, Duration::from_secs(30));
                    Ok(Captured { code: 0, stdout: vec![], stderr: vec![], stdout_cut: false })
                },
                &args,
                PROBE_SECS,
                &AtomicBool::new(false),
            )
            .unwrap();
        }
    }

    #[test]
    fn copilot_rejects_bad_marketplace_and_plugin_arguments_before_launch() {
        let manager = Copilot(|_, _, _| panic!("invalid arguments must not reach the launcher"));
        let cancel = AtomicBool::new(false);
        assert_eq!(manager.add_market("-x", "main", &cancel).unwrap_err().to_string(), "The marketplace needs a repository and a named branch.");
        assert_eq!(manager.install("bad", &cancel).unwrap_err().to_string(), "The plugin name is not in Copilot's normal form.");
    }

    #[test]
    fn copilot_rejects_branch_characters_before_launch() {
        let manager = Copilot(|_, _, _| panic!("an unsupported branch must not reach the launcher"));
        let cancel = AtomicBool::new(false);
        for branch in ["release@v2", "release%2Fv2", "release#v2", "release v2", "release\u{e9}", "release\"v2"] {
            assert_eq!(
                manager.add_market("owner/repo", branch, &cancel).unwrap_err().to_string(),
                "This branch name has characters the app will not pass to Copilot CLI: use letters, digits, '.', '_', '/', '+' or '-'.",
            );
        }
        let permitted = Copilot(|args, time, _| {
            assert_eq!(args, ["plugin", "marketplace", "add", "owner/repo#Release/v1.2_rc+test-3"]);
            assert_eq!(time, 180);
            Ok(Captured { code: 0, stdout: vec![], stderr: vec![], stdout_cut: false })
        });
        permitted.add_market("owner/repo", "Release/v1.2_rc+test-3", &cancel).unwrap();
    }

    #[test]
    fn copilot_maps_capture_errors() {
        for (io_kind, expected) in [
            (io::ErrorKind::TimedOut, ManagerError::TimedOut),
            (io::ErrorKind::Interrupted, ManagerError::Cancelled),
            (io::ErrorKind::NotFound, ManagerError::NotFound),
        ] {
            let result = launch_with(
                &|_| None,
                || Ok(PathBuf::from(r"D:\fake\copilot.exe")),
                |_, _, _, _, _| Err(io_kind.into()),
                &["plugin", "list", "--json"],
                PROBE_SECS,
                &AtomicBool::new(false),
            );
            assert_eq!(result.err(), Some(expected));
        }
    }

    #[test]
    fn copilot_rejects_clipped_json_even_if_the_prefix_parses() {
        let manager = Copilot(|_, _, _| Ok(Captured { code: 0, stdout: b"[]".to_vec(), stderr: vec![], stdout_cut: true }));
        let cancel = AtomicBool::new(false);
        assert_eq!(manager.list(&cancel), Err(ManagerError::BadOutput));
        assert_eq!(manager.markets(&cancel), Err(ManagerError::BadOutput));
    }

    #[test]
    fn copilot_errors_mask_url_userinfo() {
        let error = b"Could not fetch HTTPS://fake:p%40ss@example.test:443/tools or git+ssh://dummy@mirror.test. See help.";
        let expected = "Could not fetch HTTPS://***@example.test:443/tools or git+ssh://***@mirror.test. See help.";
        assert_eq!(first_error(error, b"").to_string(), expected);
        let no_userinfo = "Could not fetch https://example.test/a@b?mail=someone@host";
        assert_eq!(first_error(no_userinfo.as_bytes(), b"").to_string(), no_userinfo);
    }

    #[test]
    fn copilot_errors_mask_authorization_and_bearer() {
        assert_eq!(first_error(b"Request failed: aUtHoRiZaTiOn: Basic dummy data", b"").to_string(), "Request failed: aUtHoRiZaTiOn: ***");
        assert_eq!(first_error(b"Rejected bEaReR\tdummy.value, retry sign-in", b"").to_string(), "Rejected bEaReR\\u{9}***, retry sign-in");
    }

    #[test]
    fn copilot_errors_mask_named_values() {
        let error = br#"Unexpected token=demo&access_token=tiny and PASSWORD="not a real password"; continue with sign-in"#;
        let expected = "Unexpected token=***&access_token=*** and PASSWORD=***; continue with sign-in";
        assert_eq!(first_error(error, b"").to_string(), expected);
        let ordinary = "The tokenizer failed: mytoken=example, passwordless sign-in is available";
        assert_eq!(first_error(ordinary.as_bytes(), b"").to_string(), ordinary);
    }

    #[test]
    fn copilot_errors_mask_long_encoded_values_but_keep_words() {
        let hex = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let base64 = "eXh5ejAxMjM0NTY3ODkrL2FiY2RlZmdoaWpr+q/=";
        let url_safe = "eXh5ejAxMjM0NTY3ODkrL2FiY2RlZmdoaWpr-Q_f";
        let letters_only = "abcdefghijklmnopqrstuvwxyzabcdefghijklmn";
        for value in [hex, base64, url_safe, letters_only] {
            let line = format!("Server refused {value} when loading the marketplace");
            assert_eq!(first_error(line.as_bytes(), b"").to_string(), "Server refused *** when loading the marketplace");
        }
        let ordinary = "Could not reach the marketplace registry. Check your connection and try again.";
        assert_eq!(first_error(ordinary.as_bytes(), b"").to_string(), ordinary);
        let short = format!("Try again after {}", &hex[..39]);
        assert_eq!(first_error(short.as_bytes(), b"").to_string(), short);
    }

    #[test]
    fn copilot_errors_keep_repository_urls_and_paths() {
        let paths = [
            "https://github.com/AngelosP/tiny-skill-sync-fixtures-public.git",
            "owner/repo/path/with-many-segments_and_words",
            r"C:\Users\someone\AppData\Local\GitHubCopilotCLI\copilot.exe",
        ];
        assert_eq!(paths.map(|path| first_error(path.as_bytes(), b"").to_string()), paths);
    }

    #[test]
    fn copilot_errors_mask_long_jwt_segments() {
        let jwt = ["h0".repeat(21), "p1-_".repeat(11), "s2".repeat(21)].join(".");
        let line = format!("Server refused {jwt} when loading the marketplace");
        assert_eq!(first_error(line.as_bytes(), b"").to_string(), "Server refused ***.***.*** when loading the marketplace");
    }

    #[test]
    fn copilot_errors_mask_a_long_encoded_value_even_if_it_looks_like_part_of_a_path() {
        let standard = ["aB3D".repeat(6), "qR1S".repeat(6)].join("/");
        let url_safe = "aB3d-_".repeat(7);
        for (alphabet, value) in [("standard", standard), ("URL-safe", url_safe)] {
            let line = format!("Could not read team-tools/{value}");
            assert_eq!(first_error(line.as_bytes(), b"").to_string(), "Could not read ***", "{alphabet}");
        }
    }

    #[test]
    fn copilot_errors_cap_after_masking() {
        let suffix = "try again later, ".repeat(20);
        let line = format!("password={} {suffix}", "x".repeat(300));
        let expected = format!("password=*** {suffix}");
        assert_eq!(first_error(line.as_bytes(), b"").to_string(), expected[..240].trim_end());
    }

    #[test]
    fn copilot_nonzero_error_uses_first_printable_line_and_never_echoes_a_credential() {
        assert_eq!(first_error(b"\r\n\x00\r\n Failed here\r\nnoise", b"stdout").to_string(), "Failed here");
        assert_eq!(first_error(b"\x00", b"No marketplace\nsecond").to_string(), "No marketplace");
        let long = first_error(&vec![b'x'; 1000], b"").to_string();
        assert!(long.len() <= 240 && long.is_ascii());
        assert!(!first_error(b"Error with gho_dummy", b"").to_string().contains("gho_"));
    }
}
