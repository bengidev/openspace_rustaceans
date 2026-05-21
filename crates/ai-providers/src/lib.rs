//! AI provider adapters and supporting infrastructure for OpenSpace.
//!
//! The Domain crate (`openspace-shared`) declares the AI vocabulary —
//! [`openspace_shared::ai::provider::AiProvider`], the streaming event
//! surface, and the [`openspace_shared::ai::secret::SecretStore`]
//! trait. This crate hosts the *runtime-side* implementations: the
//! HTTP clients, the OS-keychain backend, the wire-format adapters
//! that turn a neutral [`openspace_shared::ai::domain::Conversation`]
//! into bytes on the wire and back.
//!
//! # Slice scope
//!
//! The current slice (issue #43) ships only the crate foundation:
//!
//! - The crate compiles green and is wired into the workspace.
//! - `InMemorySecretStore` lives behind the `test-support` feature,
//!   ready for downstream test setups to construct an `Arc<dyn
//!   SecretStore>` without touching the OS keychain. The type only
//!   exists when the feature is enabled, so this listing intentionally
//!   uses a bare reference rather than an intra-doc link — `cargo doc`
//!   without `--features test-support` would fail to resolve a link
//!   to a feature-gated item.
//!
//! Later slices add the OS-keychain backend, the `ToolCallAccumulator`
//! deep module, and the concrete provider adapters.
//!
//! # Why the in-memory store lives behind a feature flag
//!
//! Test doubles do not belong in release artefacts — a production
//! binary that accidentally read credentials from a process-local
//! `BTreeMap` would silently lose them on restart. Gating the fixture
//! behind `--features test-support` makes the dependency direction
//! explicit at the manifest level: only crates that opt in compile the
//! fixture, and `cargo build -p openspace-ai-providers` (no features)
//! never touches it.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod accumulator;

pub use accumulator::{AccumulatorError, ToolCall, ToolCallAccumulator};

#[cfg(feature = "test-support")]
mod in_memory_secret_store;

#[cfg(feature = "test-support")]
pub use in_memory_secret_store::InMemorySecretStore;
