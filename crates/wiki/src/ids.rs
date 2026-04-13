use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{Result, WikiError};

/// Stores one canonical wiki entry identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(String);

impl EntryId {
    /// Validates and builds one canonical wiki entry identifier.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        validate_prefixed_id(value.into(), "cw_").map(Self)
    }

    /// Borrows the canonical wiki entry identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for EntryId {
    type Err = WikiError;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

/// Stores one canonical digest report identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DigestId(String);

impl DigestId {
    /// Validates and builds one canonical digest report identifier.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        validate_prefixed_id(value.into(), "dg_").map(Self)
    }

    /// Borrows the canonical digest report identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DigestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for DigestId {
    type Err = WikiError;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

/// Stores one canonical digest run identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Validates and builds one canonical digest run identifier.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        validate_prefixed_id(value.into(), "cwrun_").map(Self)
    }

    /// Borrows the canonical digest run identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RunId {
    type Err = WikiError;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

/// Validates one prefixed ASCII identifier against the canonical wiki naming rule.
fn validate_prefixed_id(value: String, prefix: &str) -> Result<String> {
    let is_valid = value.starts_with(prefix)
        && value.as_bytes().iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'-'
        });
    if is_valid {
        Ok(value)
    } else {
        Err(WikiError::InvalidId { value })
    }
}
