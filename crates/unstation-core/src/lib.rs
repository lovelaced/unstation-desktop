//! The Unstation **mesh engine**: an AceStream-style deadline-aware piece-picker,
//! buffer maps, segment store, and wire protocol — IO-agnostic and trait-based.
//!
//! Everything platform- or IO-specific is a trait ([`transport`], [`signaling`],
//! [`manifest::OriginOfRecord`], [`media::MediaSink`]) and all time flows through
//! [`clock::Clock`]. That discipline is what makes the deterministic simulator
//! (`tests/`) and the criterion benchmarks (`benches/`) possible.

pub mod buffermap;
pub mod chat_codec;
pub mod clock;
pub mod config;
pub mod crypto;
pub mod engine;
pub mod manifest;
pub mod media;
pub mod node;
pub mod origin_mem;
pub mod peer;
pub mod picker;
pub mod protocol;
pub mod reassembly;
pub mod signaling;
pub mod statement_store_mem;
pub mod store;
pub mod topic;
pub mod transport;
pub mod transport_mem;
pub mod types;

pub use config::{MeshConfig, Mode, PickerWeights, Role};
pub use engine::MeshEngine;
pub use types::{Cid, PeerId, SegmentId, Seq, StreamId};

use std::future::Future;
use std::pin::Pin;

/// A boxed, `Send` future — the return type of the async trait methods, so the
/// traits stay object-safe without pulling in `async-trait` or `futures`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Errors surfaced by the engine and its injected dependencies.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("segment hash mismatch")]
    HashMismatch,
    #[error("publisher signature verification failed")]
    BadSignature,
    #[error("not found")]
    NotFound,
    #[error("transport: {0}")]
    Transport(String),
    #[error("signaling: {0}")]
    Signaling(String),
    #[error("origin-of-record: {0}")]
    Origin(String),
}

pub type Result<T> = std::result::Result<T, Error>;
