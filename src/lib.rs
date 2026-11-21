//! tiny-skill-sync keeps user-level Copilot skills in sync with GitHub repositories.
//!
//! The logic is split so that the risky decisions are pure functions that are easy to test; the Windows
//! specific code lives under `win`.

pub mod app;
pub mod apply;
pub mod autostart;
pub mod board;
pub mod cli;
pub mod config;
pub mod diff;
pub mod engine;
pub mod fsx;
pub mod github;
pub mod hash;
pub mod interval;
pub mod logo;
pub mod pathsafe;
pub mod plugins;
pub mod portable;
pub mod reconcile;
pub mod release;
pub mod replacement;
pub mod scheduler;
pub mod secret;
pub mod source;
pub mod state;
pub mod text;
pub mod tree;
pub mod update_clock;
pub mod updater;
pub mod verify;
pub mod win;
