//! Shared library for OpenSpace.
//!
//! Houses cross-crate primitives — domain types, error kinds, telemetry
//! helpers — that more than one feature crate needs. Keep additions here
//! deliberate; if only one crate uses a type, it does not belong here.

/// Placeholder module to keep the crate non-empty until real shared
/// primitives land. Remove once the first real type moves in.
pub mod placeholder {}

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {}
}
