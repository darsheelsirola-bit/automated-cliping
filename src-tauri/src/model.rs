use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Creator {
    pub id: i64,
    pub username: String,
    pub channel_url: String,
    pub enabled: bool,
    pub status: String,
    pub stream_title: Option<String>,
    pub last_live_at: Option<i64>,
    pub last_error: Option<String>,
    pub highlights: i64,
    pub clips: i64,
    pub published: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub storage_directory: String,
    pub saved_budget_gb: f64,
    pub min_clip_seconds: u32,
    pub max_clip_seconds: u32,
    pub minimum_confidence: u32,
    pub detection_threshold: u32,
    pub auto_publish: bool,
    pub monitoring_enabled: bool,
    pub start_with_windows: bool,
    pub keep_awake: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            storage_directory: String::new(),
            saved_budget_gb: 20.0,
            min_clip_seconds: 10,
            max_clip_seconds: 45,
            minimum_confidence: 75,
            detection_threshold: 60,
            auto_publish: false,
            monitoring_enabled: false,
            start_with_windows: false,
            keep_awake: false,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dashboard {
    pub creators: Vec<Creator>,
    pub settings: Settings,
    pub highlights: Vec<Highlight>,
    pub chat_activity: Vec<ChatActivity>,
    pub clips: Vec<Clip>,
    pub errors: Vec<ErrorEntry>,
    pub highlight_count: i64,
    pub queued_count: i64,
    pub published_count: i64,
    pub saved_bytes: i64,
    pub temporary_bytes: i64,
    pub available_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Highlight {
    pub id: i64,
    pub username: String,
    pub triggered_at: i64,
    pub score: f64,
    pub confidence: Option<f64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    pub id: i64,
    pub username: String,
    pub status: String,
    pub duration_ms: Option<i64>,
    pub created_at: i64,
    pub output_path: Option<String>,
    pub thumbnail_path: Option<String>,
    pub error: Option<String>,
    pub transcription_status: Option<String>,
    pub render_status: Option<String>,
    pub publishing_status: Option<String>,
    pub publishing_error: Option<String>,
    pub uploaded_bytes: Option<i64>,
    pub video_id: Option<String>,
    pub title: Option<String>,
    pub published_at: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEntry {
    pub id: i64,
    pub subsystem: String,
    pub message: String,
    pub occurred_at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatActivity {
    pub creator_id: i64,
    pub sampled_at: i64,
    pub messages: i64,
    pub unique_chatters: i64,
}
