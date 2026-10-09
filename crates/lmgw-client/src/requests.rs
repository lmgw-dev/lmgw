//! Request builders for the routes a client uses, and readers of their
//! answers. Sans-IO: a [`Request`] says what to send; the client's HTTP
//! stack sends it with the key and hands back the status and the body.
//!
//! - `GET /chat/api/folders`, `POST /chat/api/folders`: find or create the
//!   client's folder by name (names need not be unique);
//! - `POST /chat/api/folders/{id}/current`: the ongoing conversation's
//!   current thread;
//! - `GET /chat/api/threads`: every thread, for a client that shows them;
//! - `GET /chat/api/profiles` and the profile writes: the personality
//!   profiles, listed, created, changed, reset and deleted; and
//!   `POST /chat/api/folders/{id}` with `defaults_patch: {profile_id}`, the
//!   profile of a folder's conversation ([`set_folder_profile`]);
//! - `GET /chat/api/feed`: the change feed ([`crate::feed`]);
//! - `GET /v1/realtime?chat_thread=<id>`: a voice session bound to a
//!   thread ([`crate::realtime`]), taking it over from another session or
//!   not (`&takeover=never`);
//! - `GET /mcp/host`: the device's MCP host link ([`crate::mcp_host`]);
//! - `POST /chat/api/threads/{id}/approvals` and
//!   `POST /chat/api/threads/{id}/transcribe`: deciding a turn's waiting
//!   calls, and a voice answer kept out of the conversation
//!   ([`approvals`](mod@approvals));
//! - `GET /chat/api/threads/{id}/tasks`, `POST /chat/api/threads/{id}/answer` and
//!   `POST /chat/api/threads/{id}/tasks/{task}/cancel`: answering a
//!   thread's MCP task results, and cancelling a task
//!   ([`tasks`](mod@tasks)).
//!
//! A refusal reads with [`read_refusal`] as an [`ApiError`]: the Chat
//! routes answer the flat `{code, message}`, the realtime handshake
//! OpenAI's `{"error": {code, message, type, param}}`.
//!
//! What the client cannot do here: a folder's model and its retention are
//! set from the dashboard (a device key that writes the retention is
//! refused). `current`'s `folder_no_model` refusal is a folder created
//! without a model; a client names one in its folder create's `defaults`,
//! or sends the user to the folder's settings form
//! ([`folder_settings_page`]).

pub use lmgw_api_types::chat::{Folder, FolderCreate, FolderList, Thread, ThreadList, ThreadRow};
pub use lmgw_api_types::chat_folders::{
    AppliedToCurrent, CurrentReason, CurrentRequest, CurrentThread, FolderPatched,
};
pub use lmgw_api_types::chat_profiles::{
    Profile, ProfileCreate, ProfileDeleted, ProfileList, ProfilePatch,
};
pub use lmgw_api_types::ApiError;

/// An HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// One request header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// A request to send: its method, its path with the query, its headers
/// and its JSON body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// From the gateway's root: `/chat/api/feed`.
    pub path: String,
    pub headers: Vec<Header>,
    pub body: Option<String>,
}

