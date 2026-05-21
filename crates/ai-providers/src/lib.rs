//! AI provider adapters and supporting infrastructure for OpenSpace.
//!
//! The Domain crate (`openspace-shared`) declares the AI vocabulary —
//! [`openspace_shared::ai::provider::AiProvider`], the streaming event
//! surface, the [`openspace_shared::ai::secret::SecretStore`] trait,
//! and the [`openspace_shared::sandbox::SandboxPolicy`] that gates
//! every side-effecting call. This crate hosts the *runtime-side*
//! implementations: the sandbox-aware HTTP client every adapter
//! talks through, the secret-store backends, the streaming tool-call
//! accumulator, and (in later slices) the wire-format adapters that
//! turn a neutral [`openspace_shared::ai::domain::Conversation`] into
//! bytes on the wire and back.
//!
//! # Public surface (this slice)
//!
//! - [`SandboxedHttpClient`] — the only HTTP entry point. Wraps a
//!   shared `reqwest::Client`, a
//!   [`openspace_shared::sandbox::SandboxPolicy`], and an
//!   [`openspace_shared::tool::ApprovalChannel`]. Adapters obtain
//!   one via [`SandboxedHttpClient::new`] and call
//!   `get` / `post_json` / `post_streaming` against it.
//! - [`HttpClientError`] — typed error every HTTP entry point
//!   returns. Carries `SandboxBlocked`, `SandboxDenied`,
//!   `ApprovalTimeout`, transport errors, and common precondition
//!   failures.
//! - [`ToolCallAccumulator`] / [`ToolCall`] / [`AccumulatorError`] —
//!   streaming reassembly of provider tool-call deltas into a single
//!   structurally-valid JSON object.
//! - `InMemorySecretStore` — feature-gated test double for the
//!   `SecretStore` trait. Only compiled under
//!   `--features test-support`. The listing intentionally uses a
//!   bare reference rather than an intra-doc link — `cargo doc`
//!   without `--features test-support` would fail to resolve a
//!   link to a feature-gated item, and the workspace doc job runs
//!   without that feature.
//!
//! # Cancellation contract
//!
//! Dropping the in-flight response future closes the underlying
//! connection. The
//! `dropping_response_closes_connection_within_one_second`
//! integration test pins the behaviour: a local stub server observes
//! a connection termination within one second of the future being
//! dropped. Adapters therefore implement cancellation simply by
//! dropping the streaming response (or the future they `await`) —
//! no extra plumbing required.
//!
//! # Why test doubles live behind a feature flag
//!
//! Test doubles do not belong in release artefacts — a production
//! binary that accidentally read credentials from a process-local
//! `BTreeMap` would silently lose them on restart, and an
//! always-approve sandbox channel would silently waive every
//! confirmation. Gating the fixtures behind `--features test-support`
//! makes the dependency direction explicit at the manifest level:
//! only crates that opt in compile the fixtures, and
//! `cargo build -p openspace-ai-providers` (no features) never
//! touches them.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]

pub mod accumulator;

mod client;
mod error;

#[cfg(feature = "test-support")]
mod in_memory_secret_store;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use accumulator::{AccumulatorError, ToolCall, ToolCallAccumulator};
pub use client::{SandboxedHttpClient, DEFAULT_APPROVAL_TIMEOUT};
pub use error::HttpClientError;

#[cfg(feature = "test-support")]
pub use in_memory_secret_store::InMemorySecretStore;
