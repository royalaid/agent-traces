use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Legacy records retain absent versus null adapter fields; v1 uses an explicit projection.
pub type Session = Value;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Event {
    pub role: String,
    pub text: String,
    pub ts: Option<DateTime<Utc>>,
    pub name: Option<String>,
    pub input: Option<Value>,
    pub id: Option<String>,
    pub error: bool,
    pub model: Option<String>,
}
impl Event {
    pub fn new(role: &str, text: impl Into<String>, ts: Option<DateTime<Utc>>) -> Self {
        Self { role: role.into(), text: text.into(), ts, ..Self::default() }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: String,
    pub harness: Option<String>,
    pub store_id: Option<String>,
    pub message: String,
    pub retryable: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Coverage {
    pub harness: String,
    pub store_id: Option<String>,
    pub capability: String,
    pub status: String,
    pub reason: Option<String>,
}
