//! The `MediaSink` trait — hands ordered bytes to the platform player.
//!
//! On desktop this is fed by a localhost HLS re-server; `on_play_head` lets the
//! player report `play_seq` back so the picker's zones track real playback.

use bytes::Bytes;

pub trait MediaSink: Send + Sync {
    fn push_init(&self, bytes: Bytes);
    fn push_segment(&self, seq: u64, bytes: Bytes);
    fn on_play_head(&self) -> u64;
}
