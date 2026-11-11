//! Tartarus Ascendant: the rules engine, the strategy and the board model (ported
//! from an earlier JS version: js/engine.js, js/solver.js and js/analyze.js),
//! the screen reader and the app.

// Map affix reading: the affix frames on the map page and the affix page (public since 1.2.0).
pub mod affix;
pub mod app;
pub mod audit;
pub mod board;
pub mod engine;
pub mod lang;
pub mod rates;
pub mod solver;
pub mod reader;
pub mod vorax;

/// mimalloc in place of the system allocator: the playouts clone the State
/// every round on every thread, and Windows' heap serialised them; solves of
/// 48 boards from real runs took 44% less, to the same bits (2026-10-31).
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;
