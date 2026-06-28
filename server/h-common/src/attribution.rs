//! Attribution labels and confidence for reconstructed spans.
//!
//! Process attribution is only available from attribution-capable capture
//! sources such as eBPF. Passive packet/gateway capture can still carry an
//! explicit identity when operators expose one in an HTTP header at the capture
//! point. These types keep that provenance explicit so downstream export paths
//! can filter out ambiguous training data.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionConfidence {
    High,
    Medium,
    Ambiguous,
}

impl AttributionConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Ambiguous => "ambiguous",
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::High => 3,
            Self::Medium => 2,
            Self::Ambiguous => 1,
        }
    }
}

impl fmt::Display for AttributionConfidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Default for AttributionConfidence {
    fn default() -> Self {
        Self::Ambiguous
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionInfo {
    pub label: Option<String>,
    pub source: String,
    pub confidence: AttributionConfidence,
}

impl AttributionInfo {
    pub fn ambiguous() -> Self {
        Self {
            label: None,
            source: "unknown".to_string(),
            confidence: AttributionConfidence::Ambiguous,
        }
    }

    pub fn high(label: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            label: Some(label.into()),
            source: source.into(),
            confidence: AttributionConfidence::High,
        }
    }
}

impl Default for AttributionInfo {
    fn default() -> Self {
        Self::ambiguous()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AttributionConfig {
    /// Request header names whose first non-empty value is trusted as an
    /// attribution label for passive/gateway capture. Header matching is
    /// case-insensitive and order-preserving.
    pub request_headers: Vec<String>,
}
