//! The requests of a thread browser: the archived list, the full-text
//! search, a thread created in a folder and the changes to a stored thread
//! (pin, archive, rename, move, delete).
//!
//! - [`archived_threads`]: `GET /chat/api/threads?archived=1`, read with
//!   [`read_threads`](crate::requests::read_threads) like the active list
//!   ([`threads`](crate::requests::threads)); `archived_count` rides along
//!   in every mode.
//! - [`search`]: `GET /chat/api/search`, read with [`read_search`].
//! - [`create_thread`]: `POST /chat/api/threads`, read with
//!   [`read_created`].
//! - [`pin`], [`archive`], [`rename`], [`move_to_folder`]: each answers the thread as
//!   the caller reads it, read with [`read_changed`]; a temporary thread
//!   (negative id) is refused with `409 temporary_thread`.
//! - [`delete`]: `POST /chat/api/threads/{id}/delete`, read with
//!   [`read_deleted`].
//!
//! A thread names itself on the first send while it is still called "New
//! chat"; [`rename`] sets the title by hand and that title is kept. The
//! settings patch has no title field.
//!
//! What a paired device may not do here: create an `admin` thread (`403
//! forbidden`). A thread or folder out of its reach is a `404 not_found`.

pub use lmgw_api_types::chat::Thread;
pub use lmgw_api_types::chat_threads::{
    ArchiveRequest, MoveRequest, PinRequest, SearchHit, SearchPage, SearchThread, ThreadCreate,
    TitleRequest,
};
pub use lmgw_api_types::Ack;

use super::calls::segment;
use super::{read_refusal, ApiError, Method, Request};

/// Which threads a [`search`] looks in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchScope {
    /// The active threads (the route's default).
    #[default]
    Active,
    /// The archived threads only.
    Archived,
    /// Both.
    All,
}

impl SearchScope {
    fn param(self) -> &'static str {
        match self {
            SearchScope::Active => "0",
            SearchScope::Archived => "1",
            SearchScope::All => "all",
        }
    }
}

/// `GET /chat/api/threads?archived=1`: the archived threads, the most
/// recently archived first; read with
/// [`read_threads`](crate::requests::read_threads).
pub fn archived_threads() -> Request {
    Request::new(Method::Get, "/chat/api/threads?archived=1".into())
}

/// `GET /chat/api/search`: the threads whose title, messages or attachment
/// names match `q` (at least two characters, else `400 bad_request`; words
/// match as prefixes, a `"quoted phrase"` as a phrase), in `scope`,
/// optionally only in `folder_id`. `offset` is the previous page's
/// `next_offset`. Read with [`read_search`].
pub fn search(q: &str, scope: SearchScope, folder_id: Option<i64>, offset: Option<i64>) -> Request {
    let mut path = format!(
        "/chat/api/search?q={}&archived={}",
        segment(q),
        scope.param()
    );
    if let Some(f) = folder_id {
        path.push_str(&format!("&folder={f}"));
    }
    if let Some(o) = offset {
        path.push_str(&format!("&offset={o}"));
    }
    Request::new(Method::Get, path)
}

reader!(
    /// [`search`]'s answer: one page of threads with their best hits.
    read_search -> SearchPage
);

/// `POST /chat/api/threads`: a new stored thread. In `folder_id` it starts
/// from the folder's defaults (which may name the model; `model_alias` is
/// then ignored); a folder out of the caller's reach is a `404`. Read with
/// [`read_created`].
pub fn create_thread(model_alias: &str, folder_id: Option<i64>) -> Request {
    let body = ThreadCreate {
        model_alias: model_alias.into(),
        folder_id,
        ..Default::default()
    };
    Request::new(Method::Post, "/chat/api/threads".into())
        .json(serde_json::to_string(&body).expect("a thread create always serializes"))
}

reader!(
    /// [`create_thread`]'s answer: the new thread.
    read_created -> Thread
);

/// `POST /chat/api/threads/{id}/pin`; pinning an archived thread also
/// restores it. Read with [`read_changed`].
pub fn pin(thread_id: i64, pinned: bool) -> Request {
    Request::new(Method::Post, format!("/chat/api/threads/{thread_id}/pin"))
        .json(serde_json::to_string(&PinRequest { pinned }).expect("always serializes"))
}

/// `POST /chat/api/threads/{id}/archive`, `archived: false` restores it.
/// Read with [`read_changed`].
pub fn archive(thread_id: i64, archived: bool) -> Request {
    Request::new(
        Method::Post,
        format!("/chat/api/threads/{thread_id}/archive"),
    )
    .json(serde_json::to_string(&ArchiveRequest { archived }).expect("always serializes"))
}

/// `POST /chat/api/threads/{id}/title`: rename the thread. The server trims
/// the title; empty after that is `400 bad_request`. Read with
/// [`read_changed`].
pub fn rename(thread_id: i64, title: &str) -> Request {
    Request::new(Method::Post, format!("/chat/api/threads/{thread_id}/title")).json(
        serde_json::to_string(&TitleRequest {
            title: title.into(),
        })
        .expect("always serializes"),
    )
}

/// `POST /chat/api/threads/{id}/move` into `folder_id`, `None` for out of
/// any folder. Read with [`read_changed`].
pub fn move_to_folder(thread_id: i64, folder_id: Option<i64>) -> Request {
    Request::new(Method::Post, format!("/chat/api/threads/{thread_id}/move"))
        .json(serde_json::to_string(&MoveRequest { folder_id }).expect("always serializes"))
}

