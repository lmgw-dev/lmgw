//! The profile routes the editor calls (design §3.1); nothing else.

use lmgw_api_types::chat_profiles::{
    PreviewAnswer, PreviewRequest, Profile, ProfileCreate, ProfileDeleted, ProfileDraft,
    ProfileList, ProfilePatch, SpeakRequest, TestAnswer, TestRequest,
};

use crate::http::{self, Result};

const BASE: &str = "/chat/api/profiles";

pub async fn list() -> Result<ProfileList> {
    http::get(BASE).await
}

pub async fn create(body: &ProfileCreate) -> Result<Profile> {
    http::post(BASE, body).await
}

/// The built-in row again after it was deleted.
pub async fn create_builtin(key: &str) -> Result<Profile> {
    http::post(
        BASE,
        &ProfileCreate {
            builtin: Some(key.to_string()),
            ..ProfileCreate::default()
        },
    )
    .await
}

pub async fn patch(id: i64, body: &ProfilePatch) -> Result<Profile> {
    http::post(format!("{BASE}/{id}"), body).await
}

/// "Reset to built-in": the gateway makes the row follow the built-in text
/// again (the store's reset), which a patch of `null`s cannot: on a built-in
/// row that stores "unset", i.e. empty texts.
pub async fn reset(id: i64) -> Result<Profile> {
    http::post(format!("{BASE}/{id}/reset"), &serde_json::json!({})).await
}

pub async fn delete(id: i64) -> Result<ProfileDeleted> {
    http::post(format!("{BASE}/{id}/delete"), &serde_json::json!({})).await
}

pub async fn preview(
    profile: ProfileDraft,
    thread_id: Option<i64>,
    model: Option<String>,
) -> Result<PreviewAnswer> {
    let body = PreviewRequest {
        profile,
        thread_id,
        model,
    };
    http::post(format!("{BASE}/preview"), &body).await
}

pub async fn test(
    profile: ProfileDraft,
    thread_id: Option<i64>,
    model: String,
    text: String,
    voice: bool,
) -> Result<TestAnswer> {
    let body = TestRequest {
        profile,
        thread_id,
        model,
        text,
        voice,
    };
    http::post(format!("{BASE}/test"), &body).await
}

/// One WAV, as bytes with the content type the gateway sent.
pub async fn speak(
    profile: ProfileDraft,
    thread_id: Option<i64>,
    text: String,
) -> Result<(Vec<u8>, String)> {
    let body = SpeakRequest {
        profile,
        thread_id,
        text,
    };
    http::post_bytes(format!("{BASE}/speak"), &body).await
}
