//! Serde shapes mirroring `opengrok-server src/jev/routes.rs`. Marked "verify against that file
//! before consuming" (plan T9 gap). Field names are Jev's own (`camelCase`).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One question, tagged by the kind of answer it declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AskedQuestion {
    /// Yes or no, answered as a probability.
    Noul {
        /// Answer key.
        name: String,
        /// Question text.
        instructions: String,
        /// What a yes would mean.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        yes_means: Option<String>,
        /// What a no would mean.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        no_means: Option<String>,
    },
    /// One label out of several.
    Choice {
        /// Answer key.
        name: String,
        /// Question text.
        instructions: String,
        /// Offered options.
        choices: Vec<AskedChoice>,
    },
    /// A rung on a rubric, given in order.
    Score {
        /// Answer key.
        name: String,
        /// Question text.
        instructions: String,
        /// Rubric levels, lowest first.
        levels: Vec<String>,
    },
}

/// A label, or a label with what it means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AskedChoice {
    /// Bare label.
    Bare(String),
    /// Label plus a description.
    Described {
        /// The label Jev returns.
        label: String,
        /// What that label means.
        means: String,
    },
}

impl AskedChoice {
    /// The label string.
    pub fn label(&self) -> &str {
        match self {
            Self::Bare(s) | Self::Described { label: s, .. } => s,
        }
    }
}

/// Body of `POST /jev/ask`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskRequest {
    /// What Jev judges.
    pub state: Value,
    /// Optional model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Questions, in order.
    pub questions: Vec<AskedQuestion>,
}

/// Token accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpentTokens {
    /// Input tokens, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    /// Output tokens, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
}

/// Successful reply body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    /// The model that answered.
    pub model: String,
    /// Correlation id, when the service sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Token spend.
    pub usage: SpentTokens,
    /// Answers in the order the questions were asked.
    pub answers: Vec<AnsweredQuestion>,
}

/// One answered question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AnsweredQuestion {
    /// Yes/no.
    Noul {
        /// Answer key.
        name: String,
        /// Reading.
        yes: bool,
        /// Confidence of the chosen side.
        confidence: f64,
        /// `yes` / `no` probabilities.
        probabilities: Map<String, Value>,
    },
    /// One of the offered labels.
    Choice {
        /// Answer key.
        name: String,
        /// Chosen label.
        choice: String,
        /// Confidence.
        confidence: f64,
        /// Per-label probabilities.
        probabilities: Map<String, Value>,
    },
    /// A rubric rung.
    Score {
        /// Answer key.
        name: String,
        /// Numeric rung.
        score: f64,
        /// Rubric text for the rung, out of the legend; `null` when the legend has no entry.
        #[serde(default)]
        level: Option<Value>,
        /// Confidence.
        confidence: f64,
        /// Rubric legend.
        legend: Map<String, Value>,
        /// Per-rung probabilities.
        probabilities: Map<String, Value>,
    },
}
