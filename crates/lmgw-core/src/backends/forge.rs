//! The forge clients behind the Backends extras picker (container-builds
//! design §7, §15 `forge_prs` / `forge_pr` / `forge_refs`): a repository's
//! open PRs (GitHub) or MRs (GitLab), one PR's current state — what the run
//! executor and the update check read `head_sha`, `base_sha` and `merged_at`
//! from (§5 step 1, §8, §14.1) — and the branches and tags for the ref
//! combobox.
//!
//! # Tokens (§7, §10)
//!
//! A token is looked up by the repository's host — the web host its API
//! lives on, which is the `forge_tokens` key ([`token_host`], the one host
//! function every lookup uses, API calls and git alike) — and goes only to
//! that host: a github.com token only to the GitHub API base
//! ([`GITHUB_API`]), a GitLab token only to that GitLab instance. It travels
//! as an `Authorization: Bearer` header marked sensitive (or git's
//! `extraHeader`, [`git_auth`]), never in a URL, never in a message or a log
//! line. Redirects are followed only within the origin they started at, so a
//! redirect cannot carry it anywhere else either. It never goes over plain
//! `http://` to another machine ([`refuse_plain_http`]): the call fails and
//! says so instead.
//!
//! # Rate limits
//!
//! Unauthenticated GitHub allows 60 requests an hour, so a page is one
//! request of [`PAGE_SIZE`] PRs (the APIs' own maximum) and never a detail
//! call per PR. Every page reports the remaining quota; an exhausted quota
//! is an error that says when it resets, which the picker shows instead of
//! an empty list.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use lmgw_api_types::builds::{ForgePr, ForgePrPage, RateLimit, RefEntry, RemoteRefsView};
use reqwest::header::{HeaderMap, ACCEPT, LINK, LOCATION};
use reqwest::{StatusCode, Url};
use serde::Deserialize;

use super::git::{Git, GitAuth, RemoteRefs};
use super::model::Forge;
use super::presets;
use super::validate;
use crate::state::{AppState, SharedState};

/// Where github.com's API is. `LMGW_GITHUB_API` overrides it (a mock in
/// tests), like `HF_ENDPOINT` does for the hub — and the github.com token
/// then goes there.
pub const GITHUB_API: &str = "https://api.github.com";

/// The host whose API is not on the host itself.
pub const GITHUB_HOST: &str = "github.com";

/// PRs per page: the maximum both GitHub and GitLab serve. More are paged
/// through `next_page`, never dropped.
pub const PAGE_SIZE: u32 = 100;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// reqwest's own default; a longer chain is an error that says so.
const MAX_REDIRECTS: usize = 10;

/// api.github.com refuses requests without a User-Agent.
const USER_AGENT: &str = concat!("lmgw/", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// Repository URLs
// ---------------------------------------------------------------------------

/// A repository as a forge API addresses it: the web origin and the project
/// path. Parsed from any URL form a build accepts (§10) — `https://`,
/// `http://`, `git@host:path`, `ssh://` — except `file://`, which has no
/// forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeRepo {
    /// `https`, or `http` for a plain-HTTP instance. An SSH remote's API is
    /// reached over `https`.
    pub scheme: String,
    /// `host[:port]` of the web UI and the API, lowercased: the
    /// `forge_tokens` key. An `ssh://host:2222/…` port is the SSH daemon's,
    /// not the web server's, so it is dropped.
    pub host: String,
    /// `owner/repo` or `group/sub/project`, without `.git`.
    pub path: String,
}

impl ForgeRepo {
    pub fn parse(repo_url: &str) -> Result<Self, String> {
        let url = repo_url.trim();
        if url.starts_with("file://") {
            return Err(format!(
                "'{url}' is a local repository — it has no forge to list PRs from; add refs \
                 from other remotes instead"
            ));
        }
        validate::validate_repo_url("repository URL", url)?;
        let web = presets::web_url(url);
        let parsed =
            Url::parse(&web).map_err(|e| format!("repository URL '{url}' does not parse: {e}"))?;
        let scheme = parsed.scheme();
        if scheme != "https" && scheme != "http" {
            return Err(format!(
                "repository URL '{url}' has no web address a forge API could be reached at"
            ));
        }
        let host =
            host_key(&parsed).ok_or_else(|| format!("repository URL '{url}' names no host"))?;
        let path = parsed.path().trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        if path.is_empty() {
            return Err(format!(
                "repository URL '{url}' names no repository — expected \
                 {scheme}://{host}/<owner>/<repository>"
            ));
        }
        if path.split('/').any(|s| s == "-") {
            return Err(format!(
                "'{url}' points at a page inside a project, not at the project — use \
                 {scheme}://{host}/<group>/<project>"
            ));
        }
        if let Some(bad) = path.split('/').find(|s| !valid_segment(s)) {
            return Err(format!(
                "repository URL '{url}' has a path segment '{bad}' no forge project path \
                 contains (letters, digits, '.', '-', '_')"
            ));
        }
        Ok(Self {
            scheme: scheme.to_string(),
            host,
            path: path.to_string(),
        })
    }

    /// `https://host/owner/repo` — what a PR link of this repository starts
    /// with.
    pub fn web_url(&self) -> String {
        format!("{}://{}/{}", self.scheme, self.host, self.path)
    }

    /// The same project: host and path compared case-insensitively, as both
    /// forges route them.
    pub fn same_repo(&self, other: &Self) -> bool {
        self.host == other.host && self.path.eq_ignore_ascii_case(&other.path)
    }

