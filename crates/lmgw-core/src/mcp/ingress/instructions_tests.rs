//! The admin plane's instructions name only what the self-admin catalog
//! has (find-a-model design WP5's review round): every `lmgw__` tool, and
//! every argument written beside one (`` `lmgw__container model=<id>` ``,
//! or `` `lmgw__hf_repo` with `fit=true` ``).

use std::collections::{BTreeMap, BTreeSet};

/// Every tool and its argument names, as `tools/list` has them.
fn catalog() -> BTreeMap<String, BTreeSet<String>> {
    crate::mcp::selfadmin::full_catalog()
        .into_iter()
        .filter_map(|(entry, _)| {
            let name = entry["name"].as_str()?.to_string();
            let props = entry["inputSchema"]["properties"]
                .as_object()
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            Some((name, props))
        })
        .collect()
}

/// What the instructions name: each tool, with the arguments written in
/// its own backtick span, or in one that follows it after " with ".
fn named(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    let span = regex::Regex::new(r"`(lmgw__[a-z0-9_]+)([^`]*)`(?: with `([a-z_]+)=)?").unwrap();
    let arg = regex::Regex::new(r"\b([a-z_]+)=").unwrap();
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for c in span.captures_iter(text) {
        let args = out.entry(c[1].to_string()).or_default();
        args.extend(arg.captures_iter(&c[2]).map(|a| a[1].to_string()));
        if let Some(a) = c.get(3) {
            args.insert(a.as_str().to_string());
        }
    }
    out
}

#[test]
fn every_tool_and_argument_the_admin_instructions_name_is_in_the_catalog() {
    let catalog = catalog();
    let named = named(super::ADMIN_INSTRUCTIONS);
    assert!(named.contains_key("lmgw__model_get"), "{named:?}");
    let mut missing = Vec::new();
    for (tool, args) in &named {
        let Some(has) = catalog.get(tool) else {
            missing.push(tool.clone());
            continue;
        };
        for a in args.iter().filter(|a| !has.contains(*a)) {
            missing.push(format!("{tool} {a}="));
        }
    }
    assert!(
        missing.is_empty(),
        "the admin instructions name what the catalog doesn't have: {missing:?}"
    );
}
