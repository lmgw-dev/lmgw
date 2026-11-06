//! Hugging Face's model search and the finder's other reads (find-a-model
//! design §4.2, §4.3, D12, D16, D19, D124): the two searches with their
//! cursor, an original's uploads, a repo's tree and its listing fields, and
//! an author's display name. Every answer's `RateLimit` and
//! `RateLimit-Policy` are read on the way ([`Reads`]).
//!
//! Each listing is one page at the hub's own page size: no `limit` is sent,
//! so lmgw adds no bound of its own, and the hub's `Link` cursor is handed
//! back for the next page (D12, the WP4 call D70).

mod hub;

use serde::Deserialize;

pub use hub::{Budget, Fail, Hub, HubAnswer, LiveHub, Reads};

/// What the finder asks the search to expand (§4.2): the ranking's fields,
/// `private` (D124) and the parameter total (D15, D17).
pub const SEARCH_EXPAND: [&str; 9] = [
    "author",
    "downloads",
    "likes",
    "gated",
    "private",
    "baseModels",
    "pipeline_tag",
    "tags",
    "safetensors",
];

/// What an uploads listing expands: the order of D16 and D20, and `private`.
pub const UPLOAD_EXPAND: [&str; 6] = ["author", "downloads", "likes", "gated", "private", "tags"];

/// The tags the finder reads: `conversational` (D15's chat signal),
/// `pytorch` (weights of its own), `gguf` (a GGUF repo) and `license:…`
/// (D22). Every other tag is ignored, so the regression set's recording
/// leaves them out (D78); [`HubModel::has_tag`] asks only these.
pub fn tag_read(tag: &str) -> bool {
    matches!(tag, "conversational" | "pytorch" | "gguf") || tag.starts_with("license:")
}

/// One repo as the hub's model listing describes it, with the expansions
/// asked.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct HubModel {
    pub id: String,
    pub author: Option<String>,
    pub downloads: u64,
    pub likes: u64,
    /// `false`, or how access is granted (`"auto"`, `"manual"`).
    pub gated: serde_json::Value,
    pub private: bool,
    #[serde(rename = "baseModels")]
    pub base_models: Option<BaseModels>,
    pub pipeline_tag: Option<String>,
    pub tags: Vec<String>,
    pub safetensors: Option<Safetensors>,
}

/// The `baseModels` expansion: what a repo was made from, and how.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct BaseModels {
    /// `finetune`, `quantized`, `adapter`, `merge`.
    pub relation: Option<String>,
    pub models: Vec<BaseModel>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct BaseModel {
    pub id: String,
}

/// The `safetensors` expansion; only the total is read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct Safetensors {
    pub total: Option<u64>,
}

impl HubModel {
    /// The author: the expansion's, else the id's owner.
    pub fn author(&self) -> &str {
        self.author
            .as_deref()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| self.id.split('/').next().unwrap_or_default())
    }

    /// The repo's name, without its owner.
    pub fn name(&self) -> &str {
        self.id.split_once('/').map(|(_, n)| n).unwrap_or(&self.id)
    }

    pub fn gated(&self) -> bool {
        !matches!(
            self.gated,
            serde_json::Value::Bool(false) | serde_json::Value::Null
        )
    }

    /// Whether the hub tags it `tag`, one of the tags [`tag_read`] names.
    pub fn has_tag(&self, tag: &str) -> bool {
        debug_assert!(tag_read(tag), "{tag} isn't one of the tags tag_read names");
        self.tags.iter().any(|t| t == tag)
    }

    /// The parameter total the listing names (`safetensors.total`).
    pub fn parameters(&self) -> Option<u64> {
        self.safetensors
            .as_ref()
            .and_then(|s| s.total)
            .filter(|t| *t > 0)
    }

    /// Weights of its own (D15): a parameter total, or a `pytorch` repo
    /// (`BAAI/bge-m3` has only a `pytorch_model.bin`).
    pub fn own_weights(&self) -> bool {
        self.parameters().is_some() || self.has_tag("pytorch")
    }

    /// The base models and their relation.
    pub fn bases(&self) -> (Option<&str>, Vec<&str>) {
        match &self.base_models {
            Some(b) => (
                b.relation.as_deref(),
                b.models.iter().map(|m| m.id.as_str()).collect(),
            ),
            None => (None, Vec::new()),
        }
    }

    /// The licence the hub's tags name (`license:apache-2.0`).
    pub fn licence(&self) -> Option<&str> {
        self.tags.iter().find_map(|t| t.strip_prefix("license:"))
    }
}

/// `path` with `params` as its query, encoded as a URL encodes them.
fn api_path(path: &str, params: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://hub.invalid").expect("a fixed URL parses");
    url.set_path(path);
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    }
}