    /// `(owner, repo)` — a GitHub repository is exactly two segments.
    fn github_owner_repo(&self) -> Result<(&str, &str), String> {
        match self.path.split_once('/') {
            Some((owner, name)) if !name.contains('/') => Ok((owner, name)),
            _ => Err(format!(
                "'{}' is not a GitHub repository — expected {}://{}/<owner>/<repository>",
                self.web_url(),
                self.scheme,
                self.host
            )),
        }
    }
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// A pasted PR or MR page — `https://github.com/o/r/pull/123/files`,
/// `https://host/group/proj/-/merge_requests/45` — as its repository and
/// number. `None` for anything else.
pub fn parse_pr_url(s: &str) -> Option<(ForgeRepo, u64)> {
    let u = Url::parse(s.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    let (repo_segs, number) = segs.iter().enumerate().find_map(|(i, s)| {
        if !matches!(*s, "pull" | "pulls" | "merge_requests") || i < 2 {
            return None;
        }
        let number = segs.get(i + 1)?.parse::<u64>().ok()?;
        let end = if *s == "merge_requests" && segs[i - 1] == "-" {
            i - 1
        } else {
            i
        };
        Some((&segs[..end], number))
    })?;
    if repo_segs.len() < 2 || number == 0 {
        return None;
    }
    let host = host_key(&u)?;
    let path = repo_segs.join("/");
    let path = path.strip_suffix(".git").unwrap_or(&path).to_string();
    Some((
        ForgeRepo {
            scheme: u.scheme().to_string(),
            host,
            path,
        },
        number,
    ))
}

/// The forge a repository's PRs come from: github.com is always GitHub
/// ([`validate::default_forge`]); any other host is what the build chose.
pub fn effective_forge(repo_url: &str, chosen: Forge) -> Forge {
    match validate::default_forge(repo_url, |_| false) {
        Forge::Github => Forge::Github,
        _ => chosen,
    }
}

/// `host[:port]` of a web URL, lowercased, with the port only when it is not
/// the scheme's default (`Url::port` drops `:443` on `https`, `:80` on
/// `http`).
fn host_key(u: &Url) -> Option<String> {
    let host = u.host_str().filter(|h| !h.is_empty())?.to_ascii_lowercase();
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    })
}

/// The `forge_tokens` key of a repository URL — the **one** host function
/// every token lookup uses: the forge API calls ([`token_for`]), git's
/// `extraHeader` ([`git_auth`]) and the update check alike, so a token is
/// found (or not) the same way whichever of them asks.
///
/// It is the host of the repository's web address ([`ForgeRepo::host`]):
/// lowercased; a port only when it is not the scheme's default
/// (`https://git.example:443/…` is `git.example`, `https://git.example:8443/…`
/// is `git.example:8443`); an SSH remote's port dropped, because it is the
/// SSH daemon's, not the web server's (`ssh://git@host:2222/…` and
/// `git@host:…` are `host`). `None` for `file://` and for anything
/// [`validate::validate_repo_url`] refuses.
pub fn token_host(repo_url: &str) -> Option<String> {
    let url = repo_url.trim();
    validate::validate_repo_url("repository URL", url).ok()?;
    if url.starts_with("file://") {
        return None;
    }
    let parsed = Url::parse(&presets::web_url(url)).ok()?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return None;
    }
    host_key(&parsed)
}

/// The token for `repo_url` from the `forge_tokens` setting: the entry for
/// the repository's own host ([`token_host`]), if it is set.
pub fn token_for<'a>(tokens: &'a BTreeMap<String, String>, repo_url: &str) -> Option<&'a str> {
    let host = token_host(repo_url)?;
    tokens
        .get(&host)
        .map(String::as_str)
        .filter(|t| !t.is_empty())
}

/// Whether `url` is plain `http://` to another machine — where a token would
/// cross the network readable by anyone on the path. Plain `http://` to this
/// machine (`localhost`, `127.0.0.0/8`, `::1`) never leaves it, which is what
/// the tests' mock forges are.
fn plain_http_elsewhere(url: &Url) -> bool {
    if url.scheme() != "http" {
        return false;
    }
    let host = url.host_str().unwrap_or("");
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    !loopback
}

/// Refuse to send a forge token to `url` over plain `http://` (anything but
/// this machine, [`plain_http_elsewhere`]) — the visible error instead of a
/// token on the wire in clear text. A URL that does not parse is left to the
/// call itself to refuse.
pub fn refuse_plain_http(url: &str) -> Result<(), String> {
    match Url::parse(url.trim()) {
        Ok(u) if plain_http_elsewhere(&u) => Err(format!(
            "refusing to send the forge token over plain http to {}; use https",
            host_key(&u).unwrap_or_else(|| url.to_string())
        )),
        _ => Ok(()),
    }
}

/// git's credentials for `url` from the `forge_tokens` setting (§7
/// "Tokens"): the token of its [`token_host`], as the `extraHeader`
/// [`GitAuth`] scopes to that host. `Ok(None)` without a token, and for a
/// remote git does not reach over HTTP(S) (an SSH remote other than
/// github.com, `file://`), where a header means nothing. An `http://`
/// remote with a token is refused ([`refuse_plain_http`]) rather than sent
/// the token in clear text.
pub fn git_auth(
    tokens: &BTreeMap<String, String>,
    url: &str,
    forge: Forge,
) -> Result<Option<GitAuth>, String> {
    git_auth_with(url, token_for(tokens, url), forge)
}

