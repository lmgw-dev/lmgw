//! The hub the finder reads, as one seam (find-a-model design D38): the live
//! hub over HTTP, a recording of it for the regression set, and a test's
//! mock all answer [`Hub::get`]. [`Reads`] counts the calls and keeps the
//! hub's rate-limit headers from every answer (§4.5).

use std::future::Future;

/// One answer of the hub's API, with the headers the finder reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HubAnswer {
    pub status: u16,
    pub body: String,
    /// `RateLimit`: `"api";r=<remaining>;t=<seconds to the reset>`.
    pub ratelimit: Option<String>,
    /// `RateLimit-Policy`: `"fixed window";"api";q=<quota>;w=<window seconds>`.
    pub ratelimit_policy: Option<String>,
    /// `Link`, for the next page's cursor.
    pub link: Option<String>,
    /// `Retry-After`, in seconds, when a 429 carries no `RateLimit`.
    pub retry_after: Option<String>,
}

/// A source of hub answers. `path` is the API path with its query
/// (`/api/models?search=…`), always on the hub: a next page's link is read
/// for its cursor, never followed as a URL.
pub trait Hub: Sync {
    /// The answer to `GET <hub>{path}`, or why there was none (no route, a
    /// timeout, a body cut off).
    fn get(&self, path: &str) -> impl Future<Output = Result<HubAnswer, String>> + Send;
}

/// The hub at [`super::super::hf_base`] (or a test's base), with the
/// configured token on every request when one is set (D2: the hub's own
/// hops get it, as the downloader's do; these reads never leave the hub).
#[derive(Clone)]
pub struct LiveHub {
    pub http: reqwest::Client,
    pub base: String,
    pub token: String,
}

impl LiveHub {
    /// The hub this install talks to, with its token.
    pub fn of(state: &crate::state::SharedState) -> Self {
        Self {
            http: state.hub_http.clone(),
            base: crate::hf::hf_base(),
            token: state.snapshot().settings.hf_token.clone(),
        }
    }
}

impl Hub for LiveHub {
    async fn get(&self, path: &str) -> Result<HubAnswer, String> {
        let mut rb = self.http.get(format!("{}{path}", self.base));
        if !self.token.is_empty() {
            rb = rb.bearer_auth(&self.token);
        }
        // A connected but silent hub would hold the finder forever, as it
        // would a tree listing (`listing.rs`); this bounds the body too.
        let resp = rb
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let mut answer = HubAnswer {
            status: resp.status().as_u16(),
            ratelimit: header("ratelimit"),
            ratelimit_policy: header("ratelimit-policy"),
            link: header("link"),
            retry_after: header("retry-after"),
            body: String::new(),
        };
        answer.body = resp.text().await.map_err(|e| e.to_string())?;
        Ok(answer)
    }
}

/// The hub's rate limit as its last answer stated it (§4.5). lmgw keeps no
/// counter of its own: every figure is the hub's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Budget {
    /// Requests left in the window (`RateLimit`'s `r`).
    pub remaining: Option<u64>,
    /// Seconds until the window resets (`RateLimit`'s `t`, else a 429's
    /// `Retry-After`).
    pub reset_s: Option<u64>,
    /// Requests the window allows (`RateLimit-Policy`'s `q`).
    pub quota: Option<u64>,
    /// The window's length in seconds (`RateLimit-Policy`'s `w`).
    pub window_s: Option<u64>,
}

/// The `key=<number>` parameter of a structured header such as
/// `"api";r=499;t=17`.
fn param(header: &str, key: &str) -> Option<u64> {
    header.split(';').find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        (k.trim() == key).then(|| v.trim().parse().ok()).flatten()
    })
}

impl Budget {
    /// The budget after `answer`: its headers where it has them, the last
    /// policy where it doesn't (the policy rarely changes).
    fn update(&mut self, answer: &HubAnswer) {
        if let Some(rl) = &answer.ratelimit {
            self.remaining = param(rl, "r");
            self.reset_s = param(rl, "t");
        } else if let Some(t) = answer.retry_after.as_deref() {
            self.reset_s = t.trim().parse().ok();
        }
        if let Some(p) = &answer.ratelimit_policy {
            if let Some(q) = param(p, "q") {
                self.quota = Some(q);
            }
            if let Some(w) = param(p, "w") {
                self.window_s = Some(w);
            }
        }
    }
}

/// Why a read has no answer to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    /// No answer at all: no route to the hub, a timeout, a cut body.
    Unreachable(String),
    /// The hub's limit is used up: a 429, or the last answer said none are
    /// left, and then nothing was sent.
    RateLimited { reset_s: Option<u64> },
    /// Any other status, with the hub's body.
    Status { status: u16, body: String },
}

/// The finder's reads of one answer: the hub, how many calls it took, and
/// the hub's rate-limit headers from every answer.
pub struct Reads<'h, H: Hub> {
    hub: &'h H,
    pub calls: u32,
    pub budget: Budget,
}

impl<'h, H: Hub> Reads<'h, H> {
    pub fn new(hub: &'h H) -> Self {
        Self {
            hub,
            calls: 0,
            budget: Budget::default(),
        }
    }

    /// `GET path`, with a success status. When the last answer said the
    /// window has no request left, nothing is sent: the hub would refuse it,
    /// and asking anyway is what the limit asks lmgw not to do.
    pub async fn get(&mut self, path: &str) -> Result<HubAnswer, Fail> {
        if self.budget.remaining == Some(0) {
            return Err(Fail::RateLimited {
                reset_s: self.budget.reset_s,
            });
        }
        let answer = self.hub.get(path).await.map_err(Fail::Unreachable)?;
        self.calls += 1;
        self.budget.update(&answer);
        match answer.status {
            200..=299 => Ok(answer),
            429 => Err(Fail::RateLimited {
                reset_s: self.budget.reset_s,
            }),
            status => Err(Fail::Status {
                status,
                body: answer.body,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_limit_headers_are_read_as_the_hub_states_them() {
        let mut b = Budget::default();
        b.update(&HubAnswer {
            status: 200,
            ratelimit: Some("\"api\";r=499;t=17".into()),
            ratelimit_policy: Some("\"fixed window\";\"api\";q=500;w=300".into()),
            ..HubAnswer::default()
        });
        assert_eq!(
            b,
            Budget {
                remaining: Some(499),
                reset_s: Some(17),
                quota: Some(500),
                window_s: Some(300),
            }
        );
        // An answer without a policy keeps the last one.
        b.update(&HubAnswer {
            status: 429,
            retry_after: Some("120".into()),
            ..HubAnswer::default()
        });
        assert_eq!(b.reset_s, Some(120));
        assert_eq!(b.quota, Some(500));
    }
}
