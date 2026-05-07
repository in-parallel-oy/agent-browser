//! Synthetic animated cursor overlay for screen recordings.
//!
//! The OS cursor is never visible in CDP `Page.captureScreenshot` output
//! (which is how `record start` produces video frames). This module
//! installs a small, isolated, in-page cursor that is captured into the
//! recording instead. It is opt-in, recording-scoped, and has no effect
//! on click semantics or page state when the bridge is not invoked.
//!
//! Implementation lives in two siblings:
//!
//! - `scripts` — the page-side controller (`cursor.js`) plus theme SVGs
//!   compiled into the binary, and a builder that templates them into a
//!   single install script.
//! - `controller` — Rust-side install/remove/move/click bridges that talk
//!   to CDP. All bridge calls are best-effort; transient context-destroyed
//!   errors are swallowed as `Ok(())` so the cursor never blocks the real
//!   click pipeline.
//!
//! See `docs/plans/2026-05-07-001-feat-cursor-overlay-during-recording-plan.md`
//! for the full design rationale.

pub mod controller;
pub mod scripts;

// Re-exports for ergonomic access from PR 2 wiring (recording lifecycle and
// dispatch_click). Until PR 2 lands, these are the only consumers, so the
// imports are flagged as unused — silenced rather than dropped, since
// stripping them now would just churn the diff when PR 2 reintroduces them.
#[allow(unused_imports)]
pub use controller::{
    click_pulse, install, move_async, move_blocking, remove, CursorOverlayConfig,
};
#[allow(unused_imports)]
pub use scripts::{build_install_script, MotionMode, Theme};
