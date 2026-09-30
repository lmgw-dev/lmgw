//! The three URL operations ingestion needs: split, join, and host.
//!
//! Hand-rolled rather than pulled from a crate: a documentation fetcher deals
//! in `http(s)` absolute URLs and the relative links a markdown index carries,
//! which is a small enough slice of RFC 3986 that a dependency would be paying
//! for generality nothing here uses.

/// `(scheme, authority, path)` of an absolute `http(s)` URL. Query and fragment
/// stay attached to `path` — nothing here needs them apart.
pub fn split(url: &str) -> Option<(&str, &str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    if authority.is_empty() {
        return None;
    }
    Some((scheme, authority, path))
}

/// Lowercased host of an absolute URL, port stripped — what a domain fence is
/// compared against.
pub fn host(url: &str) -> Option<String> {
    let (_, authority, _) = split(url)?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.rfind(':') {
        // Not an IPv6 literal, so the last colon starts the port.
        Some(i) if !host.contains(']') => &host[..i],
        _ => host,
    };
    Some(host.to_ascii_lowercase())
}

/// Resolve `href` against the absolute `base`. `None` for anything that is not
/// a fetchable `http(s)` location — fragments, `mailto:`, `javascript:` — so a
/// caller filtering links does not have to recognise those itself.
pub fn join(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    let href = href.split('#').next().unwrap_or("");
    if href.is_empty() {
        return None;
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return split(href).map(|_| href.to_string());
    }
    let (scheme, authority, path) = split(base)?;
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    if href.contains(':') && !href.starts_with('/') {
        // `mailto:`, `javascript:`, `data:` — a scheme we do not fetch.
        let head = href.split(':').next().unwrap_or("");
        if !head.contains('/') {
            return None;
        }
    }
    if let Some(abs) = href.strip_prefix('/') {
        return Some(format!("{scheme}://{authority}/{}", normalize(abs)));
    }
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    let joined = format!("{}{href}", dir.trim_start_matches('/'));
    Some(format!("{scheme}://{authority}/{}", normalize(&joined)))
}

/// Collapse `.` and `..` segments. A `..` that would climb above the root is
/// dropped, matching what every browser and HTTP client does.
fn normalize(path: &str) -> String {
    let (path, tail) = match path.find(['?', '#']) {
        Some(i) => path.split_at(i),
        None => (path, ""),
    };
    let trailing_slash = path.ends_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut s = out.join("/");
    if trailing_slash && !s.is_empty() {
        s.push('/');
    }
    s.push_str(tail);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_ignore_port_and_case() {
        assert_eq!(
            host("https://Docs.RS:443/axum/").as_deref(),
            Some("docs.rs")
        );
        assert_eq!(host("http://example.com").as_deref(), Some("example.com"));
        assert_eq!(host("ftp://example.com"), None);
    }

    #[test]
    fn joins_the_four_link_shapes_and_refuses_the_rest() {
        let base = "https://docs.rs/axum/0.8/guide/routing.html";
        assert_eq!(
            join(base, "https://other.dev/x").as_deref(),
            Some("https://other.dev/x")
        );
        assert_eq!(
            join(base, "//cdn.dev/x").as_deref(),
            Some("https://cdn.dev/x")
        );
        assert_eq!(
            join(base, "/axum/0.8/index.html").as_deref(),
            Some("https://docs.rs/axum/0.8/index.html")
        );
        assert_eq!(
            join(base, "../handlers.md").as_deref(),
            Some("https://docs.rs/axum/0.8/handlers.md")
        );
        assert_eq!(
            join(base, "extract.md#body").as_deref(),
            Some("https://docs.rs/axum/0.8/guide/extract.md")
        );
        assert_eq!(join(base, "#section"), None);
        assert_eq!(join(base, "mailto:x@y.z"), None);
    }
}
