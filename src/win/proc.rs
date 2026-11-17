//! Asking the GitHub CLI for its token. `gh` is only a token source here; it never makes the requests.

use std::io::{self, Read};
use std::os::windows::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::secret::Secret;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const GH_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs a command without a console window and returns its exit status and (at most 64 KiB of) standard output.
/// `None` means it did not finish in time and was killed.
pub fn run_capture(mut cmd: Command, timeout: Duration) -> io::Result<Option<(ExitStatus, Vec<u8>)>> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(64 * 1024).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(Some((status, reader.join().unwrap_or_default()))),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                // The reader is not joined: a grandchild that inherited the pipe could keep it open.
                return Ok(None);
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// The token from `gh auth token` output: one line of printable ASCII.
pub fn parse_token_output(out: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(out).ok()?.trim();
    let ok = !text.is_empty() && text.len() <= 512 && text.bytes().all(|b| b.is_ascii_graphic());
    ok.then(|| text.to_string())
}

/// Without `--hostname`, gh answers for whichever host it considers the default (`GH_HOST`, or the only host it is signed
/// in to), and that token would then be sent to github.com.
fn token_command() -> Command {
    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token", "--hostname", "github.com"]).env("GH_PROMPT_DISABLED", "1").env("GH_NO_UPDATE_NOTIFIER", "1").env("NO_COLOR", "1");
    cmd
}

/// The error text is plain words for the status line; it never contains the token.
pub fn gh_token() -> Result<Secret, String> {
    match run_capture(token_command(), GH_TIMEOUT) {
        Ok(Some((status, out))) if status.success() => {
            parse_token_output(&out).map(Secret::new).ok_or_else(|| "the GitHub CLI (gh) returned no token".to_string())
        }
        Ok(Some(_)) => Err("the GitHub CLI (gh) is installed but not signed in; run: gh auth login".to_string()),
        Ok(None) => Err("the GitHub CLI (gh) did not answer within 5 seconds".to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err("the GitHub CLI (gh) was not found".to_string()),
        Err(e) => Err(format!("the GitHub CLI (gh) could not be started: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_accepted_only_when_it_looks_like_a_token() {
        assert_eq!(parse_token_output(b"gho_abc123\r\n").as_deref(), Some("gho_abc123"));
        assert_eq!(parse_token_output(b""), None);
        assert_eq!(parse_token_output(b"two\nlines"), None);
        assert_eq!(parse_token_output(b"has space"), None);
        assert_eq!(parse_token_output(&[0xff, 0xfe]), None);
        assert_eq!(parse_token_output("é".as_bytes()), None);
        assert_eq!(parse_token_output("a".repeat(513).as_bytes()), None);
    }

    #[test]
    fn the_token_asked_for_is_always_the_one_for_github_com() {
        let cmd = token_command();
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["auth", "token", "--hostname", "github.com"]);
    }

    #[test]
    fn a_command_that_finishes_returns_its_output() {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "echo", "hello"]);
        let (status, out) = run_capture(cmd, Duration::from_secs(10)).unwrap().unwrap();
        assert!(status.success());
        assert_eq!(String::from_utf8_lossy(&out).trim(), "hello");
    }

    #[test]
    fn a_command_that_hangs_is_killed_at_the_deadline() {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "ping -n 30 127.0.0.1 >nul"]);
        let start = Instant::now();
        assert!(run_capture(cmd, Duration::from_millis(300)).unwrap().is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_not_found() {
        let err = run_capture(Command::new("tss-no-such-program-xyz"), Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
