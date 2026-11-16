//! `tss`: the command line. See `tiny_skill_sync::cli`.

use std::io;
use std::process::ExitCode;

use tiny_skill_sync::cli::{self, SystemHost};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let code = cli::run(&args, &SystemHost, &mut io::stdout(), &mut io::stderr());
    ExitCode::from(u8::try_from(code).unwrap_or(cli::EXIT_FAILED as u8))
}
