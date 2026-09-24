//! Human-legible identifiers.
//!
//! Identifiers are rendered exactly as they are written in `SPEC.md` §2.1 and round-trip
//! through serde as plain strings. Numeric suffixes are allocated by a monotonic counter held
//! in project state; they are never reused and never renumbered.

use crate::error::TmError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// The kind of object an identifier names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdKind {
    /// A work ticket (`T-`).
    Ticket,
    /// A verification node (`V-`).
    Verification,
    /// An audit node (`A-`).
    Audit,
    /// A milestone (`M-`).
    Milestone,
    /// A decision (`D-`).
    Decision,
    /// An artifact (`ART-`).
    Artifact,
    /// A session (`S-`).
    Session,
    /// A lease (`L-`).
    Lease,
    /// A participant (`agent:…` / `human:…`).
    Participant,
}

impl IdKind {
    /// The rendered prefix, including the separator where one exists.
    pub fn prefix(self) -> &'static str {
        match self {
            IdKind::Ticket => "T-",
            IdKind::Verification => "V-",
            IdKind::Audit => "A-",
            IdKind::Milestone => "M-",
            IdKind::Decision => "D-",
            IdKind::Artifact => "ART-",
            IdKind::Session => "S-",
            IdKind::Lease => "L-",
            IdKind::Participant => "",
        }
    }

    /// The counter name used to allocate numeric suffixes for this kind.
    pub fn counter(self) -> &'static str {
        match self {
            IdKind::Ticket => "ticket",
            IdKind::Verification => "verification",
            IdKind::Audit => "audit",
            IdKind::Milestone => "milestone",
            IdKind::Decision => "decision",
            IdKind::Artifact => "artifact",
            IdKind::Session => "session",
            IdKind::Lease => "lease",
            IdKind::Participant => "participant",
        }
    }

    /// Zero-padding width used when rendering a numeric suffix.
    pub fn pad(self) -> usize {
        match self {
            IdKind::Decision => 3,
            _ => 1,
        }
    }
}

fn valid_numeric(s: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| {
        s.strip_prefix(p)
            .map(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false)
    })
}

