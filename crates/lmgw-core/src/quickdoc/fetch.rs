//! The fetch stage (quickdoc §8, contract point 1): **code fetches, the model
//! never free-crawls.**
//!
//! Every request passes three gates before it leaves the process — the source's
//! domain fence, the host's `robots.txt`, and a visible per-host delay. The
//! model has no `fetch` tool at all: the URL list comes from the source root and
//! the links found in it, and both are filtered here.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use quickdoc_core::ingest::{url, Fence};

/// One fetched document, as the sniffer wants it.
pub struct Fetched {
    pub body: String,
    pub content_type: Option<String>,
}

pub struct Fetcher {
    http: reqwest::Client,
    /// Minimum gap between two requests to the same host (`docs_fetch_delay_ms`).
    delay: Duration,
    last: HashMap<String, Instant>,
    /// host → its `robots.txt` rules, fetched once per run.
    robots: HashMap<String, Robots>,
}

impl Fetcher {
    pub fn new(http: reqwest::Client, delay_ms: u64) -> Self {
        Self {
            http,
            delay: Duration::from_millis(delay_ms),
            last: HashMap::new(),
            robots: HashMap::new(),
        }
    }

    pub async fn get(&mut self, url_str: &str, fence: &Fence) -> Result<Fetched, String> {
        if !fence.allows(url_str) {
            return Err(fence.refusal(url_str).to_string());
        }
        let host = url::host(url_str).ok_or_else(|| format!("{url_str} is not an http(s) URL"))?;
        if !self.robots_for(&host, url_str).await.allows(url_str) {
            return Err(format!("{url_str} is disallowed by {host}'s robots.txt"));
        }
        self.wait_for(&host).await;
        self.raw(url_str).await
    }

    /// Fetch without the fence/robots gates — used only for `robots.txt`
    /// itself, which is the thing that decides what the gates say.
    async fn raw(&self, url_str: &str) -> Result<Fetched, String> {
        let resp = self
            .http
            .get(url_str)
            .send()
            .await
            .map_err(|e| format!("GET {url_str}: {e}"))?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if !status.is_success() {
            return Err(format!("GET {url_str}: {status}"));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| format!("reading {url_str}: {e}"))?;
        Ok(Fetched { body, content_type })
    }

    async fn wait_for(&mut self, host: &str) {
        if self.delay.is_zero() {
            return;
        }
        if let Some(last) = self.last.get(host) {
            let since = last.elapsed();
            if since < self.delay {
                tokio::time::sleep(self.delay - since).await;
            }
        }
        self.last.insert(host.to_string(), Instant::now());
    }

    async fn robots_for(&mut self, host: &str, any_url: &str) -> &Robots {
        if !self.robots.contains_key(host) {
            let rules = match url::split(any_url) {
                Some((scheme, authority, _)) => {
                    let target = format!("{scheme}://{authority}/robots.txt");
                    self.wait_for(host).await;
                    match self.raw(&target).await {
                        Ok(f) => Robots::parse(&f.body),
                        // No robots.txt, or an unreachable one, means no rules —
                        // which is what every crawler does and what the standard
                        // says.
                        Err(e) => {
                            tracing::debug!("no robots.txt for {host}: {e}");
                            Robots::default()
                        }
                    }
                }
                None => Robots::default(),
            };
            self.robots.insert(host.to_string(), rules);
        }
        &self.robots[host]
    }
}

/// The `User-agent: *` group of a `robots.txt`, matched longest-prefix-first as
/// the standard specifies (an `Allow` may carve an exception out of a broader
/// `Disallow`). Other user-agent groups are ignored: lmgw sends no custom agent
/// string, so `*` is the group that applies to it.
#[derive(Debug, Default)]
pub struct Robots {
    /// `(path prefix, allowed)`.
    rules: Vec<(String, bool)>,
}

impl Robots {
    pub fn parse(body: &str) -> Self {
        let mut rules = Vec::new();
        let mut applies = false;
        let mut in_group = false;
        for line in body.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "user-agent" => {
                    // A new group starts after any rule line, so consecutive
                    // `User-agent` lines share the rules that follow them.
                    if in_group {
                        applies = false;
                        in_group = false;
                    }
                    applies |= value == "*";
                }
                "disallow" if applies => {
                    in_group = true;
                    if !value.is_empty() {
                        rules.push((value.to_string(), false));
                    }
                }
                "allow" if applies => {
                    in_group = true;
                    if !value.is_empty() {
                        rules.push((value.to_string(), true));
                    }
                }
                _ => {}
            }
        }
        Self { rules }
    }

    pub fn allows(&self, url_str: &str) -> bool {
        let path = url::split(url_str).map_or("/", |(_, _, p)| if p.is_empty() { "/" } else { p });
        self.rules
            .iter()
            .filter(|(prefix, _)| path.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .is_none_or(|(_, allowed)| *allowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robots_honours_the_star_group_longest_prefix_first() {
        let r = Robots::parse(
            "User-agent: BadBot\nDisallow: /\n\n\
             User-agent: *\n# a comment\nDisallow: /private\nAllow: /private/public\n",
        );
        assert!(r.allows("https://x.dev/guide"));
        assert!(!r.allows("https://x.dev/private/secret"));
        assert!(r.allows("https://x.dev/private/public/ok"));
        // A group aimed at somebody else must not apply to us.
        assert!(Robots::parse("User-agent: BadBot\nDisallow: /\n").allows("https://x.dev/any"));
    }

    #[test]
    fn no_robots_file_means_no_rules() {
        assert!(Robots::default().allows("https://x.dev/anything"));
        assert!(Robots::parse("").allows("https://x.dev/anything"));
    }
}
