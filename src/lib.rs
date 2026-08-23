//! jsonl-peek: single-pass health checks and sampling for JSONL files.
//!
//! No third-party dependencies. Each module is a small, self-contained piece:
//! [`json`] parses one line into a [`json::Value`], [`lines`] splits a byte
//! stream into lines without re-allocating per line, [`hist`] tracks
//! approximate quantiles in bounded memory, [`rng`] provides the seeded
//! randomness behind reservoir sampling, [`path`] selects values out of a
//! parsed document by a dotted/bracketed path string, and [`stats`] combines
//! all of it into a single-pass file profile, and [`schema`] discovers the
//! paths themselves rather than requiring the caller to name them upfront.

pub mod hist;
pub mod json;
pub mod lines;
pub mod path;
pub mod rng;
pub mod schema;
pub mod stats;

pub use path::FieldPath;
pub use schema::{Schema, SchemaOptions};
pub use stats::{Stats, StatsOptions};