/// The search's path (§4.2): every repo, or GGUF repos only, by downloads,
/// at the hub's page size, from `cursor` when it continues.
pub fn search_path(query: &str, gguf: bool, cursor: Option<&str>) -> String {
    let mut params = vec![
        ("search", query),
        ("sort", "downloads"),
        ("direction", "-1"),
    ];
    if gguf {
        params.push(("filter", "gguf"));
    }
    params.extend(SEARCH_EXPAND.iter().map(|e| ("expand[]", *e)));
    if let Some(c) = cursor {
        params.push(("cursor", c));
    }
    api_path("/api/models", &params)
}

/// An original's GGUF uploads (§4.3 step 2), by likes.
pub fn uploads_path(origin: &str) -> String {
    let filter = format!("base_model:quantized:{origin}");
    let mut params = vec![
        ("filter", filter.as_str()),
        ("filter", "gguf"),
        ("sort", "likes"),
        ("direction", "-1"),
    ];
    params.extend(UPLOAD_EXPAND.iter().map(|e| ("expand[]", *e)));
    api_path("/api/models", &params)
}

/// One repo's listing fields, for an address or `owner/repo` (§4.1).
pub fn model_path(repo: &str) -> String {
    let params: Vec<(&str, &str)> = SEARCH_EXPAND.iter().map(|e| ("expand[]", *e)).collect();
    api_path(&format!("/api/models/{repo}"), &params)
}

/// The cursor of a `Link` header's next page, when it names one.
pub fn next_cursor(link: Option<&str>) -> Option<String> {
    let next = crate::hf::next_link(link?)?;
    let url = reqwest::Url::parse("http://hub.invalid/")
        .ok()?
        .join(&next)
        .ok()?;
    url.query_pairs()
        .find(|(k, _)| k == "cursor")
        .map(|(_, v)| v.into_owned())
}

/// The next page's path of a tree listing, from its `Link`: its path and
/// query only, so the request stays on the hub the reads go to.
fn next_tree_path(link: Option<&str>) -> Option<String> {
    let next = crate::hf::next_link(link?)?;
    let url = reqwest::Url::parse("http://hub.invalid/")
        .ok()?
        .join(&next)
        .ok()?;
    url.path()
        .starts_with("/api/models/")
        .then(|| match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_string(),
        })
}

fn parse<T: serde::de::DeserializeOwned>(answer: &HubAnswer, what: &str) -> Result<T, Fail> {
    serde_json::from_str(&answer.body)
        .map_err(|e| Fail::Unreachable(format!("Hugging Face's {what} couldn't be read: {e}")))
}

/// One page of both searches (D12).
#[derive(Debug, Clone, Default)]
pub struct SearchPage {
    pub all: Vec<HubModel>,
    pub gguf: Vec<HubModel>,
    /// Each search's next page, when it has one.
    pub next_all: Option<String>,
    pub next_gguf: Option<String>,
}

/// A continuation of the two searches: each one's cursor, `-` for a search
/// with no page left. Opaque to callers.
pub fn encode_cursor(all: Option<&str>, gguf: Option<&str>) -> Option<String> {
    (all.is_some() || gguf.is_some())
        .then(|| format!("{} {}", all.unwrap_or("-"), gguf.unwrap_or("-")))
}

/// The two cursors of [`encode_cursor`]'s string; `None` when it isn't one.
pub fn decode_cursor(cursor: &str) -> Option<(Option<String>, Option<String>)> {
    let (a, g) = cursor.trim().split_once(' ')?;
    let part = |s: &str| (s != "-" && !s.is_empty()).then(|| s.to_string());
    let (a, g) = (part(a), part(g));
    (a.is_some() || g.is_some()).then_some((a, g))
}

/// The two searches for `query` (D12), or their next pages when `cursor`
/// continues them. A search whose pages ran out isn't asked again.
pub async fn search<H: Hub>(
    reads: &mut Reads<'_, H>,
    query: &str,
    cursor: Option<(Option<String>, Option<String>)>,
) -> Result<SearchPage, Fail> {
    let (ask_all, ask_gguf, all_cursor, gguf_cursor) = match &cursor {
        None => (true, true, None, None),
        Some((a, g)) => (a.is_some(), g.is_some(), a.as_deref(), g.as_deref()),
    };
    let mut page = SearchPage::default();
    if ask_all {
        let answer = reads.get(&search_path(query, false, all_cursor)).await?;
        page.all = parse(&answer, "search answer")?;
        page.next_all = next_cursor(answer.link.as_deref());
    }
    if ask_gguf {
        let answer = reads.get(&search_path(query, true, gguf_cursor)).await?;
        page.gguf = parse(&answer, "search answer")?;
        page.next_gguf = next_cursor(answer.link.as_deref());
    }
    Ok(page)
}

