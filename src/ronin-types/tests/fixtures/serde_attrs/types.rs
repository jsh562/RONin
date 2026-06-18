// Fixture data (NOT compiled into the crate) exercising the serde attribute set
// for the SynSource serde-fidelity test (TR-005 / SC-003).

use serde::{Deserialize, Serialize};

/// rename_all = "camelCase" + per-field rename + deny_unknown_fields.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Account {
    pub first_name: String,
    pub last_name: String,
    /// per-field rename wins over rename_all.
    #[serde(rename = "ID")]
    pub user_id: u64,
    /// default makes the field optional.
    #[serde(default)]
    pub nickname: String,
    /// skip removes the field from the data model entirely.
    #[serde(skip)]
    pub cached: String,
    /// skip_serializing_if makes the field optional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    /// `with` routes through a converter whose shape syn cannot see -> unknown.
    #[serde(with = "ts_seconds")]
    pub created_at: SystemTime,
}

/// Internally-tagged enum: tag = "type" -> Discriminator::Internal.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    Login { user: String },
    Logout { user: String },
}

/// Adjacently-tagged enum: tag + content -> Discriminator::Adjacent.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum Message {
    Text(String),
    Move { x: i32, y: i32 },
}

/// Untagged enum -> Discriminator::Untagged.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Int(i64),
    Text(String),
}

/// Enum whose variant names are renamed via rename_all (variants are PascalCase).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Active,
    InProgress,
    /// per-variant rename wins.
    #[serde(rename = "done!")]
    Completed,
    /// skipped variant is removed.
    #[serde(skip)]
    Internal,
}

/// transparent struct collapses to its single inner type.
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct Wrapper {
    pub inner: InnerData,
}

/// The inner type the transparent wrapper collapses onto.
#[derive(Serialize, Deserialize)]
pub struct InnerData {
    pub a: u32,
    pub b: u32,
}

/// flatten inlines the target's fields (syn references the target; see note).
#[derive(Serialize, Deserialize)]
pub struct Envelope {
    pub id: u32,
    #[serde(flatten)]
    pub payload: InnerData,
}

/// rename_all_fields on an enum applies to every variant's struct fields.
#[derive(Serialize, Deserialize)]
#[serde(rename_all_fields = "camelCase")]
pub enum Command {
    Run { exit_code: i32, working_dir: String },
}
