//! A device's lmgw URLs (device link D13, §3.1, §6.3): `https` only, in the
//! owner's order, refreshed by lmgw at enrolment and in the feed's `hello`,
//! and tried in the order [`Urls::order`] gives.

use serde::{Deserialize, Serialize};

use super::normalize_base_url;

/// The URLs a device reaches lmgw at: `https` base URLs, normalized
/// ([`normalize_base_url`]), no two alike, at least one, in the owner's
/// order. Stored as a JSON list of strings; a stored list is checked again
/// when read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct Urls(Vec<String>);

impl Urls {
    /// A list the link parser already normalized and checked.
    pub(super) fn from_normalized(urls: Vec<String>) -> Self {
        debug_assert!(!urls.is_empty());
        Self(urls)
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// The order to try them in: `last_good`, the URL that last connected,
    /// first when the list still holds it; then the rest in the list's
    /// order. A client races them in that order, starting the next one a
    /// stagger of its own after the previous one if no TCP answer has come;
    /// the first handshake that verifies wins.
    pub fn order(&self, last_good: Option<&str>) -> Vec<String> {
        let last_good = last_good.and_then(|u| normalize_base_url(u).ok());
        let mut order = Vec::with_capacity(self.0.len());
        if let Some(good) = last_good.filter(|g| self.0.contains(g)) {
            order.push(good);
        }
        for u in &self.0 {
            if !order.contains(u) {
                order.push(u.clone());
            }
        }
        order
    }
}

impl TryFrom<Vec<String>> for Urls {
    type Error = String;

    /// Strict, for a list the client stored: every URL usable, none twice.
    fn try_from(urls: Vec<String>) -> Result<Self, String> {
        let mut out: Vec<String> = Vec::with_capacity(urls.len());
        for (i, u) in urls.iter().enumerate() {
            let u = normalize_base_url(u).map_err(|e| format!("url {}: {e}", i + 1))?;
            if out.contains(&u) {
                return Err(format!("url {} repeats an earlier one", i + 1));
            }
            out.push(u);
        }
        if out.is_empty() {
            return Err("the list holds no url".into());
        }
        Ok(Self(out))
    }
}

impl From<Urls> for Vec<String> {
    fn from(urls: Urls) -> Self {
        urls.0
    }
}

/// A URL list lmgw sends (the enrolment's `urls`, the feed's `hello.urls`)
/// as the device keeps it: the `https` URLs, normalized, each once, in
/// lmgw's order; anything else is dropped. `None` when nothing usable is
/// left: the client keeps the list it has.
pub fn refresh_urls(offered: &[String]) -> Option<Urls> {
    let mut out: Vec<String> = Vec::with_capacity(offered.len());
    for u in offered.iter().filter_map(|u| normalize_base_url(u).ok()) {
        if !out.contains(&u) {
            out.push(u);
        }
    }
    (!out.is_empty()).then_some(Urls(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(list: &[&str]) -> Urls {
        Urls::try_from(list.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn the_last_good_url_goes_first_then_the_owner_s_order() {
        let list = urls(&["https://192.0.2.1:8788", "https://198.51.100.20:8788/"]);
        assert_eq!(
            list.order(None),
            ["https://192.0.2.1:8788", "https://198.51.100.20:8788"]
        );
        assert_eq!(
            list.order(Some("HTTPS://198.51.100.20:8788/")),
            ["https://198.51.100.20:8788", "https://192.0.2.1:8788"],
            "compared normalized"
        );
        assert_eq!(
            list.order(Some("https://203.0.113.9:8788")),
            ["https://192.0.2.1:8788", "https://198.51.100.20:8788"],
            "a URL the list no longer holds"
        );
        assert_eq!(list.order(Some("not a url"))[0], "https://192.0.2.1:8788");
    }

    #[test]
    fn a_refreshed_list_keeps_its_https_urls_once() {
        let offered = [
            "http://198.51.100.20:8787",
            "https://192.0.2.1:8788/",
            "https://192.0.2.1:8788",
            "ftp://h",
            "https://198.51.100.20:8788",
        ]
        .map(String::from);
        assert_eq!(
            refresh_urls(&offered).unwrap().as_slice(),
            ["https://192.0.2.1:8788", "https://198.51.100.20:8788"]
        );
        assert_eq!(refresh_urls(&["http://h".to_string()]), None);
        assert_eq!(refresh_urls(&[]), None);
    }

    #[test]
    fn a_stored_list_is_checked_again() {
        let list = urls(&["https://h:8788"]);
        let json = serde_json::to_string(&list).unwrap();
        assert_eq!(json, r#"["https://h:8788"]"#);
        assert_eq!(serde_json::from_str::<Urls>(&json).unwrap(), list);
        for bad in [r#"[]"#, r#"["http://h"]"#, r#"["https://h","https://h/"]"#] {
            assert!(serde_json::from_str::<Urls>(bad).is_err(), "{bad}");
        }
    }
}
