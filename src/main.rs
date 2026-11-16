#![windows_subsystem = "windows"]

fn main() {
    std::process::exit(tiny_skill_sync::win::run_tray());
}
