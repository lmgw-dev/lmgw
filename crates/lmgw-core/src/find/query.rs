//! What the Find box was given (find-a-model design §4.1, D124): a Hugging
//! Face address (also inside pasted text, such as §7's share sentence, a
//! Markdown link or `url:https://…`), another address, `owner/repo`, or a
//! name. Another address is answered here and never sent anywhere, and so is
//! a hub page that isn't a model's (an account's, a dataset's, a Space's).

/// The query, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// A `huggingface.co` or `hf.co` model address: the repo, the file a
    /// `blob` or `resolve` address names, and the revision a `blob`,
    /// `resolve` or `tree` address names.
    Address {
        repo: String,
        file: Option<String>,
        rev: Option<String>,
    },
    /// `owner/repo`, as `validate_repo` takes it.
    Repo(String),
    /// Anything else: a name, searched as typed (D18). The hub's own model
    /// search page (`/models?search=…`) is its search's text.
    Name(String),
    /// An address on another host (D124).
    Elsewhere { host: String },
    /// A page of the hub that isn't one model's: answered here, nothing sent.
    HubPage(HubPage),
    /// Nothing to search for.
    Empty,
}

/// A hub page that isn't a model's, by the hub's own address scheme, with
/// the address as the person reads it (`huggingface.co/datasets/a/b`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubPage {
    /// `/<name>`: an organisation's or a person's page.
    Account { address: String, name: String },
    /// `/datasets/…`.
    Dataset { address: String },
    /// `/spaces/…`.
    Space { address: String },
    /// Any other page: the front page, the model list without a search, a
    /// path that names no repo.
    Other { address: String },
}

const HUB_HOSTS: [&str; 3] = ["huggingface.co", "www.huggingface.co", "hf.co"];

/// Punctuation a sentence puts around an address it quotes.
fn trim_trailing(s: &str) -> &str {
    s.trim_end_matches(['.', ',', ';', ':', ')', ']', '>', '"', '\'', '!', '?'])
        .trim_start_matches(['(', '[', '<', '"', '\''])
}

/// What can't be part of an address as people paste it: the brackets and
/// quotes of the text around it (`[x](https://…)`, `<https://…>`).
const AROUND: [char; 8] = ['(', ')', '[', ']', '<', '>', '"', '`'];

/// The address in `token`, from the first `scheme://` anywhere in it (RFC
/// 3986's scheme: a letter, then letters, digits, `+`, `-` or `.`) to the
/// first bracket or quote after it.
fn address_in(token: &str) -> Option<&str> {
    let b = token.as_bytes();
    let scheme_char = |c: u8| c.is_ascii_alphanumeric() || b"+-.".contains(&c);
    let mut from = 0;
    while let Some(i) = token[from..].find("://").map(|i| i + from) {
        let mut s = i;
        while s > 0 && scheme_char(b[s - 1]) {
            s -= 1;
        }
        if let Some(start) = (s..i).find(|&k| b[k].is_ascii_alphabetic()) {
            let rest = &token[start..];
            let after = start + rest[..].find("://").map_or(0, |k| k + 3);
            let end = token[after..]
                .find(AROUND)
                .map_or(token.len(), |k| k + after);
            return Some(trim_trailing(&token[start..end]));
        }
        from = i + 3;
    }
    None
}

/// What a hub address names (`/owner/name[/blob/rev/file]`, a page).
fn hub_path(url: &reqwest::Url) -> Query {
    let segs: Vec<String> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).map(percent_decode).collect())
        .unwrap_or_default();
    let address = {
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        match segs.is_empty() {
            true => host,
            false => format!("{host}/{}", segs.join("/")),
        }
    };
    let page = |p: HubPage| Query::HubPage(p);
    match segs.as_slice() {
        [first, ..] if first == "models" => {
            let search = url
                .query_pairs()
                .find(|(k, _)| k == "search")
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty());
            match search {
                Some(q) => Query::Name(q),
                None => page(HubPage::Other { address }),
            }
        }
        [first, ..] if first == "datasets" => page(HubPage::Dataset { address }),
        [first, ..] if first == "spaces" => page(HubPage::Space { address }),
        [one] => page(HubPage::Account {
            address,
            name: one.clone(),
        }),
        [owner, name, rest @ ..] => {
            let repo = format!("{owner}/{name}");
            if crate::hf::validate_repo(&repo).is_err() {
                return page(HubPage::Other { address });
            }
            let (file, rev) = match rest {
                [kind, rev, path @ ..]
                    if (kind == "blob" || kind == "resolve") && !path.is_empty() =>
                {
                    (Some(path.join("/")), Some(rev.clone()))
                }
                [kind, rev, ..] if kind == "tree" || kind == "blob" || kind == "resolve" => {
                    (None, Some(rev.clone()))
                }
                _ => (None, None),
            };
            Query::Address { repo, file, rev }
        }
        [] => page(HubPage::Other { address }),
    }
}

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A hub address written without its scheme (`huggingface.co/a/b`).
fn bare_hub(token: &str) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    HUB_HOSTS
        .iter()
        .any(|h| lower.starts_with(&format!("{h}/")))
        .then(|| format!("https://{token}"))
}

