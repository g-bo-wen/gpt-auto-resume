use crate::i18n::Message;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub thread_id: String,
    pub title: String,
    pub cwd: String,
    pub updated_at: String,
    pub source: String,
    pub status: String,
    pub latest: Option<TerminalEvent>,
    pub turns: Vec<TurnEvidence>,
    #[serde(default)]
    pub observed_order: u64,
    #[serde(default)]
    pub confirmation_incomplete: bool,
    #[serde(default)]
    pub confirmation_issue: Option<String>,
    #[serde(default)]
    pub last_user_order: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEvent {
    pub event_id: String,
    pub turn_id: String,
    pub order: u64,
    pub timestamp: String,
    pub kind: String,
    pub quota: Option<Quota>,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TurnEvidence {
    pub turn_id: String,
    pub order: u64,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub user_message_at: Option<String>,
    pub prompt_hash: Option<String>,
    pub executed: bool,
    pub failed: bool,
    #[serde(default)]
    pub desktop_inputs: Vec<DesktopInputEvidence>,
    #[serde(default)]
    pub has_user_input: bool,
    #[serde(default)]
    pub invalid_desktop_input: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopInputEvidence {
    pub item_id: String,
    pub order: u64,
    pub source_thread_id: String,
    pub prompt_hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    pub scope: String,
    pub bucket: String,
    pub captured_at: String,
    pub five_hour_used: f64,
    pub weekly_used: f64,
    pub reset_at: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub compatible: bool,
    pub reason: String,
    #[serde(default)]
    pub reason_message: Option<Message>,
    pub stage: String,
    pub checked_at: String,
    pub desktop_version: Option<String>,
    pub runtime_version: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    pub sessions: Vec<Session>,
    pub quota: Option<Quota>,
    /// Cached quota is display-only; it must never authorize a submission.
    #[serde(default)]
    pub quota_stale: bool,
    pub diagnostic: Diagnostic,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub default_prompt: String,
    pub poll_seconds: u64,
    pub runtime_path: String,
    pub notifications: bool,
    pub auto_start: bool,
    #[serde(default = "default_language")]
    pub language: String,
}
fn default_language() -> String {
    "system".into()
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            default_prompt: "请继续完成之前的任务。".into(),
            poll_seconds: 60,
            runtime_path: String::new(),
            notifications: true,
            auto_start: false,
            language: default_language(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchConfig {
    pub thread_id: String,
    pub prompt: String,
    pub remaining: Option<u32>,
}
#[derive(Clone, Debug)]
pub struct Submission {
    pub attempt_id: String,
    pub thread_id: String,
    pub prompt: String,
    pub source_event_id: String,
    pub baseline_order: u64,
}
#[derive(Clone, Debug)]
pub enum Delivery {
    Accepted { turn_id: String },
    Submitted,
    Rejected(String),
    Unknown(String),
}
