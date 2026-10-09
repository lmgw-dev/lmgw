//! Personality profiles' prompt assembly (personality-profiles design
//! §2.1–§2.2, the reasoning half of §2.3), on real sends: goldens for each
//! row of §2.2 with and without each field, `voice_block` null / `""` /
//! text with and without a reply language, the `{{date}}` rule judged on
//! the persona, the examples' formatting, the reasoning precedence (the
//! voice default and an explicit thread field included) and Admin Chat's
//! wrapping. A deleted profile is no profile, and an edit applies from
//! the next turn (D21).
//!
//! The goldens are `tests/fixtures/chat_system/profiles.json` (today's
//! date as `<today>`); `LMGW_BLESS=1` rewrites them. The turn drivers are
//! `chat_golden`'s. Profiles are written through the store and assigned
//! in the database: the routes are another package's.

use lmgw_core::config::chat_profile::{create_kind, normalise_patch, ProfileCreate, ProfilePatch};
use lmgw_core::config::SelfAdmin;
use serde_json::{json, Map, Value};

use crate::chat_golden::{plain_thread, text_output_turn, text_turn, voice_turn};

/// The profile on the other turn paths: continue, Admin Chat voice, a
/// rollover's thread, a temporary thread.
mod paths;
use crate::realtime_chat_thread::{self as rt, settings, World};

/// A profile created from `body` (`ProfileCreate`'s JSON), the snapshot
/// reloaded: its id.
async fn profile(w: &World, body: Value) -> i64 {
    let c: ProfileCreate = serde_json::from_value(body).unwrap();
    let id = lmgw_core::store::chat_profiles::create_chat_profile(
        &w.state.db,
        &create_kind(&c).unwrap(),
    )
    .await
    .unwrap()
    .unwrap()
    .id;
    w.state.reload_snapshot().await.unwrap();
    id
}

/// Point `tid` at `profile`.
async fn assign(w: &World, tid: i64, profile: Option<i64>) {
    sqlx::query("UPDATE chat_threads SET profile_id = ?1 WHERE id = ?2")
        .bind(profile)
        .bind(tid)
        .execute(&w.state.db)
        .await
        .unwrap();
}

/// A thread with `prompt` (`None`: the default) and `profile`.
async fn thread(w: &World, prompt: Option<&str>, profile: Option<i64>) -> i64 {
    let tid = plain_thread(w, None, prompt).await;
    assign(w, tid, profile).await;
    tid
}

/// Compare `got` with the golden `profiles.json`.
fn golden(got: &Map<String, Value>) {
    crate::chat_golden::golden_system("profiles", got);
}

const PERSONA: &str = "You are {{model}}, a calm voice.";
const LENGTH: &str = "Answer in one sentence.";

fn examples() -> Value {
    json!([
        {"user": "Is it raining?", "reply": "I can't see outside, so I don't know."},
        {"user": "What is {{model}}?", "reply": "A model.\nThat is all."},
    ])
}

#[tokio::test]
async fn each_kind_of_turn_takes_the_profile_s_parts() {
    let w = rt::world(|_| {}).await;
    let mut cases = Map::new();

    let full = profile(
        &w,
        json!({"name": "Full", "persona": PERSONA, "length_rule": LENGTH,
               "examples": examples()}),
    )
    .await;
    let rule_only = profile(&w, json!({"name": "Rule", "length_rule": LENGTH})).await;
    let examples_only = profile(&w, json!({"name": "Examples", "examples": examples()})).await;
    let dated = profile(
        &w,
        json!({"name": "Dated", "persona": "Today is {{date}}. Be brief."}),
    )
    .await;
    let own_block = profile(
        &w,
        json!({"name": "Own block", "persona": PERSONA,
               "voice_block": "Talk in short sentences, {{model}}."}),
    )
    .await;
    let no_block = profile(
        &w,
        json!({"name": "No block", "persona": PERSONA, "voice_block": ""}),
    )
    .await;

    // Text turns: every field, the length rule alone over the thread's
    // prompt, the examples alone over an empty prompt.
    let t = thread(&w, None, Some(full)).await;
    cases.insert("text_full".into(), text_turn(&w, t, false).await);
    let t = thread(&w, Some("Du bist ein Assistent."), Some(rule_only)).await;
    cases.insert("text_rule_only".into(), text_turn(&w, t, false).await);
    let t = thread(&w, Some(""), Some(examples_only)).await;
    cases.insert("text_examples_only".into(), text_turn(&w, t, false).await);

    // Read aloud and a text-output session with a language.
    let de = thread(&w, None, Some(full)).await;
    w.set(de, json!({"voice": {"language": "de"}})).await;
    cases.insert("speak_full_de".into(), text_turn(&w, de, true).await);
    cases.insert("text_output_full_de".into(), text_output_turn(&w, de).await);

    // Voice turns: the generic block, with and without a language, the
    // date sentence judged on the persona (the thread's built-in prompt
    // has a {{date}}, the full profile's persona has none).
    let plain = thread(&w, None, Some(full)).await;
    cases.insert("voice_generic".into(), voice_turn(&w, plain).await);
    cases.insert("voice_generic_de".into(), voice_turn(&w, de).await);
    let t = thread(&w, Some("No date here."), Some(dated)).await;
    cases.insert("voice_dated_persona".into(), voice_turn(&w, t).await);
    let t = thread(&w, Some("Du bist ein Assistent."), Some(rule_only)).await;
    cases.insert("voice_rule_only".into(), voice_turn(&w, t).await);

    // The profile's own voice block, and none, with and without a language.
    let t = thread(&w, None, Some(own_block)).await;
    cases.insert("voice_own_block".into(), voice_turn(&w, t).await);
    w.set(t, json!({"voice": {"language": "de"}})).await;
    cases.insert("voice_own_block_de".into(), voice_turn(&w, t).await);
    let t = thread(&w, None, Some(no_block)).await;
    cases.insert("voice_no_block".into(), voice_turn(&w, t).await);
    w.set(t, json!({"voice": {"language": "de"}})).await;
    cases.insert("voice_no_block_de".into(), voice_turn(&w, t).await);
    // The profile's block wins over realtime.default_instructions; a
    // profile without one takes the owner's text.
    settings(&w.state, |s| {
        s.realtime.default_instructions = Some("Sprich kurz.".into())
    })
    .await;
    let t = thread(&w, None, Some(own_block)).await;
    cases.insert(
        "voice_own_block_over_setting".into(),
        voice_turn(&w, t).await,
    );
    cases.insert(
        "voice_setting_under_profile".into(),
        voice_turn(&w, plain).await,
    );
    settings(&w.state, |s| s.realtime.default_instructions = None).await;

    // The built-in "Concise", as seeded.
    let concise: i64 = sqlx::query_scalar("SELECT id FROM chat_profiles WHERE builtin = 'concise'")
        .fetch_one(&w.state.db)
        .await
        .unwrap();
    let t = thread(&w, None, Some(concise)).await;
    cases.insert("text_concise".into(), text_turn(&w, t, false).await);
    w.set(t, json!({"voice": {"language": "de"}})).await;
    cases.insert("voice_concise_de".into(), voice_turn(&w, t).await);

    // Admin Chat: the wrapper around what the profile gives.
    settings(&w.state, |s| s.self_admin = SelfAdmin::ReadOnly).await;
    let admin = plain_thread(&w, Some("admin"), Some("Du bist ein Assistent.")).await;
    assign(&w, admin, Some(full)).await;
    cases.insert("admin_text_full".into(), text_turn(&w, admin, false).await);

    golden(&cases);
}

