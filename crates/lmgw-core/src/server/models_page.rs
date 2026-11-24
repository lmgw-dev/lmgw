//! Anthropic's paging of `GET /v1/models` (`limit`, `after_id`, `before_id`).
//!
//! Anthropic's list returns at most `limit` entries (default 20, 1 to 1000),
//! a page after or before a cursor id, `has_more` for the direction of travel
//! and the page's first and last id. The OpenAI shape has no paging and does
//! not use this module. The list's order is `exposed_entries`' name order,
//! the same on every call, which is what makes a cursor mean anything.

use axum::http::Uri;

use crate::error::GatewayError;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 1000;

// `pub(crate)` and `JsonSchema`-derived so the route's `DocRoute` turns it
// into the three query parameters instead of describing them a second time.
// `limit` is read as text so a bad value gets a message of our own.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ModelsPageQuery {
    /// Number of items to return per page. Defaults to 20, ranges from 1 to
    /// 1000. Anthropic shape only.
    #[serde(default)]
    #[schemars(with = "Option<u32>")]
    limit: Option<String>,
    /// Id to use as a cursor: returns the page immediately after it. Cannot
    /// be combined with `before_id`. Anthropic shape only.
    #[serde(default)]
    after_id: Option<String>,
    /// Id to use as a cursor: returns the page immediately before it. Cannot
    /// be combined with `after_id`. Anthropic shape only.
    #[serde(default)]
    before_id: Option<String>,
}

/// Which slice of the list one request asks for.
struct Window {
    limit: usize,
    after: Option<String>,
    before: Option<String>,
}

/// One answered page: the half-open range of the list and whether more
/// entries lie beyond it in the direction of travel.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Page {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) has_more: bool,
}

fn invalid(message: String) -> GatewayError {
    GatewayError::InvalidRequest {
        code: "invalid_request",
        message,
    }
}

fn window(uri: &Uri) -> Result<Window, GatewayError> {
    let q = axum::extract::Query::<ModelsPageQuery>::try_from_uri(uri)
        .map_err(|e| invalid(format!("the query string is not valid: {e}")))?
        .0;
    let limit = match q.limit.as_deref() {
        None => DEFAULT_LIMIT,
        Some(s) => s
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| {
                invalid(format!(
                    "limit must be an integer from 1 to {MAX_LIMIT}, got '{s}'"
                ))
            })?,
    };
    if q.after_id.is_some() && q.before_id.is_some() {
        return Err(invalid(
            "after_id and before_id cannot be used together".into(),
        ));
    }
    Ok(Window {
        limit,
        after: q.after_id,
        before: q.before_id,
    })
}

fn locate(ids: &[&str], cursor: &str, param: &str) -> Result<usize, GatewayError> {
    ids.iter()
        .position(|id| *id == cursor)
        .ok_or_else(|| invalid(format!("{param} '{cursor}' is not a model of this list")))
}

fn slice(ids: &[&str], w: &Window) -> Result<Page, GatewayError> {
    let len = ids.len();
    if let Some(c) = &w.after {
        let start = locate(ids, c, "after_id")? + 1;
        let end = (start + w.limit).min(len);
        return Ok(Page {
            start,
            end,
            has_more: end < len,
        });
    }
    if let Some(c) = &w.before {
        let end = locate(ids, c, "before_id")?;
        let start = end.saturating_sub(w.limit);
        return Ok(Page {
            start,
            end,
            has_more: start > 0,
        });
    }
    let end = w.limit.min(len);
    Ok(Page {
        start: 0,
        end,
        has_more: end < len,
    })
}

/// The page the request's query string asks for, out of `ids` in list order.
pub(super) fn page_of(uri: &Uri, ids: &[&str]) -> Result<Page, GatewayError> {
    slice(ids, &window(uri)?)
}

#[cfg(test)]
mod tests;
