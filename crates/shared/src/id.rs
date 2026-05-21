//! Domain identifier newtypes.
//!
//! Every aggregate that downstream slices reference by id gets a
//! distinct `Uuid` newtype here. The newtype dance buys two things:
//!
//!   1. **Type safety at the boundary.** A function that takes a
//!      `ChatId` cannot be called with a `WorkspaceId`, even though
//!      both are 128-bit values under the hood. Mixing them up is a
//!      compile error, not a runtime mystery.
//!   2. **A single canonical place** for new id semantics. Adding a
//!      new aggregate means adding a newtype here, not redefining a
//!      tuple-of-uuid in every consuming crate.
//!
//! The full id family lands in this slice — `WorkspaceId`, `ChatId`,
//! `TurnId`, `BufferId`, `TerminalPaneId`, `PaneId` — even though
//! some of them only have a single consumer today. Acceptance
//! criteria for issue #27 require the family up front so later
//! slices do not have to thread a migration through their PRDs.
//!
//! Every id derives the same set: `Serialize + Deserialize + Clone +
//! Copy + Eq + Hash + Debug`. They all expose the same constructors
//! (`new_v4` for fresh ids, `from_uuid` for recovery from storage)
//! and a single accessor (`as_uuid`).

use serde::{Deserialize, Serialize};
use static_assertions::assert_impl_all;
use uuid::Uuid;

/// Defines a `Uuid` newtype with the standard derive set, the two
/// canonical constructors, and an accessor. Centralising the
/// boilerplate keeps every id behaving identically — adding a new
/// id later is a one-line invocation, not a copy-paste exercise.
///
/// The macro intentionally does not implement `Display` — surface
/// printing belongs to the layer that knows the audience (logs,
/// debug UI, telemetry), and a default `Display` just encourages
/// callers to render raw uuids in user-facing strings.
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Fresh id backed by a v4 (random) `Uuid`. Use this when
            /// minting a new aggregate; never to "look up" an
            /// existing one.
            #[must_use]
            pub fn new_v4() -> Self {
                Self(Uuid::new_v4())
            }

            /// Wrap an existing `Uuid` (e.g. one decoded from storage
            /// or received over the wire). Pairs with `as_uuid` for
            /// the round-trip.
            #[must_use]
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// Borrow the underlying `Uuid`. Returned by reference so
            /// the id stays the source of truth — callers that need
            /// an owned value can `.copied()`.
            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        assert_impl_all!($name: Send, Sync);
    };
}

define_id!(
    /// Identifies a workspace — the on-disk root that contains chats,
    /// buffers, terminal sessions, and the project the user is
    /// working in.
    WorkspaceId
);

define_id!(
    /// Identifies a chat (conversation thread) inside a workspace.
    ChatId
);

define_id!(
    /// Identifies a single turn within a chat — one user prompt + the
    /// model's response, or a single tool call cycle.
    TurnId
);

define_id!(
    /// Identifies an editor buffer (an open document) inside the home
    /// shell.
    BufferId
);

define_id!(
    /// Identifies a terminal pane — one shell session inside the
    /// terminal mode.
    TerminalPaneId
);

define_id!(
    /// Identifies a pane in the home shell layout (the generic
    /// container that hosts a buffer, terminal, or chat surface).
    PaneId
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts each id derives `new_v4`, `from_uuid`, and `as_uuid`,
    /// and that the round-trip preserves the underlying value. The
    /// macro is the single point of variation, so testing one id per
    /// invocation site is enough — a regression in the macro fails
    /// every assertion at once.
    macro_rules! assert_id_contract {
        ($ty:ident) => {{
            let fresh = $ty::new_v4();
            let copy = fresh;
            assert_eq!(fresh, copy, "{} should be Copy + Eq", stringify!($ty));

            let raw = Uuid::new_v4();
            let wrapped = $ty::from_uuid(raw);
            assert_eq!(wrapped.as_uuid(), &raw);
        }};
    }

    #[test]
    fn every_id_implements_the_contract() {
        assert_id_contract!(WorkspaceId);
        assert_id_contract!(ChatId);
        assert_id_contract!(TurnId);
        assert_id_contract!(BufferId);
        assert_id_contract!(TerminalPaneId);
        assert_id_contract!(PaneId);
    }

    /// Two fresh v4 ids should never collide. The probability of a
    /// real collision is astronomically low; if this ever fails in CI
    /// the cause is almost certainly a broken `new_v4` (e.g. a
    /// constant-returning stub merged by accident), which is exactly
    /// what we want this guard to catch.
    #[test]
    fn fresh_ids_are_distinct() {
        let a = ChatId::new_v4();
        let b = ChatId::new_v4();
        assert_ne!(a, b);
    }

    /// JSON round-trip for every id type. `#[serde(transparent)]`
    /// means the wire form is the bare uuid string; this test locks
    /// that contract so a later refactor cannot silently switch to a
    /// nested object form and break stored snapshots.
    #[test]
    fn serde_round_trip_preserves_every_id() {
        fn round_trip<T>(original: T)
        where
            T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
        {
            let json = serde_json::to_string(&original).expect("serialize");
            let decoded: T = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(original, decoded);
        }

        round_trip(WorkspaceId::new_v4());
        round_trip(ChatId::new_v4());
        round_trip(TurnId::new_v4());
        round_trip(BufferId::new_v4());
        round_trip(TerminalPaneId::new_v4());
        round_trip(PaneId::new_v4());
    }

    /// `#[serde(transparent)]` should produce a bare quoted-uuid
    /// string, not a `{"0":"..."}` wrapper. Lock the wire shape
    /// explicitly — callers persist these ids inside larger blobs and
    /// any change to the encoding breaks every existing snapshot.
    #[test]
    fn wire_form_is_a_bare_uuid_string() {
        let raw = Uuid::new_v4();
        let id = WorkspaceId::from_uuid(raw);
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{raw}\""));
    }
}