#[tokio::test]
async fn reasoning_goes_thread_then_profile_then_the_voice_default() {
    let w = rt::world(|_| {}).await;
    let on = profile(&w, json!({"name": "On", "reasoning": "on"})).await;
    let off = profile(&w, json!({"name": "Off", "reasoning": "off"})).await;
    let inherit = profile(&w, json!({"name": "Inherit", "persona": "P"})).await;
    let effort = |v: &Value| v["reasoning_effort"].clone();

    // A text turn: the profile's on/off, else the route's default.
    let t_off = thread(&w, Some("P"), Some(off)).await;
    assert_eq!(effort(&text_turn(&w, t_off, false).await), "none");
    let t_inherit = thread(&w, Some("P"), Some(inherit)).await;
    assert_eq!(effort(&text_turn(&w, t_inherit, false).await), Value::Null);
    let t_on = thread(&w, Some("P"), Some(on)).await;
    let text_on = text_turn(&w, t_on, false).await;

    // A voice turn: the profile's on over the voice default's off; a
    // profile that inherits leaves the default off.
    let voice_on = voice_turn(&w, t_on).await;
    assert_eq!(effort(&voice_on), effort(&text_on), "on reaches both alike");
    assert_ne!(effort(&voice_on), "none", "{voice_on}");
    assert_eq!(effort(&voice_turn(&w, t_inherit).await), "none");
    assert_eq!(effort(&voice_turn(&w, t_off).await), "none");

    // The thread's own field wins over the profile's, in both.
    w.set(t_off, json!({"reasoning_effort": "low"})).await;
    assert_eq!(effort(&text_turn(&w, t_off, false).await), "low");
    assert_eq!(effort(&voice_turn(&w, t_off).await), "low");
    w.set(t_on, json!({"reasoning_enabled": false})).await;
    assert_eq!(effort(&text_turn(&w, t_on, false).await), "none");
}

#[tokio::test]
async fn a_profile_that_is_gone_is_none_and_an_edit_applies_next_turn() {
    let w = rt::world(|_| {}).await;
    let p = profile(&w, json!({"name": "P", "persona": "First."})).await;
    let with = thread(&w, Some("Own."), Some(p)).await;
    let without = thread(&w, Some("Own."), None).await;
    assert_eq!(text_turn(&w, with, false).await["system"], "First.");

    // An edit: the next turn takes it.
    let mut patch: ProfilePatch =
        serde_json::from_value(json!({"persona": "Second.", "length_rule": "Short."})).unwrap();
    normalise_patch(&mut patch).unwrap();
    lmgw_core::store::chat_profiles::update_chat_profile(&w.state.db, p, &patch)
        .await
        .unwrap()
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    assert_eq!(
        text_turn(&w, with, false).await["system"],
        "Second.\n\nShort."
    );

    // Deleted: the thread sends what a thread with none does.
    sqlx::query("DELETE FROM chat_profiles WHERE id = ?1")
        .bind(p)
        .execute(&w.state.db)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    assert_eq!(
        text_turn(&w, with, false).await,
        text_turn(&w, without, false).await
    );
    assert_eq!(voice_turn(&w, with).await, voice_turn(&w, without).await);
}
