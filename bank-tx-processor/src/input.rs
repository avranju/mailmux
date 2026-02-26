use serde::Deserialize;

/// The JSON payload written to stdin by mailmux's command processor.
/// Only fields we actually use are declared; serde ignores the rest.
#[derive(Debug, Deserialize)]
pub struct Input {
    pub event: Event,
    pub email: Option<EmailRecord>,
}

#[derive(Debug, Deserialize)]
pub struct Event {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct EmailRecord {
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub raw_message_path: String,
}
