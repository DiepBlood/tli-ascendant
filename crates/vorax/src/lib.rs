//! Rust port of tidewarden/tli-vorax-simulator's engine, bot and advisor.
//!
//! Read-only advice: like upstream, nothing built on this crate may play the real game (no clicks, key presses
//! or other input automation acting on its advice). See AGENTS.md upstream.

// Added by Ascendant: when running `cargo clippy -- -D warnings` alone in this directory, a newer clippy reports these 4 items in the upstream code
// (the Work enum size difference in advisor.rs, the let-else in game.rs pick_by, the index loop in end_round, next in rng.rs).
// They are all style suggestions; fixing them would change the public API or the upstream code, so they are allowed here and no logic changes.
// When clippy is run from the Ascendant root, this crate is not a workspace member, so clippy does not check it anyway.
#![allow(clippy::large_enum_variant, clippy::question_mark, clippy::needless_range_loop, clippy::should_implement_trait)]

pub mod advisor;
pub mod bot;
pub mod context;
pub mod data;
pub mod effects;
pub mod game;
pub mod rng;
pub mod stack;
pub mod types;
pub mod value;

pub use data::{data, Data};
pub use types::*;