/// [`git_auth`] with the token already looked up.
fn git_auth_with(url: &str, token: Option<&str>, forge: Forge) -> Result<Option<GitAuth>, String> {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let host = token_host(url).unwrap_or_else(|| url.to_string());
    validate::validate_forge_token(&host, token)?;
    let Some(auth) = GitAuth::new(url, token, forge) else {
        return Ok(None);
    };
    refuse_plain_http(url)?;
    Ok(Some(auth))
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// GitHub and GitLab REST calls plus `git ls-remote`. The token is the
/// caller's to find ([`token_for`], or [`ForgeSession`], which does it);
/// the client sends it only to the API of the repository's own host.
#[derive(Debug, Clone)]
pub struct ForgeClient {
    http: reqwest::Client,
    github_api: String,
    git: Git,
}

/// Which API a call goes to.
struct Api {
    forge: Forge,
    name: &'static str,
    base: String,
}

/// A successful API response.
struct Fetched {
    body: String,
    headers: HeaderMap,
    quota: Quota,
}

impl ForgeClient {
    /// A client for the real forges (`LMGW_GITHUB_API` overrides
    /// [`GITHUB_API`]). Its own HTTP client: redirects stay on the origin
    /// they started at, so a token never follows one to another host.
    pub fn new() -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(USER_AGENT)
            .redirect(same_origin_redirects())
            .build()
            .map_err(|e| format!("building the forge HTTP client: {e}"))?;
        let github_api = std::env::var("LMGW_GITHUB_API")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| GITHUB_API.to_string());
        Ok(Self {
            http,
            github_api: github_api.trim_end_matches('/').to_string(),
            git: Git::new(),
        })
    }

    /// Point github.com's API somewhere else — a mock server in tests.
    pub fn with_github_api(mut self, base: impl Into<String>) -> Self {
        self.github_api = base.into().trim_end_matches('/').to_string();
        self
    }

    /// The git [`Self::refs`] runs.
    pub fn with_git(mut self, git: Git) -> Self {
        self.git = git;
        self
    }

    pub fn github_api(&self) -> &str {
        &self.github_api
    }

    /// One page of the repository's open PRs/MRs, most recently updated
    /// first (§7).
    ///
    /// - A `query` that is a number (`123`, `#123`, `!45`) or a pasted PR/MR
    ///   URL of this repository resolves that one PR directly, whatever its
    ///   state — adding by number always works. A URL of another repository
    ///   is an error: a PR extra is always of the build's own repository.
    /// - Any other `query` filters: GitHub client-side over the page (every
    ///   word must appear in the number, title or author, case-insensitive),
    ///   GitLab server-side (`search=`, title and description). A filtered
    ///   page may come back empty with a `next_page` — the next hundred may
    ///   match.
    /// - `page` starts at 1.
    pub async fn list_prs(
        &self,
        repo_url: &str,
        forge: Forge,
        query: Option<&str>,
        page: Option<u32>,
        token: Option<&str>,
    ) -> Result<ForgePrPage, String> {
        let repo = ForgeRepo::parse(repo_url)?;
        let api = self.api(&repo, effective_forge(repo_url, forge))?;
        let query = query.map(str::trim).filter(|q| !q.is_empty());
        if let Some(number) = query
            .map(|q| direct_number(q, &repo))
            .transpose()?
            .flatten()
        {
            let (pr, rate_limit) = self.fetch_pr(&api, &repo, number, token).await?;
            return Ok(ForgePrPage {
                prs: vec![pr],
                next_page: None,
                rate_limit,
            });
        }
        let page = page.unwrap_or(1).max(1);
        match api.forge {
            Forge::Github => self.github_prs(&api, &repo, query, page, token).await,
            _ => self.gitlab_mrs(&api, &repo, query, page, token).await,
        }
    }

    /// One PR/MR's current state (§5 step 1, §8): head and base SHA, draft,
    /// `merged_at`, and `state` normalized to `open` | `closed` | `merged`
    /// on both forges (a merged GitHub PR is `closed` there).
    pub async fn get_pr(
        &self,
        repo_url: &str,
        forge: Forge,
        number: u64,
        token: Option<&str>,
    ) -> Result<ForgePr, String> {
        let repo = ForgeRepo::parse(repo_url)?;
        let api = self.api(&repo, effective_forge(repo_url, forge))?;
        Ok(self.fetch_pr(&api, &repo, number, token).await?.0)
    }

    /// Branches and tags for the ref combobox (§4 "ref", §15 `forge_refs`):
    /// `git ls-remote`, so it works on every remote, forge or not. The
    /// default branch comes first among the heads; tags are newest first by
    /// the numbers in their names ([`version_cmp`]) and peeled to their
    /// commit.
    pub async fn refs(
        &self,
        repo_url: &str,
        token: Option<&str>,
    ) -> Result<RemoteRefsView, String> {
        let auth = git_auth_with(repo_url, token, effective_forge(repo_url, Forge::Plain))?;
        let refs = self.git.ls_remote(repo_url, auth.as_ref()).await?;
        Ok(refs_view(refs))
    }

    fn api(&self, repo: &ForgeRepo, forge: Forge) -> Result<Api, String> {
        match forge {
            Forge::Github => Ok(Api {
                forge,
                name: "GitHub",
                // GitHub Enterprise serves the same API under /api/v3 of its
                // own host.
                base: if repo.host == GITHUB_HOST {
                    self.github_api.clone()
                } else {
                    format!("{}://{}/api/v3", repo.scheme, repo.host)
                },
            }),
            Forge::Gitlab => Ok(Api {
                forge,
                name: "GitLab",
                base: format!("{}://{}/api/v4", repo.scheme, repo.host),
            }),
            Forge::Plain => Err(format!(
                "{} is a plain git repository — there is no PR list; choose github or gitlab \
                 as its forge, or add refs from other remotes",
                repo.web_url()
            )),
        }
    }

    async fn github_prs(
        &self,
        api: &Api,
        repo: &ForgeRepo,
        query: Option<&str>,
        page: u32,
        token: Option<&str>,
    ) -> Result<ForgePrPage, String> {
        let (owner, name) = repo.github_owner_repo()?;
        let mut url = endpoint(&api.base, &["repos", owner, name, "pulls"])?;
        url.query_pairs_mut()
            .append_pair("state", "open")
            .append_pair("sort", "updated")
            .append_pair("direction", "desc")
            .append_pair("per_page", &PAGE_SIZE.to_string())
            .append_pair("page", &page.to_string());
        let what = format!("open PRs of {}", repo.path);
        let got = self.get(api, repo, url, token, &what).await?;
        let pulls: Vec<GhPull> = parse_json(&got.body, api, &what)?;
        let prs = pulls
            .into_iter()
            .map(GhPull::into_pr)
            .filter(|pr| query.is_none_or(|q| matches_query(pr, q)))
            .collect();
        Ok(ForgePrPage {
            prs,
            next_page: next_page(&got.headers),
            rate_limit: got.quota.view(),
        })
    }

    async fn gitlab_mrs(
        &self,
        api: &Api,
        repo: &ForgeRepo,
        query: Option<&str>,
        page: u32,
        token: Option<&str>,
    ) -> Result<ForgePrPage, String> {
        let mut url = endpoint(&api.base, &["projects", &repo.path, "merge_requests"])?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("state", "opened")
                .append_pair("order_by", "updated_at")
                .append_pair("sort", "desc")
                .append_pair("per_page", &PAGE_SIZE.to_string())
                .append_pair("page", &page.to_string());
            if let Some(search) = query {
                q.append_pair("search", search);
            }
        }
        let what = format!("open MRs of {}", repo.path);
        let got = self.get(api, repo, url, token, &what).await?;
        let mrs: Vec<GlMr> = parse_json(&got.body, api, &what)?;
        Ok(ForgePrPage {
            prs: mrs.into_iter().map(GlMr::into_pr).collect(),
            next_page: next_page(&got.headers),
            rate_limit: got.quota.view(),
        })
    }

    async fn fetch_pr(
        &self,
        api: &Api,
        repo: &ForgeRepo,
        number: u64,
        token: Option<&str>,
    ) -> Result<(ForgePr, Option<RateLimit>), String> {
        let n = number.to_string();
        if api.forge == Forge::Github {
            let (owner, name) = repo.github_owner_repo()?;
            let url = endpoint(&api.base, &["repos", owner, name, "pulls", &n])?;
            let what = format!("PR #{number} of {}", repo.path);
            let got = self.get(api, repo, url, token, &what).await?;
            let pull: GhPull = parse_json(&got.body, api, &what)?;
            Ok((pull.into_pr(), got.quota.view()))
        } else {
            let url = endpoint(&api.base, &["projects", &repo.path, "merge_requests", &n])?;
            let what = format!("MR !{number} of {}", repo.path);
            let got = self.get(api, repo, url, token, &what).await?;
            let mr: GlMr = parse_json(&got.body, api, &what)?;
            Ok((mr.into_pr(), got.quota.view()))
        }
    }

    /// `GET url`, with the token when there is one — `url` is always on the
    /// repository's own API ([`Self::api`]), which is the whole of the token
    /// scoping.
    async fn get(
        &self,
        api: &Api,
        repo: &ForgeRepo,
        url: Url,
        token: Option<&str>,
        what: &str,
    ) -> Result<Fetched, String> {
        let token = token.filter(|t| !t.is_empty());
        if let Some(t) = token {
            validate::validate_forge_token(&repo.host, t)?;
            refuse_plain_http(url.as_str()).map_err(|e| format!("{what}: {e}"))?;
        }
        let mut rb = self.http.get(url).timeout(REQUEST_TIMEOUT);
        if api.forge == Forge::Github {
            rb = rb
                .header(ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
        }
        if let Some(t) = token {
            rb = rb.bearer_auth(t);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| format!("{what}: {} did not answer: {e}", api.name))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("{what}: reading {}'s answer: {e}", api.name))?;
        let quota = Quota::from_headers(&headers);
        if status.is_success() {
            return Ok(Fetched {
                body,
                headers,
                quota,
            });
        }
        Err(api_error(
            api,
            repo,
            what,
            status,
            &headers,
            &body,
            &quota,
            token.is_some(),
            chrono::Utc::now(),
        ))
    }
}

