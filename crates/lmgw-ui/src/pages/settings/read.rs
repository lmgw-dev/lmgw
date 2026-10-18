//! Reading `GET /api/settings-full` for the page.

use lmgw_api_types::SettingsFull;
use serde_json::Value;

/// `GET /api/settings-full`, read as the document's `SettingsFull` (an answer
/// of another shape is an error shown on the page) and handed to the form as
/// the keyed tree its descriptors address by pointer: what the server sent.
pub(super) async fn read_settings() -> Result<Value, String> {
    let full = crate::api::get::<SettingsFull>("/api/settings-full")
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(&full).map_err(|e| e.to_string())
}