/// An original's GGUF uploads, the hub's first page by likes, and whether
/// the hub has more.
pub async fn uploads<H: Hub>(
    reads: &mut Reads<'_, H>,
    origin: &str,
) -> Result<(Vec<HubModel>, bool), Fail> {
    let answer = reads.get(&uploads_path(origin)).await?;
    let more = next_cursor(answer.link.as_deref()).is_some();
    Ok((parse(&answer, "uploads listing")?, more))
}

/// One repo's listing fields; `None` when the hub has no such repo (or
/// doesn't show it without a token).
pub async fn model<H: Hub>(reads: &mut Reads<'_, H>, repo: &str) -> Result<Option<HubModel>, Fail> {
    crate::hf::validate_repo(repo).map_err(Fail::Unreachable)?;
    match reads.get(&model_path(repo)).await {
        Ok(answer) => parse(&answer, "model answer").map(Some),
        Err(Fail::Status {
            status: 401 | 404, ..
        }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A repo's files at `main`, every page of the listing (as
/// [`crate::hf::list_tree`] reads them, through the finder's reads).
pub async fn tree<H: Hub>(
    reads: &mut Reads<'_, H>,
    repo: &str,
) -> Result<Vec<crate::hf::HfFile>, Fail> {
    crate::hf::validate_repo(repo).map_err(Fail::Unreachable)?;
    let mut path = format!("/api/models/{repo}/tree/main?recursive=true");
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    loop {
        let answer = reads.get(&path).await?;
        out.extend(crate::hf::parse_tree_json(&answer.body).map_err(Fail::Unreachable)?);
        seen.insert(path);
        match next_tree_path(answer.link.as_deref()) {
            Some(next) if !seen.contains(&next) => path = next,
            Some(next) => {
                return Err(Fail::Unreachable(format!(
                    "the tree listing of {repo} pages in a loop ({next})"
                )))
            }
            None => return Ok(out),
        }
    }
}

#[derive(Deserialize)]
struct Overview {
    #[serde(default)]
    fullname: Option<String>,
}

/// An author's account on the hub, as its overview says it (D19).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    /// Its own display name (`fullname`); `None` when it names none.
    pub name: Option<String>,
    /// A person's account, not an organisation's: its name is self-chosen,
    /// so a card shows the account's id beside it.
    pub user: bool,
}

/// The author's own display name on the hub (D19): an organisation's, else
/// a user's `fullname`, and which of the two the account is.
pub async fn display_name<H: Hub>(reads: &mut Reads<'_, H>, author: &str) -> Result<Account, Fail> {
    for (kind, user) in [("organizations", false), ("users", true)] {
        match reads.get(&format!("/api/{kind}/{author}/overview")).await {
            Ok(answer) => {
                let o: Overview = parse(&answer, "overview")?;
                return Ok(Account {
                    name: o.fullname.filter(|f| !f.trim().is_empty()),
                    user,
                });
            }
            Err(Fail::Status { status: 404, .. }) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(Account::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_search_asks_the_hubs_page_and_its_expansions() {
        let p = search_path("qwen 3.6", true, None);
        assert!(
            p.starts_with("/api/models?search=qwen+3.6&sort=downloads&direction=-1&filter=gguf"),
            "{p}"
        );
        assert!(p.contains("expand%5B%5D=safetensors"), "{p}");
        assert!(p.contains("expand%5B%5D=private"), "{p}");
        assert!(!p.contains("limit"), "{p}");
        let u = uploads_path("Qwen/Qwen3.6-27B");
        assert!(
            u.starts_with(
                "/api/models?filter=base_model%3Aquantized%3AQwen%2FQwen3.6-27B&filter=gguf&sort=likes"
            ),
            "{u}"
        );
    }

    #[test]
    fn a_cursor_round_trips_and_comes_from_the_link() {
        let link = "<https://huggingface.co/api/models?search=x&cursor=eyJ9%3D>; rel=\"next\"";
        assert_eq!(next_cursor(Some(link)).as_deref(), Some("eyJ9="));
        let c = encode_cursor(Some("eyJ9="), None).unwrap();
        assert_eq!(decode_cursor(&c), Some((Some("eyJ9=".into()), None)));
        assert_eq!(encode_cursor(None, None), None);
        assert_eq!(decode_cursor("- -"), None);
        assert_eq!(decode_cursor("nonsense"), None);
    }

    #[test]
    fn a_tree_page_link_stays_on_the_hub_by_path() {
        let link = "<https://evil.example/api/models/a/b/tree/main?cursor=2>; rel=\"next\"";
        assert_eq!(
            next_tree_path(Some(link)).as_deref(),
            Some("/api/models/a/b/tree/main?cursor=2")
        );
        assert_eq!(next_tree_path(Some("</other>; rel=\"next\"")), None);
    }
}
