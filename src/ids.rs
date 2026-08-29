//! Validated domain identifiers shared across storage boundaries.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProjectId(String);

impl ProjectId {
    #[must_use]
    pub fn new() -> Self {
        Self(format!("prj_{}", Uuid::new_v4()))
    }

    /// Parse a prefixed or bare UUID into a canonical project ID.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is not a valid UUID with an optional `prj_` prefix.
    pub fn parse(value: &str) -> Result<Self> {
        Self::from_str(value)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProjectId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ProjectId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let uuid = value.strip_prefix("prj_").unwrap_or(value);
        Uuid::parse_str(uuid).map_err(|_| Error::InvalidProjectId(value.to_owned()))?;
        Ok(Self(format!("prj_{uuid}")))
    }
}

impl<'de> Deserialize<'de> for ProjectId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(serde::de::Error::custom)
    }
}

macro_rules! domain_id {
    ($name:ident, $prefix:literal, $kind:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(format!(concat!($prefix, "_{}"), Uuid::new_v4()))
            }

            /// Parse and canonicalize this typed identifier.
            ///
            /// # Errors
            ///
            /// Returns an error when the prefix or UUID is invalid.
            pub fn parse(value: &str) -> Result<Self> {
                value.parse()
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(value: &str) -> Result<Self> {
                let Some(uuid) = value.strip_prefix(concat!($prefix, "_")) else {
                    return Err(Error::InvalidDomainId {
                        kind: $kind,
                        value: value.to_owned(),
                    });
                };
                Uuid::parse_str(uuid).map_err(|_| Error::InvalidDomainId {
                    kind: $kind,
                    value: value.to_owned(),
                })?;
                Ok(Self(format!("{}_{}", $prefix, uuid)))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::from_str(&value).map_err(serde::de::Error::custom)
            }
        }
    };
}

domain_id!(TaskId, "tsk", "task");
domain_id!(AttemptId, "att", "attempt");
domain_id!(EvidenceId, "evd", "evidence");
domain_id!(MemoryId, "mem", "memory");
domain_id!(NodeId, "nod", "node");
domain_id!(EdgeId, "edg", "edge");
domain_id!(ContextRunId, "ctx", "context run");
domain_id!(JobId, "job", "job");
domain_id!(EvaluationRunId, "eval", "evaluation run");
