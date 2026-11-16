#![windows_subsystem = "windows"]
//! Spike S3 baseline: a GUI-subsystem Rust exe that does nothing.
//! Its startup time is the floor that process creation, the Rust runtime and the loader cost,
//! so `s3_tray` can be measured against it.

fn main() {}
