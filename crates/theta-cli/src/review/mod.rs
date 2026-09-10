//! The review screen.
//!
//! Scoped in `docs/SCOPE-review-tui.md`. The state machine and the rendering
//! are separate from the terminal so the rules that carry the safety claim —
//! no bulk confirm, the strongest gate costs more than a keypress, the queue is
//! triage's order — are properties a test can assert rather than behaviour
//! somebody has to drive a terminal to observe.

pub mod app;
pub mod render;
pub mod run;

#[cfg(test)]
mod tests;