/// Follow a redirect only to the origin the request started at — GitHub's
/// renamed-repository redirects stay on api.github.com. Anything else stops,
/// and the 3xx becomes a visible error ([`api_error`]).
fn same_origin_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let Some(first) = attempt.previous().first() else {
            return attempt.follow();
        };
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error(format!("more than {MAX_REDIRECTS} redirects"));
        }
        let next = attempt.url();
        let same = next.scheme() == first.scheme()
            && next.host_str() == first.host_str()
            && next.port_or_known_default() == first.port_or_known_default();
        if same {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// `base` + path segments, each percent-encoded as one segment — a GitLab
/// project path's `/` becomes the `%2F` its API wants.
fn endpoint(base: &str, segments: &[&str]) -> Result<Url, String> {
    let mut url = Url::parse(base).map_err(|e| format!("forge API base '{base}': {e}"))?;
    url.path_segments_mut()
        .map_err(|_| format!("forge API base '{base}' cannot carry a path"))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

fn parse_json<T: serde::de::DeserializeOwned>(
    body: &str,
    api: &Api,
    what: &str,
) -> Result<T, String> {
    serde_json::from_str(body).map_err(|e| {
        format!(
            "{what}: {} answered something that is not the expected JSON: {e}",
            api.name
        )
    })
}

/// A number (`123`, `#123`, `!45`) or a PR/MR URL of `repo` in the picker's
/// search box: the PR to resolve directly.
fn direct_number(q: &str, repo: &ForgeRepo) -> Result<Option<u64>, String> {
    let bare = q
        .strip_prefix('#')
        .or_else(|| q.strip_prefix('!'))
        .unwrap_or(q);
    if !bare.is_empty() && bare.chars().all(|c| c.is_ascii_digit()) {
        let n: u64 = bare
            .parse()
            .map_err(|_| format!("'{q}' is too large for a PR number"))?;
        if n == 0 {
            return Err("there is no PR #0 — numbers start at 1".into());
        }
        return Ok(Some(n));
    }
    if !(q.starts_with("https://") || q.starts_with("http://")) {
        return Ok(None);
    }
    let Some((other, n)) = parse_pr_url(q) else {
        return Err(format!(
            "'{q}' is not a pull request or merge request URL — paste …/pull/<n> (GitHub) or \
             …/-/merge_requests/<n> (GitLab), or type the number"
        ));
    };
    if !other.same_repo(repo) {
        return Err(format!(
            "{q} belongs to {}, not to this build's repository {} — a PR extra is always of the \
             build's own repository; add it as a ref from another remote instead \
             (refs/pull/{n}/head on GitHub, refs/merge-requests/{n}/head on GitLab)",
            other.web_url(),
            repo.web_url()
        ));
    }
    Ok(Some(n))
}

/// Every word of `q` appears in `#number title author`, case-insensitively.
fn matches_query(pr: &ForgePr, q: &str) -> bool {
    let hay = format!("#{} {} {}", pr.number, pr.title, pr.author).to_lowercase();
    q.to_lowercase().split_whitespace().all(|w| hay.contains(w))
}

/// The next page: GitLab's `x-next-page`, else the `rel="next"` entry of the
/// `Link` header (GitHub, and GitLab where it omits `x-next-page`).
fn next_page(headers: &HeaderMap) -> Option<u32> {
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    header("x-next-page")
        .and_then(|v| v.trim().parse().ok())
        .or_else(|| header(LINK.as_str()).and_then(next_page_from_link))
}

fn next_page_from_link(link: &str) -> Option<u32> {
    link.split(',').find_map(|entry| {
        let (target, params) = entry.split_once(';')?;
        let is_next = params.split(';').any(|p| {
            p.trim().strip_prefix("rel=").is_some_and(|rel| {
                rel.trim_matches('"')
                    .split_whitespace()
                    .any(|r| r == "next")
            })
        });
        if !is_next {
            return None;
        }
        let target = target.trim().trim_start_matches('<').trim_end_matches('>');
        let url = Url::parse(target).ok()?;
        let page = url.query_pairs().find(|(k, _)| k == "page")?.1;
        page.parse().ok()
    })
}

// ---------------------------------------------------------------------------
// Rate limits and errors
// ---------------------------------------------------------------------------

/// What a response says about the quota: GitHub's `x-ratelimit-*`, GitLab's
/// `ratelimit-*`, and `retry-after` (GitHub's secondary limit, GitLab 429s).
#[derive(Debug, Clone, Copy, Default)]
struct Quota {
    remaining: Option<u32>,
    limit: Option<u32>,
    /// Unix seconds.
    reset: Option<i64>,
    retry_after: Option<i64>,
}

impl Quota {
    fn from_headers(h: &HeaderMap) -> Self {
        fn num<T: std::str::FromStr>(h: &HeaderMap, names: &[&str]) -> Option<T> {
            names.iter().find_map(|n| {
                h.get(*n)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse().ok())
            })
        }
        Self {
            remaining: num(h, &["x-ratelimit-remaining", "ratelimit-remaining"]),
            limit: num(h, &["x-ratelimit-limit", "ratelimit-limit"]),
            reset: num(h, &["x-ratelimit-reset", "ratelimit-reset"]),
            retry_after: num(h, &["retry-after"]),
        }
    }

    /// The wire shape; `None` when the forge sends no quota headers (a
    /// self-hosted GitLab without rate limiting).
    fn view(&self) -> Option<RateLimit> {
        Some(RateLimit {
            remaining: self.remaining?,
            reset_at: self
                .reset
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_default(),
        })
    }

    fn exhausted(&self, status: StatusCode) -> bool {
        status == StatusCode::TOO_MANY_REQUESTS
            || (status == StatusCode::FORBIDDEN
                && (self.remaining == Some(0) || self.retry_after.is_some()))
    }
}

/// `14:32 (in 23 min)` in local time, with the date when it is not today.
pub fn reset_phrase(reset_unix: i64, now: chrono::DateTime<chrono::Utc>) -> String {
    let Some(at) = chrono::DateTime::from_timestamp(reset_unix, 0) else {
        return format!("Unix time {reset_unix}");
    };
    let local = at.with_timezone(&chrono::Local);
    let today = now.with_timezone(&chrono::Local).date_naive();
    let clock = if local.date_naive() == today {
        local.format("%H:%M").to_string()
    } else {
        local.format("%Y-%m-%d %H:%M").to_string()
    };
    // Whole seconds on both sides: the reset is a whole second, `now` is not.
    let secs = reset_unix - now.timestamp();
    let rel = match secs {
        s if s <= 0 => "now".to_string(),
        s if s < 60 => format!("in {s} s"),
        s => format!("in {} min", (s + 59) / 60),
    };
    format!("{clock} ({rel})")
}

#[allow(clippy::too_many_arguments)]
fn api_error(
    api: &Api,
    repo: &ForgeRepo,
    what: &str,
    status: StatusCode,
    headers: &HeaderMap,
    body: &str,
    quota: &Quota,
    token_sent: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let (name, host, code) = (api.name, &repo.host, status.as_u16());
    if quota.exhausted(status) {
        let used = match quota.limit {
            Some(l) => format!(" (all {l} requests used)"),
            None => String::new(),
        };
        let reset = quota
            .reset
            .or_else(|| quota.retry_after.map(|s| now.timestamp() + s));
        let when = match reset {
            Some(t) => format!(" — it resets at {}", reset_phrase(t, now)),
            None => " — the forge did not say when it resets".to_string(),
        };
        let hint = if token_sent {
            String::new()
        } else {
            format!(
                ". Requests without a token get the lowest limit (GitHub: 60 an hour); a forge \
                 token for {host} (Settings) raises it"
            )
        };
        return format!("{what}: {name} API rate limit reached{used}{when}{hint}");
    }
    let said = match forge_message(body) {
        m if m.is_empty() => String::new(),
        m => format!(": {m}"),
    };
    if status.is_redirection() {
        let to = headers
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("?");
        return format!(
            "{what}: {name} answered {code}, redirecting to {to} — lmgw follows redirects only \
             within {host}, so a token cannot be carried elsewhere; check the repository URL"
        );
    }
    // A host with a forge token defaults to GitLab (§4); a GitHub Enterprise
    // instance answers GitLab's /api/v4 with a 404, which must say how to fix
    // it rather than read as a missing repository.
    let ghe = if api.forge == Forge::Gitlab && status == StatusCode::NOT_FOUND {
        format!(
            ". If {host} is GitHub Enterprise rather than GitLab, set forge=github — lmgw asked \
             the GitLab API ({})",
            api.base
        )
    } else {
        String::new()
    };
    match status {
        StatusCode::UNAUTHORIZED if token_sent => format!(
            "{what}: the forge token for {host} was rejected ({code}{said}) — check it in \
             Settings"
        ),
        StatusCode::UNAUTHORIZED => format!(
            "{what}: {name} wants authentication ({code}{said}) — add a forge token for {host} \
             (Settings)"
        ),
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND if token_sent => format!(
            "{what}: {name} answered {code}{said} — check the repository URL and the number, \
             and that the forge token for {host} can read the repository{ghe}"
        ),
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => format!(
            "{what}: {name} answered {code}{said} — check the repository URL and the number; a \
             private repository needs a forge token for {host} (Settings){ghe}"
        ),
        _ => format!("{what}: {name} answered {code}{said}"),
    }
}

/// The reason in an error body: `message` (GitHub, GitLab), an OAuth
/// `error_description` / `error`, or the first line of a plain-text body.
fn forge_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        for key in ["message", "error_description", "error"] {
            match v.get(key) {
                Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
                    return s.trim().to_string();
                }
                Some(other @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => {
                    return other.to_string();
                }
                _ => {}
            }
        }
        return String::new();
    }
    let first = body.lines().map(str::trim).find(|l| !l.is_empty());
    match first {
        Some(l) if !l.starts_with('<') => l.to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GhPull {
    number: u64,
    #[serde(default)]
    title: String,
    user: Option<GhUser>,
    #[serde(default)]
    updated_at: String,
    draft: Option<bool>,
    #[serde(default)]
    state: String,
    head: Option<GhRef>,
    base: Option<GhRef>,
    merged_at: Option<String>,
    merge_commit_sha: Option<String>,
    #[serde(default)]
    html_url: String,
}

#[derive(Deserialize)]
struct GhUser {
    #[serde(default)]
    login: String,
}

#[derive(Deserialize)]
struct GhRef {
    #[serde(default)]
    sha: String,
}

impl GhPull {
    fn into_pr(self) -> ForgePr {
        // GitHub reports a merged PR as `closed` with `merged_at` set.
        let state = if self.merged_at.is_some() {
            "merged".to_string()
        } else {
            self.state
        };
        ForgePr {
            number: self.number,
            title: self.title,
            author: self.user.map(|u| u.login).unwrap_or_default(),
            updated_at: self.updated_at,
            draft: self.draft.unwrap_or(false),
            state,
            head_sha: self.head.map(|r| r.sha).unwrap_or_default(),
            base_sha: self.base.map(|r| r.sha).unwrap_or_default(),
            // For an open PR GitHub names its test merge here, which says
            // nothing about the base; only a merged PR's is kept.
            merge_commit_sha: self
                .merge_commit_sha
                .filter(|_| self.merged_at.is_some())
                .unwrap_or_default(),
            merged_at: self.merged_at,
            url: self.html_url,
        }
    }
}

#[derive(Deserialize)]
struct GlMr {
    iid: u64,
    #[serde(default)]
    title: String,
    author: Option<GlUser>,
    #[serde(default)]
    updated_at: String,
    draft: Option<bool>,
    /// The pre-14.0 spelling of `draft`.
    work_in_progress: Option<bool>,
    #[serde(default)]
    state: String,
    sha: Option<String>,
    /// Only on the single-MR endpoint; `base_sha` is the merge base.
    diff_refs: Option<GlDiffRefs>,
    merged_at: Option<String>,
    merge_commit_sha: Option<String>,
    squash_commit_sha: Option<String>,
    #[serde(default)]
    web_url: String,
}

#[derive(Deserialize)]
struct GlUser {
    #[serde(default)]
    username: String,
}

#[derive(Deserialize)]
struct GlDiffRefs {
    base_sha: Option<String>,
}

impl GlMr {
    fn into_pr(self) -> ForgePr {
        let state = match self.state.as_str() {
            "opened" => "open".to_string(),
            _ => self.state,
        };
        // `merged_at` is what the executor and the update check read (§14.1);
        // an old GitLab can leave it null on a merged MR, and the last update
        // of a merged MR is the closest time it offers.
        let merged_at = self
            .merged_at
            .or_else(|| (state == "merged").then(|| self.updated_at.clone()));
        ForgePr {
            number: self.iid,
            title: self.title,
            author: self.author.map(|u| u.username).unwrap_or_default(),
            updated_at: self.updated_at,
            draft: self.draft.or(self.work_in_progress).unwrap_or(false),
            state,
            head_sha: self.sha.unwrap_or_default(),
            base_sha: self.diff_refs.and_then(|d| d.base_sha).unwrap_or_default(),
            // A fast-forward squash has no merge commit, only the squash one.
            merge_commit_sha: self
                .merge_commit_sha
                .filter(|s| !s.is_empty())
                .or(self.squash_commit_sha)
                .unwrap_or_default(),
            merged_at,
            url: self.web_url,
        }
    }
}

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

/// `ls-remote`'s answer as the ref combobox wants it: the default branch
/// first, tags newest first ([`version_cmp`]) and peeled to their commit.
pub fn refs_view(refs: RemoteRefs) -> RemoteRefsView {
    let default_branch = refs.default_branch.unwrap_or_default();
    let mut heads: Vec<RefEntry> = refs
        .heads
        .into_iter()
        .map(|h| RefEntry {
            name: h.name,
            sha: h.sha,
        })
        .collect();
    heads.sort_by(|a, b| {
        (a.name != default_branch)
            .cmp(&(b.name != default_branch))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut tags: Vec<RefEntry> = refs
        .tags
        .into_iter()
        .map(|t| RefEntry {
            name: t.name,
            sha: t.commit,
        })
        .collect();
    tags.sort_by(|a, b| version_cmp(&b.name, &a.name));
    RemoteRefsView {
        default_branch,
        heads,
        tags,
    }
}

/// Version order for tags: the numbers in the names compared in turn, then
/// the whole names ([`natural_cmp`]). The prefix does not rank first, so
/// llama.cpp's `b7603` sorts above its old `v0.5.0` (a plain natural order
/// puts every `v…` above every `b…`), and `v1.10` above `v1.9`.
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    fn numbers(s: &str) -> Vec<&str> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .map(|p| p.trim_start_matches('0'))
            .collect()
    }
    let (x, y) = (numbers(a), numbers(b));
    for (p, q) in x.iter().zip(&y) {
        let ord = p.len().cmp(&q.len()).then_with(|| p.cmp(q));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    x.len().cmp(&y.len()).then_with(|| natural_cmp(a, b))
}

/// Order with digit runs compared as numbers: `b699` < `b7000`, `v1.9` <
/// `v1.10`.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a, b);
    loop {
        match (a.is_empty(), b.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        let (ca, ra) = split_chunk(a);
        let (cb, rb) = split_chunk(b);
        let digits = |s: &str| s.as_bytes()[0].is_ascii_digit();
        let ord = if digits(ca) && digits(cb) {
            let (x, y) = (ca.trim_start_matches('0'), cb.trim_start_matches('0'));
            x.len()
                .cmp(&y.len())
                .then_with(|| x.cmp(y))
                .then_with(|| ca.len().cmp(&cb.len()))
        } else {
            ca.cmp(cb)
        };
        if ord != Ordering::Equal {
            return ord;
        }
        (a, b) = (ra, rb);
    }
}

/// The leading run of digits or of non-digits, and the rest. `s` is not
/// empty.
fn split_chunk(s: &str) -> (&str, &str) {
    let digit = s.as_bytes()[0].is_ascii_digit();
    let end = s
        .find(|c: char| c.is_ascii_digit() != digit)
        .unwrap_or(s.len());
    s.split_at(end)
}

// ---------------------------------------------------------------------------
// With the tokens
// ---------------------------------------------------------------------------

/// A [`ForgeClient`] plus a snapshot of the `forge_tokens` setting: each call
/// finds its own token by the repository's host ([`token_for`]). The shape
/// the `forge_*` ops and the run executor's forge lookup want.
#[derive(Clone)]
pub struct ForgeSession {
    client: ForgeClient,
    tokens: BTreeMap<String, String>,
}

impl std::fmt::Debug for ForgeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForgeSession")
            .field("client", &self.client)
            .field("token_hosts", &self.tokens.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ForgeSession {
    pub fn new(client: ForgeClient, tokens: BTreeMap<String, String>) -> Self {
        Self { client, tokens }
    }

    pub fn client(&self) -> &ForgeClient {
        &self.client
    }

    /// [`ForgeClient::list_prs`] with the repository host's token.
    pub async fn prs(
        &self,
        repo_url: &str,
        forge: Forge,
        query: Option<&str>,
        page: Option<u32>,
    ) -> Result<ForgePrPage, String> {
        let token = token_for(&self.tokens, repo_url);
        self.client
            .list_prs(repo_url, forge, query, page, token)
            .await
    }

    /// [`ForgeClient::get_pr`] with the repository host's token.
    pub async fn pr(&self, repo_url: &str, forge: Forge, number: u64) -> Result<ForgePr, String> {
        let token = token_for(&self.tokens, repo_url);
        self.client.get_pr(repo_url, forge, number, token).await
    }

    /// [`ForgeClient::refs`] with the repository host's token.
    pub async fn refs(&self, repo_url: &str) -> Result<RemoteRefsView, String> {
        let token = token_for(&self.tokens, repo_url);
        self.client.refs(repo_url, token).await
    }
}

/// A [`ForgeSession`] of this gateway right now: its shared client
/// ([`BuildSeams::forge_client`](crate::backends::run::BuildSeams::forge_client))
/// and the `forge_tokens` of the **current** settings. Made per call, never
/// kept: a token the owner saves (or clears) applies to the next call.
pub fn session(state: &AppState) -> Result<ForgeSession, String> {
    Ok(ForgeSession::new(
        state.builds.forge_client()?,
        state.snapshot().settings.forge_tokens.clone(),
    ))
}

/// The forge lookup the gateway installs at startup
/// ([`BuildSeams::set_forge`](crate::backends::run::BuildSeams::set_forge)):
/// what a build run asks for a PR's state (§5 phase 1, §14.1), answered by
/// the real forge APIs.
///
/// It holds the gateway, not a token snapshot: every call takes the token
/// from the settings as they are at that moment ([`session`]), so a token
/// saved while the gateway runs is used by the next run. The reference is
/// weak because the lookup lives inside the state it points at
/// (`AppState::builds`) — a strong one would be a cycle that keeps the state
/// alive forever, the reason `McpManager` holds a weak one too.
pub struct GatewayForge {
    state: Weak<AppState>,
}

impl GatewayForge {
    pub fn new(state: &SharedState) -> Self {
        Self {
            state: Arc::downgrade(state),
        }
    }
}

#[async_trait::async_trait]
impl super::ForgeLookup for GatewayForge {
    async fn pr(&self, repo_url: &str, forge: Forge, number: u64) -> Result<ForgePr, String> {
        let state = self
            .state
            .upgrade()
            .ok_or("the gateway is shutting down, so the forge is not asked")?;
        let session = session(&state)?;
        session.pr(repo_url, forge, number).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_page_comes_from_link_or_x_next_page() {
        let link = "<https://api.github.com/repositories/1/pulls?state=open&page=3>; \
                    rel=\"next\", <https://api.github.com/repositories/1/pulls?page=9>; \
                    rel=\"last\"";
        assert_eq!(next_page_from_link(link), Some(3));
        let last = "<https://api.github.com/repositories/1/pulls?page=1>; rel=\"first\", \
                    <https://api.github.com/repositories/1/pulls?page=8>; rel=\"prev\"";
        assert_eq!(next_page_from_link(last), None);
        assert_eq!(next_page_from_link(""), None);

        let mut h = HeaderMap::new();
        h.insert("x-next-page", "".parse().unwrap());
        assert_eq!(next_page(&h), None);
        h.insert("x-next-page", "2".parse().unwrap());
        assert_eq!(next_page(&h), Some(2));
    }

    #[test]
    fn tags_sort_by_version_order() {
        let mut v = vec!["b699", "b7000", "b1000", "v1.10", "v1.9", "b0700", "master"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            ["b699", "b0700", "b1000", "b7000", "master", "v1.9", "v1.10"]
        );
        let mut v = vec![
            "v0.5.0", "b699", "b7603", "latest", "v0.4.1", "v1.10", "v1.9",
        ];
        v.sort_by(|a, b| version_cmp(b, a));
        assert_eq!(
            v,
            ["b7603", "b699", "v1.10", "v1.9", "v0.5.0", "v0.4.1", "latest"]
        );
    }

    #[test]
    fn pasted_pr_urls_name_repo_and_number() {
        let (r, n) =
            parse_pr_url("https://github.com/ggml-org/llama.cpp/pull/16391/files").unwrap();
        assert_eq!(
            (r.host.as_str(), r.path.as_str(), n),
            ("github.com", "ggml-org/llama.cpp", 16391)
        );
        let (r, n) =
            parse_pr_url("https://git.example.com/a/pull/b/-/merge_requests/7/diffs#x").unwrap();
        assert_eq!((r.path.as_str(), n), ("a/pull/b", 7));
        assert!(parse_pr_url("https://github.com/o/r/issues/5").is_none());
        assert!(parse_pr_url("https://github.com/o/r/pull/0").is_none());
        assert!(parse_pr_url("ftp://github.com/o/r/pull/5").is_none());
    }

    /// One host function for every token lookup: the API path and the git
    /// path used to disagree on ports (`:443`, an SSH daemon's `:2222`).
    #[test]
    fn token_host_is_the_web_host_for_every_url_form() {
        for (url, host) in [
            ("https://GitHub.com/o/r", "github.com"),
            ("https://git.example:443/g/p.git", "git.example"),
            ("https://git.example:8443/g/p", "git.example:8443"),
            ("http://git.example:80/g/p", "git.example"),
            ("ssh://git@git.example:2222/g/p.git", "git.example"),
            ("git@git.example:g/p.git", "git.example"),
        ] {
            assert_eq!(token_host(url).as_deref(), Some(host), "{url}");
            // The API path finds its token under the same key.
            let repo = ForgeRepo::parse(url).unwrap();
            assert_eq!(repo.host, host, "{url}");
        }
        assert_eq!(token_host("file:///srv/repo"), None);
        assert_eq!(token_host("-x"), None);

        let tokens: BTreeMap<String, String> =
            [("git.example".to_string(), "tok".to_string())].into();
        for url in [
            "https://git.example:443/g/p",
            "ssh://git@git.example:2222/g/p",
            "git@git.example:g/p",
        ] {
            assert_eq!(token_for(&tokens, url), Some("tok"), "{url}");
        }
        assert_eq!(token_for(&tokens, "https://git.example:8443/g/p"), None);
    }

    #[test]
    fn a_token_never_goes_over_plain_http_to_another_machine() {
        let e = refuse_plain_http("http://git.example:8080/api/v4/projects/x").unwrap_err();
        assert_eq!(
            e,
            "refusing to send the forge token over plain http to git.example:8080; use https"
        );
        for ok in [
            "https://git.example/g/p",
            "http://127.0.0.1:9000/g/p",
            "http://localhost/g/p",
            "http://[::1]:9000/g/p",
        ] {
            assert_eq!(refuse_plain_http(ok), Ok(()), "{ok}");
        }

        // git: the same refusal, before a header exists; no token, no refusal.
        let tokens: BTreeMap<String, String> =
            [("git.example".to_string(), "tok".to_string())].into();
        let e = git_auth(&tokens, "http://git.example/g/p", Forge::Gitlab).unwrap_err();
        assert!(
            e.contains("refusing to send the forge token over plain http"),
            "{e}"
        );
        assert!(
            git_auth(&BTreeMap::new(), "http://git.example/g/p", Forge::Gitlab)
                .unwrap()
                .is_none()
        );
        let auth = git_auth(&tokens, "https://git.example/g/p", Forge::Gitlab)
            .unwrap()
            .unwrap();
        assert_eq!(auth.url_prefix(), "https://git.example/");
        // An SSH remote gets no header, so nothing is refused.
        assert!(git_auth(&tokens, "git@git.example:g/p", Forge::Gitlab)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn the_api_refuses_to_send_a_token_over_plain_http() {
        let client = ForgeClient::new().unwrap();
        // Nothing listens there: the refusal comes before any request.
        let e = client
            .get_pr("http://git.invalid/g/p", Forge::Gitlab, 1, Some("tok"))
            .await
            .unwrap_err();
        assert!(
            e.contains(
                "refusing to send the forge token over plain http to git.invalid; use https"
            ),
            "{e}"
        );
    }

    #[test]
    fn a_gitlab_404_says_how_to_reach_github_enterprise() {
        let repo = ForgeRepo::parse("https://ghe.example/o/r").unwrap();
        let api = Api {
            forge: Forge::Gitlab,
            name: "GitLab",
            base: "https://ghe.example/api/v4".into(),
        };
        let e = api_error(
            &api,
            &repo,
            "MR !7 of o/r",
            StatusCode::NOT_FOUND,
            &HeaderMap::new(),
            "",
            &Quota::default(),
            true,
            chrono::Utc::now(),
        );
        assert!(
            e.contains("If ghe.example is GitHub Enterprise rather than GitLab, set forge=github"),
            "{e}"
        );
        let github = Api {
            forge: Forge::Github,
            name: "GitHub",
            base: "https://ghe.example/api/v3".into(),
        };
        let e = api_error(
            &github,
            &repo,
            "PR #7 of o/r",
            StatusCode::NOT_FOUND,
            &HeaderMap::new(),
            "",
            &Quota::default(),
            true,
            chrono::Utc::now(),
        );
        assert!(!e.contains("GitHub Enterprise"), "{e}");
    }

    #[test]
    fn quota_view_is_rfc3339_utc() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining", "59".parse().unwrap());
        h.insert("x-ratelimit-reset", "1790000000".parse().unwrap());
        let q = Quota::from_headers(&h);
        assert_eq!(
            q.view(),
            Some(RateLimit {
                remaining: 59,
                reset_at: "2026-09-21T14:13:20Z".into()
            })
        );
        assert!(Quota::default().view().is_none());
        assert!(!q.exhausted(StatusCode::FORBIDDEN));
    }
}
