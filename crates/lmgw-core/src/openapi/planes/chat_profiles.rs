//! Personality profiles' documented routes (personality-profiles design
//! §3.1, §3.6), under the "Chat" tag and merged into the Chat plane
//! (`planes::chat`). Every shape is `lmgw-api-types::chat_profiles`', the
//! types the handlers serialize. No op and no `x-lmgw` header (D12).

use lmgw_api_types::chat_profiles as profiles;

use super::super::registry::{DocRoute, Req, Resp};
use super::chat::chat_route;

/// How a device's write is checked, said after every write's description.
macro_rules! device_writes {
    () => {
        " A device key may list, create, change, reset and delete profiles; a voice.tts_alias it \
         writes must be within its alias scope (403 key_scope), unless the profile already \
         had it. A profile that Admin Chat or the self-admin toolset uses (a thread, or a \
         folder's defaults) is changed, reset or deleted by a device only while the device may \
         use lmgw's admin tools with writes, its own level and the gateway's (403 \
         profile_in_admin_use, which counts the use and names no thread)."
    };
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<profiles::ProfileList>()),
            ..chat_route(
                "GET",
                "/chat/api/profiles",
                "List the personality profiles",
                "Every personality profile in name order — how a thread's model talks, in text \
                 and in voice: a persona, a length rule, example exchanges, the voice turn's \
                 block, reasoning on or off, and the speech voice — each with used_by (the \
                 threads and folders that use it, as far as the caller may see them), and \
                 default_profile_id, the profile new threads start with (null: none). \
                 \"Default\" is no profile: a thread with profile_id null. A thread picks one \
                 with its settings' profile_id, a folder with its defaults' profile_id. The \
                 feed's profile.created, profile.updated and profile.deleted keep this list \
                 current.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<profiles::ProfileCreate>()),
            response: Resp::Json(|g| g.root_schema_for::<profiles::Profile>()),
            ..chat_route(
                "POST",
                "/chat/api/profiles",
                "Create a personality profile",
                concat!(
                    "A new profile from a name and any content fields (an absent one is unset), \
                 answered as the list carries it; or {builtin: \"concise\"} alone, which \
                 creates that built-in profile again after it was deleted (409 \
                 profile_builtin_exists while it exists). A name is unique in any case (409 \
                 profile_name_taken) and never \"default\" (400 profile_name_reserved). An \
                 example's sides must not be empty. No length or count is bounded; the \
                 request's body limit is. A voice.tts_alias must name a text-to-speech model.",
                    device_writes!()
                ),
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<profiles::Profile>()),
            ..chat_route(
                "GET",
                "/chat/api/profiles/{id}",
                "Get a personality profile",
                "The profile as the list carries it; 404 not_found when there is none.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<profiles::ProfilePatch>()),
            response: Resp::Json(|g| g.root_schema_for::<profiles::Profile>()),
            ..chat_route(
                "POST",
                "/chat/api/profiles/{id}",
                "Change a personality profile",
                concat!(
                    "The fields to change: an absent field stays, null unsets it, voice is \
                 replaced whole. On a built-in profile a field written with the built-in \
                 text follows the built-in again (follows_builtin). The change applies from \
                 the next turn of every thread using the profile, a bound voice session's \
                 included. The same refusals as a create.",
                    device_writes!()
                ),
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<profiles::ProfileDeleted>()),
            ..chat_route(
                "POST",
                "/chat/api/profiles/{id}/delete",
                "Delete a personality profile",
                concat!(
                    "Deletes the profile and, in the same transaction, sets every thread using it \
                 to none, removes it from every folder's defaults, and empties the profile \
                 new threads start with when it was that one. The answer says what was \
                 cleared (threads and folders as far as the caller may see them). A built-in \
                 profile can be created again afterwards.",
                    device_writes!()
                ),
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<profiles::Profile>()),
            ..chat_route(
                "POST",
                "/chat/api/profiles/{id}/reset",
                "Reset a built-in profile",
                concat!(
                    "Resets a built-in profile to its built-in texts: every content field \
                 follows the built-in again (follows_builtin lists them all), so later \
                 improvements to it apply, and the voice is unset. The name stays. 400 \
                 profile_not_builtin for one of the owner's own profiles, 404 not_found \
                 when there is none. Recorded as profile.updated in the feed.",
                    device_writes!()
                ),
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<profiles::PreviewRequest>()),
            response: Resp::Json(|g| g.root_schema_for::<profiles::PreviewAnswer>()),
            ..chat_route(
                "POST",
                "/chat/api/profiles/preview",
                "Preview an unsaved profile",
                "The system messages an unsaved draft assembles to, as a send builds them: the \
                 static part (persona or thread prompt, length rule, examples), a text turn's \
                 and a voice turn's (with the voice block, the language and the tag hint of the \
                 text-to-speech model the draft resolves to). thread_id assembles as that thread \
                 would, the draft in place of its own profile; without it, an empty chat thread \
                 with Settings' defaults. With model, the three are counted on that alias \
                 through the universal token counter (as POST /v1/count_tokens counts), with \
                 its approximation flags; counting on a local model loads it. Nothing is \
                 stored. A draft is checked as a create's content is (an example's empty side \
                 is 400 bad_request).",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<profiles::TestRequest>()),
            response: Resp::Json(|g| g.root_schema_for::<profiles::TestAnswer>()),
            ..chat_route(
                "POST",
                "/chat/api/profiles/test",
                "Test an unsaved profile",
                "One model call on model with the system message the draft assembles to (a \
                 text turn's, or with voice: true a voice turn's) and text as the user's \
                 message: no history, no max_tokens, no tools. The answer carries the system \
                 message sent, the reply, its reasoning, the reasoning note, the usage, \
                 first_token_ms and reasoning_ms, total_ms, and answered_by when a fallback \
                 answered. It writes the call's request row (a device's call is its key's) and \
                 stores nothing else. The model's refusals answer with their status and code.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<profiles::SpeakRequest>()),
            response: Resp::Binary(&["audio/wav"]),
            ..chat_route(
                "POST",
                "/chat/api/profiles/speak",
                "Speak a sample with an unsaved profile",
                "Says text with the voice the draft resolves to (its text-to-speech model, \
                 voice and speech style, after the thread's own, before the Chat's settings), \
                 through the read-aloud's pipeline, and answers one WAV (PCM16 mono, 24 kHz). \
                 Refused before any audio as the Chat's read-aloud refuses, in flat JSON: \
                 tts_not_configured, voice_not_found, voice_not_configured, \
                 instructions_required; a voice that fails while it speaks is 502 with its \
                 code. It writes one text-to-speech row; a thread's seed is used, none is \
                 drawn into it.",
            )
        },
    ]
}