reader!(
    /// The answer of [`pin`], [`archive`], [`rename`] and [`move_to_folder`]: the
    /// thread as it is now.
    read_changed -> Thread
);

/// `POST /chat/api/threads/{id}/delete`: the thread and its messages. The
/// owner's delete of a thread that is not there answers `ok` too; a
/// device's out of reach is a `404`. Read with [`read_deleted`].
pub fn delete(thread_id: i64) -> Request {
    Request::new(
        Method::Post,
        format!("/chat/api/threads/{thread_id}/delete"),
    )
}

reader!(
    /// [`delete`]'s answer.
    read_deleted -> Ack
);

#[cfg(test)]
mod tests {
    use super::*;

    fn body(r: &Request) -> serde_json::Value {
        serde_json::from_str(r.body.as_deref().unwrap()).unwrap()
    }

    #[test]
    fn the_archived_list_is_the_threads_route_with_the_switch() {
        let r = archived_threads();
        assert_eq!(r.method, Method::Get);
        assert_eq!(r.path, "/chat/api/threads?archived=1");
        assert!(r.body.is_none());
    }

    #[test]
    fn a_search_percent_encodes_the_words_and_adds_only_what_is_given() {
        let r = search("\"a b\" & ö", SearchScope::All, Some(3), Some(20));
        assert_eq!(r.method, Method::Get);
        assert_eq!(
            r.path,
            "/chat/api/search?q=%22a%20b%22%20%26%20%C3%B6&archived=all&folder=3&offset=20"
        );
        let r = search("hi", SearchScope::default(), None, None);
        assert_eq!(r.path, "/chat/api/search?q=hi&archived=0");
        assert!(search("x", SearchScope::Archived, None, None)
            .path
            .ends_with("&archived=1"));
    }

    #[test]
    fn a_search_page_reads_and_a_refusal_keeps_its_code() {
        let page = read_search(
            200,
            r#"{"total_threads":1,"threads":[{"thread_id":4,"title":"t","folder_id":null,
            "archived":false,"updated_at":"x","match_count":2,
            "hits":[{"kind":"m","message_id":9,"role":"user","snippet":"a\u000eb"}]}],
            "next_offset":null}"#,
        )
        .unwrap();
        assert_eq!(page.threads[0].hits[0].message_id, Some(9));
        assert_eq!(page.next_offset, None);
        let e = read_search(
            400,
            r#"{"code":"bad_request","message":"a search needs at least 2 characters"}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, "bad_request");
    }

    #[test]
    fn a_create_names_the_folder_only_when_given() {
        let r = create_thread("m", Some(2));
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/chat/api/threads");
        assert_eq!(body(&r)["folder_id"], 2);
        assert_eq!(body(&r)["model_alias"], "m");
        assert_eq!(body(&r)["temporary"], false);
        assert_eq!(
            body(&create_thread("m", None))["folder_id"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_created_thread_reads_as_a_thread() {
        let t = read_created(200, r#"{"id":12,"title":"New chat"}"#).unwrap();
        assert_eq!(t.row.id, 12);
        let e = read_created(404, r#"{"code":"not_found","message":"no folder"}"#).unwrap_err();
        assert_eq!(e.code, "not_found");
    }

    #[test]
    fn pin_archive_and_move_post_their_bodies() {
        let r = pin(5, true);
        assert_eq!(
            (r.method, r.path.as_str()),
            (Method::Post, "/chat/api/threads/5/pin")
        );
        assert_eq!(body(&r), serde_json::json!({"pinned": true}));
        let r = archive(5, false);
        assert_eq!(r.path, "/chat/api/threads/5/archive");
        assert_eq!(body(&r), serde_json::json!({"archived": false}));
        let r = move_to_folder(5, Some(8));
        assert_eq!(r.path, "/chat/api/threads/5/move");
        assert_eq!(body(&r), serde_json::json!({"folder_id": 8}));
        assert_eq!(
            body(&move_to_folder(5, None)),
            serde_json::json!({"folder_id": null})
        );
    }

    #[test]
    fn a_rename_posts_the_title_as_given() {
        let r = rename(5, "  Trip \"plans\" ");
        assert_eq!(
            (r.method, r.path.as_str()),
            (Method::Post, "/chat/api/threads/5/title")
        );
        assert_eq!(body(&r), serde_json::json!({"title": "  Trip \"plans\" "}));
        let t = read_changed(200, r#"{"id":5,"title":"Trip"}"#).unwrap();
        assert_eq!(t.row.title, "Trip");
        let e = read_changed(
            400,
            r#"{"code":"bad_request","message":"the title is empty after trimming"}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, "bad_request");
    }

    #[test]
    fn a_changed_thread_reads_and_a_temporary_one_is_refused() {
        assert_eq!(
            read_changed(200, r#"{"id":5,"pinned":true}"#)
                .unwrap()
                .row
                .id,
            5
        );
        let e = read_changed(
            409,
            r#"{"code":"temporary_thread","message":"a temporary chat cannot be pinned"}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, "temporary_thread");
    }

    #[test]
    fn a_delete_posts_no_body_and_reads_ok() {
        let r = delete(7);
        assert_eq!(
            (r.method, r.path.as_str()),
            (Method::Post, "/chat/api/threads/7/delete")
        );
        assert!(r.body.is_none());
        assert!(read_deleted(200, r#"{"ok":true}"#).unwrap().ok);
        assert_eq!(
            read_deleted(404, r#"{"code":"not_found","message":"x"}"#)
                .unwrap_err()
                .code,
            "not_found"
        );
    }
}
