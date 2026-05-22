//! Adapter for the industry-standard `/v1/chat/completions` wire
//! format.
//!
//! The module is split into five files so each concern stays
//! readable on its own:
//!
//! - [`provider`] — the [`OpenAiCompatibleProvider`] struct, the
//!   [`AiProvider`] impl, and the streaming pump.
//! - [`wire`] — request and response wire-format types.
//! - [`sse`] — the Server-Sent Events line parser.
//! - [`error_map`] — HTTP status → categorised
//!   [`openspace_shared::ai::error::AiError`] mapping plus the
//!   `Retry-After` parser.
//! - [`catalogue`] — `/models` row → [`ModelInfo`] conversion plus
//!   the dynamic / heuristic / fallback capability detection chain.
//!
//! Only [`OpenAiCompatibleProvider`] is re-exported from the crate
//! root; the supporting modules are private. See the crate-level
//! docs for the wider public surface.
//!
//! [`AiProvider`]: openspace_shared::ai::provider::AiProvider
//! [`ModelInfo`]: openspace_shared::ai::domain::ModelInfo

mod catalogue;
mod error_map;
mod provider;
mod request;
mod sse;
mod wire;

pub use provider::OpenAiCompatibleProvider;