/// `text` read as §4.1 orders it.
pub fn parse(text: &str) -> Query {
    let text = text.trim();
    if text.is_empty() {
        return Query::Empty;
    }
    let tokens: Vec<&str> = text.split_whitespace().map(trim_trailing).collect();
    // 1. A hub address, anywhere in the text; with or without a scheme.
    for t in &tokens {
        let Some(candidate) = address_in(t).map(str::to_string).or_else(|| bare_hub(t)) else {
            continue;
        };
        let Ok(url) = reqwest::Url::parse(&candidate) else {
            continue;
        };
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        if HUB_HOSTS.contains(&host.as_str()) {
            return hub_path(&url);
        }
    }
    // 2. Any other address: answered here, never sent.
    for t in &tokens {
        if let Some(a) = address_in(t) {
            let rest = a.split_once("://").map_or(a, |(_, r)| r);
            let host = reqwest::Url::parse(a)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| {
                    rest.split(['/', '?', '#'])
                        .next()
                        .unwrap_or(rest)
                        .to_string()
                });
            return Query::Elsewhere { host };
        }
    }
    // 3. `owner/repo`.
    if tokens.len() == 1 && crate::hf::validate_repo(tokens[0]).is_ok() {
        return Query::Repo(tokens[0].to_string());
    }
    Query::Name(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(repo: &str, file: Option<&str>, rev: Option<&str>) -> Query {
        Query::Address {
            repo: repo.into(),
            file: file.map(str::to_string),
            rev: rev.map(str::to_string),
        }
    }

    #[test]
    fn every_hub_address_shape_names_its_repo_and_file() {
        assert_eq!(
            parse("https://huggingface.co/maker-demo/Demo-2-8B"),
            addr("maker-demo/Demo-2-8B", None, None)
        );
        assert_eq!(
            parse("https://hf.co/maker-demo/Demo-2-8B/"),
            addr("maker-demo/Demo-2-8B", None, None)
        );
        assert_eq!(
            parse("huggingface.co/maker-demo/Demo-2-8B"),
            addr("maker-demo/Demo-2-8B", None, None)
        );
        assert_eq!(
            parse("https://huggingface.co/fan-demo/Demo-GGUF/blob/main/Demo-Q4_K_M.gguf"),
            addr("fan-demo/Demo-GGUF", Some("Demo-Q4_K_M.gguf"), Some("main"))
        );
        assert_eq!(
            parse("https://huggingface.co/fan-demo/Demo-GGUF/resolve/v2/sub/Demo-Q4_K_M.gguf?download=true"),
            addr("fan-demo/Demo-GGUF", Some("sub/Demo-Q4_K_M.gguf"), Some("v2"))
        );
        assert_eq!(
            parse("https://huggingface.co/fan-demo/Demo-GGUF/tree/main/sub"),
            addr("fan-demo/Demo-GGUF", None, Some("main"))
        );
        // The share link's encoding (§7) reads back as the file's name.
        assert_eq!(
            parse("https://huggingface.co/fan-demo/Demo-GGUF/blob/main/d/A%20b%23c%3F.gguf"),
            addr("fan-demo/Demo-GGUF", Some("d/A b#c?.gguf"), Some("main"))
        );
    }

    #[test]
    fn an_address_inside_the_share_sentence_is_found() {
        let text = "Get Demo 2 8B in lmgw: open Models → Find a model and paste\n\
                    https://huggingface.co/fan-demo/Demo-GGUF/blob/main/Demo-Q4_K_M.gguf.";
        assert_eq!(
            parse(text),
            addr("fan-demo/Demo-GGUF", Some("Demo-Q4_K_M.gguf"), Some("main"))
        );
    }

    /// WP7: a single-line input drops the share sentence's line break, and
    /// the address starts in the word "paste".
    #[test]
    fn the_share_sentence_without_its_line_break_is_found() {
        let text = "Get Demo 2 8B in lmgw: open Models → Find a model and \
                    pastehttps://huggingface.co/fan-demo/Demo-GGUF/blob/main/Demo-Q4_K_M.gguf";
        assert_eq!(
            parse(text),
            addr("fan-demo/Demo-GGUF", Some("Demo-Q4_K_M.gguf"), Some("main"))
        );
    }

    /// D124: an address is found wherever it starts in a word, as a
    /// Markdown link or after a label.
    #[test]
    fn an_address_inside_a_word_is_found() {
        assert_eq!(
            parse("[x](https://models.example/a)"),
            Query::Elsewhere {
                host: "models.example".into()
            }
        );
        assert_eq!(
            parse("url:https://models.example/a"),
            Query::Elsewhere {
                host: "models.example".into()
            }
        );
        assert_eq!(
            parse("see [Demo](https://huggingface.co/maker-demo/Demo-2-8B)"),
            addr("maker-demo/Demo-2-8B", None, None)
        );
        assert_eq!(
            parse("<https://hf.co/maker-demo/Demo-2-8B>"),
            addr("maker-demo/Demo-2-8B", None, None)
        );
        // A scheme starts with a letter: `1http` is `http` after a digit.
        assert_eq!(
            parse("1http://models.example/a"),
            Query::Elsewhere {
                host: "models.example".into()
            }
        );
        assert_eq!(parse("ratio 3://4"), Query::Name("ratio 3://4".into()));
    }

    #[test]
    fn another_host_is_answered_locally() {
        assert_eq!(
            parse("https://models.example/demo/Demo-2"),
            Query::Elsewhere {
                host: "models.example".into()
            }
        );
        assert_eq!(
            parse("look at ftp://files.example/x"),
            Query::Elsewhere {
                host: "files.example".into()
            }
        );
        // A look-alike host is another host.
        assert_eq!(
            parse("https://huggingface.co.evil.example/a/b"),
            Query::Elsewhere {
                host: "huggingface.co.evil.example".into()
            }
        );
        // So is the host after a userinfo that looks like the hub.
        assert_eq!(
            parse("https://huggingface.co@evil.example/a/b"),
            Query::Elsewhere {
                host: "evil.example".into()
            }
        );
        // The host is read in any case, with a port.
        assert_eq!(
            parse("HTTPS://HuggingFace.co:443/maker-demo/Demo-2-8B"),
            addr("maker-demo/Demo-2-8B", None, None)
        );
    }

    /// A hub page that isn't a model's is said as what it is (§4.1).
    #[test]
    fn hub_pages_that_arent_a_model() {
        assert_eq!(
            parse("https://huggingface.co/models?search=demo+2&sort=likes"),
            Query::Name("demo 2".into())
        );
        assert_eq!(
            parse("https://huggingface.co/models"),
            Query::HubPage(HubPage::Other {
                address: "huggingface.co/models".into()
            })
        );
        assert_eq!(
            parse("https://huggingface.co/maker-demo"),
            Query::HubPage(HubPage::Account {
                address: "huggingface.co/maker-demo".into(),
                name: "maker-demo".into()
            })
        );
        assert_eq!(
            parse("huggingface.co/datasets/maker-demo/demo-set"),
            Query::HubPage(HubPage::Dataset {
                address: "huggingface.co/datasets/maker-demo/demo-set".into()
            })
        );
        assert_eq!(
            parse("https://huggingface.co/spaces/maker-demo/demo-app"),
            Query::HubPage(HubPage::Space {
                address: "huggingface.co/spaces/maker-demo/demo-app".into()
            })
        );
        assert_eq!(
            parse("https://huggingface.co/"),
            Query::HubPage(HubPage::Other {
                address: "huggingface.co".into()
            })
        );
    }

    #[test]
    fn owner_repo_and_names() {
        assert_eq!(
            parse(" maker-demo/Demo-2-8B "),
            Query::Repo("maker-demo/Demo-2-8B".into())
        );
        assert_eq!(parse("qwen 3.6"), Query::Name("qwen 3.6".into()));
        assert_eq!(parse("gemma four"), Query::Name("gemma four".into()));
        assert_eq!(parse("a/b/c"), Query::Name("a/b/c".into()));
        assert_eq!(parse("   "), Query::Empty);
    }
}