fn valid_hex(s: &str, prefix: &str, len: usize) -> bool {
    s.strip_prefix(prefix)
        .map(|rest| {
            rest.len() == len
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
        .unwrap_or(false)
}

macro_rules! id_newtype {
    ($name:ident, $doc:literal, $validate:expr, $shape:literal, $friendly_name:literal) => {
        #[doc = $doc]
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validate and construct.
            pub fn new(s: impl Into<String>) -> Result<Self, TmError> {
                let s = s.into();
                let ok: fn(&str) -> bool = $validate;
                if ok(&s) {
                    Ok($name(s))
                } else {
                    Err(TmError::parse(format!(
                        "{} must look like {} (e.g. {})",
                        $friendly_name,
                        $shape,
                        $shape.split(',').next().unwrap_or($shape)
                    )))
                }
            }

            /// The rendered form.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume into the rendered form.
            pub fn into_string(self) -> String {
                self.0
            }

            /// The numeric suffix, when this identifier has one.
            pub fn number(&self) -> Option<u64> {
                let digits: String = self.0.chars().skip_while(|c| !c.is_ascii_digit()).collect();
                digits.parse().ok()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl FromStr for $name {
            type Err = TmError;
            fn from_str(s: &str) -> Result<Self, TmError> {
                $name::new(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                $name::new(s).map_err(serde::de::Error::custom)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

id_newtype!(
    TicketId,
    "A ticket identifier. Work tickets are `T-<n>`; verification nodes are `V-<n>`; audit nodes are `A-<n>`.",
    |s| valid_numeric(s, &["T-", "V-", "A-"]),
    "T-<n>, V-<n> or A-<n>",
    "ticket ID"
);
id_newtype!(
    MilestoneId,
    "A milestone identifier, `M-<n>`.",
    |s| valid_numeric(s, &["M-"]),
    "M-<n>",
    "milestone ID"
);
id_newtype!(
    DecisionId,
    "A decision identifier, `D-<n>`.",
    |s| valid_numeric(s, &["D-"]),
    "D-<n>",
    "decision ID"
);
id_newtype!(
    SessionId,
    "A session identifier, `S-<n>`.",
    |s| valid_numeric(s, &["S-"]),
    "S-<n>",
    "session ID"
);
id_newtype!(
    ArtifactId,
    "An artifact identifier, `ART-<hex12>`.",
    |s| valid_hex(s, "ART-", 12),
    "ART-<12 lowercase hex digits>",
    "artifact ID"
);
id_newtype!(
    LeaseId,
    "A lease identifier, `L-<hex12>`.",
    |s| valid_hex(s, "L-", 12),
    "L-<12 lowercase hex digits>",
    "lease ID"
);
id_newtype!(
    ParticipantId,
    "A participant identifier: `agent:<provider>/<id>` or `human:<handle>`.",
    |s| {
        if let Some(rest) = s.strip_prefix("agent:") {
            let mut parts = rest.splitn(2, '/');
            matches!((parts.next(), parts.next()), (Some(p), Some(i)) if !p.is_empty() && !i.is_empty())
        } else if let Some(rest) = s.strip_prefix("human:") {
            !rest.is_empty()
        } else {
            s == "system"
        }
    },
    "agent:<provider>/<id>, human:<handle> or system",
    "participant ID"
);

impl TicketId {
    /// The kind implied by this ticket identifier's prefix.
    pub fn kind(&self) -> IdKind {
        match self.0.as_bytes()[0] {
            b'V' => IdKind::Verification,
            b'A' => IdKind::Audit,
            _ => IdKind::Ticket,
        }
    }
}

impl ParticipantId {
    /// The system actor, used for events produced by deterministic machinery.
    pub fn system() -> Self {
        ParticipantId("system".to_string())
    }

    /// True when this participant is a model-backed agent.
    pub fn is_agent(&self) -> bool {
        self.0.starts_with("agent:")
    }

    /// True when this participant is a human.
    pub fn is_human(&self) -> bool {
        self.0.starts_with("human:")
    }
}

/// A heterogeneous reference to any identified object, used for event subjects.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(String);

impl Id {
    /// Wrap an already-rendered identifier.
    pub fn new(s: impl Into<String>) -> Self {
        Id(s.into())
    }

    /// The empty subject, used by events that do not name an object.
    pub fn none() -> Self {
        Id(String::new())
    }

    /// The rendered form.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True when this is the empty subject.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The kind this identifier's prefix implies, if it is recognizable.
    pub fn kind(&self) -> Option<IdKind> {
        let s = &self.0;
        if valid_numeric(s, &["T-"]) {
            Some(IdKind::Ticket)
        } else if valid_numeric(s, &["V-"]) {
            Some(IdKind::Verification)
        } else if valid_numeric(s, &["A-"]) {
            Some(IdKind::Audit)
        } else if valid_numeric(s, &["M-"]) {
            Some(IdKind::Milestone)
        } else if valid_numeric(s, &["D-"]) {
            Some(IdKind::Decision)
        } else if valid_numeric(s, &["S-"]) {
            Some(IdKind::Session)
        } else if valid_hex(s, "ART-", 12) {
            Some(IdKind::Artifact)
        } else if valid_hex(s, "L-", 12) {
            Some(IdKind::Lease)
        } else if s.starts_with("agent:") || s.starts_with("human:") {
            Some(IdKind::Participant)
        } else {
            None
        }
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! into_id {
    ($($t:ty),*) => {$(
        impl From<&$t> for Id { fn from(v: &$t) -> Id { Id(v.0.clone()) } }
        impl From<$t> for Id { fn from(v: $t) -> Id { Id(v.0) } }
    )*};
}
into_id!(
    TicketId,
    MilestoneId,
    DecisionId,
    SessionId,
    ArtifactId,
    LeaseId,
    ParticipantId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_ids_accept_the_three_node_prefixes() {
        for s in ["T-1", "V-51", "A-19", "T-184"] {
            assert_eq!(TicketId::new(s).unwrap().as_str(), s);
        }
        for s in ["T-", "X-1", "T1", "t-1", "T-1a", ""] {
            assert!(TicketId::new(s).is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn ticket_id_kind_follows_prefix() {
        assert_eq!(TicketId::new("T-1").unwrap().kind(), IdKind::Ticket);
        assert_eq!(TicketId::new("V-1").unwrap().kind(), IdKind::Verification);
        assert_eq!(TicketId::new("A-1").unwrap().kind(), IdKind::Audit);
    }

    #[test]
    fn hex_ids_require_exact_lowercase_width() {
        assert!(ArtifactId::new("ART-9f2a1c0b77de").is_ok());
        assert!(ArtifactId::new("ART-9F2A1C0B77DE").is_err());
        assert!(ArtifactId::new("ART-9f2a1c0b77d").is_err());
        assert!(LeaseId::new("L-0123456789ab").is_ok());
    }

    #[test]
    fn participants_have_three_shapes() {
        assert!(ParticipantId::new("agent:claude/a81").unwrap().is_agent());
        assert!(ParticipantId::new("human:allie").unwrap().is_human());
        assert_eq!(ParticipantId::system().as_str(), "system");
        assert!(ParticipantId::new("agent:claude").is_err());
        assert!(ParticipantId::new("agent:/a81").is_err());
        assert!(ParticipantId::new("nobody").is_err());
    }

    #[test]
    fn numbers_are_extractable() {
        assert_eq!(TicketId::new("T-184").unwrap().number(), Some(184));
        assert_eq!(DecisionId::new("D-019").unwrap().number(), Some(19));
    }

    #[test]
    fn ids_serialize_as_bare_strings() {
        let t = TicketId::new("T-7").unwrap();
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"T-7\"");
        assert_eq!(serde_json::from_str::<TicketId>("\"T-7\"").unwrap(), t);
        assert!(serde_json::from_str::<TicketId>("\"nope\"").is_err());
    }

    #[test]
    fn generic_id_recognizes_kinds() {
        assert_eq!(Id::new("M-3").kind(), Some(IdKind::Milestone));
        assert_eq!(Id::new("ART-000000000000").kind(), Some(IdKind::Artifact));
        assert_eq!(Id::new("whatever").kind(), None);
        assert!(Id::none().is_empty());
    }
}
