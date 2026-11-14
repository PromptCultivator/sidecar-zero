//! `sidecarzero`: model inference through shared memory, with a fallback that always answers.
//!
//! ```no_run
//! use sidecarzero::{Client, Endpoint, F32Echo};
//! use std::time::Duration;
//!
//! let client = Client::builder(Endpoint::parse("/run/sidecarzero/scz.sock"), F32Echo::<4>)
//!     .fallback(|x: &[f32; 4], _reason| *x)
//!     .build();
//! let out = client.infer(&[1.0, 2.0, 3.0, 4.0], Duration::from_millis(2));
//! assert_eq!(out.value, [1.0, 2.0, 3.0, 4.0]); // from the model, or from the fallback
//! ```
//!
//! The synchronous call path of sdk-contract §4 with the per-caller breaker, the background
//! thread with reconnect and safe retirement of old arenas, per-caller counters (late wake-ups
//! among them), fork handling (a forked child falls back and closes its copy of the session
//! socket), and the registry binding with Arrow frame codecs (arrow-data-plane §7, §9): strict
//! binding by schema fingerprint, rebinding when a reload changes the schema. Not yet: lenient
//! binding and the async path (v1.x). Builds on Linux only.

#![forbid(unsafe_code)]

mod client;
mod codec;
mod conn;
mod fallback;
pub mod metrics;

pub use client::{Client, ClientBuilder, ClientState, NoFallback, Options, Stats, WithFallback};
pub use codec::{
    BytesEcho, Codec, CodecError, Column, F32Echo, FeatureBatch, FeatureScore, FrameReader, FrameWriter, OutOfBounds, Schema, SlotReader, SlotWriter,
};
pub use fallback::{Fallback, FallbackReason, Outcome, Source};
pub use metrics::{Histogram, MetricsSink, PrometheusText};
pub use scz_shm::handshake::Endpoint;