impl Request {
    fn new(method: Method, path: String) -> Self {
        Request {
            method,
            path,
            headers: Vec::new(),
            body: None,
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push(Header {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    fn json(mut self, body: String) -> Self {
        self.body = Some(body);
        self.header("Content-Type", "application/json")
    }

    /// With the key as `Authorization: Bearer <key>`. Keep the request out
    /// of logs once it carries it.
    pub fn bearer(self, key: &str) -> Self {
        self.header("Authorization", &format!("Bearer {key}"))
    }

    /// The URL under `base_url` (`http://127.0.0.1:8001`, with or without a
    /// path prefix and a trailing slash).
    pub fn url(&self, base_url: &str) -> String {
        format!("{}{}", base_url.trim_end_matches('/'), self.path)
    }

    /// [`Self::url`] for a WebSocket: `ws:` for an `http:` base, `wss:` for
    /// `https:`, the scheme in any case (`HTTPS://` too). A base that is
    /// neither is refused (`bad_base_url`).
    pub fn ws_url(&self, base_url: &str) -> Result<String, ApiError> {
        let url = self.url(base_url.trim());
        let scheme = |name: &str| {
            url.get(..name.len())
                .filter(|head| head.eq_ignore_ascii_case(name))
                .map(|_| &url[name.len()..])
        };
        if let Some(rest) = scheme("https://") {
            Ok(format!("wss://{rest}"))
        } else if let Some(rest) = scheme("http://") {
            Ok(format!("ws://{rest}"))
        } else {
            Err(ApiError {
                code: "bad_base_url".into(),
                message: format!(
                    "the gateway's address must start with http:// or https:// (got '{}')",
                    base_url.trim()
                ),
            })
        }
    }
}

/// `GET /chat/api/folders`; read with [`read_folders`].
pub fn folders() -> Request {
    Request::new(Method::Get, "/chat/api/folders".into())
}

/// `POST /chat/api/folders`; read with [`read_folder`].
pub fn create_folder(body: &FolderCreate) -> Request {
    Request::new(Method::Post, "/chat/api/folders".into())
        .json(serde_json::to_string(body).expect("a folder create always serializes"))
}

/// `POST /chat/api/folders/{id}/current`, `new: true` for a new
/// conversation; read with [`read_current`].
pub fn current(folder_id: i64, new: bool) -> Request {
    Request::new(
        Method::Post,
        format!("/chat/api/folders/{folder_id}/current"),
    )
    .json(serde_json::to_string(&CurrentRequest { new }).expect("always serializes"))
}

/// `GET /chat/api/threads` (the active threads); read with
/// [`read_threads`].
pub fn threads() -> Request {
    Request::new(Method::Get, "/chat/api/threads".into())
}

/// `GET /chat/api/profiles`: every personality profile and the one new
/// threads start with; read with [`read_profiles`]. The feed's `profile.*`
/// events say when to read it again.
pub fn profiles() -> Request {
    Request::new(Method::Get, "/chat/api/profiles".into())
}

/// `GET /chat/api/profiles/{id}`; read with [`read_profile`].
pub fn profile(id: i64) -> Request {
    Request::new(Method::Get, format!("/chat/api/profiles/{id}"))
}

/// `POST /chat/api/profiles`; read with [`read_profile`].
pub fn create_profile(body: &ProfileCreate) -> Request {
    Request::new(Method::Post, "/chat/api/profiles".into())
        .json(serde_json::to_string(body).expect("a profile create always serializes"))
}

/// `POST /chat/api/profiles/{id}`: the fields `patch` names (absent
/// unchanged, `null` unset); read with [`read_profile`].
pub fn update_profile(id: i64, patch: &ProfilePatch) -> Request {
    Request::new(Method::Post, format!("/chat/api/profiles/{id}"))
        .json(serde_json::to_string(patch).expect("a profile patch always serializes"))
}

/// `POST /chat/api/profiles/{id}/reset`: a built-in profile back to its
/// built-in texts (each follows the built-in again), voice unset, name
/// kept; read with [`read_profile`]. `400 profile_not_builtin` for one of
/// the owner's own.
pub fn reset_profile(id: i64) -> Request {
    Request::new(Method::Post, format!("/chat/api/profiles/{id}/reset"))
}

/// `POST /chat/api/profiles/{id}/delete`; read with
/// [`read_profile_deleted`].
pub fn delete_profile(id: i64) -> Request {
    Request::new(Method::Post, format!("/chat/api/profiles/{id}/delete"))
}

/// `POST /chat/api/folders/{id}` with `{defaults_patch: {profile_id}}`: the
/// folder's conversation talks with profile `profile_id` from now on
/// (`None`: none, unless Settings → Chat names one for new threads). New
/// threads in the folder start with it, and with `apply_to_current` (the
/// gateway's default) its current thread switches too, from its next turn
/// on, a bound voice session's included; the answer's `applied` names the
/// thread. Read with [`read_folder_patched`].
pub fn set_folder_profile(folder_id: i64, profile_id: Option<i64>) -> Request {
    Request::new(Method::Post, format!("/chat/api/folders/{folder_id}"))
        .json(serde_json::json!({ "defaults_patch": { "profile_id": profile_id } }).to_string())
}

/// The profile `name` names in `list`, as a user says it: any case,
/// trimmed; `default` is no profile (`Ok(None)`). An unknown name is the
/// `unknown_profile` refusal, listing the names there are in one line.
pub fn profile_named(list: &ProfileList, name: &str) -> Result<Option<i64>, ApiError> {
    let key = name.trim().to_lowercase();
    if key == lmgw_api_types::chat_profiles::RESERVED_NAME {
        return Ok(None);
    }
    if let Some(p) = list
        .profiles
        .iter()
        .find(|p| p.name.trim().to_lowercase() == key)
    {
        return Ok(Some(p.id));
    }
    let mut names = vec!["Default".to_string()];
    names.extend(list.profiles.iter().map(|p| p.name.clone()));
    Err(ApiError {
        code: "unknown_profile".into(),
        message: format!(
            "there is no profile named '{}'; the profiles are: {}",
            name.trim(),
            names.join(", ")
        ),
    })
}

/// `GET /chat/api/feed`, resumed from `cursor` with `Last-Event-ID` when
/// there is one.
pub fn feed(cursor: Option<&str>) -> Request {
    let r =
        Request::new(Method::Get, "/chat/api/feed".into()).header("Accept", "text/event-stream");
    match cursor.filter(|c| !c.is_empty()) {
        Some(c) => r.header("Last-Event-ID", c),
        None => r,
    }
}

/// `GET /v1/realtime?chat_thread=<id>`: open it with [`Request::ws_url`]
/// and the key. A refusal before the upgrade reads with [`read_refusal`].
pub fn realtime(thread_id: i64) -> Request {
    Request::new(Method::Get, format!("/v1/realtime?chat_thread={thread_id}"))
}

/// [`realtime`] that never takes the thread over (`&takeover=never`): while
/// another session is bound to the thread the upgrade is refused, 409
/// `chat_thread_bound` ([`code::CHAT_THREAD_BOUND`]), naming who holds it
/// ("voice is in use on device 'phone'"), and that session goes on. For a
/// client's own automatic rebinds — after its conversation moved to a new
/// thread, after a restart's 1001 or a dropped link — so it never takes the
/// voice from another device that followed the same move; a user's explicit
/// choice to talk here binds with [`realtime`] and takes over.
///
/// What is not "another session": one bound with the same key (the
/// client's own, whose link the gateway has not yet seen drop) and one that
/// is already ending (after its 1001, a revocation or its close, while it
/// writes its last turns). Such a session is taken over, so a rebind is
/// never refused in the client's own name.
pub fn realtime_unless_bound(thread_id: i64) -> Request {
    Request::new(
        Method::Get,
        format!("/v1/realtime?chat_thread={thread_id}&takeover=never"),
    )
}

/// `GET /mcp/host`: a device's MCP host link (client-apps design §5,
/// [`crate::mcp_host`]). Open it with [`Request::ws_url`] and the device key
/// ([`Request::bearer`]), and send no `Origin` header. A refusal before the
/// upgrade reads with [`read_refusal`]: `host_not_granted`,
/// `cross_origin_refused`, or the key's own.
pub fn mcp_host() -> Request {
    Request::new(Method::Get, lmgw_api_types::mcp_host::PATH.into())
}

/// The bind's URL for a page served by the gateway itself: its origin,
/// `ws:` or `wss:` as the page is `http:` or `https:`.
pub fn realtime_page_url(protocol: &str, host: &str, thread_id: i64) -> String {
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    format!("{scheme}://{host}{}", realtime(thread_id).path)
}

/// The refusal a non-2xx answer carries, in either shape the gateway
/// answers with: the flat `{code, message}` of the Chat routes, or the
/// realtime handshake's `{"error": {code, message, …}}` (its `type` when it
/// has no `code`). A body that is neither is `http_<status>` with the body
/// as the message.
pub fn read_refusal(status: u16, body: &str) -> ApiError {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let text = |v: &serde_json::Value| {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let nested = &v["error"];
    let (code, message) = if nested.is_object() {
        (
            text(&nested["code"]).or_else(|| text(&nested["type"])),
            text(&nested["message"]),
        )
    } else {
        (text(&v["code"]), text(&v["message"]))
    };
    match code {
        Some(code) => ApiError {
            message: message.unwrap_or_else(|| code.clone()),
            code,
        },
        None => {
            let body = body.trim();
            ApiError {
                code: format!("http_{status}"),
                message: if body.is_empty() {
                    format!("HTTP {status}")
                } else {
                    body.to_string()
                },
            }
        }
    }
}

/// The dashboard page with folder `folder_id`'s settings form open, from
/// the gateway's root (put it after the base URL, as [`Request::url`]
/// does): where the owner marks a folder as an ongoing conversation, gives
/// it a model and its own retention — what a device key cannot set. A
/// browser opens it; the owner signs in there if it asks.
pub fn folder_settings_page(folder_id: i64) -> String {
    format!("/chat?folder={folder_id}&settings=1")
}

/// When the ongoing conversation of `folder` moves on to a new thread by
/// itself, in unix seconds: the next `current` call from then on answers a
/// new thread (reason `idle`), as long as no message came meanwhile and no
/// turn runs in `thread` then. `None` when it never does by itself: the
/// folder is not ongoing, its idle minutes are 0 (a new thread only when a
/// client asks), `thread` has no message yet, or the moment lies past what
/// a unix time in seconds holds (an idle time no clock reaches). `thread`
/// is the folder's current thread, as `current` or the thread list gave
/// it; compare with the client's clock.
pub fn idle_rollover_at(folder: &Folder, thread: &ThreadRow) -> Option<i64> {
    let idle = folder.ongoing.as_ref()?.idle_minutes;
    if idle <= 0 {
        return None;
    }
    thread.last_message_at?.checked_add(idle.checked_mul(60)?)
}

/// What a refusal says about the client's own key, when it says the key
/// opens nothing now ([`key_refused`]): the client stops retrying and tells
/// the user what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyRefused {
    /// `device_key_unknown`: the device key matches no device any more (it
    /// was rotated or deleted). The device must be paired again.
    PairAgain,
    /// `device_disabled`: the device is disabled on the gateway; the same
    /// key works once it is enabled again.
    Disabled,
    /// `key_expired`: the key is past its expiry date.
    Expired,
}

/// Refusal codes a client acts on: the three [`key_refused`] reads, and
/// the bind's when another session holds the thread
/// ([`realtime_unless_bound`]).
pub mod code {
    pub const DEVICE_KEY_UNKNOWN: &str = "device_key_unknown";
    pub const DEVICE_DISABLED: &str = "device_disabled";
    pub const KEY_EXPIRED: &str = "key_expired";
    pub const CHAT_THREAD_BOUND: &str = "chat_thread_bound";
    pub const CHAT_TOOLSET_NEEDS_FULL: &str = "chat_toolset_needs_full";
    pub const HOST_NOT_GRANTED: &str = lmgw_api_types::mcp_host::HOST_NOT_GRANTED;
    pub const CROSS_ORIGIN_REFUSED: &str = lmgw_api_types::mcp_host::CROSS_ORIGIN_REFUSED;
}

/// Whether refusal `e` says the client's key opens nothing now, and why;
/// `None` for any other refusal.
pub fn key_refused(e: &ApiError) -> Option<KeyRefused> {
    match e.code.as_str() {
        code::DEVICE_KEY_UNKNOWN => Some(KeyRefused::PairAgain),
        code::DEVICE_DISABLED => Some(KeyRefused::Disabled),
        code::KEY_EXPIRED => Some(KeyRefused::Expired),
        _ => None,
    }
}

macro_rules! reader {
    ($(#[$doc:meta])* $name:ident -> $ty:ty) => {
        $(#[$doc])*
        pub fn $name(status: u16, body: &str) -> Result<$ty, ApiError> {
            if !(200..300).contains(&status) {
                return Err(read_refusal(status, body));
            }
            serde_json::from_str::<$ty>(body).map_err(|e| ApiError {
                code: "unreadable".into(),
                message: format!(
                    "the answer does not read as {}: {e}",
                    stringify!($ty)
                ),
            })
        }
    };
}

reader!(
    /// `GET /chat/api/folders`'s answer.
    read_folders -> FolderList
);
reader!(
    /// A folder create's answer: the folder as listed.
    read_folder -> Folder
);
reader!(
    /// `POST /chat/api/folders/{id}/current`'s answer.
    read_current -> CurrentThread
);
reader!(
    /// `GET /chat/api/threads`'s answer.
    read_threads -> ThreadList
);
reader!(
    /// `GET /chat/api/profiles`'s answer.
    read_profiles -> ProfileList
);
reader!(
    /// A profile: `GET /chat/api/profiles/{id}`'s answer, and a create's
    /// or a change's.
    read_profile -> Profile
);
reader!(
    /// A profile delete's answer: what it cleared.
    read_profile_deleted -> ProfileDeleted
);
reader!(
    /// A folder patch's answer ([`set_folder_profile`]'s): the folder, and
    /// what reached its current thread.
    read_folder_patched -> FolderPatched
);

/// MCP approvals and the dictation route (client-apps design §6).
pub mod approvals;
pub use approvals::{approvals, read_transcription, transcribe};

/// A thread's MCP tasks: `answer` and the cancel (MCP Tasks design §5.1).
pub mod tasks;
pub use tasks::{answer, cancel_task, read_task_cancelled, read_thread_tasks, thread_tasks};

#[cfg(test)]
mod tests {
    use lmgw_api_types::chat_folders::OngoingInput;

    use super::*;

    #[test]
    fn current_posts_its_body() {
        let r = current(12, true).bearer("lmgw-device-x");
        assert_eq!(r.method.as_str(), "POST");
        assert_eq!(
            r.url("http://127.0.0.1:8001/"),
            "http://127.0.0.1:8001/chat/api/folders/12/current"
        );
        assert_eq!(r.body.as_deref(), Some(r#"{"new":true}"#));
        let names: Vec<_> = r.headers.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["Content-Type", "Authorization"]);
        assert_eq!(r.headers[1].value, "Bearer lmgw-device-x");
    }

    #[test]
    fn a_folder_create_sends_only_what_it_sets() {
        let r = create_folder(&FolderCreate {
            name: "Assistant".into(),
            ongoing: Some(OngoingInput { idle_minutes: 30 }),
            ..Default::default()
        });
        assert_eq!(
            r.body.as_deref(),
            Some(r#"{"name":"Assistant","ongoing":{"idle_minutes":30}}"#)
        );
    }

    #[test]
    fn the_idle_rollover_is_timed_from_the_newest_message() {
        use lmgw_api_types::chat_folders::FolderOngoing;
        let folder = |idle: Option<i64>| Folder {
            ongoing: idle.map(|idle_minutes| FolderOngoing {
                idle_minutes,
                current_thread_id: Some(5),
            }),
            ..Default::default()
        };
        let thread = |last: Option<i64>| ThreadRow {
            id: 5,
            last_message_at: last,
            ..Default::default()
        };
        let at = 1_760_000_000;
        assert_eq!(
            idle_rollover_at(&folder(Some(30)), &thread(Some(at))),
            Some(at + 1800)
        );
        assert_eq!(idle_rollover_at(&folder(Some(30)), &thread(None)), None);
        assert_eq!(idle_rollover_at(&folder(Some(0)), &thread(Some(at))), None);
        assert_eq!(idle_rollover_at(&folder(None), &thread(Some(at))), None);
        // An idle time no clock reaches (review F-12): never, not a panic
        // or a wrapped time.
        assert_eq!(
            idle_rollover_at(&folder(Some(i64::MAX / 2)), &thread(Some(at))),
            None
        );
        assert_eq!(
            idle_rollover_at(&folder(Some(i64::MAX / 120)), &thread(Some(i64::MAX))),
            None
        );
        let row: ThreadRow =
            serde_json::from_str(r#"{"id": 5, "last_message_at": 1760000000}"#).unwrap();
        assert_eq!(row.last_message_at, Some(at));
    }

    #[test]
    fn a_folder_s_profile_is_a_defaults_patch() {
        let r = set_folder_profile(4, Some(3));
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/chat/api/folders/4");
        assert_eq!(
            r.body.as_deref(),
            Some(r#"{"defaults_patch":{"profile_id":3}}"#)
        );
        assert_eq!(
            set_folder_profile(4, None).body.as_deref(),
            Some(r#"{"defaults_patch":{"profile_id":null}}"#)
        );
        let p = read_folder_patched(
            200,
            r#"{"id": 4, "name": "Assistant", "defaults": {"profile_id": 3},
                "applied": {"thread_id": 9, "fields": ["profile_id"]}}"#,
        )
        .unwrap();
        assert_eq!(p.folder.defaults.profile_id, Some(3));
        assert_eq!(p.applied.map(|a| a.thread_id), Some(9));
    }

    #[test]
    fn profile_requests_and_names() {
        assert_eq!(profiles().path, "/chat/api/profiles");
        assert_eq!(profile(3).path, "/chat/api/profiles/3");
        assert_eq!(delete_profile(3).path, "/chat/api/profiles/3/delete");
        assert_eq!(reset_profile(3).path, "/chat/api/profiles/3/reset");
        let r = update_profile(
            3,
            &ProfilePatch {
                persona: Some(None),
                ..Default::default()
            },
        );
        assert_eq!(r.body.as_deref(), Some(r#"{"persona":null}"#));
        let r = create_profile(&ProfileCreate {
            name: Some("Calm".into()),
            ..Default::default()
        });
        assert_eq!(r.method, Method::Post);
        let list = read_profiles(
            200,
            r#"{"profiles": [{"id": 3, "name": "Concise"}, {"id": 5, "name": "Ärger"}],
                "default_profile_id": null}"#,
        )
        .unwrap();
        assert_eq!(profile_named(&list, " concise ").unwrap(), Some(3));
        assert_eq!(profile_named(&list, "ÄRGER").unwrap(), Some(5));
        assert_eq!(profile_named(&list, "Default").unwrap(), None);
        let e = profile_named(&list, "Pirate").unwrap_err();
        assert_eq!(e.code, "unknown_profile");
        assert!(
            e.message.ends_with("Default, Concise, Ärger"),
            "{}",
            e.message
        );
    }

    #[test]
    fn the_feed_resumes_with_last_event_id() {
        assert!(feed(None).headers.iter().all(|h| h.name != "Last-Event-ID"));
        assert!(feed(Some(""))
            .headers
            .iter()
            .all(|h| h.name != "Last-Event-ID"));
        let r = feed(Some("ab12:40"));
        assert_eq!(r.method, Method::Get);
        assert!(r
            .headers
            .iter()
            .any(|h| h.name == "Last-Event-ID" && h.value == "ab12:40"));
    }

    #[test]
    fn the_realtime_url_follows_the_base() {
        assert_eq!(
            realtime(7).ws_url("http://127.0.0.1:8001").unwrap(),
            "ws://127.0.0.1:8001/v1/realtime?chat_thread=7"
        );
        assert_eq!(
            realtime(-2).ws_url("https://gw.example/lmgw/").unwrap(),
            "wss://gw.example/lmgw/v1/realtime?chat_thread=-2"
        );
        assert_eq!(
            realtime(3).ws_url(" HTTPS://Gw.example ").unwrap(),
            "wss://Gw.example/v1/realtime?chat_thread=3",
            "a hand-edited link's scheme"
        );
        for bad in ["gw.example:8001", "ftp://gw.example", "ws://gw.example", ""] {
            let e = realtime(3).ws_url(bad).unwrap_err();
            assert_eq!(e.code, "bad_base_url", "{bad}");
        }
        assert_eq!(
            realtime_unless_bound(7)
                .ws_url("http://127.0.0.1:8001")
                .unwrap(),
            "ws://127.0.0.1:8001/v1/realtime?chat_thread=7&takeover=never"
        );
        assert_eq!(
            realtime_page_url("https:", "gw.example:8443", -2),
            "wss://gw.example:8443/v1/realtime?chat_thread=-2"
        );
        assert_eq!(
            realtime_page_url("http:", "127.0.0.1:8001", 7),
            "ws://127.0.0.1:8001/v1/realtime?chat_thread=7"
        );
    }

    #[test]
    fn answers_read_as_their_type_or_the_refusal() {
        let e = read_current(
            409,
            r#"{"code":"folder_no_model","message":"folder 'A' names no model"}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, "folder_no_model");
        let e = read_current(502, "Bad Gateway").unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.message.as_str()),
            ("http_502", "Bad Gateway")
        );
        assert_eq!(read_refusal(401, "").message, "HTTP 401");
        assert_eq!(folder_settings_page(12), "/chat?folder=12&settings=1");
        // The realtime handshake's refusal, as the gateway writes it.
        let e = read_refusal(
            404,
            r#"{"error":{"message":"chat thread 12 not found","type":"invalid_request_error","param":null,"code":"chat_thread_not_found"}}"#,
        );
        assert_eq!(
            (e.code.as_str(), e.message.as_str()),
            ("chat_thread_not_found", "chat thread 12 not found")
        );
        let e = read_refusal(
            401,
            r#"{"error":{"message":"no key","type":"authentication_error","param":null,"code":null}}"#,
        );
        assert_eq!(
            (e.code.as_str(), e.message.as_str()),
            ("authentication_error", "no key")
        );
        // A rotated or deleted device key, in either shape.
        for body in [
            r#"{"code":"device_key_unknown","message":"this device key is no longer valid"}"#,
            r#"{"error":{"message":"this device key is no longer valid","type":"invalid_request_error","param":null,"code":"device_key_unknown"}}"#,
        ] {
            let e = read_refusal(401, body);
            assert_eq!(key_refused(&e), Some(KeyRefused::PairAgain), "{body}");
        }
        assert_eq!(
            key_refused(&read_refusal(
                401,
                r#"{"code":"device_disabled","message":"x"}"#
            )),
            Some(KeyRefused::Disabled)
        );
        assert_eq!(
            key_refused(&read_refusal(404, r#"{"code":"not_found","message":"x"}"#)),
            None
        );
        // A JSON body of neither shape is the body.
        let e = read_refusal(500, r#"{"detail":"x"}"#);
        assert_eq!(e.code, "http_500");
        let e = read_folders(200, "not json").unwrap_err();
        assert_eq!(e.code, "unreadable");
        let body = r#"{"thread": {"id": 5, "title": "Morning", "voice_resolved": {},
            "continue": {"ok": false, "reason": "there is no reply to continue yet"}},
            "rolled_over": true, "reason": "idle", "note": "idle for 30 minutes"}"#;
        let c = read_current(200, body).unwrap();
        assert_eq!(
            (c.thread.row.id, c.thread.row.title.as_str()),
            (5, "Morning")
        );
        assert_eq!(c.reason, Some(CurrentReason::Idle));
        // A reason a newer gateway added reads, and the note says it.
        let c = read_current(
            200,
            r#"{"thread": {"id": 6}, "rolled_over": true, "reason": "later",
                "note": "a new thread, for a reason this build does not know"}"#,
        )
        .unwrap();
        assert_eq!(c.reason, Some(CurrentReason::Unknown));
        let f = read_folders(
            200,
            r#"{"folders": [{"id": 3, "name": "Assistant",
            "ongoing": {"idle_minutes": 30, "current_thread_id": 5}}]}"#,
        )
        .unwrap();
        assert_eq!(
            f.folders[0].ongoing.as_ref().map(|o| o.idle_minutes),
            Some(30)
        );
    }
}
