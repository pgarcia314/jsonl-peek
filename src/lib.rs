//! jsonl-peek: single-pass health checks and sampling for JSONL files.
//!
//! No third-party dependencies. Each module is a small, self-contained piece:
//! [`json`] parses one line into a [`json::Value`], [`lines`] splits a byte
//! stream into lines without re-allocating per line, [`hist`] tracks
//! approximate quantiles in bounded memory, [`rng`] provides the seeded
//! randomness behind reservoir sampling, and [`path`] selects values out of a
//! parsed document by a dotted/bracketed path string.

pub mod hist;
pub mod json;
pub mod lines;
pub mod path;
pub mod rng;

pub use path::FieldPath;
