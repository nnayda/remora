//! Identifier slugs shared across the wire protocol.
//!
//! Ids are lower-case slugs matching `[a-z0-9-]+` (ADR-0004). The underscore
//! is reserved as the separator in tmux session names
//! (`remora_<project-id>_<session-id>`), so a valid id can never break that
//! parse. Ids are never interpolated into shell strings; validation here is
//! defense in depth, not the only line.

use serde::{Deserialize, Serialize};

/// Error returned when a string is not a valid Remora id slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidIdError {
    value: String,
}

impl InvalidIdError {
    /// The rejected input.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl std::fmt::Display for InvalidIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid id `{}`: must be a non-empty lower-case slug of [a-z0-9-]",
            self.value
        )
    }
}

impl std::error::Error for InvalidIdError {}

fn validate_slug(value: &str) -> Result<(), InvalidIdError> {
    let valid = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(InvalidIdError {
            value: value.to_string(),
        })
    }
}

macro_rules! slug_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps `value`.
            ///
            /// Valid ids are non-empty lower-case slugs of `[a-z0-9-]`
            /// (ADR-0004); anything else is rejected, including on
            /// deserialization.
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdError> {
                let value = value.into();
                validate_slug(&value)?;
                Ok(Self(value))
            }

            /// The id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = InvalidIdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidIdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }
    };
}

slug_id!(
    /// Identifies one session within a project: a workspace (git worktree,
    /// or the project directory in shared mode) that is live while its named
    /// tmux session exists and stopped when only the worktree survives.
    /// Minted client-side at spawn (ADR-0004).
    SessionId
);

slug_id!(
    /// Identifies a project in local config — the stable join key between
    /// configuration and sessions discovered on a host (ADR-0004).
    ProjectId
);

slug_id!(
    /// Names a per-agent adapter entry in local config (ADR-0003). The
    /// protocol carries only this opaque id; launch commands and prompt
    /// heuristics stay in configuration.
    AgentId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_lower_case_slugs() {
        for ok in ["a", "fix-login", "a1-b2", "0", "feature-x"] {
            let id = SessionId::new(ok).expect("valid slug");
            assert_eq!(id.as_str(), ok);
        }
    }

    #[test]
    fn rejects_invalid_slugs() {
        for bad in ["", "Fix", "fix_login", "fix login", "fix.login", "héllo"] {
            assert!(SessionId::new(bad).is_err(), "should reject {bad:?}");
            assert!(bad.parse::<SessionId>().is_err());
        }
    }

    #[test]
    fn all_id_types_validate() {
        assert!(ProjectId::new("api").is_ok());
        assert!(ProjectId::new("API").is_err());
        assert!(AgentId::new("claude").is_ok());
        assert!(AgentId::new("claude_code").is_err());
    }

    #[test]
    fn round_trips_through_json_as_plain_string() {
        let id = SessionId::new("remora-feature-x").expect("valid slug");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, r#""remora-feature-x""#);
        let back: SessionId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn deserialization_rejects_invalid_ids() {
        for bad_json in [r#""""#, r#""Fix""#, r#""fix_login""#] {
            assert!(serde_json::from_str::<SessionId>(bad_json).is_err());
        }
    }

    #[test]
    fn display_and_string_conversion() {
        let id = ProjectId::new("api").expect("valid slug");
        assert_eq!(id.to_string(), "api");
        assert_eq!(String::from(id), "api");
    }

    #[test]
    fn invalid_id_error_names_the_offender() {
        let err = SessionId::new("Fix_Login").expect_err("invalid slug");
        assert!(err.to_string().contains("Fix_Login"));
        let _: &dyn std::error::Error = &err;
    }
}
