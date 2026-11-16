//! Reading a secret from the console without showing it.

use std::io::{self, BufRead};

use windows_sys::Win32::System::Console::{ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};

/// One line from standard input. When that is a console, what is typed is not shown; a pipe is read as it is.
pub fn read_hidden_line() -> io::Result<String> {
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut mode = 0;
    let console = unsafe { GetConsoleMode(input, &mut mode) } != 0;
    if console {
        unsafe { SetConsoleMode(input, mode & !ENABLE_ECHO_INPUT) };
    }
    let mut line = String::new();
    let result = io::stdin().lock().read_line(&mut line);
    if console {
        unsafe { SetConsoleMode(input, mode) };
        // The Enter key was not shown either, so the next output would continue the prompt's line.
        eprintln!();
    }
    result.map(|_| line)
}
