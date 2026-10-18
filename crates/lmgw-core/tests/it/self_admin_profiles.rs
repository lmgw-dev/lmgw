//! The answers of the personality-profile tools (`lmgw__profiles`,
//! `lmgw__profile_set`, `lmgw__profile_delete`), which no OpenAPI document
//! describes: each tool's text is read into its type and written out again,
//! and the two texts are equal byte for byte. The text is the answer's pretty
//! JSON with sorted keys, so a key a type adds, drops, renames or turns from
//! absent into `null` shows up here. A profile the tools show is the one the
//! route lists, with its examples as text.

use lmgw_api_types::chat_profiles::ProfileList;
use lmgw_api_types::self_admin::{ToolProfile, ToolProfileDeleted, ToolProfileList};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, ThreadDefaults};
use serde_json::{json, Value};

use crate::common::{round_trips, serve, Gw};
use crate::support::self_admin::{parse, read_back, Admin};

const EXAMPLES: &str = "User: Hi\nReply: Hello.\n\nUser: Two\n  lines\nReply: Ok.";

async fn world() -> (SharedState, Admin, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let admin = Admin::new(&state).await;
    let gw = serve(state.clone()).await;
    (state, admin, gw)
}

/// What `GET /chat/api/profiles` lists, through the document's type.
async fn listed(gw: &Gw) -> ProfileList {
    let r = gw
        .client()
        .get(format!("{gw}/chat/api/profiles"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    round_trips::<ProfileList>("profiles", &v)
}

#[tokio::test]
async fn a_profile_the_tools_write_is_a_tool_profile_with_its_examples_as_text() {
    let (_state, admin, _gw) = world().await;

    let text = admin
        .text(
            "lmgw__profile_set",
            json!({
                "action": "create", "name": "Terse", "persona": "You are curt.",
                "length_rule": "One sentence.", "examples": EXAMPLES,
                "voice_block_mode": "none", "reasoning": "off", "voice": "anna",
            }),
        )
        .await;
    let made: ToolProfile = read_back(&text);
    assert_eq!(made.name, "Terse");
    assert_eq!(made.examples, EXAMPLES);
    assert_eq!(made.voice_block.as_deref(), Some(""));
    assert_eq!(made.voice.voice.as_deref(), Some("anna"));
    assert!(made.builtin.is_none() && made.follows_builtin.is_empty());
    let live = parse(&text);
    assert_eq!(live["examples"], json!(EXAMPLES));
    assert_eq!(live["reasoning"], json!("off"));

    // Only what is given changes; the examples stay as text.
    let text = admin
        .text(
            "lmgw__profile_set",
            json!({"action": "update", "id": made.id, "persona": "You are calm.",
                   "reasoning": "inherit", "speech_style": "calm"}),
        )
        .await;
    let changed: ToolProfile = read_back(&text);
    assert_eq!(changed.persona, "You are calm.");
    assert_eq!(changed.examples, EXAMPLES);
    assert_eq!(changed.reasoning, None);
    assert_eq!(changed.voice.voice.as_deref(), Some("anna"));
    assert_eq!(changed.voice.speech_style.as_deref(), Some("calm"));

    // An example list that is cleared is the empty text, not `null`.
    let text = admin
        .text(
            "lmgw__profile_set",
            json!({"action": "update", "id": made.id, "clear": "examples"}),
        )
        .await;
    assert_eq!(read_back::<ToolProfile>(&text).examples, "");
    assert_eq!(parse(&text)["examples"], json!(""));
}

#[tokio::test]
async fn the_list_is_the_route_s_list_with_each_profile_s_examples_as_text() {
    let (_state, admin, gw) = world().await;
    admin
        .text(
            "lmgw__profile_set",
            json!({"action": "create", "name": "Terse", "examples": EXAMPLES}),
        )
        .await;
    admin
        .text(
            "lmgw__profile_set",
            json!({"action": "create", "name": "Bare"}),
        )
        .await;

    let text = admin.text("lmgw__profiles", json!({})).await;
    let tools: ToolProfileList = read_back(&text);
    let names: Vec<&str> = tools.profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Bare", "Concise", "Terse"]);
    assert_eq!(tools.default_profile_id, None);
    let terse = tools.profiles.iter().find(|p| p.name == "Terse").unwrap();
    assert_eq!(terse.examples, EXAMPLES);
    let concise = tools.profiles.iter().find(|p| p.name == "Concise").unwrap();
    assert_eq!(concise.builtin.as_deref(), Some("concise"));
    assert!(
        concise.examples.starts_with("User: "),
        "{}",
        concise.examples
    );
    assert_eq!(
        tools
            .profiles
            .iter()
            .find(|p| p.name == "Bare")
            .unwrap()
            .examples,
        ""
    );

    // The same rows as the route lists, key for key but the examples.
    let route = listed(&gw).await;
    assert_eq!(ToolProfileList::from(&route), tools);
    let live = parse(&text);
    assert!(live["profiles"][0]["examples"].is_string());
}

#[tokio::test]
async fn a_delete_says_what_it_cleared() {
    let (state, admin, gw) = world().await;
    let made: ToolProfile = read_back(
        &admin
            .text(
                "lmgw__profile_set",
                json!({"action": "create", "name": "Gone"}),
            )
            .await,
    );
    let id = made.id;
    // A thread, a folder default and the Chat's default all name it.
    sqlx::query("INSERT INTO chat_threads (id, profile_id) VALUES (9001, ?1)")
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    let folder = store::create_chat_folder(
        &state.db,
        "Uses it",
        &ThreadDefaults {
            profile_id: Some(id),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    let r = gw
        .client()
        .post(format!("{gw}/api/op/settings_set_full"))
        .json(&json!({"chat_profile": id}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let text = admin.text("lmgw__profile_delete", json!({"id": id})).await;
    let gone: ToolProfileDeleted = read_back(&text);
    assert_eq!(gone.deleted.deleted, id);
    assert_eq!(gone.deleted.threads_cleared, 1);
    assert_eq!(gone.deleted.folders_cleared.len(), 1);
    assert_eq!(gone.deleted.folders_cleared[0].id, folder);
    assert_eq!(gone.deleted.folders_cleared[0].name, "Uses it");
    assert!(gone.deleted.default_cleared);
    assert_eq!(
        gone.message,
        format!("profile {id} deleted; 1 thread(s) and 1 folder default(s) no longer use it")
    );
    let mut keys: Vec<String> = parse(&text).as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "default_cleared",
            "deleted",
            "folders_cleared",
            "message",
            "threads_cleared"
        ]
    );

    // It is gone from the list, and the Chat has no default any more.
    let list: ToolProfileList = read_back(&admin.text("lmgw__profiles", json!({})).await);
    assert!(list.profiles.iter().all(|p| p.id != id));
    assert_eq!(list.default_profile_id, None);
    let (failed, why) = admin.call("lmgw__profile_delete", json!({"id": id})).await;
    assert!(failed, "{why}");
}
