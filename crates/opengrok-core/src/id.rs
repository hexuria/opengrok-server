//! Ids that cannot be confused with one another.
//!
//! Newtypes rather than `String` because every seam in this server takes several ids, and the
//! compiler is the only reviewer that never gets bored of checking which is which.

use std::fmt;

macro_rules! id {
    ($name:ident, $prefix:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Mint a fresh id, prefixed so a value seen in a log says what it identifies.
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, uuid::Uuid::now_v7()))
            }

            /// Take an id that already exists — from the wire, or from the database.
            pub fn from_stored(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id!(
    CoworkerId,
    "cw",
    "One coworker: a name, a face, a computer, a model route."
);
id!(RunId, "run", "One turn taken by one coworker.");
id!(BoxId, "box", "The computer a coworker works on.");
id!(
    PrincipalId,
    "pr",
    "Whoever is asking: a person, another coworker, or an anonymous visitor."
);
id!(
    TranscriptEntryId,
    "e",
    "One entry in a transcript, as the client knows it."
);
id!(
    AccountId,
    "acct",
    "A person's account with us — what the client's Cursor sign-in resolves to."
);
id!(
    SessionId,
    "sess",
    "One signed-in session: an access/refresh token pair the client holds."
);
