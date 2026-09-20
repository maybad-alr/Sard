//! Discord's desktop IPC transport is unavailable on mobile. Keep the command contract so
//! existing callers can safely clear/update presence without spawning a worker or connecting.

use serde::Deserialize;
use tauri::State;

use crate::db::AppState;

pub const RPC_SETTING_KEY: &str = "discord_rpc_enabled";
pub const SHOW_BOOK_KEY: &str = "discord_rpc_show_book";
pub const SHOW_POSITION_KEY: &str = "discord_rpc_show_position";
pub const SHOW_BROWSING_KEY: &str = "discord_rpc_show_browsing";

#[derive(Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ActivityKind {
    #[default]
    Reading,
    Browsing,
}

#[derive(Deserialize, Debug)]
pub struct PresenceActivity {
    #[serde(default)]
    pub kind: ActivityKind,
    pub details: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub started_at: Option<u64>,
}

pub struct PresenceManager;

impl PresenceManager {
    pub fn start() -> Self {
        Self
    }
}

pub fn shutdown(_manager: &PresenceManager) {}

#[tauri::command]
pub fn presence_update(
    activity: PresenceActivity,
    state: State<AppState>,
    presence: State<PresenceManager>,
) -> Result<(), String> {
    let _ = (activity, state, presence);
    Ok(())
}

#[tauri::command]
pub fn presence_clear(presence: State<PresenceManager>) -> Result<(), String> {
    let _ = presence;
    Ok(())
}
