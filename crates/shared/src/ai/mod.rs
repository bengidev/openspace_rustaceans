//! AI provider Domain.
//!
//! Houses the conversation shape, the provider trait surface, and the
//! AI-specific error enum. Each lives in its own submodule so a
//! consumer that only needs the data model does not pay for the
//! provider trait, and vice versa.
//!
//! Populated in later PRD-01 slices (issues #28+). Module declarations
//! land here so downstream slices land as additive-only changes.

pub mod domain;
pub mod error;
pub mod provider;
pub mod secret;
