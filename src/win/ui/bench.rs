//! Timing marks for `scripts\bench.ps1`. Off unless the environment variable `TSS_BENCH_REPORT` names a file;
//! then every mark appends one line to it, for example `mark=tray at_ms=21.84 cpu_ms=15.6`:
//!
//! * `at_ms`: milliseconds since Windows created the process,
//! * `cpu_ms`: CPU time the process used so far (user and kernel),
//! * `took_ms`: how long the step took, for steps with a length.
//!
//! When it is off, a mark costs one look at a value read once.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use crate::app::Job;

pub const REPORT_VAR: &str = "TSS_BENCH_REPORT";

fn report() -> Option<&'static PathBuf> {
    static REPORT: OnceLock<Option<PathBuf>> = OnceLock::new();
    REPORT.get_or_init(|| std::env::var_os(REPORT_VAR).filter(|v| !v.is_empty()).map(PathBuf::from)).as_ref()
}

pub fn on() -> bool {
    report().is_some()
}

pub fn mark(label: &str, took: Option<Duration>) {
    let Some(file) = report() else { return };
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user, mut now) = (zero, zero, zero, zero, zero);
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user);
        GetSystemTimePreciseAsFileTime(&mut now);
    }
    let ticks = |ft: FILETIME| (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    let mut line = format!("mark={label} at_ms={} cpu_ms={}", ms(ticks(now).saturating_sub(ticks(created))), ms(ticks(kernel) + ticks(user)));
    if let Some(took) = took {
        line += &format!(" took_ms={}", ms((took.as_nanos() / 100) as u64));
    }
    line.push_str("\r\n");
    // Measuring must never get in the way: a file that cannot be written loses the mark, nothing else.
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(file) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Milliseconds with two decimals, from 100-nanosecond ticks. Integers only: formatting a float adds 10 KiB to the exe.
fn ms(ticks: u64) -> String {
    format!("{}.{:02}", ticks / 10_000, ticks % 10_000 / 100)
}

/// A one-word name for a job in a mark.
pub fn job_label(job: &Job) -> &'static str {
    match job {
        Job::LoadSettings => "job-settings",
        Job::Startup => "job-startup",
        Job::Look => "job-look",
        Job::Poll => "job-poll",
        Job::Check(_) => "job-check",
        Job::PreviewSource(_) => "job-source-preview",
        Job::SaveSource(..) => "job-source-save",
        Job::AppUpdate(_) | Job::ScheduledUpdate => "job-app-update",
        Job::InstallOnQuit { .. } => "job-quit-install",
        Job::Resolve { .. } => "job-resolve",
        Job::DeleteGone(_) => "job-delete-gone",
        Job::Remove { .. } => "job-remove",
        Job::Diff { .. } => "job-diff",
        Job::Flush => "job-flush",
        Job::Plan => "job-plan",
    }
}

#[cfg(test)]
mod tests {
    use super::ms;

    #[test]
    fn milliseconds_keep_two_decimals() {
        assert_eq!([ms(0), ms(10_500), ms(1_234_567)], ["0.00", "1.05", "123.45"]);
    }
}
