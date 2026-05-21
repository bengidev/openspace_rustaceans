//! Home shell crate.
//!
//! Composes the three first-class modes — terminal, chat, editor — into
//! a single workspace surface. Each mode lives in its own sub-feature
//! crate under `sub-features/` and is wired up here.

pub mod placeholder {}

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {}
}
